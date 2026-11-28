//! Folder and extension totals for the map view, and the squarified treemap layout.
//!
//! The tree keeps one small node per folder and lists only the largest files of each, so a map
//! of a large disk stays quick to draw. The full list of files stays in the [`Index`].

use crate::report::{Confidence, Finding};
use crate::scan::Index;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Stands for the files whose name has no extension.
pub const NO_EXTENSION: &str = "(no ext)";

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
    /// Files and folders anywhere below. Like `newest`, not tracked by a [`LiveTree`].
    pub file_count: u64,
    pub dir_count: u64,
    /// The latest change to any file below.
    pub newest: Option<SystemTime>,
    /// The largest files directly in this folder, largest first. The rest only count towards `files_size`.
    pub files: Vec<FileLeaf>,
    /// Set when a finding covers this whole folder.
    pub flag: Option<Confidence>,
}

impl FolderNode {
    fn new(name: String, parent: Option<usize>) -> FolderNode {
        FolderNode {
            name,
            parent,
            children: Vec::new(),
            files_size: 0,
            size: 0,
            safe: 0,
            review: 0,
            file_count: 0,
            dir_count: 0,
            newest: None,
            files: Vec::new(),
            flag: None,
        }
    }
}

/// A file listed individually in the map.
pub struct FileLeaf {
    pub name: String,
    pub size: u64,
    /// Set when a finding covers this file.
    pub flag: Option<Confidence>,
    /// Position in [`FolderTree::extensions`].
    pub extension: usize,
}

/// What all files with one extension take together.
pub struct ExtensionStat {
    /// Lowercase and without the dot, or [`NO_EXTENSION`].
    pub extension: Box<str>,
    pub size: u64,
    pub count: u64,
}

/// Files kept per folder for the map; the smaller ones are shown as one block.
const FILES_PER_FOLDER: usize = 250;

pub struct FolderTree {
    root: PathBuf,
    nodes: Vec<FolderNode>,
    extensions: Vec<ExtensionStat>,
    file_flags: FileFlags,
}

/// Flags of single files, by folder and then by name.
type FileFlags = HashMap<usize, HashMap<Box<OsStr>, Confidence>>;

fn root_name(root: &Path) -> String {
    root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Writes the extension of `name` into `out`, which is reused from file to file.
fn extension_of(name: &OsStr, out: &mut String) {
    out.clear();
    if let Some(extension) = Path::new(name).extension() {
        out.extend(extension.to_string_lossy().chars().flat_map(char::to_lowercase));
    }
    if out.is_empty() {
        out.push_str(NO_EXTENSION);
    }
}

impl FolderTree {
    pub const ROOT: usize = 0;

    /// Aggregates the scanned files by folder and by extension, and marks how much of each folder
    /// the findings cover. A node's position is its folder's position in [`Index::dirs`].
    pub fn build(index: &Index, findings: &[Finding]) -> FolderTree {
        let paths = index.dir_paths();
        let ids: HashMap<&Path, usize> =
            paths.iter().enumerate().filter(|(id, _)| !index.dirs[*id].removed).map(|(id, p)| (p.as_path(), id)).collect();

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
        // The same flags for files, found by folder and name, so no path is built per file.
        let mut file_flags: FileFlags = HashMap::new();
        for (path, flag) in &flags {
            let folder = path.parent().and_then(|parent| ids.get(parent));
            if let (Some(&folder), Some(name)) = (folder, path.file_name()) {
                file_flags.entry(folder).or_default().insert(name.into(), *flag);
            }
        }

        let mut nodes: Vec<FolderNode> = index
            .dirs
            .iter()
            .zip(&paths)
            .map(|(dir, path)| {
                let name = match dir.parent {
                    Some(_) => dir.name.to_string_lossy().into_owned(),
                    None => root_name(&index.root),
                };
                let mut node = FolderNode::new(name, dir.parent.map(|p| p as usize));
                node.flag = flags.get(path.as_path()).copied();
                node
            })
            .collect();
        for (id, dir) in index.dirs.iter().enumerate() {
            if let (Some(parent), false) = (dir.parent, dir.removed) {
                nodes[parent as usize].children.push(id);
            }
        }

        let mut extensions: Vec<ExtensionStat> = Vec::new();
        let mut extension_ids: HashMap<String, usize> = HashMap::new();
        let mut extension = String::new();
        for file in &index.files {
            let id = file.dir as usize;
            extension_of(&file.name, &mut extension);
            let kind = extension_ids.get(&extension).copied().unwrap_or_else(|| {
                extensions.push(ExtensionStat { extension: extension.as_str().into(), size: 0, count: 0 });
                extension_ids.insert(extension.clone(), extensions.len() - 1);
                extensions.len() - 1
            });
            extensions[kind].size += file.size;
            extensions[kind].count += 1;

            let node = &mut nodes[id];
            node.files_size += file.size;
            node.file_count += 1;
            node.newest = node.newest.max(file.modified);
            let name = file.name.to_string_lossy().into_owned();
            let flag = file_flags.get(&id).and_then(|names| names.get(&*file.name)).copied();
            node.files.push(FileLeaf { name, size: file.size, flag, extension: kind });
            if node.files.len() >= FILES_PER_FOLDER * 2 {
                node.files.sort_by_key(|f| std::cmp::Reverse(f.size));
                node.files.truncate(FILES_PER_FOLDER);
            }
        }

        // Extensions are listed largest first; the files learn their new positions.
        let mut order: Vec<(usize, ExtensionStat)> = extensions.into_iter().enumerate().collect();
        order.sort_by(|(_, a), (_, b)| b.size.cmp(&a.size).then_with(|| a.extension.cmp(&b.extension)));
        let mut position = vec![0; order.len()];
        for (new, (old, _)) in order.iter().enumerate() {
            position[*old] = new;
        }
        let extensions = order.into_iter().map(|(_, stat)| stat).collect();
        for node in &mut nodes {
            node.files.sort_by_key(|f| std::cmp::Reverse(f.size));
            node.files.truncate(FILES_PER_FOLDER);
            node.files.shrink_to_fit();
            for file in &mut node.files {
                file.extension = position[file.extension];
            }
        }

        for node in &mut nodes {
            node.size = node.files_size;
        }
        for finding in findings {
            let folder = ids.get(finding.path.as_path()).or_else(|| ids.get(finding.path.parent()?));
            match (folder, finding.confidence) {
                (Some(&id), Confidence::Safe) => nodes[id].safe += finding.size,
                (Some(&id), Confidence::Review) => nodes[id].review += finding.size,
                _ => {}
            }
        }

        // Parents come before their subfolders, so walking backwards adds each total to its parent.
        for i in (1..nodes.len()).rev() {
            let Some(parent) = nodes[i].parent.filter(|_| !index.dirs[i].removed) else { continue };
            let node = &nodes[i];
            let (size, safe, review) = (node.size, node.safe, node.review);
            let (files, dirs, newest) = (node.file_count, node.dir_count, node.newest);
            let parent = &mut nodes[parent];
            parent.size += size;
            parent.safe += safe;
            parent.review += review;
            parent.file_count += files;
            parent.dir_count += dirs + 1;
            parent.newest = parent.newest.max(newest);
        }
        for i in 0..nodes.len() {
            // Overlapping findings (a duplicate inside a flagged folder) must not exceed the folder.
            nodes[i].safe = nodes[i].safe.min(nodes[i].size);
            nodes[i].review = nodes[i].review.min(nodes[i].size - nodes[i].safe);
            let mut children = std::mem::take(&mut nodes[i].children);
            children.sort_by_key(|c| std::cmp::Reverse(nodes[*c].size));
            nodes[i].children = children;
        }
        FolderTree { root: index.root.clone(), nodes, extensions, file_flags }
    }

    /// Set when a finding names this file itself, whether or not the file is listed in its node.
    pub fn file_flag(&self, folder: usize, name: &OsStr) -> Option<Confidence> {
        self.file_flags.get(&folder).and_then(|names| names.get(name)).copied()
    }

    /// Every extension among the files, the one taking the most space first.
    pub fn extensions(&self) -> &[ExtensionStat] {
        &self.extensions
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
        let nodes = vec![FolderNode::new(root_name(root), None)];
        let tree = FolderTree { root: root.to_path_buf(), nodes, extensions: Vec::new(), file_flags: HashMap::new() };
        let lookup = HashMap::from([(root.to_path_buf(), FolderTree::ROOT)]);
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
        nodes.push(FolderNode::new(name, Some(parent_id)));
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
