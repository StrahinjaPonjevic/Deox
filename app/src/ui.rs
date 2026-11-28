use deox_core::report::{Category, Confidence};
use eframe::egui::{self, Color32, RichText};
use std::sync::Arc;
use std::time::Duration;

pub(crate) const CHECK_W: f32 = 24.0;
pub(crate) const TAG_W: f32 = 72.0;
pub(crate) const SIZE_W: f32 = 76.0;
pub(crate) const ITEM_INDENT: f32 = 18.0;
/// Seconds the bar below the headline takes to fill after a scan finishes.
pub(crate) const REVEAL_SECS: f64 = 0.9;

/// Darkens or lightens the default text so that even the dim text used for reasons and paths
/// keeps at least 4.5:1 against the panel in both themes (egui's own defaults give about 2.7:1).
pub(crate) fn configure_theme(ctx: &egui::Context) {
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
pub(crate) fn dialog_width(ctx: &egui::Context, wanted: f32) -> f32 {
    wanted.min(ctx.content_rect().width() - 48.0).max(320.0)
}

pub(crate) fn human(bytes: u64) -> String {
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

pub(crate) fn category_name(category: Category) -> &'static str {
    match category {
        Category::Temp => "Temporary files",
        Category::AppCache => "Application caches",
        Category::DevArtifact => "Developer artifacts",
        Category::Duplicate => "Duplicate files",
        Category::OldLarge => "Old large files",
        Category::OrphanedAppData => "Orphaned app data",
        Category::Bloatware => "Installed programs worth a look",
    }
}

/// Label, color and meaning of a safety level. Colors keep at least 4.5:1 against the panel in both themes.
pub(crate) fn confidence_tag(confidence: Confidence, dark: bool) -> (&'static str, Color32, &'static str) {
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
pub(crate) fn name_checkbox(check: &egui::Response, on: bool, name: &str) {
    check.widget_info(|| egui::WidgetInfo::selected(egui::accesskit::Role::CheckBox, true, on, name));
}

pub(crate) fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

pub(crate) fn paths(n: usize) -> String {
    if n == 1 { "1 path".into() } else { format!("{n} paths") }
}

/// The scanned size as one bar; the Safe and Review parts fill in from the left as `reveal` goes from 0 to 1.
pub(crate) fn reclaim_strip(ui: &mut egui::Ui, safe: u64, review: u64, total: u64, reveal: f32) {
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
pub(crate) fn share_bar(ui: &mut egui::Ui, part: u64, whole: u64) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(120.0, 6.0), egui::Sense::hover());
    let share = (part as f32 / whole as f32).clamp(0.0, 1.0);
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share, rect.height()));
    painter.rect_filled(fill, 3.0, confidence_tag(Confidence::Safe, ui.visuals().dark_mode).1);
    response.on_hover_text(format!("{:.0}% of what could be freed", share * 100.0));
}

/// Marks a safety level by shape as well as color: solid for Safe, hatched for Review, outlined for Info.
pub(crate) fn level_glyph(ui: &mut egui::Ui, level: Confidence, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    paint_level(ui.painter(), rect, level, color);
}

/// The mark of [`level_glyph`], drawn into a given square.
pub(crate) fn paint_level(painter: &egui::Painter, rect: egui::Rect, level: Confidence, color: Color32) {
    let painter = painter.with_clip_rect(rect);
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

/// A fixed-width table cell, so tags and sizes line up down the list.
pub(crate) fn cell(ui: &mut egui::Ui, width: f32, layout: egui::Layout, add: impl FnOnce(&mut egui::Ui)) {
    let size = egui::vec2(width, ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, layout, |ui| {
        ui.set_width(width);
        add(ui);
    });
}

/// A small triangle that points right when the group is collapsed and down when it is open.
pub(crate) fn disclosure_icon(ui: &mut egui::Ui, open: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
    paint_disclosure(ui.painter(), rect.center(), open, ui.visuals().text_color());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Expand or collapse group"));
    response
}

/// The triangle of [`disclosure_icon`], centred on `c`.
pub(crate) fn paint_disclosure(painter: &egui::Painter, c: egui::Pos2, open: bool, color: Color32) {
    let points = if open {
        vec![c + egui::vec2(-5.0, -3.0), c + egui::vec2(5.0, -3.0), c + egui::vec2(0.0, 4.0)]
    } else {
        vec![c + egui::vec2(-3.0, -5.0), c + egui::vec2(-3.0, 5.0), c + egui::vec2(4.0, 0.0)]
    };
    painter.add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));
}

/// The three safety levels in one always-visible line, so the colours and shapes are never a guess.
pub(crate) fn legend_line(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    ui.horizontal_wrapped(|ui| {
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
}

/// The number from a "Step N of M: …" status line.
pub(crate) fn scan_step(status: &str) -> Option<usize> {
    status.strip_prefix("Step ")?.split(' ').next()?.parse().ok()
}

/// One tick per scan step: finished steps steel, the running one teal, the rest empty.
pub(crate) fn step_ticks(ui: &mut egui::Ui, step: Option<usize>) {
    const STEPS: usize = 4;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(STEPS as f32 * 18.0 - 4.0, 14.0), egui::Sense::hover());
    let widgets = &ui.visuals().widgets;
    let (done, running, todo) = (
        widgets.noninteractive.fg_stroke.color.gamma_multiply(0.6),
        ui.visuals().selection.stroke.color,
        widgets.noninteractive.bg_stroke.color,
    );
    for n in 1..=STEPS {
        let x = rect.left() + (n - 1) as f32 * 18.0;
        let tick = egui::Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), egui::vec2(14.0, 4.0));
        let color = match step {
            Some(s) if n < s => done,
            Some(s) if n == s => running,
            _ => todo,
        };
        ui.painter().rect_filled(tick, 2.0, color);
    }
}

/// A line with a teal segment travelling along it. It is the scan's only continuous motion, so a stalled window is visible.
pub(crate) fn sweep(ui: &mut egui::Ui) {
    const LENGTH: f64 = 0.22;
    let (track, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
    let head = (ui.input(|i| i.time) * 0.5) % (1.0 + LENGTH);
    let at = |share: f64| track.left() + track.width() * share.clamp(0.0, 1.0) as f32;
    let segment = egui::Rect::from_min_max(egui::pos2(at(head - LENGTH), track.top()), egui::pos2(at(head), track.bottom()));
    ui.painter().rect_filled(track, 1.5, ui.visuals().widgets.noninteractive.bg_stroke.color);
    ui.painter().rect_filled(segment, 1.5, ui.visuals().selection.stroke.color);
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

#[cfg(test)]
mod tests {
    use super::scan_step;

    #[test]
    fn reads_the_step_number() {
        assert_eq!(scan_step("Step 2 of 4: applying cleanup rules…"), Some(2));
        assert_eq!(scan_step("Cancelling…"), None);
    }
}
