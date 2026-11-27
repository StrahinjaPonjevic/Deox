use deox_core::report::{Category, Confidence, Finding};
use deox_core::scan::{scan, Cancel};
use deox_core::treemap::{squarify, FolderTree, LiveTree, Rect};
use std::fs;
use std::path::Path;

fn write(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; len]).unwrap();
}

fn finding(path: &Path, size: u64, confidence: Confidence) -> Finding {
    Finding {
        path: path.to_path_buf(),
        size,
        modified: None,
        category: Category::Temp,
        confidence,
        rule: "test".into(),
        reason: "test".into(),
        uninstall: None,
    }
}

#[test]
fn folder_sizes_add_up_through_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("top.bin"), 5);
    write(&dir.path().join("a/one.bin"), 100);
    write(&dir.path().join("a/deep/two.bin"), 50);
    write(&dir.path().join("b/three.bin"), 20);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});

    let tree = FolderTree::build(dir.path(), &index, &[]);

    let root = tree.node(FolderTree::ROOT);
    assert_eq!(root.size, 175);
    assert_eq!(root.files_size, 5);
    let names: Vec<(&str, u64)> = root.children.iter().map(|c| (tree.node(*c).name.as_str(), tree.node(*c).size)).collect();
    assert_eq!(names, vec![("a", 150), ("b", 20)], "children are sorted largest first");
    assert_eq!(tree.path(root.children[0]), dir.path().join("a"));
}

#[test]
fn findings_mark_the_folders_that_contain_them() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("cache/x.bin"), 40);
    write(&dir.path().join("cache/y.bin"), 10);
    write(&dir.path().join("docs/z.bin"), 30);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let findings = [
        finding(&dir.path().join("cache/x.bin"), 40, Confidence::Safe),
        finding(&dir.path().join("docs/z.bin"), 30, Confidence::Review),
        finding(&dir.path().join("docs"), 30, Confidence::Info),
    ];

    let tree = FolderTree::build(dir.path(), &index, &findings);

    let root = tree.node(FolderTree::ROOT);
    assert_eq!((root.safe, root.review), (40, 30), "Info findings are not counted");
    let cache = tree.node(*root.children.iter().find(|c| tree.node(**c).name == "cache").unwrap());
    assert_eq!((cache.safe, cache.review), (40, 0));
}

#[test]
fn flagged_bytes_never_exceed_the_folder_size() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("node_modules/pkg/a.bin"), 100);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    // A flagged folder and a duplicate inside it overlap.
    let findings = [
        finding(&dir.path().join("node_modules"), 100, Confidence::Review),
        finding(&dir.path().join("node_modules/pkg/a.bin"), 100, Confidence::Review),
    ];

    let tree = FolderTree::build(dir.path(), &index, &findings);

    let root = tree.node(FolderTree::ROOT);
    assert!(root.safe + root.review <= root.size);
}

#[test]
fn trail_runs_from_the_root_to_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a/b/c.bin"), 1);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let tree = FolderTree::build(dir.path(), &index, &[]);

    let deepest = *tree.node(*tree.node(*tree.node(FolderTree::ROOT).children.first().unwrap()).children.first().unwrap()).parent.as_ref().unwrap();
    let names: Vec<&str> = tree.trail(deepest).iter().map(|i| tree.node(*i).name.as_str()).collect();

    assert_eq!(names.last(), Some(&"a"));
    assert_eq!(tree.trail(deepest).first(), Some(&FolderTree::ROOT));
}

fn area(r: &Rect) -> f64 {
    f64::from(r.w) * f64::from(r.h)
}

#[test]
fn layout_fills_the_bounds_in_proportion_to_size() {
    let bounds = Rect { x: 10.0, y: 20.0, w: 400.0, h: 300.0 };
    let sizes = [600u64, 300, 200, 100, 50, 50];

    let rects = squarify(&sizes, bounds);

    let total_area: f64 = rects.iter().map(area).sum();
    assert!((total_area - 120_000.0).abs() < 1.0, "{total_area}");
    for (rect, size) in rects.iter().zip(sizes) {
        let expected = 120_000.0 * size as f64 / 1300.0;
        assert!((area(rect) - expected).abs() < 1.0, "{rect:?} vs {expected}");
        assert!(rect.x >= bounds.x - 0.01 && rect.y >= bounds.y - 0.01);
        assert!(rect.x + rect.w <= bounds.x + bounds.w + 0.01 && rect.y + rect.h <= bounds.y + bounds.h + 0.01);
    }
}

#[test]
fn layout_gives_empty_rectangles_to_zero_sizes_and_handles_no_input() {
    let bounds = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

    let rects = squarify(&[10, 0], bounds);
    assert_eq!(area(&rects[0]) as u32, 10_000);
    assert_eq!(area(&rects[1]), 0.0);

    assert!(squarify(&[], bounds).is_empty());
    assert_eq!(squarify(&[0, 0], bounds).iter().map(area).sum::<f64>(), 0.0);
    assert_eq!(squarify(&[5], Rect { x: 0.0, y: 0.0, w: 0.0, h: 50.0 })[0].w, 0.0);
}

#[test]
fn layout_keeps_shapes_reasonably_square() {
    let bounds = Rect { x: 0.0, y: 0.0, w: 800.0, h: 500.0 };
    let sizes: Vec<u64> = (1..=40).rev().map(|n| n * 10).collect();

    let rects = squarify(&sizes, bounds);

    let worst = rects.iter().map(|r| (r.w / r.h).max(r.h / r.w)).fold(0.0f32, f32::max);
    assert!(worst < 6.0, "worst aspect ratio {worst}");
}

#[test]
fn live_tree_matches_the_tree_built_after_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("top.bin"), 5);
    write(&dir.path().join("a/one.bin"), 100);
    write(&dir.path().join("a/deep/two.bin"), 50);
    write(&dir.path().join("a/three.bin"), 7);
    write(&dir.path().join("b/four.bin"), 20);
    let mut live = LiveTree::new(dir.path());
    let index = scan(dir.path(), &Cancel::default(), |_, _, path, size| live.add(path, size));
    let built = FolderTree::build(dir.path(), &index, &[]);

    let live = live.tree();
    assert_eq!(live.node(FolderTree::ROOT).size, built.node(FolderTree::ROOT).size);
    assert_eq!(live.node(FolderTree::ROOT).files_size, 5);
    let sizes = |tree: &FolderTree| -> Vec<(String, u64)> {
        tree.children_by_size(FolderTree::ROOT).into_iter().map(|(c, s)| (tree.node(c).name.clone(), s)).collect()
    };
    assert_eq!(sizes(live), sizes(&built));
    assert_eq!(sizes(live), vec![("a".to_string(), 157), ("b".to_string(), 20)]);
}

#[test]
fn live_tree_sizes_are_usable_midway_and_files_in_alternating_folders_count_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut live = LiveTree::new(dir.path());

    live.add(&dir.path().join("a/x.bin"), 10);
    live.add(&dir.path().join("b/y.bin"), 4);
    live.add(&dir.path().join("a/z.bin"), 1);

    let tree = live.tree();
    assert_eq!(tree.node(FolderTree::ROOT).size, 15);
    let by_size = tree.children_by_size(FolderTree::ROOT);
    assert_eq!(by_size.iter().map(|c| c.1).collect::<Vec<_>>(), vec![11, 4]);
}

#[test]
fn removed_findings_leave_the_sizes_of_every_folder_above() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("cache/x.bin"), 40);
    write(&dir.path().join("cache/sub/y.bin"), 10);
    write(&dir.path().join("docs/z.bin"), 30);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let findings = [
        finding(&dir.path().join("cache/x.bin"), 40, Confidence::Safe),
        finding(&dir.path().join("cache/sub"), 10, Confidence::Safe),
    ];
    let mut tree = FolderTree::build(dir.path(), &index, &findings);

    tree.remove(&dir.path().join("cache/x.bin"), 40, Confidence::Safe);
    tree.remove(&dir.path().join("cache/sub"), 10, Confidence::Safe);

    let root = tree.node(FolderTree::ROOT);
    assert_eq!((root.size, root.safe), (30, 0));
    let (cache, exact) = tree.locate(&dir.path().join("cache"));
    assert!(exact);
    assert_eq!(tree.node(cache).size, 0);
    assert_eq!(tree.node(root.children[0]).name, "docs", "the emptied folder sorts last");
}

#[test]
fn layout_gives_every_listed_file_a_cell_inside_its_folder() {
    use deox_core::treemap::{layout, CellKind};
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("cache/big.bin"), 600);
    write(&dir.path().join("cache/small.bin"), 100);
    write(&dir.path().join("docs/note.txt"), 300);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let findings = [finding(&dir.path().join("cache"), 700, Confidence::Safe)];
    let tree = FolderTree::build(dir.path(), &index, &findings);

    let cells = layout(&tree, FolderTree::ROOT, Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 }, None, 16.0, 2.0);

    let files: Vec<_> = cells.iter().filter(|c| matches!(c.kind, CellKind::File { .. })).collect();
    assert_eq!(files.len(), 3);
    let big = files.iter().find(|c| c.size == 600).unwrap();
    assert_eq!(big.flag, Some(Confidence::Safe), "a file inherits the flag of the folder above it");
    let note = files.iter().find(|c| c.size == 300).unwrap();
    assert_eq!(note.flag, None);
    let cache = cells.iter().find(|c| matches!(c.kind, CellKind::Folder(_)) && c.size == 700).unwrap();
    assert!(big.rect.x >= cache.rect.x && big.rect.x + big.rect.w <= cache.rect.x + cache.rect.w + 0.01);
}

#[test]
fn a_removed_folder_and_a_finding_inside_it_are_taken_out_once() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("project/modules/dep.bin"), 100);
    write(&dir.path().join("project/modules/dup.bin"), 10);
    write(&dir.path().join("keep/file.bin"), 50);
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let mut tree = FolderTree::build(dir.path(), &index, &[]);
    let modules = dir.path().join("project/modules");
    let dup = modules.join("dup.bin");

    tree.remove_all(&[(modules.as_path(), 110, Confidence::Review), (dup.as_path(), 10, Confidence::Review), (dup.as_path(), 10, Confidence::Review)]);

    assert_eq!(tree.node(FolderTree::ROOT).size, 50);
    let (project, _) = tree.locate(&dir.path().join("project"));
    assert_eq!(tree.node(project).size, 0);
}

#[test]
fn layout_inside_a_flagged_folder_keeps_the_flag_on_files_and_the_rest() {
    use deox_core::treemap::{layout, CellKind};
    let dir = tempfile::tempdir().unwrap();
    for n in 0..300 {
        write(&dir.path().join(format!("cache/f{n}.bin")), 10 + n);
    }
    let index = scan(dir.path(), &Cancel::default(), |_, _, _, _| {});
    let tree = FolderTree::build(dir.path(), &index, &[]);
    let (cache, _) = tree.locate(&dir.path().join("cache"));

    let cells = layout(&tree, cache, Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }, Some(Confidence::Safe), 16.0, 1.0);

    assert!(cells.iter().any(|c| matches!(c.kind, CellKind::Rest(_))), "the smallest files are lumped together");
    assert!(cells.iter().all(|c| c.flag == Some(Confidence::Safe)));
}
