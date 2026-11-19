use crate::protect::is_protected;
use crate::report::{Category, Confidence, Finding};
use crate::scan::{Cancel, FileEntry, Index, ScanError};
use blake3::{Hash, Hasher};
use rayon::prelude::*;
use same_file::Handle;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Bytes hashed before committing to a full read of a file.
const PARTIAL_BYTES: u64 = 64 * 1024;

/// Files with identical content that are distinct on disk (hard links count once).
pub struct DupeGroup {
    pub size: u64,
    /// The first entry is the copy that is kept; only the others are ever offered.
    pub files: Vec<PathBuf>,
}

#[derive(Default)]
pub struct Duplicates {
    pub groups: Vec<DupeGroup>,
    pub errors: Vec<ScanError>,
    pub cancelled: bool,
}

/// Finds files with identical content: by size, then a hash of the first 64 KiB,
/// then a hash of the whole file. Empty files are ignored.
pub fn find_duplicates(index: &Index, cancel: &Cancel) -> Duplicates {
    let errors = Mutex::new(Vec::new());

    let mut by_size: HashMap<u64, Vec<&FileEntry>> = HashMap::new();
    for file in index.files.iter().filter(|f| f.size > 0) {
        by_size.entry(file.size).or_default().push(file);
    }
    let candidates: Vec<&FileEntry> = by_size.into_values().filter(|g| g.len() > 1).flatten().collect();

    let partial = hash_groups(candidates, Some(PARTIAL_BYTES), cancel, &errors);
    // Hard links share their content, so they are collapsed before the costly full read.
    let distinct: Vec<&FileEntry> =
        partial.into_iter().flat_map(|g| distinct_on_disk(g, &errors)).collect();
    let full = hash_groups(distinct, None, cancel, &errors);

    let errors = errors.into_inner().unwrap_or_default();
    if cancel.is_cancelled() {
        return Duplicates { groups: Vec::new(), errors, cancelled: true };
    }
    Duplicates { groups: full.into_iter().map(group).collect(), errors, cancelled: false }
}

impl DupeGroup {
    /// Every copy except the kept one that may be deleted. Protected paths are left out.
    pub fn findings(&self) -> Vec<Finding> {
        let kept = &self.files[0];
        self.files[1..]
            .iter()
            .filter(|path| !is_protected(path))
            .map(|path| Finding {
                path: path.clone(),
                size: self.size,
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
fn group(files: Vec<&FileEntry>) -> DupeGroup {
    let size = files[0].size;
    let mut files = files;
    files.sort_by_cached_key(|f| (!is_protected(&f.path), f.modified, f.path.clone()));
    DupeGroup { size, files: files.into_iter().map(|f| f.path.clone()).collect() }
}

/// Hashes files in parallel and keeps only the groups that still hold two or more.
fn hash_groups<'a>(
    files: Vec<&'a FileEntry>,
    limit: Option<u64>,
    cancel: &Cancel,
    errors: &Mutex<Vec<ScanError>>,
) -> Vec<Vec<&'a FileEntry>> {
    let hashed: Vec<((u64, Hash), &FileEntry)> = files
        .into_par_iter()
        .filter_map(|file| {
            if cancel.is_cancelled() {
                return None;
            }
            match hash_file(&file.path, limit) {
                Ok(hash) => Some(((file.size, hash), file)),
                Err(e) => {
                    record(errors, &file.path, &e);
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
fn distinct_on_disk<'a>(files: Vec<&'a FileEntry>, errors: &Mutex<Vec<ScanError>>) -> Vec<&'a FileEntry> {
    let mut seen = HashSet::new();
    let distinct: Vec<&FileEntry> = files
        .into_iter()
        .filter(|file| match Handle::from_path(&file.path) {
            Ok(handle) => seen.insert(handle),
            Err(e) => {
                record(errors, &file.path, &e);
                false
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
