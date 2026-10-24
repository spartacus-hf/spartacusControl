//! The look: the radio's own phosphor palette on black,
//! amber and red kept for warnings and transmit, as on the panel.

use eframe::egui::{self, Color32, CornerRadius, FontId, Stroke, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0, 0, 0);
pub const PANEL: Color32 = Color32::from_rgb(0, 28, 16);
pub const PANEL_HI: Color32 = Color32::from_rgb(0, 52, 32);
pub const LINE: Color32 = Color32::from_rgb(0, 104, 56);
pub const TEXT: Color32 = Color32::from_rgb(224, 255, 232);
pub const DIM: Color32 = Color32::from_rgb(96, 224, 144);
pub const MUTED: Color32 = Color32::from_rgb(40, 160, 96);
pub const ACCENT: Color32 = Color32::from_rgb(0, 232, 224);
pub const FREQ: Color32 = Color32::from_rgb(72, 255, 120);
pub const OK: Color32 = Color32::from_rgb(48, 240, 96);
pub const WARN: Color32 = Color32::from_rgb(255, 196, 0);
pub const ALERT: Color32 = Color32::from_rgb(255, 64, 48);
pub const ALERT_BG: Color32 = Color32::from_rgb(128, 0, 0);

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(0, 12, 6);
    v.faint_bg_color = PANEL;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = Color32::from_rgb(0, 88, 88);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(2);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(2);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, DIM);
    v.widgets.inactive.bg_fill = PANEL;
    v.widgets.inactive.weak_bg_fill = PANEL;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.bg_fill = PANEL_HI;
    v.widgets.hovered.weak_bg_fill = PANEL_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.bg_fill = Color32::from_rgb(0, 88, 88);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0, 88, 88);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.text_styles.insert(TextStyle::Body, FontId::monospace(13.0));
    style.text_styles.insert(TextStyle::Button, FontId::monospace(13.0));
    style.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
    style.text_styles.insert(TextStyle::Small, FontId::monospace(10.0));
    style.text_styles.insert(TextStyle::Heading, FontId::monospace(18.0));
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(6.0, 2.0);
    // Dragging the frameless window must never turn into selecting text:
    // only the console and diagnostics output opt back in.
    style.interaction.selectable_labels = false;
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.set_theme(egui::Theme::Dark);
}
