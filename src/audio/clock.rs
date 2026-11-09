/// audio/clock.rs — fixed-cadence chunk emission + mixing (spec 4.3).
///
/// A high-resolution waitable timer ticks every 20 ms. Each tick computes
/// `target = qpc_now - T0 - MARGIN` (in video-clock 100 ns units) and emits
/// one 960-sample (20 ms) chunk per track for every whole chunk below the
/// target:
///
/// 1. Take 960 frames from each source accumulator; missing data is zeros.
/// 2. App track = app samples; mic track = mic samples (zeros when muted).
/// 3. Mix = app + mic (mono mic duplicated into both channels, summed
///    without clipping — see `mix_stereo`).
/// 4. Each chunk is written as an IMFSample with timestamp = chunk start
///    (100 ns) and duration exactly 20 ms.
///
/// Timestamps are contiguous by construction, so the AAC packet count equals
/// `duration x 48000 / 1024` within ~0.5% ([GATE] T5).
///
/// On shutdown (spec 5.3) the clock emits the remaining chunks up to the
/// last video timestamp, then exits. Sources must be joined before this
/// thread joins so their final packets are placed first.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::WaitForSingleObject;
use windows::Win32::Media::MediaFoundation::IMFSample;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS, CreateWaitableTimerExW,
    SetWaitableTimer,
};

use super::Shared;
use super::{
    CHUNK_100NS, CHUNK_FRAMES, CLOCK_OFFSET_100NS, MARGIN_100NS, SAMPLE_RATE, make_pcm_sample,
    qpc_frequency, qpc_now, qpc_to_100ns,
};
use crate::mux::MuxWriter;

/// Mutable state of the running audio clock.
pub(super) struct ClockState {
    /// Absolute start frame (relative to T0) of the next chunk to emit.
    next_frame: AtomicI64,
    /// Rate-limited counters for write-error logs.
    written_chunks: AtomicU64,
    write_errors: AtomicU64,
}

impl ClockState {
    pub(super) fn new() -> Self {
        Self {
            next_frame: AtomicI64::new(0),
            written_chunks: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
        }
    }
}

pub(super) fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("audio-clock".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
            }
            run_loop(shared);
        })
        .expect("audio clock thread spawn failed")
}

fn run_loop(shared: Arc<Shared>) {
    let freq = qpc_frequency();

    // High-resolution waitable timer (spec 4.3). Failure falls back to a
    // Sleep-based cadence — logged, named fallback (spec rule 2).
    let timer = unsafe {
        CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
    };
    if let Err(e) = &timer {
        crate::log::log(&format!(
            "audio clock: high-resolution timer unavailable ({e}); falling back to Sleep cadence"
        ));
    }
    let timer = timer.ok();

    if let Some(t) = &timer {
        // Relative due time of -20 ms (in 100 ns units), repeating period 20 ms.
        let due: i64 = -200_000;
        let _ = unsafe { SetWaitableTimer(t, &due, CHUNK_MS as i32, None, None, false) };
    }

    crate::log::log(&format!(
        "audio clock started: cadence={}ms chunk={} frames margin={}ms",
        CHUNK_MS,
        CHUNK_FRAMES,
        MARGIN_100NS / 100_000
    ));

    loop {
        // Wait for the timer (or Sleep fallback), bounded so the stop flag is
        // always observed promptly.
        if let Some(t) = &timer {
            let _wait = unsafe { WaitForSingleObject(t, 100) };
            // Timeout is harmless: loop round and re-check flags.
        } else {
            std::thread::sleep(std::time::Duration::from_millis(CHUNK_MS));
        }

        let stop = shared.stop.load(Ordering::Acquire);
        let origin = shared.map.origin_100ns.load(Ordering::Relaxed);

        if origin == 0 {
            // No video frame yet: nothing has a timeline position. On stop,
            // there is nothing to flush — the file has no recorded audio.
            if stop {
                break;
            }
            continue;
        }

        // Flush target or regular cadence margin.
        let target_abs = if stop {
            let flush = *shared.flush_to.lock().unwrap();
            match flush {
                Some(ts) => ts,
                None => break, // no video ever arrived; nothing to flush
            }
        } else {
            let now = qpc_to_100ns(qpc_now(), freq);
            now - CLOCK_OFFSET_100NS - origin - MARGIN_100NS
        };

        // While not stopping, wait for the mux writer to be created (it is
        // created lazily on the first video frame, same moment as T0).
        if !stop && !shared.mux.is_ready() {
            continue;
        }

        // Target chunk index, floored to whole chunks (960-frame grid).
        let target_rel_100ns = (target_abs - origin).max(0);
        let target_frame =
            ((target_rel_100ns as i128 * SAMPLE_RATE as i128) / 10_000_000i128) as i64;
        let target_chunk = target_frame / CHUNK_FRAMES as i64;

        let mut next_chunk = shared.clock_state.next_frame.load(Ordering::Relaxed);
        while next_chunk <= target_chunk {
            let result = emit_chunk(&shared, next_chunk);
            match result {
                Ok(()) => {
                    next_chunk += 1;
                    shared.clock_state.next_frame.store(next_chunk, Ordering::Relaxed);
                    shared.clock_state.written_chunks.fetch_add(1, Ordering::Relaxed);
                }
                Err(EmitAbort::MuxNotReady) => break,
            }
        }

        if stop {
            break;
        }
    }

    let written = shared.clock_state.written_chunks.load(Ordering::Relaxed);
    let errors = shared.clock_state.write_errors.load(Ordering::Relaxed);
    crate::log::log(&format!(
        "audio clock stopped: chunks_written={} write_errors={}",
        written, errors
    ));
}

enum EmitAbort {
    /// Mux not ready (no sink writer): pause emission, do not advance.
    MuxNotReady,
}

/// Emit one 20 ms chunk for all configured tracks starting at `chunk`.
fn emit_chunk(shared: &Shared, chunk: i64) -> Result<(), EmitAbort> {
    if !shared.mux.is_ready() {
        return Err(EmitAbort::MuxNotReady);
    }

    let start_frame = chunk * CHUNK_FRAMES as i64;
    let time_100ns = start_frame * 10_000_000i64 / SAMPLE_RATE as i64; // exact on the 960 grid

    // App source (stereo). Zeros where this source never delivered.
    let app_samples: Vec<i16> = {
        let mut tl = shared.app.lock().unwrap();
        tl.take(start_frame, CHUNK_FRAMES)
    };

    // Mic source (mono). Zeros when disabled or muted (spec 4.5).
    let mic_samples: Option<Vec<i16>> = if shared.mic_enabled {
        let muted = shared.mic_muted.load(Ordering::Relaxed);
        Some(match (&shared.mic, muted) {
            (Some(tl), false) => tl.lock().unwrap().take(start_frame, CHUNK_FRAMES),
            _ => vec![0i16; CHUNK_FRAMES as usize],
        })
    } else {
        None
    };

    // Track 0 = mix (written first so every player plays the full clip,
    // spec 4.1), track 1 = app-only, track 2 = mic-only.
    let mixed: Vec<i16> = match &mic_samples {
        Some(mic) => mix_stereo(&app_samples, mic),
        None => app_samples.clone(),
    };
    let mix_sample = super::make_pcm_sample(&mixed, time_100ns, CHUNK_100NS);
    write_track(&shared.mux, 0, &mix_sample, &shared.clock_state.write_errors);

    let app_sample = super::make_pcm_sample(&app_samples, time_100ns, CHUNK_100NS);
    write_track(&shared.mux, 1, &app_sample, &shared.clock_state.write_errors);

    if let Some(mic) = &mic_samples {
        let mic_sample = super::make_pcm_sample(mic, time_100ns, CHUNK_100NS);
        write_track(&shared.mux, 2, &mic_sample, &shared.clock_state.write_errors);
    }

    Ok(())
}

/// Mix: app stereo + mono mic duplicated to both channels, summed with hard
/// clamping (spec 4.4). No gain, no ducking.
fn mix_stereo(app: &[i16], mic_mono: &[i16]) -> Vec<i16> {
    app.iter()
        .enumerate()
        .map(|(i, a)| {
            let mic_head = (i / 2) % mic_mono.len().max(1);
            let m = mic_mono[mic_head] as i32;
            let out = (*a as i32) + m;
            out.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        })
        .collect()
}

fn write_track(mux: &MuxWriter, track: usize, sample: &IMFSample, error_count: &AtomicU64) {
    if let Err(e) = mux.write_audio(track, sample) {
        let count = error_count.fetch_add(1, Ordering::Relaxed);
        // Log the first error, then every 100th — per-write logging is
        // banned, but a persistent failure must leave log evidence.
        if count == 0 || count % 100 == 99 {
            crate::log::log(&format!(
                "audio write error (track {track}, n={count}): 0x{:08X} ({e})",
                e.code().0 as u32
            ));
        }
    }
}
