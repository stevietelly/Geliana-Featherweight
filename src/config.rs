//! Persistent application settings for the screen recorder.
//!
//! This module owns the [`AppConfig`] structure that is serialized to (and
//! loaded from) a JSON file on disk, the platform-specific location of that
//! file, and the validation/migration logic applied when loading.
//!
//! Contents:
//! - [`NoiseSuppressionMethod`]: selector for the microphone noise-reduction
//!   engine.
//! - [`Preset`]: performance preset (`balanced` / `low_impact`).
//! - [`AppConfig`]: every persisted user preference (video, audio, UI,
//!   filename template, global shortcuts, and capture-mode flags).
//! - `Default for AppConfig`: sane out-of-the-box values. Serde also uses this
//!   impl to fill in fields that are missing from older settings files.
//! - `AppConfig::load` / `AppConfig::load_from`: locate, read, migrate, and
//!   repair settings on disk.
//! - `AppConfig::save` / `AppConfig::save_to`: write settings atomically.
//! - `tests`: unit tests covering defaults, one-time migration, value clamping,
//!   corrupt-file backups, and atomic saves.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::hotkeys::Shortcuts;

/// What the recorder captures: a monitor or a specific application window.
///
/// Serialized as a tagged enum so older settings files (which have no
/// `capture_target` key) silently fall back to `Default`, which is
/// `Monitor { index: 1 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// Capture an entire monitor (1-based index matches the UI label).
    Monitor { index: usize },
    /// Capture a specific application window identified by its Win32 HWND
    /// (stored as `u64` for JSON portability across 32/64-bit) and a
    /// human-readable title snapshot taken at save time.
    Window { id: u64, title: String },
}

impl Default for CaptureTarget {
    fn default() -> Self {
        CaptureTarget::Monitor { index: 1 }
    }
}

/// Selects which microphone noise-suppression engine is used.
///
/// Serialized as snake_case (`"speex"`, `"rnnoise"`) so the value is stable
/// across application versions and human-readable in `settings.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppressionMethod {
    Speex,
    #[default]
    Rnnoise,
}

/// Performance / resource-use preset.
///
/// Only `Balanced` is exposed in the UI (P0); `LowImpact` is a P1 feature
/// readable from `settings.json` but has no UI toggle in v1.
///
/// Serialized as snake_case (`"balanced"`, `"low_impact"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// Default balanced preset: full frame rate, standard encoding priority.
    #[default]
    Balanced,
    /// Reduced-impact preset: lowers encoder thread priority to minimise
    /// interference with foreground apps.
    LowImpact,
}

/// All persisted user preferences, serialized to `settings.json`.
///
/// `#[serde(default)]` means every field falls back to the matching value in
/// [`AppConfig::default`] when it is missing from an older settings file, which
/// keeps the app working across upgrades that add new options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Index of the display to capture (1-based, as presented by the UI).
    ///
    /// Kept for backward compatibility; the authoritative field is
    /// `capture_target`. When `capture_target` is `Monitor { index }`,
    /// this field mirrors that index.
    pub monitor_index: usize,
    /// What the recorder should capture: a full monitor or an application window.
    ///
    /// Defaults to `Monitor { index: 1 }` (primary display).
    #[serde(default)]
    pub capture_target: CaptureTarget,
    /// Target frames per second; clamped to 1..=240 when loading.
    pub fps: u32,
    /// Target video bitrate in Mbps; clamped to 1..=100 when loading.
    pub bitrate_mbps: u32,
    /// Whether the mouse cursor is included in the recording.
    pub show_cursor: bool,
    /// Whether desktop/system audio is captured.
    pub system_audio: bool,
    /// Whether the microphone is captured.
    pub microphone: bool,
    /// Chosen microphone device name; `None` means the system default device.
    pub microphone_device: Option<String>,
    /// Gain applied to system audio (0.0..=1.5 when loading).
    pub system_volume: f32,
    /// Gain applied to microphone audio (0.0..=1.5 when loading).
    pub microphone_volume: f32,
    /// Optional microphone gate threshold in dB; `None` disables gating.
    pub microphone_threshold_db: Option<f32>,
    /// Whether noise suppression is enabled for the microphone.
    pub microphone_noise_suppression: bool,
    /// Which noise-suppression engine to use when suppression is enabled.
    pub microphone_noise_method: NoiseSuppressionMethod,
    /// Strength of the suppression filter in dB (-60..=0 when loading).
    pub microphone_suppression_db: i32,
    /// Migration marker: 0 = legacy settings file, 1 = RNNoise default applied.
    /// Used to apply the new noise-suppression default exactly once.
    #[serde(default)]
    pub microphone_filter_defaults_version: u8,
    /// Audio capture buffer size in milliseconds (100..=1000 when loading).
    pub audio_buffer_ms: u32,
    /// Whether to trust device-provided audio timestamps.
    pub audio_device_timestamps: bool,
    /// Maximum recording length in minutes; 0 means unlimited (capped at a day).
    pub duration_minutes: u32,
    /// Directory where recordings are written.
    pub output_dir: PathBuf,
    /// Template used to generate output file names.
    pub filename_template: String,
    /// Whether diagnostic logging is written to disk.
    pub logging_enabled: bool,
    /// Whether the recorder UI starts hidden (minimized to the tray).
    pub hide_window: bool,
    /// Whether system audio and microphone are mixed into separate output tracks.
    ///
    /// When `true`, the output file contains two audio tracks (system + mic)
    /// instead of a single mixed-down track.
    pub separate_audio_tracks: bool,
    /// When `true`, forces the legacy GDI screen-capture path instead of the
    /// Windows Graphics Capture (WGC) API. Use as a compatibility fallback for
    /// systems where WGC is unavailable or buggy.
    pub force_gdi: bool,
    /// Active performance preset. `Balanced` is the default; `LowImpact`
    /// reduces encoder thread priority to minimise interference with foreground
    /// apps. No UI toggle in v1 — read and written via `settings.json` only.
    pub preset: Preset,
    /// Global hotkeys (start/stop and related actions).
    pub shortcuts: Shortcuts,
}

impl Default for AppConfig {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|dirs| {
                dirs.video_dir()
                    .map(|path| path.join("Featherweight"))
            })
            .unwrap_or_else(|| PathBuf::from("recordings"));

        Self {
            monitor_index: 1,
            capture_target: CaptureTarget::Monitor { index: 1 },
            fps: 30,
            bitrate_mbps: 12,
            show_cursor: true,
            system_audio: true,
            microphone: false,
            microphone_device: None,
            system_volume: 1.0,
            microphone_volume: 1.0,
            microphone_threshold_db: None,
            microphone_noise_suppression: true,
            microphone_noise_method: NoiseSuppressionMethod::Rnnoise,
            microphone_suppression_db: -30,
            microphone_filter_defaults_version: 1,
            audio_buffer_ms: 250,
            audio_device_timestamps: true,
            duration_minutes: 0,
            output_dir,
            filename_template: crate::filename::DEFAULT_TEMPLATE.to_owned(),
            logging_enabled: false,
            hide_window: true,
            separate_audio_tracks: false,
            force_gdi: false,
            preset: Preset::Balanced,
            shortcuts: Shortcuts::default(),
        }
    }
}

impl AppConfig {
    /// Absolute path to the active `settings.json` in the OS config directory,
    /// or `None` if no config directory can be determined.
    ///
    /// The application identifier is `Featherweight`, so this resolves to
    /// `%APPDATA%\Featherweight\config\settings.json` (the `config` segment is
    /// added by the `directories` crate on Windows). It deliberately does not
    /// reuse the older app's identifier: sharing that path meant this binary
    /// and the previous recorder wrote each other's settings.
    pub fn path() -> Option<PathBuf> {
        ProjectDirs::from("dev", "", "Featherweight")
            .map(|dirs| dirs.config_dir().join("settings.json"))
    }

    /// Loads settings from the active config location.
    ///
    /// A missing file yields defaults (first run). Invalid values are clamped
    /// and reported through the returned warning so the UI can say so.
    pub fn load() -> Result<(Self, Option<String>), String> {
        let path = Self::path().ok_or("The settings directory is unavailable")?;
        Self::load_from(&path)
    }

    /// Reads, parses, migrates, and sanitizes settings from `path`.
    ///
    /// Behavior:
    /// - A missing file yields `AppConfig::default()` with no warning.
    /// - Unparseable JSON is an error; a timestamped `.corrupt-*.json` backup
    ///   of the bad file is written next to the original when possible.
    /// - Values outside their supported ranges are clamped, and a warning is
    ///   returned when anything actually changed.
    fn load_from(path: &Path) -> Result<(Self, Option<String>), String> {
        let json = match fs::read_to_string(path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(error) => {
                return Err(format!("Could not read {}: {error}", path.display()));
            }
        };
        let mut config: Self = serde_json::from_str(&json).map_err(|error| {
            let backup = path.with_extension(format!(
                "corrupt-{}.json",
                chrono::Local::now().format("%Y%m%d%H%M%S%f")
            ));
            match fs::copy(path, &backup) {
                Ok(_) => {
                    format!("Invalid settings: {error}. Backup: {}", backup.display())
                }
                Err(copy_error) => {
                    format!(
                        "Invalid settings: {error}. Backup failed: {copy_error}"
                    )
                }
            }
        })?;
        // Apply the enabled RNNoise default once to existing installations.
        // Subsequent changes made by the user are saved and respected.
        if config.microphone_filter_defaults_version == 0 {
            config.microphone_noise_suppression = true;
            config.microphone_noise_method = NoiseSuppressionMethod::Rnnoise;
            config.microphone_filter_defaults_version = 1;
        }
        let before = serde_json::to_string(&config).map_err(|error| error.to_string())?;
        // Preserve documented advanced values and expose the same ranges in UI.
        config.fps = config.fps.clamp(1, 240);
        config.bitrate_mbps = config.bitrate_mbps.clamp(1, 100);
        config.system_volume = config.system_volume.clamp(0.0, 1.5);
        config.microphone_volume = config.microphone_volume.clamp(0.0, 1.5);
        config.microphone_suppression_db = config.microphone_suppression_db.clamp(-60, 0);
        config.microphone_threshold_db = config
            .microphone_threshold_db
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(-60.0, -10.0));
        config.audio_buffer_ms = config.audio_buffer_ms.clamp(100, 1000);
        config.duration_minutes = config.duration_minutes.min(24 * 60);
        if !config.shortcuts.is_valid() {
            config.shortcuts = Shortcuts::default();
        }
        if config.output_dir.as_os_str().is_empty() {
            config.output_dir = Self::default().output_dir;
        }
        let changed =
            before != serde_json::to_string(&config).map_err(|error| error.to_string())?;
        Ok((
            config,
            changed.then(|| {
                String::from(
                    "Invalid settings were corrected; check your preferences and shortcuts.",
                )
            }),
        ))
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or("The settings directory is unavailable")?;
        self.save_to(&path).map_err(|error| {
            format!("Could not save {}: {error}", path.display())
        })
    }

    fn save_to(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().ok_or("Missing settings directory")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_live_under_the_apps_own_identity() {
        // Guards the rename away from the previous app's identifier: both apps
        // writing one settings file is how a template ends up ignoring its own
        // preferences.
        let path = AppConfig::path().expect("config path");
        assert!(
            path.ends_with(
                Path::new("Featherweight")
                    .join("config")
                    .join("settings.json")
            ),
            "unexpected settings path: {}",
            path.display()
        );
        assert!(
            !path.to_string_lossy().contains("hidden-screen-recorder"),
            "settings still point at the old app's identity: {}",
            path.display()
        );
    }

    #[test]
    fn rnnoise_is_enabled_by_default_and_migrates_existing_settings_once() {
        let defaults = AppConfig::default();
        assert!(defaults.microphone_noise_suppression);
        assert_eq!(
            defaults.microphone_noise_method,
            NoiseSuppressionMethod::Rnnoise
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, r#"{"microphone_noise_suppression":false,"microphone_noise_method":"speex","microphone_volume":0.8}"#).unwrap();
        let (mut config, warning) = AppConfig::load_from(&path).unwrap();
        assert!(warning.is_none());
        assert!(config.microphone_noise_suppression);
        assert_eq!(
            config.microphone_noise_method,
            NoiseSuppressionMethod::Rnnoise
        );
        assert_eq!(config.microphone_volume, 0.8);
        config.microphone_noise_suppression = false;
        config.microphone_noise_method = NoiseSuppressionMethod::Speex;
        config.save_to(&path).unwrap();
        let (loaded, warning) = AppConfig::load_from(&path).unwrap();
        assert!(warning.is_none());
        assert!(
            !loaded.microphone_noise_suppression,
            "an explicit choice after migration must persist"
        );
        assert_eq!(
            loaded.microphone_noise_method,
            NoiseSuppressionMethod::Speex
        );
    }

    #[test]
    fn atomic_save_replaces_existing_valid_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "previous contents").unwrap();
        let config = AppConfig {
            fps: 144,
            duration_minutes: 1000,
            ..Default::default()
        };
        config.save_to(&path).unwrap();
        let (loaded, warning) = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.fps, 144);
        assert_eq!(loaded.duration_minutes, 1000);
        assert!(warning.is_none());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_settings_are_reported_and_backed_up() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "{broken").unwrap();
        assert!(
            AppConfig::load_from(&path)
                .unwrap_err()
                .contains("Invalid settings")
        );
        let files: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|file| fs::read_to_string(file).unwrap() == "{broken")
        );
    }

    #[test]
    fn invalid_values_are_corrected_with_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, r#"{"fps":0,"bitrate_mbps":300,"microphone_threshold_db":-100,"duration_minutes":99999,"shortcuts":{"stop":{"key":120,"ctrl":true,"alt":false,"shift":false}}}"#).unwrap();
        let (loaded, warning) = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.fps, 1);
        assert_eq!(loaded.bitrate_mbps, 100);
        assert_eq!(loaded.microphone_threshold_db, Some(-60.0));
        assert_eq!(loaded.duration_minutes, 1440);
        assert!(loaded.shortcuts.is_valid());
        assert!(warning.is_some());
    }

    #[test]
    fn failed_save_preserves_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "original").unwrap();
        let guard = fs::OpenOptions::new().read(true).open(&path).unwrap();
        assert!(AppConfig::default().save_to(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        drop(guard);
    }

    #[test]
    fn old_settings_keep_hidden_mode_and_disable_logging() {
        let config: AppConfig = serde_json::from_str(r#"{"fps":60,"microphone":true}"#).unwrap();
        assert_eq!(config.fps, 60);
        assert!(config.microphone);
        assert_eq!(config.microphone_threshold_db, None);
        assert!(config.microphone_noise_suppression);
        assert_eq!(
            config.microphone_noise_method,
            NoiseSuppressionMethod::Rnnoise
        );
        assert_eq!(config.microphone_suppression_db, -30);
        assert!(config.hide_window);
        assert!(!config.logging_enabled);
        assert_eq!(config.shortcuts, Shortcuts::default());
    }

    #[test]
    fn new_preferences_survive_serialization() {
        let config = AppConfig {
            logging_enabled: true,
            hide_window: false,
            audio_buffer_ms: 500,
            audio_device_timestamps: false,
            microphone_threshold_db: Some(-38.0),
            microphone_noise_suppression: true,
            microphone_noise_method: NoiseSuppressionMethod::Rnnoise,
            microphone_suppression_db: -42,
            filename_template: "Game_{d}_{t}".to_owned(),
            shortcuts: Shortcuts {
                start: crate::hotkeys::Shortcut {
                    key: 0x52,
                    ctrl: true,
                    alt: false,
                    shift: true,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: AppConfig = serde_json::from_str(&json).unwrap();
        assert!(restored.logging_enabled);
        assert!(!restored.hide_window);
        assert_eq!(restored.audio_buffer_ms, 500);
        assert!(!restored.audio_device_timestamps);
        assert_eq!(restored.microphone_threshold_db, Some(-38.0));
        assert!(restored.microphone_noise_suppression);
        assert_eq!(
            restored.microphone_noise_method,
            NoiseSuppressionMethod::Rnnoise
        );
        assert_eq!(restored.microphone_suppression_db, -42);
        assert_eq!(restored.filename_template, "Game_{d}_{t}");
        assert_eq!(restored.shortcuts, config.shortcuts);
    }
}