//! System tray icon and menu for Featherweight.
//!
//! [`Tray`] owns the tray icon and the menu items from [`menu::TrayMenu`].
//!
//! # Event handling
//!
//! While the window is hidden, winit delivers no redraws, so egui's `update`
//! never runs and anything polled there is delayed until the window is shown.
//! On Windows the tray therefore installs `set_event_handler` callbacks that
//! act immediately (Win32 show/hide, quit) without waiting for a frame.
//!
//! Once a handler is set, `TrayIconEvent::receiver()` and
//! `MenuEvent::receiver()` no longer receive events, so [`Tray::poll_events`]
//! only yields events on non-Windows platforms.
//!
//! Unimplemented features log `[NOT IMPLEMENTED] ...` to the console and log
//! file (see [`menu::todo_feature`]).

pub mod menu;

use std::cell::RefCell;
use std::error::Error;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use tray_icon::menu::MenuEvent;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::session::{SessionShared, SessionStats, StopReason};
use crate::app::theme::{self, AMBER, RED};
use crate::capture::recorder::SessionStatus;
use crate::disk::format_bytes;
use menu::{
    ID_QUIT, ID_STOP, ID_TOGGLE, LABEL_HIDE, LABEL_OPEN, TrayMenu, todo_feature,
};

// ---------------------------------------------------------------------------
// Tray event — returned by poll_events (non-Windows)
// ---------------------------------------------------------------------------

/// An event decoded from the tray menu that the app needs to act on.
pub enum TrayEvent {
    /// User clicked the open/hide toggle.
    Toggle,
    /// User clicked Stop recording. The receiver should signal the shared
    /// stop flag (only emitted on non-Windows; on Windows the handler sets
    /// the flag directly).
    Stop,
    /// User clicked Quit.
    Quit,
}

// ---------------------------------------------------------------------------
// Icon state
// ---------------------------------------------------------------------------

/// Which tray icon to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Idle.
    Stopped,
    /// Recording (red).
    Recording,
    /// Recording with low disk space (amber).
    DiskWarn,
}

// ---------------------------------------------------------------------------
// Tray struct
// ---------------------------------------------------------------------------

pub struct Tray {
    /// Keeping this alive keeps the system tray icon alive.
    icon: TrayIcon,
    menu: TrayMenu,
    /// Set to `true` once the quit menu item is clicked.
    quit_requested: Arc<AtomicBool>,
    /// The shared session hub: recording status, config snapshot, stop flag,
    /// mic flags. Read by [`Tray::sync_session`] and the menu handlers;
    /// written by the event pump and tray actions. (Reads only here — the
    /// tray never mutates session state except through menu actions, which
    /// run on this thread.)
    shared: Arc<SessionShared>,
    /// What [`Tray::sync_session`] last applied, to avoid redundant shell
    /// calls every frame. Main-thread only, hence RefCell.
    applied: RefCell<SessionApplied>,
}

/// What the sync loop last pushed to the OS. Tray-icon calls are not free —
/// each menu `set_text` rebuilds the Win32 menu and each `set_icon`/tooltip is
/// a `Shell_NotifyIcon` call — so the view is compared before anything is
/// applied. Main-thread only, hence `RefCell`.
struct SessionApplied {
    view: Option<SessionView>,
    mic_track_checked: Option<bool>,
    separate_tracks_checked: Option<bool>,
    force_gdi_checked: Option<bool>,
}

impl Tray {
    /// Builds the tray. Must be called from the eframe creation closure so
    /// the window handle and egui context are available to the handlers.
    ///
    /// `shared` is the app's session hub (see `app::session`): the tray reads
    /// recording status and the config snapshot from it, its Stop handler
    /// sets `stop_flag`, its preference checkboxes write the config snapshot,
    /// and its "New recording" starts sessions. All on the main thread.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<SessionShared>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let quit_requested = Arc::new(AtomicBool::new(false));

        // Seed the menu from the shared config/status.
        let (recording, mic_track, separate_tracks, force_gdi) = {
            let status = shared.status.lock().unwrap();
            let config = shared.config.lock().unwrap();
            (
                matches!(
                    *status,
                    SessionStatus::Recording { .. } | SessionStatus::WaitingForFirstFrame { .. }
                ),
                config.microphone,
                config.separate_audio_tracks,
                config.force_gdi,
            )
        };
        let (menu, items) = TrayMenu::build(recording, mic_track, separate_tracks, force_gdi)?;

        // The idle icon is the fixed monochrome glyph; recording states tint
        // it (see `icon_for`). Picking a variant from AppsUseLightTheme is
        // still unimplemented.
        todo_feature(
            "Tray icon theme awareness",
            "the idle icon is fixed monochrome; follow AppsUseLightTheme to pick a dark/light variant",
        );
        let icon = icon_for(IconState::Stopped)?;

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("Geliana Featherweight")
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .build()?;

        #[cfg(windows)]
        win::install_handlers(cc, &tray_icon, &items, quit_requested.clone(), shared.clone());
        #[cfg(not(windows))]
        let _ = cc;

        Ok(Self {
            icon: tray_icon,
            menu: items,
            quit_requested,
            shared,
            applied: RefCell::new(SessionApplied {
                view: None,
                mic_track_checked: None,
                separate_tracks_checked: None,
                force_gdi_checked: None,
            }),
        })
    }

    /// Re-applies session-dependent tray visuals if anything changed since
    /// the last call. Call once per UI frame; cheap no-op when nothing
    /// changed.
    ///
    /// Covers recording state (icon, enable rules, status line) and the
    /// preference checkboxes (so UI-made config changes are mirrored into
    /// the tray). While the window is hidden no frames run — the menu
    /// handlers apply the current state directly instead.
    pub fn sync_session(&self) {
        let status = self.shared.status.lock().unwrap().clone();
        let stats = self.shared.stats.lock().unwrap().clone();
        let stop_reason = self.shared.stop_reason.lock().unwrap().clone();
        let mic = self.shared.mic_track_on.load(Ordering::Relaxed);
        let mic_muted = self.shared.mic_muted.load(Ordering::Relaxed);
        let (mic_checked, sep_checked, gdi_checked) = {
            let config = self.shared.config.lock().unwrap();
            (config.microphone, config.separate_audio_tracks, config.force_gdi)
        };

        let view = SessionView::new(&status, &stats, mic, mic_muted, stop_reason.as_ref());

        let mut applied = self.applied.borrow_mut();

        if applied.view.as_ref() != Some(&view) {
            apply_session_state(&self.menu, &self.icon, &view);
            applied.view = Some(view);
        }

        // Mirror config-driven checkbox states (UI changes land here).
        if applied.mic_track_checked != Some(mic_checked) {
            self.menu.mic_track_item.set_checked(mic_checked);
            applied.mic_track_checked = Some(mic_checked);
        }
        if applied.separate_tracks_checked != Some(sep_checked) {
            self.menu.separate_audio_tracks_item.set_checked(sep_checked);
            applied.separate_tracks_checked = Some(sep_checked);
        }
        if applied.force_gdi_checked != Some(gdi_checked) {
            self.menu.force_gdi_item.set_checked(gdi_checked);
            applied.force_gdi_checked = Some(gdi_checked);
        }
    }

    /// Returns `true` if the Quit menu item has been clicked.
    pub fn is_quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::SeqCst)
    }

    /// Updates the toggle item label to match the current window visibility.
    pub fn set_window_visible(&self, visible: bool) {
        self.menu
            .toggle_item
            .set_text(if visible { LABEL_HIDE } else { LABEL_OPEN });
    }

    /// Drains pending tray menu events (non-Windows; on Windows the handler
    /// consumes them first, so this returns an empty list).
    pub fn poll_events(&self) -> Vec<TrayEvent> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                ID_TOGGLE => out.push(TrayEvent::Toggle),
                ID_STOP => out.push(TrayEvent::Stop),
                ID_QUIT => {
                    self.quit_requested.store(true, Ordering::SeqCst);
                    out.push(TrayEvent::Quit);
                }
                other => {
                    if !self.menu.handle(other, &self.shared) {
                        crate::log::log(&format!("Tray: unknown menu id '{other}'"));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Session-state visuals
// ---------------------------------------------------------------------------

/// Everything the tray renders from the current session, resolved once so the
/// "has anything changed" comparison and the apply itself cannot disagree.
#[derive(Clone, PartialEq)]
pub(crate) struct SessionView {
    /// A session is running.
    pub recording: bool,
    /// The running session has a mic track (drives the Mute item's enable
    /// state).
    pub mic_on: bool,
    /// Free space is low: amber icon and "Low disk" prefix.
    pub disk_warn: bool,
    /// Disabled status line, shown in the menu and as the tooltip.
    pub line: String,
}

impl SessionView {
    fn new(
        status: &SessionStatus,
        stats: &SessionStats,
        mic_on: bool,
        mic_muted: bool,
        stop_reason: Option<&StopReason>,
    ) -> Self {
        let recording = matches!(
            status,
            SessionStatus::Recording { .. } | SessionStatus::WaitingForFirstFrame { .. }
        );
        let line = match status {
            SessionStatus::Recording { elapsed_secs, .. } => recording_status(
                *elapsed_secs,
                stats.file_size,
                stats.ram_bytes,
                stats.free_bytes,
                stats.eta_secs.map(|secs| secs as f64 / 60.0),
                stats.disk_warn,
                // The pipeline does not report which encoder it negotiated
                // (Media Foundation picks hardware or software internally), so
                // naming one here would be a guess. Empty = leave it out.
                "",
                "MP4",
                mic_state(mic_on, mic_muted),
            ),
            SessionStatus::WaitingForFirstFrame { target } => waiting_status(target),
            SessionStatus::Idle => idle_status(stop_reason),
        };
        Self {
            recording,
            mic_on,
            disk_warn: recording && stats.disk_warn,
            line,
        }
    }
}

/// Builds the waiting status line shown in the menu and as the tooltip while
/// the capture target has not delivered its first frame, truncated to 127
/// characters (Windows tooltip cap).
///
/// Example: "Geliana Featherweight - waiting for Game 🎮…"
fn waiting_status(target: &str) -> String {
    let target = if target.is_empty() {
        "the selected window"
    } else {
        target
    };
    format!("Geliana Featherweight - waiting for {target}…")
        .chars()
        .take(127)
        .collect()
}

/// Maps the hub's mic flags to the status-line wording.
fn mic_state(mic_on: bool, mic_muted: bool) -> MicState {    if !mic_on {
        MicState::Off
    } else if mic_muted {
        MicState::Muted
    } else {
        MicState::Live
    }
}

/// Idle line: the plain app name, plus why the last session ended. A low-disk
/// stop is otherwise invisible to a tray-only user who never reopens the window.
fn idle_status(stop_reason: Option<&StopReason>) -> String {
    match stop_reason.filter(|reason| reason.is_notable()) {
        Some(reason) => format!("Geliana Featherweight - idle · stopped: {}", reason.short_label()),
        None => "Geliana Featherweight - idle".to_owned(),
    }
}

/// Applies a [`SessionView`] to the menu (enable rules + status line), the icon
/// and the tooltip.
///
/// **Main thread only** — all tray-icon types are main-thread-bound on Windows.
/// Called from [`Tray::sync_session`] (UI frames) and from the Windows handlers
/// (menu events, icon hover), which run on the main thread even while the
/// window is hidden, so opening the menu always shows the current state.
pub(crate) fn apply_session_state(menu: &TrayMenu, icon: &TrayIcon, view: &SessionView) {
    menu.set_recording(view.recording, view.mic_on);
    menu.status_item.set_text(&view.line);

    let state = match (view.recording, view.disk_warn) {
        (true, false) => IconState::Recording,
        (true, true) => IconState::DiskWarn,
        (false, _) => IconState::Stopped,
    };
    match icon_for(state) {
        Ok(new_icon) => {
            let _ = icon.set_icon(Some(new_icon));
        }
        Err(err) => crate::log::log(&format!("tray: could not build {state:?} icon: {err}")),
    }

    // Idle keeps the plain app name; a running session's tooltip carries the
    // same numbers as the menu line.
    let tooltip = if view.recording {
        view.line.as_str()
    } else {
        "Featherweight"
    };
    let _ = icon.set_tooltip(Some(tooltip));
}

// ---------------------------------------------------------------------------
// Windows: immediate handlers that don't depend on egui frames
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::*;
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// Wrapper so main-thread-only tray objects can be captured by the
    /// `Send + Sync` handler closures.
    ///
    /// Sound here because tray-icon dispatches these handlers from the window
    /// procedure on the thread that owns the event loop, which is also the
    /// thread that created the objects.
    struct MainThreadOnly<T>(T);

    impl<T> MainThreadOnly<T> {
        // Accessing through a method (not `.0`) makes closures capture the
        // whole wrapper instead of the non-Send field.
        fn get(&self) -> &T {
            &self.0
        }
    }

    unsafe impl<T> Send for MainThreadOnly<T> {}
    unsafe impl<T> Sync for MainThreadOnly<T> {}

    fn show(hwnd: isize) {
        unsafe {
            ShowWindow(hwnd as _, SW_RESTORE);
            SetForegroundWindow(hwnd as _);
        }
    }

    fn hide(hwnd: isize) {
        unsafe {
            ShowWindow(hwnd as _, SW_HIDE);
        }
    }

    fn is_visible(hwnd: isize) -> bool {
        unsafe { IsWindowVisible(hwnd as _) != 0 }
    }

    pub fn install_handlers(
        cc: &eframe::CreationContext<'_>,
        tray: &TrayIcon,
        menu: &TrayMenu,
        quit_requested: Arc<AtomicBool>,
        shared: Arc<SessionShared>,
    ) {
        let hwnd = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(w)) => w.hwnd.get(),
            _ => {
                crate::log::log("Tray: no Win32 window handle, handlers not installed");
                return;
            }
        };

        // Left click shows the window; hovering refreshes the status line and
        // tooltip. While the window is hidden no egui frames run, so the hover
        // is the only moment the tooltip can be brought up to date before the
        // shell displays it.
        {
            let ctx = cc.egui_ctx.clone();
            let menu = MainThreadOnly(menu.clone());
            let tray = MainThreadOnly(tray.clone());
            let shared = shared.clone();
            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        show(hwnd);
                        menu.get().toggle_item.set_text(LABEL_HIDE);
                        ctx.request_repaint();
                    }
                    TrayIconEvent::Enter { .. } => apply_now(&menu, &tray, &shared),
                    _ => {}
                }
            }));
        }

        // Menu items.
        {
            let ctx = cc.egui_ctx.clone();
            let menu = MainThreadOnly(menu.clone());
            let tray = MainThreadOnly(tray.clone());
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                // Apply session visuals first: while the window is hidden no
                // frames run, so this handler is the only chance to show the
                // current recording state. (Runs on the main thread — tray
                // types are safe here.)
                apply_now(&menu, &tray, &shared);
                match event.id.0.as_str() {
                    ID_TOGGLE => {
                        if is_visible(hwnd) {
                            hide(hwnd);
                            menu.get().toggle_item.set_text(LABEL_OPEN);
                        } else {
                            show(hwnd);
                            menu.get().toggle_item.set_text(LABEL_HIDE);
                        }
                        ctx.request_repaint();
                    }
                    ID_STOP => {
                        crate::log::log("tray: stop recording requested");
                        // Same entry point as the window's Stop button, so the
                        // stop reason is recorded either way.
                        shared.request_stop(StopReason::User);
                    }
                    ID_QUIT => {
                        println!("Tray: Quit requested");
                        quit_requested.store(true, Ordering::SeqCst);
                        // Remove the icon first so no ghost is left behind.
                        let _ = tray.get().set_visible(false);
                        std::process::exit(0);
                    }
                    other => {
                        // Everything else (New recording, Open recordings
                        // folder, preference checkboxes, Mute microphone)
                        // lives in TrayMenu::handle.
                        if !menu.get().handle(other, &shared) {
                            crate::log::log(&format!("Tray: unknown menu id '{other}'"));
                        }
                    }
                }
                // Re-apply so state-changing clicks (stop/new/mute/prefs) are
                // reflected immediately in an open menu.
                apply_now(&menu, &tray, &shared);
            }));
        }
    }

    /// Applies the current session state from the shared hub to the tray.
    /// Main thread only.
    fn apply_now(
        menu: &MainThreadOnly<TrayMenu>,
        tray: &MainThreadOnly<TrayIcon>,
        shared: &SessionShared,
    ) {
        let status = shared.status.lock().unwrap();
        let stats = shared.stats.lock().unwrap().clone();
        let stop_reason = shared.stop_reason.lock().unwrap().clone();
        let mic = shared.mic_track_on.load(Ordering::Relaxed);
        let mic_muted = shared.mic_muted.load(Ordering::Relaxed);
        let view = super::SessionView::new(&status, &stats, mic, mic_muted, stop_reason.as_ref());
        super::apply_session_state(menu.get(), tray.get(), &view);
    }
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// The monochrome tray glyph (white feather on transparent background),
/// shared by every state: idle uses it as-is, the recording states tint it.
/// 32 px matches the shell's small-icon size for the notification area.
const TRAY_GLYPH: &[u8] = include_bytes!("../assets/Featherweight_Tray_Monochrome_32.png");

const FALLBACK_SIZE: u32 = 32;

fn icon_for(state: IconState) -> Result<Icon, tray_icon::BadIcon> {
    let tint: Option<[u8; 3]> = match state {
        IconState::Stopped => None,
        IconState::Recording => Some([RED.r(), RED.g(), RED.b()]),
        IconState::DiskWarn => Some([AMBER.r(), AMBER.g(), AMBER.b()]),
    };
    match load_glyph_icon(tint) {
        Some(icon) => Ok(icon),
        // The PNG is compiled into the binary, so a decode failure means the
        // file itself is malformed; a solid disc keeps the tray functional.
        None => fallback_disc(state),
    }
}

/// Builds the tray glyph, optionally tinted.
///
/// The tint colour replaces the glyph's luminance-scaled RGB while the
/// anti-aliased alpha is preserved, so the white feather turns red/amber
/// without losing its soft edges.
fn load_glyph_icon(tint: Option<[u8; 3]>) -> Option<Icon> {
    let (mut rgba, width, height) = theme::decode_png_rgba(TRAY_GLYPH)?;
    if let Some([r, g, b]) = tint {
        for px in rgba.chunks_exact_mut(4) {
            // Glyph is near-white, so its RGB average is the shading factor.
            let luma = (u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) as f32
                / (3.0 * f32::from(u8::MAX));
            px[0] = (f32::from(r) * luma) as u8;
            px[1] = (f32::from(g) * luma) as u8;
            px[2] = (f32::from(b) * luma) as u8;
        }
    }
    Icon::from_rgba(rgba, width, height).ok()
}

/// Solid-colour disc shown only if the embedded glyph cannot be decoded.
fn fallback_disc(state: IconState) -> Result<Icon, tray_icon::BadIcon> {
    let color: [u8; 4] = match state {
        IconState::Stopped => [13, 20, 38, 255],
        IconState::Recording => [RED.r(), RED.g(), RED.b(), 255],
        IconState::DiskWarn => [AMBER.r(), AMBER.g(), AMBER.b(), 255],
    };
    let mut pixels = vec![0u8; (FALLBACK_SIZE * FALLBACK_SIZE * 4) as usize];
    draw_disc(&mut pixels, 16.0, 16.0, 15.0, color);
    Icon::from_rgba(pixels, FALLBACK_SIZE, FALLBACK_SIZE)
}

fn draw_disc(pixels: &mut [u8], cx: f32, cy: f32, radius: f32, color: [u8; 4]) {
    const SIZE: i32 = FALLBACK_SIZE as i32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= radius * radius {
                let index = ((y * SIZE + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&color);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Status line formatting
// ---------------------------------------------------------------------------

/// Mic recording state for the status line.
///
/// Deliberately not a place for encoder state: "software encoding" is a
/// property of the encoder, not the microphone, and would need its own
/// argument when the pipeline can report it.
#[derive(Debug, Clone, Copy)]
pub enum MicState {
    /// Mic track is disabled; don't mention it.
    Off,
    /// Mic track is running.
    Live,
    /// Mic track is muted.
    Muted,
}

/// Format an elapsed duration as HH:MM:SS.
pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Builds the recording status line shown in the menu and tooltip, truncated
/// to 127 characters (Windows tooltip cap).
///
/// Unknown segments are omitted rather than shown as zero, so the line never
/// claims a size, a free-space figure, or a RAM figure the recorder has not
/// measured.
///
/// Example: "Recording 00:12:34 | 3.2 GB | RAM 214 MB | 41.2 GB free | ~54 min left | MP4 | mic muted"
pub fn recording_status(
    elapsed_secs: u64,
    file_size: u64,
    ram_bytes: Option<u64>,
    free_bytes: Option<u64>,
    eta_min: Option<f64>,
    disk_warn: bool,
    encoder_short: &str,
    container: &str,
    mic_state: MicState,
) -> String {
    let mut parts = vec![format!("Recording {}", format_duration(elapsed_secs))];

    // 0 means the encoder has not flushed anything yet.
    if file_size > 0 {
        parts.push(format_bytes(file_size));
    }
    if let Some(ram) = ram_bytes {
        parts.push(format!("RAM {}", format_bytes(ram)));
    }
    if let Some(free) = free_bytes {
        parts.push(format!("{} free", format_bytes(free)));
    }
    // "~0 min left" is noise; the window's banner carries that case.
    if let Some(minutes) = eta_min.filter(|minutes| *minutes >= 1.0) {
        parts.push(format!("~{minutes:.0} min left"));
    }
    if !encoder_short.is_empty() {
        parts.push(encoder_short.to_owned());
        parts.push(container.to_owned());
    }
    match mic_state {
        MicState::Off => {}
        MicState::Live => parts.push("mic live".to_owned()),
        MicState::Muted => parts.push("mic muted".to_owned()),
    }

    let line = parts.join(" | ");
    let line = if disk_warn {
        format!("Low disk | {line}")
    } else {
        line
    };
    line.chars().take(127).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_are_omitted_instead_of_zeroed() {
        let line = recording_status(1, 0, None, None, None, false, "", "MP4", MicState::Off);
        assert_eq!(line, "Recording 00:00:01");
        assert!(!line.contains("0 B"));
    }

    #[test]
    fn full_line_carries_size_ram_free_space_and_eta() {
        let line = recording_status(
            754,
            3_221_225_472,
            Some(224_804_864),
            Some(129_000_000_000),
            Some(54.4),
            false,
            "Quick Sync",
            "MP4",
            MicState::Muted,
        );
        assert_eq!(
            line,
            "Recording 00:12:34 | 3.0 GB | RAM 214.4 MB | 120.1 GB free | ~54 min left | Quick Sync | MP4 | mic muted"
        );
    }

    #[test]
    fn disk_warning_prefixes_the_line() {
        let line = recording_status(5, 1024, None, Some(2048), None, true, "", "MP4", MicState::Off);
        assert!(line.starts_with("Low disk | "), "{line}");
    }

    #[test]
    fn sub_minute_eta_is_dropped() {
        let line = recording_status(5, 1024, None, Some(2048), Some(0.4), false, "", "MP4", MicState::Off);
        assert!(!line.contains("min left"), "{line}");
    }

    #[test]
    fn encoder_and_container_appear_only_when_known() {
        let hidden = recording_status(5, 1024, None, None, None, false, "", "MP4", MicState::Off);
        assert!(!hidden.contains("MP4"), "{hidden}");
        let shown = recording_status(5, 1024, None, None, None, false, "Quick Sync", "MP4", MicState::Off);
        assert!(shown.ends_with("Quick Sync | MP4"), "{shown}");
    }

    #[test]
    fn line_is_capped_for_the_windows_tooltip() {
        let line = recording_status(
            99_999,
            9_999_999_999,
            Some(9_999_999_999),
            Some(9_999_999_999),
            Some(9_999.0),
            true,
            "a very long encoder name",
            "MP4",
            MicState::Live,
        );
        // The cap only bites with very long fields, so check the invariant.
        assert!(line.chars().count() <= 127);
    }
}