//! Drives the screens the way a person would. Set `DEOX_SCREENS` to also render each one to a PNG
//! under `target/screens`, which needs a GPU adapter; without it the tests run headless.

use super::*;
use deox_core::scan::IndexBuilder;
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::{wgpu::WgpuTestRenderer, Harness};
use std::fs;
use std::path::Path;

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

/// Findings and a made-up scan, so no files are written.
struct Demo {
    findings: Vec<Finding>,
    scanned: Scanned,
    files: u64,
}

fn demo_at(root: &Path) -> Demo {
    let mut builder = IndexBuilder::new(root);
    for i in 0..40u64 {
        for (name, base, step) in
            [("Temp/t", 20_000, 3_000), ("Cache/Chrome/c", 60_000, 9_000), ("Projects/app/node_modules/m", 15_000, 2_000), ("Downloads/d", 90_000, 20_000)]
        {
            builder.file(&root.join(format!("{name}{i}.bin")), base + i * step, None);
        }
    }
    let index = builder.finish();
    let findings = vec![
        finding(&root.join("Temp"), 2_000_000, Category::Temp, Confidence::Safe),
        finding(&root.join("Cache/Chrome"), 4_000_000, Category::AppCache, Confidence::Safe),
        finding(&root.join("Projects/app/node_modules"), 1_500_000, Category::DevArtifact, Confidence::Review),
        finding(&root.join("Downloads/d39.bin"), 870_000, Category::Duplicate, Confidence::Review),
    ];
    let tree = FolderTree::build(&index, &findings);
    Demo { files: index.files.len() as u64, findings, scanned: Scanned { index, tree } }
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
    let Demo { findings, scanned, files } = demo();
    harness(size, move |app| app.set_results(findings, files, 0, Some(scanned)))
}

/// The results with the Cleanup tab open; they start in the explorer.
fn cleanup_harness(size: egui::Vec2) -> Harness<'static, DeoxApp> {
    let mut harness = results_harness(size);
    harness.state_mut().tab = Tab::Cleanup;
    harness
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
    let mut h = cleanup_harness(WIDE);
    shot(&mut h, "results");
    assert!(h.query_by_label("Move to Recycle Bin…").is_some());
}

#[test]
fn results_open_in_the_explorer_and_the_tabs_switch_at_any_width() {
    let _gpu = gpu();
    for (size, name) in [(NARROW, "narrow"), (WIDE, "wide")] {
        let mut h = results_harness(size);
        shot(&mut h, &format!("explorer_{name}"));
        assert!(h.query_by_label("in this folder").is_some(), "the explorer comes first");
        assert!(h.query_by_label("Filter:").is_none());
        assert!(h.query_by_label("Move to Recycle Bin…").is_none());

        h.get_by_label_contains("Cleanup (4 · ").click();
        shot(&mut h, &format!("cleanup_{name}"));
        assert!(h.query_by_label("Filter:").is_some());
        assert!(h.query_by_label("Move to Recycle Bin…").is_some());
        assert!(h.query_by_label("in this folder").is_none(), "the list has the whole window");

        h.get_by_label("Disk Explorer").click();
        h.run();
        assert!(h.query_by_label("in this folder").is_some());
    }
}

#[test]
fn selecting_in_the_explorer_updates_the_cleanup_tab_title() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.state_mut().selected.clear();
    h.run();
    assert!(h.query_by_label_contains(" · 0 selected)").is_some());

    h.get_by_label("Select Safe here").click();
    h.run();

    assert!(h.query_by_label_contains(" · 2 selected)").is_some());
    assert!(h.state().tab == Tab::Explorer);
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
    let mut h = cleanup_harness(WIDE);
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
    let mut h = cleanup_harness(WIDE);
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
    let mut h = cleanup_harness(WIDE);
    h.run();
    assert_eq!(h.state().map.folder, FolderTree::ROOT);

    h.get_by_label("Chrome").click();
    h.step();
    h.get_by_label("Chrome").click();
    h.run();

    assert!(h.state().tab == Tab::Explorer);
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
    let before = h.state().results.as_ref().unwrap().tree().unwrap().node(FolderTree::ROOT).size;

    h.state_mut().drop_findings(&gone);
    shot(&mut h, "results_after_move");

    let results = h.state().results.as_ref().unwrap();
    let tree = results.tree().expect("the tree is kept");
    assert!(tree.node(FolderTree::ROOT).size < before, "its sizes shrink instead of the map disappearing");
    assert_eq!(results.report.findings.len(), 3);
}

#[test]
fn moving_a_folder_drops_what_was_inside_it_while_the_map_shows_it() {
    let _gpu = gpu();
    let Demo { mut findings, scanned, files } = demo();
    let temp = Path::new(r"C:\Demo\Temp");
    findings.push(finding(&temp.join("t0.bin"), 20_000, Category::Duplicate, Confidence::Review));
    let (temp_id, _) = scanned.tree.locate(temp);
    let mut h = harness(NARROW, move |app| app.set_results(findings, files, 0, Some(scanned)));
    h.run();
    h.state_mut().map.open(temp_id, 0.0, None);
    h.run();

    h.state_mut().drop_findings(&HashSet::from([temp.to_path_buf()]));
    h.run();

    let results = h.state().results.as_ref().unwrap();
    assert!(results.report.findings.iter().all(|f| !f.path.starts_with(temp)));
    assert_eq!(results.scanned.as_ref().unwrap().index.files.len(), files as usize - 40);
    assert!(h.query_by_label("This folder is empty.").is_some());
}

#[test]
fn a_long_folder_name_does_not_push_the_size_out_of_the_window() {
    let _gpu = gpu();
    let deep = r"AppData\Local\Google\Chrome\User Data\Default\Service Worker\CacheStorage\Deeply Nested Folder Name";
    let root = format!(r"C:\Users\Some Person\{deep}\{deep}");
    let Demo { findings, scanned, files } = demo_at(Path::new(&root));
    let mut h = harness(WIDE, move |app| app.set_results(findings, files, 0, Some(scanned)));
    shot(&mut h, "results_long_name");

    let sizes: Vec<egui::Rect> = h.query_all_by_label_contains("MB").map(|n| n.rect()).filter(|r| r.min.y > 560.0).collect();
    assert!(!sizes.is_empty(), "the size under the map is shown");
    assert!(sizes.iter().all(|r| r.max.x <= WIDE.x), "and stays inside the window: {sizes:?}");
}

#[test]
fn long_root_breadcrumb_stays_on_one_line() {
    let _gpu = gpu();
    let mut short = results_harness(WIDE);
    short.run();
    let baseline_y = short.get_by_label("in this folder").rect().min.y;

    let deep = r"AppData\Local\Google\Chrome\User Data\Default\Service Worker\CacheStorage\Deeply Nested Folder Name";
    let root = format!(r"C:\Users\Some Person\{deep}\{deep}");
    let Demo { findings, scanned, files } = demo_at(Path::new(&root));
    let mut long = harness(WIDE, move |app| app.set_results(findings, files, 0, Some(scanned)));
    long.run();
    assert!(
        (long.get_by_label("in this folder").rect().min.y - baseline_y).abs() < 1.0,
        "a long root path should truncate instead of wrapping the breadcrumb"
    );

    long.state_mut().tab = Tab::Cleanup;
    long.run();
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
fn deep_breadcrumb_stays_inside_the_window() {
    let _gpu = gpu();
    let root = Path::new(r"C:\Demo");
    let deep_rel = Path::new("level1/level2/level3/level4/level5/level6/level7/level8/level9");
    let deep_dir = root.join(deep_rel);
    let mut builder = IndexBuilder::new(root);
    builder.file(&deep_dir.join("deep.bin"), 4_000_000, None);
    let index = builder.finish();
    let findings = vec![finding(&deep_dir, 4_000_000, Category::AppCache, Confidence::Safe)];
    let tree = FolderTree::build(&index, &findings);
    let (deep_id, _) = tree.locate(&deep_dir);
    let mut h = harness(WIDE, move |app| app.set_results(findings, 1, 0, Some(Scanned { index, tree })));
    h.run();

    h.state_mut().map.open(deep_id, 0.0, None);
    h.run();

    let last_crumb = h
        .query_all_by_label("level9")
        .min_by_key(|n| n.rect().min.y as i32)
        .expect("level9 breadcrumb")
        .rect();
    assert!(last_crumb.max.x <= WIDE.x, "last breadcrumb {last_crumb:?} stays inside the window");
}

#[test]
fn ctrl_a_selects_only_visible_removable_items() {
    let _gpu = gpu();
    let Demo { mut findings, scanned, files } = demo();
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
    let mut h = harness(WIDE, move |app| {
        app.set_results(findings, files, 0, Some(scanned));
        app.tab = Tab::Cleanup;
    });
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
    assert!(h.get_by_label("Disk Explorer").accesskit_node().is_disabled());
}

/// Names of the rows of the directory tree, top to bottom.
fn tree_rows(app: &DeoxApp) -> Vec<String> {
    let scanned = app.results.as_ref().unwrap().scanned.as_ref().unwrap();
    let name = |entry| match entry {
        Entry::Folder(id) => scanned.tree.node(id).name.clone(),
        Entry::File(position) => scanned.index.files[position].name.to_string_lossy().into_owned(),
    };
    app.directory.lines.iter().map(|line| name(line.entry)).collect()
}

fn folder_id(app: &DeoxApp, path: &str) -> usize {
    let (id, exact) = app.results.as_ref().unwrap().tree().unwrap().locate(Path::new(path));
    assert!(exact, "{path} is a folder of the demo");
    id
}

#[test]
fn the_directory_tree_lists_folders_largest_first_and_every_file_of_an_opened_one() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    assert_eq!(tree_rows(h.state()), ["Demo", "Downloads", "Cache", "Temp", "Projects"]);
    assert!(h.query_by_label_contains("Temp, ").unwrap().accesskit_node().label().unwrap().ends_with("Safe"));

    h.get_by_label("Expand Downloads").click();
    h.run();
    let rows = tree_rows(h.state());
    assert_eq!(rows.len(), 45);
    assert_eq!(rows[1..4], ["Downloads", "d39.bin", "d38.bin"]);

    h.get_by_label("Collapse Downloads").click();
    h.run();
    assert_eq!(tree_rows(h.state()).len(), 5);
}

#[test]
fn selecting_in_the_tree_opens_the_same_place_in_the_map() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    h.get_by_label_contains("Cache, ").click();
    h.run();
    let cache = folder_id(h.state(), r"C:\Demo\Cache");
    assert_eq!(h.state().map.folder, cache);
    assert!(h.state().directory.selected == Entry::Folder(cache));

    h.get_by_label("Expand Downloads").click();
    h.run();
    h.get_by_label_contains("d38.bin, ").click();
    h.run();
    assert_eq!(h.state().map.folder, folder_id(h.state(), r"C:\Demo\Downloads"));
    assert!(matches!(h.state().map.picked, Some(Pick::File { index: 1, .. })), "its rectangle is pointed out");
    assert!(matches!(h.state().directory.selected, Entry::File(_)), "the map does not take the selection back");
}

#[test]
fn moving_in_the_map_reveals_the_place_in_the_tree_and_scrolls_to_it() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    let downloads = folder_id(h.state(), r"C:\Demo\Downloads");
    assert!(h.query_by_label_contains("d0.bin, ").is_none());

    // The smallest file of the folder, far below the first screen of the tree.
    h.state_mut().map.open(downloads, 0.0, None);
    h.state_mut().map.picked = Some(Pick::File { folder: downloads, index: 39 });
    h.run();

    assert!(matches!(h.state().directory.selected, Entry::File(_)));
    assert!(h.query_by_label_contains("d0.bin, ").is_some(), "the row is on screen");

    let chrome = folder_id(h.state(), r"C:\Demo\Cache\Chrome");
    h.state_mut().map.open(chrome, 0.0, None);
    h.run();
    assert!(h.state().directory.selected == Entry::Folder(chrome));
    assert!(tree_rows(h.state()).contains(&"Chrome".to_string()), "the folders above it are opened");
    shot(&mut h, "explorer_revealed");
}

#[test]
fn arrow_keys_walk_the_directory_tree() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    let press = |h: &mut Harness<'_, DeoxApp>, key| {
        h.key_press(key);
        h.run();
        let state = h.state();
        let scanned = state.results.as_ref().unwrap().scanned.as_ref().unwrap();
        match state.directory.selected {
            Entry::Folder(id) => scanned.tree.node(id).name.clone(),
            Entry::File(position) => scanned.index.files[position].name.to_string_lossy().into_owned(),
        }
    };

    assert_eq!(press(&mut h, egui::Key::ArrowDown), "Downloads");
    assert_eq!(h.state().map.folder, folder_id(h.state(), r"C:\Demo\Downloads"), "the map follows");
    assert_eq!(press(&mut h, egui::Key::ArrowRight), "Downloads");
    assert_eq!(press(&mut h, egui::Key::ArrowRight), "d39.bin");
    assert_eq!(press(&mut h, egui::Key::ArrowLeft), "Downloads");
    assert_eq!(press(&mut h, egui::Key::ArrowLeft), "Downloads");
    assert_eq!(tree_rows(h.state()).len(), 5, "the folder is closed again");
    assert_eq!(press(&mut h, egui::Key::End), "Projects");
    assert_eq!(press(&mut h, egui::Key::Home), "Demo");

    h.get_by_label_contains("Cache, ").click();
    h.run();
    assert_eq!(press(&mut h, egui::Key::ArrowDown), "Temp", "the keys carry on from a clicked row");
}

#[test]
fn what_was_moved_leaves_the_directory_tree() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    h.get_by_label_contains("Temp, ").click();
    h.run();
    h.state_mut().drop_findings(&HashSet::from([PathBuf::from(r"C:\Demo\Temp")]));
    h.run();

    assert_eq!(tree_rows(h.state()), ["Demo", "Downloads", "Cache", "Projects"]);

    // The map still shows the folder that is gone; the keys keep working.
    h.key_press(egui::Key::ArrowDown);
    h.run();
    assert_eq!(h.state().map.folder, folder_id(h.state(), r"C:\Demo\Downloads"));
}

#[test]
fn the_explorer_fits_the_smallest_window() {
    let _gpu = gpu();
    let size = egui::vec2(780.0, 520.0);
    let mut h = results_harness(size);
    shot(&mut h, "explorer_smallest");
    assert!(h.get_by_label("Select Safe here").rect().max.y <= size.y, "the map's buttons stay on screen");
    assert!(h.query_by_label_contains("Cache, ").is_some(), "and some of the tree is shown");
    assert!(h.query_by_label_contains("bin: ").is_some(), "next to the extension list");
}

/// A scan holding several file types: mkv takes the most, then zip, txt, log and no extension.
fn types_harness(size: egui::Vec2) -> Harness<'static, DeoxApp> {
    let mut builder = IndexBuilder::new(r"C:\Demo");
    for (name, bytes) in [
        ("Media/a.mkv", 9_000_000),
        ("Media/b.mkv", 3_000_000),
        ("Backup/c.zip", 6_000_000),
        ("Docs/d.txt", 2_000_000),
        ("Docs/e.log", 1_000_000),
        ("Docs/README", 500_000),
    ] {
        builder.file(&Path::new(r"C:\Demo").join(name), bytes, None);
    }
    let index = builder.finish();
    let tree = FolderTree::build(&index, &[]);
    harness(size, move |app| app.set_results(Vec::new(), 6, 0, Some(Scanned { index, tree })))
}

#[test]
fn the_extension_list_is_ordered_by_the_space_each_type_takes() {
    let _gpu = gpu();
    let mut h = types_harness(WIDE);
    shot(&mut h, "explorer_types");
    let top = |label: &str| h.get_by_label(label).rect().min.y;
    let rows = [
        top("mkv: 11.4 MB, 2 files"),
        top("zip: 5.7 MB, 1 file"),
        top("txt: 1.9 MB, 1 file"),
        top("log: 976.6 KB, 1 file"),
        top("(no ext): 488.3 KB, 1 file"),
    ];
    assert!(rows.windows(2).all(|pair| pair[0] < pair[1]), "{rows:?}");
}

#[test]
fn clicking_an_extension_points_it_out_in_the_map_until_clicked_again() {
    let _gpu = gpu();
    let mut h = types_harness(WIDE);
    h.run();
    assert!(h.state().map.coloring == Coloring::FileTypes, "the map starts with a color per type");
    h.get_by_label("Cleanup status").click();
    h.run();
    assert!(h.state().map.coloring == Coloring::CleanupStatus);

    h.get_by_label("zip: 5.7 MB, 1 file").click();
    shot(&mut h, "explorer_zip_pointed_out");
    assert_eq!(h.state().file_types.highlighted.as_deref(), Some("zip"));
    assert!(h.state().map.coloring == Coloring::FileTypes, "so that it can be seen");

    h.get_by_label("zip: 5.7 MB, 1 file").click();
    h.run();
    assert_eq!(h.state().file_types.highlighted, None);
}

#[test]
fn a_color_stays_with_its_extension_after_a_move() {
    let _gpu = gpu();
    let mut h = types_harness(WIDE);
    h.run();
    let colors = |app: &DeoxApp| ["mkv", "zip", "txt", "log"].map(|e| app.file_types.color(e, true));
    let before = colors(h.state());
    assert!(before[0] != before[1] && before[1] != before[2] && before[0] != before[2]);
    assert!(before[..3].iter().all(Option::is_some));
    assert_eq!(before[3], None, "the rest share the neutral color");
    h.get_by_label("mkv: 11.4 MB, 2 files").click();
    h.run();

    h.state_mut().drop_findings(&HashSet::from([PathBuf::from(r"C:\Demo\Media")]));
    h.run();

    assert_eq!(colors(h.state()), before, "zip does not take the color of the extension that left");
    assert_eq!(h.state().file_types.highlighted, None, "there is nothing left to point out");
    assert!(h.query_by_label_contains("mkv: ").is_none());
}

/// What the last frame asked to put on the clipboard.
fn copied(h: &Harness<'_, DeoxApp>) -> Option<String> {
    h.output().platform_output.commands.iter().find_map(|command| match command {
        egui::OutputCommand::CopyText(text) => Some(text.clone()),
        _ => None,
    })
}

/// Clicks the topmost button of that name: a context menu opens above the bar under the map.
fn click_topmost(h: &mut Harness<'_, DeoxApp>, label: &str) {
    h.query_all_by_label(label).min_by_key(|n| n.rect().min.y as i32).expect(label).click();
    h.step();
}

#[test]
fn a_right_click_in_the_tree_selects_the_entry_and_copies_its_path() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();
    h.get_by_label_contains("Cache, ").click_secondary();
    shot(&mut h, "tree_menu");
    assert_eq!(h.state().map.folder, folder_id(h.state(), r"C:\Demo\Cache"), "the map shows it too");
    assert_eq!(h.query_all_by_label("Open in Explorer").count(), 2, "in the menu and under the map");

    click_topmost(&mut h, "Copy path");

    assert_eq!(copied(&h).as_deref(), Some(r"C:\Demo\Cache"));
    h.run();
    assert_eq!(h.query_all_by_label("Copy path").count(), 1, "the menu closes");
}

#[test]
fn the_buttons_under_the_map_act_on_what_it_shows() {
    let _gpu = gpu();
    let mut h = results_harness(WIDE);
    h.run();

    click_topmost(&mut h, "Copy path");
    assert_eq!(copied(&h).as_deref(), Some(r"C:\Demo"));

    // The demo folders are made up, so Explorer is not started.
    h.get_by_label("Open in Explorer").click();
    h.run();
    assert!(h.state().error.as_deref().is_some_and(|e| e.starts_with(r#"Explorer could not show "C:\Demo""#)));
}

#[test]
fn a_right_click_on_a_suggestion_leads_to_its_place() {
    let _gpu = gpu();
    let mut h = cleanup_harness(WIDE);
    h.run();
    h.get_by_label("Chrome").click_secondary();
    shot(&mut h, "cleanup_menu");

    click_topmost(&mut h, "Copy path");
    assert_eq!(copied(&h).as_deref(), Some(r"C:\Demo\Cache/Chrome"));

    h.run();
    h.get_by_label("Chrome").click_secondary();
    h.run();
    h.get_by_label("Show in Disk Explorer").click();
    h.run();
    assert!(h.state().tab == Tab::Explorer);
    assert_eq!(h.state().directory.selected, Entry::Folder(folder_id(h.state(), r"C:\Demo\Cache\Chrome")));
}
