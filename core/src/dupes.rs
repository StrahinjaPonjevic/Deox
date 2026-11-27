use crate::protect::ScanGuard;
use crate::report::{Category, Confidence, Finding};
use crate::scan::{is_dot_name, Cancel, FileEntry, Index, ScanError};
use blake3::{Hash, Hasher};
use rayon::prelude::*;
use same_file::Handle;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

/// Bytes hashed before committing to a full read of a file.
const PARTIAL_BYTES: u64 = 64 * 1024;

/// Files with identical content that are distinct on disk (hard links count once).
pub struct DupeGroup {
    pub size: u64,
    /// The first entry is the copy that is kept; only the others are ever offered.
    pub files: Vec<DupeFile>,
}

/// A copy as it was when scanned; deletion re-checks it against the disk.
pub struct DupeFile {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    /// Never offered for deletion.
    pub protected: bool,
}

#[derive(Default)]
pub struct Duplicates {
    pub groups: Vec<DupeGroup>,
    pub errors: Vec<ScanError>,
    pub cancelled: bool,
}

/// Finds files with identical content: by size, then a hash of the first 64 KiB,
/// then a hash of the whole file. Files smaller than `min_size` bytes and empty files
/// are ignored. `progress(stage, done, total)` is called from worker threads.
pub fn find_duplicates(
    index: &Index,
    cancel: &Cancel,
    min_size: u64,
    progress: &(dyn Fn(&str, u64, u64) + Sync),
) -> Duplicates {
    let errors = Mutex::new(Vec::new());

    let mut by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    // Copies a tool keeps under dot names (`.git`, editor extensions) are not spare copies.
    let in_dot_folder = index.in_dot_folder();
    let comparable = |f: &&FileEntry| {
        f.size > 0 && f.size >= min_size && !in_dot_folder[f.dir as usize] && !is_dot_name(&f.name)
    };
    for file in index.files.iter().filter(comparable) {
        by_size.entry(file.size).or_default().push(file);
    }
    let candidates: Vec<&FileEntry> = by_size.into_values().filter(|g| g.len() > 1).flatten().collect();

    let partial = hash_groups(index, candidates, Some(PARTIAL_BYTES),cancel, &errors, &Stage::new("Comparing file beginnings", progress));
    // Hard links share their content, so they are collapsed before the costly full read.
    let stage = Stage::new("Checking for hard links", progress);
    stage.total.store(partial.iter().map(|g| g.len() as u64).sum(), Ordering::Relaxed);
    let distinct: Vec<&FileEntry> =
        partial.into_par_iter().flat_map_iter(|g| distinct_on_disk(index, g, cancel, &errors, &stage)).collect();
    let full = hash_groups(index, distinct, None,cancel, &errors, &Stage::new("Comparing whole files", progress));

    let errors = errors.into_inner().unwrap_or_default();
    if cancel.is_cancelled() {
        return Duplicates { groups: Vec::new(), errors, cancelled: true };
    }
    let guard = ScanGuard::new(&index.root);
    let groups = full.into_iter().map(|g| group(index, &guard, g)).collect();
    Duplicates { groups, errors, cancelled: false }
}

/// Progress of one phase, reported every few hundred files.
struct Stage<'a> {
    label: &'static str,
    total: AtomicU64,
    done: AtomicU64,
    report: &'a (dyn Fn(&str, u64, u64) + Sync),
}

impl<'a> Stage<'a> {
    fn new(label: &'static str, report: &'a (dyn Fn(&str, u64, u64) + Sync)) -> Self {
        Stage { label, total: AtomicU64::new(0), done: AtomicU64::new(0), report }
    }

    fn tick(&self) {
        let done = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total.load(Ordering::Relaxed);
        if done.is_multiple_of(256) || done == total {
            (self.report)(self.label, done, total);
        }
    }
}

impl DupeGroup {
    /// Every copy except the kept one that may be deleted. Protected paths are left out.
    pub fn findings(&self) -> Vec<Finding> {
        let kept = &self.files[0].path;
        self.files[1..]
            .iter()
            .filter(|f| !f.protected)
            .map(|f| Finding {
                path: f.path.clone(),
                size: self.size,
                modified: f.modified,
                category: Category::Duplicate,
                confidence: Confidence::Review,
                rule: "Duplicate files".to_string(),
                reason: format!("Identical content to {}", kept.display()),
                uninstall: None,
            })
            .collect()
    }
}

/// Orders a group so the copy to keep comes first: a protected copy (it can never be
/// deleted anyway), otherwise the oldest.
fn group(index: &Index, guard: &ScanGuard, files: Vec<&FileEntry>) -> DupeGroup {
    let size = files[0].size;
    let mut files: Vec<DupeFile> = files
        .into_iter()
        .map(|f| {
            let path = index.file_path(f);
            DupeFile { protected: guard.protects(&path), path, modified: f.modified }
        })
        .collect();
    files.sort_by_cached_key(|f| (!f.protected, f.modified, f.path.clone()));
    DupeGroup { size, files }
}

/// Hashes files in parallel and keeps only the groups that still hold two or more.
fn hash_groups<'a>(
    index: &Index,
    files: Vec<&'a FileEntry>,
    limit: Option<u64>,
    cancel: &Cancel,
    errors: &Mutex<Vec<ScanError>>,
    stage: &Stage,
) -> Vec<Vec<&'a FileEntry>> {
    stage.total.store(files.len() as u64, Ordering::Relaxed);
    let hashed: Vec<((u64, Hash), &FileEntry)> = files
        .into_par_iter()
        .filter_map(|file| {
            if cancel.is_cancelled() {
                return None;
            }
            let path = index.file_path(file);
            let result = hash_file(&path, limit);
            stage.tick();
            match result {
                Ok(hash) => Some(((file.size, hash), file)),
                Err(e) => {
                    record(errors, &path, &e);
                    None
                }
            }
        })
        .collect();
    let mut groups: HashMap<(u64, Hash), Vec<&FileEntry>> = HashMap::new();
    for (key, file) in hashed {
        groups.entry(key).or_default().push(file);
    }
    groups.into_values().filter(|g| g.len() > 1).collect()
}

/// Drops files that are the same on-disk file as one already seen (hard links).
/// Returns nothing if fewer than two distinct files remain.
fn distinct_on_disk<'a>(
    index: &Index,
    files: Vec<&'a FileEntry>,
    cancel: &Cancel,
    errors: &Mutex<Vec<ScanError>>,
    stage: &Stage,
) -> Vec<&'a FileEntry> {
    let mut seen = HashSet::new();
    let distinct: Vec<&FileEntry> = files
        .into_iter()
        .filter(|file| {
            stage.tick();
            if cancel.is_cancelled() {
                return false;
            }
            let path = index.file_path(file);
            match Handle::from_path(&path) {
                Ok(handle) => seen.insert(handle),
                Err(e) => {
                    record(errors, &path, &e);
                    false
                }
            }
        })
        .collect();
    if distinct.len() > 1 {
        distinct
    } else {
        Vec::new()
    }
}

fn hash_file(path: &Path, limit: Option<u64>) -> io::Result<Hash> {
    let file = File::open(path)?;
    let mut hasher = Hasher::new();
    match limit {
        Some(n) => hasher.update_reader(file.take(n))?,
        None => hasher.update_reader(file)?,
    };
    Ok(hasher.finalize())
}

fn record(errors: &Mutex<Vec<ScanError>>, path: &Path, e: &io::Error) {
    if let Ok(mut list) = errors.lock() {
        list.push(ScanError { path: Some(path.to_path_buf()), message: e.to_string() });
    }
}
