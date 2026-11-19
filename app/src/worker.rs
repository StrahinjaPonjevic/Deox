//! Background jobs, so the window stays responsive. Results come back as messages.

use deox_core::delete::{default_log_path, delete, DeleteReport};
use eframe::egui;
use deox_core::dupes::find_duplicates;
use deox_core::programs::{installed_programs, AppxRegistry, BloatwareList, WindowsRegistry};
use deox_core::report::Finding;
use deox_core::rules::Rules;
use deox_core::scan::{scan, Cancel};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::SystemTime;

/// Smaller files are not worth comparing: they free next to no space and are the slowest part.
const MIN_DUPLICATE_SIZE: u64 = 64 * 1024;

pub struct Analysis {
    pub findings: Vec<Finding>,
    pub files: u64,
    /// Paths that could not be read (no permission, in use).
    pub unreadable: usize,
    pub cancelled: bool,
}

pub enum Msg {
    Status(String),
    Progress(u64),
    Analysis(Analysis),
    Deleted(DeleteReport),
    Failed(String),
}

/// Reports progress to the window and wakes it up.
#[derive(Clone)]
pub struct Notifier {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Notifier {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Notifier { tx, ctx }
    }

    fn send(&self, msg: Msg) {
        // The receiver is gone only when the window was closed.
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

pub fn spawn_analysis(root: PathBuf, cancel: Arc<Cancel>, notify: Notifier) {
    std::thread::spawn(move || match analyze(&root, &cancel, &notify) {
        Ok(analysis) => notify.send(Msg::Analysis(analysis)),
        Err(e) => notify.send(Msg::Failed(format!("{e:#}"))),
    });
}

pub fn spawn_delete(findings: Vec<Finding>, dry_run: bool, notify: Notifier) {
    std::thread::spawn(move || {
        let Some(log) = default_log_path() else {
            return notify.send(Msg::Failed("Cannot locate a folder for the action log.".into()));
        };
        match delete(&findings, dry_run, &log) {
            Ok(report) => notify.send(Msg::Deleted(report)),
            Err(e) => notify.send(Msg::Failed(format!("{e:#}"))),
        }
    });
}

fn analyze(root: &std::path::Path, cancel: &Cancel, notify: &Notifier) -> anyhow::Result<Analysis> {
    notify.send(Msg::Status("Scanning files…".into()));
    let index = scan(root, cancel, |seen| {
        if seen % 512 == 0 {
            notify.send(Msg::Progress(seen));
        }
    });
    let files = index.files.len() as u64;
    let mut unreadable = index.errors.len();
    let cancelled = |findings| Analysis { findings, files, unreadable, cancelled: true };
    if index.cancelled {
        return Ok(cancelled(Vec::new()));
    }

    notify.send(Msg::Progress(files));
    notify.send(Msg::Status("Applying cleanup rules…".into()));
    let mut findings = Rules::builtin()?.evaluate(&index, SystemTime::now());

    notify.send(Msg::Status("Looking for duplicate files…".into()));
    let dupes = find_duplicates(&index, cancel, MIN_DUPLICATE_SIZE, &|stage, done, total| {
        notify.send(Msg::Status(format!("{stage}… {done} of {total}")));
    });
    if dupes.cancelled {
        return Ok(cancelled(Vec::new()));
    }
    unreadable += dupes.errors.len();
    findings.extend(dupes.groups.iter().flat_map(|g| g.findings()));

    notify.send(Msg::Status("Reading installed programs…".into()));
    let mut programs = installed_programs(&WindowsRegistry);
    programs.extend(installed_programs(&AppxRegistry));
    findings.extend(BloatwareList::builtin()?.evaluate(&programs));

    Ok(Analysis { findings, files, unreadable, cancelled: false })
}
