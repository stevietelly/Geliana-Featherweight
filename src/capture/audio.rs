//! Multi-source WASAPI audio pipeline used during recordings.
//!
//! Ported from `samples/audio.rs`. It captures the default render endpoint
//! (system audio, via loopback) and/or the default microphone in shared mode,
//! timestamps every packet against a shared QPC-based master clock, and feeds
//! a mixing thread that delivers 48 kHz stereo s16le PCM to caller-provided
//! sinks (typically the MP4 encoder, plus an optional mic PCM file for
//! separate-track recordings).
//!
//! Public API:
//! - [`AudioPipeline::start`]: spawns one capture thread per enabled source.
//! - [`AudioPipeline::begin_with_sinks`]: connects the mixing/writer thread to
//!   the encoder sinks once video is about to start (keeps A/V aligned).
//! - [`AudioPipeline::stop_and_join`]: stops capture, drains trailing audio,
//!   joins all threads.
//!
//! Minor additions over the sample: [`AudioPipeline::set_volumes`] and
//! [`AudioPipeline::set_mic_gate_db`], so the config's per-source gain and
//! microphone gate threshold are honored.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wasapi::{BufferInfo, SampleType, WaveFormat, initialize_mta};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Media::Audio::{
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
            IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
                WaitForSingleObject,
            },
        },
    },
    core::w,
};

use crate::log;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
const FRAMES_PER_CHUNK: usize = 960;
pub(crate) const BYTES_PER_FRAME: usize = CHANNELS as usize * std::mem::size_of::<f32>();
const MAX_BUFFER_FRAMES: usize = SAMPLE_RATE as usize * 2;
// How long the mixer waits before emitting audio, to let WASAPI buffers fill.
// 20ms is enough for WASAPI's ~10ms event cadence while keeping A/V in sync.
// (Was 250ms, which caused a noticeable audio delay behind video.)
const DEFAULT_MIX_LATENCY: Duration = Duration::from_millis(20);
const TIMESTAMP_SMOOTHING_FRAMES: u64 = SAMPLE_RATE as u64 * 70 / 1000;

pub(crate) struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

pub(crate) struct AudioScheduling(Option<HANDLE>);

impl AudioScheduling {
    pub(crate) fn enter() -> Self {
        let mut task = 0;
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut task) } {
            Ok(handle) => Self(Some(handle)),
            Err(error) => {
                log::log(&format!("audio: MMCSS scheduling unavailable: {error}"));
                Self(None)
            }
        }
    }
}

impl Drop for AudioScheduling {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

pub(crate) struct AudioEvent(pub(crate) HANDLE);
impl Drop for AudioEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Source {
    System,
    Microphone,
}

struct AudioPacket {
    start: u64,
    data: Vec<u8>,
}

impl AudioPacket {
    fn frames(&self) -> usize {
        self.data.len() / BYTES_PER_FRAME
    }
    fn end(&self) -> u64 {
        self.start + self.frames() as u64
    }
}

#[derive(Default)]
struct PacketTimeline {
    next: Option<u64>,
}

impl PacketTimeline {
    fn place(&mut self, timestamp: u64, frames: usize, discontinuity: bool) -> u64 {
        let start = match self.next {
            Some(next)
                if !discontinuity && next.abs_diff(timestamp) < TIMESTAMP_SMOOTHING_FRAMES =>
            {
                next
            }
            _ => timestamp,
        };
        self.next = Some(start + frames as u64);
        start
    }
}

#[derive(Default)]
struct SourceBuffer {
    packets: VecDeque<AudioPacket>,
    queued_frames: usize,
    consumed: u64,
    enabled: bool,
    ready: bool,
    failed: bool,
    dropped_frames: u64,
    late_frames: u64,
    discontinuities: u64,
}

impl SourceBuffer {
    fn push(&mut self, mut packet: AudioPacket) {
        if packet.start < self.consumed {
            let late = (self.consumed - packet.start).min(packet.frames() as u64) as usize;
            self.late_frames += late as u64;
            packet.data.drain(..late * BYTES_PER_FRAME);
            packet.start += late as u64;
        }
        if packet.data.is_empty() {
            return;
        }
        self.queued_frames += packet.frames();
        self.packets.push_back(packet);
        while self.queued_frames > MAX_BUFFER_FRAMES {
            let packet = self.packets.pop_front().unwrap();
            self.queued_frames -= packet.frames();
            self.dropped_frames += packet.frames() as u64;
        }
    }

    fn take(&mut self, start: u64, frames: usize) -> Vec<u8> {
        let end = start + frames as u64;
        let mut output = vec![0; frames * BYTES_PER_FRAME];
        for packet in &self.packets {
            let from = start.max(packet.start);
            let to = end.min(packet.end());
            if from < to {
                let source = (from - packet.start) as usize * BYTES_PER_FRAME;
                let target = (from - start) as usize * BYTES_PER_FRAME;
                let length = (to - from) as usize * BYTES_PER_FRAME;
                output[target..target + length]
                    .copy_from_slice(&packet.data[source..source + length]);
            }
        }
        while self
            .packets
            .front()
            .is_some_and(|packet| packet.end() <= end)
        {
            self.queued_frames -= self.packets.pop_front().unwrap().frames();
        }
        self.consumed = end;
        output
    }
}

struct ClockStart {
    instant: Instant,
    qpc_100ns: u64,
}

struct AudioClock {
    start: OnceLock<ClockStart>,
    end: OnceLock<Instant>,
    latency: Duration,
}

impl Default for AudioClock {
    fn default() -> Self {
        Self {
            start: OnceLock::new(),
            end: OnceLock::new(),
            latency: DEFAULT_MIX_LATENCY,
        }
    }
}

impl AudioClock {
    fn stop(&self) {
        self.end.get_or_init(Instant::now);
    }

    fn frames(&self, finalizing: bool) -> u64 {
        let Some(start) = self.start.get() else {
            return 0;
        };
        let until = self.end.get().copied().unwrap_or_else(Instant::now);
        let duration = until.saturating_duration_since(start.instant);
        let duration = if finalizing {
            duration
        } else {
            duration.saturating_sub(self.latency)
        };
        let frames = (duration.as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000) as u64;
        if finalizing {
            frames
        } else {
            frames / FRAMES_PER_CHUNK as u64 * FRAMES_PER_CHUNK as u64
        }
    }
}

fn qpc_100ns() -> Result<u64, windows::core::Error> {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        QueryPerformanceFrequency(&mut frequency)?;
        QueryPerformanceCounter(&mut counter)?;
    }
    Ok((counter as u128 * 10_000_000 / frequency as u128) as u64)
}

struct MicrophoneGate {
    threshold: Option<f32>,
    gain: f32,
    hold_frames: u32,
}

impl MicrophoneGate {
    fn new(threshold_db: Option<f32>) -> Self {
        Self {
            threshold: threshold_db.map(|db| 10.0_f32.powf(db.clamp(-60.0, -10.0) / 20.0)),
            gain: 0.0,
            hold_frames: 0,
        }
    }

    fn apply(&mut self, data: &mut [u8]) {
        let Some(threshold) = self.threshold else {
            return;
        };
        for frame in data.as_chunks_mut::<BYTES_PER_FRAME>().0 {
            let peak = frame
                .as_chunks::<4>()
                .0
                .iter()
                .map(|sample| f32::from_le_bytes(*sample).abs())
                .filter(|value| value.is_finite())
                .fold(0.0_f32, f32::max);
            let trigger = if self.hold_frames > 0 {
                threshold * 0.707_945_76
            } else {
                threshold
            };
            if peak >= trigger {
                self.hold_frames = SAMPLE_RATE * 150 / 1000;
            } else {
                self.hold_frames = self.hold_frames.saturating_sub(1);
            }
            if self.hold_frames > 0 {
                self.gain = (self.gain + 1.0 / (SAMPLE_RATE as f32 * 0.005)).min(1.0);
            } else {
                self.gain = (self.gain - 1.0 / (SAMPLE_RATE as f32 * 0.080)).max(0.0);
            }
            for sample in frame.as_chunks_mut::<4>().0 {
                let value = f32::from_le_bytes(*sample) * self.gain;
                sample.copy_from_slice(&value.to_le_bytes());
            }
        }
    }
}

fn float_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn f32_to_i16_pcm(data: &[u8], volume: f32) -> Vec<u8> {
    data.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|sample| {
            let val = f32::from_le_bytes(*sample) * volume;
            float_to_i16(val).to_le_bytes()
        })
        .collect()
}

fn mix_f32(a: &[u8], b: &[u8], a_volume: f32, b_volume: f32) -> Vec<u8> {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .flat_map(|(a, b)| {
            let a = f32::from_le_bytes(*a) * a_volume;
            let b = f32::from_le_bytes(*b) * b_volume;
            float_to_i16(a + b).to_le_bytes()
        })
        .collect()
}

type SourceBuffers = [Arc<Mutex<SourceBuffer>>; 2];

struct AudioMixer {
    buffers: SourceBuffers,
    volumes: [f32; 2],
    /// Live mic-mute flag, polled every mix tick: when set, the mic channel
    /// is silenced in both the mixed track and the separate mic track.
    mic_muted: Arc<AtomicBool>,
    microphone_gate: MicrophoneGate,
    position: u64,
}

impl AudioMixer {
    /// Effective mic gain: zero while muted, otherwise the configured gain.
    fn mic_volume(&self) -> f32 {
        if self.mic_muted.load(Ordering::Relaxed) {
            0.0
        } else {
            self.volumes[1]
        }
    }

    fn drain_to(
        &mut self,
        target: u64,
        send: &mut impl FnMut(&[u8]) -> Result<(), String>,
        secondary_send: &mut Option<Box<dyn FnMut(&[u8]) -> Result<(), String> + Send>>,
    ) -> Result<(), String> {
        while self.position < target {
            let frames = (target - self.position).min(FRAMES_PER_CHUNK as u64) as usize;
            let a = self.buffers[0]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(self.position, frames);
            let mut b = self.buffers[1]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(self.position, frames);
            self.microphone_gate.apply(&mut b);

            if let Some(sec) = secondary_send.as_mut() {
                let sys_pcm = f32_to_i16_pcm(&a, self.volumes[0]);
                send(&sys_pcm)?;
                let mic_pcm = f32_to_i16_pcm(&b, self.mic_volume());
                sec(&mic_pcm)?;
            } else {
                let mixed = mix_f32(&a, &b, self.volumes[0], self.mic_volume());
                send(&mixed)?;
            }
            self.position += frames as u64;

            // Log every ~5 seconds of audio mixed
            if self.position % 240_000 < frames as u64 {
                log::log(&format!(
                    "audio: mixer active — encoded {:.1}s of audio (separate_tracks={})",
                    self.position as f64 / SAMPLE_RATE as f64,
                    secondary_send.is_some()
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public AudioPipeline
// ---------------------------------------------------------------------------

pub struct AudioPipeline {
    buffers: SourceBuffers,
    volumes: [f32; 2],
    mic_gate_db: Option<f32>,
    /// Live mic-mute flag shared with the session hub; consulted by the
    /// mixer every mix tick. `None` until [`AudioPipeline::set_mic_muted_flag`].
    mic_muted: Option<Arc<AtomicBool>>,
    clock: Arc<AudioClock>,
    handles: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<Result<(), String>>>,
    sources_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl AudioPipeline {
    pub fn start(
        system_audio: bool,
        mic_audio: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<Option<Self>, String> {
        if !system_audio && !mic_audio {
            log::log("audio: pipeline requested with both system and mic disabled.");
            return Ok(None);
        }

        log::log(&format!(
            "audio: AudioPipeline::start initializing (system_loopback={system_audio}, microphone={mic_audio})"
        ));

        let mut pipeline = Self {
            buffers: [system_audio, mic_audio].map(|enabled| {
                Arc::new(Mutex::new(SourceBuffer {
                    enabled,
                    ..Default::default()
                }))
            }),
            volumes: [1.0, 1.0],
            mic_gate_db: Some(-40.0),
            mic_muted: None,
            clock: Arc::new(AudioClock::default()),
            handles: Vec::new(),
            writer: None,
            sources_done: Arc::new(AtomicBool::new(false)),
            stop,
        };

        for (index, source) in [Source::System, Source::Microphone].into_iter().enumerate() {
            if !pipeline.buffers[index].lock().unwrap().enabled {
                continue;
            }
            let buffer = pipeline.buffers[index].clone();
            let clock = pipeline.clock.clone();
            let stop = pipeline.stop.clone();
            let name = if index == 0 { "system" } else { "mic" };

            let handle = thread::Builder::new()
                .name(format!("wasapi-{name}"))
                .spawn(move || {
                    if let Err(error) = capture_loop(source, buffer.clone(), clock, stop) {
                        buffer.lock().unwrap().failed = true;
                        log::log(&format!("audio: capture_loop failed on {name}: {error}"));
                    }
                })
                .map_err(|e| format!("Failed to spawn audio thread for {name}: {e}"))?;

            pipeline.handles.push(handle);
        }

        Ok(Some(pipeline))
    }

    /// Sets the per-source gain applied by the mixer: `[system, microphone]`.
    ///
    /// Call before [`AudioPipeline::begin_with_sinks`]. Values above 1.0 are
    /// allowed (the final PCM conversion clamps samples to the i16 range).
    pub fn set_volumes(&mut self, volumes: [f32; 2]) {
        self.volumes = volumes;
    }

    /// Sets the microphone noise-gate threshold in dB, or `None` to disable
    /// gating. Call before [`AudioPipeline::begin_with_sinks`].
    pub fn set_mic_gate_db(&mut self, threshold_db: Option<f32>) {
        self.mic_gate_db = threshold_db;
    }

    /// Supplies the shared live-mute flag: while it is set, the mixer emits
    /// silence for the mic channel. Polled every mix tick (~5 ms), so muting
    /// and unmuting apply mid-recording.
    pub fn set_mic_muted_flag(&mut self, flag: Arc<AtomicBool>) {
        self.mic_muted = Some(flag);
    }

    #[allow(dead_code)]
    pub fn begin_with_sink(
        &mut self,
        send: impl FnMut(&[u8]) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<()> {
        self.begin_with_sinks(send, None)
    }

    pub fn begin_with_sinks(
        &mut self,
        mut primary_send: impl FnMut(&[u8]) -> Result<(), String> + Send + 'static,
        mut secondary_send: Option<Box<dyn FnMut(&[u8]) -> Result<(), String> + Send>>,
    ) -> std::io::Result<()> {
        log::log(&format!(
            "audio: begin_with_sinks waiting for device buffers (separate_tracks={})...",
            secondary_send.is_some()
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !self.stop.load(Ordering::Acquire) {
            if self.buffers.iter().all(|b| {
                let b = b.lock().unwrap();
                !b.enabled || b.ready || b.failed
            }) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let clock = self.clock.clone();
        let stop = self.stop.clone();
        let done = self.sources_done.clone();
        let mut mixer = AudioMixer {
            buffers: self.buffers.clone(),
            volumes: self.volumes,
            mic_muted: self
                .mic_muted
                .clone()
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            microphone_gate: MicrophoneGate::new(self.mic_gate_db),
            position: 0,
        };

        let qpc = qpc_100ns().map_err(std::io::Error::other)?;
        self.clock
            .start
            .set(ClockStart {
                instant: Instant::now(),
                qpc_100ns: qpc,
            })
            .map_err(|_| std::io::Error::other("Audio already started"))?;

        log::log(&format!("audio: master audio clock started with QPC timestamp {qpc}"));

        self.writer = Some(
            thread::Builder::new()
                .name("featherweight-audio-writer".into())
                .spawn(move || {
                    initialize_mta().ok().map_err(|e| e.to_string())?;
                    let _com = ComGuard;
                    let _scheduling = AudioScheduling::enter();

                    log::log("audio: writer thread running; feeding samples to encoder");

                    while !stop.load(Ordering::Acquire) {
                        let target = clock.frames(false);
                        mixer.drain_to(target, &mut primary_send, &mut secondary_send)?;
                        thread::sleep(Duration::from_millis(5));
                    }

                    log::log("audio: writer thread observed stop; draining trailing samples...");
                    clock.stop();
                    while !done.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(2));
                    }
                    mixer.drain_to(clock.frames(true), &mut primary_send, &mut secondary_send)?;
                    log::log(&format!(
                        "audio: writer finished. Total audio frames encoded: {}",
                        mixer.position
                    ));
                    Ok(())
                })
                .map_err(std::io::Error::other)?,
        );

        Ok(())
    }

    pub fn stop_and_join(&mut self) -> Result<(), String> {
        log::log("audio: stop_and_join requested; stopping clock and joining threads...");
        self.clock.stop();
        self.stop.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
        self.sources_done.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| "Audio encoder thread panicked".to_string())??;
        }
        log::log("audio: all capture and writer threads joined cleanly.");
        Ok(())
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

// ---------------------------------------------------------------------------
// WASAPI Capture Loop
// ---------------------------------------------------------------------------

fn capture_loop(
    source: Source,
    buffer: Arc<Mutex<SourceBuffer>>,
    clock: Arc<AudioClock>,
    stop: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source_name = match source {
        Source::System => "System Loopback",
        Source::Microphone => "Microphone",
    };

    let _ = initialize_mta().ok();
    let _com = ComGuard;
    let _scheduling = AudioScheduling::enter();

    log::log(&format!("audio: {source_name} initializing COM and audio endpoint"));

    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let device = unsafe {
        enumerator.GetDefaultAudioEndpoint(
            match source {
                Source::System => eRender,
                Source::Microphone => eCapture,
            },
            eConsole,
        )?
    };

    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
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
        )?;
    }

    let event = AudioEvent(unsafe { CreateEventW(None, false, false, None)? });
    unsafe {
        client.SetEventHandle(event.0)?;
    }

    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    buffer.lock().unwrap().ready = true;
    log::log(&format!("audio: {source_name} endpoint ready, waiting for clock start"));

    while clock.start.get().is_none() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }

    let origin = clock.start.get().unwrap();
    unsafe {
        client.Start()?;
    }
    log::log(&format!("audio: {source_name} client.Start() active"));

    let mut timeline = PacketTimeline::default();
    let mut seen_packet = false;
    let mut total_packets = 0u64;

    let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            while unsafe { capture.GetNextPacketSize()? } > 0 {
                let (mut data, info) = read_capture_packet(&capture)?;
                let frames = data.len() / BYTES_PER_FRAME;
                if frames == 0 {
                    continue;
                }

                if !seen_packet {
                    log::log(&format!(
                        "audio: {source_name} first packet arrived: {frames} frames (flags: {:?})",
                        info.flags
                    ));
                }

                total_packets += 1;
                let relative = i128::from(info.timestamp) - i128::from(origin.qpc_100ns);
                let frame = relative * i128::from(SAMPLE_RATE) / 10_000_000;
                if frame < 0 {
                    let trim = (-frame).min(frames as i128) as usize;
                    data.drain(..trim * BYTES_PER_FRAME);
                }
                let start = frame.max(0) as u64;

                let start = timeline.place(
                    start,
                    data.len() / BYTES_PER_FRAME,
                    info.flags.data_discontinuity,
                );

                let mut buf = buffer.lock().unwrap();
                if seen_packet && info.flags.data_discontinuity {
                    buf.discontinuities += 1;
                }
                buf.push(AudioPacket { start, data });
                seen_packet = true;
            }

            if stop.load(Ordering::Acquire) {
                break;
            }

            unsafe {
                WaitForSingleObject(event.0, 10);
            }
        }
        Ok(())
    })();

    let _ = unsafe { client.Stop() };
    log::log(&format!(
        "audio: {source_name} capture loop finished. Total packets processed: {total_packets}"
    ));
    result
}

pub(crate) fn read_capture_packet(
    capture: &IAudioCaptureClient,
) -> Result<(Vec<u8>, BufferInfo), Box<dyn std::error::Error + Send + Sync>> {
    let mut data = std::ptr::null_mut();
    let mut frames = 0;
    let mut flags = 0;
    let mut position = 0;
    let mut timestamp = 0;

    unsafe {
        capture.GetBuffer(
            &mut data,
            &mut frames,
            &mut flags,
            Some(&mut position),
            Some(&mut timestamp),
        )?;
    }

    let info = BufferInfo::new(flags, position, timestamp);
    let result = if frames == 0 || info.flags.silent {
        Ok(vec![0; frames as usize * BYTES_PER_FRAME])
    } else if data.is_null() {
        Err(std::io::Error::other("WASAPI returned a null audio packet"))
    } else {
        Ok(unsafe { std::slice::from_raw_parts(data, frames as usize * BYTES_PER_FRAME) }.to_vec())
    };

    if frames != 0 {
        unsafe {
            capture.ReleaseBuffer(frames)?;
        }
    }

    Ok((result?, info))
}
