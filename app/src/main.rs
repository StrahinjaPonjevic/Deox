#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod map;
mod ui;
mod worker;

use deox_core::delete::{launch_uninstaller, DeleteReport, Outcome};
use deox_core::report::{Category, Confidence, Finding, Report};
use deox_core::scan::Cancel;
use deox_core::treemap::{FolderTree, LiveTree};
use eframe::egui::{self, Color32, RichText};
use map::{LiveMap, MapAction, MapState, Pick};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use ui::{
    category_name, cell, confidence_tag, configure_theme, dialog_width, disclosure_icon, human, items, legend_line, level_glyph,
    name_checkbox, paths, reclaim_strip, scan_step, share_bar, step_ticks, sweep, CHECK_W, ITEM_INDENT, REVEAL_SECS, SIZE_W, TAG_W,
};
use worker::{Msg, Notifier};

/// Rows shown in the list of skipped items; longer lists are summarized.
const MAX_LISTED_SKIPS: usize = 200;
/// Width of the centered column on the start screen.
const START_COLUMN: f32 = 600.0;
/// Content width from which the map and the list are shown side by side.
const SPLIT_MIN_WIDTH: f32 = 860.0;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([780.0, 520.0])
            .with_title("Deox"),
        ..Default::default()
    };
    eframe::run_native(
        "Deox",
        options,
        Box::new(|cc| {
            configure_theme(&cc.egui_ctx);
            Ok(Box::new(DeoxApp::new(cc.egui_ctx.clone())))
        }),
    )
}

#[derive(PartialEq)]
enum Phase {
    Idle,
    Scanning,
    Deleting,
}

enum Row {
    Header { category: Category, total: u64, count: usize },
    Item(usize),
}

struct Results {
    report: Report,
    files: u64,
    unreadable: usize,
    /// Folder sizes for the map; updated after a move, so what was moved leaves the map as well.
    tree: Option<FolderTree>,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    List,
    Map,
}

struct DeoxApp {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: Arc<Cancel>,
    phase: Phase,
    root: String,
    /// The folder the running scan was started on; the input field may change meanwhile.
    scan_root: String,
    drives: Vec<String>,
    /// Drive root, total and free bytes of the local fixed drives, read once at start.
    drive_space: Vec<(String, u64, u64)>,
    status: String,
    files_seen: u64,
    bytes_seen: u64,
    /// Largest folders under the scan root so far, in display order (see `map::stable_order`).
    live_folders: Vec<(String, u64)>,
    live_reordered: Instant,
    /// The scan's folder tree as it grows, shared with the worker that fills it.
    live_tree: Option<Arc<Mutex<LiveTree>>>,
    /// List or map of the folders while scanning. The list stays the default until the map is proven on a large drive.
    scan_view: View,
    live_map: LiveMap,
    /// Items settled and total while a move or check is running.
    move_progress: (usize, usize),
    move_cancel: Arc<Cancel>,
    /// Narrows the result list to names and paths containing this text.
    filter: String,
    /// Per finding: whether it passes the current filter.
    visible: Vec<bool>,
    tab: View,
    /// The folder shown in the map, as a node of the folder tree.
    map: MapState,
    /// Row highlighted for keyboard control, as a position in `rows`.
    cursor_row: Option<usize>,
    /// Scroll offset to apply on the next frame, set when the cursor leaves the view.
    scroll_to: Option<f32>,
    /// Last known scroll offset and viewport height of the result list.
    scroll_state: (f32, f32),
    /// The folder the scan reported most recently.
    scan_folder: String,
    results: Option<Results>,
    /// Time of the last finished scan, for the bar that fills in under the headline.
    reveal_start: f64,
    rows: Vec<Row>,
    collapsed: HashSet<Category>,
    selected: HashSet<usize>,
    confirm_delete: bool,
    dry_run: bool,
    last_delete: Option<(DeleteReport, bool)>,
    pending_uninstall: Option<usize>,
    error: Option<String>,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDiskFreeSpaceExW(dir: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
}

const DRIVE_FIXED: u32 = 3;

/// Total and free bytes of a local fixed drive. Removable, network and optical drives are skipped
/// because asking them can block while a disc spins up or a share times out.
fn drive_space(root: &str) -> Option<(u64, u64)> {
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is a NUL-terminated string and the out pointers refer to live locals.
    unsafe {
        if GetDriveTypeW(wide.as_ptr()) != DRIVE_FIXED || GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) == 0 {
            return None;
        }
    }
    Some((total, free))
}

/// Mounted drive roots from the system's drive bitmask; unlike probing each letter, this
/// never touches an empty floppy or a disconnected network drive.
fn logical_drives() -> Vec<String> {
    // SAFETY: the call takes no arguments and only returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z').enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, c)| format!("{c}:\\")).collect()
}

impl DeoxApp {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let drives = logical_drives();
        let drive_space = drives.iter().filter_map(|d| drive_space(d).map(|(total, free)| (d.clone(), total, free))).collect();
        DeoxApp {
            drive_space,
            ctx,
            tx,
            rx,
            cancel: Arc::new(Cancel::default()),
            phase: Phase::Idle,
            root: String::new(),
            scan_root: String::new(),
            drives,
            status: String::new(),
            files_seen: 0,
            bytes_seen: 0,
            live_folders: Vec::new(),
            live_reordered: Instant::now(),
            live_tree: None,
            scan_view: View::List,
            live_map: LiveMap::default(),
            move_progress: (0, 0),
            move_cancel: Arc::new(Cancel::default()),
            filter: String::new(),
            visible: Vec::new(),
            tab: View::List,
            map: MapState::default(),
            cursor_row: None,
            scroll_to: None,
            scroll_state: (0.0, 0.0),
            scan_folder: String::new(),
            results: None,
            reveal_start: 0.0,
            rows: Vec::new(),
            collapsed: HashSet::new(),
            selected: HashSet::new(),
            confirm_delete: false,
            // A first run should show what would happen before anything is moved.
            dry_run: true,
            last_delete: None,
            pending_uninstall: None,
            error: None,
        }
    }

    fn notifier(&self) -> Notifier {
        Notifier::new(self.tx.clone(), self.ctx.clone())
    }

    fn start_scan(&mut self) {
        // "Copy as path" in Explorer wraps the path in quotes.
        let mut root = self.root.trim().trim_matches('"').trim().to_string();
        if root.ends_with(':') {
            root.push('\\');
        }
        let root = PathBuf::from(root);
        if !root.is_dir() {
            self.error = Some(format!(
                "\"{}\" is not a folder or drive. Enter a path such as C:\\Users\\you, or choose one with Browse.",
                root.display()
            ));
            return;
        }
        self.scan_root = root.display().to_string();
        self.cancel = Arc::new(Cancel::default());
        self.phase = Phase::Scanning;
        self.files_seen = 0;
        self.bytes_seen = 0;
        self.live_folders.clear();
        self.live_map = LiveMap::default();
        let live = Arc::new(Mutex::new(LiveTree::new(&root)));
        self.live_tree = Some(live.clone());
        self.scan_folder.clear();
        self.status = "Starting scan…".into();
        self.results = None;
        self.rows.clear();
        self.selected.clear();
        worker::spawn_analysis(root, self.cancel.clone(), live, self.notifier());
    }

    fn modal_open(&self) -> bool {
        self.confirm_delete || self.last_delete.is_some() || self.pending_uninstall.is_some() || self.error.is_some()
    }

    fn begin_delete(&mut self, findings: Vec<Finding>, dry_run: bool) {
        self.phase = Phase::Deleting;
        self.move_progress = (0, findings.len());
        self.move_cancel = Arc::new(Cancel::default());
        self.status = if dry_run { "Checking the selected items…".into() } else { "Moving the selected items to the Recycle Bin…".into() };
        worker::spawn_delete(findings, dry_run, self.move_cancel.clone(), self.notifier());
    }

    fn set_results(&mut self, findings: Vec<Finding>, files: u64, unreadable: usize, tree: Option<FolderTree>) {
        // Only regenerating caches start selected; everything else is the user's call.
        self.selected =
            findings.iter().enumerate().filter(|(_, f)| f.confidence == Confidence::Safe).map(|(i, _)| i).collect();
        self.results = Some(Results { report: Report::new(findings), files, unreadable, tree });
        // A long list of duplicates would push everything else off screen; the user opens it on purpose.
        self.collapsed.insert(Category::Duplicate);
        self.tab = View::List;
        self.map = MapState::default();
        self.reveal_start = self.ctx.input(|i| i.time);
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(results) = &self.results else { return };
        let needle = self.filter.trim().to_lowercase();
        self.visible = results.report.findings.iter().map(|f| matches_filter(f, &needle)).collect();
        for group in results.report.groups() {
            let shown: Vec<(usize, &Finding)> = group.items.iter().filter(|(i, _)| self.visible[*i]).copied().collect();
            if shown.is_empty() {
                continue;
            }
            self.rows.push(Row::Header {
                category: group.category,
                total: shown.iter().map(|(_, f)| f.size).sum(),
                count: shown.len(),
            });
            if !self.collapsed.contains(&group.category) {
                self.rows.extend(shown.iter().map(|(i, _)| Row::Item(*i)));
            }
        }
        self.cursor_row = self.cursor_row.filter(|c| *c < self.rows.len());
    }

    fn live_folder_list(&self, ui: &mut egui::Ui) {
        const SHOWN: usize = 8;
        let Some(largest) = self.live_folders.iter().map(|f| f.1).max().filter(|m| *m > 0) else { return };
        ui.add_space(12.0);
        ui.label(RichText::new("Largest folders so far").weak());
        let width = ui.available_width().min(720.0);
        // Steel, not rust: rust means "can be freed", and nothing has been judged yet.
        let (bar, track) = if ui.visuals().dark_mode {
            (Color32::from_rgb(0x5B, 0x6B, 0x78), Color32::from_rgb(0x2A, 0x33, 0x3B))
        } else {
            (Color32::from_rgb(0x87, 0x95, 0xA1), Color32::from_rgb(0xD2, 0xD9, 0xDE))
        };
        let text = ui.visuals().text_color();
        for (name, size) in self.live_folders.iter().take(SHOWN) {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
            let name_w = 150.0;
            let size_w = 80.0;
            let track_rect = egui::Rect::from_min_max(
                rect.left_top() + egui::vec2(name_w, 5.0),
                rect.right_bottom() - egui::vec2(size_w, 5.0),
            );
            // Keyed by folder, so a row that moves keeps its own bar width instead of inheriting its neighbour's.
            let share = ui.ctx().animate_value_with_time(egui::Id::unique(("live_folder", name)), *size as f32 / largest as f32, 0.3);
            let fill = egui::Rect::from_min_size(track_rect.min, egui::vec2(track_rect.width() * share, track_rect.height()));
            let painter = ui.painter();
            painter.rect_filled(track_rect, 2.0, track);
            painter.rect_filled(fill, 2.0, bar);
            let name_font = egui::TextStyle::Body.resolve(ui.style());
            // One line, cut at the bar: a long name must not wrap into the rows around it.
            let shown = painter.layout_no_wrap(name.clone(), name_font, text);
            let name_clip = egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + name_w - 8.0, rect.bottom()));
            painter
                .with_clip_rect(name_clip)
                .galley(egui::pos2(rect.left(), rect.center().y - shown.size().y / 2.0), shown, text);
            painter.text(
                egui::pos2(rect.right(), rect.center().y),
                egui::Align2::RIGHT_CENTER,
                human(*size),
                egui::TextStyle::Monospace.resolve(ui.style()),
                text,
            );
        }
        if self.live_folders.len() > SHOWN {
            ui.label(RichText::new(format!("+ {} smaller", self.live_folders.len() - SHOWN)).weak());
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Status(s) => self.status = s,
            Msg::Progress(n, bytes) => (self.files_seen, self.bytes_seen) = (n, bytes),
            Msg::Folders(folders) => map::stable_order(&mut self.live_folders, folders, &mut self.live_reordered),
            Msg::Folder(folder) => self.scan_folder = folder,
            Msg::Moved(done, total) => self.move_progress = (done, total),
            Msg::Analysis(a) => {
                self.phase = Phase::Idle;
                self.live_tree = None;
                self.live_map = LiveMap::default();
                self.files_seen = a.files;
                if a.cancelled {
                    self.status = "Scan cancelled.".into();
                } else {
                    self.status = String::new();
                    self.set_results(a.findings, a.files, a.unreadable, a.tree);
                }
            }
            Msg::Deleted(report) => {
                self.phase = Phase::Idle;
                let moved: HashSet<PathBuf> =
                    report.entries.iter().filter(|e| e.outcome == Outcome::Deleted).map(|e| e.path.clone()).collect();
                if !moved.is_empty() {
                    self.drop_findings(&moved);
                }
                self.last_delete = Some((report, self.dry_run));
            }
            Msg::Failed(message) => {
                self.phase = Phase::Idle;
                self.live_tree = None;
                self.live_map = LiveMap::default();
                self.error = Some(message);
            }
        }
    }

    /// Removes findings that are gone from disk and keeps the rest selected as before.
    fn drop_findings(&mut self, gone: &HashSet<PathBuf>) {
        let Some(mut results) = self.results.take() else { return };
        if let Some(tree) = &mut results.tree {
            let removed: Vec<(&Path, u64, Confidence)> =
                results.report.findings.iter().filter(|f| gone.contains(&f.path)).map(|f| (f.path.as_path(), f.size, f.confidence)).collect();
            tree.remove_all(&removed);
        }
        self.map.picked = None;
        let keep_selected: HashSet<PathBuf> =
            self.selected.iter().filter_map(|i| results.report.findings.get(*i)).map(|f| f.path.clone()).collect();
        let findings: Vec<Finding> = results.report.findings.into_iter().filter(|f| !gone.contains(&f.path)).collect();
        self.selected =
            findings.iter().enumerate().filter(|(_, f)| keep_selected.contains(&f.path)).map(|(i, _)| i).collect();
        self.results = Some(Results { report: Report::new(findings), ..results });
        self.rebuild_rows();
    }

    fn selected_findings(&self) -> Vec<Finding> {
        let Some(results) = &self.results else { return Vec::new() };
        let mut indices: Vec<usize> = self.selected.iter().copied().collect();
        indices.sort_unstable();
        indices.into_iter().map(|i| results.report.findings[i].clone()).collect()
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let idle = self.phase == Phase::Idle;
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Folder or drive:");
            let field = ui.add_enabled(
                idle,
                egui::TextEdit::singleline(&mut self.root).desired_width(360.0).hint_text(r"C:\Users\you"),
            );
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && idle && !self.root.trim().is_empty() {
                self.start_scan();
            }
            if ui.add_enabled(idle, egui::Button::new("Browse…")).clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.root = dir.display().to_string();
                }
            }
            for drive in self.drives.clone() {
                if ui.add_enabled(idle, egui::Button::new(drive.trim_end_matches('\\'))).clicked() {
                    self.root = drive;
                }
            }
            ui.separator();
            match self.phase {
                Phase::Scanning => {
                    if ui.button("Cancel").clicked() {
                        self.cancel.cancel();
                        self.status = "Cancelling…".into();
                    }
                }
                _ => {
                    if ui.add_enabled(idle && !self.root.trim().is_empty(), egui::Button::new("Scan")).clicked() {
                        self.start_scan();
                    }
                }
            }
        });
        ui.add_space(6.0);
    }

    /// Left-aligned with fixed offsets, so switching between List and Map changes only what is below the header.
    fn scan_screen(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.add(egui::Label::new(RichText::new(&self.scan_root).weak()).truncate());
        ui.label(RichText::new(human(self.bytes_seen)).monospace().size(40.0));
        ui.label(format!("{} files checked", self.files_seen));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            step_ticks(ui, scan_step(&self.status));
            ui.label(&self.status);
        });
        ui.add_space(6.0);
        sweep(ui);
        ui.add_space(6.0);
        // Always one line tall, so the controls below stay put before the first folder is known.
        let current = if self.scan_folder.is_empty() { " " } else { &self.scan_folder };
        ui.add(egui::Label::new(RichText::new(current).weak().small()).truncate());
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.scan_view, View::List, "List");
            ui.selectable_value(&mut self.scan_view, View::Map, "Map");
        });
        if self.scan_view == View::Map {
            map::live_map_view(ui, self.live_tree.as_ref(), &mut self.live_map);
        } else {
            self.live_folder_list(ui);
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        match self.phase {
            Phase::Scanning => self.scan_screen(ui),
            Phase::Deleting => self.deleting_screen(ui),
            Phase::Idle if self.results.is_none() => self.start_screen(ui),
            Phase::Idle => self.results_screen(ui),
        }
    }

    fn deleting_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(120.0);
            ui.spinner();
            ui.add_space(8.0);
            ui.label(&self.status);
            let (done, total) = self.move_progress;
            if total > 0 {
                ui.add_space(8.0);
                ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(360.0));
                ui.label(format!("{done} of {total}"));
                ui.add_space(8.0);
                ui.label(RichText::new("Many small files take a few minutes. Please keep Deox open.").weak());
                ui.add_space(8.0);
                if ui.button("Stop after this batch").clicked() {
                    self.move_cancel.cancel();
                    self.status = "Stopping…".into();
                }
            }
        });
    }

    fn start_screen(&mut self, ui: &mut egui::Ui) {
        struct Place {
            title: String,
            caption: &'static str,
            root: String,
            space: Option<(u64, u64)>,
        }
        let mut places = Vec::new();
        if let Some(home) = std::env::var_os("USERPROFILE").filter(|h| PathBuf::from(h).is_dir()) {
            let home = PathBuf::from(home).display().to_string();
            places.push(Place { title: "Your user folder".into(), caption: "A good place to start", root: home, space: None });
        }
        for (root, total, free) in &self.drive_space {
            places.push(Place {
                title: root.trim_end_matches('\\').into(),
                caption: "Whole drive",
                root: root.clone(),
                space: Some((total.saturating_sub(*free), *total)),
            });
        }
        let mut chosen = None;
        let top = ((ui.available_height() - 400.0) / 2.0).max(24.0);
        ui.add_space(top);
        ui.horizontal(|ui| {
            let available = ui.available_width();
            let column = START_COLUMN.min(available);
            ui.add_space(((available - column) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(column);
                ui.label(RichText::new("Find what is taking space").size(32.0));
                ui.add_space(4.0);
                ui.label(RichText::new("Choose where to look. Nothing is removed without your confirmation.").weak());
                ui.add_space(24.0);
                ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0);
                let card_width = ((column - 16.0) / 2.0).floor();
                ui.horizontal_wrapped(|ui| {
                    for Place { title, caption, root, space } in &places {
                        let frame = egui::Frame::group(ui.style()).inner_margin(14.0).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
                            ui.set_width(card_width - 30.0);
                            ui.set_min_height(86.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.label(RichText::new(title).size(22.0));
                                ui.label(RichText::new(*caption).weak());
                                if let Some((used, total)) = space {
                                    ui.add_space(6.0);
                                    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
                                    let colors = &ui.visuals().widgets;
                                    ui.painter().rect_filled(bar, 2.0, colors.noninteractive.bg_stroke.color);
                                    let width = bar.width() * *used as f32 / (*total).max(1) as f32;
                                    let fill = egui::Rect::from_min_size(bar.min, egui::vec2(width, bar.height()));
                                    ui.painter().rect_filled(fill, 2.0, colors.noninteractive.fg_stroke.color.gamma_multiply(0.6));
                                    ui.label(RichText::new(format!("{} used of {}", human(*used), human(*total))).weak());
                                } else {
                                    ui.add_space(6.0);
                                    ui.add(egui::Label::new(RichText::new(root).monospace().small().weak()).truncate());
                                }
                            });
                        });
                        let rect = frame.response.rect;
                        let click = ui.interact(rect, ui.scope_id().with(("place", root)), egui::Sense::click());
                        click.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Scan {title}")));
                        if click.hovered() {
                            let stroke = egui::Stroke::new(1.5, ui.visuals().selection.stroke.color);
                            ui.painter().rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
                        }
                        if click.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            chosen = Some(root.clone());
                        }
                    }
                });
                ui.add_space(20.0);
                legend_line(ui);
                if !self.status.is_empty() {
                    ui.add_space(12.0);
                    ui.label(&self.status);
                }
            });
        });
        if let Some(root) = chosen {
            self.root = root;
            self.start_scan();
        }
    }

    fn results_screen(&mut self, ui: &mut egui::Ui) {
        let Some(results) = &self.results else { return };
        let (removable_count, removable_size) = removable(&results.report);
        if removable_count == 0 {
            ui.label(format!("Scanned {} files. System folders such as Windows and Program Files are skipped.", results.files));
            ui.add_space(8.0);
            ui.label("Nothing to clean up here. Try another folder or a whole drive.");
        } else {
            let by = |level| results.report.findings.iter().filter(|f| f.confidence == level).map(|f| f.size).sum::<u64>();
            let selected_size: u64 = self.selected.iter().filter_map(|i| results.report.findings.get(*i)).map(|f| f.size).sum();
            ui.horizontal(|ui| {
                ui.label(RichText::new(human(selected_size)).size(34.0).strong());
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("selected for the Recycle Bin").strong());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} selected.", items(self.selected.len()))).weak());
                        if ui.small_button("Select all Safe").clicked() {
                            self.selected = results
                                .report
                                .findings
                                .iter()
                                .enumerate()
                                .filter(|(_, f)| f.confidence == Confidence::Safe)
                                .map(|(i, _)| i)
                                .collect();
                        }
                        if ui.small_button("Clear").clicked() {
                            self.selected.clear();
                        }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    ui.vertical(|ui| {
                        let none_selected = self.selected.is_empty();
                        let label = "Move to Recycle Bin…";
                        let primary = egui::Button::new(RichText::new(label).size(16.0).strong().color(Color32::WHITE))
                            .fill(ui.visuals().selection.bg_fill)
                            .min_size(egui::vec2(220.0, 38.0));
                        let button = ui.add_enabled(!none_selected, primary);
                        if none_selected {
                            button.clone().on_disabled_hover_text("Select at least one item first.");
                        }
                        if button.clicked() {
                            self.confirm_delete = true;
                        }
                        ui.checkbox(&mut self.dry_run, RichText::new("Preview only: nothing is moved").weak())
                            .on_hover_text("Runs every check and reports what would happen. Nothing is moved.");
                    });
                });
            });
            if let Some(tree) = &results.tree {
                let elapsed = ui.input(|i| i.time) - self.reveal_start;
                let progress = (elapsed / REVEAL_SECS).clamp(0.0, 1.0) as f32;
                if progress < 1.0 {
                    ui.ctx().request_repaint();
                }
                let eased = 1.0 - (1.0 - progress).powi(3);
                reclaim_strip(ui, by(Confidence::Safe), by(Confidence::Review), tree.node(FolderTree::ROOT).size, eased);
            }
        }
        ui.separator();

        // Wide windows show the map beside the list; narrow ones switch between them.
        let split = results.tree.is_some() && ui.available_width() >= SPLIT_MIN_WIDTH;
        if split && self.tab == View::Map {
            self.tab = View::List;
        }
        let mut map_action = None;
        if split {
            if removable_count > 0 {
                scan_notes(ui, results);
            }
            if let Some(tree) = &results.tree {
                let dark = ui.visuals().dark_mode;
                let width = ui.available_width() * 0.55;
                egui::Panel::left("map_panel").resizable(true).default_size(width).size_range(320.0..=width * 1.4).show(ui, |ui| {
                    map_action = map::map_view(ui, tree, &mut self.map, dark, &results.report, &self.selected);
                });
            }
        } else {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, View::List, "List");
                ui.add_enabled_ui(results.tree.is_some(), |ui| {
                    ui.selectable_value(&mut self.tab, View::Map, "Map");
                })
                .response
                .on_disabled_hover_text("The map is not available for this scan.");
            });
        }
        if removable_count > 0 && !split && self.tab == View::List {
            scan_notes(ui, results);
        }

        if let (View::Map, Some(tree)) = (self.tab, &results.tree) {
            let dark = ui.visuals().dark_mode;
            if let Some(action) = map::map_view(ui, tree, &mut self.map, dark, &results.report, &self.selected) {
                self.apply_map_action(action);
            }
            return;
        }
        self.results_list(ui, removable_size);
        if let Some(action) = map_action {
            self.apply_map_action(action);
        }
    }

    fn results_list(&mut self, ui: &mut egui::Ui, removable_size: u64) {
        ui.add_space(4.0);

        let mut filter_changed = false;
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let field = ui
                .add(egui::TextEdit::singleline(&mut self.filter).desired_width(240.0).hint_text("name or path"))
                .on_hover_text("Ctrl+A selects everything shown, except installed programs.");
            filter_changed = field.changed();
            if !self.filter.trim().is_empty() {
                let shown = self.visible.iter().filter(|v| **v).count();
                ui.label(RichText::new(format!("{shown} match")).weak());
            }
            if !self.filter.is_empty() && ui.button("Clear").clicked() {
                self.filter.clear();
                filter_changed = true;
            }
        });
        let select_visible = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) && !ui.ctx().egui_wants_keyboard_input();
        if self.rows.is_empty() && !self.filter.trim().is_empty() {
            ui.add_space(8.0);
            ui.label(format!("Nothing matches \"{}\".", self.filter.trim()));
        }

        let row_height = ui.text_style_height(&egui::TextStyle::Body).max(ui.spacing().interact_size.y) + ui.spacing().item_spacing.y;
        let row_pitch = row_height + ui.spacing().item_spacing.y;
        let (key_action, key_enter) = self.list_keys(ui, row_pitch);
        let Some(results) = &self.results else { return };
        let cursor = self.cursor_row;
        let dark = ui.visuals().dark_mode;
        let mut toggled = None;
        let mut category_select: Option<(Category, bool)> = None;
        let mut uninstall = None;
        let mut locate: Option<usize> = None;
        let mut clicked_row: Option<usize> = None;
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(y) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(y);
        }
        let output = area.show_rows(ui, row_height, self.rows.len(), |ui, range| {
            for (pos, row) in (range.start..).zip(&self.rows[range]) {
                let highlight = ui.painter().add(egui::Shape::Noop);
                let row_top = ui.cursor().min.y;
                match row {
                    Row::Header { category, total, count } => {
                        let (chosen, total_selectable) = selectable(&results.report, &self.visible, Some(*category))
                            .fold((0, 0), |(c, t), i| (c + usize::from(self.selected.contains(&i)), t + 1));
                        ui.horizontal(|ui| {
                            ui.set_min_height(row_height);
                            if total_selectable > 0 {
                                let mut on = chosen == total_selectable;
                                let partial = chosen > 0 && chosen < total_selectable;
                                let check = ui.add(egui::Checkbox::new(&mut on, "").indeterminate(partial));
                                name_checkbox(&check, on, &format!("Select all in {}", category_name(*category)));
                                if check.changed() {
                                    category_select = Some((*category, on));
                                }
                                check.on_hover_text("Select or clear every item in this group");
                            } else {
                                ui.add_space(CHECK_W);
                            }
                            let open = !self.collapsed.contains(category);
                            let icon = disclosure_icon(ui, open);
                            let label = ui.add(egui::Button::new(RichText::new(category_name(*category)).size(16.0)).frame(false));
                            if icon.clicked() || label.clicked() {
                                toggled = Some(*category);
                            }
                            ui.label(RichText::new(items(*count)).weak());
                            ui.label(RichText::new(human(*total)).monospace());
                            if total_selectable > 0 {
                                share_bar(ui, *total, removable_size.max(1));
                            }
                        });
                    }
                    Row::Item(i) => {
                        let finding = &results.report.findings[*i];
                        let (name, parent) = if finding.path.as_os_str().is_empty() {
                            (finding.rule.clone(), String::new())
                        } else {
                            let name = finding.path.file_name().map_or_else(|| finding.path.display().to_string(), |n| n.to_string_lossy().into_owned());
                            (name, finding.path.parent().map(|p| p.display().to_string()).unwrap_or_default())
                        };
                        let full_path = finding.path.display().to_string();
                        let can_locate = results.tree.is_some() && !full_path.is_empty();
                        // The whole row is the click target. It is registered before its content, so the checkbox
                        // and buttons on top of it keep their own clicks.
                        let row_rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), row_height));
                        let row = ui.interact(row_rect, ui.auto_id_with(("row", pos)), egui::Sense::click());
                        ui.horizontal(|ui| {
                            ui.add_space(ITEM_INDENT);
                            cell(ui, CHECK_W, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                if finding.confidence != Confidence::Info {
                                    let mut on = self.selected.contains(i);
                                    let check = ui.checkbox(&mut on, "");
                                    name_checkbox(&check, on, &format!("{name} ({})", human(finding.size)));
                                    if check.changed() {
                                        if on {
                                            self.selected.insert(*i);
                                        } else {
                                            self.selected.remove(i);
                                        }
                                    }
                                }
                            });
                            let (tag, color, meaning) = confidence_tag(finding.confidence, dark);
                            cell(ui, TAG_W, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                level_glyph(ui, finding.confidence, color);
                                ui.label(RichText::new(tag).strong()).on_hover_text(meaning);
                            });
                            cell(ui, SIZE_W, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(human(finding.size)).monospace());
                            });
                            ui.add(egui::Label::new(RichText::new(name).strong()).truncate().selectable(false));
                            if !parent.is_empty() {
                                ui.add(egui::Label::new(RichText::new(parent).weak()).truncate().selectable(false));
                            }
                            if finding.confidence == Confidence::Info && finding.uninstall.is_some() && ui.button("Uninstall…").clicked() {
                                uninstall = Some(*i);
                            }
                        });
                        // The reason sits on hover so every row stays one line.
                        let mut hover = if full_path.is_empty() { finding.reason.clone() } else { format!("{full_path}\n{}", finding.reason) };
                        if can_locate {
                            hover.push_str("\nDouble-click to show on the map.");
                        }
                        if row.clicked() {
                            clicked_row = Some(pos);
                        }
                        if can_locate && row.double_clicked() {
                            locate = Some(*i);
                        }
                        let row = if can_locate { row.on_hover_cursor(egui::CursorIcon::PointingHand) } else { row };
                        row.on_hover_text(hover);
                    }
                }
                let rect = egui::Rect::from_min_max(egui::pos2(ui.max_rect().left(), row_top), egui::pos2(ui.max_rect().right(), ui.cursor().min.y));
                let tint = if cursor == Some(pos) {
                    Some(ui.visuals().selection.bg_fill.gamma_multiply(0.45))
                } else if ui.rect_contains_pointer(rect) {
                    Some(ui.visuals().widgets.hovered.bg_fill.gamma_multiply(0.7))
                } else {
                    None
                };
                if let Some(tint) = tint {
                    ui.painter().set(highlight, egui::Shape::rect_filled(rect, 3.0, tint));
                }
            }
        });
        self.scroll_state = (output.state.clamped_offset().y, output.inner_rect.height());
        if let Some(pos) = key_action {
            match self.rows.get(pos) {
                Some(Row::Item(i)) if results.report.findings[*i].confidence != Confidence::Info => {
                    if !self.selected.remove(i) {
                        self.selected.insert(*i);
                    }
                }
                Some(Row::Header { category, .. }) => {
                    if key_enter {
                        toggled = Some(*category);
                    } else {
                        let (chosen, total) = selectable(&results.report, &self.visible, Some(*category))
                            .fold((0, 0), |(c, t), i| (c + usize::from(self.selected.contains(&i)), t + 1));
                        if total > 0 {
                            category_select = Some((*category, chosen < total));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some((category, on)) = category_select {
            for i in selectable(&results.report, &self.visible, Some(category)) {
                if on {
                    self.selected.insert(i);
                } else {
                    self.selected.remove(&i);
                }
            }
        }
        if select_visible {
            self.selected.extend(selectable(&results.report, &self.visible, None));
        }
        if clicked_row.is_some() {
            self.cursor_row = clicked_row;
        }
        if let Some(finding) = locate {
            self.show_on_map(finding);
        }
        if filter_changed {
            self.rebuild_rows();
        }
        if let Some(category) = toggled {
            if !self.collapsed.remove(&category) {
                self.collapsed.insert(category);
            }
            self.rebuild_rows();
        }
        if uninstall.is_some() {
            self.pending_uninstall = uninstall;
        }
    }

    /// Moves the keyboard cursor across rows and reports Space/Enter on the current row.
    fn list_keys(&mut self, ui: &egui::Ui, row_pitch: f32) -> (Option<usize>, bool) {
        if self.modal_open() {
            return (None, false);
        }
        if let Some(id) = ui.memory(|m| m.focused()) {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                ui.memory_mut(|m| m.surrender_focus(id));
            }
            return (None, false);
        }
        if ui.ctx().egui_wants_keyboard_input() || self.rows.is_empty() {
            return (None, false);
        }
        let none = egui::Modifiers::NONE;
        let (down, up, home, end, space, enter) = ui.input_mut(|i| {
            (
                i.consume_key(none, egui::Key::ArrowDown),
                i.consume_key(none, egui::Key::ArrowUp),
                i.consume_key(none, egui::Key::Home),
                i.consume_key(none, egui::Key::End),
                i.consume_key(none, egui::Key::Space),
                i.consume_key(none, egui::Key::Enter),
            )
        });
        let last = self.rows.len() - 1;
        let current = self.cursor_row;
        let moved_to = if down {
            Some(current.map_or(0, |c| (c + 1).min(last)))
        } else if up {
            Some(current.map_or(0, |c| c.saturating_sub(1)))
        } else if home {
            Some(0)
        } else if end {
            Some(last)
        } else {
            None
        };
        if let Some(target) = moved_to {
            self.cursor_row = Some(target);
            let (offset, height) = self.scroll_state;
            let top = target as f32 * row_pitch;
            if top < offset {
                self.scroll_to = Some(top);
            } else if top + row_pitch > offset + height {
                self.scroll_to = Some(top + row_pitch - height);
            }
        }
        if (space || enter) && current.is_some() {
            (current, enter)
        } else {
            (None, false)
        }
    }

    /// Opens the folder that holds a finding in the map and points out the rectangle it is: its own folder,
    /// its file, or the small files of the folder when it is not listed one by one.
    fn show_on_map(&mut self, finding: usize) {
        let Some(results) = &self.results else { return };
        let (Some(tree), Some(f)) = (&results.tree, results.report.findings.get(finding)) else { return };
        let (found, exact) = tree.locate(&f.path);
        let (folder, pick) = if exact {
            let own = found != FolderTree::ROOT;
            (if own { tree.node(found).parent.unwrap_or(found) } else { found }, own.then_some(Pick::Folder(found)))
        } else {
            let listed = f.path.file_name().and_then(|name| tree.file_index(found, name));
            (found, Some(listed.map_or(Pick::Rest(found), |index| Pick::File { folder: found, index })))
        };
        let now = self.ctx.input(|i| i.time);
        self.map.open(folder, now, None);
        self.map.picked = pick;
        self.tab = View::Map;
    }

    fn apply_map_action(&mut self, action: MapAction) {
        self.ctx.request_repaint();
        match action {
            MapAction::ShowInList(path) => {
                self.filter = path;
                self.tab = View::List;
                self.rebuild_rows();
            }
            MapAction::SelectSafe(path) => {
                let Some(results) = &self.results else { return };
                for (i, f) in results.report.findings.iter().enumerate() {
                    if f.confidence == Confidence::Safe && f.path.starts_with(&path) {
                        self.selected.insert(i);
                    }
                }
            }
            MapAction::ClearHere(path) => {
                let Some(results) = &self.results else { return };
                self.selected.retain(|i| !results.report.findings.get(*i).is_some_and(|f| f.path.starts_with(&path)));
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if self.confirm_delete {
            self.confirm_delete_dialog(ctx, escape);
        }
        self.delete_report_dialog(ctx, escape);
        self.uninstall_dialog(ctx, escape);
        self.error_dialog(ctx, escape);
    }

    fn confirm_delete_dialog(&mut self, ctx: &egui::Context, escape: bool) {
        let mut summary: Vec<(Category, usize, u64)> = Vec::new();
        let mut total = 0;
        let mut count = 0;
        if let Some(results) = &self.results {
            for f in self.selected.iter().filter_map(|i| results.report.findings.get(*i)) {
                total += f.size;
                count += 1;
                match summary.iter_mut().find(|(c, _, _)| *c == f.category) {
                    Some(row) => {
                        row.1 += 1;
                        row.2 += f.size;
                    }
                    None => summary.push((f.category, 1, f.size)),
                }
            }
        }
        summary.sort_by_key(|(_, _, size)| std::cmp::Reverse(*size));
        let mut run = false;
        let mut cancel = false;
        let title = if self.dry_run { "Check before moving" } else { "Move to Recycle Bin?" };
        egui::Window::new(title).collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_width(dialog_width(ctx, 640.0));
            ui.label(format!("{} ({}) will be moved to the Recycle Bin. You can restore them from there.", items(count), human(total)));
            ui.label("Anything that changed since the scan, or sits in a protected location, is skipped.");
            ui.add_space(6.0);
            for (category, n, size) in &summary {
                ui.horizontal(|ui| {
                    cell(ui, SIZE_W, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(human(*size)).monospace());
                    });
                    ui.label(format!("{}  ·  {}", category_name(*category), items(*n)));
                });
            }
            ui.add_space(6.0);
            egui::CollapsingHeader::new("Show every item").show(ui, |ui| {
                let Some(results) = &self.results else { return };
                let mut indices: Vec<usize> = self.selected.iter().copied().collect();
                indices.sort_unstable();
                egui::ScrollArea::vertical().max_height(260.0).show_rows(ui, 18.0, indices.len(), |ui, range| {
                    for f in indices[range].iter().filter_map(|i| results.report.findings.get(*i)) {
                        ui.add(egui::Label::new(format!("{:>10}  {}", human(f.size), f.path.display())).truncate());
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(if self.dry_run { "Preview only is on: nothing will be moved." } else { "Move for real is on." }).weak(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = if self.dry_run { "Check items".to_string() } else { format!("Move {} to Recycle Bin", items(count)) };
                run = ui.button(label).clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if run {
            self.confirm_delete = false;
            self.begin_delete(self.selected_findings(), self.dry_run);
        } else if cancel || escape {
            self.confirm_delete = false;
        }
    }

    fn delete_report_dialog(&mut self, ctx: &egui::Context, escape: bool) {
        let Some((report, dry_run)) = &self.last_delete else { return };
        let mut close = false;
        let mut move_now = false;
        egui::Window::new(if *dry_run { "Check complete" } else { "Moved to Recycle Bin" })
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(dialog_width(ctx, 640.0));
                let count = |o: fn(&Outcome) -> bool| report.entries.iter().filter(|e| o(&e.outcome)).count();
                let ready = count(|o| *o == Outcome::WouldDelete);
                if *dry_run {
                    ui.label(format!("{} can be moved. Nothing was changed.", items(ready)));
                } else {
                    ui.label(format!(
                        "{} moved to the Recycle Bin. Empty the Recycle Bin to free {}.",
                        items(count(|o| *o == Outcome::Deleted)),
                        human(report.freed_bytes())
                    ));
                }
                if report.cancelled {
                    ui.label(format!("Stopped early. {} left exactly as they were.", items(report.not_attempted)));
                }
                let problems: Vec<_> =
                    report.entries.iter().filter(|e| e.outcome != Outcome::Deleted && e.outcome != Outcome::WouldDelete).collect();
                if !problems.is_empty() {
                    ui.add_space(6.0);
                    ui.label(format!("{} skipped:", items(problems.len())));
                    if problems.iter().any(|e| matches!(e.outcome, Outcome::Failed(_))) {
                        ui.label(RichText::new("Windows refused some items, usually because a program is using them. Close browsers, chat apps and editors, then scan and try again.").weak());
                    }
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        let mut shown: Vec<_> = problems.iter().take(MAX_LISTED_SKIPS).collect();
                        shown.sort_by_key(|e| describe(&e.outcome));
                        let mut current = String::new();
                        for e in shown {
                            let reason = describe(&e.outcome);
                            if reason != current {
                                let count = problems.iter().filter(|p| describe(&p.outcome) == reason).count();
                                ui.add_space(6.0);
                                let heading = ui.add(egui::Label::new(RichText::new(format!("{reason}, {}", items(count))).strong()));
                                if let Outcome::Failed(why) = &e.outcome {
                                    heading.on_hover_text(why);
                                }
                                current = reason;
                            }
                            ui.add(egui::Label::new(RichText::new(e.path.display().to_string()).weak().small()).wrap());
                        }
                        if problems.len() > MAX_LISTED_SKIPS {
                            ui.label(format!("…and {} more. All of them are in the action log.", items(problems.len() - MAX_LISTED_SKIPS)));
                        }
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if *dry_run && ready > 0 {
                        move_now = ui.button(format!("Move {} to Recycle Bin", items(ready))).clicked();
                    }
                    close = ui.button(if *dry_run && ready > 0 { "Not now" } else { "Close" }).clicked();
                });
            });
        if move_now {
            self.last_delete = None;
            self.dry_run = false;
            self.begin_delete(self.selected_findings(), false);
        } else if close || escape {
            self.last_delete = None;
        }
    }

    fn uninstall_dialog(&mut self, ctx: &egui::Context, escape: bool) {
        let Some(i) = self.pending_uninstall else { return };
        let finding = self.results.as_ref().and_then(|r| r.report.findings.get(i)).cloned();
        let mut done = finding.is_none();
        if let Some(finding) = finding {
            egui::Window::new("Open uninstaller").collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_width(dialog_width(ctx, 520.0));
                ui.label(&finding.reason);
                ui.label("Deox only starts the program's own uninstaller. It removes nothing itself.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Open uninstaller").clicked() {
                        if let Err(e) = launch_uninstaller(&finding) {
                            self.error = Some(format!("{e:#}"));
                        }
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
            });
        }
        if done || escape {
            self.pending_uninstall = None;
        }
    }

    fn error_dialog(&mut self, ctx: &egui::Context, escape: bool) {
        let Some(message) = &self.error else { return };
        let mut close = false;
        egui::Window::new("Couldn't complete that").collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_width(dialog_width(ctx, 480.0));
            ui.label(message);
            ui.add_space(6.0);
            close = ui.button("Close").clicked();
        });
        if close || escape {
            self.error = None;
        }
    }
}

impl eframe::App for DeoxApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let modal = self.modal_open();
        ui.add_enabled_ui(!modal, |ui| {
            egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
            let frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(20, 14));
            egui::CentralPanel::default().frame(frame).show(ui, |ui| self.central(ui));
        });
        self.dialogs(&ctx);
    }
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Deleted | Outcome::WouldDelete => "ok".into(),
        Outcome::Protected => "Protected location".into(),
        Outcome::NotDeletable => "Can't be removed".into(),
        Outcome::Missing => "Already gone".into(),
        Outcome::Changed => "Changed since the scan".into(),
        Outcome::Failed(_) => "Windows refused to move them (hover for the technical detail)".into(),
    }
}

/// Count and size of the findings Deox can actually move; installed programs are only reported.
fn removable(report: &Report) -> (usize, u64) {
    report.findings.iter().filter(|f| f.confidence != Confidence::Info).fold((0, 0), |(n, s), f| (n + 1, s + f.size))
}

/// The colour key and how much was scanned, across the full width so the key never wraps.
fn scan_notes(ui: &mut egui::Ui, results: &Results) {
    legend_line(ui);
    let skipped = if results.unreadable > 0 { format!(" {} couldn't be read.", paths(results.unreadable)) } else { String::new() };
    ui.label(RichText::new(format!("{} files scanned.{skipped}", results.files)).weak());
}

/// Indices of findings that pass the current filter and can be selected for removal (optionally within one category).
fn selectable<'a>(report: &'a Report, visible: &'a [bool], category: Option<Category>) -> impl Iterator<Item = usize> + 'a {
    report.findings.iter().enumerate().filter_map(move |(i, f)| {
        let in_group = category.is_none_or(|c| f.category == c);
        (in_group && f.confidence != Confidence::Info && visible.get(i).copied().unwrap_or(false)).then_some(i)
    })
}

/// Case-insensitive match on a finding's path or rule name; an empty filter matches everything.
fn matches_filter(finding: &Finding, needle: &str) -> bool {
    needle.is_empty()
        || finding.path.to_string_lossy().to_lowercase().contains(needle)
        || finding.rule.to_lowercase().contains(needle)
}

#[cfg(test)]
mod screens_test;
