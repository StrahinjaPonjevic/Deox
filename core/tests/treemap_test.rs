use deox_core::report::{Category, Confidence, Finding};
use deox_core::scan::{scan, Cancel};
use deox_core::treemap::{squarify, FolderTree, Rect};
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
    let index = scan(dir.path(), &Cancel::default(), |_, _| {});

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
    let index = scan(dir.path(), &Cancel::default(), |_, _| {});
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
    let index = scan(dir.path(), &Cancel::default(), |_, _| {});
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
    let index = scan(dir.path(), &Cancel::default(), |_, _| {});
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
