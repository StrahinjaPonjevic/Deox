//! Drives the screens the way a person would. Set `DEOX_SCREENS` to also render each one to a PNG
//! under `target/screens`, which needs a GPU adapter; without it the tests run headless.

use super::*;
use deox_core::scan::{FileEntry, Index};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::{wgpu::WgpuTestRenderer, Harness};
use std::fs;

fn finding(path: &Path, size: u64, category: Category, confidence: Confidence) -> Finding {
    Finding {
        path: path.to_path_buf(),
        size,
        modified: None,
        category,
        confidence,
        rule: "demo".into(),
        reason: "Demo finding".into(),
        uninstall: None,
    }
}

/// Findings and a folder tree over a made-up scan, so no files are written.
struct Demo {
    findings: Vec<Finding>,
    tree: FolderTree,
    files: u64,
}

fn demo_at(root: &Path) -> Demo {
    let mut files = Vec::new();
    for i in 0..40u64 {
        for (name, base, step) in
            [("Temp/t", 20_000, 3_000), ("Cache/Chrome/c", 60_000, 9_000), ("Projects/app/node_modules/m", 15_000, 2_000), ("Downloads/d", 90_000, 20_000)]
        {
            files.push(FileEntry { path: root.join(format!("{name}{i}.bin")), size: base + i * step, modified: None, accessed: None });
        }
    }
    let index = Index { files, ..Default::default() };
    let findings = vec![
        finding(&root.join("Temp"), 2_000_000, Category::Temp, Confidence::Safe),
        finding(&root.join("Cache/Chrome"), 4_000_000, Category::AppCache, Confidence::Safe),
        finding(&root.join("Projects/app/node_modules"), 1_500_000, Category::DevArtifact, Confidence::Review),
        finding(&root.join("Downloads/d39.bin"), 870_000, Category::Duplicate, Confidence::Review),
    ];
    let tree = FolderTree::build(root, &index, &findings);
    Demo { files: index.files.len() as u64, findings, tree }
}

fn demo() -> Demo {
    demo_at(Path::new(r"C:\Demo"))
}

/// Pictures need a GPU, so they are only made when `DEOX_SCREENS` is set.
fn pictures() -> bool {
    std::env::var_os("DEOX_SCREENS").is_some()
}

static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Rendering on the GPU from several threads at once crashes, so each test takes turns.
fn gpu() -> std::sync::MutexGuard<'static, ()> {
    GPU.lock().unwrap_or_else(|e| e.into_inner())
}

fn harness(size: egui::Vec2, make: impl FnOnce(&mut DeoxApp) + 'static) -> Harness<'static, DeoxApp> {
    let mut builder = Harness::builder().with_size(size).with_step_dt(0.05).with_max_steps(40).with_context_setup(configure_theme);
    if pictures() {
        builder = builder.renderer(WgpuTestRenderer::new());
    }
    builder.build_eframe(|cc| {
        let mut app = DeoxApp::new(cc.egui_ctx.clone());
        make(&mut app);
        app
    })
}

fn results_harness(size: egui::Vec2) -> Harness<'static, DeoxApp> {
    let Demo { findings, tree, files } = demo();
    harness(size, move |app| app.set_results(findings, files, 0, Some(tree)))
}

fn shot(harness: &mut Harness<'_, DeoxApp>, name: &str) {
    harness.run();
    if !pictures() {
        return;
    }
    let image = harness.render().expect("render");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/screens");
    fs::create_dir_all(&dir).unwrap();
    image.save(dir.join(format!("{name}.png"))).unwrap();
}

const WIDE: egui::Vec2 = egui::vec2(1100.0, 720.0);
const NARROW: egui::Vec2 = egui::vec2(720.0, 720.0);

#[test]
fn start_screen() {
    let _gpu = gpu();
    let mut h = harness(WIDE, |_| {});
    shot(&mut h, "home");
    assert!(h.query_by_label("Scan").is_some());
    assert!(h.query_by_label("Move to Recycle Bin…").is_none());
}

#[test]
fn scan_screen() {
    let _gpu = gpu();
    let mut h = harness(WIDE, |app| {
        app.phase = Phase::Scanning;
        app.scan_root = r"C:\Users\you".into();
        app.status = "Step 2 of 4: applying cleanup rules".into();
        app.files_seen = 123_456;
        app.bytes_seen = 48_000_000_000;
        app.scan_folder = r"C:\Users\you\AppData\Local\Temp".into();
    });
    shot(&mut h, "scan");
    assert!(h.query_by_label_contains("Step 2 of 4").is_some());
    assert!(h.query_by_label("Cancel").is_some());
    assert!(h.query_by_label_contains("123456 files").is_some());
}

#[test]
fn results_screen() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    shot(&mut h, "results");
    assert!(h.query_by_label("Move to Recycle Bin…").is_some());
}

#[test]
fn narrow_window_switches_between_list_and_map() {
    let _gpu = gpu();
    let mut h = results_harness(NARROW);
    shot(&mut h, "results_narrow");
    assert!(h.query_by_label("Filter:").is_some(), "the list is shown first");

    h.get_by_label("Map").click();
    shot(&mut h, "results_narrow_map");
    assert!(h.query_by_label("Filter:").is_none(), "the map replaces the list");
    assert!(h.state().tab == View::Map);
}

#[test]
fn duplicates_start_collapsed_and_come_last() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    assert!(h.state().collapsed.contains(&Category::Duplicate));
    assert!(!h.state().collapsed.contains(&Category::Temp));
    let last = h.state().rows.iter().rev().find_map(|r| match r {
        Row::Header { category, .. } => Some(*category),
        Row::Item(_) => None,
    });
    assert_eq!(last, Some(Category::Duplicate));
}

#[test]
fn only_safe_items_start_selected() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    let state = h.state();
    let report = &state.results.as_ref().unwrap().report;
    assert_eq!(state.selected.len(), 2);
    assert!(state.selected.iter().all(|i| report.findings[*i].confidence == Confidence::Safe));
}

#[test]
fn nothing_selected_disables_the_move_button() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    assert!(!h.get_by_label("Move to Recycle Bin…").accesskit_node().is_disabled());

    h.get_by_label("Clear").click();
    h.run();

    assert!(h.state().selected.is_empty());
    assert!(h.get_by_label("Move to Recycle Bin…").accesskit_node().is_disabled());
}

#[test]
fn select_all_safe_restores_the_selection() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    h.get_by_label("Clear").click();
    h.run();

    h.get_by_label("Select all Safe").click();
    h.run();

    assert_eq!(h.state().selected.len(), 2);
}

#[test]
fn double_clicking_a_row_shows_it_on_the_map() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    assert_eq!(h.state().map.folder, FolderTree::ROOT);

    h.get_by_label("Chrome").click();
    h.step();
    h.get_by_label("Chrome").click();
    h.run();

    assert_ne!(h.state().map.folder, FolderTree::ROOT, "the map opens the folder that holds the item");
    assert!(h.state().map.picked.is_some(), "and points out the item");
    shot(&mut h, "results_after_double_click");
}

#[test]
fn the_map_survives_a_move_to_the_recycle_bin() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    let gone: HashSet<PathBuf> = h.state().results.as_ref().unwrap().report.findings.iter().take(1).map(|f| f.path.clone()).collect();
    let before = h.state().results.as_ref().unwrap().tree.as_ref().unwrap().node(FolderTree::ROOT).size;

    h.state_mut().drop_findings(&gone);
    shot(&mut h, "results_after_move");

    let results = h.state().results.as_ref().unwrap();
    let tree = results.tree.as_ref().expect("the tree is kept");
    assert!(tree.node(FolderTree::ROOT).size < before, "its sizes shrink instead of the map disappearing");
    assert_eq!(results.report.findings.len(), 3);
}

#[test]
fn a_long_folder_name_does_not_push_the_size_out_of_the_map_panel() {
    let _gpu = gpu();
    let deep = r"AppData\Local\Google\Chrome\User Data\Default\Service Worker\CacheStorage\Deeply Nested Folder Name";
    let root = format!(r"C:\Users\Some Person\{deep}\{deep}");
    let Demo { findings, tree, files } = demo_at(Path::new(&root));
    let mut h = harness(WIDE, move |app| app.set_results(findings, files, 0, Some(tree)));
    shot(&mut h, "results_long_name");

    let sizes: Vec<egui::Rect> = h.query_all_by_label_contains("MB").map(|n| n.rect()).filter(|r| r.min.y > 560.0).collect();
    assert!(!sizes.is_empty(), "the size under the map is shown");
    assert!(sizes.iter().all(|r| r.max.x <= WIDE.x * 0.6), "and stays inside the map panel: {sizes:?}");
}

#[test]
fn long_root_breadcrumb_stays_on_one_line() {
    let _gpu = gpu();
    let mut short = results_harness(WIDE);
    short.run();
    let baseline_y = short.get_by_label("in this folder").rect().min.y;

    let deep = r"AppData\Local\Google\Chrome\User Data\Default\Service Worker\CacheStorage\Deeply Nested Folder Name";
    let root = format!(r"C:\Users\Some Person\{deep}\{deep}");
    let Demo { findings, tree, files } = demo_at(Path::new(&root));
    let mut long = harness(WIDE, move |app| app.set_results(findings, files, 0, Some(tree)));
    long.run();
    assert!(
        (long.get_by_label("in this folder").rect().min.y - baseline_y).abs() < 1.0,
        "a long root path should truncate instead of wrapping the breadcrumb"
    );

    long.get_by_label("Chrome").click();
    long.step();
    long.get_by_label("Chrome").click();
    long.run();
    assert!(
        (long.get_by_label("in this folder").rect().min.y - baseline_y).abs() < 1.0,
        "drilling into a subfolder keeps the breadcrumb on one line"
    );
}

#[test]
fn deep_breadcrumb_does_not_widen_the_map_panel() {
    let _gpu = gpu();
    let root = Path::new(r"C:\Demo");
    let deep_rel = Path::new("level1/level2/level3/level4/level5/level6/level7/level8/level9");
    let deep_dir = root.join(deep_rel);
    let index = Index {
        files: vec![FileEntry { path: deep_dir.join("deep.bin"), size: 4_000_000, modified: None, accessed: None }],
        ..Default::default()
    };
    let findings = vec![finding(&deep_dir, 4_000_000, Category::AppCache, Confidence::Safe)];
    let tree = FolderTree::build(root, &index, &findings);
    let (deep_id, _) = tree.locate(&deep_dir);
    let mut h = harness(WIDE, move |app| app.set_results(findings, 1, 0, Some(tree)));
    h.run();
    let baseline_filter_x = h.get_by_label("Filter:").rect().min.x;

    h.state_mut().map.open(deep_id, 0.0, None);
    h.run();

    let filter_x = h.get_by_label("Filter:").rect().min.x;
    assert!((filter_x - baseline_filter_x).abs() < 1.0, "Filter moved from {baseline_filter_x} to {filter_x}");
    let last_crumb = h
        .query_all_by_label("level9")
        .min_by_key(|n| n.rect().min.y as i32)
        .expect("level9 breadcrumb")
        .rect();
    assert!(last_crumb.max.x < filter_x, "last breadcrumb {last_crumb:?} stays inside map panel (< {filter_x})");
}

#[test]
fn ctrl_a_selects_only_visible_removable_items() {
    let _gpu = gpu();
    let Demo { mut findings, tree, files } = demo();
    findings.push(Finding {
        path: PathBuf::new(),
        size: 1_000_000,
        modified: None,
        category: Category::Bloatware,
        confidence: Confidence::Info,
        rule: "Cache Cleaner App".into(),
        reason: "Installed program".into(),
        uninstall: Some("ms-settings:appsfeatures".into()),
    });
    let mut h = harness(WIDE, move |app| app.set_results(findings, files, 0, Some(tree)));
    h.run();
    h.get_by_label("Clear").click();
    h.run();
    assert!(h.state().selected.is_empty());

    // Matches both Cache/Chrome (Safe) and "Cache Cleaner App" (Info), while hiding Temp, node_modules, d39.bin.
    h.state_mut().filter = "Cache".into();
    h.state_mut().rebuild_rows();
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run();

    let state = h.state();
    let report = &state.results.as_ref().unwrap().report;
    assert_eq!(state.selected.len(), 1);
    let chosen = &report.findings[*state.selected.iter().next().unwrap()];
    assert!(chosen.path.ends_with("Chrome"));
    assert_ne!(chosen.confidence, Confidence::Info);

    // Group checkbox toggles visible items in that category.
    h.get_by_label("Select all in Application caches").click();
    h.run();
    assert!(h.state().selected.is_empty(), "toggling Application caches off clears Chrome");
    h.get_by_label("Select all in Application caches").click();
    h.run();
    assert_eq!(h.state().selected.len(), 1);
}

#[test]
fn results_without_a_map_still_show_the_list() {
    let _gpu = gpu();
    let Demo { findings, files, .. } = demo();
    let mut h = harness(WIDE, move |app| app.set_results(findings, files, 0, None));
    shot(&mut h, "results_no_map");
    assert!(h.query_by_label("Filter:").is_some());
}
