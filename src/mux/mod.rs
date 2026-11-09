/// mux/ — IMFSinkWriter wrapper: stream management, thread-safe write, finalize.
///
/// All IMFSinkWriter calls go through one Mutex here (section 2.1 of the spec).
/// Video writes come from the capture thread; audio writes come from the audio
/// clock thread. Contention is negligible: video writes take ~20-80 µs, audio
/// writes occur ~150 times per second total.
///
/// Dependency boundary: this module must NOT import tao, tray-icon, or global-hotkey.
pub mod container;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFAttributes, IMFSinkWriter, IMFTransform, MF_API_VERSION, MF_LOW_LATENCY,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_SINK_WRITER_DISABLE_THROTTLING, MFSTARTUP_NOSOCKET, MFCreateAttributes,
    MFCreateSinkWriterFromURL, MFShutdown, MFStartup,
    // Codec API GUIDs for bitrate/GOP control
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, eAVEncCommonRateControlMode_CBR,
    // H.264 profile/level for negotiation
    eAVEncH264VLevel4_1, eAVEncH264VProfile_Base, eAVEncH264VProfile_High,
    eAVEncH264VProfile_Main,
    // Encoder identification attributes
    MFT_FRIENDLY_NAME_Attribute, MF_SA_D3D11_AWARE,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Variant::{VARIANT, VT_I4, VT_UI4};
use windows::core::{GUID, HSTRING, Interface};

use crate::log::log;
use crate::mux::container::{ContainerMode, apply_container_attributes};
use crate::video::device;
use crate::video::encode;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the mux writer. Set at construction, read-only after.
#[derive(Debug, Clone)]
pub struct MuxConfig {
    pub width: u32,
    pub height: u32,
    pub output_path: String,
    /// Number of audio tracks: 0 (video-only), 2 (mix+app), or 3 (mix+app+mic).
    pub audio_track_count: usize,
    pub container_mode: ContainerMode,
    pub low_latency: bool,
    pub mic_track_enabled: bool,
}

/// Encoder info discovered during init. Read-only after initialize().
#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub name: String,
    pub is_software: bool,
    pub profile: &'static str,
}

// ---------------------------------------------------------------------------
// MuxWriter
// ---------------------------------------------------------------------------

/// Thread-safe wrapper around IMFSinkWriter.
///
/// The sink writer is created lazily on the first frame (section 3.2) because
/// the D3D device comes from the first capture frame. After init, video and
/// audio writes are serialized through the internal Mutex.
pub struct MuxWriter {
    config: MuxConfig,
    state: Mutex<MuxState>,
    ready: AtomicBool,
    encoder_info: Mutex<Option<EncoderInfo>>,
}

struct MuxState {
    sink_writer: Option<IMFSinkWriter>,
    video_stream: u32,
    audio_streams: Vec<u32>,
    finalized: bool,
}

// SAFETY: IMFSinkWriter is used in MTA (multi-threaded apartment) mode.
// All access is serialized through the Mutex. The capture thread and any
// audio threads all use CoInitializeEx(MTA), making cross-thread COM access safe.
unsafe impl Send for MuxState {}

impl MuxWriter {
    /// Create a MuxWriter. The sink writer is NOT created yet — call
    /// `initialize()` on the first frame.
    pub fn new(config: MuxConfig) -> Self {
        Self {
            config,
            state: Mutex::new(MuxState {
                sink_writer: None,
                video_stream: 0,
                audio_streams: Vec::new(),
                finalized: false,
            }),
            ready: AtomicBool::new(false),
            encoder_info: Mutex::new(None),
        }
    }

    /// Lazy initialization on the first frame.
    ///
    /// Init order (section 3.2 — do not reorder):
    /// 1. CoInitializeEx(MTA) — idempotent, capture thread already has MTA.
    /// 2. MFStartup.
    /// 3. Enable D3D multithread protection.
    /// 4. Create DXGI device manager and ResetDevice with the capture device.
    /// 5. Create attributes (hardware transforms, D3D manager, disable
    ///    throttling, low latency, container type).
    /// 6. Create sink writer from URL.
    /// 7. AddStream + SetInputMediaType for video (with profile negotiation).
    /// 8. AddStream + SetInputMediaType for each audio track.
    /// 9. BeginWriting last.
    /// 10. Identify encoder (section 3.7).
    /// 11. Try ICodecAPI bitrate/GOP control (best-effort, [U]).
    /// 12. Set ready flag.
    pub fn initialize(
        &self,
        capture_device: &ID3D11Device,
        _capture_context: &ID3D11DeviceContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 1. COM init (idempotent in MTA).
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };

        // 2. MFStartup.
        unsafe { MFStartup(MF_API_VERSION, MFSTARTUP_NOSOCKET)? };

        // 3. Multithread protection.
        device::enable_multithread(capture_device)?;

        // 4. Device manager.
        let (device_manager, _token) = device::create_device_manager(capture_device)?;

        // 5. Attributes.
        let attrs = mf_create_attributes(8)?;
        unsafe {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &device_manager)?;
            if self.config.low_latency {
                attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
            }
        }
        apply_container_attributes(&attrs, self.config.container_mode)?;

        // 6. Create sink writer from URL.
        let path = HSTRING::from(self.config.output_path.as_str());
        let sink_writer: IMFSinkWriter = unsafe { MFCreateSinkWriterFromURL(&path, None, Some(&attrs))? };

        log(&format!(
            "MuxWriter init: {}x{} container={} audio_tracks={}",
            self.config.width,
            self.config.height,
            self.config.container_mode.as_str(),
            self.config.audio_track_count,
        ));

        // 7. Add video stream with profile negotiation.
        let bitrate = encode::scale_bitrate(self.config.width, self.config.height);
        let (video_stream, profile_name) =
            negotiate_video_stream(&sink_writer, self.config.width, self.config.height, bitrate)?;

        // 8. Add audio streams.
        let mut audio_streams = Vec::new();
        for i in 0..self.config.audio_track_count {
            // Track 0 = mix, 1 = app (both stereo), 2 = mic (mono or stereo fallback).
            let is_mic = i == 2 && self.config.mic_track_enabled;
            let idx = add_audio_stream(&sink_writer, is_mic)?;
            audio_streams.push(idx);
        }

        // 9. BeginWriting last (section 3.2).
        // Must come after ResetDevice so the encoder has the D3D device.
        unsafe { sink_writer.BeginWriting()? };
        log("MuxWriter: BeginWriting OK");

        // 10. Identify encoder (section 3.7).
        let encoder_info = identify_encoder(&sink_writer, video_stream, profile_name);
        if let Some(ref info) = encoder_info {
            log(&format!(
                "Encoder: {} ({}aware){}",
                info.name,
                if info.is_software { "SOFTWARE " } else { "" },
                if info.is_software { " — WARNING: expect high CPU" } else { "" },
            ));
            *self.encoder_info.lock().unwrap() = Some(info.clone());
        }

        // 11. Try ICodecAPI bitrate/GOP control (best-effort, [U]).
        try_set_codec_api_params(&sink_writer, video_stream, bitrate);

        // 12. Store state and set ready.
        {
            let mut state = self.state.lock().unwrap();
            state.sink_writer = Some(sink_writer);
            state.video_stream = video_stream;
            state.audio_streams = audio_streams;
        }
        self.ready.store(true, Ordering::Release);

        Ok(())
    }

    /// Write a video sample. Called from the capture thread.
    pub fn write_video(&self, sample: &windows::Win32::Media::MediaFoundation::IMFSample) -> windows::core::Result<()> {
        if !self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let state = self.state.lock().unwrap();
    let Some(ref sw) = state.sink_writer else {
        return Ok(());
    };
    if state.finalized {
        return Ok(());
    }
    unsafe { sw.WriteSample(state.video_stream, sample)? };
    Ok(())
    }

    /// Write an audio sample to the given track index (0=mix, 1=app, 2=mic).
    /// Called from the audio clock thread.
    pub fn write_audio(
        &self,
        track: usize,
        sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    ) -> windows::core::Result<()> {
        if !self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let state = self.state.lock().unwrap();
    let Some(ref sw) = state.sink_writer else {
        return Ok(());
    };
    if state.finalized || track >= state.audio_streams.len() {
        return Ok(());
    }
    unsafe { sw.WriteSample(state.audio_streams[track], sample)? };
    Ok(())
    }

    /// Finalize the sink writer. Must be called exactly once.
    ///
    /// Order (section 5.3):
    /// 1. Video submission has stopped (caller's responsibility).
    /// 2. Audio clock has flushed remaining chunks (caller's responsibility).
    /// 3. Stop audio sources and join threads (caller's responsibility).
    /// 4. Finalize() — this method.
    /// 5. Caller reports CaptureEnded.
    pub fn finalize(&self) -> windows::core::Result<()> {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };

        let mut state = self.state.lock().unwrap();
        if state.finalized {
            return Ok(());
        }
        state.finalized = true;

        if let Some(sw) = state.sink_writer.take() {
            log("MuxWriter: Finalizing");
            unsafe { sw.Finalize()? };
        }

        // MFShutdown pairs with MFStartup from initialize().
        unsafe { MFShutdown()? };
        log("MuxWriter: Done");
        Ok(())
    }

    /// Whether the sink writer is ready (after BeginWriting).
    /// Audio threads check this before writing.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Encoder info discovered during init. None if not yet initialized.
    pub fn encoder_info(&self) -> Option<EncoderInfo> {
        self.encoder_info.lock().unwrap().clone()
    }

    /// Container mode in use.
    pub fn container_mode(&self) -> ContainerMode {
        self.config.container_mode
    }
}

impl Drop for MuxWriter {
    fn drop(&mut self) {
        // Safety net: if finalize() was not called, finalize now.
        let _ = self.finalize();
    }
}

// ---------------------------------------------------------------------------
// MF out-param helper (windows 0.62 pattern)
// ---------------------------------------------------------------------------

fn mf_create_attributes(count: u32) -> windows::core::Result<IMFAttributes> {
    let mut out: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut out, count)? };
    Ok(out.unwrap())
}

// ---------------------------------------------------------------------------
// Profile negotiation (section 3.5)
// ---------------------------------------------------------------------------

/// Try adding the video stream with H.264 profile High, falling back to Main
/// then Baseline. Returns (stream_index, profile_name).
///
/// [U] Intel Quick Sync negotiated Constrained Baseline when no profile was set.
/// Requesting High is expected to work but has not been tested.
fn negotiate_video_stream(
    sink_writer: &IMFSinkWriter,
    width: u32,
    height: u32,
    bitrate: u32,
) -> Result<(u32, &'static str), Box<dyn std::error::Error + Send + Sync>> {
    let input_type = encode::make_video_input_type(width, height)?;

    for (profile, name) in [
        (eAVEncH264VProfile_High, "High"),
        (eAVEncH264VProfile_Main, "Main"),
        (eAVEncH264VProfile_Base, "Baseline"),
    ] {
        let output_type =
            encode::make_video_output_type(width, height, bitrate, profile, eAVEncH264VLevel4_1)?;
        match unsafe { sink_writer.AddStream(&output_type) } {
            Ok(idx) => {
                unsafe { sink_writer.SetInputMediaType(idx, &input_type, None)? };
                log(&format!("Video stream added: index={idx}, profile={name}, bitrate={bitrate}"));
                return Ok((idx, name));
            }
            Err(e) => {
                log(&format!(
                    "H.264 {name} profile rejected: HRESULT=0x{:08X} ({e})",
                    e.code().0 as u32
                ));
            }
        }
    }

    Err("All H.264 profiles rejected by the sink writer".into())
}

// ---------------------------------------------------------------------------
// Audio stream addition (section 4.1)
// ---------------------------------------------------------------------------

/// Add an audio stream. Stereo tracks use 16000 bytes/sec (128 kbps).
/// The mic track tries mono 12000 bytes/sec first, falling back to stereo.
///
/// [U] The spike measured 163-166 kb/s with its own settings; these requested
/// values have not been verified. [GATE] T4.
fn add_audio_stream(
    sink_writer: &IMFSinkWriter,
    is_mic: bool,
) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    if is_mic {
        // Try mono first (section 4.1: "for the mono mic try 12000").
        let mono_out = encode::make_audio_output_type(1, 12_000)?;
        match unsafe { sink_writer.AddStream(&mono_out) } {
            Ok(idx) => {
                let mono_in = encode::make_audio_input_type(1)?;
                unsafe { sink_writer.SetInputMediaType(idx, &mono_in, None)? };
                log("Mic audio stream added: mono, 96 kbps");
                return Ok(idx);
            }
            Err(e) => {
                log(&format!(
                    "Mono mic track rejected (0x{:08X}), falling back to stereo",
                    e.code().0 as u32
                ));
            }
        }
    }

    // Stereo fallback (or default for mix/app tracks).
    let stereo_out = encode::make_audio_output_type(2, 16_000)?;
    let idx = unsafe { sink_writer.AddStream(&stereo_out) }?;
    let stereo_in = encode::make_audio_input_type(2)?;
    unsafe { sink_writer.SetInputMediaType(idx, &stereo_in, None)? };
    log(&format!("Audio stream added: stereo, 128 kbps, index={idx}"));
    Ok(idx)
}

// ---------------------------------------------------------------------------
// Encoder identification (section 3.7)
// ---------------------------------------------------------------------------

/// Get the encoder MFT for the video stream and read its friendly name.
/// If the name contains "Microsoft", flag it as software.
fn identify_encoder(
    sink_writer: &IMFSinkWriter,
    video_stream: u32,
    profile_name: &'static str,
) -> Option<EncoderInfo> {
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let result = unsafe {
        sink_writer.GetServiceForStream(
            video_stream,
            &GUID::zeroed(),
            &IMFTransform::IID,
            &mut ptr,
        )
    };
    if result.is_err() {
        log("Could not get encoder transform via GetServiceForStream");
        return Some(EncoderInfo {
            name: "Unknown".into(),
            is_software: false,
            profile: profile_name,
        });
    }

    let transform: IMFTransform = unsafe { std::mem::transmute(ptr) };

    let attrs = match unsafe { transform.GetAttributes() } {
        Ok(a) => a,
        Err(_) => {
            return Some(EncoderInfo {
                name: "Unknown".into(),
                is_software: false,
                profile: profile_name,
            });
        }
    };

    // Read friendly name. GetString fills a caller-allocated buffer.
    let mut name_buf = [0u16; 512];
    let name = match unsafe {
        attrs.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name_buf, None)
    } {
        Ok(()) => {
            let len = name_buf.iter().position(|&c| c == 0).unwrap_or(name_buf.len());
            String::from_utf16_lossy(&name_buf[..len])
        }
        Err(_) => "Unknown".into(),
    };

    let d3d_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }
        .map(|v| v != 0)
        .unwrap_or(false);

    let is_software = name.contains("Microsoft") || name.contains("mfh264enc");

    if is_software {
        log("WARNING: Software encoder detected — expect high CPU usage");
    } else if !d3d_aware {
        log("WARNING: Encoder is not D3D-aware — may use software fallback");
    }

    Some(EncoderInfo {
        name,
        is_software,
        profile: profile_name,
    })
}

// ---------------------------------------------------------------------------
// ICodecAPI bitrate/GOP control (section 3.5, [U])
// ---------------------------------------------------------------------------

/// Try to set rate control mode, bitrate, and GOP size via ICodecAPI.
/// Best-effort: every failure is logged and non-fatal.
///
/// [U] The spike never used ICodecAPI; the media type's MF_MT_AVG_BITRATE was
/// the only bitrate hint. These calls may be ignored by the hardware MFT.
/// [GATE] T4 verifies the actual bitrate.
fn try_set_codec_api_params(sink_writer: &IMFSinkWriter, video_stream: u32, bitrate: u32) {
    // Get ICodecAPI via GetServiceForStream.
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let result = unsafe {
        sink_writer.GetServiceForStream(
            video_stream,
            &GUID::zeroed(),
            &ICodecAPI::IID,
            &mut ptr,
        )
    };
    if let Err(e) = result {
        log(&format!(
            "ICodecAPI not available (0x{:08X}) — using media type bitrate only",
            e.code().0 as u32
        ));
        return;
    }

    let codec: ICodecAPI = unsafe { std::mem::transmute(ptr) };

    // Rate control mode: CBR.
    let rc = eAVEncCommonRateControlMode_CBR;
    match unsafe { codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &make_variant_i32(rc.0)) } {
        Ok(()) => log(&format!("ICodecAPI: rate control = CBR ({})", rc.0)),
        Err(e) => log(&format!(
            "ICodecAPI: rate control mode rejected (0x{:08X})",
            e.code().0 as u32
        )),
    }

    // Mean bitrate.
    match unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &make_variant_u32(bitrate)) } {
        Ok(()) => log(&format!("ICodecAPI: mean bitrate = {bitrate} bps")),
        Err(e) => log(&format!(
            "ICodecAPI: mean bitrate rejected (0x{:08X})",
            e.code().0 as u32
        )),
    }

    // GOP size: ~2 seconds at 30 fps = 60 frames.
    match unsafe { codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &make_variant_u32(60)) } {
        Ok(()) => log("ICodecAPI: GOP size = 60 frames (~2s)"),
        Err(e) => log(&format!(
            "ICodecAPI: GOP size rejected (0x{:08X}) — using encoder default",
            e.code().0 as u32
        )),
    }
}

// ---------------------------------------------------------------------------
// VARIANT helpers for ICodecAPI::SetValue
// ---------------------------------------------------------------------------

/// Create a VT_I4 (int32) VARIANT.
///
/// Uses raw pointer writes because the VARIANT's nested ManuallyDrop<union>
/// structure prevents auto-deref in assignment. The layout (#[repr(C)]):
///   offset 0: vt (u16), offset 2-7: reserved, offset 8: value (i32).
fn make_variant_i32(value: i32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let raw = &mut v as *mut VARIANT as *mut u8;
        std::ptr::write(raw as *mut u16, VT_I4.0);
        std::ptr::write(raw.add(8) as *mut i32, value);
    }
    v
}

/// Create a VT_UI4 (uint32) VARIANT.
fn make_variant_u32(value: u32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let raw = &mut v as *mut VARIANT as *mut u8;
        std::ptr::write(raw as *mut u16, VT_UI4.0);
        std::ptr::write(raw.add(8) as *mut u32, value);
    }
    v
}
