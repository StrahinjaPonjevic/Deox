//! The extension list of the explorer, and the colors the map gives to file types.

use crate::ui::human;
use deox_core::treemap::FolderTree;
use eframe::egui::{self, Align2, Color32};

/// Colors that stay apart from one another in every pairing, also for color-blind readers, so no
/// more are added without checking them. Each is the light and the dark theme step.
const PALETTE: [([u8; 3], [u8; 3]); 3] = [
    ([0x2A, 0x78, 0xD6], [0x39, 0x87, 0xE5]),
    ([0xE8, 0x7B, 0xA4], [0xD5, 0x51, 0x81]),
    ([0x00, 0x83, 0x00], [0x0C, 0xA3, 0x0C]),
];

/// Widths of the columns right of the name.
const SIZE_COLUMN: f32 = 84.0;
const SHARE_COLUMN: f32 = 54.0;
const COUNT_COLUMN: f32 = 62.0;
/// The count is left out of a list narrower than this.
const COUNT_FROM: f32 = 270.0;

/// Which extensions have a color of their own, and which one is pointed out in the map.
#[derive(Default)]
pub(crate) struct FileTypes {
    /// In palette order. Kept by name, so a color stays with its extension when sizes change.
    colored: Vec<Box<str>>,
    pub(crate) highlighted: Option<Box<str>>,
}

impl FileTypes {
    /// Colors go to the extensions that take the most space.
    pub(crate) fn new(tree: &FolderTree) -> FileTypes {
        let colored = tree.extensions().iter().take(PALETTE.len()).map(|e| e.extension.clone()).collect();
        FileTypes { colored, highlighted: None }
    }

    /// `None` for the extensions that share the neutral color.
    pub(crate) fn color(&self, extension: &str, dark: bool) -> Option<Color32> {
        let slot = self.colored.iter().position(|name| &**name == extension)?;
        let (light_step, dark_step) = PALETTE[slot];
        let [r, g, b] = if dark { dark_step } else { light_step };
        Some(Color32::from_rgb(r, g, b))
    }

    /// Stops pointing out an extension that no file has any more.
    pub(crate) fn tree_rebuilt(&mut self, tree: &FolderTree) {
        self.highlighted = self.highlighted.take().filter(|name| tree.extensions().iter().any(|e| e.extension == *name));
    }
}

/// The right edges of the size, share and count columns; the count is `None` where it does not fit.
fn columns(row: egui::Rect) -> (f32, f32, Option<f32>) {
    let count = (row.width() >= COUNT_FROM).then(|| row.right());
    let share = count.map_or(row.right(), |end| end - COUNT_COLUMN);
    (share - SHARE_COLUMN, share, count)
}

/// Draws the list. A click points an extension out, and a second click stops. Returns `true` when
/// one was just pointed out.
pub(crate) fn view(ui: &mut egui::Ui, tree: &FolderTree, types: &mut FileTypes) -> bool {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    // As wide as the rows below, which leave room for the scroll bar.
    let width = ui.available_width() - ui.spacing().scroll.allocated_width();
    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (header, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let (size_end, share_end, count_end) = columns(header);
    let weak = ui.visuals().weak_text_color();
    let painter = ui.painter();
    let title_at = header.left_center() + egui::vec2(4.0, 0.0);
    painter.text(title_at, Align2::LEFT_CENTER, "Extension", font.clone(), weak);
    for (title, end) in [("Size", Some(size_end)), ("Share", Some(share_end)), ("Files", count_end)] {
        if let Some(end) = end {
            painter.text(egui::pos2(end - 4.0, header.center().y), Align2::RIGHT_CENTER, title, font.clone(), weak);
        }
    }

    let row_height = ui.spacing().interact_size.y;
    let total = tree.node(FolderTree::ROOT).size.max(1);
    let dark = ui.visuals().dark_mode;
    let mut clicked = None;
    let area = egui::ScrollArea::vertical().id_salt("extension_list").auto_shrink([false, false]);
    area.show_rows(ui, row_height, tree.extensions().len(), |ui, range| {
        for stat in &tree.extensions()[range] {
            let size = egui::vec2(ui.available_width(), row_height);
            let (row, response) = ui.allocate_exact_size(size, egui::Sense::click());
            let selected = types.highlighted.as_ref() == Some(&stat.extension);
            let files = if stat.count == 1 { "1 file".to_string() } else { format!("{} files", stat.count) };
            let label = format!("{}: {}, {files}", stat.extension, human(stat.size));
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
            let (size_end, share_end, count_end) = columns(row);
            let (text, weak) = (visuals.text_color(), visuals.weak_text_color());
            let middle = row.center().y;
            let swatch = egui::Rect::from_center_size(egui::pos2(row.left() + 10.0, middle), egui::vec2(12.0, 12.0));
            let fill = types.color(&stat.extension, dark).unwrap_or(visuals.widgets.inactive.bg_fill);
            painter.rect_filled(swatch, 2.0, fill);
            let names = egui::Rect::from_x_y_ranges(row.left()..=size_end - SIZE_COLUMN, row.y_range());
            let name_at = egui::pos2(row.left() + 22.0, middle);
            let names = painter.with_clip_rect(names);
            names.text(name_at, Align2::LEFT_CENTER, &*stat.extension, font.clone(), text);
            let at = |end: f32| egui::pos2(end - 4.0, middle);
            painter.text(at(size_end), Align2::RIGHT_CENTER, human(stat.size), mono.clone(), text);
            let percent = format!("{:.1}%", stat.size as f64 / total as f64 * 100.0);
            painter.text(at(share_end), Align2::RIGHT_CENTER, percent, font.clone(), weak);
            if let Some(end) = count_end {
                painter.text(at(end), Align2::RIGHT_CENTER, stat.count.to_string(), font.clone(), weak);
            }
            if response.clicked() {
                clicked = Some((stat.extension.clone(), selected));
            }
            response.on_hover_text(format!("{label}\nClick to point these files out in the map."));
        }
    });
    let Some((extension, was_selected)) = clicked else { return false };
    types.highlighted = (!was_selected).then_some(extension);
    ui.ctx().request_repaint();
    !was_selected
}
