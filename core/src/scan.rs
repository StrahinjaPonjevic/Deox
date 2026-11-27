use std::collections::HashMap;
use std::ffi::OsStr;
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

/// A folder in the index. A parent always comes before its subfolders.
pub struct DirEntry {
    /// `None` only for the scan root, which is the first entry.
    pub parent: Option<u32>,
    /// Empty for the scan root, whose path is [`Index::root`].
    pub name: Box<OsStr>,
}

pub struct FileEntry {
    /// Position of the containing folder in [`Index::dirs`].
    pub dir: u32,
    /// Not converted to text: a lossy name could point at a different file.
    pub name: Box<OsStr>,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

pub struct ScanError {
    pub path: Option<PathBuf>,
    pub message: String,
}

/// The scanned files. Folder paths are stored once, in a table, and files refer to them by position.
pub struct Index {
    pub root: PathBuf,
    pub dirs: Vec<DirEntry>,
    pub files: Vec<FileEntry>,
    pub errors: Vec<ScanError>,
    pub cancelled: bool,
}

impl Index {
    /// Full path of the folder at `id` in [`Index::dirs`].
    pub fn dir_path(&self, id: u32) -> PathBuf {
        let mut names = Vec::new();
        let mut dir = &self.dirs[id as usize];
        while let Some(parent) = dir.parent {
            names.push(Path::new(&dir.name));
            dir = &self.dirs[parent as usize];
        }
        let mut path = self.root.clone();
        path.extend(names.iter().rev());
        path
    }

    /// Full paths of all folders, in the order of [`Index::dirs`]. For passes over every file.
    pub fn dir_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::with_capacity(self.dirs.len());
        for dir in &self.dirs {
            let path = match dir.parent {
                Some(parent) => paths[parent as usize].join(&*dir.name),
                None => self.root.clone(),
            };
            paths.push(path);
        }
        paths
    }

    pub fn file_path(&self, file: &FileEntry) -> PathBuf {
        self.dir_path(file.dir).join(&*file.name)
    }
}

/// Builds an [`Index`] from full paths.
pub struct IndexBuilder {
    index: Index,
    ids: HashMap<PathBuf, u32>,
}

impl IndexBuilder {
    pub fn new(root: impl Into<PathBuf>) -> IndexBuilder {
        let root = root.into();
        let ids = HashMap::from([(root.clone(), 0)]);
        let dirs = vec![DirEntry { parent: None, name: OsStr::new("").into() }];
        IndexBuilder { index: Index { root, dirs, files: Vec::new(), errors: Vec::new(), cancelled: false }, ids }
    }

    /// The position of the folder at `path`, adding it and any missing parents.
    ///
    /// # Panics
    /// If `path` is not the root or below it.
    pub fn dir(&mut self, path: &Path) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            panic!("{} is outside the index root", path.display());
        };
        let parent = Some(self.dir(parent));
        let id = u32::try_from(self.index.dirs.len()).expect("too many folders");
        self.index.dirs.push(DirEntry { parent, name: name.into() });
        self.ids.insert(path.to_path_buf(), id);
        id
    }

    /// Adds the file at `path`, which must be below the root.
    pub fn file(&mut self, path: &Path, size: u64, modified: Option<SystemTime>) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            panic!("{} is not a file path", path.display());
        };
        let dir = self.dir(parent);
        self.index.files.push(FileEntry { dir, name: name.into(), size, modified });
    }

    pub fn finish(mut self) -> Index {
        self.index.files.shrink_to_fit();
        self.index.dirs.shrink_to_fit();
        self.index
    }
}

/// Walks `root` in parallel and indexes every regular file.
/// Links and junctions are not entered. Unreadable entries are recorded in
/// `Index::errors` and never abort the scan.
/// `progress` receives the number of files indexed so far, their total size in bytes, and the path
/// and size of the file just indexed, once per file.
pub fn scan(root: &Path, cancel: &Cancel, progress: impl FnMut(u64, u64, &Path, u64)) -> Index {
    scan_with_folders(root, cancel, progress, |_| {})
}

/// Like [`scan`], and also calls `on_folder` with every folder the walk enters.
/// Protected folders that cannot yield findings are not entered (see [`is_skippable_dir`]).
pub fn scan_with_folders(
    root: &Path,
    cancel: &Cancel,
    progress: impl FnMut(u64, u64, &Path, u64),
    on_folder: impl FnMut(&Path),
) -> Index {
    walk(root, cancel, progress, on_folder, is_skippable_dir)
}

/// The walk behind [`scan_with_folders`], with the folder-skipping rule passed in.
/// The root itself is never skipped, only folders below it.
pub fn walk(
    root: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(u64, u64, &Path, u64),
    mut on_folder: impl FnMut(&Path),
    skip: fn(&Path) -> bool,
) -> Index {
    let mut builder = IndexBuilder::new(root);
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
            builder.index.cancelled = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let path = e.path().map(Path::to_path_buf);
                builder.index.errors.push(ScanError { path, message: e.to_string() });
                continue;
            }
        };
        // A directory that could not be listed is reported on the directory entry itself.
        if let Some(e) = entry.read_children.as_ref().and_then(|r| r.error()) {
            builder.index.errors.push(ScanError { path: Some(entry.path()), message: e.to_string() });
        }
        if entry.file_type().is_dir() {
            let path = entry.path();
            on_folder(&path);
            if entry.depth() > 0 {
                builder.dir(&path);
            }
        }
        // The root is a folder; a file given as the root is not indexed.
        if entry.depth() == 0 || !entry.file_type().is_file() {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => {
                let path = entry.path();
                builder.file(&path, meta.len(), meta.modified().ok());
                bytes += meta.len();
                progress(builder.index.files.len() as u64, bytes, &path, meta.len());
            }
            Err(e) => builder.index.errors.push(ScanError { path: Some(entry.path()), message: e.to_string() }),
        }
    }
    builder.finish()
}
