//! Folder sizes for the map view, and the squarified treemap layout.
//!
//! The tree is built once from the scan and keeps one small node per folder, not per file,
//! so it stays compact on large disks.

use crate::report::{Confidence, Finding};
use crate::scan::Index;
use std::collections::HashMap;
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
}

pub struct FolderTree {
    root: PathBuf,
    nodes: Vec<FolderNode>,
}

impl FolderTree {
    pub const ROOT: usize = 0;

    /// Aggregates the scanned files by folder and marks how much of each folder the findings cover.
    pub fn build(root: &Path, index: &Index, findings: &[Finding]) -> FolderTree {
        let (mut tree, mut lookup) = Self::empty(root);
        let nodes = &mut tree.nodes;

        for file in &index.files {
            let Some(dir) = file.path.parent() else { continue };
            let id = ensure_folder(nodes, &mut lookup, root, dir);
            nodes[id].files_size += file.size;
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
        let nodes = vec![FolderNode { name, parent: None, children: Vec::new(), files_size: 0, size: 0, safe: 0, review: 0 }];
        let lookup = HashMap::from([(root.to_path_buf(), Self::ROOT)]);
        (FolderTree { root: root.to_path_buf(), nodes }, lookup)
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
        nodes.push(FolderNode { name, parent: Some(parent_id), children: Vec::new(), files_size: 0, size: 0, safe: 0, review: 0 });
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
