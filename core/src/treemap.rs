//! Folder sizes for the map view, and the squarified treemap layout.
//!
//! The tree is built once from the scan and keeps one small node per folder, not per file,
//! so it stays compact on large disks.

use crate::report::{Confidence, Finding};
use crate::scan::Index;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub struct FolderNode {
    pub name: String,
    pub parent: Option<usize>,
    /// Child folders, largest first.
    pub children: Vec<usize>,
    /// Combined size of the files directly in this folder.
    pub files_size: u64,
    /// Size of everything below, including `files_size`.
    pub size: u64,
    /// Bytes of Safe and Review findings below; together never more than `size`.
    pub safe: u64,
    pub review: u64,
    /// The largest files directly in this folder, largest first. The rest only count towards `files_size`.
    pub files: Vec<FileLeaf>,
    /// Set when a finding covers this whole folder.
    pub flag: Option<Confidence>,
}

/// A file listed individually in the map.
pub struct FileLeaf {
    pub name: String,
    pub size: u64,
    /// Set when a finding covers this file.
    pub flag: Option<Confidence>,
}

/// Files kept per folder for the map; the smaller ones are shown as one block.
const FILES_PER_FOLDER: usize = 250;

pub struct FolderTree {
    root: PathBuf,
    nodes: Vec<FolderNode>,
    /// Changes whenever sizes change, so a layout computed from the tree can tell it is out of date.
    revision: u64,
}

impl FolderTree {
    pub const ROOT: usize = 0;

    /// Aggregates the scanned files by folder and marks how much of each folder the findings cover.
    pub fn build(root: &Path, index: &Index, findings: &[Finding]) -> FolderTree {
        let (mut tree, mut lookup) = Self::empty(root);
        let nodes = &mut tree.nodes;

        // Safe wins over Review where both cover the same path.
        let mut flags: HashMap<&Path, Confidence> = HashMap::new();
        for finding in findings {
            match finding.confidence {
                Confidence::Safe => {
                    flags.insert(finding.path.as_path(), Confidence::Safe);
                }
                Confidence::Review => {
                    flags.entry(finding.path.as_path()).or_insert(Confidence::Review);
                }
                Confidence::Info => {}
            }
        }
        let dirs = index.dir_paths();
        // A folder gets a node only once a file shows up in it or below it.
        let mut node_of: Vec<Option<usize>> = vec![None; dirs.len()];
        for file in &index.files {
            let dir = file.dir as usize;
            let id = *node_of[dir].get_or_insert_with(|| ensure_folder(nodes, &mut lookup, root, &dirs[dir]));
            nodes[id].files_size += file.size;
            let name = file.name.to_string_lossy().into_owned();
            let flag = flags.get(dirs[dir].join(&*file.name).as_path()).copied();
            let files = &mut nodes[id].files;
            files.push(FileLeaf { name, size: file.size, flag });
            if files.len() >= FILES_PER_FOLDER * 2 {
                files.sort_by_key(|f| std::cmp::Reverse(f.size));
                files.truncate(FILES_PER_FOLDER);
            }
        }
        for node in nodes.iter_mut() {
            node.files.sort_by_key(|f| std::cmp::Reverse(f.size));
            node.files.truncate(FILES_PER_FOLDER);
            node.files.shrink_to_fit();
        }
        for (path, id) in &lookup {
            nodes[*id].flag = flags.get(path.as_path()).copied();
        }

        let mut direct_safe = vec![0u64; nodes.len()];
        let mut direct_review = vec![0u64; nodes.len()];
        for finding in findings {
            if finding.path.as_os_str().is_empty() {
                continue;
            }
            let id = lookup.get(finding.path.as_path()).or_else(|| finding.path.parent().and_then(|p| lookup.get(p)));
            match (id, finding.confidence) {
                (Some(&id), Confidence::Safe) => direct_safe[id] += finding.size,
                (Some(&id), Confidence::Review) => direct_review[id] += finding.size,
                _ => {}
            }
        }

        // Folders are created before their children, so walking backwards adds each total to its parent.
        for (i, node) in nodes.iter_mut().enumerate() {
            node.size = node.files_size;
            node.safe = direct_safe[i];
            node.review = direct_review[i];
        }
        for i in (1..nodes.len()).rev() {
            let (size, safe, review) = (nodes[i].size, nodes[i].safe, nodes[i].review);
            if let Some(parent) = nodes[i].parent {
                nodes[parent].size += size;
                nodes[parent].safe += safe;
                nodes[parent].review += review;
            }
        }
        for i in 0..nodes.len() {
            // Overlapping findings (a duplicate inside a flagged folder) must not exceed the folder.
            nodes[i].safe = nodes[i].safe.min(nodes[i].size);
            nodes[i].review = nodes[i].review.min(nodes[i].size - nodes[i].safe);
            let mut children = std::mem::take(&mut nodes[i].children);
            children.sort_by_key(|c| std::cmp::Reverse(nodes[*c].size));
            nodes[i].children = children;
        }
        tree
    }

    /// A tree holding only the root folder, and the lookup from folder path to node.
    fn empty(root: &Path) -> (FolderTree, HashMap<PathBuf, usize>) {
        let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
        let nodes = vec![FolderNode { name, parent: None, children: Vec::new(), files_size: 0, size: 0, safe: 0, review: 0, files: Vec::new(), flag: None }];
        let lookup = HashMap::from([(root.to_path_buf(), Self::ROOT)]);
        (FolderTree { root: root.to_path_buf(), nodes, revision: 0 }, lookup)
    }

    /// The subfolders of `id` with their sizes, largest first. Trees from [`FolderTree::build`] keep them
    /// sorted already; a [`LiveTree`] does not.
    pub fn children_by_size(&self, id: usize) -> Vec<(usize, u64)> {
        let mut children: Vec<(usize, u64)> = self.nodes[id].children.iter().map(|c| (*c, self.nodes[*c].size)).collect();
        children.sort_by_key(|c| std::cmp::Reverse(c.1));
        children
    }

    pub fn node(&self, id: usize) -> &FolderNode {
        &self.nodes[id]
    }

    /// Full path of a folder.
    pub fn path(&self, id: usize) -> PathBuf {
        let mut names = Vec::new();
        let mut current = id;
        while let Some(parent) = self.nodes[current].parent {
            names.push(self.nodes[current].name.as_str());
            current = parent;
        }
        names.iter().rev().fold(self.root.clone(), |path, name| path.join(name))
    }

    /// Changes whenever the sizes in the tree change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Position of the file called `name` among the files listed for `folder`.
    pub fn file_index(&self, folder: usize, name: &OsStr) -> Option<usize> {
        self.nodes[folder].files.iter().position(|f| OsStr::new(&f.name) == name)
    }

    /// The deepest known folder on `path`, and whether the whole path is a known folder.
    pub fn locate(&self, path: &Path) -> (usize, bool) {
        let Ok(relative) = path.strip_prefix(&self.root) else { return (Self::ROOT, false) };
        let mut current = Self::ROOT;
        for part in relative.components() {
            let next = self.nodes[current].children.iter().copied().find(|c| OsStr::new(&self.nodes[*c].name) == part.as_os_str());
            match next {
                Some(child) => current = child,
                None => return (current, false),
            }
        }
        (current, true)
    }

    /// Removes several findings at once. A path inside another removed path is already gone with it, and a
    /// path listed twice counts once.
    pub fn remove_all(&mut self, removed: &[(&Path, u64, Confidence)]) {
        let paths: HashSet<&Path> = removed.iter().map(|r| r.0).collect();
        let mut seen: HashSet<&Path> = HashSet::new();
        for (path, size, confidence) in removed {
            if !path.ancestors().skip(1).any(|a| paths.contains(a)) && seen.insert(path) {
                self.remove(path, *size, *confidence);
            }
        }
    }

    /// Takes a finding that was moved away out of the sizes, so the map stays right without a new scan.
    /// A path that is a known folder removes the whole folder; any other path is a file in the deepest known folder.
    pub fn remove(&mut self, path: &Path, size: u64, confidence: Confidence) {
        self.revision += 1;
        let (id, is_folder) = self.locate(path);
        let (removed, safe, review, mut cursor) = if is_folder {
            if id == Self::ROOT {
                return;
            }
            let node = &self.nodes[id];
            let (removed, safe, review, parent) = (node.size, node.safe, node.review, node.parent);
            let mut stack = vec![id];
            while let Some(n) = stack.pop() {
                let node = &mut self.nodes[n];
                (node.size, node.files_size, node.safe, node.review) = (0, 0, 0, 0);
                node.files.clear();
                stack.extend(node.children.iter().copied());
            }
            (removed, safe, review, parent)
        } else {
            let node = &mut self.nodes[id];
            node.files_size = node.files_size.saturating_sub(size);
            if let Some(name) = path.file_name() {
                if let Some(at) = node.files.iter().position(|f| OsStr::new(&f.name) == name) {
                    node.files.remove(at);
                }
            }
            let (safe, review) = match confidence {
                Confidence::Safe => (size, 0),
                Confidence::Review => (0, size),
                _ => (0, 0),
            };
            (size, safe, review, Some(id))
        };
        while let Some(a) = cursor {
            let node = &mut self.nodes[a];
            node.size = node.size.saturating_sub(removed);
            node.safe = node.safe.saturating_sub(safe).min(node.size);
            node.review = node.review.saturating_sub(review).min(node.size - node.safe);
            cursor = node.parent;
            let mut children = std::mem::take(&mut self.nodes[a].children);
            children.sort_by_key(|c| std::cmp::Reverse(self.nodes[*c].size));
            self.nodes[a].children = children;
        }
    }

    /// The folders from the scan root down to `id`, root first.
    pub fn trail(&self, id: usize) -> Vec<usize> {
        let mut trail = vec![id];
        while let Some(parent) = self.nodes[*trail.last().unwrap()].parent {
            trail.push(parent);
        }
        trail.reverse();
        trail
    }
}

/// A [`FolderTree`] that grows file by file while a scan runs, so a map can be drawn from it before the
/// scan ends. Sizes are kept up to date on every ancestor; children are not kept sorted (see
/// [`FolderTree::children_by_size`]) and there are no findings yet.
pub struct LiveTree {
    tree: FolderTree,
    lookup: HashMap<PathBuf, usize>,
    /// Files arrive folder by folder, so the previous folder usually answers without a lookup.
    last: Option<(PathBuf, usize)>,
}

impl LiveTree {
    pub fn new(root: &Path) -> LiveTree {
        let (tree, lookup) = FolderTree::empty(root);
        LiveTree { tree, lookup, last: None }
    }

    pub fn add(&mut self, path: &Path, size: u64) {
        let Some(dir) = path.parent() else { return };
        let id = match &self.last {
            Some((last, id)) if last == dir => *id,
            _ => {
                let id = ensure_folder(&mut self.tree.nodes, &mut self.lookup, &self.tree.root, dir);
                self.last = Some((dir.to_path_buf(), id));
                id
            }
        };
        let nodes = &mut self.tree.nodes;
        nodes[id].files_size += size;
        let mut current = Some(id);
        while let Some(i) = current {
            nodes[i].size += size;
            current = nodes[i].parent;
        }
    }

    pub fn tree(&self) -> &FolderTree {
        &self.tree
    }
}

fn ensure_folder(nodes: &mut Vec<FolderNode>, lookup: &mut HashMap<PathBuf, usize>, root: &Path, dir: &Path) -> usize {
    if let Some(&id) = lookup.get(dir) {
        return id;
    }
    if !dir.starts_with(root) {
        return FolderTree::ROOT;
    }
    let mut missing = Vec::new();
    let mut current = dir;
    while !lookup.contains_key(current) {
        missing.push(current);
        match current.parent() {
            Some(parent) => current = parent,
            None => return FolderTree::ROOT,
        }
    }
    let mut parent_id = lookup[current];
    for path in missing.into_iter().rev() {
        let id = nodes.len();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        nodes.push(FolderNode { name, parent: Some(parent_id), children: Vec::new(), files_size: 0, size: 0, safe: 0, review: 0, files: Vec::new(), flag: None });
        nodes[parent_id].children.push(id);
        lookup.insert(path.to_path_buf(), id);
        parent_id = id;
    }
    parent_id
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Lays out `sizes` (largest first) inside `bounds` as a squarified treemap: each rectangle's
/// area is proportional to its size and the shapes stay as close to square as possible.
/// Entries with size 0 get an empty rectangle.
pub fn squarify(sizes: &[u64], bounds: Rect) -> Vec<Rect> {
    let empty = Rect { x: bounds.x, y: bounds.y, w: 0.0, h: 0.0 };
    let mut rects = vec![empty; sizes.len()];
    let used = sizes.iter().take_while(|s| **s > 0).count();
    let total: f64 = sizes[..used].iter().map(|s| *s as f64).sum();
    if used == 0 || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return rects;
    }
    let scale = f64::from(bounds.w) * f64::from(bounds.h) / total;
    let areas: Vec<f64> = sizes[..used].iter().map(|s| *s as f64 * scale).collect();

    let mut rest = bounds;
    let mut start = 0;
    while start < used {
        let side = f64::from(rest.w.min(rest.h));
        let mut end = start + 1;
        let mut sum = areas[start];
        while end < used && worst(&areas[start..=end], sum + areas[end], side) <= worst(&areas[start..end], sum, side) {
            sum += areas[end];
            end += 1;
        }
        let thickness = (sum / side) as f32;
        let along_column = rest.w >= rest.h;
        let mut offset = 0.0f32;
        for i in start..end {
            let length = (areas[i] / f64::from(thickness)) as f32;
            rects[i] = if along_column {
                Rect { x: rest.x, y: rest.y + offset, w: thickness, h: length }
            } else {
                Rect { x: rest.x + offset, y: rest.y, w: length, h: thickness }
            };
            offset += length;
        }
        if along_column {
            rest.x += thickness;
            rest.w -= thickness;
        } else {
            rest.y += thickness;
            rest.h -= thickness;
        }
        start = end;
    }
    rects
}

/// Worst aspect ratio in a row of `areas` laid along a side of length `side`.
fn worst(areas: &[f64], sum: f64, side: f64) -> f64 {
    let (min, max) = areas.iter().fold((f64::MAX, 0.0f64), |(lo, hi), a| (lo.min(*a), hi.max(*a)));
    let square = side * side;
    (square * max / (sum * sum)).max(sum * sum / (square * min))
}

/// What a cell of [`layout`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Folder(usize),
    File { folder: usize, index: usize },
    /// The files of a folder that are too small to be listed one by one.
    Rest(usize),
}

/// One rectangle of the nested map. A folder's cell comes before the cells inside it.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub rect: Rect,
    pub kind: CellKind,
    pub size: u64,
    /// Set when a finding covers this cell, directly or through a folder above it.
    pub flag: Option<Confidence>,
    /// Whether the top of a folder's cell is kept free for its name.
    pub header: bool,
}

/// Lays out `folder` inside `bounds` with every subfolder and every listed file as a rectangle, folders
/// nested inside their parents. Rectangles narrower than `min_side` are left out; a folder that is too
/// small to show its contents stays one rectangle. `inherited` is the flag of `folder` and the folders above it.
pub fn layout(tree: &FolderTree, folder: usize, bounds: Rect, inherited: Option<Confidence>, header: f32, min_side: f32) -> Vec<Cell> {
    let mut cells = Vec::new();
    fill(tree, folder, bounds, inherited, header, min_side, &mut cells);
    cells
}

fn fill(tree: &FolderTree, folder: usize, bounds: Rect, inherited: Option<Confidence>, header: f32, min_side: f32, out: &mut Vec<Cell>) {
    let node = &tree.nodes[folder];
    let mut items: Vec<(CellKind, u64, Option<Confidence>)> = Vec::with_capacity(node.children.len() + node.files.len() + 1);
    for &child in &node.children {
        let n = &tree.nodes[child];
        if n.size > 0 {
            items.push((CellKind::Folder(child), n.size, n.flag.or(inherited)));
        }
    }
    for (index, file) in node.files.iter().enumerate() {
        items.push((CellKind::File { folder, index }, file.size, file.flag.or(inherited)));
    }
    let listed: u64 = node.files.iter().map(|f| f.size).sum();
    let rest = node.files_size.saturating_sub(listed);
    if rest > 0 {
        items.push((CellKind::Rest(folder), rest, inherited));
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.1));
    let sizes: Vec<u64> = items.iter().map(|item| item.1).collect();
    for ((kind, size, flag), rect) in items.into_iter().zip(squarify(&sizes, bounds)) {
        if rect.w < min_side || rect.h < min_side {
            continue;
        }
        let mut cell = Cell { rect, kind, size, flag, header: false };
        let CellKind::Folder(child) = kind else {
            out.push(cell);
            continue;
        };
        const PAD: f32 = 2.0;
        let band = if rect.h >= header * 3.0 && rect.w >= header * 3.0 { header } else { 0.0 };
        let top = rect.y + if band > 0.0 { band } else { PAD };
        let inner = Rect { x: rect.x + PAD, y: top, w: rect.w - 2.0 * PAD, h: rect.y + rect.h - PAD - top };
        cell.header = band > 0.0;
        out.push(cell);
        if inner.w >= min_side * 2.0 && inner.h >= min_side * 2.0 {
            fill(tree, child, inner, flag, header, min_side, out);
        }
    }
}
