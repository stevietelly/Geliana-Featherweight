//! About view: application identity and the full third-party license texts.
//!
//! The license texts are embedded from `src/assets/licenses/` (exact texts
//! copied from the crates' own files) and from the vendored SpeexDSP tree, so
//! the binary ships its complete attribution without network access.

use eframe::egui::{
    self, ColorImage, RichText, ScrollArea, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};

use crate::app::{
    theme::{APP_ICON, BORDER, MUTED, SURFACE, TEXT},
    widgets::{section_title, settings_card, stat_chip},
};

// ---------------------------------------------------------------------------
// Embedded license texts
// ---------------------------------------------------------------------------

/// Exact license of the `windows-capture` crate.
const MIT_WINDOWS_CAPTURE: &str = include_str!("../assets/licenses/MIT-windows-capture.txt");
/// Exact license of the `wasapi` crate.
const MIT_WASAPI: &str = include_str!("../assets/licenses/MIT-wasapi.txt");
/// Exact license of the `rfd` crate.
const MIT_RFD: &str = include_str!("../assets/licenses/MIT-rfd.txt");
/// The MIT license itself (the app's license and most dependencies').
const MIT_TEXT: &str = include_str!("../assets/licenses/MIT.txt");
/// Full Apache-2.0 text (eframe/egui, winit — all dual MIT/Apache-2.0).
const APACHE_2_0: &str = include_str!("../assets/licenses/Apache-2.0.txt");
/// Exact license of the `nnnoiseless` crate (RNNoise in Rust).
const BSD_3_NNNOISELESS: &str = include_str!("../assets/licenses/BSD-3-Clause-nnnoiseless.txt");
/// The vendored SpeexDSP's own license.
const SPEEXDSP_COPYING: &str = include_str!("../../vendor/speexdsp/COPYING");

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The About view. Owned by [`crate::app::window::FeatherweightApp`]; holds
/// the app icon texture so it is decoded and uploaded once.
pub struct AboutView {
    icon: Option<TextureHandle>,
}

impl AboutView {
    pub fn new() -> Self {
        Self { icon: None }
    }

    /// Renders the identity card and the collapsible license groups.
    pub fn render(&mut self, ui: &mut egui::Ui) {
        // Cloned (cheap) so the view can pass `ui` on mutably while still
        // reaching the context for the icon texture upload.
        let ctx = ui.ctx().clone();
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.identity_card(ui, &ctx);
                ui.add_space(10.0);
                licenses_card(ui);
            });
    }

    /// Icon, name, version, description, developer, homepage, license chip.
    fn identity_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Decoded and uploaded once; kept for the view's lifetime.
        if self.icon.is_none() {
            self.icon = load_icon(ctx);
        }
        settings_card(ui, 0.0, |ui| {
            ui.horizontal(|ui| {
                match &self.icon {
                    Some(icon) => {
                        ui.add(egui::Image::new(icon).fit_to_exact_size(Vec2::splat(64.0)));
                    }
                    None => {
                        // Keeps the row's height stable if the embedded icon
                        // somehow fails to decode.
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(64.0), egui::Sense::hover());
                        ui.painter()
                            .rect_stroke(rect, 8.0, Stroke::new(1.0, BORDER), StrokeKind::Inside);
                    }
                }
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Geliana Featherweight")
                            .size(22.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.horizontal(|ui| {
                        stat_chip(
                            ui,
                            &format!("v{}", env!("CARGO_PKG_VERSION")),
                            MUTED,
                            SURFACE,
                        );
                        stat_chip(ui, "MIT", MUTED, SURFACE);
                    });
                    ui.label(
                        RichText::new(env!("CARGO_PKG_DESCRIPTION"))
                            .size(12.5)
                            .color(MUTED),
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Developed by {}",
                                env!("CARGO_PKG_AUTHORS").replace(',', " ·")
                            ))
                            .size(12.0)
                            .color(TEXT),
                        );
                        ui.hyperlink_to(env!("CARGO_PKG_HOMEPAGE"), env!("CARGO_PKG_HOMEPAGE"));
                    });
                });
            });
        });
    }
}

// ---------------------------------------------------------------------------
// License groups
// ---------------------------------------------------------------------------

/// The collapsible license groups card.
fn licenses_card(ui: &mut egui::Ui) {
    settings_card(ui, 0.0, |ui| {
        section_title(ui, "Third-party licenses");
        ui.label(
            RichText::new(
                "Geliana Featherweight is MIT-licensed and is built on open source. \
                 Each group lists the covered crates; expanding it shows the \
                 full license text.",
            )
            .size(12.0)
            .color(MUTED),
        );
        ui.add_space(4.0);

        license_group(
            ui,
            "license-mit",
            "MIT",
            "chrono · dirs · directories · eframe / egui · tray-icon · png · serde · \
             serde_json · tempfile · windows · windows-sys · windows-capture · wasapi · \
             rfd · winresource (build) · cc (build)",
            Some(MIT_TEXT),
            &[
                ("windows-capture", MIT_WINDOWS_CAPTURE),
                ("wasapi", MIT_WASAPI),
                ("rfd", MIT_RFD),
            ],
        );
        license_single(
            ui,
            "license-apache",
            "Apache-2.0",
            "eframe / egui · winit (both dual-licensed MIT / Apache-2.0)",
            APACHE_2_0,
        );
        license_single(
            ui,
            "license-bsd-nnnoiseless",
            "BSD-3-Clause",
            "nnnoiseless (RNNoise noise suppression)",
            BSD_3_NNNOISELESS,
        );
        license_single(
            ui,
            "license-speexdsp",
            "SpeexDSP (vendored)",
            "Speex noise suppression, vendored under its own license",
            SPEEXDSP_COPYING,
        );
    });
}

/// One collapsible group: the covered crates, the license itself (the plain
/// MIT text where every listed crate shares it), and any crate-specific
/// exact texts, each full-length in a bounded monospace scroll area.
fn license_group(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    crates: &str,
    license: Option<&str>,
    texts: &[(&str, &str)],
) {
    collapsing(ui, title, |ui| {
        ui.label(RichText::new(crates).size(11.5).color(MUTED));
        if let Some(license) = license {
            ui.add_space(6.0);
            license_text(ui, &format!("{id}-license"), license);
        }
        for (name, text) in texts {
            ui.add_space(6.0);
            ui.label(RichText::new(*name).size(12.0).strong().color(TEXT));
            license_text(ui, &format!("{id}-{name}"), text);
        }
    });
}

/// A group with a single license text.
fn license_single(ui: &mut egui::Ui, id: &str, title: &str, crates: &str, text: &str) {
    collapsing(ui, title, |ui| {
        ui.label(RichText::new(crates).size(11.5).color(MUTED));
        ui.add_space(4.0);
        license_text(ui, id, text);
    });
}

/// The group's collapsing header, so the full texts stay out of the way
/// until wanted.
fn collapsing(
    ui: &mut egui::Ui,
    title: &str,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(RichText::new(title).size(14.0).strong().color(TEXT))
        .show(ui, content);
}

/// Full license text, monospace, in a bounded scroll area.
fn license_text(ui: &mut egui::Ui, id: &str, text: &str) {
    ScrollArea::vertical()
        .max_height(170.0)
        .id_salt(id)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(10.5).color(MUTED)).wrap(),
            );
        });
}

// ---------------------------------------------------------------------------
// Icon
// ---------------------------------------------------------------------------

/// Decodes and uploads the app icon once; `None` if decoding failed.
fn load_icon(ctx: &egui::Context) -> Option<TextureHandle> {
    let (rgba, width, height) = crate::app::theme::decode_png_rgba(APP_ICON)?;
    Some(ctx.load_texture(
        "about-app-icon",
        ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba),
        TextureOptions::LINEAR,
    ))
}
