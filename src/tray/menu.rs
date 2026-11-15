//! Tray menu construction — follows the production menu spec:
//!
//!   [disabled]  <status line>
//!   -----------
//!   Open / Hide Featherweight                (window toggle)
//!   -----------
//!   Stop recording                           (enabled only while recording)
//!   New recording…                           (enabled only while stopped)
//!   Open recordings folder
//!   -----------
//!   [checkbox]  Record microphone track (next recording)
//!   [checkbox]  Separate audio tracks (System & Mic)
//!   [checkbox]  Force GDI capture (compatibility mode)
//!   [item]      Mute microphone              (enabled only while recording with mic)
//!   -----------
//!   Quit
//!
//! Wiring:
//! - Open/Hide (Toggle), Stop recording, and Quit are handled by the caller
//!   (`tray::Tray`), because they need window/process control.
//! - Everything else is handled by [`TrayMenu::handle`], which acts through
//!   the shared session hub (`app::session::SessionShared`): starts
//!   sessions, opens the recordings folder, writes preference changes into
//!   the config snapshot (the window pulls them back and persists), and
//!   toggles the mic mute flag. Preference persistence happens through the
//!   config's own `save()` (settings.json).
//!
//! On Windows, menu events are handled by `MenuEvent::set_event_handler`
//! (installed in `Tray::new`), because the egui update loop does not run
//! while the window is hidden. On other platforms they are polled via
//! `MenuEvent::receiver()`.

use std::error::Error;
use std::path::Path;
use std::sync::{Arc, atomic::Ordering};

use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

use crate::app::session::SessionShared;
use crate::config::AppConfig;

pub const ID_STATUS: &str = "status";
pub const ID_TOGGLE: &str = "toggle";
pub const ID_STOP: &str = "stop";
pub const ID_NEW: &str = "new";
pub const ID_OPEN_FOLDER: &str = "open_folder";
pub const ID_MIC_TRACK: &str = "mic_track";
pub const ID_SEPARATE_TRACKS: &str = "separate_tracks";
pub const ID_FORCE_GDI: &str = "force_gdi";
pub const ID_MUTE_MIC: &str = "mute_mic";
pub const ID_QUIT: &str = "quit";

/// Toggle label while the window is hidden.
pub const LABEL_OPEN: &str = "Open Geliana Featherweight";
/// Toggle label while the window is visible.
pub const LABEL_HIDE: &str = "Hide Geliana Featherweight";

/// Loudly flag a feature that is wired into the UI but not implemented yet.
/// Prints to the console and writes to the log file.
pub fn todo_feature(feature: &str, detail: &str) {
    let msg = format!("[NOT IMPLEMENTED] {feature} -- {detail}");
    println!("{msg}");
    crate::log::log(&msg);
}

/// Handles to every menu item the app needs to update or read.
#[derive(Clone)]
pub struct TrayMenu {
    pub status_item: MenuItem,
    pub toggle_item: MenuItem,
    pub stop_item: MenuItem,
    pub new_item: MenuItem,
    pub open_folder_item: MenuItem,
    /// Persisted in settings.ini (persistence not implemented yet).
    pub mic_track_item: CheckMenuItem,
    /// Persisted in settings.ini (persistence not implemented yet).
    pub separate_audio_tracks_item: CheckMenuItem,
    /// Persisted in settings.ini (persistence not implemented yet).
    pub force_gdi_item: CheckMenuItem,
    pub mute_mic_item: MenuItem,
    pub quit_item: MenuItem,
}

impl TrayMenu {
    /// Builds the menu. `recording` is true when starting already recording.
    pub fn build(
        recording: bool,
        mic_track_enabled: bool,
        separate_audio_tracks_enabled: bool,
        force_gdi_enabled: bool,
    ) -> Result<(Menu, Self), Box<dyn Error + Send + Sync>> {
        let status_item = MenuItem::with_id(ID_STATUS, "Geliana Featherweight - idle", false, None);
        // Initial label reflects the app's default hidden-at-start state.
        let toggle_item = MenuItem::with_id(ID_TOGGLE, LABEL_OPEN, true, None);
        let stop_item = MenuItem::with_id(ID_STOP, "Stop recording", recording, None);
        let new_item = MenuItem::with_id(ID_NEW, "New recording…", !recording, None);
        let open_folder_item =
            MenuItem::with_id(ID_OPEN_FOLDER, "Open recordings folder", true, None);
        let mic_track_item = CheckMenuItem::with_id(
            ID_MIC_TRACK,
            "Record microphone track (next recording)",
            true,
            mic_track_enabled,
            None,
        );
        let separate_audio_tracks_item = CheckMenuItem::with_id(
            ID_SEPARATE_TRACKS,
            "Separate audio tracks (System & Mic)",
            true,
            separate_audio_tracks_enabled,
            None,
        );
        let force_gdi_item = CheckMenuItem::with_id(
            ID_FORCE_GDI,
            "Force GDI capture (compatibility mode)",
            true,
            force_gdi_enabled,
            None,
        );
        // Enabled only during a mic-enabled recording.
        let mute_mic_item = MenuItem::with_id(ID_MUTE_MIC, "Mute microphone", false, None);
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);

        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&toggle_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&stop_item)?;
        menu.append(&new_item)?;
        menu.append(&open_folder_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&mic_track_item)?;
        menu.append(&separate_audio_tracks_item)?;
        menu.append(&force_gdi_item)?;
        menu.append(&mute_mic_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_item)?;

        let items = TrayMenu {
            status_item,
            toggle_item,
            stop_item,
            new_item,
            open_folder_item,
            mic_track_item,
            separate_audio_tracks_item,
            force_gdi_item,
            mute_mic_item,
            quit_item,
        };
        Ok((menu, items))
    }

    /// Applies the enabled/disabled rules for the recording state.
    pub fn set_recording(&self, recording: bool, mic_track_on: bool) {
        self.stop_item.set_enabled(recording);
        self.new_item.set_enabled(!recording);
        self.mute_mic_item.set_enabled(recording && mic_track_on);
    }

    /// Handles every item except Toggle, Stop, and Quit (those need
    /// window/process control and are handled by the caller). Returns `true`
    /// if `id` was recognised.
    ///
    /// Runs on the main thread (menu handler on Windows, poll loop
    /// elsewhere). All actions go through the shared session hub; tray-icon
    /// calls are safe here.
    pub fn handle(&self, id: &str, shared: &Arc<SessionShared>) -> bool {
        match id {
            ID_NEW => {
                let config = shared.config.lock().unwrap().clone();
                if !shared.start_recording(config) {
                    crate::log::log(
                        "tray: 'New recording' ignored; a session is already running",
                    );
                }
            }
            ID_OPEN_FOLDER => {
                let dir = shared.config.lock().unwrap().output_dir.clone();
                if let Err(e) = open_folder(&dir) {
                    crate::log::log(&format!("tray: could not open recordings folder: {e}"));
                }
            }
            ID_MIC_TRACK => {
                let checked = self.mic_track_item.is_checked();
                set_preference(shared, |config| config.microphone = checked);
            }
            ID_SEPARATE_TRACKS => {
                let checked = self.separate_audio_tracks_item.is_checked();
                set_preference(shared, |config| config.separate_audio_tracks = checked);
            }
            ID_FORCE_GDI => {
                let checked = self.force_gdi_item.is_checked();
                set_preference(shared, |config| config.force_gdi = checked);
            }
            ID_MUTE_MIC => {
                let muted = !shared.mic_muted.load(Ordering::Relaxed);
                shared.mic_muted.store(muted, Ordering::Relaxed);
                crate::log::log(&format!(
                    "tray: microphone {}",
                    if muted { "muted" } else { "unmuted" }
                ));
            }
            _ => return false,
        }
        true
    }
}

/// Writes a tray-made preference change into the config snapshot, persists
/// it to disk, and flags the window to pull the field back into its
/// editable copy on the next frame.
fn set_preference(shared: &Arc<SessionShared>, apply: impl FnOnce(&mut AppConfig)) {
    {
        let mut config = shared.config.lock().unwrap();
        apply(&mut config);
        if let Err(e) = config.save() {
            crate::log::log(&format!("tray: could not persist setting: {e}"));
        }
    }
    shared.prefs_dirty.store(true, Ordering::Release);
}

/// Opens `dir` in the platform file manager, creating it first if needed.
///
/// Shared with the Recording view's Open folder button, which opens the
/// current session's parent directory.
pub(crate) fn open_folder(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create folder: {e}"))?;

    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(not(windows))]
    let spawned = std::process::Command::new("xdg-open").arg(dir).spawn();

    spawned
        .map(|_| ())
        .map_err(|e| format!("could not launch file manager: {e}"))
}

/// Opens a file with its default associated application.
///
/// Used by the Recording view's past-recordings list to play a finished
/// capture. Windows hands the path to Explorer, which launches the default
/// handler; other platforms use xdg-open.
pub(crate) fn open_file(file: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let spawned = std::process::Command::new("explorer").arg(file).spawn();
    #[cfg(not(windows))]
    let spawned = std::process::Command::new("xdg-open").arg(file).spawn();

    spawned
        .map(|_| ())
        .map_err(|e| format!("could not open recording: {e}"))
}