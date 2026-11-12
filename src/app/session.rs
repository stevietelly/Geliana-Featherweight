//! Shared recording-session hub.
//!
//! [`SessionShared`] is the single meeting point between the recorder
//! worker, the UI, and the tray:
//!
//! - The window starts sessions through [`SessionShared::start_recording`]
//!   and asks for a stop through [`SessionShared::request_stop`]. The tray's
//!   Stop menu item calls the same method, so stopping works while the window
//!   is hidden (no egui frames involved).
//! - A pump thread (see [`spawn_pump`]) is the *only* consumer of the
//!   recorder's event channel. It mirrors session state into `status`, turns
//!   the once-per-second size ticks into [`SessionStats`] (output size, growth
//!   rate, disk headroom, ETA), and wakes the egui context so a visible window
//!   repaints.
//! - The tray reads `status`, `stats`, `mic_track_on`, and `mic_muted` to
//!   render recording state, and reads/writes `config` for its own actions
//!   (New recording, Open recordings folder, preference checkboxes). Tray-icon
//!   types are main-thread-bound, so the tray applies its visuals on the main
//!   thread (UI frames + its own menu handlers) — never from the pump.
//! - The disk floor is enforced here, not in the UI: the window is normally
//!   hidden while recording, so it cannot be the thing that watches free space.
//!
//! Config ownership: the window's editable `AppConfig` is the source of
//! truth. The window mirrors it into `config` whenever it saves, and pulls
//! tray-made preference changes back via `prefs_dirty`. This keeps the two
//! views consistent in both directions. The pump gets its own clone of the
//! session config at start, so `duration_minutes`/`output_dir` are exact even
//! if the user edits them mid-recording.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread,
};

use eframe::egui::Context;

use crate::{
    capture::recorder::{RecorderEvent, RecorderManager, SessionStatus},
    config::AppConfig,
    disk::{self, GrowthEstimator},
};

// ---------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------

/// Why a session stopped.
///
/// Kept on the hub after the session ends so a stop that happened while the
/// window was hidden is not silent — without this, an automatic stop and the
/// user pressing Stop look identical afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The user pressed Stop (window button or tray item).
    User,
    /// The configured duration limit was reached.
    DurationLimit,
    /// Free space on the output volume fell to the hard floor.
    LowDisk {
        /// Free bytes measured when the stop was requested.
        free_bytes: u64,
    },
}

impl StopReason {
    /// Whether the user should be told about this after the fact. A
    /// user-requested stop needs no explanation.
    pub fn is_notable(&self) -> bool {
        !matches!(self, Self::User)
    }

    /// One-line explanation for the window.
    pub fn summary(&self) -> String {
        match self {
            Self::User => "Recording stopped.".to_owned(),
            Self::DurationLimit => "Recording stopped: the duration limit was reached.".to_owned(),
            Self::LowDisk { free_bytes } => format!(
                "Recording stopped: only {} left on the output drive.",
                disk::format_bytes(*free_bytes)
            ),
        }
    }

    /// Compact form for the tray's idle status line.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::User => "stopped",
            Self::DurationLimit => "duration limit",
            Self::LowDisk { .. } => "disk full",
        }
    }
}

// ---------------------------------------------------------------------------
// Session stats
// ---------------------------------------------------------------------------

/// Live numbers for a running session: output size, measured growth, and the
/// disk state behind them.
///
/// Written once per second by the pump, read by the window (each frame) and the
/// tray (menu open / icon hover). Plain data behind a `Mutex` — no callbacks,
/// nothing to keep alive while the window is hidden.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// Bytes written to the active output file so far.
    pub file_size: u64,
    /// Measured output growth in bytes per second; `None` until there are
    /// enough samples or when the file is not growing.
    pub growth_bps: Option<f64>,
    /// Free bytes on the output volume, or `None` if it could not be queried.
    pub free_bytes: Option<u64>,
    /// Seconds until the session ends: the disk headroom ETA, the duration
    /// deadline, or whichever comes first.
    pub eta_secs: Option<u64>,
    /// Free space is low, either by absolute threshold or by measured ETA.
    pub disk_warn: bool,
    /// Free space has reached the hard floor; the session is being stopped.
    pub disk_critical: bool,
}

// ---------------------------------------------------------------------------
// Shared hub
// ---------------------------------------------------------------------------

/// App-level handle to everything recording-related, shared by the window,
/// the event pump, and the tray. Created once in `app::try_run` and passed
/// around as an `Arc` (it is `Send + Sync`).
pub struct SessionShared {
    /// egui context; the pump repaints the visible window through it.
    pub egui: Context,
    /// Mirrored session state; written by the pump, read by the UI and the
    /// tray. Single source of truth for "is a recording running".
    pub status: Mutex<SessionStatus>,
    /// Live size/growth/disk numbers for the running session. Zeroed when a
    /// session starts and when one ends.
    pub stats: Mutex<SessionStats>,
    /// Why the current (or most recently ended) session stopped. Cleared when a
    /// new session starts or when the window dismisses the notice.
    pub stop_reason: Mutex<Option<StopReason>>,
    /// Fatal error from the last session, if any. The recorder reports these
    /// through [`RecorderEvent::Failed`]; the window shows them so a failed
    /// recording does not silently do nothing.
    pub last_error: Mutex<Option<String>>,
    /// External stop signal. Set by the UI's Stop button, the tray's Stop
    /// menu item, or the disk guard; the recorder polls it every tick as its
    /// `external_stop` input. Re-armed to `false` when a session ends.
    /// `Arc` because the recorder thread takes ownership of a clone.
    pub stop_flag: Arc<AtomicBool>,
    /// Whether the current recording includes a mic track. Drives the tray's
    /// "Mute microphone" enable rule. Set at session start from the config.
    pub mic_track_on: AtomicBool,
    /// Whether the user muted the mic during the current recording. Set by
    /// the tray's "Mute microphone" item; reflected in the status line.
    /// `Arc` because the recorder thread takes a clone: the mixer polls it
    /// every mix tick, so muting actually silences the recorded mic track.
    pub mic_muted: Arc<AtomicBool>,
    /// Set by tray preference handlers after they update `config`; the
    /// window pulls the affected fields into its editable copy on the next
    /// frame so the UI reflects tray-made changes.
    pub prefs_dirty: AtomicBool,
    /// Snapshot of the window's config. The window overwrites it whenever
    /// its own config changes (and saves); tray actions read it (New
    /// recording, Open recordings folder) and tray preference checkboxes
    /// write to it. Kept in sync both ways via the window's save + pull.
    pub config: Mutex<AppConfig>,
    /// The recorder. The window and the tray both start sessions through
    /// [`SessionShared::start_recording`].
    pub recorder: Mutex<RecorderManager>,
}

impl SessionShared {
    /// Starts a recording session using `config` (the real capture pipeline,
    /// or the simulated worker when `FEATHERWEIGHT_SIMULATE=1`).
    ///
    /// Sets the mic-track flag and clears the mute flag from the config,
    /// clears any previous session from the manager, spawns the worker, and
    /// spawns the event pump that distributes events and computes stats.
    ///
    /// Returns `false` (and does nothing) when a session is already running.
    pub fn start_recording(self: &Arc<Self>, config: AppConfig) -> bool {
        {
            let status = self.status.lock().unwrap();
            if !matches!(*status, SessionStatus::Idle) {
                return false; // already recording
            }
        }

        self.mic_track_on
            .store(config.microphone, Ordering::Relaxed);
        self.mic_muted.store(false, Ordering::Relaxed);
        *self.stats.lock().unwrap() = SessionStats::default();
        *self.stop_reason.lock().unwrap() = None;
        *self.last_error.lock().unwrap() = None;

        let wake = {
            let egui = self.egui.clone();
            Arc::new(move || egui.request_repaint())
        };

        let mut recorder = self.recorder.lock().unwrap();
        // Join any finished worker and reset the manager before a new
        // session; a no-op when no session has run yet.
        recorder.clear();
        let rx = recorder.start(
            config.clone(),
            self.stop_flag.clone(),
            self.mic_muted.clone(),
            wake,
        );
        drop(recorder);

        spawn_pump(rx, self.clone(), config);
        true
    }

    /// Requests a stop and records why.
    ///
    /// Every stop path goes through here — window Stop button, tray Stop item,
    /// and the disk guard — so the reason is available after the session ends.
    pub fn request_stop(&self, reason: StopReason) {
        // Reason first: the recorder polls the flag and can finish before a
        // write that happened afterwards would land.
        *self.stop_reason.lock().unwrap() = Some(reason);
        self.stop_flag.store(true, Ordering::Release);
    }

    /// Forgets the stop explanation once the window has shown it.
    pub fn clear_stop_reason(&self) {
        *self.stop_reason.lock().unwrap() = None;
    }

    /// Forgets the last fatal error once the window has shown it.
    pub fn clear_last_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------
// Event pump
// ---------------------------------------------------------------------------

/// Drains `rx` on a background thread and distributes events to the shared
/// status, stats, and the egui repaint queue.
///
/// The thread blocks on `recv`, so it idles at zero cost between events, and
/// exits when the channel closes (recorder dropped).
///
/// This is where [`GrowthEstimator`] lives: it is per-session state, the pump
/// is per-session, and the disk guard has to keep working while the window is
/// hidden — so neither the window nor the tray can own it.
///
/// Note: the pump never touches the tray — tray-icon types are
/// main-thread-bound. The tray picks up changes on the next UI frame or the
/// next tray interaction (its handlers build their view from the shared state).
pub fn spawn_pump(rx: Receiver<RecorderEvent>, shared: Arc<SessionShared>, session: AppConfig) {
    let duration_secs = session.duration_minutes as u64 * 60;
    let output_dir = session.output_dir.clone();

    thread::Builder::new()
        .name("recorder-events".into())
        .spawn(move || {
            let mut growth = GrowthEstimator::new();
            while let Ok(event) = rx.recv() {
                match event {
                    RecorderEvent::Waiting { target } => {
                        *shared.status.lock().unwrap() =
                            SessionStatus::WaitingForFirstFrame { target };
                        shared.egui.request_repaint();
                    }
                    RecorderEvent::Started(path) => {
                        *shared.status.lock().unwrap() = SessionStatus::Recording {
                            path,
                            elapsed_secs: 0,
                        };
                        shared.egui.request_repaint();
                    }
                    RecorderEvent::Progress {
                        elapsed_secs,
                        file_size,
                        sampled_at,
                    } => {
                        {
                            let mut status = shared.status.lock().unwrap();
                            if let SessionStatus::Recording { elapsed_secs: e, .. } = &mut *status {
                                *e = elapsed_secs;
                            }
                        }

                        // One metadata sample and one disk query per second —
                        // the whole overhead of the readout. The estimator is
                        // fed the *sampling* instant, not "now", so a busy
                        // machine cannot make the rate read low.
                        let growth_bps = growth.push_at(sampled_at, file_size);
                        let free_bytes = disk::free_bytes_for_path(&output_dir);
                        let stats = compose_stats(
                            file_size,
                            growth_bps,
                            free_bytes,
                            elapsed_secs,
                            duration_secs,
                        );
                        let disk_critical = stats.disk_critical;
                        *shared.stats.lock().unwrap() = stats;

                        note_duration_limit(&shared, elapsed_secs, duration_secs);

                        // Enforce the floor the UI advertises. Without this the
                        // banner's "recording will stop below 1.0 GB" is a lie.
                        if disk_critical {
                            crate::log::log(&format!(
                                "disk guard: free space at the hard floor ({} free); stopping",
                                free_bytes
                                    .map(disk::format_bytes)
                                    .unwrap_or_else(|| "unknown".to_owned())
                            ));
                            shared.request_stop(StopReason::LowDisk {
                                free_bytes: free_bytes.unwrap_or(0),
                            });
                        }

                        shared.egui.request_repaint();
                    }
                    RecorderEvent::AudioWarning(warning) => {
                        crate::log::log(&format!("recorder warning: {warning}"));
                    }
                    RecorderEvent::Finished(path) => {
                        crate::log::log(&format!("recording finished: {}", path.display()));
                        // A session that ran to its configured deadline is
                        // stopped by the worker itself, so no stop reason was
                        // recorded: infer it before the state is cleared.
                        let elapsed_secs = match &*shared.status.lock().unwrap() {
                            SessionStatus::Recording { elapsed_secs, .. } => *elapsed_secs,
                            SessionStatus::WaitingForFirstFrame { .. } | SessionStatus::Idle => 0,
                        };
                        note_duration_limit(&shared, elapsed_secs, duration_secs);
                        finish(&shared);
                    }
                    RecorderEvent::Failed(message) => {
                        crate::log::log(&format!("recording failed: {message}"));
                        *shared.last_error.lock().unwrap() = Some(message);
                        finish(&shared);
                    }
                }
            }
        })
        .expect("could not spawn recorder event pump");
}

/// Records a duration-limit stop reason when the session reached its deadline
/// and nothing else explained the stop.
fn note_duration_limit(shared: &SessionShared, elapsed_secs: u64, duration_secs: u64) {
    if duration_secs == 0 || elapsed_secs.saturating_add(1) < duration_secs {
        return;
    }
    let mut reason = shared.stop_reason.lock().unwrap();
    if reason.is_none() {
        *reason = Some(StopReason::DurationLimit);
    }
}

/// Builds the stats snapshot from the tick's inputs.
///
/// Kept as a free function so the ETA and warning rules are unit-testable
/// without threads, clocks, or a real disk.
fn compose_stats(
    file_size: u64,
    growth_bps: Option<f64>,
    free_bytes: Option<u64>,
    elapsed_secs: u64,
    duration_secs: u64,
) -> SessionStats {
    let floor = disk::hard_floor();

    let disk_critical = free_bytes.is_some_and(|free| free <= floor);

    // Time until the hard floor, from the measured rate. This is the number
    // that matters when the disk is close: "N minutes left", not "N GB".
    let disk_eta = match (free_bytes, growth_bps) {
        (Some(free), Some(rate)) => disk::remaining_secs(free, floor, rate),
        _ => None,
    };

    let deadline_eta =
        (duration_secs > 0).then(|| duration_secs.saturating_sub(elapsed_secs));

    let eta_secs = match (disk_eta, deadline_eta) {
        (Some(disk_eta), Some(deadline_eta)) => Some(disk_eta.min(deadline_eta)),
        (eta, None) | (None, eta) => eta,
    };

    // The deadline is not a disk problem, so only the disk ETA can warn.
    let disk_warn = !disk_critical
        && (free_bytes.is_some_and(|free| free <= disk::WARN_FLOOR_BYTES)
            || disk_eta.is_some_and(|eta| eta <= disk::WARN_ETA_SECS));

    SessionStats {
        file_size,
        growth_bps,
        free_bytes,
        eta_secs,
        disk_warn,
        disk_critical,
    }
}

/// Resets shared state after a session ends (success or failure).
///
/// The stop reason and last error survive so the window can explain the end of
/// the session; stats do not, because they describe a file that is no longer
/// growing.
fn finish(shared: &SessionShared) {
    *shared.status.lock().unwrap() = SessionStatus::Idle;
    *shared.stats.lock().unwrap() = SessionStats::default();
    // Re-arm the external stop flag for the next session.
    shared.stop_flag.store(false, Ordering::Release);
    shared.egui.request_repaint();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const MIB: u64 = 1024 * 1024;

    /// Hub wired the way `app::try_run` wires it, minus the window.
    fn test_shared(config: &AppConfig) -> Arc<SessionShared> {
        Arc::new(SessionShared {
            egui: Context::default(),
            status: Mutex::new(SessionStatus::Idle),
            stats: Mutex::new(SessionStats::default()),
            stop_reason: Mutex::new(None),
            last_error: Mutex::new(None),
            stop_flag: Arc::new(AtomicBool::new(false)),
            mic_track_on: AtomicBool::new(false),
            mic_muted: Arc::new(AtomicBool::new(false)),
            prefs_dirty: AtomicBool::new(false),
            config: Mutex::new(config.clone()),
            recorder: Mutex::new(RecorderManager::default()),
        })
    }

    fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    #[test]
    fn no_free_space_means_no_disk_eta() {
        let stats = compose_stats(0, Some(MIB as f64), None, 10, 0);
        assert_eq!(stats.eta_secs, None);
        assert!(!stats.disk_warn);
        assert!(!stats.disk_critical);
    }

    #[test]
    fn healthy_disk_does_not_warn() {
        let stats = compose_stats(10 * MIB, Some(MIB as f64), Some(500_000 * MIB), 10, 0);
        assert!(!stats.disk_warn);
        assert!(!stats.disk_critical);
        assert!(stats.eta_secs.unwrap() > 3600);
    }

    #[test]
    fn warn_on_absolute_floor_even_without_a_rate() {
        // 2 GiB free is below the 3 GiB warning floor.
        let stats = compose_stats(0, None, Some(2 * 1024 * MIB), 10, 0);
        assert!(stats.disk_warn);
        assert!(!stats.disk_critical);
    }

    #[test]
    fn warn_on_short_eta_even_with_plenty_of_space() {
        // 20 GiB free but 80 MiB/s: ~4 minutes to the floor.
        let stats = compose_stats(0, Some(80.0 * MIB as f64), Some(20 * 1024 * MIB), 10, 0);
        assert!(stats.disk_warn);
        assert!(!stats.disk_critical);
        let eta = stats.eta_secs.unwrap();
        assert!(eta < disk::WARN_ETA_SECS, "eta {eta}");
    }

    #[test]
    fn floor_is_critical_and_has_zero_eta() {
        let stats = compose_stats(0, Some(MIB as f64), Some(MIB), 10, 0);
        assert!(stats.disk_critical);
        assert_eq!(stats.eta_secs, Some(0));
    }

    #[test]
    fn duration_deadline_wins_when_it_is_sooner() {
        // 40 minutes of disk headroom, 5 minutes of duration left.
        let disk_rate = MIB as f64;
        let stats = compose_stats(
            0,
            Some(disk_rate),
            Some(disk::HARD_FLOOR_BYTES + 40 * 60 * MIB),
            0,
            300,
        );
        assert_eq!(stats.eta_secs, Some(300));
        // A short recording deadline is not a disk warning.
        assert!(!stats.disk_warn);
    }

    #[test]
    fn duration_without_a_rate_still_produces_an_eta() {
        let stats = compose_stats(0, None, Some(500_000 * MIB), 60, 600);
        assert_eq!(stats.eta_secs, Some(540));
    }

    #[test]
    fn stop_reason_wording() {
        assert!(!StopReason::User.is_notable());
        assert!(StopReason::DurationLimit.is_notable());
        assert!(StopReason::LowDisk { free_bytes: 1024 }.is_notable());
        assert_eq!(StopReason::LowDisk { free_bytes: 0 }.short_label(), "disk full");
        assert!(StopReason::LowDisk { free_bytes: 1024 * 1024 }
            .summary()
            .contains("1.0 MB"));
    }

    /// End-to-end: simulated recorder -> event pump -> stats -> stop.
    ///
    /// Runs the real pump and the real (simulated) worker against a temporary
    /// directory, so the file-size sampling path is exercised for real rather
    /// than mocked. Needs ~3.5 s because the worker ticks once per second.
    #[test]
    fn pump_publishes_size_growth_and_disk_headroom() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = AppConfig::default();
        config.output_dir = dir.path().to_path_buf();
        config.bitrate_mbps = 8; // exactly 1 MB/s
        config.duration_minutes = 0;

        // If the test machine is genuinely near the disk floor the guard would
        // stop the session mid-test, which is correct behaviour but would make
        // this assertion meaningless.
        let free = disk::free_bytes_for_path(dir.path()).unwrap_or(u64::MAX);
        if free <= disk::hard_floor() * 2 {
            eprintln!("skipping: only {free} bytes free on the temp volume");
            return;
        }

        let shared = test_shared(&config);
        let mut manager = RecorderManager::default();
        let rx = manager.start_record(
            config.clone(),
            shared.stop_flag.clone(),
            Arc::new(|| {}),
        );
        spawn_pump(rx, shared.clone(), config);

        // Wait for a few one-second ticks: the estimator needs two samples, and
        // three ticks give the size assertion something to check.
        let recorded = wait_until(Duration::from_secs(8), || {
            let stats = shared.stats.lock().unwrap();
            stats.file_size >= 2_000_000 && stats.growth_bps.is_some()
        });
        assert!(recorded, "pump never published a growth estimate");

        let stats = shared.stats.lock().unwrap().clone();
        assert!(stats.file_size >= 2_000_000, "size {}", stats.file_size);
        let rate = stats.growth_bps.expect("growth");
        assert!(
            (rate - 1_000_000.0).abs() < 100_000.0,
            "expected ~1 MB/s, got {rate}"
        );
        assert!(stats.free_bytes.is_some(), "free space was not sampled");
        assert!(stats.eta_secs.is_some(), "no ETA with a rate and free space");
        assert!(!stats.disk_critical);

        // A user stop is recorded and clears the live state.
        shared.request_stop(StopReason::User);
        let idle = wait_until(Duration::from_secs(5), || {
            matches!(*shared.status.lock().unwrap(), SessionStatus::Idle)
        });
        assert!(idle, "session did not stop");
        assert_eq!(*shared.stop_reason.lock().unwrap(), Some(StopReason::User));
        assert_eq!(*shared.stats.lock().unwrap(), SessionStats::default());
        assert!(!shared.stop_flag.load(Ordering::Acquire), "flag not re-armed");
        manager.shutdown();
    }
}
