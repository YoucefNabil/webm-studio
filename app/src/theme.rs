//! Dark, Vegas-like look.

use egui::{Color32, CornerRadius, Stroke, Visuals};

pub const PANEL_BG: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x20);
pub const TIMELINE_BG: Color32 = Color32::from_rgb(0x26, 0x27, 0x2a);
pub const HEADER_BG: Color32 = Color32::from_rgb(0x30, 0x31, 0x35);
pub const RULER_BG: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x1e);
pub const REGION_BAR_BG: Color32 = Color32::from_rgb(0x38, 0x3a, 0x40);
pub const LANE_VIDEO: Color32 = Color32::from_rgb(0x2c, 0x2d, 0x33);
pub const LANE_A: Color32 = Color32::from_rgb(0x29, 0x2b, 0x2e);
pub const LANE_B: Color32 = Color32::from_rgb(0x2e, 0x30, 0x33);
pub const LINE: Color32 = Color32::from_rgb(0x14, 0x14, 0x16);
pub const GRID: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 10);

pub const TEXT: Color32 = Color32::from_rgb(0xc8, 0xc8, 0xcc);
pub const DIM: Color32 = Color32::from_rgb(0x88, 0x89, 0x90);
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0xb3, 0xff);
pub const CURSOR: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
pub const OK: Color32 = Color32::from_rgb(0x6c, 0xd0, 0x7a);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xb0, 0x40);
pub const ERR: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);
pub const RENDER_BTN: Color32 = Color32::from_rgb(0xb8, 0x3a, 0x2e);

pub const REGION: Color32 = Color32::from_rgb(0x5d, 0x8f, 0xd8);
pub const REGION_PIN: Color32 = Color32::from_rgb(0xf2, 0xd2, 0x4b);
pub const REGION_SHADE: Color32 = Color32::from_rgba_premultiplied(30, 50, 80, 50);

pub const VCLIP: Color32 = Color32::from_rgb(0x3d, 0x4f, 0x7a);
pub const VCLIP_HEAD: Color32 = Color32::from_rgb(0x52, 0x66, 0x9a);
pub const VCLIP_SEL: Color32 = Color32::from_rgb(0x4c, 0x63, 0x99);
pub const VCLIP_HEAD_SEL: Color32 = Color32::from_rgb(0x6f, 0x88, 0xc8);
pub const ACLIP: Color32 = Color32::from_rgb(0x2f, 0x5c, 0x55);
pub const ACLIP_HEAD: Color32 = Color32::from_rgb(0x3f, 0x77, 0x6e);
pub const ACLIP_SEL: Color32 = Color32::from_rgb(0x3a, 0x73, 0x6a);
pub const ACLIP_HEAD_SEL: Color32 = Color32::from_rgb(0x55, 0x9d, 0x90);
pub const WAVE: Color32 = Color32::from_rgb(0x9f, 0xe0, 0xd2);
pub const WAVE_SEL: Color32 = Color32::from_rgb(0xd8, 0xff, 0xf4);

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = Color32::from_rgb(0x2a, 0x2b, 0x2f);
    v.window_fill = Color32::from_rgb(0x2a, 0x2b, 0x2f);
    v.extreme_bg_color = Color32::from_rgb(0x1a, 0x1a, 0x1c);
    v.selection.bg_fill = Color32::from_rgb(0x2f, 0x6d, 0xb5);
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = CornerRadius::same(3);
    v.widgets.hovered.corner_radius = CornerRadius::same(3);
    v.widgets.active.corner_radius = CornerRadius::same(3);
    ctx.set_visuals(v);
}
