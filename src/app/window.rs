//! Main application window for Featherweight.
//!
//! [`FeatherweightApp`] is the top-level egui application. A tab strip
//! switches between three views: a live recording monitor (the landing
//! view), the two-column settings grid built from the widget primitives in
//! [`crate::app::widgets`], and the About view with its license texts. All
//! persistent state lives in [`AppConfig`]; the window calls `config.save()`
//! after every user change.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::Ordering};

use eframe::egui::{
    self, Align, Button, Color32, ComboBox, Image, Key, RichText, ScrollArea, Stroke, Vec2,
};

use crate::{
    app::{
        session::{SessionShared, SessionStats, StopReason},
        theme::{AMBER, BG, BORDER, MUTED, RED, TEXT},
        widgets::{
            self, CONTROL_HEIGHT, VALUE_WIDTH,
            integer_slider, row, section_title, settings_card, stat_chip,
            toggle_row, volume_slider, warning_box,
        },
    },
    capture::{
        monitors::{MonitorInfo, enumerate_monitors},
        preview::AudioPreview,
        recorder::SessionStatus,
        windows::{WindowInfo, enumerate_windows},
    },
    config::{AppConfig, CaptureTarget, NoiseSuppressionMethod},
    hotkeys::{Shortcut, input_matches_polling},
    tray::{Tray, TrayEvent},
};

// ---------------------------------------------------------------------------
// App struct
// ---------------------------------------------------------------------------

/// Which shortcut row is being rebound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebindSlot {
    /// The start-recording hotkey.
    Start,
    /// The stop-recording hotkey.
    Stop,
}

impl RebindSlot {
    /// Human name for messages.
    fn name(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
        }
    }

    /// The slot's current binding in a shortcut pair.
    fn get(self, shortcuts: crate::hotkeys::Shortcuts) -> Shortcut {
        match self {
            Self::Start => shortcuts.start,
            Self::Stop => shortcuts.stop,
        }
    }

    /// Writes a new binding into a shortcut pair.
    fn set(self, shortcuts: &mut crate::hotkeys::Shortcuts, value: Shortcut) {
        match self {
            Self::Start => shortcuts.start = value,
            Self::Stop => shortcuts.stop = value,
        }
    }

    /// The *other* slot, for conflict messages.
    fn other(self) -> Self {
        match self {
            Self::Start => Self::Stop,
            Self::Stop => Self::Start,
        }
    }
}

/// Top-level views, selectable from the tab strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// Live session monitor; the landing view.
    Recording,
    /// The two-column settings grid.
    Settings,
    /// App identity and third-party licenses.
    About,
}

pub struct FeatherweightApp {
    /// System tray icon and menu (main-thread-bound; visuals are synced
    /// from the shared session status each frame).
    tray: Tray,
    /// Shared recording hub (recorder, session status, stop flag).
    shared: Arc<SessionShared>,
    config: AppConfig,
    monitors: Vec<MonitorInfo>,
    /// Cached list of open application windows for the Window capture target.
    open_windows: Vec<WindowInfo>,
    /// Non-fatal warning shown in the warning bar; `None` = no bar.
    warning: Option<String>,
    /// Neutral, one-off hint (first run: where the tray icon lives).
    hint: Option<String>,
    /// Last known free bytes on the output volume; `None` = not yet queried or unavailable.
    disk_free: Option<u64>,
    /// Instant of the last disk check; we refresh at most once per second.
    last_disk_check: std::time::Instant,
    /// Tracks whether the window is currently visible so we can sync the tray
    /// menu label without an extra round-trip to the OS.
    window_visible: bool,
    /// Set when the app was asked to start in the notification area. The first
    /// frame hides the window explicitly, because the viewport's initial
    /// visibility is only a hint the windowing layer may not honour.
    hide_on_first_frame: bool,
    /// Idle audio meters: lightweight WASAPI streams measuring live peaks
    /// while no recording runs (see `capture::preview`). Dropped when the
    /// window hides; restarted when it becomes visible again.
    audio_preview: Option<AudioPreview>,
    /// Latest raw (pre-gain) peaks from the preview: [system, microphone].
    /// The volume sliders apply the gain themselves.
    audio_levels: [f32; 2],
    /// Sources the preview last tried to capture: (system, microphone).
    /// A failed start is not retried until the selection changes.
    preview_signature: (bool, bool),
    /// Shortcut row currently being rebound, if any. While set, the global
    /// hotkey poller is suspended (so the new keys cannot fire the current
    /// bindings) and the next key press becomes the binding.
    rebinding: Option<RebindSlot>,
    /// Transient message for the shortcuts card: guidance, or the outcome of
    /// the last rebind. The `bool` selects error styling.
    rebind_note: Option<(String, bool)>,
    /// Which view the central panel shows. Recording is the landing view so
    /// a fresh open answers "is it recording, and what is it doing".
    view: View,
    /// Whether the previous frame saw an active session. The Idle→active
    /// edge (a tray or hotkey start while the window is visible) lands on
    /// the Recording view.
    was_active: bool,
    /// The Recording view's still of the session's first frame.
    thumb: Option<egui::TextureHandle>,
    /// The session output path `thumb` belongs to: guards against showing a
    /// previous session's still while the current one has not produced its
    /// first frame yet, and lets the finished session's still linger on the
    /// idle card.
    thumb_for: Option<PathBuf>,
    /// Capture resolution recorded inside the thumbnail PNG; the true video
    /// size, which the downscaled still itself no longer shows.
    thumb_resolution: Option<(u32, u32)>,
    /// About view state (the app icon texture).
    about: crate::app::about::AboutView,
}

impl FeatherweightApp {
    /// `start_visible` is false when the app is configured to start in the
    /// notification area (see `hide_window`).
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        tray: Tray,
        shared: Arc<SessionShared>,
        mut config: AppConfig,
        start_visible: bool,
    ) -> Self {
        crate::app::theme::configure_style(&cc.egui_ctx);
        let monitors = enumerate_monitors();
        if !monitors
            .iter()
            .any(|monitor| monitor.index == config.monitor_index)
        {
            config.monitor_index = monitors[0].index;
        }
        // Mirror the initial config into the shared snapshot so tray actions
        // (New recording, Open recordings folder, preference checkboxes) see
        // the same values the UI will show.
        {
            let mut snapshot = shared.config.lock().unwrap();
            *snapshot = config.clone();
        }

        // Test hook, next to FEATHERWEIGHT_SIMULATE: start a session at launch
        // so the recording readout and the tray status line can be inspected
        // without clicking. Only honoured in simulated mode, so it can never
        // turn a real capture session into something the user did not ask for.
        let simulated = std::env::var("FEATHERWEIGHT_SIMULATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let autostart = std::env::var("FEATHERWEIGHT_AUTOSTART")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if simulated && autostart {
            shared.start_recording(config.clone());
        }

        // Keep the tray's Open/Hide label honest from the first frame,
        // whichever way the window started.
        tray.set_window_visible(start_visible);

        Self {
            tray,
            shared,
            config,
            monitors,
            open_windows: enumerate_windows(),
            warning: None,
            hint: None,
            disk_free: None,
            last_disk_check: std::time::Instant::now()
                - std::time::Duration::from_secs(10), // force immediate first check
            window_visible: start_visible,
            hide_on_first_frame: !start_visible,
            audio_preview: None,
            audio_levels: [0.0; 2],
            preview_signature: (false, false),
            rebinding: None,
            rebind_note: None,
            view: View::Recording,
            was_active: false,
            thumb: None,
            thumb_for: None,
            thumb_resolution: None,
            about: crate::app::about::AboutView::new(),
        }
    }

    /// Attaches one-off messages produced before the window existed: a settings
    /// load problem (`warning`) and the first-run tray hint (`hint`).
    pub fn with_startup_notices(mut self, warning: Option<String>, hint: Option<String>) -> Self {
        self.warning = warning;
        self.hint = hint;
        self
    }

    /// Saves the config and surfaces any error as a dismissible warning.
    fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.warning = Some(e);
        }
    }

    /// Shows or hides the window and keeps the tray menu label in sync.
    fn set_visible(&mut self, ctx: &egui::Context, visible: bool) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            // Bring to front when restoring from tray.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        } else {
            // No meters to feed while hidden; release the preview streams.
            // Resetting the signature makes the next visible frame restart
            // them if the config still has sources enabled.
            self.audio_preview = None;
            self.preview_signature = (false, false);

            // A rebinding left open while the window hides would wait for a
            // key press indefinitely on reopen; cancel it and re-arm the
            // global poller.
            if self.rebinding.is_some() {
                self.end_rebinding();
            }
        }
        self.window_visible = visible;
        self.tray.set_window_visible(visible);
    }

    /// Starts a simulated recording session through the shared hub, which
    /// spawns the worker and the event pump (see
    /// [`crate::app::session::SessionShared::start_recording`]).
    fn start_recording(&mut self) {
        self.shared.start_recording(self.config.clone());
        // The user just asked to record: land on the live monitor.
        self.view = View::Recording;
    }

    /// Requests the running session to stop. Every stop path reports a reason,
    /// so an automatic stop is distinguishable from a user one afterwards.
    fn stop_recording(&self) {
        self.shared.request_stop(StopReason::User);
    }

    // -----------------------------------------------------------------------
    // Hotkey rebinding
    // -----------------------------------------------------------------------

    /// Leaves rebind capture mode and re-arms the global poller. Every exit
    /// path from capture mode (Esc, clicking the row again, a successful
    /// binding, hiding the window) must call this — the poller is suspended
    /// the whole time.
    fn end_rebinding(&mut self) {
        self.shared.hotkeys.suspend(false);
        self.rebinding = None;
        self.rebind_note = None;
    }

    /// Consumes key input while a shortcut row is in capture mode.
    ///
    /// Called from the shortcuts card each frame; when nothing is being
    /// rebound it returns immediately. Rules, in order:
    /// - Esc cancels.
    /// - A key the poller cannot represent (not in the `KEYS` table, or a
    ///   bare letter with no modifier) is rejected with a hint.
    /// - A press that cannot be verified as the *physical* key it maps to
    ///   (keypad digits merge onto top-row digits in egui; Windows-key
    ///   contamination) is rejected — the poller would never see it.
    /// - A binding equal to the other shortcut is rejected as a conflict.
    /// - Anything else is applied: config, the live controller, and saved on
    ///     the frame's regular config-change sweep.
    fn poll_rebind_input(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.rebinding else { return };
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } = event
            else {
                continue;
            };

            if key == Key::Escape {
                self.end_rebinding();
                return;
            }

            let Some(pressed) = Shortcut::from_input(key, modifiers) else {
                self.rebind_note = Some((
                    "That key can't be a global shortcut — use a letter, digit, F-key, or add \
                     Ctrl/Alt/Shift."
                        .to_owned(),
                    true,
                ));
                continue;
            };

            // The egui press must be the same physical key the global poller
            // watches; otherwise the binding would be dead on arrival.
            if !input_matches_polling(pressed) {
                self.rebind_note = Some((
                    "Key press couldn't be verified (keypad and Windows-key presses don't \
                     register globally) — press it again."
                        .to_owned(),
                    true,
                ));
                continue;
            }

            let mut candidate = self.config.shortcuts;
            slot.set(&mut candidate, pressed);
            if !candidate.is_valid() {
                self.rebind_note = Some((
                    format!(
                        "That's already the {} shortcut — pick a different combination.",
                        slot.other().name().to_lowercase()
                    ),
                    true,
                ));
                continue;
            }

            self.config.shortcuts = candidate;
            // Push to the live controller; `update` bumps the revision so the
            // edge detector waits for all keys to be released before firing —
            // the just-pressed combination is likely still held.
            self.shared.hotkeys.update(candidate);
            self.rebind_note = Some((
                format!("{} shortcut set to {}.", slot.name(), pressed.label()),
                false,
            ));
            self.end_rebinding();
            return;
        }
    }

    // -----------------------------------------------------------------------
    // Recording view — live session monitor (landing view)
    // -----------------------------------------------------------------------

    /// The live monitor: the session state at a glance, with the first
    /// captured frame as a still. Everything here is mirrored from the hub,
    /// which the pump updates once per second — including while this window
    /// is hidden, which is why the window is not the sampler.
    fn render_recording(&mut self, ui: &mut egui::Ui) {
        // ---------------------------------------------------------------------------
        // Session state and its live numbers. Both are mirrored from the hub,
        // which the pump updates once per second — including while this window
        // is hidden, which is why the window is not the sampler.
        // ---------------------------------------------------------------------------
        let status = self.shared.status.lock().unwrap().clone();
        let stats = self.shared.stats.lock().unwrap().clone();
        let recording = matches!(status, SessionStatus::Recording { .. });

        // ---------------------------------------------------------------------------
        // Disk free-space polling — at most once per second, non-blocking.
        // While a session runs the pump owns this query (it keeps sampling with
        // the window hidden); asking twice would duplicate the syscall and could
        // disagree with the ETA shown next to it.
        // ---------------------------------------------------------------------------
        let now = std::time::Instant::now();
        if !recording
            && now.duration_since(self.last_disk_check) >= std::time::Duration::from_secs(1)
        {
            self.disk_free = crate::disk::free_bytes_for_path(&self.config.output_dir);
            self.last_disk_check = now;
        }
        // (Repaint scheduling lives in `fn ui`, so it applies to every
        // view, not just this one.)

        let hard_floor = crate::disk::hard_floor();
        let free = if recording { stats.free_bytes } else { self.disk_free };
        let disk_critical = if recording {
            stats.disk_critical
        } else {
            self.disk_free.is_some_and(|f| f <= hard_floor)
        };
        let disk_warn = if recording {
            stats.disk_warn
        } else {
            self.disk_free
                .is_some_and(|f| f <= crate::disk::WARN_FLOOR_BYTES)
        };

        // ---------------------------------------------------------------------------
        // Title row + disk status chip
        // ---------------------------------------------------------------------------
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Featherweight")
                    .size(22.0)
                    .strong()
                    .color(TEXT),
            );

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if let Some(free_bytes) = free {
                    let (fg, bg) = if disk_critical {
                        (RED, Color32::from_rgb(80, 16, 20))
                    } else if disk_warn {
                        (AMBER, Color32::from_rgb(60, 44, 0))
                    } else {
                        (MUTED, Color32::from_rgb(26, 29, 35))
                    };
                    // Idle: just the free space. Recording: the measured rate
                    // turns that into a time, which is what actually tells the
                    // user whether the recording will survive.
                    let text = match (recording, stats.eta_secs) {
                        (true, Some(eta)) => format!(
                            "{} free · {}",
                            crate::disk::format_bytes(free_bytes),
                            crate::disk::format_eta(eta)
                        ),
                        _ => format!("{} free", crate::disk::format_bytes(free_bytes)),
                    };
                    stat_chip(ui, &text, fg, bg);
                }
            });
        });
        ui.add_space(6.0);

        // -------------------------------------------------------------------
        // Recording bar — start/stop, live elapsed time, output size and rate.
        // State comes from the shared hub (updated by the event pump), so it
        // stays correct across hide/show cycles and tray starts.
        // -------------------------------------------------------------------
        match status {
            SessionStatus::Recording { path, elapsed_secs } => {
                self.render_recording_active(ui, &stats, &path, elapsed_secs);
            }
            SessionStatus::WaitingForFirstFrame { target } => {
                ui.horizontal(|ui| {
                    let frame = egui::Frame::new()
                        .fill(Color32::from_rgb(60, 44, 0))
                        .stroke(Stroke::new(1.0, AMBER))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(10, 6));
                    frame.show(ui, |ui| {
                        ui.label(RichText::new("◌").color(AMBER).size(14.0));
                        ui.label(
                            RichText::new(format!("Waiting for {target}…"))
                                .monospace()
                                .color(TEXT),
                        );
                    });
                    ui.label(
                        RichText::new(
                            "The window has not delivered a frame yet — is it minimized?",
                        )
                        .color(MUTED),
                    );
                    if ui
                        .add_sized([110.0, CONTROL_HEIGHT], Button::new("Stop"))
                        .clicked()
                    {
                        self.stop_recording();
                    }
                });
                ui.add_space(8.0);
                // Placeholder until the target's first frame lands.
                draw_still(
                    ui,
                    None,
                    &format!("Waiting for {target}…"),
                    "The still appears once the target delivers its first frame — a \
                     minimized window delivers none until restored.",
                );
            }
            SessionStatus::Idle => {
                let simulated = std::env::var("FEATHERWEIGHT_SIMULATE")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized([110.0, CONTROL_HEIGHT], Button::new("Record"))
                        .clicked()
                    {
                        self.start_recording();
                    }
                    ui.label(
                        RichText::new(if simulated {
                            "No recording running. Simulated sessions capture no frames."
                        } else {
                            "No recording running. Record uses the settings on the Settings tab."
                        })
                        .size(12.0)
                        .color(MUTED),
                    );
                });

                // The finished session's still lingers as a dimmed card.
                if let (Some(last_thumb), Some(last_path)) =
                    (self.thumb.clone(), self.thumb_for.clone())
                {
                    ui.add_space(10.0);
                    egui::Frame::new()
                        .fill(Color32::from_rgb(24, 28, 34))
                        .stroke(Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("Last recording")
                                            .size(12.0)
                                            .strong()
                                            .color(MUTED),
                                    );
                                    let dir = last_path
                                        .parent()
                                        .map(Path::to_string_lossy)
                                        .unwrap_or_default();
                                    ui.label(
                                        RichText::new(widgets::ellipsis(&dir, 44))
                                            .size(11.0)
                                            .color(MUTED),
                                    )
                                    .on_hover_text(dir.as_ref());
                                    if ui
                                        .add_sized(
                                            [110.0, CONTROL_HEIGHT],
                                            Button::new("Open folder"),
                                        )
                                        .clicked()
                                    {
                                        if let Err(e) = crate::tray::menu::open_folder(
                                            last_path.parent().unwrap_or(Path::new(&last_path)),
                                        ) {
                                            self.warning = Some(e);
                                        }
                                    }
                                });
                                // Dimmed still, right-aligned.
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    let [tex_w, tex_h] = last_thumb.size();
                                    let aspect = tex_w.max(1) as f32 / tex_h.max(1) as f32;
                                    let width = 150.0;
                                    ui.add(
                                        Image::new(&last_thumb)
                                            .fit_to_exact_size(Vec2::new(width, width / aspect))
                                            .corner_radius(6.0)
                                            .tint(Color32::from_rgb(150, 155, 165)),
                                    );
                                });
                            });
                        });
                }
            }
        }
        ui.add_space(6.0);

        // Critical disk banner — sticky, not dismissible (actual stop risk)
        if disk_critical {
            let frame = egui::Frame::new()
                .fill(Color32::from_rgb(70, 12, 16))
                .stroke(Stroke::new(1.0, RED))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(10, 6));
            frame.show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.label(
                    RichText::new(format!(
                        "⚠  Disk almost full — recording will stop below {}. Free up space on the output drive.",
                        crate::disk::format_bytes(hard_floor)
                    ))
                    .size(12.5)
                    .color(RED),
                );
            });
            ui.add_space(4.0);
        } else if disk_warn {
            let frame = egui::Frame::new()
                .fill(Color32::from_rgb(50, 36, 0))
                .stroke(Stroke::new(1.0, AMBER))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(10, 6));
            frame.show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                // While recording, the useful form of "low disk" is how long
                // that actually is at the measured rate.
                let detail = match (recording, stats.eta_secs) {
                    (true, Some(eta)) => format!(" — {}", crate::disk::format_eta(eta)),
                    _ => String::new(),
                };
                ui.label(
                    RichText::new(format!(
                        "⚠  Low disk space — {} remaining on output drive{}.",
                        crate::disk::format_bytes(free.unwrap_or(0)),
                        detail
                    ))
                    .size(12.5)
                    .color(AMBER),
                );
            });
            ui.add_space(4.0);
        }

        // Automatic stops and failures are explained here: the pump records
        // why, because the window is usually hidden when it happens.
        if !recording {
            let last_error = self.shared.last_error.lock().unwrap().clone();
            let stop_reason = self.shared.stop_reason.lock().unwrap().clone();
            if let Some(error) = last_error {
                if warning_box(ui, &format!("Recording failed: {error}")) {
                    self.shared.clear_last_error();
                }
                ui.add_space(6.0);
            } else if let Some(reason) = stop_reason.filter(StopReason::is_notable) {
                if warning_box(ui, &reason.summary()) {
                    self.shared.clear_stop_reason();
                }
                ui.add_space(6.0);
            }
        }

        // Config-save / startup warnings (dismissible). Rendered on the
        // Settings view too, so save or preview errors stay visible.
        self.render_warning_bar(ui);

        // First-run hint: where the app went, and how to pin the tray icon.
        if let Some(hint) = self.hint.clone() {
            if widgets::hint_box(ui, &hint) {
                self.hint = None;
            }
            ui.add_space(6.0);
        }
    }

    /// The monitor card shown while a session records: the first captured
    /// frame as a still on the left, the live readouts on the right.
    fn render_recording_active(
        &mut self,
        ui: &mut egui::Ui,
        stats: &SessionStats,
        path: &Path,
        elapsed_secs: u64,
    ) {
        // Load (or wait for) this session's still; until it arrives the
        // placeholder explains why. In simulated mode no frames are ever
        // captured, so say that instead of implying the still is merely slow.
        let simulated = std::env::var("FEATHERWEIGHT_SIMULATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let (pending_title, pending_detail) = if simulated {
            (
                "No still in simulated mode",
                "Simulated sessions capture no frames, so there is no still to show.",
            )
        } else {
            (
                "First frame pending…",
                "The still appears after the first captured frame.",
            )
        };
        self.sync_thumbnail(ui.ctx(), Some(path));
        let thumb_ready = self.thumb_for.as_deref() == Some(path);
        let thumb = if thumb_ready {
            self.thumb.clone()
        } else {
            None
        };
        let resolution = if thumb_ready {
            self.thumb_resolution
        } else {
            None
        };

        ui.horizontal(|ui| {
            // Left: the still, sized to fit what is left of the window.
            let left_w = (ui.available_width() * 0.45).clamp(180.0, 420.0);
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, ui.available_height().max(140.0)),
                egui::Layout::top_down(Align::Center),
                |ui| {
                    draw_still(
                        ui,
                        thumb.as_ref(),
                        pending_title,
                        pending_detail,
                    );
                },
            );

            // Right: live readouts.
            ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                let frame = egui::Frame::new()
                    .fill(Color32::from_rgb(70, 12, 16))
                    .stroke(Stroke::new(1.0, RED))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 6));
                frame.show(ui, |ui| {
                    ui.label(RichText::new("●").color(RED).size(14.0));
                    ui.label(
                        RichText::new(format!(
                            "Recording {}",
                            crate::tray::format_duration(elapsed_secs)
                        ))
                        .monospace()
                        .color(TEXT),
                    );
                });

                // What is being captured, as named at session start (the
                // picker can change mid-session without rewriting this).
                if let Some(target) = &stats.target_title {
                    ui.label(
                        RichText::new(widgets::ellipsis(target, 64))
                            .size(14.0)
                            .strong()
                            .color(TEXT),
                    )
                    .on_hover_text(target);
                }

                // Size · rate · RAM, plus the capture resolution once known.
                ui.horizontal(|ui| {
                    stat_chip(
                        ui,
                        &stats_readout(stats),
                        MUTED,
                        Color32::from_rgb(26, 29, 35),
                    );
                    if let Some((width, height)) = resolution {
                        stat_chip(
                            ui,
                            &format!("{width}×{height}"),
                            MUTED,
                            Color32::from_rgb(26, 29, 35),
                        );
                    }
                });

                // Destination (hover = full path), Open folder, Stop.
                ui.horizontal(|ui| {
                    let path_text = path.to_string_lossy();
                    ui.add(
                        egui::Label::new(
                            RichText::new(widgets::ellipsis(&path_text, 48))
                                .size(12.0)
                                .color(MUTED),
                        )
                        .truncate(),
                    )
                    .on_hover_text(path_text.as_ref());
                    if ui
                        .add_sized([110.0, CONTROL_HEIGHT], Button::new("Open folder"))
                        .clicked()
                    {
                        let dir = path.parent().unwrap_or(path).to_path_buf();
                        if let Err(e) = crate::tray::menu::open_folder(&dir) {
                            self.warning = Some(e);
                        }
                    }
                    if ui
                        .add_sized([90.0, CONTROL_HEIGHT], Button::new("Stop"))
                        .clicked()
                    {
                        self.stop_recording();
                    }
                });
            });
        });
    }

    /// Loads the first-frame still for `session_path` when it is not the one
    /// already loaded.
    ///
    /// While a session runs and its still has not landed (the pipeline
    /// writes it on the first captured frame), this re-reads on a 200 ms
    /// repaint cadence until it does. On load it also picks up the capture
    /// resolution recorded inside the PNG.
    fn sync_thumbnail(&mut self, ctx: &egui::Context, session_path: Option<&Path>) {
        let Some(session_path) = session_path else {
            return;
        };
        if self.thumb_for.as_deref() == Some(session_path) {
            return; // this session's still is already loaded
        }
        let loaded = (|| -> Option<(egui::TextureHandle, Option<(u32, u32)>)> {
            let png_path = crate::capture::pipeline::first_frame_png_path()?;
            let bytes = std::fs::read(&png_path).ok()?;
            let (rgba, width, height) = crate::app::theme::decode_png_rgba(&bytes)?;
            let texture = ctx.load_texture(
                "first-frame",
                egui::ColorImage::from_rgba_unmultiplied(
                    [width as usize, height as usize],
                    &rgba,
                ),
                egui::TextureOptions::LINEAR,
            );
            let resolution = crate::capture::pipeline::first_frame_source_resolution();
            Some((texture, resolution))
        })();
        match loaded {
            Some((texture, resolution)) => {
                self.thumb = Some(texture);
                self.thumb_resolution = resolution;
                self.thumb_for = Some(session_path.to_path_buf());
            }
            None => {
                // The still has not landed yet; try again shortly.
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }

    /// The dismissible warning bar (config-save problems, audio-preview
    /// failures). Rendered on the Recording and Settings views alike so a
    /// warning raised while the settings grid is open stays visible.
    fn render_warning_bar(&mut self, ui: &mut egui::Ui) {
        if let Some(warning) = self.warning.clone() {
            if warning_box(ui, &warning) {
                self.warning = None;
            }
            ui.add_space(6.0);
        }
    }

    // -----------------------------------------------------------------------
    // Settings view — the full two-column settings grid
    // -----------------------------------------------------------------------

    /// The two-column settings grid (the app's old single view), kept
    /// unchanged apart from what moved to the Recording view.
    fn render_settings(&mut self, ui: &mut egui::Ui) {
        // Config-save warnings are mostly raised by interactions on this
        // very view, so it renders the dismissible warning bar as well.
        self.render_warning_bar(ui);

        // Scrollable settings area
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                // Calculate a shared card height so both columns match.
                let card_h = (ui.available_height() - 8.0).clamp(0.0, 480.0);

                ui.columns(2, |cols| {
                    // -------------------------------------------------------
                    // LEFT column — Video & Audio
                    // -------------------------------------------------------
                    settings_card(&mut cols[0], card_h, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.spacing_mut().button_padding.y = 5.0;

                        section_title(ui, "Video & Audio");

                        // Capture target — Monitor or Window
                        let is_monitor = matches!(
                            self.config.capture_target,
                            CaptureTarget::Monitor { .. }
                        );

                        // Mode toggle: two selectable chips side by side
                        row(ui, "Capture", |ui| {
                            let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
                            if ui
                                .add_sized(
                                    [w, CONTROL_HEIGHT],
                                    Button::new("Monitor").selected(is_monitor),
                                )
                                .clicked()
                                && !is_monitor
                            {
                                // Switch to monitor mode; default to the first monitor.
                                let idx = self
                                    .monitors
                                    .first()
                                    .map(|m| m.index)
                                    .unwrap_or(1);
                                self.config.capture_target =
                                    CaptureTarget::Monitor { index: idx };
                                self.config.monitor_index = idx;
                            }
                            if ui
                                .add_sized(
                                    [w, CONTROL_HEIGHT],
                                    Button::new("Window").selected(!is_monitor),
                                )
                                .clicked()
                                && is_monitor
                            {
                                // Switch to window mode; refresh the list and
                                // pre-select the first entry if any.
                                self.open_windows = enumerate_windows();
                                if let Some(first) = self.open_windows.first() {
                                    self.config.capture_target = CaptureTarget::Window {
                                        id: first.id,
                                        title: first.title.clone(),
                                    };
                                } else {
                                    self.config.capture_target =
                                        CaptureTarget::Window { id: 0, title: String::new() };
                                }
                            }
                        });

                        // Second row: the specific monitor or window picker
                        match &self.config.capture_target.clone() {
                            CaptureTarget::Monitor { index } => {
                                let mut sel_idx = *index;
                                let selected_label = self
                                    .monitors
                                    .iter()
                                    .find(|m| m.index == sel_idx)
                                    .map(|m| m.label.as_str())
                                    .unwrap_or("Primary display");
                                row(ui, "Display", |ui| {
                                    ComboBox::from_id_salt("monitor")
                                        .selected_text(widgets::ellipsis(selected_label, 28))
                                        .width(ui.available_width())
                                        .show_ui(ui, |ui| {
                                            for monitor in &self.monitors {
                                                ui.selectable_value(
                                                    &mut sel_idx,
                                                    monitor.index,
                                                    &monitor.label,
                                                );
                                            }
                                        });
                                });
                                if sel_idx != *index {
                                    self.config.capture_target =
                                        CaptureTarget::Monitor { index: sel_idx };
                                    self.config.monitor_index = sel_idx;
                                }
                            }
                            CaptureTarget::Window { id, title } => {
                                let mut sel_id = *id;
                                let selected_title = self
                                    .open_windows
                                    .iter()
                                    .find(|w| w.id == sel_id)
                                    .map(|w| w.title.clone())
                                    .unwrap_or_else(|| {
                                        if title.is_empty() {
                                            "Pick a window".to_owned()
                                        } else {
                                            title.clone()
                                        }
                                    });
                                row(ui, "Window", |ui| {
                                    // Refresh button — re-enumerate windows
                                    let refresh_w = 26.0;
                                    let combo_w = ui.available_width()
                                        - refresh_w
                                        - ui.spacing().item_spacing.x;
                                    ComboBox::from_id_salt("window_target")
                                        .selected_text(widgets::ellipsis(&selected_title, 26))
                                        .width(combo_w)
                                        .show_ui(ui, |ui| {
                                            if self.open_windows.is_empty() {
                                                ui.label(
                                                    RichText::new("No open windows found")
                                                        .color(MUTED),
                                                );
                                            }
                                            for win in &self.open_windows {
                                                ui.selectable_value(
                                                    &mut sel_id,
                                                    win.id,
                                                    &win.title,
                                                );
                                            }
                                        });
                                    if ui
                                        .add_sized(
                                            [refresh_w, CONTROL_HEIGHT],
                                            Button::new("↺"),
                                        )
                                        .on_hover_text("Refresh window list")
                                        .clicked()
                                    {
                                        self.open_windows = enumerate_windows();
                                        // If current selection vanished, pick the first.
                                        if !self.open_windows.iter().any(|w| w.id == sel_id) {
                                            if let Some(first) = self.open_windows.first() {
                                                sel_id = first.id;
                                            }
                                        }
                                    }
                                });
                                // Commit if selection changed.
                                if sel_id != *id {
                                    let new_title = self
                                        .open_windows
                                        .iter()
                                        .find(|w| w.id == sel_id)
                                        .map(|w| w.title.clone())
                                        .unwrap_or_default();
                                    self.config.capture_target = CaptureTarget::Window {
                                        id: sel_id,
                                        title: new_title,
                                    };
                                }
                            }
                        }

                        // Frame rate
                        row(ui, "Frame rate", |ui| {
                            ComboBox::from_id_salt("fps")
                                .selected_text(format!("{} FPS", self.config.fps))
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for fps in [15u32, 24, 30, 60, 120, 144, 240] {
                                        ui.selectable_value(
                                            &mut self.config.fps,
                                            fps,
                                            format!("{fps} FPS"),
                                        );
                                    }
                                    ui.separator();
                                    ui.add(
                                        egui::DragValue::new(&mut self.config.fps)
                                            .range(1..=240)
                                            .suffix(" FPS"),
                                    );
                                });
                        });

                        // Bitrate
                        row(ui, "Bitrate", |ui| {
                            integer_slider(
                                ui,
                                &mut self.config.bitrate_mbps,
                                1..=100,
                                " Mbps",
                                false,
                            );
                        });

                        // Cursor
                        toggle_row(ui, "Record cursor", &mut self.config.show_cursor);

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // System audio
                        toggle_row(ui, "System audio", &mut self.config.system_audio);
                        if self.config.system_audio {
                            row(ui, "Volume", |ui| {
                                volume_slider(
                                    ui,
                                    &mut self.config.system_volume,
                                    self.audio_levels[0],
                                    None,
                                );
                            });
                        }

                        ui.add_space(4.0);

                        // Microphone toggle
                        toggle_row(ui, "Microphone", &mut self.config.microphone);
                        if self.config.microphone {
                            // A muted mic records silence; the meter shows
                            // that rather than the raw pre-mute signal.
                            let mic_peak = if self.shared.mic_muted.load(Ordering::Relaxed) {
                                0.0
                            } else {
                                self.audio_levels[1]
                            };
                            row(ui, "Volume", |ui| {
                                volume_slider(
                                    ui,
                                    &mut self.config.microphone_volume,
                                    mic_peak,
                                    self.config.microphone_threshold_db,
                                );
                            });

                            // Noise suppression
                            toggle_row(
                                ui,
                                "Noise suppression",
                                &mut self.config.microphone_noise_suppression,
                            );
                            if self.config.microphone_noise_suppression {
                                row(ui, "Engine", |ui| {
                                    ui.selectable_value(
                                        &mut self.config.microphone_noise_method,
                                        NoiseSuppressionMethod::Rnnoise,
                                        "RNNoise",
                                    );
                                    ui.selectable_value(
                                        &mut self.config.microphone_noise_method,
                                        NoiseSuppressionMethod::Speex,
                                        "Speex",
                                    );
                                });
                                // RNNoise has no strength knob; SpeexDSP
                                // takes a maximum attenuation in dB.
                                if self.config.microphone_noise_method
                                    == NoiseSuppressionMethod::Speex
                                {
                                    row(ui, "Suppression", |ui| {
                                        let track_width = (ui.available_width()
                                            - VALUE_WIDTH
                                            - ui.spacing().item_spacing.x)
                                            .max(60.0);
                                        let slider = ui.scope(|ui| {
                                            ui.spacing_mut().slider_width = track_width;
                                            ui.add(
                                                egui::Slider::new(
                                                    &mut self
                                                        .config
                                                        .microphone_suppression_db,
                                                    -60..=0,
                                                )
                                                .show_value(false),
                                            )
                                        });
                                        ui.add_sized(
                                            [VALUE_WIDTH, CONTROL_HEIGHT],
                                            egui::DragValue::new(
                                                &mut self.config.microphone_suppression_db,
                                            )
                                            .range(-60..=0)
                                            .suffix(" dB"),
                                        );
                                        slider.inner
                                            .on_hover_text("Lower values suppress more noise.");
                                    });
                                }
                            }
                        }

                        ui.add_space(4.0);

                        // Separate audio tracks
                        toggle_row(
                            ui,
                            "Separate audio tracks",
                            &mut self.config.separate_audio_tracks,
                        )
                        .on_hover_text(
                            "Record system audio and microphone into separate tracks.",
                        );
                    });

                    // -------------------------------------------------------
                    // RIGHT column — App & Capture
                    // -------------------------------------------------------
                    settings_card(&mut cols[1], card_h, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.spacing_mut().button_padding.y = 5.0;

                        section_title(ui, "App & Capture");

                        // Duration limit
                        row(ui, "Duration", |ui| {
                            integer_slider(
                                ui,
                                &mut self.config.duration_minutes,
                                0..=1440,
                                " min",
                                true,
                            );
                        });

                        // Output directory
                        row(ui, "Save to", |ui| {
                            let path = self.config.output_dir.to_string_lossy();
                            // Path label: fills available width minus Browse button
                            let path_w = (ui.available_width()
                                - VALUE_WIDTH
                                - ui.spacing().item_spacing.x)
                                .max(50.0);
                            ui.add_sized(
                                [path_w, CONTROL_HEIGHT],
                                egui::Label::new(
                                    RichText::new(path.as_ref())
                                        .size(12.0)
                                        .color(MUTED),
                                )
                                .truncate()
                                .halign(Align::Min),
                            )
                            .on_hover_text(path.as_ref());
                            if ui
                                .add_sized(
                                    [VALUE_WIDTH, CONTROL_HEIGHT],
                                    Button::new("Browse"),
                                )
                                .clicked()
                            {
                                if let Some(folder) = rfd::FileDialog::new()
                                    .set_directory(&self.config.output_dir)
                                    .pick_folder()
                                {
                                    self.config.output_dir = folder;
                                }
                            }
                        });

                        // Filename template
                        row(ui, "Filename", |ui| {
                            let now = chrono::Local::now().naive_local();
                            let example = format!(
                                "{}.mp4",
                                crate::filename::render(
                                    &self.config.filename_template,
                                    now,
                                    self.config.monitor_index,
                                )
                            );
                            ui.add_sized(
                                [ui.available_width(), CONTROL_HEIGHT],
                                egui::TextEdit::singleline(
                                    &mut self.config.filename_template,
                                )
                                .align(egui::Align2::CENTER_CENTER)
                                .char_limit(120)
                                .hint_text(crate::filename::DEFAULT_TEMPLATE),
                            )
                            .on_hover_text(format!("Example: {example}"));
                        });

                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // Start in the notification area (no window on launch)
                        toggle_row(ui, "Start hidden", &mut self.config.hide_window)
                            .on_hover_text(
                                "Launch straight to the notification area instead of showing this window.",
                            );

                        // Force GDI capture
                        toggle_row(ui, "Force GDI capture", &mut self.config.force_gdi)
                            .on_hover_text(
                                "Use the legacy GDI capture path instead of Windows Graphics Capture. Enable if WGC causes issues.",
                            );

                        // Diagnostic logging
                        toggle_row(ui, "Diagnostic logs", &mut self.config.logging_enabled)
                            .on_hover_text(
                                "Write featherweight.log next to the app.",
                            );

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // Shortcuts — global start/stop hotkeys, polled even
                        // while this window is hidden. Click a row to rebind:
                        // the next key press becomes the binding, Esc or
                        // clicking the row again cancels.
                        section_title(ui, "Shortcuts");
                        ui.label(
                            RichText::new(
                                "Work anywhere — even while this window is hidden or another \
                                 app has focus.",
                            )
                            .size(12.0)
                            .color(MUTED),
                        );
                        ui.add_space(4.0);

                        // Consume key events while a row is being rebound,
                        // before the buttons render this frame.
                        self.poll_rebind_input(ui.ctx());

                        for (slot, name) in
                            [(RebindSlot::Start, "Start"), (RebindSlot::Stop, "Stop")]
                        {
                            row(ui, name, |ui| {
                                let editing = self.rebinding == Some(slot);
                                let label = if editing {
                                    "Press keys…".to_owned()
                                } else {
                                    slot.get(self.config.shortcuts).label()
                                };
                                let button = ui.add_sized(
                                    [ui.available_width(), CONTROL_HEIGHT],
                                    Button::new(label),
                                );
                                if button.clicked() {
                                    if editing {
                                        // Click the active row again to cancel.
                                        self.end_rebinding();
                                    } else {
                                        // Suspend the global poller first: while
                                        // capturing, the new keys must not fire
                                        // the current bindings.
                                        self.shared.hotkeys.suspend(true);
                                        self.rebinding = Some(slot);
                                        self.rebind_note = None;
                                    }
                                }
                            });
                        }

                        if let Some((note, error)) = &self.rebind_note {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(note.clone())
                                    .size(12.0)
                                    .color(if *error { widgets::ERROR } else { MUTED }),
                            );
                        } else if self.rebinding.is_some() {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(
                                    "Press the new key combination, then release. Esc cancels.",
                                )
                                .size(12.0)
                                .color(MUTED),
                            );
                        }
                    });
                });
            });
    }

    /// Keeps the idle audio meters alive.
    ///
    /// Runs a lightweight WASAPI preview (loopback and/or microphone) that
    /// measures live signal peaks while no recording is active, restarting
    /// it whenever the enabled sources change and when the window becomes
    /// visible again after being hidden. Levels are raw (pre-gain); the
    /// volume sliders apply the gain themselves, so a meter shows what the
    /// recording would capture at the current slider position.
    fn update_window_and_audio(&mut self, _ctx: &egui::Context) {
        let signature = (self.config.system_audio, self.config.microphone);
        if self.preview_signature != signature {
            self.preview_signature = signature;
            self.audio_preview = None;
            if signature.0 || signature.1 {
                match AudioPreview::start(signature.0, signature.1) {
                    Ok(preview) => self.audio_preview = Some(preview),
                    Err(error) => {
                        crate::log::log(&format!("audio preview unavailable: {error}"));
                        self.warning = Some(format!("audio preview unavailable: {error}"));
                    }
                }
            }
        }
        self.audio_levels = self
            .audio_preview
            .as_ref()
            .map_or([0.0; 2], |preview| preview.levels());
    }
}

/// Recording-bar readout: "86.4 MB · 1.4 MB/s · RAM 214 MB", degrading to
/// just the pieces that have been measured, and to a placeholder before the
/// encoder has written anything.
fn stats_readout(stats: &SessionStats) -> String {
    let mut parts: Vec<String> = Vec::new();
    if stats.file_size > 0 {
        let size = crate::disk::format_bytes(stats.file_size);
        match stats.growth_bps {
            Some(rate) => parts.push(format!("{size} · {}", crate::disk::format_rate(rate))),
            None => parts.push(size),
        }
    }
    if let Some(ram) = stats.ram_bytes {
        parts.push(format!("RAM {}", crate::disk::format_bytes(ram)));
    }
    if parts.is_empty() {
        "starting…".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// Draws the first-frame still fit into the available space preserving its
/// aspect ratio, or a bordered placeholder explaining why there is nothing
/// to show yet.
fn draw_still(
    ui: &mut egui::Ui,
    thumb: Option<&egui::TextureHandle>,
    placeholder_title: &str,
    placeholder_detail: &str,
) {
    let Some(texture) = thumb else {
        // Placeholder sized like a 16:9 still.
        let width = ui.available_width();
        let height = (width * 9.0 / 16.0)
            .min((ui.available_height() - 4.0).max(90.0))
            .max(90.0);
        egui::Frame::new()
            .fill(Color32::from_rgb(24, 28, 34))
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_min_size(Vec2::new(width, height));
                ui.vertical_centered(|ui| {
                    ui.add_space((height * 0.3).min(60.0));
                    ui.label(RichText::new(placeholder_title).size(14.0).color(MUTED));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(placeholder_detail)
                            .size(11.5)
                            .color(MUTED),
                    );
                });
            });
        return;
    };

    // Fit within the available space, preserving the aspect ratio.
    let [tex_w, tex_h] = texture.size();
    let aspect = tex_w.max(1) as f32 / tex_h.max(1) as f32;
    let max_w = ui.available_width();
    let max_h = (ui.available_height() - 4.0).max(90.0);
    let mut size = Vec2::new(max_w, max_w / aspect);
    if size.y > max_h {
        size = Vec2::new(max_h * aspect, max_h);
    }
    ui.add(
        Image::new(texture)
            .fit_to_exact_size(size)
            .corner_radius(6.0),
    );
}

// ---------------------------------------------------------------------------
// eframe::App impl
// ---------------------------------------------------------------------------

impl eframe::App for FeatherweightApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Track whether any config field changed this frame so we save once
        // at the end rather than once per widget interaction.
        let config_before = format!("{:?}", self.config);

        // ------------------------------------------------------------------
        // Start-in-tray. `with_visible(false)` is only a hint to the windowing
        // layer, so make it true on the first frame: one explicit hide, then
        // never again (the user can show the window from here on).
        // ------------------------------------------------------------------
        if self.hide_on_first_frame {
            self.hide_on_first_frame = false;
            self.set_visible(ui.ctx(), false);
        }

        // ------------------------------------------------------------------
        // Pull tray-made preference changes (checkbox clicks made while the
        // window was hidden wrote the shared snapshot; adopt them so the UI
        // reflects them). Counted as a config change below → saved.
        // ------------------------------------------------------------------
        if self.shared.prefs_dirty.swap(false, Ordering::AcqRel) {
            let snapshot = self.shared.config.lock().unwrap().clone();
            self.config.microphone = snapshot.microphone;
            self.config.separate_audio_tracks = snapshot.separate_audio_tracks;
            self.config.force_gdi = snapshot.force_gdi;
        }

        // ------------------------------------------------------------------
        // Single-instance surfacing: a second launch flagged the hub; show
        // and focus this window whether hidden, minimized, or merely behind
        // others (Visible + Focus through set_visible).
        // ------------------------------------------------------------------
        if self.shared.show_requested.swap(false, Ordering::AcqRel) {
            self.set_visible(ui.ctx(), true);
        }

        // ------------------------------------------------------------------
        // Tray event polling — handle toggle and quit from the system tray
        // ------------------------------------------------------------------
        for event in self.tray.poll_events() {
            match event {
                TrayEvent::Toggle => {
                    let next = !self.window_visible;
                    self.set_visible(ui.ctx(), next);
                }
                TrayEvent::Stop => {
                    // Non-Windows path: the Windows handler sets the flag
                    // directly. Same shared signal either way.
                    self.stop_recording();
                }
                TrayEvent::Quit => {
                    // quit_requested is already set inside poll_events;
                    // just close the window.
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // ------------------------------------------------------------------
        // Window-close interception: X button hides rather than quits
        // ------------------------------------------------------------------
        let close_requested = ui.ctx().input(|i| i.viewport().close_requested());
        if close_requested {
            if self.tray.is_quit_requested() {
                // Real quit — let it fall through so the event loop exits.
            } else {
                // Hide instead of close.
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.set_visible(ui.ctx(), false);
            }
        }

        // ------------------------------------------------------------------
        // Idle audio meters: start/restart the preview as needed and read
        // the latest peaks for the volume sliders.
        // ------------------------------------------------------------------
        self.update_window_and_audio(ui.ctx());

        // ------------------------------------------------------------------
        // Render
        // ------------------------------------------------------------------
        // Push the current session state into the tray (icon, enable rules,
        // status line). Cheap no-op unless something changed.
        self.tray.sync_session();

        // Keep every view live: 1 s cadence for the readouts, ~20 Hz while
        // the idle audio meters run. Lives here — not in a view — so it
        // applies to all views.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        if self.audio_preview.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        // While a session runs, the Recording tab carries a red ● and the
        // live elapsed time; and an Idle→active edge from a tray or hotkey
        // start (while the window is visible) lands on the Recording view.
        // A hidden window runs no frames, so this only fires when the user
        // can see it.
        let status = self.shared.status.lock().unwrap().clone();
        let active = !matches!(status, SessionStatus::Idle);
        if active && !self.was_active {
            self.view = View::Recording;
        }
        self.was_active = active;

        egui::Panel::top("tabs")
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (view, label) in [
                        (View::Recording, "Recording"),
                        (View::Settings, "Settings"),
                        (View::About, "About"),
                    ] {
                        let selected = self.view == view;
                        let text = if view == View::Recording {
                            match &status {
                                SessionStatus::Recording { elapsed_secs, .. } => RichText::new(
                                    format!(
                                        "● Recording {}",
                                        crate::tray::format_duration(*elapsed_secs)
                                    ),
                                )
                                .color(RED),
                                _ => {
                                    RichText::new(label).color(if selected { TEXT } else { MUTED })
                                }
                            }
                        } else {
                            RichText::new(label).color(if selected { TEXT } else { MUTED })
                        };
                        if ui
                            .add(Button::new(text.size(13.0)).selected(selected))
                            .clicked()
                        {
                            self.view = view;
                        }
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20, 16)),
            )
            .show(ui, |ui| {
                match self.view {
                    View::Recording => self.render_recording(ui),
                    View::Settings => self.render_settings(ui),
                    View::About => self.about.render(ui),
                }
            });

        // Save config if anything changed, and mirror it into the shared
        // snapshot so tray actions see the same values.
        if format!("{:?}", self.config) != config_before {
            self.save_config();
            *self.shared.config.lock().unwrap() = self.config.clone();
        }
    }
}
