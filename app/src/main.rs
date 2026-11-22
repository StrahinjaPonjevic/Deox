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
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals.widgets.noninteractive.fg_stroke.color = Color32::from_gray(180);
        style.visuals.weak_text_alpha = 0.7;
    });
    ctx.style_mut_of(egui::Theme::Light, |style| {
        style.visuals.widgets.noninteractive.fg_stroke.color = Color32::from_gray(60);
        style.visuals.weak_text_alpha = 0.8;
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
        DeoxApp {
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
            ui.label(format!(
                "{} of {} selected · {} of {}",
                self.selected.len(),
                removable_count,
                human(selected_size),
                human(removable_size),
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let none_selected = self.selected.is_empty();
                let button = ui.add_enabled(idle && !none_selected, egui::Button::new("Move to Recycle Bin…"));
                if none_selected {
                    button.clone().on_disabled_hover_text("Select at least one item first.");
                }
                if button.clicked() {
                    self.confirm_delete = true;
                }
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
                        ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(360.0).text(format!("{done} of {total}")));
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
            ui.add_space(20.0);
            ui.heading("Find what is taking space");
            ui.label("Pick a folder or a drive above, then press Scan. Your user folder is a good place to start.");
            if let Some(home) = std::env::var_os("USERPROFILE").filter(|h| PathBuf::from(h).is_dir()) {
                ui.add_space(8.0);
                if ui.button("Scan my user folder").clicked() {
                    self.root = PathBuf::from(home).display().to_string();
                    self.start_scan();
                    return;
                }
            }
            ui.add_space(16.0);
            ui.label(RichText::new("Each suggestion carries a safety level").strong());
            legend(ui);
            ui.add_space(16.0);
            ui.label("Deox only suggests. Nothing is removed without your confirmation, and removed items go to the Recycle Bin.");
            if !self.status.is_empty() {
                ui.add_space(8.0);
                ui.label(&self.status);
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
            egui::CollapsingHeader::new("What do Safe, Review and Info mean?").show(ui, legend);
        }
        ui.separator();

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
                match action {
                    MapAction::ShowInList(path) => {
                        self.filter = path;
                        self.view = View::List;
                        self.rebuild_rows();
                    }
                    MapAction::SelectSafe(path) => {
                        for (i, f) in results.report.findings.iter().enumerate() {
                            if f.confidence == Confidence::Safe && f.path.starts_with(&path) {
                                self.selected.insert(i);
                            }
                        }
                    }
                    MapAction::ClearHere(path) => {
                        self.selected.retain(|i| !results.report.findings.get(*i).is_some_and(|f| f.path.starts_with(&path)));
                    }
                }
            }
            return;
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
                            let text = format!("{}  ·  {}  ·  {}", category_name(*category), items(*count), human(*total));
                            let icon = disclosure_icon(ui, open);
                            let label = ui.add(egui::Button::new(RichText::new(text).strong()).frame(false));
                            if icon.clicked() || label.clicked() {
                                toggled = Some(*category);
                            }
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
                                ui.label(RichText::new(tag).color(color).strong()).on_hover_text(meaning);
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
                ui.checkbox(&mut self.dry_run, "Check only. Move nothing yet.");
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
                            for e in problems.iter().take(MAX_LISTED_SKIPS) {
                                let reason = ui.add(egui::Label::new(RichText::new(describe(&e.outcome)).strong()));
                                if let Outcome::Failed(why) = &e.outcome {
                                    reason.on_hover_text(why);
                                }
                                ui.add(egui::Label::new(RichText::new(e.path.display().to_string()).weak()).wrap());
                                ui.add_space(4.0);
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
            egui::CentralPanel::default().show(ui, |ui| self.central(ui));
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
        Confidence::Safe => ("Safe", pick([92, 205, 128], [17, 107, 43]), "Regenerated automatically. Selected by default."),
        Confidence::Review => (
            "Review",
            pick([232, 183, 70], [133, 84, 0]),
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
        Outcome::Failed(_) => "Windows refused to move it (hover for the technical detail)".into(),
    }
}

/// Count and size of the findings Deox can actually move; installed programs are only reported.
fn removable(report: &Report) -> (usize, u64) {
    report.findings.iter().filter(|f| f.confidence != Confidence::Info).fold((0, 0), |(n, s), f| (n + 1, s + f.size))
}

/// A thin bar showing a group's share of everything that could be freed.
fn share_bar(ui: &mut egui::Ui, part: u64, whole: u64) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(120.0, 6.0), egui::Sense::hover());
    let share = (part as f32 / whole as f32).clamp(0.0, 1.0);
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height()));
    painter.rect_filled(fill, 3.0, ui.visuals().selection.bg_fill);
    response.on_hover_text(format!("{:.0}% of what could be freed", share * 100.0));
}

/// Rows shown in the list of skipped items; longer lists are summarized.
const MAX_LISTED_SKIPS: usize = 200;
const CHECK_W: f32 = 24.0;
const TAG_W: f32 = 52.0;
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

    ui.horizontal(|ui| {
        ui.label(RichText::new(human(node.size)).strong());
        ui.label(RichText::new(format!("in this folder · {} Safe · {} Review suggested", human(node.safe), human(node.review))).weak());
        if ui.button("Show these items in the list").clicked() {
            action = Some(MapAction::ShowInList(folder_path.display().to_string()));
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Select Safe items here").on_hover_text("Selects every Safe item under this folder. Nothing moves until you confirm.").clicked() {
            action = Some(MapAction::SelectSafe(folder_path.clone()));
        }
        if ui.add_enabled(chosen_here > 0, egui::Button::new("Clear selection here")).clicked() {
            action = Some(MapAction::ClearHere(folder_path.clone()));
        }
        ui.label(RichText::new(format!("{} selected under this folder", items(chosen_here))).weak());
    });

    let (_, safe_color, _) = confidence_tag(Confidence::Safe, dark);
    let (_, review_color, _) = confidence_tag(Confidence::Review, dark);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Tile size is its share of this folder.").weak());
        for (color, name) in [(safe_color, "Safe"), (review_color, "Review")] {
            let (swatch, _) = ui.allocate_exact_size(egui::vec2(18.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(swatch, 2.0, color);
            ui.label(RichText::new(name).weak());
        }
        ui.label(RichText::new("stripe: what Deox suggests inside.").weak());
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
                ui.label(RichText::new(tag).color(color).strong());
            });
            ui.label(meaning);
        });
    }
}

/// Case-insensitive match on a finding's path or rule name; an empty filter matches everything.
fn matches_filter(finding: &Finding, needle: &str) -> bool {
    needle.is_empty()
        || finding.path.to_string_lossy().to_lowercase().contains(needle)
        || finding.rule.to_lowercase().contains(needle)
}
