//! Terminal/shell aesthetic for the native window: dark background, monospace,
//! the >Ratio palette (cyan/green/yellow/red on near-black).

use egui::{Color32, FontFamily, FontId, TextStyle};

pub const BG: Color32 = Color32::from_rgb(0x07, 0x0a, 0x09);
pub const PANEL: Color32 = Color32::from_rgb(0x0e, 0x13, 0x10);
pub const LINE: Color32 = Color32::from_rgb(0x1f, 0x2a, 0x22);
pub const FG: Color32 = Color32::from_rgb(0xe6, 0xf0, 0xe6);
pub const DIM: Color32 = Color32::from_rgb(0x8b, 0x94, 0x8c);
pub const CY: Color32 = Color32::from_rgb(0x36, 0xc5, 0xf4);
pub const GN: Color32 = Color32::from_rgb(0x41, 0xe0, 0x7a);
pub const YL: Color32 = Color32::from_rgb(0xf2, 0xc6, 0x6b);
pub const RD: Color32 = Color32::from_rgb(0xf0, 0x6c, 0x75);
pub const SEL: Color32 = Color32::from_rgb(0x14, 0x2a, 0x20);

/// Apply the dark terminal visuals + a monospace-everywhere text style.
pub fn apply(ctx: &egui::Context) {
    use egui::style::Visuals;
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = PANEL;
    v.faint_bg_color = PANEL;
    v.override_text_color = Some(FG);
    v.widgets.noninteractive.bg_stroke.color = LINE;
    v.widgets.inactive.bg_fill = PANEL;
    v.widgets.hovered.bg_fill = SEL;
    v.widgets.active.bg_fill = SEL;
    v.selection.bg_fill = CY.linear_multiply(0.35);
    v.hyperlink_color = CY;
    ctx.set_visuals(v);

    // Monospace everywhere (egui ships a monospace face with default_fonts).
    let mut style = (*ctx.global_style()).clone();
    for (st, sz) in [
        (TextStyle::Heading, 17.0),
        (TextStyle::Body, 13.0),
        (TextStyle::Monospace, 13.0),
        (TextStyle::Button, 13.0),
        (TextStyle::Small, 11.0),
    ] {
        style
            .text_styles
            .insert(st, FontId::new(sz, FontFamily::Monospace));
    }
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    ctx.set_global_style(style);
}
