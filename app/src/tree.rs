//! The directory tree of the explorer: every scanned folder and, inside an opened folder, every
//! file, largest first.

use crate::map::Pick;
use crate::ui::{confidence_tag, human, paint_disclosure, paint_level};
use deox_core::report::Confidence;
use deox_core::scan::Index;
use deox_core::treemap::FolderTree;
use eframe::egui::{self, Align2};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// Widths of the columns right of the name.
const SIZE_COLUMN: f32 = 90.0;
const SHARE_COLUMN: f32 = 120.0;
const FILES_COLUMN: f32 = 76.0;
const CHANGED_COLUMN: f32 = 92.0;
/// Files and Changed are left out of a tree narrower than this, so names stay readable.
const ALL_COLUMNS_FROM: f32 = 560.0;
/// Horizontal offset of each level below the root.
const INDENT: f32 = 16.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Entry {
    Folder(usize),
    /// A position in `Index::files`.
    File(usize),
}

/// One visible row.
pub(crate) struct Line {
    pub(crate) entry: Entry,
    depth: usize,
    /// Set when a finding covers the entry, directly or through a folder above it.
    flag: Option<Confidence>,
}

pub(crate) struct TreeState {
    expanded: HashSet<usize>,
    /// The visible rows, rebuilt when `stale`.
    pub(crate) lines: Vec<Line>,
    stale: bool,
    pub(crate) selected: Entry,
    /// Bring the selected row into view on the next frame.
    scroll_to_selected: bool,
    /// Scroll offset and viewport height of the last frame.
    scroll: (f32, f32),
    /// The folder and pick of the map that the selection answers to.
    followed: Option<(usize, Option<Pick>)>,
}

impl Default for TreeState {
    fn default() -> Self {
        TreeState {
            expanded: HashSet::from([FolderTree::ROOT]),
            lines: Vec::new(),
            stale: true,
            selected: Entry::Folder(FolderTree::ROOT),
            scroll_to_selected: false,
            scroll: (0.0, 0.0),
            followed: None,
        }
    }
}

impl TreeState {
    /// File positions change along with the tree. Opened folders keep theirs and stay open.
    pub(crate) fn tree_rebuilt(&mut self) {
        self.stale = true;
        self.selected = Entry::Folder(FolderTree::ROOT);
        self.followed = None;
    }

    /// Selects what the map shows, when the map has moved since the last call: its picked
    /// rectangle, or else its folder.
    pub(crate) fn follow(&mut self, shown: (usize, Option<Pick>), index: &Index, tree: &FolderTree) {
        if self.followed == Some(shown) {
            return;
        }
        self.followed = Some(shown);
        let entry = match shown.1 {
            Some(Pick::Folder(id)) => Entry::Folder(id),
            Some(Pick::File { folder, index: listed }) => {
                let name = tree.node(folder).files.get(listed).map(|leaf| leaf.name.as_str());
                let position = index.files.iter().position(|f| {
                    f.dir as usize == folder && name.is_some_and(|name| f.name.to_string_lossy() == name)
                });
                position.map_or(Entry::Folder(folder), Entry::File)
            }
            Some(Pick::Rest(folder)) => Entry::Folder(folder),
            None => Entry::Folder(shown.0),
        };
        self.reveal(entry, index, tree);
    }

    /// The map was moved from here, so there is nothing to follow.
    pub(crate) fn moved_map(&mut self, shown: (usize, Option<Pick>)) {
        self.followed = Some(shown);
    }

    /// Asks for another frame when the map has moved after [`TreeState::follow`] ran.
    pub(crate) fn repaint_if_behind(&self, shown: (usize, Option<Pick>), ctx: &egui::Context) {
        if self.followed != Some(shown) {
            ctx.request_repaint();
        }
    }

    /// Selects `entry`, opens the folders above it and scrolls to it.
    fn reveal(&mut self, entry: Entry, index: &Index, tree: &FolderTree) {
        let mut above = match entry {
            Entry::Folder(id) => tree.node(id).parent,
            Entry::File(position) => index.files.get(position).map(|f| f.dir as usize),
        };
        while let Some(id) = above {
            self.stale |= self.expanded.insert(id);
            above = tree.node(id).parent;
        }
        self.selected = entry;
        self.scroll_to_selected = true;
    }

    fn toggle(&mut self, folder: usize) {
        if !self.expanded.remove(&folder) {
            self.expanded.insert(folder);
        }
        self.stale = true;
    }

    fn rebuild(&mut self, index: &Index, tree: &FolderTree) {
        // One pass over the files serves every opened folder.
        let mut files: HashMap<usize, Vec<usize>> = HashMap::new();
        for (position, file) in index.files.iter().enumerate() {
            let folder = file.dir as usize;
            if self.expanded.contains(&folder) {
                files.entry(folder).or_default().push(position);
            }
        }
        self.lines.clear();
        let root = FolderTree::ROOT;
        let mut pending = vec![Line { entry: Entry::Folder(root), depth: 0, flag: tree.node(root).flag }];
        while let Some(line) = pending.pop() {
            if let Entry::Folder(id) = line.entry {
                if self.expanded.contains(&id) {
                    let depth = line.depth + 1;
                    let folders = tree.node(id).children.iter().map(|&child| {
                        let node = tree.node(child);
                        let flag = node.flag.or(line.flag);
                        (node.size, Line { entry: Entry::Folder(child), depth, flag })
                    });
                    let inside = files.remove(&id).unwrap_or_default().into_iter().map(|position| {
                        let file = &index.files[position];
                        let flag = tree.file_flag(id, &file.name).or(line.flag);
                        (file.size, Line { entry: Entry::File(position), depth, flag })
                    });
                    let mut inside: Vec<(u64, Line)> = folders.chain(inside).collect();
                    // Smallest first, so that the largest is taken off the stack first.
                    inside.sort_by_key(|(size, _)| *size);
                    pending.extend(inside.into_iter().map(|(_, line)| line));
                }
            }
            self.lines.push(line);
        }
        self.stale = false;
    }

    /// Arrow keys, Home and End move the selection; Right and Left open and close folders.
    /// Returns the newly selected entry.
    fn keys(&mut self, ui: &egui::Ui) -> Option<Entry> {
        if !ui.is_enabled() || ui.ctx().egui_wants_keyboard_input() {
            return None;
        }
        if let Some(focused) = ui.memory(|m| m.focused()) {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                ui.memory_mut(|m| m.surrender_focus(focused));
            }
            return None;
        }
        // With a modifier the arrows belong to the map's back and forward.
        let key = ui.input_mut(|i| {
            use egui::Key::{ArrowDown, ArrowLeft, ArrowRight, ArrowUp, End, Home};
            if !i.modifiers.is_none() {
                return None;
            }
            [ArrowDown, ArrowUp, ArrowLeft, ArrowRight, Home, End]
                .into_iter()
                .find(|key| i.consume_key(egui::Modifiers::NONE, *key))
        })?;
        // A folder that was moved away can stay selected without having a row; start from the top.
        let at = self.lines.iter().position(|line| line.entry == self.selected).unwrap_or(0);
        let last = self.lines.len() - 1;
        let target = match (key, self.selected) {
            (egui::Key::ArrowDown, _) => (at + 1).min(last),
            (egui::Key::ArrowUp, _) => at.saturating_sub(1),
            (egui::Key::Home, _) => 0,
            (egui::Key::End, _) => last,
            (egui::Key::ArrowRight, Entry::Folder(id)) if !self.expanded.contains(&id) => {
                self.toggle(id);
                at
            }
            (egui::Key::ArrowRight, _) => (at + 1).min(last),
            (egui::Key::ArrowLeft, Entry::Folder(id)) if self.expanded.contains(&id) => {
                self.toggle(id);
                at
            }
            // Up to the folder that holds the entry: the closest row above with less indent.
            _ => {
                let depth = self.lines[at].depth;
                self.lines[..at].iter().rposition(|line| line.depth < depth).unwrap_or(at)
            }
        };
        let entry = self.lines[target].entry;
        self.scroll_to_selected = true;
        (entry != self.selected).then(|| {
            self.selected = entry;
            entry
        })
    }
}

/// How long ago, in the largest unit that says something.
fn age(changed: Option<SystemTime>, now: SystemTime) -> String {
    let Some(changed) = changed else { return String::new() };
    match now.duration_since(changed).unwrap_or_default().as_secs() / 86_400 {
        0 => "today".into(),
        1 => "1 day".into(),
        days @ 2..=59 => format!("{days} days"),
        days @ 60..=729 => format!("{} months", days / 30),
        days => format!("{} years", days / 365),
    }
}

/// The left edges of the size, share, files and changed columns, and the right edge of the last.
/// In a narrow row the last two have no width.
fn columns(row: egui::Rect) -> [f32; 5] {
    let wide = |width: f32| if row.width() >= ALL_COLUMNS_FROM { width } else { 0.0 };
    let changed = row.right() - wide(CHANGED_COLUMN);
    let files = changed - wide(FILES_COLUMN);
    let share = files - SHARE_COLUMN;
    [share - SIZE_COLUMN, share, files, changed, row.right()]
}

fn header(ui: &mut egui::Ui) {
    let height = ui.text_style_height(&egui::TextStyle::Body);
    // As wide as the rows below, which leave room for the scroll bar.
    let width = ui.available_width() - ui.spacing().scroll.allocated_width();
    let (row, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let [_, share, _, changed, end] = columns(row);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let painter = ui.painter();
    painter.text(row.left_center() + egui::vec2(4.0, 0.0), Align2::LEFT_CENTER, "Name", font.clone(), color);
    let wide = row.width() >= ALL_COLUMNS_FROM;
    for (title, right, shown) in [("Size", share, true), ("Files", changed, wide), ("Changed", end, wide)] {
        if shown {
            painter.text(egui::pos2(right - 8.0, row.center().y), Align2::RIGHT_CENTER, title, font.clone(), color);
        }
    }
    painter.text(egui::pos2(share + 8.0, row.center().y), Align2::LEFT_CENTER, "Share", font, color);
}

/// Draws the tree and returns the entry the user selected in this frame.
pub(crate) fn view(ui: &mut egui::Ui, index: &Index, tree: &FolderTree, state: &mut TreeState) -> Option<Entry> {
    if state.stale {
        state.rebuild(index, tree);
    }
    let mut chosen = state.keys(ui);
    if state.stale {
        state.rebuild(index, tree);
    }
    header(ui);

    let row_height = ui.spacing().interact_size.y;
    let pitch = row_height + ui.spacing().item_spacing.y;
    let mut area = egui::ScrollArea::vertical().id_salt("directory_tree").auto_shrink([false, false]);
    if std::mem::take(&mut state.scroll_to_selected) {
        if let Some(at) = state.lines.iter().position(|line| line.entry == state.selected) {
            let top = at as f32 * pitch;
            let (offset, height) = state.scroll;
            if top < offset || top + pitch > offset + height {
                area = area.vertical_scroll_offset((top - height / 3.0).max(0.0));
            }
        }
    }

    let now = SystemTime::now();
    let total = tree.node(FolderTree::ROOT).size.max(1);
    let dark = ui.visuals().dark_mode;
    let mut toggled = None;
    let output = area.show_rows(ui, row_height, state.lines.len(), |ui, range| {
        for (at, line) in (range.start..).zip(&state.lines[range]) {
            let (name, size, files, newest) = match line.entry {
                Entry::Folder(id) => {
                    let node = tree.node(id);
                    (node.name.clone(), node.size, Some(node.file_count), node.newest)
                }
                Entry::File(position) => {
                    let file = &index.files[position];
                    (file.name.to_string_lossy().into_owned(), file.size, None, file.modified)
                }
            };
            let (row, response) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::click());
            let selected = state.selected == line.entry;
            let level = line.flag.map(|flag| confidence_tag(flag, dark));
            let label = match level {
                Some((tag, ..)) => format!("{name}, {}, {tag}", human(size)),
                None => format!("{name}, {}", human(size)),
            };
            response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, &label));

            let visuals = ui.visuals();
            let tint = if selected {
                Some(visuals.selection.bg_fill.gamma_multiply(0.45))
            } else {
                response.hovered().then(|| visuals.widgets.hovered.bg_fill.gamma_multiply(0.7))
            };
            let painter = ui.painter();
            if let Some(tint) = tint {
                painter.rect_filled(row, 3.0, tint);
            }
            let [size_left, share_left, files_left, changed_left, end] = columns(row);
            let body = egui::TextStyle::Body.resolve(ui.style());
            let mono = egui::TextStyle::Monospace.resolve(ui.style());
            let (text, weak) = (visuals.text_color(), visuals.weak_text_color());
            let middle = row.center().y;

            let mut x = row.left() + line.depth as f32 * INDENT + 8.0;
            let names = painter.with_clip_rect(egui::Rect::from_x_y_ranges(row.left()..=size_left - 8.0, row.y_range()));
            let folder = match line.entry {
                Entry::Folder(id) => Some(id),
                Entry::File(_) => None,
            };
            let openable = folder.filter(|&id| !tree.node(id).children.is_empty() || tree.node(id).file_count > 0);
            let toggle_at = egui::pos2(x, middle);
            if let Some(id) = openable {
                paint_disclosure(&names, toggle_at, state.expanded.contains(&id), text);
            }
            x += 14.0;
            if let (Some(flag), Some((_, color, _))) = (line.flag, level) {
                let square = egui::Rect::from_center_size(egui::pos2(x + 6.0, middle), egui::vec2(12.0, 12.0));
                paint_level(&names, square, flag, color);
                x += 18.0;
            }
            // Folders are told apart from files by the trailing separator as well as by the triangle.
            let shown = if folder.is_some() && line.depth > 0 { format!("{name}\\") } else { name.clone() };
            names.text(egui::pos2(x, middle), Align2::LEFT_CENTER, shown, body.clone(), text);

            painter.text(egui::pos2(share_left - 8.0, middle), Align2::RIGHT_CENTER, human(size), mono, text);
            let share = size as f64 / total as f64;
            let bar = egui::Rect::from_min_size(egui::pos2(share_left + 8.0, middle - 3.0), egui::vec2(56.0, 6.0));
            painter.rect_filled(bar, 3.0, visuals.widgets.noninteractive.bg_stroke.color);
            let filled = egui::vec2(bar.width() * share as f32, bar.height());
            painter.rect_filled(egui::Rect::from_min_size(bar.min, filled), 3.0, visuals.selection.stroke.color);
            let percent = format!("{:.1}%", share * 100.0);
            painter.text(egui::pos2(files_left - 4.0, middle), Align2::RIGHT_CENTER, percent, body.clone(), weak);
            if row.width() >= ALL_COLUMNS_FROM {
                if let Some(files) = files {
                    let at = egui::pos2(changed_left - 8.0, middle);
                    painter.text(at, Align2::RIGHT_CENTER, files.to_string(), body.clone(), weak);
                }
                painter.text(egui::pos2(end - 8.0, middle), Align2::RIGHT_CENTER, age(newest, now), body, weak);
            }

            // Registered after the row, so a click on the triangle is its own.
            if let Some(id) = openable {
                let hit = egui::Rect::from_center_size(toggle_at, egui::vec2(16.0, row_height));
                let toggle = ui.interact(hit, ui.scope_id().with(("tree_toggle", at)), egui::Sense::click());
                let verb = if state.expanded.contains(&id) { "Collapse" } else { "Expand" };
                toggle.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{verb} {name}"))
                });
                if toggle.clicked() {
                    toggled = Some(id);
                }
            }
            if response.double_clicked() {
                toggled = openable;
            }
            if response.clicked() {
                state.selected = line.entry;
                chosen = Some(line.entry);
            }
            response.on_hover_ui(|ui| {
                let path = match line.entry {
                    Entry::Folder(id) => tree.path(id),
                    Entry::File(position) => index.file_path(&index.files[position]),
                };
                ui.label(path.display().to_string());
                if let Some((_, _, meaning)) = level {
                    ui.label(meaning);
                }
            });
        }
    });
    state.scroll = (output.state.clamped_offset().y, output.inner_rect.height());
    if let Some(folder) = toggled {
        state.toggle(folder);
        ui.ctx().request_repaint();
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::age;
    use std::time::{Duration, SystemTime};

    #[test]
    fn age_picks_a_readable_unit() {
        let now = SystemTime::now();
        let ago = |days: u64| age(Some(now - Duration::from_secs(days * 86_400)), now);

        assert_eq!(
            [ago(0), ago(1), ago(59), ago(60), ago(729), ago(730)],
            ["today", "1 day", "59 days", "2 months", "24 months", "2 years"]
        );
        assert_eq!(age(None, now), "");
        assert_eq!(age(Some(now + Duration::from_secs(99)), now), "today");
    }
}
