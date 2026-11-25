use crate::ui::{confidence_tag, human, items};
use deox_core::report::{Confidence, Report};
use deox_core::treemap::{layout, squarify, Cell, CellKind, FolderTree, LiveTree, Rect};
use eframe::egui::{self, Color32, RichText};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Height kept free under the map for the description of the picked tile.
const MAP_DETAIL_H: f32 = 92.0;
/// Name of the tile that stands for the files lying directly in a folder.
const FILES_TILE: &str = "Files in this folder";
/// Seconds the new tiles take to grow out of the tile that was opened.
const ZOOM_SECS: f64 = 0.22;
/// Most tiles drawn for one folder; the rest are merged into one tile.
const MAP_TILES: usize = 300;
/// Folders below this share of their parent are merged into one "smaller folders" tile.
const MAP_MIN_SHARE: u64 = 100;
/// How often the live map is rebuilt from the growing tree.
const LIVE_MAP_REFRESH: Duration = Duration::from_millis(500);
/// Smallest side, in points, of a rectangle in the map.
const MAP_MIN_SIDE: f32 = 3.0;

struct LiveTile {
    target: Option<usize>,
    name: String,
    size: u64,
    /// The folder has subfolders to open.
    opens: bool,
    path: String,
}

/// One frame of the live map, kept between rebuilds.
struct LiveSnapshot {
    trail: Vec<(usize, String)>,
    size: u64,
    tiles: Vec<LiveTile>,
}

pub(crate) struct LiveMap {
    /// The folder being shown, as a node of the live tree.
    folder: usize,
    /// Its subfolders in display order, with sizes (see [`stable_order`]).
    order: Vec<(usize, u64)>,
    reordered: Instant,
    refreshed: Option<Instant>,
    snapshot: Option<LiveSnapshot>,
}

impl Default for LiveMap {
    fn default() -> Self {
        LiveMap { folder: FolderTree::ROOT, order: Vec::new(), reordered: Instant::now(), refreshed: None, snapshot: None }
    }
}

/// Merges fresh sizes (largest first, every key seen so far) into `shown` without making entries jump:
/// known entries keep their place unless one is overtaken by more than 5%, at most once a second.
/// A key seen for the first time takes its place at once.
pub(crate) fn stable_order<K: Eq + std::hash::Hash>(shown: &mut Vec<(K, u64)>, fresh: Vec<(K, u64)>, reordered: &mut Instant) {
    let grew = fresh.len() > shown.len();
    if !grew {
        let sizes: HashMap<&K, u64> = fresh.iter().map(|(k, s)| (k, *s)).collect();
        for (key, size) in shown.iter_mut() {
            *size = sizes.get(key).copied().unwrap_or(*size);
        }
    }
    let overtaken = shown.windows(2).any(|w| w[1].1 as f64 > w[0].1 as f64 * 1.05);
    if grew || overtaken && reordered.elapsed() >= Duration::from_secs(1) {
        *shown = fresh;
        *reordered = Instant::now();
    }
}

/// The tiles for `folder`: its subfolders in the given order (largest first when sorted), with the small
/// ones merged into one tile and the files lying directly in the folder as another.
fn map_tiles(tree: &FolderTree, folder: usize, children: &[(usize, u64)]) -> Vec<(Option<usize>, String, u64)> {
    let node = tree.node(folder);
    let threshold = node.size / MAP_MIN_SHARE;
    let mut tiles = Vec::new();
    let (mut merged, mut merged_size) = (0usize, 0u64);
    for (position, (child, size)) in children.iter().enumerate() {
        if position < MAP_TILES && *size >= threshold.max(1) {
            tiles.push((Some(*child), tree.node(*child).name.clone(), *size));
        } else {
            merged += 1;
            merged_size += size;
        }
    }
    if merged > 0 && merged_size > 0 {
        tiles.push((None, format!("{merged} smaller folders"), merged_size));
    }
    if node.files_size > 0 {
        tiles.push((None, FILES_TILE.into(), node.files_size));
    }
    tiles
}

/// Rebuilds what the live map shows, at most twice a second, so tiles do not flicker while sizes grow.
fn refresh_live_map(live: &Mutex<LiveTree>, map: &mut LiveMap) {
    if map.refreshed.is_some_and(|t| t.elapsed() < LIVE_MAP_REFRESH) {
        return;
    }
    let live = live.lock().unwrap_or_else(|e| e.into_inner());
    let tree = live.tree();
    stable_order(&mut map.order, tree.children_by_size(map.folder), &mut map.reordered);
    let tiles = map_tiles(tree, map.folder, &map.order)
        .into_iter()
        .map(|(target, name, size)| LiveTile {
            path: target.map_or_else(|| name.clone(), |c| tree.path(c).display().to_string()),
            opens: target.is_some_and(|c| !tree.node(c).children.is_empty()),
            target,
            name,
            size,
        })
        .collect();
    let trail = tree.trail(map.folder);
    let trail = trail
        .iter()
        .enumerate()
        .map(|(n, id)| (*id, if n == 0 { tree.path(*id).display().to_string() } else { tree.node(*id).name.clone() }))
        .collect();
    map.snapshot = Some(LiveSnapshot { trail, size: tree.node(map.folder).size, tiles });
    map.refreshed = Some(Instant::now());
}

/// The folder tree so far as a treemap. Tiles are plain: nothing has been judged yet, so there is no
/// Safe/Review stripe, and a click opens a folder the same way as in the finished map.
pub(crate) fn live_map_view(ui: &mut egui::Ui, live: Option<&Arc<Mutex<LiveTree>>>, map: &mut LiveMap) {
    let Some(live) = live else { return };
    refresh_live_map(live, map);
    ui.ctx().request_repaint_after(LIVE_MAP_REFRESH);
    let Some(snapshot) = &map.snapshot else { return };
    let mut open = None;
    ui.horizontal_wrapped(|ui| {
        for (n, (id, name)) in snapshot.trail.iter().enumerate() {
            if n > 0 {
                ui.label("›");
            }
            if ui.selectable_label(n + 1 == snapshot.trail.len(), name).clicked() {
                open = Some(*id);
            }
        }
    });
    ui.label(RichText::new(format!("{} so far. Sizes are still growing.", human(snapshot.size))).weak());
    ui.add_space(4.0);
    if snapshot.tiles.is_empty() {
        ui.label("Nothing measured in this folder yet.");
    }

    let available = ui.available_size();
    let (area, _) = ui.allocate_exact_size(egui::vec2(available.x, available.y.max(160.0)), egui::Sense::hover());
    let sizes: Vec<u64> = snapshot.tiles.iter().map(|t| t.size).collect();
    let rects = squarify(&sizes, Rect { x: area.min.x, y: area.min.y, w: area.width(), h: area.height() });
    let font = egui::TextStyle::Body.resolve(ui.style());
    let line = ui.text_style_height(&egui::TextStyle::Body) + 2.0;
    for (index, (t, r)) in snapshot.tiles.iter().zip(&rects).enumerate() {
        let tile = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h)).shrink(1.0);
        if tile.width() < 2.0 || tile.height() < 2.0 {
            continue;
        }
        let opens = t.target.is_some() && t.opens;
        let sense = if opens { egui::Sense::click() } else { egui::Sense::hover() };
        let response = ui.interact(tile, ui.auto_id_with(("live_tile", index)), sense);
        let widgets = &ui.visuals().widgets;
        let (fill, text_color) = if response.hovered() {
            (widgets.hovered.bg_fill, widgets.hovered.fg_stroke.color)
        } else {
            (widgets.inactive.bg_fill, widgets.inactive.fg_stroke.color)
        };
        let painter = ui.painter().with_clip_rect(tile);
        painter.rect_filled(tile, 2.0, fill);
        if tile.width() > 56.0 && tile.height() > 24.0 {
            painter.text(tile.left_top() + egui::vec2(6.0, 4.0), egui::Align2::LEFT_TOP, &t.name, font.clone(), text_color);
            if tile.height() > 24.0 + line {
                painter.text(tile.left_top() + egui::vec2(6.0, 4.0 + line), egui::Align2::LEFT_TOP, human(t.size), font.clone(), text_color.gamma_multiply(0.8));
            }
        }
        if opens {
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{}, {}", t.name, human(t.size))));
        }
        let clicked = response.clicked();
        let hint = if opens { "\nClick to open." } else { "" };
        let response = if opens { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response };
        response.on_hover_text(format!("{}\n{}{hint}", t.path, human(t.size)));
        if clicked {
            open = t.target;
        }
    }
    if let Some(folder) = open {
        *map = LiveMap { folder, ..LiveMap::default() };
        ui.ctx().request_repaint();
    }
}

/// What the user asked for in the map besides browsing it.
pub(crate) enum MapAction {
    /// Show the items under this folder in the list.
    ShowInList(String),
    /// Select every Safe item under this folder.
    SelectSafe(PathBuf),
    /// Clear the selection under this folder.
    ClearHere(PathBuf),
}

/// What is picked in the map: a folder, one listed file, or the small files of a folder taken together.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pick {
    Folder(usize),
    File { folder: usize, index: usize },
    Rest(usize),
}

impl Pick {
    fn matches(self, kind: CellKind) -> bool {
        match (self, kind) {
            (Pick::Folder(a), CellKind::Folder(b)) | (Pick::Rest(a), CellKind::Rest(b)) => a == b,
            (Pick::File { folder: a, index: i }, CellKind::File { folder: b, index: j }) => a == b && i == j,
            _ => false,
        }
    }
}

/// The layout of the current folder, kept until the folder, the size of the map or the sizes change.
struct MapCache {
    key: (usize, i32, i32, u64),
    cells: Vec<Cell>,
}

/// Where the map is and the way back: folders visited, what is picked, and the zoom into a newly opened folder.
pub(crate) struct MapState {
    pub(crate) folder: usize,
    back: Vec<usize>,
    forward: Vec<usize>,
    pub(crate) picked: Option<Pick>,
    entered: f64,
    /// Where the opened cell was on screen, so the new cells can grow out of it.
    from: Option<egui::Rect>,
    cache: Option<MapCache>,
    /// The cell the context menu was opened on.
    menu: Option<usize>,
}

impl Default for MapState {
    fn default() -> Self {
        MapState {
            folder: FolderTree::ROOT,
            back: Vec::new(),
            forward: Vec::new(),
            picked: None,
            entered: f64::NEG_INFINITY,
            from: None,
            cache: None,
            menu: None,
        }
    }
}

impl MapState {
    pub(crate) fn open(&mut self, folder: usize, now: f64, from: Option<egui::Rect>) {
        if folder != self.folder {
            self.back.push(self.folder);
            self.forward.clear();
            self.arrive(folder, now, from);
        }
    }

    fn go_back(&mut self, now: f64) {
        if let Some(previous) = self.back.pop() {
            self.forward.push(self.folder);
            self.arrive(previous, now, None);
        }
    }

    fn go_forward(&mut self, now: f64) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.folder);
            self.arrive(next, now, None);
        }
    }

    fn arrive(&mut self, folder: usize, now: f64, from: Option<egui::Rect>) {
        self.folder = folder;
        self.picked = None;
        self.entered = now;
        self.from = from;
    }
}

/// The flag a finding gives to `folder` or to a folder above it, which every file inside inherits.
fn inherited_flag(tree: &FolderTree, folder: usize) -> Option<Confidence> {
    tree.trail(folder).into_iter().find_map(|id| tree.node(id).flag)
}

fn flag_text(flag: Option<Confidence>) -> &'static str {
    match flag {
        Some(Confidence::Safe) => "Safe to remove.",
        Some(Confidence::Review) => "Suggested for review.",
        _ => "Not suggested for removal.",
    }
}

/// The folder to open when a cell is clicked, and what to point out in it.
fn cell_target(kind: CellKind) -> (usize, Option<Pick>) {
    match kind {
        CellKind::Folder(id) => (id, None),
        CellKind::File { folder, index } => (folder, Some(Pick::File { folder, index })),
        CellKind::Rest(folder) => (folder, Some(Pick::Rest(folder))),
    }
}

/// The path a cell stands for.
fn cell_path(tree: &FolderTree, kind: CellKind) -> PathBuf {
    match kind {
        CellKind::Folder(id) | CellKind::Rest(id) => tree.path(id),
        CellKind::File { folder, index } => tree.node(folder).files.get(index).map_or_else(|| tree.path(folder), |f| tree.path(folder).join(&f.name)),
    }
}

fn cell_title(tree: &FolderTree, kind: CellKind) -> String {
    match kind {
        CellKind::Folder(id) => tree.node(id).name.clone(),
        CellKind::File { folder, index } => tree.node(folder).files.get(index).map_or_else(String::new, |f| f.name.clone()),
        CellKind::Rest(_) => "Smaller files".into(),
    }
}

/// Draws the current folder as a nested treemap: every folder holds its subfolders and files, and every
/// file big enough to see is a rectangle of its own. A click on a rectangle opens the folder it belongs to
/// and points out the rectangle; the bar under the map says what it is and what can be done with it.
pub(crate) fn map_view(
    ui: &mut egui::Ui,
    tree: &FolderTree,
    state: &mut MapState,
    dark: bool,
    report: &Report,
    selected: &HashSet<usize>,
) -> Option<MapAction> {
    let mut action = None;
    let now = ui.input(|i| i.time);
    let folder = state.folder;
    let trail = tree.trail(folder);

    let typing = ui.ctx().egui_wants_keyboard_input();
    let (key_back, key_forward) = ui.input(|i| {
        (
            i.pointer.button_clicked(egui::PointerButton::Extra1)
                || (!typing && i.key_pressed(egui::Key::Backspace))
                || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
            i.pointer.button_clicked(egui::PointerButton::Extra2) || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)),
        )
    });
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!state.back.is_empty(), egui::Button::new("←")).on_hover_text("Back (Backspace)").clicked() || key_back {
            state.go_back(now);
        }
        if ui.add_enabled(!state.forward.is_empty(), egui::Button::new("→")).on_hover_text("Forward (Alt+Right)").clicked() || key_forward {
            state.go_forward(now);
        }
        let parent = trail.len().checked_sub(2).map(|n| trail[n]);
        if ui.add_enabled(parent.is_some(), egui::Button::new("↑")).on_hover_text("Up one folder").clicked() {
            if let Some(parent) = parent {
                state.open(parent, now, None);
            }
        }
        ui.add_space(6.0);
        for (n, id) in trail.iter().copied().enumerate() {
            if n > 0 {
                ui.label("›");
            }
            let name = if n == 0 { tree.path(id).display().to_string() } else { tree.node(id).name.clone() };
            if ui.selectable_label(id == folder, name).clicked() {
                state.open(id, now, None);
            }
        }
    });
    let node = tree.node(folder);

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(human(node.size)).size(22.0));
        ui.label(RichText::new("in this folder").weak());
    });
    ui.add_space(4.0);
    if node.size == 0 {
        ui.label("This folder is empty.");
        return action;
    }

    let (_, safe_color, _) = confidence_tag(Confidence::Safe, dark);
    let (_, review_color, _) = confidence_tag(Confidence::Review, dark);
    let available = ui.available_size();
    let (area, response) = ui.allocate_exact_size(egui::vec2(available.x, (available.y - MAP_DETAIL_H).max(160.0)), egui::Sense::click());
    let progress = ((now - state.entered) / ZOOM_SECS).clamp(0.0, 1.0) as f32;
    let layout_area = match state.from {
        Some(from) if progress < 1.0 => {
            ui.ctx().request_repaint();
            let t = 1.0 - (1.0 - progress).powi(3);
            egui::Rect::from_min_max(from.min.lerp(area.min, t), from.max.lerp(area.max, t))
        }
        _ => area,
    };
    let font = egui::TextStyle::Body.resolve(ui.style());
    let line = ui.text_style_height(&egui::TextStyle::Body) + 2.0;

    let key = (folder, layout_area.width().round() as i32, layout_area.height().round() as i32, tree.revision());
    let cache = match state.cache.take() {
        Some(cache) if cache.key == key => cache,
        _ => {
            let bounds = Rect { x: 0.0, y: 0.0, w: layout_area.width(), h: layout_area.height() };
            MapCache { key, cells: layout(tree, folder, bounds, inherited_flag(tree, folder), line + 8.0, MAP_MIN_SIDE) }
        }
    };
    let origin = layout_area.min.to_vec2();
    let screen = |c: &Cell| egui::Rect::from_min_size(egui::pos2(c.rect.x, c.rect.y) + origin, egui::vec2(c.rect.w, c.rect.h));

    let hovered = response
        .hover_pos()
        .and_then(|p| cache.cells.iter().rposition(|c| screen(c).contains(p)));

    let widgets = &ui.visuals().widgets;
    let file_fill = widgets.inactive.bg_fill;
    let folder_fill = ui.visuals().extreme_bg_color;
    let text_color = widgets.inactive.fg_stroke.color;
    let (safe_fill, review_fill) = (file_fill.lerp_to_gamma(safe_color, 0.6), file_fill.lerp_to_gamma(review_color, 0.6));
    let (safe_frame, review_frame) = (folder_fill.lerp_to_gamma(safe_color, 0.25), folder_fill.lerp_to_gamma(review_color, 0.25));
    let strong = ui.visuals().strong_text_color();
    let painter = ui.painter().with_clip_rect(area);
    let mut picked_rect = None;
    for (index, cell) in cache.cells.iter().enumerate() {
        let rect = screen(cell).shrink(0.5);
        let is_folder = matches!(cell.kind, CellKind::Folder(_));
        let fill = match (cell.kind, cell.flag) {
            (CellKind::Folder(_), Some(Confidence::Safe)) => safe_frame,
            (CellKind::Folder(_), Some(Confidence::Review)) => review_frame,
            (CellKind::Folder(_), _) => folder_fill,
            (CellKind::Rest(_), _) => file_fill.gamma_multiply(0.8),
            (_, Some(Confidence::Safe)) => safe_fill,
            (_, Some(Confidence::Review)) => review_fill,
            _ => file_fill,
        };
        let radius = if rect.width() > 12.0 && rect.height() > 12.0 { 3.0 } else { 0.0 };
        painter.rect_filled(rect, radius, fill);
        if cell.header {
            let band = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), line + 8.0));
            painter.rect_filled(band, egui::CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 }, Color32::from_black_alpha(60));
        }
        if rect.width() > 60.0 && rect.height() > 22.0 && (cell.header || !is_folder) {
            let text = painter.with_clip_rect(rect);
            text.text(rect.left_top() + egui::vec2(6.0, 4.0), egui::Align2::LEFT_TOP, cell_title(tree, cell.kind), font.clone(), text_color);
            let below = rect.height() > line * 2.0 + 10.0 && !is_folder;
            if below {
                text.text(rect.left_top() + egui::vec2(6.0, 4.0 + line), egui::Align2::LEFT_TOP, human(cell.size), font.clone(), text_color.gamma_multiply(0.75));
            } else if cell.header && rect.width() > 150.0 {
                text.text(rect.right_top() + egui::vec2(-6.0, 4.0), egui::Align2::RIGHT_TOP, human(cell.size), font.clone(), text_color.gamma_multiply(0.75));
            }
        }
        if state.picked.is_some_and(|p| p.matches(cell.kind)) {
            picked_rect = Some(rect);
        }
        if hovered == Some(index) {
            painter.rect_stroke(rect, radius, egui::Stroke::new(1.5, strong.gamma_multiply(0.8)), egui::StrokeKind::Inside);
        }
    }
    if let Some(rect) = picked_rect {
        painter.rect_stroke(rect, 3.0, egui::Stroke::new(3.0, strong), egui::StrokeKind::Inside);
    }

    let mut enter: Option<(usize, Option<Pick>, egui::Rect)> = None;
    if let Some(index) = hovered {
        let cell = &cache.cells[index];
        if response.clicked() {
            let (target, pick) = cell_target(cell.kind);
            enter = Some((target, pick, screen(cell)));
        }
        if response.secondary_clicked() {
            state.menu = Some(index);
        }
    }
    let menu_cell = state.menu.and_then(|i| cache.cells.get(i)).map(|c| (c.kind, screen(c)));
    response.context_menu(|ui| {
        let Some((kind, rect)) = menu_cell else {
            ui.close();
            return;
        };
        if ui.button("Open folder").clicked() {
            let (target, pick) = cell_target(kind);
            enter = Some((target, pick, rect));
            ui.close();
        }
        if ui.button("Show in list").clicked() {
            action = Some(MapAction::ShowInList(cell_path(tree, kind).display().to_string()));
            ui.close();
        }
        if ui.button("Select Safe here").clicked() {
            action = Some(MapAction::SelectSafe(cell_path(tree, kind)));
            ui.close();
        }
        if ui.button("Clear selection here").clicked() {
            action = Some(MapAction::ClearHere(cell_path(tree, kind)));
            ui.close();
        }
    });
    let response = match hovered {
        Some(index) => {
            let cell = &cache.cells[index];
            let flag = match cell.flag {
                Some(Confidence::Safe) => "\nSafe to remove",
                Some(Confidence::Review) => "\nSuggested for review",
                _ => "",
            };
            let text = format!("{}\n{}{flag}", cell_path(tree, cell.kind).display(), human(cell.size));
            response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(text)
        }
        None => response,
    };
    drop(response);
    state.cache = Some(cache);
    if let Some((target, pick, rect)) = enter {
        state.open(target, now, Some(rect));
        state.picked = pick;
        ui.ctx().request_repaint();
    }

    // What can be done with the picked rectangle, or with this folder when none is picked.
    let focus = state.picked.and_then(|p| match p {
        Pick::Folder(id) => Some(CellKind::Folder(id)),
        Pick::File { folder, index } => tree.node(folder).files.get(index).map(|_| CellKind::File { folder, index }),
        Pick::Rest(folder) => Some(CellKind::Rest(folder)),
    });
    let focus_kind = focus.unwrap_or(CellKind::Folder(folder));
    let focus_path = cell_path(tree, focus_kind);
    let (title, size, detail) = match focus_kind {
        CellKind::Folder(id) => {
            let n = tree.node(id);
            let title = if id == FolderTree::ROOT { focus_path.display().to_string() } else { n.name.clone() };
            (title, n.size, format!("{} Safe, {} Review suggested.", human(n.safe), human(n.review)))
        }
        CellKind::File { folder, index } => {
            let f = &tree.node(folder).files[index];
            (f.name.clone(), f.size, flag_text(f.flag.or(inherited_flag(tree, folder))).to_string())
        }
        CellKind::Rest(id) => {
            let n = tree.node(id);
            let listed: u64 = n.files.iter().map(|f| f.size).sum();
            let note = format!("Too small to show one by one. {}", flag_text(inherited_flag(tree, id)));
            ("Smaller files".to_string(), n.files_size.saturating_sub(listed), note)
        }
    };
    let here = selected.iter().filter_map(|i| report.findings.get(*i)).filter(|f| f.path.starts_with(&focus_path)).count();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // The size comes first so a long name is cut short instead of pushing it out of the panel.
        ui.label(RichText::new(human(size)).monospace());
        ui.add(egui::Label::new(RichText::new(&title).strong()).truncate()).on_hover_text(&title);
    });
    let detail = format!("{detail} {} selected.", items(here));
    ui.add(egui::Label::new(RichText::new(&detail).weak()).truncate()).on_hover_text(detail);
    ui.horizontal(|ui| {
        if ui.button("Show in list").on_hover_text("Lists the suggested items here.").clicked() {
            action = Some(MapAction::ShowInList(focus_path.display().to_string()));
        }
        if ui.button("Select Safe here").on_hover_text("Selects every Safe item here. Nothing moves until you confirm.").clicked() {
            action = Some(MapAction::SelectSafe(focus_path.clone()));
        }
        if ui.add_enabled(here > 0, egui::Button::new("Clear here")).on_hover_text("Clears the selection here.").clicked() {
            action = Some(MapAction::ClearHere(focus_path.clone()));
        }
    });
    action
}
