#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod worker;

use deox_core::delete::{launch_uninstaller, DeleteReport, Outcome};
use deox_core::report::{Category, Confidence, Finding, Report};
use deox_core::scan::Cancel;
use eframe::egui::{self, Color32, RichText};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use worker::{Msg, Notifier};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]).with_title("Deox"),
        ..Default::default()
    };
    eframe::run_native("Deox", options, Box::new(|cc| Ok(Box::new(DeoxApp::new(cc.egui_ctx.clone())))))
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
}

struct DeoxApp {
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: Arc<Cancel>,
    phase: Phase,
    root: String,
    drives: Vec<String>,
    status: String,
    files_seen: u64,
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

impl DeoxApp {
    fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        let drives = ('A'..='Z').map(|c| format!("{c}:\\")).filter(|d| PathBuf::from(d).exists()).collect();
        DeoxApp {
            ctx,
            tx,
            rx,
            cancel: Arc::new(Cancel::default()),
            phase: Phase::Idle,
            root: String::new(),
            drives,
            status: String::new(),
            files_seen: 0,
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
        self.cancel = Arc::new(Cancel::default());
        self.phase = Phase::Scanning;
        self.files_seen = 0;
        self.status = "Starting scan…".into();
        self.results = None;
        self.rows.clear();
        self.selected.clear();
        worker::spawn_analysis(root, self.cancel.clone(), self.notifier());
    }

    fn set_results(&mut self, findings: Vec<Finding>, files: u64, unreadable: usize) {
        // Only regenerating caches start selected; everything else is the user's call.
        self.selected =
            findings.iter().enumerate().filter(|(_, f)| f.confidence == Confidence::Safe).map(|(i, _)| i).collect();
        self.results = Some(Results { report: Report::new(findings), files, unreadable });
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(results) = &self.results else { return };
        for group in results.report.groups() {
            self.rows.push(Row::Header {
                category: group.category,
                total: group.total_size,
                count: group.items.len(),
            });
            if !self.collapsed.contains(&group.category) {
                self.rows.extend(group.items.iter().map(|(i, _)| Row::Item(*i)));
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Status(s) => self.status = s,
            Msg::Progress(n) => self.files_seen = n,
            Msg::Analysis(a) => {
                self.phase = Phase::Idle;
                self.files_seen = a.files;
                if a.cancelled {
                    self.status = "Scan cancelled.".into();
                } else {
                    self.status = String::new();
                    self.set_results(a.findings, a.files, a.unreadable);
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
                    ui.label(format!("{} files checked so far", self.files_seen));
                }
            });
            return;
        }
        let Some(results) = &self.results else {
            ui.add_space(20.0);
            ui.heading("Find what is taking space");
            ui.label("Choose a folder or a drive and press Scan.");
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
                });
            });
        }
        ui.separator();

        let row_height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + ui.spacing().item_spacing.y * 2.0;
        let dark = ui.visuals().dark_mode;
        let mut toggled = None;
        let mut category_select: Option<(Category, bool)> = None;
        let mut uninstall = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_height, self.rows.len(), |ui, range| {
            for row in &self.rows[range] {
                match row {
                    Row::Header { category, total, count } => {
                        let (chosen, selectable) = results
                            .report
                            .findings
                            .iter()
                            .enumerate()
                            .filter(|(_, f)| f.category == *category && f.confidence != Confidence::Info)
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
                            let arrow = if self.collapsed.contains(category) { "▶" } else { "▼" };
                            let text = format!("{arrow} {}  ·  {}  ·  {}", category_name(*category), items(*count), human(*total));
                            if ui.add(egui::Button::new(RichText::new(text).strong()).frame(false)).clicked() {
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
            }
        });
        if let (Some((category, on)), Some(results)) = (category_select, &self.results) {
            for (i, _) in results.report.findings.iter().enumerate().filter(|(_, f)| f.category == category && f.confidence != Confidence::Info) {
                if on {
                    self.selected.insert(i);
                } else {
                    self.selected.remove(&i);
                }
            }
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
            let findings = self.selected_findings();
            let total: u64 = findings.iter().map(|f| f.size).sum();
            let mut run = false;
            let mut cancel = false;
            let title = if self.dry_run { "Check before moving" } else { "Move to Recycle Bin?" };
            egui::Window::new(title).collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(format!("{} ({}) will be moved to the Recycle Bin. You can restore them from there.", items(findings.len()), human(total)));
                ui.label("Anything that changed since the scan, or sits in a protected location, is skipped.");
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(300.0).show_rows(ui, 18.0, findings.len(), |ui, range| {
                    for f in &findings[range] {
                        ui.add(egui::Label::new(format!("{:>10}  {}", human(f.size), f.path.display())).truncate());
                    }
                });
                ui.add_space(6.0);
                ui.checkbox(&mut self.dry_run, "Check only. Move nothing yet.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let label = if self.dry_run { "Check items".to_string() } else { format!("Move {} to Recycle Bin", items(findings.len())) };
                    run = ui.button(label).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if run {
                self.confirm_delete = false;
                self.phase = Phase::Deleting;
                self.status = if self.dry_run { "Checking the selected items…".into() } else { "Moving the selected items to the Recycle Bin…".into() };
                worker::spawn_delete(findings, self.dry_run, self.notifier());
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
                    ui.set_width(640.0);
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
                    let problems: Vec<_> =
                        report.entries.iter().filter(|e| e.outcome != Outcome::Deleted && e.outcome != Outcome::WouldDelete).collect();
                    if !problems.is_empty() {
                        ui.add_space(6.0);
                        ui.label(format!("{} skipped:", items(problems.len())));
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            for e in problems {
                                ui.add(egui::Label::new(format!("{}  {}", describe(&e.outcome), e.path.display())).truncate());
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
                self.phase = Phase::Deleting;
                self.status = "Moving the selected items to the Recycle Bin…".into();
                worker::spawn_delete(self.selected_findings(), false, self.notifier());
            } else if close || escape {
                self.last_delete = None;
            }
        }

        if let Some(i) = self.pending_uninstall {
            let finding = self.results.as_ref().and_then(|r| r.report.findings.get(i)).cloned();
            let mut done = finding.is_none();
            if let Some(finding) = finding {
                egui::Window::new("Open uninstaller").collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                    ui.set_width(520.0);
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
                ui.set_width(480.0);
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
        Outcome::Failed(why) => format!("Failed: {why}"),
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
