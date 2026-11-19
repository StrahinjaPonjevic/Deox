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
        let mut root = self.root.trim().to_string();
        if root.ends_with(':') {
            root.push('\\');
        }
        let root = PathBuf::from(root);
        if !root.is_dir() {
            self.error = Some(format!("\"{}\" is not a folder or drive that can be scanned.", root.display()));
            return;
        }
        self.cancel = Arc::new(Cancel::default());
        self.phase = Phase::Scanning;
        self.files_seen = 0;
        self.status = "Starting…".into();
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
        ui.horizontal(|ui| {
            ui.label("Folder or drive:");
            ui.add_enabled(
                idle,
                egui::TextEdit::singleline(&mut self.root).desired_width(360.0).hint_text(r"C:\Users\you"),
            );
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
                "Selected {} of {} ({} of {})",
                self.selected.len(),
                results.report.findings.len(),
                human(selected_size),
                human(results.report.total_size()),
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(idle && !self.selected.is_empty(), egui::Button::new("Review and move to Recycle Bin…")).clicked() {
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
                    ui.label(format!("{} files indexed", self.files_seen));
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

        ui.label(format!(
            "{} files scanned, {} reclaimable items found{}.",
            results.files,
            results.report.findings.len(),
            if results.unreadable > 0 { format!(", {} paths could not be read", results.unreadable) } else { String::new() },
        ));
        ui.separator();

        let row_height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + ui.spacing().item_spacing.y * 2.0;
        let mut toggled = None;
        let mut uninstall = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_height, self.rows.len(), |ui, range| {
            for row in &self.rows[range] {
                match row {
                    Row::Header { category, total, count } => {
                        let arrow = if self.collapsed.contains(category) { "▶" } else { "▼" };
                        let text = format!("{arrow} {}  ·  {count} items  ·  {}", category_name(*category), human(*total));
                        if ui.add(egui::Button::new(RichText::new(text).strong()).frame(false)).clicked() {
                            toggled = Some(*category);
                        }
                    }
                    Row::Item(i) => {
                        let finding = &results.report.findings[*i];
                        ui.horizontal(|ui| {
                            ui.add_space(18.0);
                            if finding.confidence == Confidence::Info {
                                ui.add_space(24.0);
                            } else {
                                let mut on = self.selected.contains(i);
                                if ui.checkbox(&mut on, "").changed() {
                                    if on {
                                        self.selected.insert(*i);
                                    } else {
                                        self.selected.remove(i);
                                    }
                                }
                            }
                            let (tag, color) = confidence_tag(finding.confidence);
                            ui.label(RichText::new(tag).color(color).strong());
                            ui.label(RichText::new(human(finding.size)).monospace());
                            let title = if finding.path.as_os_str().is_empty() {
                                finding.rule.clone()
                            } else {
                                finding.path.display().to_string()
                            };
                            ui.add(egui::Label::new(title).truncate());
                            if finding.confidence == Confidence::Info && finding.uninstall.is_some() && ui.button("Uninstall…").clicked() {
                                uninstall = Some(*i);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(60.0);
                            ui.add(egui::Label::new(RichText::new(&finding.reason).weak()).truncate());
                        });
                    }
                }
            }
        });
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
        if self.confirm_delete {
            let findings = self.selected_findings();
            let total: u64 = findings.iter().map(|f| f.size).sum();
            let mut run = false;
            let mut cancel = false;
            egui::Window::new("Confirm").collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(format!("{} items, {} in total, will be moved to the Recycle Bin and can be restored from there.", findings.len(), human(total)));
                ui.label("Items that changed since the scan, or that are protected, are skipped.");
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(300.0).show_rows(ui, 18.0, findings.len(), |ui, range| {
                    for f in &findings[range] {
                        ui.add(egui::Label::new(format!("{:>10}  {}", human(f.size), f.path.display())).truncate());
                    }
                });
                ui.add_space(6.0);
                ui.checkbox(&mut self.dry_run, "Preview only: check everything but move nothing");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let label = if self.dry_run { "Run preview" } else { "Move to Recycle Bin" };
                    run = ui.button(label).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if run {
                self.confirm_delete = false;
                self.phase = Phase::Deleting;
                self.status = if self.dry_run { "Checking selected items…".into() } else { "Moving items to the Recycle Bin…".into() };
                worker::spawn_delete(findings, self.dry_run, self.notifier());
            } else if cancel {
                self.confirm_delete = false;
            }
        }

        if let Some((report, dry_run)) = &self.last_delete {
            let mut close = false;
            egui::Window::new(if *dry_run { "Preview result" } else { "Result" })
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(640.0);
                    let count = |o: fn(&Outcome) -> bool| report.entries.iter().filter(|e| o(&e.outcome)).count();
                    if *dry_run {
                        ui.label(format!("{} items would be moved. Nothing was changed.", count(|o| *o == Outcome::WouldDelete)));
                    } else {
                        ui.label(format!("{} items moved to the Recycle Bin, {} freed.", count(|o| *o == Outcome::Deleted), human(report.freed_bytes())));
                    }
                    let problems: Vec<_> =
                        report.entries.iter().filter(|e| e.outcome != Outcome::Deleted && e.outcome != Outcome::WouldDelete).collect();
                    if !problems.is_empty() {
                        ui.add_space(6.0);
                        ui.label(format!("{} items were skipped:", problems.len()));
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            for e in problems {
                                ui.add(egui::Label::new(format!("{}  {}", describe(&e.outcome), e.path.display())).truncate());
                            }
                        });
                    }
                    ui.add_space(6.0);
                    close = ui.button("Close").clicked();
                });
            if close {
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
                    ui.label("Deox will only start the program's own uninstaller; it removes nothing itself.");
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
            if done {
                self.pending_uninstall = None;
            }
        }

        if let Some(message) = &self.error {
            let mut close = false;
            egui::Window::new("Something went wrong").collapsible(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_width(480.0);
                ui.label(message);
                ui.add_space(6.0);
                close = ui.button("OK").clicked();
            });
            if close {
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

fn confidence_tag(confidence: Confidence) -> (&'static str, Color32) {
    match confidence {
        Confidence::Safe => ("Safe", Color32::from_rgb(46, 160, 67)),
        Confidence::Review => ("Review", Color32::from_rgb(210, 153, 34)),
        Confidence::Info => ("Info", Color32::from_rgb(110, 118, 129)),
    }
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Deleted | Outcome::WouldDelete => "ok".into(),
        Outcome::Protected => "protected".into(),
        Outcome::NotDeletable => "not removable".into(),
        Outcome::Missing => "already gone".into(),
        Outcome::Changed => "changed since scan".into(),
        Outcome::Failed(why) => format!("failed: {why}"),
    }
}
