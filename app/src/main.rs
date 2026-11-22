#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod worker;

use deox_core::delete::{launch_uninstaller, DeleteReport, Outcome};
use deox_core::report::{Category, Confidence, Finding, Report};
use deox_core::scan::Cancel;
use deox_core::treemap::{squarify, FolderTree, Rect};
use eframe::egui::{self, Color32, RichText};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use worker::{Msg, Notifier};

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

/// Darkens or lightens the default text so that even the dim text used for reasons and paths
/// keeps at least 4.5:1 against the panel in both themes (egui's own defaults give about 2.7:1).
fn configure_theme(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes, family) in [
        ("Lato-Light", &include_bytes!("../assets/fonts/Lato-Light.ttf")[..], egui::FontFamily::Proportional),
        ("JetBrainsMono", &include_bytes!("../assets/fonts/JetBrainsMono.ttf")[..], egui::FontFamily::Monospace),
    ] {
        fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(bytes)));
        // Placed first, so the bundled font is used and egui's own remain as fallback for missing glyphs.
        fonts.families.entry(family).or_default().insert(0, name.into());
    }
    ctx.set_fonts(fonts);

    ctx.style_mut_of(egui::Theme::Dark, |style| {
        let v = &mut style.visuals;
        v.panel_fill = Color32::from_rgb(0x1D, 0x23, 0x29);
        v.window_fill = Color32::from_rgb(0x1D, 0x23, 0x29);
        v.extreme_bg_color = Color32::from_rgb(0x15, 0x19, 0x1D);
        v.faint_bg_color = Color32::from_rgb(0x22, 0x29, 0x30);
        v.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(0x2E, 0x38, 0x41);
        v.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(0xC4, 0xCD, 0xD5);
        v.widgets.inactive.bg_fill = Color32::from_rgb(0x2A, 0x33, 0x3B);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x2A, 0x33, 0x3B);
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x36, 0x42, 0x4C);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x36, 0x42, 0x4C);
        v.selection.bg_fill = Color32::from_rgb(0x2F, 0x8F, 0x7B);
        v.selection.stroke.color = Color32::from_rgb(0x4F, 0xC3, 0xA8);
        v.weak_text_alpha = 0.72;
    });
    ctx.style_mut_of(egui::Theme::Light, |style| {
        let v = &mut style.visuals;
        v.panel_fill = Color32::from_rgb(0xE4, 0xE8, 0xEB);
        v.window_fill = Color32::from_rgb(0xEE, 0xF1, 0xF3);
        v.extreme_bg_color = Color32::from_rgb(0xF6, 0xF8, 0xF9);
        v.faint_bg_color = Color32::from_rgb(0xDC, 0xE1, 0xE5);
        v.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(0xC3, 0xCB, 0xD2);
        v.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(0x2B, 0x33, 0x3A);
        v.widgets.inactive.bg_fill = Color32::from_rgb(0xD2, 0xD9, 0xDE);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xD2, 0xD9, 0xDE);
        v.selection.bg_fill = Color32::from_rgb(0x1F, 0x7A, 0x68);
        v.selection.stroke.color = Color32::from_rgb(0x15, 0x5E, 0x4F);
        v.weak_text_alpha = 0.82;
    });
}

/// Keeps a dialog inside a narrow window.
fn dialog_width(ctx: &egui::Context, wanted: f32) -> f32 {
    wanted.min(ctx.content_rect().width() - 48.0).max(320.0)
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
    /// Folder sizes for the map; dropped after a move, because they would no longer match the disk.
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
    /// Items settled and total while a move or check is running.
    move_progress: (usize, usize),
    move_cancel: Arc<Cancel>,
    /// Narrows the result list to names and paths containing this text.
    filter: String,
    /// Per finding: whether it passes the current filter.
    visible: Vec<bool>,
    view: View,
    /// The folder shown in the map, as a node of the folder tree.
    map_folder: usize,
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
            move_progress: (0, 0),
            move_cancel: Arc::new(Cancel::default()),
            filter: String::new(),
            visible: Vec::new(),
            view: View::List,
            map_folder: FolderTree::ROOT,
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
        self.scan_folder.clear();
        self.status = "Starting scan…".into();
        self.results = None;
        self.rows.clear();
        self.selected.clear();
        worker::spawn_analysis(root, self.cancel.clone(), self.notifier());
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
        self.map_folder = FolderTree::ROOT;
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

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Status(s) => self.status = s,
            Msg::Progress(n) => self.files_seen = n,
            Msg::Folder(folder) => self.scan_folder = folder,
            Msg::Moved(done, total) => self.move_progress = (done, total),
            Msg::Analysis(a) => {
                self.phase = Phase::Idle;
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
                self.error = Some(message);
            }
        }
    }

    /// Removes findings that are gone from disk and keeps the rest selected as before.
    fn drop_findings(&mut self, gone: &HashSet<PathBuf>) {
        let Some(results) = self.results.take() else { return };
        let keep_selected: HashSet<PathBuf> =
            self.selected.iter().filter_map(|i| results.report.findings.get(*i)).map(|f| f.path.clone()).collect();
        let findings: Vec<Finding> = results.report.findings.into_iter().filter(|f| !gone.contains(&f.path)).collect();
        self.selected =
            findings.iter().enumerate().filter(|(_, f)| keep_selected.contains(&f.path)).map(|(i, _)| i).collect();
        self.results = Some(Results { report: Report::new(findings), tree: None, ..results });
        self.view = View::List;
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

    fn footer(&mut self, ui: &mut egui::Ui) {
        let Some(results) = &self.results else { return };
        let selected_size: u64 = self.selected.iter().filter_map(|i| results.report.findings.get(*i)).map(|f| f.size).sum();
        let idle = self.phase == Phase::Idle;
        let (removable_count, removable_size) = removable(&results.report);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(idle, egui::Button::new("Select all Safe")).clicked() {
                self.selected = results
                    .report
                    .findings
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.confidence == Confidence::Safe)
                    .map(|(i, _)| i)
                    .collect();
            }
            if ui.add_enabled(idle, egui::Button::new("Clear selection")).clicked() {
                self.selected.clear();
            }
            ui.separator();
            ui.label(RichText::new(human(selected_size)).strong().size(16.0));
            ui.label(RichText::new(format!("selected: {} of {}, out of {}", self.selected.len(), removable_count, human(removable_size))).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let none_selected = self.selected.is_empty();
                let label = if self.dry_run { "Check items…" } else { "Move to Recycle Bin…" };
                let button = ui.add_enabled(idle && !none_selected, egui::Button::new(label));
                if none_selected {
                    button.clone().on_disabled_hover_text("Select at least one item first.");
                }
                if button.clicked() {
                    self.confirm_delete = true;
                }
                // Items are added right to left, so Move comes first to sit on the right.
                ui.add_enabled_ui(idle, |ui| {
                    ui.selectable_value(&mut self.dry_run, false, "Move for real")
                        .on_hover_text("Items go to the Recycle Bin, where you can restore them.");
                    ui.selectable_value(&mut self.dry_run, true, "Preview only")
                        .on_hover_text("Runs every check and reports what would happen. Nothing is moved.");
                });
            });
        });
        ui.add_space(6.0);
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        if self.phase != Phase::Idle {
            ui.vertical_centered(|ui| {
                ui.add_space(120.0);
                ui.spinner();
                ui.add_space(8.0);
                ui.label(&self.status);
                if self.phase == Phase::Scanning {
                    ui.label(RichText::new(&self.scan_root).weak());
                    ui.label(format!("{} files checked so far", self.files_seen));
                    if !self.scan_folder.is_empty() {
                        ui.add(egui::Label::new(RichText::new(&self.scan_folder).weak().small()).truncate());
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new("Large drives can take several minutes. You can cancel at any time.").weak());
                } else {
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
                }
            });
            return;
        }
        let Some(results) = &self.results else {
            let mut places: Vec<(String, String, String, Option<(u64, u64)>)> = Vec::new();
            if let Some(home) = std::env::var_os("USERPROFILE").filter(|h| PathBuf::from(h).is_dir()) {
                let home = PathBuf::from(home).display().to_string();
                places.push(("Your user folder".into(), "A good place to start".into(), home, None));
            }
            for (root, total, free) in &self.drive_space {
                places.push((root.trim_end_matches('\\').into(), "Whole drive".into(), root.clone(), Some((total.saturating_sub(*free), *total))));
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
                        for (title, caption, root, space) in &places {
                            let frame = egui::Frame::group(ui.style()).inner_margin(14.0).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
                                ui.set_width(card_width - 30.0);
                                ui.set_min_height(86.0);
                                ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.label(RichText::new(title).size(22.0));
                                ui.label(RichText::new(caption).weak());
                                if let Some((used, total)) = space {
                                    ui.add_space(6.0);
                                    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
                                    let colors = &ui.visuals().widgets;
                                    ui.painter().rect_filled(bar, 2.0, colors.noninteractive.bg_stroke.color);
                                    let width = bar.width() * *used as f32 / (*total).max(1) as f32;
                                    let fill = egui::Rect::from_min_size(bar.min, egui::vec2(width, bar.height()));
                                    ui.painter().rect_filled(fill, 2.0, colors.noninteractive.fg_stroke.color.gamma_multiply(0.6));
                                    ui.label(RichText::new(format!("{} used of {}", human(*used), human(*total))).weak());
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
                    ui.horizontal_wrapped(|ui| {
                        let dark = ui.visuals().dark_mode;
                        for (level, text) in [
                            (Confidence::Safe, "regenerates itself"),
                            (Confidence::Review, "check it first"),
                            (Confidence::Info, "uninstall only"),
                        ] {
                            let (name, color, _) = confidence_tag(level, dark);
                            level_glyph(ui, level, color);
                            ui.label(name);
                            ui.label(RichText::new(text).weak());
                            ui.add_space(8.0);
                        }
                    });
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
            return;
        };

        let (removable_count, removable_size) = removable(&results.report);
        if removable_count == 0 {
            ui.label(format!("Scanned {} files.", results.files));
            ui.add_space(8.0);
            ui.label("Nothing to clean up here. Try another folder or a whole drive.");
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new(human(removable_size)).size(34.0).strong());
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("could be freed").strong());
                    ui.label(
                        RichText::new(format!(
                            "{} found in {} scanned files.{}",
                            items(removable_count),
                            results.files,
                            if results.unreadable > 0 { format!(" {} couldn't be read.", paths(results.unreadable)) } else { String::new() },
                        ))
                        .weak(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} selected. Safe items start selected for you; nothing moves until you confirm.",
                            items(self.selected.len())
                        ))
                        .weak(),
                    );
                });
            });
            if let Some(tree) = &results.tree {
                let by = |level| results.report.findings.iter().filter(|f| f.confidence == level).map(|f| f.size).sum::<u64>();
                let elapsed = ui.input(|i| i.time) - self.reveal_start;
                let progress = (elapsed / REVEAL_SECS).clamp(0.0, 1.0) as f32;
                if progress < 1.0 {
                    ui.ctx().request_repaint();
                }
                let eased = 1.0 - (1.0 - progress).powi(3);
                reclaim_strip(ui, by(Confidence::Safe), by(Confidence::Review), tree.node(FolderTree::ROOT).size, eased);
            }
            egui::CollapsingHeader::new("What do Safe, Review and Info mean?").show(ui, legend);
        }
        ui.separator();

        // Wide windows show the map beside the list; narrow ones switch between them.
        let split = results.tree.is_some() && ui.available_width() >= SPLIT_MIN_WIDTH;
        let mut map_action = None;
        if split {
            if let Some(tree) = &results.tree {
                let dark = ui.visuals().dark_mode;
                let width = ui.available_width() * 0.55;
                egui::Panel::left("map_panel").resizable(true).default_size(width).size_range(320.0..=width * 1.4).show(ui, |ui| {
                    map_action = map_view(ui, tree, &mut self.map_folder, dark, &results.report, &self.selected);
                });
            }
        } else {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::List, "List");
                let map_ready = results.tree.is_some();
                ui.add_enabled_ui(map_ready, |ui| {
                    ui.selectable_value(&mut self.view, View::Map, "Map");
                })
                .response
                .on_disabled_hover_text("The map is out of date after a move. Scan again to refresh it.");
            });
            if let (View::Map, Some(tree)) = (self.view, &results.tree) {
                let dark = ui.visuals().dark_mode;
                if let Some(action) = map_view(ui, tree, &mut self.map_folder, dark, &results.report, &self.selected) {
                    self.apply_map_action(action);
                }
                return;
            }
        }

        let mut filter_changed = false;
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let field = ui
                .add(egui::TextEdit::singleline(&mut self.filter).desired_width(240.0).hint_text("name or path"))
                .on_hover_text("Ctrl+A selects everything shown, except installed programs.");
            filter_changed = field.changed();
            if !self.filter.trim().is_empty() {
                let shown = self.visible.iter().filter(|v| **v).count();
                ui.label(RichText::new(format!("{shown} of {} match", self.visible.len())).weak());
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

        let row_height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + ui.spacing().item_spacing.y * 2.0;
        let row_pitch = row_height + ui.spacing().item_spacing.y;
        let mut key_action: Option<usize> = None;
        let mut key_enter = false;
        if !self.modal_open() {
            if let Some(id) = ui.memory(|m| m.focused()) {
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    ui.memory_mut(|m| m.surrender_focus(id));
                }
            } else if !ui.ctx().egui_wants_keyboard_input() && !self.rows.is_empty() {
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
                    key_action = current;
                    key_enter = enter;
                }
            }
        }
        let cursor = self.cursor_row;
        let dark = ui.visuals().dark_mode;
        let mut toggled = None;
        let mut category_select: Option<(Category, bool)> = None;
        let mut uninstall = None;
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
                        let (chosen, selectable) = results
                            .report
                            .findings
                            .iter()
                            .enumerate()
                            .filter(|(i, f)| f.category == *category && f.confidence != Confidence::Info && self.visible[*i])
                            .fold((0, 0), |(c, t), (i, _)| (c + usize::from(self.selected.contains(&i)), t + 1));
                        ui.horizontal(|ui| {
                            ui.set_min_height(row_height);
                            if selectable > 0 {
                                let mut on = chosen == selectable;
                                let partial = chosen > 0 && chosen < selectable;
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
                            if selectable > 0 {
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
                            let title = ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
                            if !parent.is_empty() {
                                ui.add(egui::Label::new(RichText::new(parent).weak()).truncate()).on_hover_text(&full_path);
                                title.on_hover_text(&full_path);
                            }
                            if finding.confidence == Confidence::Info && finding.uninstall.is_some() && ui.button("Uninstall…").clicked() {
                                uninstall = Some(*i);
                            }
                        });
                        ui.horizontal(|ui| {
                            // Lines up under the name column of the row above.
                            ui.add_space(ITEM_INDENT + CHECK_W + TAG_W + SIZE_W + ui.spacing().item_spacing.x * 3.0);
                            ui.add(egui::Label::new(RichText::new(&finding.reason).weak()).truncate()).on_hover_text(&finding.reason);
                        });
                    }
                }
                if cursor == Some(pos) {
                    let rect = egui::Rect::from_min_max(egui::pos2(ui.max_rect().left(), row_top), egui::pos2(ui.max_rect().right(), ui.cursor().min.y));
                    ui.painter().set(highlight, egui::Shape::rect_filled(rect, 3.0, ui.visuals().selection.bg_fill.gamma_multiply(0.45)));
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
                        let (chosen, selectable) = results
                            .report
                            .findings
                            .iter()
                            .enumerate()
                            .filter(|(i, f)| f.category == *category && f.confidence != Confidence::Info && self.visible[*i])
                            .fold((0, 0), |(c, t), (i, _)| (c + usize::from(self.selected.contains(&i)), t + 1));
                        if selectable > 0 {
                            category_select = Some((*category, chosen < selectable));
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some((category, on)), Some(results)) = (category_select, &self.results) {
            for (i, _) in results.report.findings.iter().enumerate().filter(|(i, f)| f.category == category && f.confidence != Confidence::Info && self.visible[*i]) {
                if on {
                    self.selected.insert(i);
                } else {
                    self.selected.remove(&i);
                }
            }
        }
        if select_visible {
            for (i, _) in results.report.findings.iter().enumerate().filter(|(i, f)| f.confidence != Confidence::Info && self.visible[*i]) {
                self.selected.insert(i);
            }
        }
        if let Some(action) = map_action {
            self.apply_map_action(action);
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

    fn apply_map_action(&mut self, action: MapAction) {
        match action {
            MapAction::ShowInList(path) => {
                self.filter = path;
                self.view = View::List;
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

        if let Some((report, dry_run)) = &self.last_delete {
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

        if let Some(i) = self.pending_uninstall {
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

        if let Some(message) = &self.error {
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
}

impl eframe::App for DeoxApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let modal = self.confirm_delete || self.last_delete.is_some() || self.pending_uninstall.is_some() || self.error.is_some();
        ui.add_enabled_ui(!modal, |ui| {
            egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
            egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui));
            let frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(20, 14));
            egui::CentralPanel::default().frame(frame).show(ui, |ui| self.central(ui));
        });
        self.dialogs(&ctx);
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn category_name(category: Category) -> &'static str {
    match category {
        Category::Temp => "Temporary files",
        Category::AppCache => "Application caches",
        Category::DevArtifact => "Developer artifacts",
        Category::Duplicate => "Duplicate files",
        Category::OldLarge => "Old large files",
        Category::Bloatware => "Installed programs worth a look",
    }
}

/// Label, color and meaning of a safety level. Colors keep at least 4.5:1 against the panel in both themes.
fn confidence_tag(confidence: Confidence, dark: bool) -> (&'static str, Color32, &'static str) {
    let pick = |dark_rgb: [u8; 3], light_rgb: [u8; 3]| {
        let [r, g, b] = if dark { dark_rgb } else { light_rgb };
        Color32::from_rgb(r, g, b)
    };
    match confidence {
        Confidence::Safe => ("Safe", pick([232, 96, 47], [178, 63, 18]), "Regenerated automatically. Selected by default."),
        Confidence::Review => (
            "Review",
            pick([229, 160, 66], [140, 92, 0]),
            "Probably not needed, but check it first. Never selected by default.",
        ),
        Confidence::Info => (
            "Info",
            pick([158, 166, 178], [84, 91, 103]),
            "An installed program. Deox only reports it; removal goes through the program's own uninstaller.",
        ),
    }
}

/// Gives a checkbox without visible text a name for screen readers.
fn name_checkbox(check: &egui::Response, on: bool, name: &str) {
    check.widget_info(|| egui::WidgetInfo::selected(egui::accesskit::Role::CheckBox, true, on, name));
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

/// Seconds the bar below the headline takes to fill after a scan finishes.
const REVEAL_SECS: f64 = 0.9;

/// The scanned size as one bar; the Safe and Review parts fill in from the left as `reveal` goes from 0 to 1.
fn reclaim_strip(ui: &mut egui::Ui, safe: u64, review: u64, total: u64, reveal: f32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let width = |bytes: u64| if bytes == 0 { 0.0 } else { (rect.width() * bytes as f32 / total.max(1) as f32).max(3.0) * reveal };
    let safe_w = width(safe).min(rect.width());
    let review_w = width(review).min(rect.width() - safe_w);
    let part = |from: f32, to: f32| egui::Rect::from_min_max(egui::pos2(rect.left() + from, rect.top()), egui::pos2(rect.left() + to, rect.bottom()));
    painter.rect_filled(part(0.0, safe_w), 2.0, confidence_tag(Confidence::Safe, dark).1);
    painter.rect_filled(part(safe_w, safe_w + review_w), 2.0, confidence_tag(Confidence::Review, dark).1);
    response.on_hover_text(format!("Of {} scanned: {} Safe, {} Review", human(total), human(safe), human(review)));
}

/// A thin bar showing a group's share of everything that could be freed.
fn share_bar(ui: &mut egui::Ui, part: u64, whole: u64) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(120.0, 6.0), egui::Sense::hover());
    let share = (part as f32 / whole as f32).clamp(0.0, 1.0);
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height()));
    painter.rect_filled(fill, 3.0, confidence_tag(Confidence::Safe, ui.visuals().dark_mode).1);
    response.on_hover_text(format!("{:.0}% of what could be freed", share * 100.0));
}

/// Rows shown in the list of skipped items; longer lists are summarized.
const MAX_LISTED_SKIPS: usize = 200;
const CHECK_W: f32 = 24.0;
const TAG_W: f32 = 72.0;
/// Width of the centered column on the start screen.
const START_COLUMN: f32 = 600.0;
/// Content width from which the map and the list are shown side by side.
const SPLIT_MIN_WIDTH: f32 = 860.0;

/// Marks a safety level by shape as well as color: solid for Safe, hatched for Review, outlined for Info.
fn level_glyph(ui: &mut egui::Ui, level: Confidence, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let painter = ui.painter().with_clip_rect(rect);
    match level {
        Confidence::Safe => {
            painter.rect_filled(rect, 2.0, color);
        }
        Confidence::Review => {
            for k in 0..5 {
                let x = rect.left() + k as f32 * 4.0 - 4.0;
                painter.line_segment([egui::pos2(x, rect.bottom()), egui::pos2(x + 12.0, rect.top())], egui::Stroke::new(1.5, color));
            }
            painter.rect_stroke(rect.shrink(0.75), 2.0, egui::Stroke::new(1.5, color), egui::StrokeKind::Inside);
        }
        Confidence::Info => {
            painter.rect_stroke(rect.shrink(0.75), 2.0, egui::Stroke::new(1.5, color), egui::StrokeKind::Inside);
        }
    }
}
const SIZE_W: f32 = 76.0;
const ITEM_INDENT: f32 = 18.0;

/// A fixed-width table cell, so tags and sizes line up down the list.
fn cell(ui: &mut egui::Ui, width: f32, layout: egui::Layout, add: impl FnOnce(&mut egui::Ui)) {
    let size = egui::vec2(width, ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, layout, |ui| {
        ui.set_width(width);
        add(ui);
    });
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

fn paths(n: usize) -> String {
    if n == 1 { "1 path".into() } else { format!("{n} paths") }
}


/// Most tiles drawn for one folder; the rest are merged into one tile.
const MAP_TILES: usize = 300;

/// Folders below this share of their parent are merged into one "smaller folders" tile.
const MAP_MIN_SHARE: u64 = 100;

/// What the user asked for in the map besides browsing it.
enum MapAction {
    /// Show the items under this folder in the list.
    ShowInList(String),
    /// Select every Safe item under this folder.
    SelectSafe(PathBuf),
    /// Clear the selection under this folder.
    ClearHere(PathBuf),
}

/// Draws the folder at `folder` as a treemap. Clicking a tile opens that folder.
fn map_view(
    ui: &mut egui::Ui,
    tree: &FolderTree,
    folder: &mut usize,
    dark: bool,
    report: &Report,
    selected: &HashSet<usize>,
) -> Option<MapAction> {
    let mut action = None;
    let folder_path = tree.path(*folder);
    ui.horizontal_wrapped(|ui| {
        for (n, id) in tree.trail(*folder).into_iter().enumerate() {
            if n > 0 {
                ui.label("›");
            }
            let name = if n == 0 { tree.path(id).display().to_string() } else { tree.node(id).name.clone() };
            if ui.selectable_label(id == *folder, name).clicked() {
                *folder = id;
            }
        }
    });
    let node = tree.node(*folder);

    // Selected items under this folder, counted per tile.
    let names: HashMap<&OsStr, usize> = node.children.iter().map(|c| (OsStr::new(tree.node(*c).name.as_str()), *c)).collect();
    let mut chosen: HashMap<usize, (usize, u64)> = HashMap::new();
    let mut chosen_here = 0;
    for finding in selected.iter().filter_map(|i| report.findings.get(*i)) {
        let Ok(relative) = finding.path.strip_prefix(&folder_path) else { continue };
        chosen_here += 1;
        if let Some(child) = relative.components().next().and_then(|c| names.get(c.as_os_str())) {
            let entry = chosen.entry(*child).or_default();
            entry.0 += 1;
            entry.1 += finding.size;
        }
    }

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(human(node.size)).size(22.0));
        ui.label(RichText::new(format!("in this folder, {} Safe and {} Review suggested", human(node.safe), human(node.review))).weak());
    });
    ui.horizontal_wrapped(|ui| {
        if ui.button("Show in list").on_hover_text("Lists the suggested items under this folder.").clicked() {
            action = Some(MapAction::ShowInList(folder_path.display().to_string()));
        }
        if ui.button("Select Safe here").on_hover_text("Selects every Safe item under this folder. Nothing moves until you confirm.").clicked() {
            action = Some(MapAction::SelectSafe(folder_path.clone()));
        }
        if ui.add_enabled(chosen_here > 0, egui::Button::new("Clear here")).on_hover_text("Clears the selection under this folder.").clicked() {
            action = Some(MapAction::ClearHere(folder_path.clone()));
        }
        ui.label(RichText::new(format!("{} selected here", items(chosen_here))).weak());
    });

    let (_, safe_color, _) = confidence_tag(Confidence::Safe, dark);
    let (_, review_color, _) = confidence_tag(Confidence::Review, dark);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("The stripe on a tile shows what Deox suggests inside:").weak());
        for (color, name) in [(safe_color, "Safe"), (review_color, "Review")] {
            let (swatch, _) = ui.allocate_exact_size(egui::vec2(18.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(swatch, 2.0, color);
            ui.label(RichText::new(name).weak());
        }
    });

    // The subfolders holding the most suggested bytes, as shortcuts.
    let mut flagged: Vec<(usize, u64)> =
        node.children.iter().map(|c| (*c, tree.node(*c).safe + tree.node(*c).review)).filter(|(_, bytes)| *bytes > 0).collect();
    flagged.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    if !flagged.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Where to look:").strong());
            for (child, bytes) in flagged.into_iter().take(3) {
                if ui.button(format!("{} · {}", tree.node(child).name, human(bytes))).clicked() {
                    *folder = child;
                }
            }
        });
    }
    ui.add_space(4.0);

    let threshold = node.size / MAP_MIN_SHARE;
    let mut tiles: Vec<(Option<usize>, String, u64)> = Vec::new();
    let (mut merged, mut merged_size) = (0usize, 0u64);
    for (position, child) in node.children.iter().enumerate() {
        let size = tree.node(*child).size;
        if position < MAP_TILES && size >= threshold.max(1) {
            tiles.push((Some(*child), tree.node(*child).name.clone(), size));
        } else {
            merged += 1;
            merged_size += size;
        }
    }
    if merged > 0 && merged_size > 0 {
        tiles.push((None, format!("{merged} smaller folders"), merged_size));
    }
    if node.files_size > 0 {
        tiles.push((None, "Files in this folder".into(), node.files_size));
    }
    tiles.sort_by_key(|t| std::cmp::Reverse(t.2));
    if tiles.is_empty() {
        ui.label("This folder is empty.");
        return action;
    }

    let available = ui.available_size();
    let (area, _) = ui.allocate_exact_size(egui::vec2(available.x, available.y.max(160.0)), egui::Sense::hover());
    let sizes: Vec<u64> = tiles.iter().map(|t| t.2).collect();
    let rects = squarify(&sizes, Rect { x: area.min.x, y: area.min.y, w: area.width(), h: area.height() });
    let font = egui::TextStyle::Body.resolve(ui.style());
    let line = ui.text_style_height(&egui::TextStyle::Body) + 2.0;

    for (index, ((target, name, size), r)) in tiles.iter().zip(&rects).enumerate() {
        let tile = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h)).shrink(1.0);
        if tile.width() < 2.0 || tile.height() < 2.0 {
            continue;
        }
        let opens = target.is_some_and(|c| !tree.node(c).children.is_empty());
        let sense = if opens { egui::Sense::click() } else { egui::Sense::hover() };
        let response = ui.interact(tile, ui.auto_id_with(("tile", index)), sense);
        let widgets = &ui.visuals().widgets;
        let (fill, text_color) = if response.hovered() {
            (widgets.hovered.bg_fill, widgets.hovered.fg_stroke.color)
        } else {
            (widgets.inactive.bg_fill, widgets.inactive.fg_stroke.color)
        };
        let painter = ui.painter().with_clip_rect(tile);
        painter.rect_filled(tile, 2.0, fill);
        let mut detail = String::new();
        let stripe = (tile.height() / 4.0).min(8.0);
        if let Some(c) = target {
            let child = tree.node(*c);
            let width = tile.width();
            let share = |bytes: u64| if bytes == 0 { 0.0 } else { (width * bytes as f32 / child.size.max(1) as f32).max(6.0) };
            let safe_w = share(child.safe).min(width);
            let review_w = share(child.review).min(width - safe_w);
            let bottom = tile.bottom();
            let at = |from: f32, to: f32| egui::Rect::from_min_max(egui::pos2(tile.left() + from, bottom - stripe), egui::pos2(tile.left() + to, bottom));
            painter.rect_filled(at(0.0, safe_w), 0.0, safe_color);
            painter.rect_filled(at(safe_w, safe_w + review_w), 0.0, review_color);
            detail = format!("\n{} Safe, {} Review suggested", human(child.safe), human(child.review));
        }
        if let Some((count, bytes)) = target.and_then(|c| chosen.get(&c)) {
            painter.rect_stroke(tile, 2.0, egui::Stroke::new(2.0, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
            if tile.width() > 90.0 && tile.height() > 24.0 + line * 2.0 + stripe {
                painter.text(
                    tile.left_bottom() + egui::vec2(6.0, -stripe - 4.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{count} selected · {}", human(*bytes)),
                    font.clone(),
                    text_color,
                );
            }
            detail.push_str(&format!("\n{} selected", items(*count)));
        }
        if tile.width() > 56.0 && tile.height() > 24.0 {
            painter.text(tile.left_top() + egui::vec2(6.0, 4.0), egui::Align2::LEFT_TOP, name, font.clone(), text_color);
            if tile.height() > 24.0 + line {
                painter.text(tile.left_top() + egui::vec2(6.0, 4.0 + line), egui::Align2::LEFT_TOP, human(*size), font.clone(), text_color.gamma_multiply(0.8));
            }
        }
        if opens && tile.width() > 40.0 && tile.height() > 24.0 {
            // A small arrow marks tiles that open into subfolders.
            let c = egui::pos2(tile.right() - 12.0, tile.top() + 12.0);
            let points = vec![c + egui::vec2(-3.0, -5.0), c + egui::vec2(-3.0, 5.0), c + egui::vec2(4.0, 0.0)];
            painter.add(egui::Shape::convex_polygon(points, text_color.gamma_multiply(0.7), egui::Stroke::NONE));
        }
        if let Some(c) = target {
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{name}, {}", human(*size))));
            let hint = if opens { "\nClick to open." } else { "" };
            let clicked = response.clicked();
            let response = if opens { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response };
            response.on_hover_text(format!("{}\n{}{detail}{hint}", tree.path(*c).display(), human(*size)));
            if clicked {
                *folder = *c;
            }
        } else {
            response.on_hover_text(format!("{name}: {}", human(*size)));
        }
    }
    action
}

/// A small triangle that points right when the group is collapsed and down when it is open.
fn disclosure_icon(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
    let c = rect.center();
    let points = if open {
        vec![c + egui::vec2(-5.0, -3.0), c + egui::vec2(5.0, -3.0), c + egui::vec2(0.0, 4.0)]
    } else {
        vec![c + egui::vec2(-3.0, -5.0), c + egui::vec2(-3.0, 5.0), c + egui::vec2(4.0, 0.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(points, ui.visuals().text_color(), egui::Stroke::NONE));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Expand or collapse group"));
    response
}

/// The three safety levels with their meaning, as a short list.
fn legend(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    for level in [Confidence::Safe, Confidence::Review, Confidence::Info] {
        let (tag, color, meaning) = confidence_tag(level, dark);
        ui.horizontal(|ui| {
            cell(ui, TAG_W, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                level_glyph(ui, level, color);
                ui.label(RichText::new(tag).strong());
            });
            ui.label(RichText::new(meaning).weak());
        });
    }
}

/// Case-insensitive match on a finding's path or rule name; an empty filter matches everything.
fn matches_filter(finding: &Finding, needle: &str) -> bool {
    needle.is_empty()
        || finding.path.to_string_lossy().to_lowercase().contains(needle)
        || finding.rule.to_lowercase().contains(needle)
}
