use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

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
/// `progress` receives the number of files indexed so far, once per file.
pub fn scan(root: &Path, cancel: &Cancel, mut progress: impl FnMut(u64)) -> Index {
    let mut index = Index::default();
    for entry in jwalk::WalkDir::new(root) {
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
                progress(index.files.len() as u64);
            }
            Err(e) => index.errors.push(ScanError { path: Some(entry.path()), message: e.to_string() }),
        }
    }
    index
}
