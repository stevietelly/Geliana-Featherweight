//! Reusable widget primitives for the Featherweight UI.
//!
//! All functions are pure presentation — they take no application state beyond
//! what is passed as parameters. Import via `use crate::app::widgets::*;` or
//! individually as needed.
//!
//! Contents:
//! - Layout helpers: [`settings_card`], [`settings_card_state`],
//!   [`section_title`], [`row`], [`toggle_row`].
//! - Input controls: [`toggle`], [`integer_slider`], [`volume_slider`].
//! - Informational: [`level_bar`], [`warning_box`], [`ellipsis`].
//! - Internal helpers: [`db_of`], [`audio_level_text`], [`audio_level_dbfs`].

use eframe::egui::{
    self, Align, Button, Color32, Frame, Layout, Margin, Pos2, RichText, Slider, Stroke, Vec2,
};

// ---------------------------------------------------------------------------
// Design tokens (re-exported from theme so callers only import this module)
// ---------------------------------------------------------------------------

use crate::app::theme::{ACCENT, BORDER, MUTED, SURFACE, TEXT};

pub(crate) use crate::app::theme::{AMBER, GREEN, METER_BG, RED};

/// Error / alert color — used for hotkey conflicts, warnings.
pub(crate) const ERROR: Color32 = Color32::from_rgb(245, 85, 96);
/// Label column width in form rows.
pub(crate) const LABEL_WIDTH: f32 = 140.0;
/// Value column width (DragValue / small buttons) in form rows.
pub(crate) const VALUE_WIDTH: f32 = 96.0;
/// Standard interactive control height.
pub(crate) const CONTROL_HEIGHT: f32 = 28.0;

// ---------------------------------------------------------------------------
// Card containers
// ---------------------------------------------------------------------------

/// A bordered, rounded card that fills available width and has a minimum
/// `height`. Use this for the normal (valid) state.
pub(crate) fn settings_card(ui: &mut egui::Ui, height: f32, content: impl FnOnce(&mut egui::Ui)) {
    settings_card_state(ui, false, height, content);
}

/// Like [`settings_card`] but switches to an error tint when `invalid` is
/// `true` — used to highlight the shortcuts card when a hotkey conflict exists.
pub(crate) fn settings_card_state(
    ui: &mut egui::Ui,
    invalid: bool,
    height: f32,
    content: impl FnOnce(&mut egui::Ui),
) {
    Frame::new()
        .fill(if invalid {
            // Subtle red tint to signal an error state without being harsh.
            Color32::from_rgb(43, 26, 30)
        } else {
            SURFACE
        })
        .stroke(Stroke::new(
            1.0_f32,
            if invalid { ERROR } else { BORDER },
        ))
        .corner_radius(12.0)
        .inner_margin(Margin::symmetric(18, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height(height);
            content(ui);
        });
}

// ---------------------------------------------------------------------------
// Section headers
// ---------------------------------------------------------------------------

/// Bold section title inside a card, followed by a small vertical gap.
pub(crate) fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        ui.label(RichText::new(title).size(14.0).strong().color(TEXT));
    });
    ui.add_space(4.0);
}

/// Compact read-only status chip: one line of small text in a bordered pill.
///
/// `fg` colors both the border and the text; `bg` is the state tint (neutral
/// surface, amber, red). Used for the title-row disk state and the recording
/// bar's size/rate readout, so both read as the same kind of information.
pub(crate) fn stat_chip(
    ui: &mut egui::Ui,
    text: &str,
    fg: Color32,
    bg: Color32,
) -> egui::Response {
    Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, fg))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.5).color(fg)))
        .inner
}

// ---------------------------------------------------------------------------
// Row layouts
// ---------------------------------------------------------------------------

/// Two-column row: a [`LABEL_WIDTH`]-wide label on the left, then `content`
/// fills the remaining width, both vertically centered at [`CONTROL_HEIGHT`].
pub(crate) fn row(ui: &mut egui::Ui, label: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        // Fixed-width label column.
        ui.allocate_ui_with_layout(
            Vec2::new(LABEL_WIDTH, CONTROL_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_width(LABEL_WIDTH);
                ui.label(RichText::new(label).size(13.0));
            },
        );
        // Content column takes the rest.
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), CONTROL_HEIGHT),
            Layout::left_to_right(Align::Center),
            content,
        );
    });
}

/// Row with a label on the left and a [`toggle`] switch right-aligned.
///
/// Returns the toggle's `egui::Response` so the caller can chain
/// `.on_hover_text(...)` or `.changed()`.
pub(crate) fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    ui.horizontal(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        ui.label(RichText::new(label).size(13.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Pass the label text so screen-reader info is populated.
            toggle(ui, value, label)
        })
        .inner
    })
    .inner
}

// ---------------------------------------------------------------------------
// Toggle switch
// ---------------------------------------------------------------------------

/// Custom pill-shaped toggle switch with smooth animation and a focus ring.
///
/// `label` is used only for accessibility metadata (not painted). Returns the
/// widget's `egui::Response`.
pub(crate) fn toggle(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    // Populate accessibility info so VoiceOver / Narrator reads it correctly.
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *value, label)
    });
    // Animate the knob position: 0.0 = off (left), 1.0 = on (right).
    let animation = ui.ctx().animate_bool_responsive(response.id, *value);
    // Track color: accent when on, neutral when off.
    let track = if *value {
        ACCENT
    } else {
        Color32::from_rgb(72, 72, 79)
    };
    ui.painter().rect_filled(rect, 11.0, track);
    // Slide the knob along the pill.
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), animation);
    ui.painter()
        .circle_filled(Pos2::new(x, rect.center().y), 8.0, Color32::WHITE);
    // Draw a keyboard-focus ring around the pill.
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            13.0,
            Stroke::new(1.0_f32, TEXT),
            egui::StrokeKind::Outside,
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Sliders
// ---------------------------------------------------------------------------

/// Volume slider: an `f32` slider (0.0 – 1.5) with a live audio level meter
/// painted behind the rail, plus a percentage `DragValue`.
///
/// `peak` is the raw signal level (0.0 – 1.0+) *before* the volume gain is
/// applied; pass `0.0` if no audio preview is available.
/// `threshold_db` draws an amber tick mark at the gate threshold, or is `None`
/// to omit it.
pub(crate) fn volume_slider(
    ui: &mut egui::Ui,
    volume: &mut f32,
    peak: f32,
    threshold_db: Option<f32>,
) {
    // Reserve a placeholder shape in the painter's list; we fill it after
    // the slider has been laid out so we know where the rail actually lives.
    let rail_shape = ui.painter().add(egui::Shape::Noop);
    let rail_height = ui.spacing().slider_rail_height.max(6.0);

    // Available width minus the percentage DragValue column and its gap.
    let track_width =
        (ui.available_width() - VALUE_WIDTH - ui.spacing().item_spacing.x).max(60.0);

    // Render the slider with no visible rail (rail_height = 0) so our custom
    // painted rail is the only thing visible.
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().slider_rail_height = 0.0;
            ui.spacing_mut().slider_width = track_width;
            ui.add(
                Slider::new(volume, 0.0..=1.5)
                    .trailing_fill(false)
                    .show_value(false),
            )
        })
        .inner;

    // Percentage DragValue.
    let mut percent = *volume * 100.0;
    if ui
        .add_sized(
            [VALUE_WIDTH, CONTROL_HEIGHT],
            egui::DragValue::new(&mut percent)
                .range(0.0..=150.0)
                .speed(1.0)
                .fixed_decimals(0)
                .suffix("%"),
        )
        .changed()
    {
        *volume = percent / 100.0;
    }

    // Compute the fill fraction from the post-gain dBFS level.
    let level = peak * *volume;
    // Logarithmic scale: maps -60 dBFS → 0.0, 0 dBFS → 1.0.
    let fraction =
        audio_level_dbfs(level).map_or(0.0, |db| ((db + 60.0) / 60.0).clamp(0.0, 1.0));

    // Build the custom rail geometry.
    let left = response.rect.left();
    let width = response.rect.width();
    let rail = egui::Rect::from_min_size(
        Pos2::new(left, response.rect.center().y - rail_height / 2.0),
        Vec2::new(width, rail_height),
    );

    // Dark background track.
    let mut shapes = vec![egui::Shape::rect_filled(
        rail,
        rail_height / 2.0,
        Color32::from_rgb(26, 58, 43),
    )];

    // Colored fill proportional to signal level.
    if fraction > 0.0 {
        let fill = egui::Rect::from_min_size(rail.min, Vec2::new(width * fraction, rail_height));
        let color = if level >= 1.0 {
            // Clipping — paint red.
            Color32::from_rgb(239, 72, 72)
        } else {
            // Safe — paint green.
            Color32::from_rgb(48, 210, 111)
        };
        shapes.push(egui::Shape::rect_filled(fill, rail_height / 2.0, color));
    }

    // Optional gate threshold tick — amber vertical line.
    if let Some(db) = threshold_db.filter(|_| *volume > 0.0) {
        let position = ((db + 20.0 * volume.log10() + 60.0) / 60.0).clamp(0.0, 1.0);
        let x = rail.left() + rail.width() * position;
        shapes.push(egui::Shape::line_segment(
            [
                Pos2::new(x, rail.top() - 2.0),
                Pos2::new(x, rail.bottom() + 2.0),
            ],
            Stroke::new(1.5_f32, Color32::from_rgb(240, 184, 78)),
        ));
    }

    // Write all shapes back into the reserved slot so they appear behind the
    // native slider thumb.
    ui.painter().set(rail_shape, egui::Shape::Vec(shapes));

    // Hover tooltip: clipping warning or dBFS reading.
    response.on_hover_text(if level >= 1.0 {
        "Audio clipping — lower the volume".to_owned()
    } else {
        audio_level_text(level)
    });
}

/// Integer slider: a draggable slider plus a [`egui::DragValue`] edit box.
///
/// `suffix` is appended to the number (e.g. `" FPS"`, `" min"`). When
/// `unlimited` is `true`, a value of `0` renders as `"∞"`.
pub(crate) fn integer_slider(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    suffix: &str,
    unlimited: bool,
) {
    // The slider takes most of the row; the DragValue takes VALUE_WIDTH.
    let track_width =
        (ui.available_width() - VALUE_WIDTH - ui.spacing().item_spacing.x).max(60.0);
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = track_width;
        ui.add(Slider::new(value, range.clone()).show_value(false));
    });
    ui.add_sized(
        [VALUE_WIDTH, CONTROL_HEIGHT],
        egui::DragValue::new(value)
            .range(range)
            .speed(1.0)
            .suffix(suffix)
            .custom_formatter(move |val, _| {
                if unlimited && val < 0.5 {
                    "∞".to_owned()
                } else {
                    format!("{val:.0}")
                }
            }),
    );
}

// ---------------------------------------------------------------------------
// Level meter
// ---------------------------------------------------------------------------

/// Horizontal audio level bar: label on left, logarithmic color-coded bar,
/// dBFS text on right.
pub(crate) fn level_bar(ui: &mut egui::Ui, label: &str, peak: f32) {
    ui.horizontal(|ui| {
        ui.set_min_height(CONTROL_HEIGHT);
        // Label column.
        ui.allocate_ui_with_layout(
            Vec2::new(56.0, CONTROL_HEIGHT),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(RichText::new(label).size(12.0).color(MUTED));
            },
        );
        let bar_width = (ui.available_width() - 64.0).max(40.0);
        let (bar_rect, _) = ui.allocate_exact_size(Vec2::new(bar_width, 8.0), egui::Sense::hover());
        if ui.is_rect_visible(bar_rect) {
            let painter = ui.painter();
            let r = 4.0;
            // Background track.
            painter.rect_filled(bar_rect, r, METER_BG);
            // Level fill on a log scale.
            let db = db_of(peak);
            let fraction = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            if fraction > 0.0 {
                let fill_w = bar_rect.width() * fraction;
                let fill_rect = egui::Rect::from_min_size(
                    bar_rect.min,
                    Vec2::new(fill_w, bar_rect.height()),
                );
                let color = if peak >= 1.0 {
                    RED
                } else if db > -6.0 {
                    AMBER
                } else {
                    GREEN
                };
                painter.rect_filled(fill_rect, r, color);
            }
        }
        // dBFS label.
        let dbfs_text = if peak >= 0.001 {
            format!("{:.1} dB", db_of(peak))
        } else {
            "−∞ dB".into()
        };
        ui.label(
            RichText::new(dbfs_text)
                .size(10.0)
                .monospace()
                .color(MUTED),
        );
    });
}

// ---------------------------------------------------------------------------
// Warning box
// ---------------------------------------------------------------------------

/// Amber warning bar with a dismiss (`×`) button. Returns `true` when the user
/// clicked the dismiss button (the caller should clear the warning).
pub(crate) fn warning_box(ui: &mut egui::Ui, text: &str) -> bool {
    Frame::new()
        .fill(Color32::from_rgb(33, 30, 26))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = Vec2::ZERO;
                ui.spacing_mut().interact_size.y = 18.0;
                // Warning icon.
                ui.label(
                    RichText::new("⚠")
                        .size(12.0)
                        .color(Color32::from_rgb(211, 166, 93)),
                );
                // Truncated message text; full text in a hover tooltip.
                let width = (ui.available_width() - 24.0).max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text).size(11.0).color(MUTED))
                                .truncate(),
                        )
                    },
                )
                .inner
                .on_hover_text(text);
                // Dismiss button.
                ui.add_sized(
                    [18.0, 18.0],
                    Button::new(RichText::new("×").size(13.0).color(MUTED)).frame(false),
                )
                .on_hover_text("Dismiss")
                .clicked()
            })
            .inner
        })
        .inner
}

/// Neutral informational bar with a dismiss (`×`) button, used for one-off
/// hints (first run: where the tray icon lives). Returns `true` when dismissed.
pub(crate) fn hint_box(ui: &mut egui::Ui, text: &str) -> bool {
    Frame::new()
        .fill(Color32::from_rgb(24, 30, 40))
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().button_padding = Vec2::ZERO;
                ui.spacing_mut().interact_size.y = 18.0;
                ui.label(RichText::new("ℹ").size(12.0).color(ACCENT));
                let width = (ui.available_width() - 24.0).max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 18.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(text).size(11.0).color(MUTED))
                                .truncate(),
                        )
                    },
                )
                .inner
                .on_hover_text(text);
                ui.add_sized(
                    [18.0, 18.0],
                    Button::new(RichText::new("×").size(13.0).color(MUTED)).frame(false),
                )
                .on_hover_text("Dismiss")
                .clicked()
            })
            .inner
        })
        .inner
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Truncates `text` to `max_chars` characters, prefixing `"…"` when truncated.
/// Used to keep ComboBox selected-text labels from overflowing their widget.
pub(crate) fn ellipsis(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        text.to_owned()
    } else {
        format!(
            "…{}",
            chars[chars.len() - max_chars..].iter().collect::<String>()
        )
    }
}

// ---------------------------------------------------------------------------
// Audio helpers
// ---------------------------------------------------------------------------

/// Converts a linear peak amplitude to dBFS.
///
/// Returns `None` for values that are below the meter floor (< 0.001) or
/// non-finite, which the meter displays as −∞.
pub(crate) fn audio_level_dbfs(peak: f32) -> Option<f32> {
    (peak.is_finite() && peak >= 0.001).then(|| 20.0 * peak.log10())
}

/// Human-readable dBFS string for a peak amplitude, e.g. `"-6.0 dBFS"` or
/// `"−∞ dBFS"` for silence / sub-floor levels.
pub(crate) fn audio_level_text(peak: f32) -> String {
    audio_level_dbfs(peak)
        .map_or_else(|| "−∞ dBFS".to_owned(), |db| format!("{db:.1} dBFS"))
}

/// Raw dBFS conversion, clamped to −60 as the floor.
fn db_of(peak: f32) -> f32 {
    if peak >= 0.001 {
        20.0 * peak.log10()
    } else {
        -60.0
    }
}
