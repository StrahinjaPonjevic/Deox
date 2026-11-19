//! Moves findings to the Recycle Bin. Every safeguard is checked again here, at the
//! moment of deletion, because the disk may have changed since the scan.

use crate::protect::is_protected;
use crate::report::{Category, Confidence, Finding};
use anyhow::{anyhow, bail, Context, Result};
use directories::ProjectDirs;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Deleted,
    /// Dry run: all checks passed and the item would have been moved.
    WouldDelete,
    Protected,
    /// Installed programs are removed by their own uninstaller, never by Deox.
    NotDeletable,
    Missing,
    /// Size, date or type differs from the scan, or the path is now a link.
    Changed,
    /// The system refused, for example because the file is in use.
    Failed(String),
}

impl Outcome {
    fn label(&self) -> String {
        match self {
            Outcome::Deleted => "DELETED".into(),
            Outcome::WouldDelete => "DRY-RUN".into(),
            Outcome::Protected => "SKIPPED:protected".into(),
            Outcome::NotDeletable => "SKIPPED:not-deletable".into(),
            Outcome::Missing => "SKIPPED:missing".into(),
            Outcome::Changed => "SKIPPED:changed".into(),
            Outcome::Failed(why) => format!("FAILED:{}", why.replace(['\t', '\r', '\n'], " ")),
        }
    }
}

pub struct DeleteEntry {
    pub path: PathBuf,
    pub size: u64,
    pub outcome: Outcome,
}

#[derive(Default)]
pub struct DeleteReport {
    pub entries: Vec<DeleteEntry>,
}

impl DeleteReport {
    pub fn freed_bytes(&self) -> u64 {
        self.entries.iter().filter(|e| e.outcome == Outcome::Deleted).map(|e| e.size).sum()
    }
}

/// `%LOCALAPPDATA%\Deox\data\actions.log` (the platform's per-user data folder).
pub fn default_log_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "Deox").map(|d| d.data_local_dir().join("actions.log"))
}

/// Moves each finding to the Recycle Bin and appends one line per action to `log`
/// (time, result, size, path). With `dry_run` nothing is touched and nothing is logged.
/// If the log cannot be opened or written, deletion stops: no action goes unrecorded.
pub fn delete(findings: &[Finding], dry_run: bool, log: &Path) -> Result<DeleteReport> {
    let mut log_file = if dry_run { None } else { Some(open_log(log)?) };
    let mut report = DeleteReport::default();
    for finding in findings {
        let outcome = attempt(finding, dry_run);
        if let Some(file) = log_file.as_mut() {
            let line = format!("{}\t{}\t{}\t{}\n", timestamp(), outcome.label(), finding.size, finding.path.display());
            file.write_all(line.as_bytes()).context("cannot write the action log")?;
        }
        report.entries.push(DeleteEntry { path: finding.path.clone(), size: finding.size, outcome });
    }
    Ok(report)
}

fn attempt(finding: &Finding, dry_run: bool) -> Outcome {
    if finding.confidence == Confidence::Info || finding.category == Category::Bloatware {
        return Outcome::NotDeletable;
    }
    let path = &finding.path;
    // Checked before touching the disk, so a protected path is refused even if it is gone.
    if path.as_os_str().is_empty() || is_protected(path) {
        return Outcome::Protected;
    }
    let Ok(meta) = fs::symlink_metadata(path) else { return Outcome::Missing };
    // Links and junctions are never followed or removed (Rust reports both as symlinks).
    if meta.file_type().is_symlink() {
        return Outcome::Changed;
    }
    let unchanged = if meta.is_dir() {
        dir_size(path) == Some(finding.size)
    } else {
        meta.len() == finding.size && meta.modified().ok() == finding.modified
    };
    if !unchanged {
        return Outcome::Changed;
    }
    if dry_run {
        return Outcome::WouldDelete;
    }
    match trash::delete(path) {
        Ok(()) => Outcome::Deleted,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Total size of regular files below `dir`, skipping links as the scanner does.
/// `None` if anything cannot be read, which counts as a change.
fn dir_size(dir: &Path) -> Option<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let kind = entry.file_type().ok()?;
        if kind.is_symlink() {
            continue;
        }
        total += if kind.is_dir() { dir_size(&entry.path())? } else { entry.metadata().ok()?.len() };
    }
    Some(total)
}

fn open_log(path: &Path) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open the action log {}", path.display()))
}

fn timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    now.replace_nanosecond(0).unwrap_or(now).format(&Rfc3339).unwrap_or_default()
}

/// Starts the program's own uninstaller. The caller must have the user's confirmation first.
pub fn launch_uninstaller(finding: &Finding) -> Result<()> {
    if finding.category != Category::Bloatware || finding.confidence != Confidence::Info {
        bail!("only installed programs have an uninstaller");
    }
    let command = finding
        .uninstall
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("no uninstall command is recorded for this program"))?;
    if command.starts_with("ms-settings:") {
        Command::new("explorer").arg(command).spawn()?;
    } else {
        // The registry stores a full command line; `cmd /S` runs it exactly as written.
        Command::new("cmd").args(["/S", "/C"]).raw_arg(format!("\"{command}\"")).spawn()?;
    }
    Ok(())
}
