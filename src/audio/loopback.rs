/// audio/loopback.rs — app audio capture (production spec section 4.2).
///
/// Primary path: WASAPI **process loopback** via `ActivateAudioInterfaceAsync`
/// with `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`, activation params
/// `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` and
/// `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` (games and launchers
/// play sound through helper processes). Requires Win10 build 20348+.
///
/// Fallback: endpoint loopback on the default render device — used when no
/// PID could be resolved, activation is unsupported, or it fails. Every
/// step of the ladder is a named, logged decision (spec rule 2).
///
/// Packet handling (spec 4.2):
/// - event-driven capture with a bounded wait,
/// - the packet's QPC position (`pu64QPCPosition`) is the packet timestamp,
/// - `AUDCLNT_BUFFERFLAGS_SILENT` becomes zeros,
/// - `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` is logged once (the timeline
///   heals it),
/// - no packets during silence is normal; the timeline/clock fills the gap.
///
/// Device invalidation ([U], spec 4.6): `AUDCLNT_E_DEVICE_INVALIDATED` tears
/// the client down and re-initialises every 2 s; the recording continues
/// with silence meanwhile. Only video or mux failures end a recording.
///
/// Dependency boundary: no tao/tray-icon/global-hotkey.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT, CloseHandle};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient,
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    ActivateAudioInterfaceAsync, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVE_FORMAT_PCM, eConsole, eRender,
};
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, implement};

use super::timeline::SharedTimeline;
use super::{AudioStatusEvent, ClockMap, qpc_frequency, qpc_to_100ns};

// Capture buffer: 200 ms (hns units of 100 ns each).
const HNS_BUFFER: i64 = 2_000_000;
// Event wait bound: the stop flag is checked at this cadence at the latest.
const WAIT_MS: u32 = 100;
// Process-loopback activation wait bound.
const ACTIVATION_TIMEOUT_MS: u32 = 5_000;

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

pub(super) struct LoopbackParams {
    /// Target PID for process loopback; `None` = system-wide loopback.
    pub pid: Option<u32>,
    pub app: SharedTimeline,
    #[allow(dead_code)] // part of the params contract; clock reads the map
    pub map: ClockMap,
    pub stop: Arc<AtomicBool>,
    pub events: Option<Sender<AudioStatusEvent>>,
}

pub(super) fn spawn(params: LoopbackParams) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("app-loopback".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
            }
            if let Err(e) = run(params) {
                let msg = format!("app audio stopped: {e}");
                crate::log::log(&msg);
            }
        })
        .expect("loopback thread spawn failed")
}

struct RunState<'a> {
    app: &'a SharedTimeline,
    events: &'a Option<Sender<AudioStatusEvent>>,
    stop: &'a Arc<AtomicBool>,
}

fn report(events: &Option<Sender<AudioStatusEvent>>, ev: AudioStatusEvent) {
    if let Some(tx) = events {
        let _ = tx.send(ev);
    }
}

fn run(params: LoopbackParams) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pid = params.pid;
    let state = RunState {
        app: &params.app,
        events: &params.events,
        stop: &params.stop,
    };

    let mut status_msg: Option<String> = None;

    loop {
        if state.stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Try process loopback first when a PID is known.
        let mut ok_init: Option<(LoopbackInit, &'static str)> = None;
        if let Some(pid) = pid {
            match attempt_process_loopback(pid) {
                Ok(init) => {
                    ok_init = Some((
                        init,
                        "App audio: process loopback, pid ownership ok (includes child tree)",
                    ));
                }
                Err(e) => {
                    crate::log::log(&format!(
                        "app audio: process loopback failed for pid {pid}: {e}; \
                         falling back to system-wide loopback"
                    ));
                }
            }
        }

        let (init, strategy) = match ok_init {
            Some(v) => v,
            None => {
                // System-wide endpoint loopback (named fallback path).
                match attempt_system_loopback() {
                    Ok(init) => (
                        init,
                        match pid {
                            Some(_) => "App audio: system-wide loopback (process loopback failed)",
                            None => {
                                "App audio: system-wide loopback (no process could be resolved)"
                            }
                        },
                    ),
                    Err(e) => {
                        // An audio failure never aborts a recording (4.6).
                        let msg = format!("App audio unavailable: {e}");
                        crate::log::log(&msg);
                        report(state.events, AudioStatusEvent::Unavailable(msg));
                        // Do not spin: wait for stop or a retry chance.
                        wait_for_stop_or_retry(state.stop, 2_000);
                        continue;
                    }
                }
            }
        };

        crate::log::log(&format!("app audio: {strategy}"));
        if let Some(m) = status_msg.take() {
            crate::log::log(&format!("app audio recovery note: {m}"));
        }
        let strategy_owned = strategy.to_string();
        report(state.events, AudioStatusEvent::Strategy(strategy_owned));

        let result = run_packet_loop(&init, state);
        match result {
            Ok(()) => {
                let _ = unsafe { init.client.Stop() };
                unsafe { init.teardown() };
                return Ok(());
            }
            Err(e) => {
                // Teardown on error paths.
                let _ = unsafe { init.client.Stop() };
                unsafe { init.teardown() };

                if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                    // Headset unplugged / default device changed (spec 4.6 [U]).
                    crate::log::log(
                        "app audio: device invalidated (0x88890004); will retry every 2 s",
                    );
                    status_msg = Some("App audio: device invalidated, reconnecting".into());
                    wait_for_stop_or_retry(state.stop, 2_000);
                    continue;
                }

                return Err(e.into());
            }
        }
    }
}

/// Sleep in WAIT_MS-bounded slices so the stop flag is honoured promptly.
fn wait_for_stop_or_retry(stop: &AtomicBool, ms: u64) {
    let mut remaining = ms;
    while remaining > 0 && !stop.load(Ordering::Relaxed) {
        let slice = remaining.min(WAIT_MS as u64);
        std::thread::sleep(std::time::Duration::from_millis(slice));
        remaining -= slice;
    }
}

// ---------------------------------------------------------------------------
// Format negotiation
// ---------------------------------------------------------------------------

/// Capture format ladder: stereo 16-bit PCM first (feeds the timeline
/// directly), stereo 32-bit float second (converted on read). Each step is
/// logged when tried.
#[derive(Clone, Copy)]
pub(crate) struct CaptureFormat {
    pub channels: u16,
    pub float: bool,
}

pub(crate) fn make_waveformatex(f: CaptureFormat) -> WAVEFORMATEX {
    let bits: u16 = if f.float { 32 } else { 16 };
    let block_align = f.channels * (bits / 8);
    WAVEFORMATEX {
        // WAVE_FORMAT_IEEE_FLOAT is not exposed by windows 0.62.2; the
        // canonical value is 0x0003 (mmreg.h).
        wFormatTag: if f.float { 0x0003 } else { WAVE_FORMAT_PCM as u16 },
        nChannels: f.channels,
        nSamplesPerSec: super::SAMPLE_RATE,
        nAvgBytesPerSec: super::SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Initialize an audio client for loopback capture with the format ladder.
/// Returns the working format.
pub(crate) fn init_format(client: &IAudioClient) -> Result<CaptureFormat, windows::core::Error> {
    let formats = [
        CaptureFormat { channels: 2, float: false },
        CaptureFormat { channels: 2, float: true },
    ];
    let mut last_err: Option<windows::core::Error> = None;
    for f in formats {
        let fmt = make_waveformatex(f);
        let result = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                HNS_BUFFER,
                0,
                &fmt as *const _,
                None,
            )
        };
        match result {
            Ok(()) => {
                crate::log::log(&format!(
                    "app audio: format {} bit, {} ch, {} Hz accepted",
                    fmt.wBitsPerSample, fmt.nChannels, fmt.nSamplesPerSec
                ));
                return Ok(f);
            }
            Err(e) => {
                crate::log::log(&format!(
                    "app audio: format {} bit rejected: 0x{:08X}; trying next in ladder",
                    fmt.wBitsPerSample,
                    e.code().0 as u32
                ));
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        windows::core::Error::from(windows::core::HRESULT(-2_147_467_260)) // E_FAIL
    }))
}

// ---------------------------------------------------------------------------
// Initialization paths
// ---------------------------------------------------------------------------

pub(crate) struct LoopbackInit {
    pub client: IAudioClient,
    pub capture: IAudioCaptureClient,
    pub event: HANDLE,
    pub format: CaptureFormat,
}

impl LoopbackInit {
    unsafe fn teardown(&self) {
        let _ = CloseHandle(self.event);
    }
}

/// Attempt per-process loopback via ActivateAudioInterfaceAsync ([U], gate T7a).
pub(crate) fn attempt_process_loopback(pid: u32) -> Result<LoopbackInit, String> {
    unsafe {
        // The event used by the completion handler to unblock this thread.
        let done = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;

        // Activation parameters as a VT_BLOB PROPVARIANT. Both the params
        // struct and the PROPVARIANT must outlive the (possibly async)
        // activation — `done` is the completion signal, so the locals stay
        // alive across the async call.
        let mut activation = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: std::mem::zeroed(),
        };
        activation.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: pid,
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        };

        let mut pv = PROPVARIANT::default();
        pv.Anonymous.Anonymous.vt = VT_BLOB;
        pv.Anonymous.Anonymous.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut activation as *mut AUDIOCLIENT_ACTIVATION_PARAMS as *mut u8,
        };

        // Shared result slot: the handler writes (HRESULT, IUnknown) and then
        // signals `done`. The Arc keeps the slot addressable from this thread
        // without casting the #[implement] COM wrapper.
        let slot: Arc<Mutex<Option<(windows::core::HRESULT, Option<IUnknown>)>>> =
            Arc::new(Mutex::new(None));
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
            done,
            slot: Arc::clone(&slot),
        }
        .into();

        let op: IActivateAudioInterfaceAsyncOperation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&pv as *const PROPVARIANT),
            &handler,
        )
        .map_err(|e| {
            format!("ActivateAudioInterfaceAsync: HRESULT 0x{:08X}", e.code().0 as u32)
        })?;

        // Wait for activation to complete (bounded).
        let wait = WaitForSingleObject(done, ACTIVATION_TIMEOUT_MS);
        let _ = CloseHandle(done);
        if wait == WAIT_TIMEOUT {
            drop(op);
            return Err(format!(
                "process loopback activation timed out ({ACTIVATION_TIMEOUT_MS} ms)"
            ));
        }

        let slot_guard = slot.lock().unwrap();
        let Some((hr, unk)) = slot_guard.as_ref() else {
            return Err("process loopback activation produced no result".into());
        };
        if hr.is_err() {
            return Err(format!("process loopback activation HRESULT 0x{:08X}", hr.0 as u32));
        }
        let unknown = unk
            .as_ref()
            .ok_or_else(|| "activation returned no interface".to_string())?;
        let client: IAudioClient = unknown
            .cast()
            .map_err(|e| format!("IUnknown::cast(IAudioClient): HRESULT 0x{:08X}", e.code().0 as u32))?;
        drop(slot_guard);
        drop(op);

        loopback_init_from_client(client, "process loopback")
    }
}

/// Endpoint loopback on the default render device (system-wide fallback).
pub(crate) fn attempt_system_loopback() -> Result<LoopbackInit, Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        loopback_init_from_client(client, "system-wide loopback").map_err(Into::into)
    }
}

/// Common tail for both strategies: format ladder init, event wiring, start.
fn loopback_init_from_client(
    client: IAudioClient,
    strategy_name: &str,
) -> Result<LoopbackInit, String> {
    unsafe {
        let format = init_format(&client).map_err(|e| {
            format!("{strategy_name}: no format accepted: 0x{:08X}", e.code().0 as u32)
        })?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("{strategy_name}: GetService: 0x{:08X}", e.code().0 as u32))?;
        let event = CreateEventW(None, false, false, None)
            .map_err(|e| format!("{strategy_name}: CreateEventW: {e}"))?;
        client
            .SetEventHandle(event)
            .map_err(|e| format!("{strategy_name}: SetEventHandle: 0x{:08X}", e.code().0 as u32))?;
        client
            .Start()
            .map_err(|e| format!("{strategy_name}: Start: 0x{:08X}", e.code().0 as u32))?;
        Ok(LoopbackInit { client, capture, event, format })
    }
}

// ---------------------------------------------------------------------------
// Process-loopback activation handler plumbing
// ---------------------------------------------------------------------------

/// COM completion handler for `ActivateAudioInterfaceAsync`.
///
/// `#[implement]` requires Send + Sync fields; the mutex slot is both.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
    slot: Arc<Mutex<Option<(windows::core::HRESULT, Option<IUnknown>)>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let op = activateoperation.ok()?;
        let mut hr = windows::core::HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        unsafe { op.GetActivateResult(&mut hr, &mut unk)? };
        *self.slot.lock().unwrap() = Some((hr, unk));
        let _ = unsafe { SetEvent(self.done) };
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Packet loop
// ---------------------------------------------------------------------------

/// Drain all available packets. Returns Err on capture errors that should
/// reconnect; Ok(()) when the packet queue is exhausted (zero frames).
///
/// Note: GetBuffer reports "no data" as `AUDCLNT_S_BUFFER_EMPTY` (a success
/// HRESULT returning zero frames in windows 0.62), so the num_frames check is
/// the end-of-drain condition; no error mapping needed.
pub(crate) fn drain_packets(
    init: &LoopbackInit,
    mut on_packet: impl FnMut(i64, &[i16], bool),
) -> Result<(), windows::core::Error> {
    unsafe {
        loop {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames = 0u32;
            let mut flags = 0u32;
            let mut qpc_position = 0u64;

            init.capture.GetBuffer(
                &mut data_ptr,
                &mut num_frames,
                &mut flags,
                None,
                Some(&mut qpc_position),
            )?;

            if num_frames == 0 {
                // No more packets ready.
                return Ok(());
            }

            let ch = init.format.channels as usize;
            let byte_count = num_frames as usize * ch * (if init.format.float { 4 } else { 2 });
            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let discontinuity = (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;

            let samples: Vec<i16> = if silent || data_ptr.is_null() {
                vec![0i16; num_frames as usize * ch]
            } else if init.format.float {
                let f32s = std::slice::from_raw_parts(data_ptr as *const f32, byte_count / 4);
                f32s.iter()
                    .map(|s| (s * i16::MAX as f32).clamp(-32768.0, 32767.0) as i16)
                    .collect()
            } else {
                std::slice::from_raw_parts(data_ptr as *const i16, byte_count / 2).to_vec()
            };

            init.capture.ReleaseBuffer(num_frames)?;

            // QPC position: WASAPI hands back ticks; convert to 100 ns.
            let freq = qpc_frequency();
            let qpc_100ns = qpc_to_100ns(qpc_position as i64, freq);

            on_packet(qpc_100ns, &samples, discontinuity);
        }
    }
}

fn run_packet_loop(init: &LoopbackInit, state: &RunState<'_>) -> Result<(), windows::core::Error> {
    let mut discontinuity_logged = false;
    loop {
        if state.stop.load(Ordering::Relaxed) {
            // Final drain: place whatever packets are already in flight.
            let app = Arc::clone(state.app);
            drain_packets(init, |qpc, samples, disc| {
                if disc && !discontinuity_logged {
                    discontinuity_logged = true;
                    crate::log::log(
                        "app audio: AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY (timeline heals gaps)",
                    );
                }
                app.lock().unwrap().place(qpc, samples);
            })?;
            return Ok(());
        }

        let wait = unsafe { WaitForSingleObject(init.event, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            let app = Arc::clone(state.app);
            drain_packets(init, |qpc, samples, disc| {
                if disc && !discontinuity_logged {
                    discontinuity_logged = true;
                    crate::log::log(
                        "app audio: AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY (timeline heals gaps)",
                    );
                }
                app.lock().unwrap().place(qpc, samples);
            })?;
        }
        // WAIT_TIMEOUT during silence is normal: the timeline/clock fills
        // the gap (spec 4.2). Loop and re-check the stop flag.
    }
}

// ---------------------------------------------------------------------------
// --audio-test WAV recorder (spec section 11, gate T7a)
// ---------------------------------------------------------------------------

/// Record `seconds` of app audio directly to a WAV file. Used by the
/// `--audio-test <pid>` dev flag: same strategy ladder as production, minus
/// the timeline/clock.
///
/// Returns Err(String) with the first hard failure; silence gaps simply
/// produce no samples (invisible for a smoke test).
pub fn record_audio_test(pid: Option<u32>, seconds: u32, path: &Path) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
    }

    let init = match pid {
        Some(pid) => match attempt_process_loopback(pid) {
            Ok(init) => {
                crate::log::log(&format!("--audio-test: process loopback for pid {pid}"));
                init
            }
            Err(e) => {
                crate::log::log(&format!(
                    "--audio-test: process loopback failed for pid {pid}: {e}; system fallback"
                ));
                attempt_system_loopback().map_err(|e| format!("system loopback: {e}"))?
            }
        },
        None => attempt_system_loopback().map_err(|e| format!("system loopback: {e}"))?,
    };

    let ch = init.format.channels as usize;
    let rate = super::SAMPLE_RATE;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut all_samples: Vec<i16> = Vec::new();

    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let wait = unsafe { WaitForSingleObject(init.event, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            drain_packets(&init, |_qpc, samples, _disc| {
                all_samples.extend_from_slice(samples);
            })
            .map_err(|e| format!("GetBuffer: 0x{:08X}", e.code().0 as u32))?;
        }
    }

    let _ = unsafe { init.client.Stop() };
    unsafe { init.teardown() };

    let byte_len = all_samples.len() * 2;
    if byte_len == 0 {
        return Err("no audio packets captured during the test window".into());
    }
    crate::log::log(&format!(
        "--audio-test: captured {} frames ({} ch @ {rate} Hz), writing {}",
        all_samples.len() / ch,
        ch,
        path.display()
    ));

    write_wav_file(path, rate as u32, ch as u16, &all_samples)
        .map_err(|e| format!("WAV write failed: {e}"))
}

/// Minimal RIFF/WAVE writer: 16-bit PCM, `channels`, `rate`.
fn write_wav_file(
    path: &Path,
    rate: u32,
    channels: u16,
    samples: &[i16],
) -> std::io::Result<()> {
    // WAVE_FORMAT_PCM as a raw u16 tag; windows exposes it as u32.
    let fmt_tag: u16 = WAVE_FORMAT_PCM as u16;
    let bits: u16 = 16;
    let block_align: u16 = channels * (bits / 8);
    let byte_rate: u32 = rate * block_align as u32;
    let data_len: u32 = (samples.len() * 2) as u32;
    let file_len: u32 = 36 + data_len;

    let mut f = std::fs::File::create(path)?;
    use std::io::Write;

    f.write_all(b"RIFF")?;
    f.write_all(&file_len.to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&fmt_tag.to_le_bytes())?;
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    f.write_all(&bytes)?;
    f.flush()?;
    Ok(())
}
