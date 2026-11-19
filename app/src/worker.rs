//! Background jobs, so the window stays responsive. Results come back as messages.

use deox_core::delete::{default_log_path, delete_with_progress, DeleteReport};
use eframe::egui;
use deox_core::dupes::find_duplicates;
use deox_core::programs::{installed_programs, AppxRegistry, BloatwareList, WindowsRegistry};
use deox_core::report::Finding;
use deox_core::rules::Rules;
use deox_core::scan::{scan_with_folders, Cancel};
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
    /// The folder the scan is currently walking.
    Folder(String),
    /// Items settled so far and the total, while moving to the Recycle Bin.
    Moved(usize, usize),
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

/// Runs a job on its own thread. A panic becomes an error message; otherwise the window would wait forever.
fn run_job(notify: Notifier, job: impl FnOnce(&Notifier) + Send + 'static) {
    std::thread::spawn(move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&notify))).is_err() {
            notify.send(Msg::Failed("Deox hit an unexpected problem and stopped. Nothing else was changed.".into()));
        }
    });
}

pub fn spawn_analysis(root: PathBuf, cancel: Arc<Cancel>, notify: Notifier) {
    run_job(notify, move |notify| match analyze(&root, &cancel, notify) {
        Ok(analysis) => notify.send(Msg::Analysis(analysis)),
        Err(e) => notify.send(Msg::Failed(format!("The scan could not finish: {e:#}"))),
    });
}

pub fn spawn_delete(findings: Vec<Finding>, dry_run: bool, cancel: Arc<Cancel>, notify: Notifier) {
    run_job(notify, move |notify| {
        let Some(log) = default_log_path() else {
            return notify.send(Msg::Failed("Deox can't find a folder for its action log, so nothing was moved.".into()));
        };
        let report_progress = |done: usize, total: usize| {
            // Thousands of small items would otherwise flood the window with messages.
            if done == total || done.is_multiple_of(25) {
                notify.send(Msg::Moved(done, total));
            }
        };
        match delete_with_progress(&findings, dry_run, &log, &report_progress, &cancel) {
            Ok(report) => notify.send(Msg::Deleted(report)),
            Err(e) => notify.send(Msg::Failed(format!(
                "The move could not finish: {e:#}\nSome items may already be in the Recycle Bin. Scan again to refresh the list."
            ))),
        }
    });
}

fn analyze(root: &std::path::Path, cancel: &Cancel, notify: &Notifier) -> anyhow::Result<Analysis> {
    notify.send(Msg::Status("Step 1 of 4: scanning files…".into()));
    let mut folders = 0u32;
    let index = scan_with_folders(
        root,
        cancel,
        |seen| {
            if seen.is_multiple_of(512) {
                notify.send(Msg::Progress(seen));
            }
        },
        |folder| {
            // Folders are far more frequent than the window needs to redraw.
            folders = folders.wrapping_add(1);
            if folders.is_multiple_of(200) {
                notify.send(Msg::Folder(folder.display().to_string()));
            }
        },
    );
    let files = index.files.len() as u64;
    let mut unreadable = index.errors.len();
    let cancelled = |findings| Analysis { findings, files, unreadable, cancelled: true };
    if index.cancelled {
        return Ok(cancelled(Vec::new()));
    }

    notify.send(Msg::Progress(files));
    notify.send(Msg::Status("Step 2 of 4: applying cleanup rules…".into()));
    let mut findings = Rules::builtin()?.evaluate(&index, SystemTime::now());

    notify.send(Msg::Status("Step 3 of 4: looking for duplicate files…".into()));
    let dupes = find_duplicates(&index, cancel, MIN_DUPLICATE_SIZE, &|stage, done, total| {
        notify.send(Msg::Status(format!("Step 3 of 4: {}… {done} of {total}", stage.to_lowercase())));
    });
    if dupes.cancelled {
        return Ok(cancelled(Vec::new()));
    }
    unreadable += dupes.errors.len();
    findings.extend(dupes.groups.iter().flat_map(|g| g.findings()));

    notify.send(Msg::Status("Step 4 of 4: reading installed programs…".into()));
    let mut programs = installed_programs(&WindowsRegistry);
    programs.extend(installed_programs(&AppxRegistry));
    findings.extend(BloatwareList::builtin()?.evaluate(&programs));

    Ok(Analysis { findings, files, unreadable, cancelled: false })
}
