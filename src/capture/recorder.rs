//! Recording session orchestration.
//!
//! This module is the top-level coordinator for a single recording: it drives
//! the real capture pipeline (`capture::pipeline`: WGC with GDI fallback plus
//! WASAPI audio), output-file naming, and duration enforcement. It is fully
//! decoupled from the UI — the UI hands it a config and receives
//! [`RecorderEvent`]s over an `mpsc` channel. Setting `FEATHERWEIGHT_SIMULATE=1`
//! swaps the real pipeline for the simulated worker (no capture), which is
//! handy for testing the window/tray switch flow.
//!
//! Contents:
//! - [`RecorderEvent`]: events sent to the UI (started, warnings, finished,
//!   failed).
//! - [`RecorderManager`]: owns the lifecycle of a recording session. Only one
//!   recording can run at a time. The UI creates a manager once and reuses it
//!   across sessions.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    capture::pipeline,
    config::{AppConfig, CaptureTarget},
    filename,
    log,
};

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// High-level session state mirrored from [`RecorderEvent`]s.
///
/// Held in an `Arc<Mutex<..>>` shared by the event pump (writer), the UI
/// (reader), and the tray (reader). Deliberately UI/tray-agnostic: plain
/// data only.
#[derive(Debug, Clone)]
pub enum SessionStatus {
    /// No session running.
    Idle,
    /// A session was started but the capture target has not delivered a
    /// video frame yet — typically a minimized window. No output file
    /// exists at this point; the session becomes [`SessionStatus::Recording`]
    /// on the first frame, or returns to [`SessionStatus::Idle`] if stopped
    /// while waiting.
    WaitingForFirstFrame {
        /// Title of the capture target being waited for.
        target: String,
    },
    /// A session is running. `elapsed_secs` is refreshed by the recorder's
    /// once-per-second [`RecorderEvent::Progress`] tick.
    Recording {
        /// Destination file of the current recording.
        path: PathBuf,
        /// Elapsed recording time in whole seconds.
        elapsed_secs: u64,
    },
}

/// Events emitted by the recording pipeline to the UI over an `mpsc` channel.
#[derive(Debug)]
pub enum RecorderEvent {
    /// The session is waiting for the capture target to deliver its first
    /// frame (a minimized window, for instance). No output file exists yet;
    /// [`RecorderEvent::Started`] follows once one arrives, so cancelling
    /// while waiting leaves nothing on disk.
    Waiting {
        /// Title of the window being waited for.
        target: String,
    },

    /// The recorder has opened the output file and begun writing frames.
    /// The path is the final destination of the recording.
    Started(PathBuf),

    /// Periodic tick with the elapsed recording time and the current size of
    /// the file being written. Emitted roughly once per second so the UI and
    /// the tray can show a live timer, output size and growth rate (see
    /// `disk::GrowthEstimator`).
    Progress {
        /// Elapsed recording time in whole seconds.
        elapsed_secs: u64,
        /// Size of the file the encoder is writing right now. Zero until the
        /// encoder has created/flushed it.
        file_size: u64,
        /// When `file_size` was sampled, on the monotonic clock.
        ///
        /// The growth estimator must use this rather than the time the event
        /// is handled: on a busy machine the event thread can be scheduled
        /// late, and timing at handling would under-report the rate exactly
        /// when the machine is struggling.
        sampled_at: std::time::Instant,
    },

    /// A non-fatal audio warning occurred (device glitch, format mismatch).
    /// The recording continues; the UI may display this to the user.
    AudioWarning(String),

    /// The recording finished successfully and the output file is complete.
    Finished(PathBuf),

    /// A fatal error stopped the recording. The output file may be incomplete.
    Failed(String),
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Owns the state of a single recording session.
///
/// Call [`RecorderManager::start`] to begin a recording. The manager spawns a
/// worker thread and returns a channel receiver for [`RecorderEvent`]s. Call
/// [`RecorderManager::stop`] (or drop the manager) to request a stop;
/// the worker sends [`RecorderEvent::Finished`] or [`RecorderEvent::Failed`]
/// before exiting.
///
/// Only one session can run at a time. `start` while already running is a
/// no-op — `stop` the current session first.
#[derive(Default)]
pub struct RecorderManager {
    /// The stop signal for the current session.
    stop: Option<Arc<AtomicBool>>,
    /// The worker thread for the current session.
    worker: Option<thread::JoinHandle<()>>,
}

impl RecorderManager {
    /// Starts a new recording session.
    ///
    /// `config` is cloned at the call site; subsequent changes to the UI's
    /// copy do not affect the running session. `external_stop` is an
    /// additional stop signal (e.g. from the tray or a hotkey). `mic_muted`
    /// is the live mute flag: the audio mixer polls it, so the tray's
    /// "Mute microphone" item silences the mic mid-recording. `wake` is
    /// called after each event so the egui context repaints.
    ///
    /// Runs the real capture pipeline (see [`pipeline`]) unless the
    /// `FEATHERWEIGHT_SIMULATE=1` environment variable is set, in which case
    /// the simulated worker runs instead (useful for exercising the
    /// window/tray switch flow without real capture).
    ///
    /// Returns the [`mpsc::Receiver`] end of the event channel. The pump
    /// thread (see `app::session::spawn_pump`) consumes it.
    pub fn start(
        &mut self,
        config: AppConfig,
        external_stop: Arc<AtomicBool>,
        mic_muted: Arc<AtomicBool>,
        wake: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> mpsc::Receiver<RecorderEvent> {
        let simulate = std::env::var("FEATHERWEIGHT_SIMULATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());

        let handle = if simulate {
            // Simulated sessions have no audio to mute.
            let _ = mic_muted;
            thread::Builder::new()
                .name("recorder-simulated".into())
                .spawn(move || {
                    simulated_worker(config, tx, stop, external_stop, wake);
                })
                .expect("could not spawn simulated recorder worker")
        } else {
            thread::Builder::new()
                .name("recorder-worker".into())
                .spawn(move || {
                    worker(config, tx, stop, external_stop, mic_muted, wake);
                })
                .expect("could not spawn recorder worker")
        };

        self.worker = Some(handle);
        rx
    }

    /// Starts a **simulated** recording session.
    ///
    /// Mirrors [`RecorderManager::start`] from the caller's point of view —
    /// same lifecycle, same events, same stop semantics — but performs no
    /// real screen capture or encoding. Intended for exercising the
    /// window/tray switch while the real recorder is under construction.
    ///
    /// What is simulated:
    /// - The output path is resolved exactly like a real session (directory
    ///   creation, filename template, collision suffixes) and the file is
    ///   created empty.
    /// - A [`RecorderEvent::Started`] is sent immediately.
    /// - A [`RecorderEvent::Progress`] tick is sent roughly every second with
    ///   the elapsed time and the file's current size. `bitrate_mbps * 1e6 / 8`
    ///   bytes are appended per second so the file grows at the configured
    ///   rate, which is what the growth estimator and the status line read.
    /// - The optional `duration_minutes` deadline applies, and stopping (via
    ///   [`RecorderManager::stop`], `external_stop`, or the deadline) sends
    ///   [`RecorderEvent::Finished`].
    pub fn start_record(
        &mut self,
        config: AppConfig,
        external_stop: Arc<AtomicBool>,
        wake: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> mpsc::Receiver<RecorderEvent> {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());

        let handle = thread::Builder::new()
            .name("recorder-simulated".into())
            .spawn(move || {
                simulated_worker(config, tx, stop, external_stop, wake);
            })
            .expect("could not spawn simulated recorder worker");

        self.worker = Some(handle);
        rx
    }

    /// Requests the current session to stop.
    ///
    /// This sets the internal stop flag; the worker will finish encoding the
    /// last frame and then send [`RecorderEvent::Finished`]. Safe to call when
    /// no session is running (no-op).
    pub fn stop(&self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
    }

    /// Clears the finished session state so the manager is ready for another
    /// `start` call. Safe to call after [`RecorderEvent::Finished`] or
    /// [`RecorderEvent::Failed`] is received.
    pub fn clear(&mut self) {
        // The worker has already exited by the time the UI clears; join to
        // avoid a leaked thread handle.
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        self.stop = None;
    }

    /// Signals stop and joins the worker. Called from `Drop`.
    pub fn shutdown(&mut self) {
        self.stop();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RecorderManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Entry point for the recorder worker thread.
///
/// Responsible for:
/// 1. Resolving the output file path.
/// 2. Starting the real capture pipeline ([`pipeline::start_recording`]).
/// 3. Emitting a once-per-second [`RecorderEvent::Progress`] tick and
///    enforcing the optional duration limit.
/// 4. Waiting for the stop signal (internal or external).
/// 5. Stopping the capture, muxing (when separate tracks were requested), and
///    sending the final event.
fn worker(
    config: AppConfig,
    tx: Sender<RecorderEvent>,
    internal_stop: Arc<AtomicBool>,
    external_stop: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    // Resolve output path: directory + filename template + collision suffix.
    let output_path = match resolve_output_path(&config) {
        Ok(path) => path,
        Err(e) => {
            let _ = tx.send(RecorderEvent::Failed(e));
            wake();
            return;
        }
    };

    // Start the capture pipeline. Failure here is fatal for the session.
    let active = match pipeline::start_recording(
        &config,
        output_path.clone(),
        internal_stop.clone(),
        &mic_muted,
    ) {
        Ok(active) => active,
        Err(e) => {
            log::log(&format!("could not start recording: {e}"));
            let _ = tx.send(RecorderEvent::Failed(e));
            wake();
            return;
        }
    };

    log::log(&format!("recording to {}", output_path.display()));

    // Window targets can sit waiting for their first frame (a minimized or
    // fully static window delivers none); monitors deliver one within
    // milliseconds, so they start immediately.
    let wait_target = match config.capture_target {
        CaptureTarget::Window { title, .. } => Some(title),
        CaptureTarget::Monitor { .. } => None,
    };
    if let Some(target) = &wait_target {
        let _ = tx.send(RecorderEvent::Waiting {
            target: target.clone(),
        });
        wake();
    } else {
        let _ = tx.send(RecorderEvent::Started(output_path.clone()));
        wake();
    }

    // Poll for stop signals, emit Progress ticks, and enforce the duration
    // limit at 50 ms intervals.
    //
    // The size is sampled from the *active* file: with separate audio tracks
    // the encoder writes `<name>.temp.mp4` and the final file only appears
    // after the mux, so the destination in `Started` would report zero bytes
    // for the whole session.
    //
    // While waiting for the first frame there is no file to sample and no
    // elapsed time to show, so ticks are withheld and the duration deadline
    // only begins once the session actually starts recording.
    let sampled_path = active.active_file_path().clone();
    let mut waiting = wait_target.is_some();
    // Monitors start immediately; window targets only get a start instant
    // once the first frame arrives (the transition branch below).
    let mut started = if waiting { None } else { Some(Instant::now()) };
    let mut deadline = if waiting {
        None
    } else if config.duration_minutes > 0 {
        Some(Instant::now() + Duration::from_secs(config.duration_minutes as u64 * 60))
    } else {
        None
    };
    let mut last_tick: Option<u64> = None;
    loop {
        thread::sleep(Duration::from_millis(50));

        if internal_stop.load(Ordering::Acquire) || external_stop.load(Ordering::Acquire) {
            break;
        }

        if waiting {
            if active.first_frame_arrived() {
                waiting = false;
                started = Some(Instant::now());
                deadline = if config.duration_minutes > 0 {
                    Some(Instant::now() + Duration::from_secs(config.duration_minutes as u64 * 60))
                } else {
                    None
                };
                let _ = tx.send(RecorderEvent::Started(output_path.clone()));
                wake();
            }
            continue;
        }

        let started = started.expect("recording implies a start instant");

        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        let elapsed_secs = started.elapsed().as_secs();
        if last_tick != Some(elapsed_secs) {
            last_tick = Some(elapsed_secs);
            let (sampled_at, file_size) = sample_file_size(&sampled_path);
            let _ = tx.send(RecorderEvent::Progress {
                elapsed_secs,
                file_size,
                sampled_at,
            });
            wake();
        }
    }

    log::log("recording stopped; finalizing output");

    // Stop the capture and finalize. `ActiveRecording::stop` joins the
    // capture/encoder threads and runs the ffmpeg multi-track mux when
    // separate audio tracks were requested.
    match active.stop() {
        Ok(path) => {
            let _ = tx.send(RecorderEvent::Finished(path));
        }
        Err(e) => {
            let _ = tx.send(RecorderEvent::Failed(e));
        }
    }
    wake();
}

// ---------------------------------------------------------------------------
// Simulated worker
// ---------------------------------------------------------------------------

/// Entry point for the **simulated** recorder worker thread.
///
/// Runs the same session lifecycle as [`worker`] — resolve path, `Started`,
/// duration limit, stop signals, `Finished` — without any capture or encoding.
/// See [`RecorderManager::start_record`] for the details.
fn simulated_worker(
    config: AppConfig,
    tx: Sender<RecorderEvent>,
    internal_stop: Arc<AtomicBool>,
    external_stop: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    // Resolve the output path exactly like a real session so the filename
    // template, directory creation, and collision suffixes get exercised.
    let output_path = match resolve_output_path(&config) {
        Ok(path) => path,
        Err(e) => {
            let _ = tx.send(RecorderEvent::Failed(e));
            wake();
            return;
        }
    };

    log::log(&format!("[simulated] recording to {}", output_path.display()));

    // Create the (empty) file so path collisions behave like a real recording,
    // and keep the handle open like the encoder does.
    let mut output = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&output_path)
    {
        Ok(file) => file,
        Err(e) => {
            let _ = tx.send(RecorderEvent::Failed(format!(
                "Could not create output file {}: {e}",
                output_path.display()
            )));
            wake();
            return;
        }
    };

    let _ = tx.send(RecorderEvent::Started(output_path.clone()));
    wake();

    // Optional deadline (0 = unlimited), same rule as the real worker.
    let deadline = if config.duration_minutes > 0 {
        Some(Instant::now() + Duration::from_secs(config.duration_minutes as u64 * 60))
    } else {
        None
    };

    // Simulated data rate: the bitrate is in megabits per second, so bytes per
    // second is `bitrate * 1e6 / 8` (12 Mbps => 1.5 MB/s). Done in one step so
    // the file grows at the rate the status line will claim. Written in 64 KiB
    // pieces to avoid holding a whole second of filler in memory.
    let bytes_per_second = (config.bitrate_mbps.max(1) as u64) * 1_000_000 / 8;
    let chunk = vec![0u8; 64 * 1024];

    let started = Instant::now();
    let mut last_tick: Option<u64> = None;
    // Wall-clock position of the last fill, so each tick writes the bytes that
    // belong to the interval that just elapsed. A lumpy "1 MB per tick" would
    // read as a rate spike whenever a tick is late (which is exactly what a
    // busy machine produces, and what the growth readout is meant to show).
    let mut written_secs = 0.0_f64;

    // Poll at 200 ms; emit a Progress tick (and grow the file) once per second.
    loop {
        thread::sleep(Duration::from_millis(200));

        let timed_out = deadline.is_some_and(|d| Instant::now() >= d);
        let stopped = internal_stop.load(Ordering::Acquire)
            || external_stop.load(Ordering::Acquire)
            || timed_out;
        if stopped {
            break;
        }

        let elapsed_secs = started.elapsed().as_secs();
        if last_tick != Some(elapsed_secs) {
            last_tick = Some(elapsed_secs);
            let elapsed_now = started.elapsed().as_secs_f64();
            let bytes = ((elapsed_now - written_secs) * bytes_per_second as f64).round() as u64;
            written_secs = elapsed_now;
            if let Err(e) = write_repeated(&mut output, &chunk, bytes) {
                log::log(&format!("[simulated] write failed: {e}"));
            }
            let (sampled_at, file_size) = sample_file_size(&output_path);
            let _ = tx.send(RecorderEvent::Progress {
                elapsed_secs,
                file_size,
                sampled_at,
            });
            wake();
        }
    }

    log::log("[simulated] recording stopped; finalizing output");

    let _ = tx.send(RecorderEvent::Finished(output_path));
    wake();
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Samples the size of the output file.
///
/// Returns the instant the size was read alongside it, so the growth estimator
/// can use the sampling time rather than the time the UI thread handles the
/// event. This is a single `metadata` call once per tick — the cheapest way to
/// observe what the encoder is actually writing, without coupling the recorder
/// to the encoder's internals.
fn sample_file_size(path: &std::path::Path) -> (Instant, u64) {
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    (Instant::now(), size)
}

/// Appends `total` bytes to `file` in `chunk`-sized pieces, so the simulated
/// output grows at the requested rate without allocating a second of filler.
fn write_repeated(file: &mut std::fs::File, chunk: &[u8], total: u64) -> std::io::Result<()> {
    let mut remaining = total;
    while remaining > 0 {
        let len = remaining.min(chunk.len() as u64) as usize;
        std::io::Write::write_all(file, &chunk[..len])?;
        remaining -= len as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output path helpers
// ---------------------------------------------------------------------------

/// Builds a unique output file path from the config's directory and filename
/// template, appending `_1`, `_2`, … if the base name is taken.
fn resolve_output_path(config: &AppConfig) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&config.output_dir)
        .map_err(|e| format!("Could not create output directory: {e}"))?;

    let now = chrono::Local::now().naive_local();
    let base = filename::render(&config.filename_template, now, config.monitor_index);

    // Try the base name first, then _1, _2, … up to _999.
    for suffix in std::iter::once(String::new())
        .chain((1u32..=999).map(|n| format!("_{n}")))
    {
        let name = format!("{base}{suffix}.mp4");
        let path = config.output_dir.join(&name);
        if !path.exists() {
            return Ok(path);
        }
    }

    Err(format!(
        "Could not find a free filename for {base}.mp4 in {}",
        config.output_dir.display()
    ))
}
