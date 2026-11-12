//! Live audio level preview for the idle window.
//!
//! While no recording runs, this module opens lightweight WASAPI capture
//! streams (system loopback and/or microphone) purely to measure signal
//! peaks, so the volume sliders in the settings view can show real-time
//! meters. It shares the open sequence and RAII helpers with
//! [`super::audio`] but nothing else: no buffers, no mixer, no encoder
//! sinks, and no master clock — one thread per source, publishing decayed
//! peaks through atomics.
//!
//! Levels are raw (0.0–1.0+) *before* the user's volume gain; the sliders
//! apply the gain when painting, so the meter shows what the recording
//! would capture at the current slider position.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use wasapi::{SampleType, WaveFormat, initialize_mta};
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
        IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
    },
    System::{
        Com::{CLSCTX_ALL, CoCreateInstance},
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

use super::audio::{
    AudioEvent, AudioScheduling, ComGuard, Source, CHANNELS, SAMPLE_RATE, read_capture_packet,
};
use crate::log;

/// How fast the meter falls back toward silence: the peak decays to 25%
/// (~-12 dBFS) per second, which reads as motion without flickering.
const PEAK_DECAY_PER_SEC: f32 = 0.25;
/// Event-wait timeout; also bounds how often the meter decays while the
/// loopback source is silent (WASAPI delivers no packets for silence).
const EVENT_TIMEOUT_MS: u32 = 10;

/// Levels are stored as f32 bits in `AtomicU32`s — plain integer atomics,
/// no version-dependent float atomic availability.
type Levels = [AtomicU32; 2];

/// Shared atomic levels: index 0 = system loopback, 1 = microphone.
pub struct AudioPreview {
    levels: Arc<Levels>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl AudioPreview {
    /// Starts one capture thread per enabled source. Fails only if all
    /// sources are disabled; an individual source's failure is logged and
    /// that channel's meter simply stays at zero.
    pub fn start(system_audio: bool, microphone: bool) -> Result<Self, String> {
        if !(system_audio || microphone) {
            return Err("audio preview requested with all sources disabled".into());
        }
        let levels: Arc<Levels> = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (slot, (enabled, source)) in [
            (system_audio, Source::System),
            (microphone, Source::Microphone),
        ]
        .into_iter()
        .enumerate()
        {
            if !enabled {
                continue;
            }
            let levels = levels.clone();
            let stop = stop.clone();
            let name = match source {
                Source::System => "preview-system".into(),
                Source::Microphone => "preview-mic".into(),
            };
            let handle = thread::Builder::new().name(name).spawn(move || {
                if let Err(error) = capture_peak_loop(source, slot, &levels, &stop) {
                    store_level(&levels[slot], 0.0);
                    log::log(&format!("audio preview: {error}"));
                }
            });
            threads.push(handle.map_err(|e| format!("could not spawn preview thread: {e}"))?);
        }
        Ok(Self {
            levels,
            stop,
            threads,
        })
    }

    /// Raw (pre-gain) peaks: [system loopback, microphone].
    pub fn levels(&self) -> [f32; 2] {
        [load_level(&self.levels[0]), load_level(&self.levels[1])]
    }

    /// Stops capture and joins the threads; also called by `Drop`.
    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioPreview {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn store_level(slot: &AtomicU32, level: f32) {
    slot.store(level.to_bits(), Ordering::Relaxed);
}

fn load_level(slot: &AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

/// WASAPI event-driven capture that only measures peaks. Mirrors the open
/// sequence in `audio::capture_loop` (default endpoint, shared mode,
/// 48 kHz stereo f32, loopback flag for the render endpoint) but never
/// buffers or timestamps anything.
fn capture_peak_loop(
    source: Source,
    slot: usize,
    levels: &Levels,
    stop: &AtomicBool,
) -> Result<(), String> {
    let source_name = match source {
        Source::System => "system loopback",
        Source::Microphone => "microphone",
    };
    let _ = initialize_mta().ok();
    let _com = ComGuard;
    let _scheduling = AudioScheduling::enter();

    log::log(&format!("audio preview: {source_name} opening endpoint"));

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("{source_name}: device enumerator: {e}"))?;
    let device = unsafe {
        enumerator.GetDefaultAudioEndpoint(
            match source {
                Source::System => eRender,
                Source::Microphone => eCapture,
            },
            eConsole,
        )
    }
    .map_err(|e| format!("{source_name}: default endpoint: {e}"))?;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| format!("{source_name}: activate endpoint: {e}"))?;

    let format = WaveFormat::new(
        32,
        32,
        SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS as usize,
        None,
    );
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if matches!(source, Source::System) {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }

    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            1_000_000,
            0,
            format.as_waveformatex_ref(),
            None,
        )
    }
    .map_err(|e| format!("{source_name}: initialize: {e}"))?;

    let event = AudioEvent(
        unsafe { CreateEventW(None, false, false, None) }
            .map_err(|e| format!("{source_name}: event: {e}"))?,
    );
    unsafe { client.SetEventHandle(event.0) }
        .map_err(|e| format!("{source_name}: event handle: {e}"))?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }
        .map_err(|e| format!("{source_name}: capture service: {e}"))?;

    unsafe { client.Start() }.map_err(|e| format!("{source_name}: start: {e}"))?;
    log::log(&format!("audio preview: {source_name} streaming"));

    let mut peak = 0.0f32;
    let mut last_update = Instant::now();
    loop {
        let mut packet_seen = false;
        while unsafe { capture.GetNextPacketSize() }
            .map_err(|e| format!("{source_name}: packet size: {e}"))?
            > 0
        {
            let (data, _info) =
                read_capture_packet(&capture).map_err(|e| format!("{source_name}: {e}"))?;
            if data.is_empty() {
                continue;
            }
            // Peak of this packet: largest |sample| across interleaved f32
            // frames (same math as the recording gate in audio.rs).
            let packet_peak = data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|sample| f32::from_le_bytes(*sample).abs())
                .fold(0.0f32, f32::max);
            let dt = last_update.elapsed().as_secs_f32();
            last_update = Instant::now();
            peak = packet_peak.max(peak * PEAK_DECAY_PER_SEC.powf(dt));
            store_level(&levels[slot], peak);
            packet_seen = true;
        }
        if !packet_seen {
            // No packets this tick (loopback silence, idle mic): keep the
            // bar falling so the meter visibly rests instead of sticking.
            let dt = last_update.elapsed().as_secs_f32();
            last_update = Instant::now();
            peak *= PEAK_DECAY_PER_SEC.powf(dt);
            store_level(&levels[slot], peak);
        }

        if stop.load(Ordering::Acquire) {
            break;
        }
        unsafe {
            WaitForSingleObject(event.0, EVENT_TIMEOUT_MS);
        }
    }

    let _ = unsafe { client.Stop() };
    // The meter rests at zero while idle, not at the last peak.
    store_level(&levels[slot], 0.0);
    log::log(&format!("audio preview: {source_name} stopped"));
    Ok(())
}
