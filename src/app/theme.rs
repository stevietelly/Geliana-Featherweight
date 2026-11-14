//! theme.rs — design tokens, window styling, and the app icon.
//!
//! Single source of truth for colors and spacing. Every `ui/` module reads
//! from here; no Color32 literals should appear elsewhere.

use std::io::Cursor;

use eframe::egui::{self, Color32, Stroke, Vec2};

// ---------------------------------------------------------------------------
// Design tokens
// ---------------------------------------------------------------------------

pub(crate) const BG: Color32 = Color32::from_rgb(18, 20, 24);
pub(crate) const SURFACE: Color32 = Color32::from_rgb(26, 29, 35);
pub(crate) const BORDER: Color32 = Color32::from_rgb(44, 48, 58);
pub(crate) const TEXT: Color32 = Color32::from_rgb(228, 232, 240);
pub(crate) const MUTED: Color32 = Color32::from_rgb(130, 138, 155);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(10, 115, 235);
pub(crate) const RED: Color32 = Color32::from_rgb(220, 48, 56);
pub(crate) const AMBER: Color32 = Color32::from_rgb(220, 165, 0);
pub(crate) const GREEN: Color32 = Color32::from_rgb(48, 210, 111);
pub(crate) const METER_BG: Color32 = Color32::from_rgb(20, 24, 30);
pub(crate) const LABEL_WIDTH: f32 = 120.0;
pub(crate) const CONTROL_HEIGHT: f32 = 28.0;

/// The 256 px application icon, embedded so both the window/taskbar icon and
/// the About view can share one decoded source.
pub(crate) const APP_ICON: &[u8] = include_bytes!("../assets/Featherweight_App_Icon_256.png");

// ---------------------------------------------------------------------------
// Window icon
// ---------------------------------------------------------------------------

pub(crate) fn load_window_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = decode_png_rgba(APP_ICON)?;
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

/// Decodes an embedded PNG into raw RGBA pixels (pixels, width, height).
///
/// Shared by the window icon ([`load_window_icon`]) and the tray icon (which
/// tints the monochrome glyph per recording state). `EXPAND | ALPHA` widens
/// palette and grayscale sources, and the result is normalized to 8-bit RGBA
/// so callers can assume one byte per channel.
pub(crate) fn decode_png_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let (width, height) = (info.width, info.height);
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|ga| [ga[0], ga[0], ga[0], ga[1]])
            .collect(),
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
            .collect(),
        _ => return None,
    };
    Some((pixels, width, height))
}

// ---------------------------------------------------------------------------
// Style configuration
// ---------------------------------------------------------------------------

pub(crate) fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 5.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size.y = CONTROL_HEIGHT;
    style.visuals = egui::Visuals::dark();
    style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = BG;
    style.visuals.faint_bg_color = SURFACE;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(40, 44, 54);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 44, 54);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(55, 60, 74);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(55, 60, 74);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, MUTED);
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, TEXT);
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.noninteractive,
    ] {
        widget.corner_radius = egui::CornerRadius::same(7);
    }
    ctx.set_style_of(egui::Theme::Dark, style);
}