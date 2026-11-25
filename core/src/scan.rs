use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::protect::is_skippable_dir;

/// Cooperative cancellation flag, safe to share between threads.
#[derive(Default)]
pub struct Cancel(AtomicBool);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// shortcut: a full `PathBuf` per file (~100 B each); store a directory table and bare
// names if memory on very large disks becomes a problem.
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
}

pub struct ScanError {
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Default)]
pub struct Index {
    pub files: Vec<FileEntry>,
    pub errors: Vec<ScanError>,
    pub cancelled: bool,
}

/// Walks `root` in parallel and indexes every regular file.
/// Links and junctions are not entered. Unreadable entries are recorded in
/// `Index::errors` and never abort the scan.
/// `progress` receives the number of files indexed so far, their total size in bytes and the file just
/// indexed, once per file.
pub fn scan(root: &Path, cancel: &Cancel, progress: impl FnMut(u64, u64, &FileEntry)) -> Index {
    scan_with_folders(root, cancel, progress, |_| {})
}

/// Like [`scan`], and also calls `on_folder` with every folder the walk enters.
/// Protected folders that cannot yield findings are not entered (see [`is_skippable_dir`]).
pub fn scan_with_folders(
    root: &Path,
    cancel: &Cancel,
    progress: impl FnMut(u64, u64, &FileEntry),
    on_folder: impl FnMut(&Path),
) -> Index {
    walk(root, cancel, progress, on_folder, is_skippable_dir)
}

/// The walk behind [`scan_with_folders`], with the folder-skipping rule passed in.
/// The root itself is never skipped, only folders below it.
pub fn walk(
    root: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(u64, u64, &FileEntry),
    mut on_folder: impl FnMut(&Path),
    skip: fn(&Path) -> bool,
) -> Index {
    let mut index = Index::default();
    let mut bytes = 0u64;
    let walker = jwalk::WalkDir::new(root).process_read_dir(move |depth, _, _, children| {
        // `depth` is `None` for the pseudo listing that holds only the root itself.
        if depth.is_none() {
            return;
        }
        children.retain(|c| !c.as_ref().is_ok_and(|e| e.file_type().is_dir() && skip(&e.path())));
    });
    for entry in walker {
        if cancel.is_cancelled() {
            index.cancelled = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                index.errors.push(ScanError { path: e.path().map(Path::to_path_buf), message: e.to_string() });
                continue;
            }
        };
        // A directory that could not be listed is reported on the directory entry itself.
        if let Some(e) = entry.read_children.as_ref().and_then(|r| r.error()) {
            index.errors.push(ScanError { path: Some(entry.path()), message: e.to_string() });
        }
        if entry.file_type().is_dir() {
            on_folder(&entry.path());
        }
        if !entry.file_type().is_file() {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => {
                index.files.push(FileEntry {
                    path: entry.path(),
                    size: meta.len(),
                    modified: meta.modified().ok(),
                    accessed: meta.accessed().ok(),
                });
                bytes += meta.len();
                progress(index.files.len() as u64, bytes, index.files.last().expect("just pushed"));
            }
            Err(e) => index.errors.push(ScanError { path: Some(entry.path()), message: e.to_string() }),
        }
    }
    index
}
