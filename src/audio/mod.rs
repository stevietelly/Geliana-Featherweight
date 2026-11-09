/// audio/ — WASAPI capture, timeline and clock (production spec section 4).
///
/// Layout (spec 2.2):
///   loopback.rs   app audio (process loopback with system fallback)
///   mic.rs        microphone capture (opt-in)
///   timeline.rs   per-source accumulator placed by QPC timestamp
///   clock.rs      fixed-cadence chunk emission + mixing
///   pid.rs        resolve the captured window's process ID
///
/// Threading (spec 2.1): each source owns a WASAPI capture thread; the audio
/// clock thread emits fixed 20 ms chunks for all tracks through the muxer.
/// An audio failure never aborts a recording (spec 4.6): failed sources are
/// logged and the clock emits zeros for them.
///
/// Dependency boundary: nothing here imports tao/tray-icon/global-hotkey.
pub mod clock;
pub mod loopback;
pub mod mic;
pub mod pid;
pub mod timeline;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFSample, MFCreateMemoryBuffer, MFCreateSample,
};

use crate::mux::MuxWriter;

// ---------------------------------------------------------------------------
// Shared constants (spec 4.1 / 4.3)
// ---------------------------------------------------------------------------

pub const SAMPLE_RATE: u32 = 48_000;

/// Chunk cadence: 20 ms = 960 frames at 48 kHz (spec 4.3).
pub const CHUNK_MS: u64 = 20;
pub const CHUNK_FRAMES: u64 = SAMPLE_RATE as u64 * CHUNK_MS / 1000; // 960
pub const CHUNK_100NS: i64 = CHUNK_MS as i64 * 100_000; // 2 000 000

/// Emission margin (spec 4.3): the clock emits up to `now - T0 - MARGIN` so
/// late packets still arrive in time.
pub const MARGIN_100NS: i64 = 80 * 100_000; // 80 ms

/// Epoch offset between the QPC clock and WGC frame timestamps, in 100 ns.
///
/// The design assumption [U, verified by gate C1] is that `Frame::timestamp()`
/// (SystemRelativeTime) and WASAPI QPC positions share the QPC epoch, so the
/// offset is 0. The delta is logged at T0 (see `AudioPipeline::set_origin`);
/// if C1 shows a stable non-zero epoch offset, hard-code it here and report.
pub const CLOCK_OFFSET_100NS: i64 = 0;

// ---------------------------------------------------------------------------
// QPC helpers (bridge between the WASAPI and WGC clocks, spec 3.6)
// ---------------------------------------------------------------------------

pub fn qpc_frequency() -> i64 {
    let mut freq = 0i64;
    unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq).ok() }
        .expect("QueryPerformanceFrequency failed");
    freq
}

pub fn qpc_now() -> i64 {
    let mut t = 0i64;
    unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut t).ok() }
        .expect("QueryPerformanceCounter failed");
    t
}

/// Convert QPC ticks to 100 ns units.
pub fn qpc_to_100ns(ticks: i64, freq: i64) -> i64 {
    ((ticks as i128 * 10_000_000) / freq as i128) as i64
}

// ---------------------------------------------------------------------------
// Clock map: audio packet positions → video clock (relative to T0)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ClockMap {
    /// Recording origin `T0` = the timestamp of the first video frame
    /// (100 ns of the frame clock). 0 until the first frame arrives.
    pub origin_100ns: Arc<AtomicI64>,
}

impl ClockMap {
    pub fn new() -> Self {
        Self {
            origin_100ns: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Convert a QPC-derived 100 ns value to the video-clock space relative
    /// to T0. Returns `None` while T0 is not yet set.
    pub fn video_clock_100ns(&self, qpc_100ns: i64) -> Option<i64> {
        let origin = self.origin_100ns.load(Ordering::Relaxed);
        if origin == 0 {
            None
        } else {
            Some(qpc_100ns - CLOCK_OFFSET_100NS - origin)
        }
    }
}

// ---------------------------------------------------------------------------
// Status events (spec section 6 — no UI types here; recorder.rs maps them)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum AudioStatusEvent {
    /// The audio strategy actually in use, e.g. "App audio: pid 4312",
    /// "App audio: system loopback (no window matched)".
    Strategy(String),
    /// Audio capture cannot proceed; the recording continues with silence.
    Unavailable(String),
}

// ---------------------------------------------------------------------------
// PCM sample creation
// ---------------------------------------------------------------------------

/// Create an `IMFSample` carrying interleaved 16-bit PCM. Time and duration
/// are in 100 ns units of the video-clock space (relative to T0).
pub fn make_pcm_sample(
    samples: &[i16],
    time_100ns: i64,
    dur_100ns: i64,
) -> windows::core::Result<IMFSample> {
    let byte_len = (samples.len() * 2) as u32;
    let buffer: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(byte_len)? };
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(samples.as_ptr() as *const u8, ptr, byte_len as usize);
        buffer.Unlock()?;
        buffer.SetCurrentLength(byte_len)?;
    }
    let sample: IMFSample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time_100ns)?;
        sample.SetSampleDuration(dur_100ns)?;
    }
    Ok(sample)
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// Configuration for the audio pipeline.
pub struct AudioConfig {
    /// Where status lines are sent (recorder maps them to `StatusKind`).
    pub events: Option<Sender<AudioStatusEvent>>,
    /// Shared mux writer; audio writes go through the same Mutex as video.
    pub mux: Arc<MuxWriter>,
    /// Whether the mic track is enabled (default off, spec 4.5).
    pub mic_enabled: bool,
    /// Process ID for per-process loopback, or None for system loopback.
    pub pid: Option<u32>,
}

/// Owns the audio threads: app loopback, optional mic, and the 20 ms clock.
///
/// Shutdown follows the finalize order from spec 5.3:
/// 1. the clock emits remaining chunks up to the last video timestamp,
/// 2. audio sources stop and join,
/// 3. the caller finalizes the mux only after this returns.
pub struct AudioPipeline {
    shared: Arc<Shared>,
    source_handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    clock_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    shutdown_done: AtomicBool,
}

pub(super) struct Shared {
    /// App timeline: stereo 48 kHz (track 1 "app" and the mix base).
    pub app: timeline::SharedTimeline,
    /// Mic timeline: mono 48 kHz. `None` when the mic track is disabled.
    pub mic: Option<timeline::SharedTimeline>,
    pub mic_enabled: bool,
    pub mic_muted: Arc<AtomicBool>,
    pub map: ClockMap,
    pub mux: Arc<MuxWriter>,
    pub stop: Arc<AtomicBool>,
    /// Last video timestamp (absolute frame-clock 100 ns) to emit through on
    /// shutdown. Set right before `stop` flips.
    pub flush_to: Mutex<Option<i64>>,
    /// Fixed-cadence clock state (frames already emitted).
    pub clock_state: clock::ClockState,
    pub events: Option<Sender<AudioStatusEvent>>,
}

impl AudioPipeline {
    /// Start the audio threads. Sources start first (spec 6: before the
    /// first video frame) and buffer packets until T0; the clock idles until
    /// T0 appears and the mux is ready.
    pub fn start(cfg: AudioConfig) -> Result<Self, String> {
        let map = ClockMap::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mic_muted = Arc::new(AtomicBool::new(false));

        let app = timeline::SharedTimeline::new(Arc::new(Mutex::new(timeline::Timeline::new(
            2,
            map.clone(),
        ))));

        let mic_holder = if cfg.mic_enabled {
            Some(timeline::SharedTimeline::new(Arc::new(Mutex::new(
                timeline::Timeline::new(1, map.clone()),
            ))))
        } else {
            None
        };

        let shared = Arc::new(Shared {
            app,
            mic: mic_holder,
            mic_enabled: cfg.mic_enabled,
            mic_muted,
            map,
            mux: cfg.mux,
            stop,
            flush_to: Mutex::new(None),
            clock_state: clock::ClockState::new(),
            events: cfg.events,
        });

        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

        // App audio thread (process loopback with system-wide fallback).
        handles.push(loopback::spawn(loopback::LoopbackParams {
            pid: cfg.pid,
            app: Arc::clone(&shared.app),
            map: shared.map.clone(),
            stop: Arc::clone(&shared.stop),
            events: cfg.events.as_ref().map(|e| e.clone()),
        }));

        // Mic thread (opt-in; the device is never opened when disabled).
        if let Some(tl) = shared.mic.as_ref().map(Arc::clone) {
            handles.push(mic::spawn(mic::MicParams {
                timeline: tl,
                map: shared.map.clone(),
                stop: Arc::clone(&shared.stop),
                events: cfg.events.as_ref().map(|e| e.clone()),
            }));
        }

        let clock_handle = clock::spawn(Arc::clone(&shared));

        Ok(Self {
            shared,
            source_handles: Mutex::new(handles),
            clock_handle: Mutex::new(Some(clock_handle)),
            shutdown_done: AtomicBool::new(false),
        })
    }

    /// Called by the capture handler on the first video frame: sets T0 and
    /// logs the C1 probe delta (spec 3.6).
    pub fn set_origin(&self, frame_ts_100ns: i64) {
        let qpc_now_100ns = qpc_to_100ns(qpc_now(), qpc_frequency());
        // C1 evidence: QPC now vs frame timestamp now. Small (delivery lag
        // only) when both share the QPC epoch.
        let _ = crate::log::log(&format!(
            "audio C1 probe at T0: qpc_100ns={qpc_now_100ns} frame_ts={frame_ts_100ns} delta={} (small implies a shared QPC epoch)",
            qpc_now_100ns - frame_ts_100ns
        ));
        self.shared
            .map
            .origin_100ns
            .store(frame_ts_100ns, Ordering::Release);
    }

    /// Toggle the mic mute (spec 4.5): while muted the mic track and its
    /// share of the mix are silent; the device stays open. Takes effect
    /// within one 20 ms chunk.
    pub fn set_mic_muted(&self, muted: bool) {
        self.shared.mic_muted.store(muted, Ordering::Relaxed);
    }

    /// Whether the mic device is muted right now.
    pub fn mic_muted(&self) -> bool {
        self.shared.mic_muted.load(Ordering::Relaxed)
    }

    /// Whether the mic track is enabled for this recording.
    pub fn mic_enabled(&self) -> bool {
        self.shared.mic_enabled
    }

    /// Shut down in the finalize order (spec 5.3). Idempotent; safe from
    /// every exit path including `Drop`.
    ///
    /// `last_video_ts_100ns` is the latest video frame timestamp (absolute
    /// frame clock). The clock emits chunks up to it so the tracks end with
    /// the video; then all threads are joined. The caller finalizes the mux
    /// only after this returns.
    pub fn shutdown(&self, last_video_ts_100ns: Option<i64>) {
        if self.shutdown_done.swap(true, Ordering::AcqRel) {
            return;
        }

        // 1. Arm the flush target, then signal stop. Order matters: the
        //    clock reads flush_to before it honours `stop`.
        *self.shared.flush_to.lock().unwrap() = last_video_ts_100ns;
        self.shared.stop.store(true, Ordering::Release);

        // 2. Join the source threads first (they place their final packets
        //    on the way out). Each exits within its event-wait timeout.
        let sources = self
            .source_handles
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        for h in sources {
            let _ = h.join();
        }

        // 3. Join the clock (drains chunks up to the last video timestamp
        //    and exits; wakes within one 20 ms period).
        if let Some(h) = self.clock_handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        // Safety net for unexpected exit paths (panics, early errors).
        self.shutdown(None);
    }
}
