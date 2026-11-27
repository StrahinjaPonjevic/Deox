use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::SystemTime;

use crate::listing::{read_folder, Entry};
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
    /// Set by [`Index::remove`]. The entry stays so that positions do not change.
    pub removed: bool,
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

    /// Takes out what is gone from disk. Each path is a file, or a folder with everything in it.
    pub fn remove(&mut self, gone: &HashSet<&Path>) {
        let paths = self.dir_paths();
        // Parents come first, so a removed folder passes the mark down.
        for (id, path) in paths.iter().enumerate() {
            let inherited = self.dirs[id].parent.is_some_and(|parent| self.dirs[parent as usize].removed);
            if inherited || gone.contains(path.as_path()) {
                self.dirs[id].removed = true;
            }
        }
        // File paths are only built in folders that held something that is gone.
        let beside: HashSet<&Path> = gone.iter().filter_map(|path| path.parent()).collect();
        let dirs = &self.dirs;
        self.files.retain(|file| {
            let id = file.dir as usize;
            let folder = paths[id].as_path();
            !dirs[id].removed && !(beside.contains(folder) && gone.contains(folder.join(&*file.name).as_path()))
        });
    }

    /// For each folder in [`Index::dirs`], whether it, or any folder above it, has a name starting
    /// with a dot. Tools keep their own data there: it is measured like everything else, but
    /// offered for removal only where a rule names it.
    pub fn in_dot_folder(&self) -> Vec<bool> {
        let mut inside: Vec<bool> = Vec::with_capacity(self.dirs.len());
        for dir in &self.dirs {
            inside.push(match dir.parent {
                Some(parent) => inside[parent as usize] || is_dot_name(&dir.name),
                // The folders above the root count too, so the answer for a file does not
                // depend on where the scan started.
                None => self.root.components().any(|c| matches!(c, Component::Normal(name) if is_dot_name(name))),
            });
        }
        inside
    }
}

pub(crate) fn is_dot_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b".")
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
        let dirs = vec![DirEntry { parent: None, name: OsStr::new("").into(), removed: false }];
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
        self.index.dirs.push(DirEntry { parent, name: name.into(), removed: false });
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
///
/// Folders are read in parallel, so their order differs between runs. The callbacks run on the
/// calling thread.
pub fn walk(
    root: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(u64, u64, &Path, u64),
    mut on_folder: impl FnMut(&Path),
    skip: fn(&Path) -> bool,
) -> Index {
    let mut builder = IndexBuilder::new(root);
    // A link given as the root is not followed either.
    if std::fs::symlink_metadata(root).is_ok_and(|m| m.is_symlink()) {
        return builder.finish();
    }
    let mut bytes = 0u64;
    let (sender, listings) = mpsc::channel();
    std::thread::scope(|threads| {
        threads.spawn(move || rayon::scope(|pool| read_tree(pool, root.to_path_buf(), cancel, skip, sender)));
        // Only this thread touches the index; the workers just read folders.
        for listing in listings {
            if cancel.is_cancelled() {
                break;
            }
            // Already known from its parent's listing, which always arrives first.
            let dir = builder.dir(&listing.path);
            on_folder(&listing.path);
            for folder in &listing.folders {
                builder.dir(folder);
            }
            if let Some(e) = listing.error {
                builder.index.errors.push(ScanError { path: Some(listing.path.clone()), message: e.to_string() });
            }
            for file in listing.files {
                if cancel.is_cancelled() {
                    break;
                }
                let path = listing.path.join(&file.name);
                let name = file.name.into_boxed_os_str();
                builder.index.files.push(FileEntry { dir, name, size: file.size, modified: file.modified });
                bytes += file.size;
                progress(builder.index.files.len() as u64, bytes, &path, file.size);
            }
        }
    });
    builder.index.cancelled = cancel.is_cancelled();
    builder.finish()
}

/// What one folder holds, as read by a worker.
struct Listing {
    path: PathBuf,
    files: Vec<Entry>,
    /// Subfolders that will be read too.
    folders: Vec<PathBuf>,
    error: Option<io::Error>,
}

/// Reads `path` and queues its subfolders on the pool.
fn read_tree<'s>(
    pool: &rayon::Scope<'s>,
    path: PathBuf,
    cancel: &'s Cancel,
    skip: fn(&Path) -> bool,
    sender: Sender<Listing>,
) {
    if cancel.is_cancelled() {
        return;
    }
    let (mut files, mut folders) = (Vec::new(), Vec::new());
    let error = read_folder(&path, cancel, |entry| {
        if !entry.is_dir {
            files.push(entry);
            return;
        }
        let folder = path.join(&entry.name);
        if !skip(&folder) {
            folders.push(folder);
        }
    })
    .err();
    let subfolders = folders.clone();
    // Sent before the subfolders are queued, so a parent always arrives before its children.
    if sender.send(Listing { path, files, folders, error }).is_err() {
        return;
    }
    for folder in subfolders {
        let sender = sender.clone();
        pool.spawn(move |pool| read_tree(pool, folder, cancel, skip, sender));
    }
}
