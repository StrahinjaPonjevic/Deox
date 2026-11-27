//! Moves findings to the Recycle Bin. Every safeguard is checked again here, at the
//! moment of deletion, because the disk may have changed since the scan.

use crate::protect::is_protected;
use crate::report::{Category, Confidence, Finding};
use crate::scan::{walk, Cancel};
use anyhow::{anyhow, bail, Context, Result};
use directories::ProjectDirs;
use std::fs::{self, OpenOptions};
use std::cell::RefCell;
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
    /// The run was stopped by the user before every item was handled.
    pub cancelled: bool,
    /// Items left untouched because of the cancellation.
    pub not_attempted: usize,
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
    delete_with_progress(findings, dry_run, log, &|_, _| {}, &Cancel::default())
}

/// Items handed to the Recycle Bin in one system call. One call per item costs tens of
/// milliseconds each, which adds up to many minutes for thousands of small cache files.
const TRASH_BATCH: usize = 200;

/// Like [`delete`], and calls `progress(done, total)` as items are settled.
pub fn delete_with_progress(
    findings: &[Finding],
    dry_run: bool,
    log: &Path,
    progress: &dyn Fn(usize, usize),
    cancel: &Cancel,
) -> Result<DeleteReport> {
    let log_file = RefCell::new(if dry_run { None } else { Some(open_log(log)?) });
    let total = findings.len();
    let mut outcomes: Vec<Option<Outcome>> = vec![None; total];
    let mut done = 0;
    let mut pending: Vec<usize> = Vec::new();

    // The log keeps the order of the selection, even though items are settled in batches.
    let mut logged = 0;
    let mut settle = |i: usize, outcome: Outcome, outcomes: &mut Vec<Option<Outcome>>| -> Result<()> {
        outcomes[i] = Some(outcome);
        done += 1;
        if let Some(file) = log_file.borrow_mut().as_mut() {
            while let Some(Some(outcome)) = outcomes.get(logged) {
                let finding = &findings[logged];
                let line = format!("{}\t{}\t{}\t{}\n", timestamp(), outcome.label(), finding.size, finding.path.display());
                file.write_all(line.as_bytes()).context("cannot write the action log")?;
                logged += 1;
            }
        }
        progress(done, total);
        Ok(())
    };

    let mut cancelled = false;
    for (i, finding) in findings.iter().enumerate() {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        match check(finding, dry_run) {
            Err(outcome) => settle(i, outcome, &mut outcomes)?,
            Ok(()) => pending.push(i),
        }
        if pending.len() >= TRASH_BATCH {
            let batch = std::mem::take(&mut pending);
            log_pending(&log_file, findings, &batch)?;
            for (j, outcome) in trash_batch(findings, &batch) {
                settle(j, outcome, &mut outcomes)?;
            }
        }
    }
    // Items checked but not yet handed to the Recycle Bin stay where they are when cancelled.
    if !cancelled {
        log_pending(&log_file, findings, &pending)?;
        for (j, outcome) in trash_batch(findings, &pending) {
            settle(j, outcome, &mut outcomes)?;
        }
    }

    let entries: Vec<DeleteEntry> = findings
        .iter()
        .zip(outcomes)
        .filter_map(|(f, outcome)| outcome.map(|outcome| DeleteEntry { path: f.path.clone(), size: f.size, outcome }))
        .collect();
    let not_attempted = total - entries.len();
    Ok(DeleteReport { entries, cancelled, not_attempted })
}

/// Records the items about to be moved, so a crash during the move still leaves a trace.
fn log_pending(log_file: &RefCell<Option<fs::File>>, findings: &[Finding], indices: &[usize]) -> Result<()> {
    if let Some(file) = log_file.borrow_mut().as_mut() {
        let now = timestamp();
        let text: String = indices
            .iter()
            .map(|&i| format!("{now}\tPENDING\t{}\t{}\n", findings[i].size, findings[i].path.display()))
            .collect();
        file.write_all(text.as_bytes()).context("cannot write the action log")?;
    }
    Ok(())
}

/// Moves the items in one call. If the system rejects the batch, each item is tried alone so
/// that one locked file does not hide what happened to the others.
fn trash_batch(findings: &[Finding], indices: &[usize]) -> Vec<(usize, Outcome)> {
    if indices.is_empty() {
        return Vec::new();
    }
    let paths: Vec<&Path> = indices.iter().map(|&i| findings[i].path.as_path()).collect();
    if trash::delete_all(&paths).is_ok() {
        return indices.iter().map(|&i| (i, Outcome::Deleted)).collect();
    }
    indices
        .iter()
        .map(|&i| {
            let path = &findings[i].path;
            let outcome = match trash::delete(path) {
                Ok(()) => Outcome::Deleted,
                // Gone now although it passed the check: the failed batch had already moved it.
                Err(_) if fs::symlink_metadata(path).is_err() => Outcome::Deleted,
                Err(e) => Outcome::Failed(e.to_string()),
            };
            (i, outcome)
        })
        .collect()
}

/// `Err` carries the final outcome of an item that must not be moved (or, in a dry run, can be).
fn check(finding: &Finding, dry_run: bool) -> std::result::Result<(), Outcome> {
    if finding.confidence == Confidence::Info || finding.category == Category::Bloatware {
        return Err(Outcome::NotDeletable);
    }
    let path = &finding.path;
    // Checked before touching the disk, so a protected path is refused even if it is gone.
    if path.as_os_str().is_empty() || is_protected(path) {
        return Err(Outcome::Protected);
    }
    let Ok(meta) = fs::symlink_metadata(path) else { return Err(Outcome::Missing) };
    // Links and junctions are never followed or removed (Rust reports both as symlinks).
    if meta.file_type().is_symlink() {
        return Err(Outcome::Changed);
    }
    let unchanged = if meta.is_dir() {
        dir_size(path) == Some(finding.size)
    } else {
        meta.len() == finding.size && meta.modified().ok() == finding.modified
    };
    if !unchanged {
        return Err(Outcome::Changed);
    }
    if dry_run {
        return Err(Outcome::WouldDelete);
    }
    Ok(())
}

/// Total size of the files below `dir`, read in parallel the way the scan reads them, so links
/// are skipped. `None` if anything cannot be read, which counts as a change.
fn dir_size(dir: &Path) -> Option<u64> {
    let everything = walk(dir, &Cancel::default(), |_, _, _, _| {}, |_| {}, |_| false);
    everything.errors.is_empty().then(|| everything.files.iter().map(|f| f.size).sum())
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
