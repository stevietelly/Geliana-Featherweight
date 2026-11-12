//! Real recording pipeline: Windows Graphics Capture (WGC) with an automatic
//! GDI compatibility fallback, WASAPI audio, and the multi-track muxer.
//!
//! Ported from `samples/recorder.rs` with these adaptations:
//!
//! - The capture target comes from [`AppConfig::capture_target`]: either a
//!   monitor (1-based index) or an application window (HWND from the window
//!   picker). The GDI fallback only applies to monitor capture — window
//!   capture requires WGC, so a failed WGC start there is a hard error.
//! - FPS, bitrate, cursor visibility, volumes, and the mic gate threshold are
//!   all taken from the config instead of hard-coded values.
//! - `CursorCaptureSettings` derives from `show_cursor` instead of `Default`.
//!
//! The caller (see `capture/recorder.rs`) owns the stop signal and the output
//! path; [`start_recording`] consumes both and returns an [`ActiveRecording`]
//! whose `stop()` finalizes the file (and muxes the mic track when separate
//! audio was requested).

use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, HBITMAP,
    HDC, HGDIOBJ, HMONITOR, MONITORINFO, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
    ICONINFO,
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    encoder::{
        AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
        VideoSettingsSubType,
    },
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use crate::{
    capture::audio::{AudioPipeline, CHANNELS, SAMPLE_RATE},
    config::{AppConfig, CaptureTarget},
    log,
};

// ---------------------------------------------------------------------------
// High-Speed Windows Graphics Capture (WGC)
// ---------------------------------------------------------------------------

struct WgcFlags {
    output: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    stop: Arc<AtomicBool>,
    audio: Option<AudioPipeline>,
    mic_pcm_path: Option<PathBuf>,
    finalization_error: Arc<Mutex<Option<String>>>,
}

struct WgcCapture {
    encoder: Arc<Mutex<Option<VideoEncoder>>>,
    stop: Arc<AtomicBool>,
    audio: Option<AudioPipeline>,
    finalization_error: Arc<Mutex<Option<String>>>,
    frame_interval: Duration,
    last_frame: Option<Instant>,
}

impl GraphicsCaptureApiHandler for WgcCapture {
    type Flags = WgcFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(mut ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let audio_enabled = ctx.flags.audio.is_some();
        let frame_interval = Duration::from_secs_f64(1.0 / ctx.flags.fps.max(1) as f64);
        log::log(&format!(
            "WGC: initializing encoder for {}x{} @ {}fps (audio_enabled={audio_enabled}) to {}",
            ctx.flags.width,
            ctx.flags.height,
            ctx.flags.fps,
            ctx.flags.output.display()
        ));

        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(ctx.flags.width, ctx.flags.height)
                .sub_type(VideoSettingsSubType::H264)
                .bitrate(ctx.flags.bitrate)
                .frame_rate(ctx.flags.fps),
            AudioSettingsBuilder::new()
                .disabled(!audio_enabled)
                .sample_rate(SAMPLE_RATE)
                .channel_count(CHANNELS)
                .bit_per_sample(16),
            ContainerSettingsBuilder::new(),
            &ctx.flags.output,
        )?;

        let shared_encoder = Arc::new(Mutex::new(Some(encoder)));
        let mut audio = ctx.flags.audio.take();

        if let Some(ref mut audio_pipe) = audio {
            let enc = shared_encoder.clone();
            let primary_sink = move |data: &[u8]| {
                enc.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .ok_or_else(|| "Encoder already finalized".to_string())?
                    .send_audio_buffer(data, 0)
                    .map_err(|e| e.to_string())
            };

            let secondary_sink: Option<Box<dyn FnMut(&[u8]) -> Result<(), String> + Send>> =
                if let Some(ref mic_path) = ctx.flags.mic_pcm_path {
                    let mut file = std::fs::File::create(mic_path)
                        .map_err(|e| format!("Failed to create mic pcm file: {e}"))?;
                    Some(Box::new(move |data: &[u8]| {
                        use std::io::Write;
                        file.write_all(data)
                            .map_err(|e| format!("Mic write error: {e}"))
                    }))
                } else {
                    None
                };

            audio_pipe.begin_with_sinks(primary_sink, secondary_sink)?;
            log::log(&format!(
                "WGC: WASAPI audio sink actively connected (separate_tracks={}).",
                ctx.flags.mic_pcm_path.is_some()
            ));
        }

        Ok(Self {
            encoder: shared_encoder,
            stop: ctx.flags.stop,
            audio,
            finalization_error: ctx.flags.finalization_error,
            frame_interval,
            last_frame: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.stop.load(Ordering::Acquire) {
            capture_control.stop();
            return Ok(());
        }

        // Throttle frames to the configured framerate so high-refresh monitors
        // (144Hz+) do not flood the encoder queue and cause video lag.
        let now = Instant::now();
        if self
            .last_frame
            .is_some_and(|last| now.duration_since(last) < self.frame_interval)
        {
            return Ok(());
        }
        self.last_frame = Some(now);

        let mut enc = self.encoder.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(encoder) = enc.as_mut() {
            encoder.send_frame(frame)?;
        }

        Ok(())
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        log::log("WGC: capture handler dropping; stopping audio and finalizing encoder...");
        if let Some(mut audio) = self.audio.take() {
            if let Err(e) = audio.stop_and_join() {
                log::log(&format!("WGC audio stop error: {e}"));
            }
        }
        if let Some(encoder) = self.encoder.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Err(e) = encoder.finish() {
                let err_msg = format!("Failed to finalize WGC MP4: {e}");
                log::log(&err_msg);
                *self.finalization_error.lock().unwrap() = Some(err_msg);
            } else {
                log::log("WGC: MP4 finalized cleanly with audio.");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GDI Compatible Screen Capture Fallback
// Used when Desktop Composition is disabled or WGC is unavailable
// (monitor targets only — window capture requires WGC)
// ---------------------------------------------------------------------------

struct GdiCapture {
    screen_dc: HDC,
    memory_dc: HDC,
    bitmap: HBITMAP,
    previous_object: HGDIOBJ,
    pixels: *mut u8,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    draw_cursor: bool,
}

unsafe impl Send for GdiCapture {}

impl GdiCapture {
    fn new(hmonitor: isize, width: u32, height: u32, draw_cursor: bool) -> Result<Self, String> {
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        if !unsafe { GetMonitorInfoW(HMONITOR(hmonitor as _), &mut monitor_info) }.as_bool() {
            return Err(format!("GetMonitorInfoW failed: {}", std::io::Error::last_os_error()));
        }

        let screen_dc = unsafe { GetDC(None) };
        if screen_dc.0.is_null() {
            return Err(format!("GetDC failed: {}", std::io::Error::last_os_error()));
        }

        let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        if memory_dc.0.is_null() {
            unsafe { ReleaseDC(None, screen_dc) };
            return Err(format!("CreateCompatibleDC failed: {}", std::io::Error::last_os_error()));
        }

        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: height as i32, // Positive height = bottom-up DIB, matching send_frame_buffer's expected layout
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };

        let mut pixels = std::ptr::null_mut();
        let bitmap = match unsafe {
            CreateDIBSection(
                Some(memory_dc),
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut pixels,
                None,
                0,
            )
        } {
            Ok(b) => b,
            Err(e) => {
                unsafe {
                    let _ = DeleteDC(memory_dc);
                    ReleaseDC(None, screen_dc);
                }
                return Err(format!("CreateDIBSection failed: {e}"));
            }
        };

        let previous_object = unsafe { SelectObject(memory_dc, HGDIOBJ(bitmap.0)) };

        Ok(Self {
            screen_dc,
            memory_dc,
            bitmap,
            previous_object,
            pixels: pixels.cast(),
            left: monitor_info.rcMonitor.left,
            top: monitor_info.rcMonitor.top,
            width,
            height,
            draw_cursor,
        })
    }

    fn grab(&mut self) -> Result<&[u8], String> {
        unsafe {
            BitBlt(
                self.memory_dc,
                0,
                0,
                self.width as i32,
                self.height as i32,
                Some(self.screen_dc),
                self.left,
                self.top,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            )
            .map_err(|e| format!("BitBlt failed: {e}"))?;
        }

        if self.draw_cursor {
            self.draw_cursor();
        }

        let length = (self.width as usize) * (self.height as usize) * 4;
        Ok(unsafe { std::slice::from_raw_parts(self.pixels, length) })
    }

    fn draw_cursor(&self) {
        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..CURSORINFO::default()
        };
        if unsafe { GetCursorInfo(&mut cursor) }.is_err()
            || cursor.flags != CURSOR_SHOWING
            || cursor.hCursor.0.is_null()
        {
            return;
        }

        let icon_handle = HICON(cursor.hCursor.0);
        let mut icon = ICONINFO::default();
        let (hotspot_x, hotspot_y) = if unsafe { GetIconInfo(icon_handle, &mut icon) }.is_ok() {
            let hotspot = (icon.xHotspot as i32, icon.yHotspot as i32);
            unsafe {
                if !icon.hbmMask.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
                }
                if !icon.hbmColor.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
                }
            }
            hotspot
        } else {
            (0, 0)
        };

        let x = cursor.ptScreenPos.x - self.left - hotspot_x;
        let y = cursor.ptScreenPos.y - self.top - hotspot_y;
        let _ = unsafe { DrawIconEx(self.memory_dc, x, y, icon_handle, 0, 0, 0, None, DI_NORMAL) };
    }
}

impl Drop for GdiCapture {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory_dc, self.previous_object);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory_dc);
            ReleaseDC(None, self.screen_dc);
        }
    }
}

// ---------------------------------------------------------------------------
// Active Recording Handle & Multi-track Audio Muxer
// ---------------------------------------------------------------------------

fn mux_separate_audio_tracks(
    temp_video: &Path,
    mic_pcm: &Path,
    final_output: &Path,
) -> Result<(), String> {
    log::log(&format!(
        "ffmpeg: muxing multi-track audio into {}",
        final_output.display()
    ));

    #[cfg(windows)]
    use std::os::windows::process::CommandExt;

    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        temp_video.to_str().unwrap(),
        "-f",
        "s16le",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-i",
        mic_pcm.to_str().unwrap(),
        "-map",
        "0:v:0",
        "-map",
        "0:a:0",
        "-map",
        "1:a:0",
        "-metadata:s:a:0",
        "title=Desktop Audio",
        "-metadata:s:a:1",
        "title=Microphone",
        "-c:v",
        "copy",
        "-c:a:0",
        "copy",
        "-c:a:1",
        "aac",
        "-b:a:1",
        "192k",
        final_output.to_str().unwrap(),
    ]);

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW on Windows

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to spawn ffmpeg: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg failed: {stderr}"));
    }

    let _ = std::fs::remove_file(temp_video);
    let _ = std::fs::remove_file(mic_pcm);
    log::log("ffmpeg multi-track mux completed cleanly.");
    Ok(())
}

/// A running capture session. Call [`ActiveRecording::stop`] to finalize.
pub struct ActiveRecording {
    stop_signal: Arc<AtomicBool>,
    output_path: PathBuf,
    temp_video_path: Option<PathBuf>,
    mic_pcm_path: Option<PathBuf>,
    worker: Option<thread::JoinHandle<()>>,
    finalization_error: Arc<Mutex<Option<String>>>,
}

impl ActiveRecording {
    /// Stops the capture, finalizes the encoder, and (for separate-track
    /// recordings) muxes the mic track into the final file. Returns the final
    /// output path, or the finalization error.
    pub fn stop(mut self) -> Result<PathBuf, String> {
        log::log("ActiveRecording: stop requested.");
        self.stop_signal.store(true, Ordering::Release);

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }

        let err = self.finalization_error.lock().unwrap().take();
        if let Some(e) = err {
            return Err(e);
        }

        if let (Some(temp_video), Some(mic_pcm)) =
            (self.temp_video_path.take(), self.mic_pcm_path.take())
        {
            if let Err(e) = mux_separate_audio_tracks(&temp_video, &mic_pcm, &self.output_path) {
                log::log(&format!(
                    "Multi-track mux error: {e}; falling back to single-track file"
                ));
                let _ = std::fs::rename(&temp_video, &self.output_path);
                let _ = std::fs::remove_file(&mic_pcm);
            }
        }

        Ok(self.output_path.clone())
    }

    #[allow(dead_code)]
    pub fn output_path(&self) -> &PathBuf {
        &self.output_path
    }

    /// The file the encoder is writing *right now*. With separate audio tracks
    /// this is the `<name>.temp.mp4` staging file, which is what grows during
    /// the session; the final output only exists after the mux in
    /// [`ActiveRecording::stop`]. The recorder samples this path for the size,
    /// growth-rate and disk-headroom readouts.
    pub fn active_file_path(&self) -> &PathBuf {
        self.temp_video_path.as_ref().unwrap_or(&self.output_path)
    }
}

// ---------------------------------------------------------------------------
// Capture-target resolution
// ---------------------------------------------------------------------------

/// Resolved capture source. `Monitor` carries everything the GDI fallback
/// needs; `Window` has no GDI equivalent.
enum CaptureSource {
    Monitor {
        monitor: Monitor,
        hmonitor: isize,
        width: u32,
        height: u32,
    },
    Window {
        window: Window,
        width: u32,
        height: u32,
    },
}

impl CaptureSource {
    fn width(&self) -> u32 {
        match self {
            Self::Monitor { width, .. } | Self::Window { width, .. } => *width,
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Monitor { height, .. } | Self::Window { height, .. } => *height,
        }
    }

    /// Rounds down to an even pixel count (H.264 requires even dimensions);
    /// clamps to at least 2.
    fn even(value: u32) -> u32 {
        value.max(2) & !1
    }

    /// Resolves the configured target into a concrete source. Returns `Err`
    /// for a window that has been closed or re-created since it was picked.
    fn resolve(config: &AppConfig) -> Result<Self, String> {
        match &config.capture_target {
            CaptureTarget::Monitor { index } => {
                let monitor = Monitor::from_index((*index).max(1))
                    .map_err(|e| format!("Could not open monitor {index}: {e}"))?;
                let width = monitor
                    .width()
                    .map_err(|e| format!("Could not get monitor width: {e}"))?;
                let height = monitor
                    .height()
                    .map_err(|e| format!("Could not get monitor height: {e}"))?;
                let hmonitor = monitor.as_raw_hmonitor() as isize;
                log::log(&format!("Capture target: monitor {index} ({width}x{height})"));
                Ok(Self::Monitor {
                    monitor,
                    hmonitor,
                    width,
                    height,
                })
            }
            CaptureTarget::Window { id, title } => {
                if *id == 0 {
                    return Err("No window is selected. Pick an application window first.".into());
                }
                let window = Window::from_raw_hwnd(*id as usize as *mut std::ffi::c_void);
                if !window.is_valid() {
                    return Err(format!(
                        "The selected window \"{title}\" is no longer available. Pick a window again."
                    ));
                }
                let width = Self::even(
                    window
                        .width()
                        .map_err(|e| format!("Could not get window width: {e}"))? as u32,
                );
                let height = Self::even(
                    window
                        .height()
                        .map_err(|e| format!("Could not get window height: {e}"))? as u32,
                );
                log::log(&format!("Capture target: window \"{title}\" ({width}x{height})"));
                Ok(Self::Window {
                    window,
                    width,
                    height,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API: Start Recording (WGC with automatic GDI fallback for monitors)
// ---------------------------------------------------------------------------

/// Starts a real recording to `output_path` for the target in `config`.
///
/// `stop_signal` is the session stop flag: the pipeline polls it, and
/// [`ActiveRecording::stop`] sets it before joining. Returns `Err` if the
/// target is invalid, the encoder cannot be created, or (for window targets)
/// WGC cannot start — the GDI fallback only exists for monitor capture.
pub fn start_recording(
    config: &AppConfig,
    output_path: PathBuf,
    stop_signal: Arc<AtomicBool>,
) -> Result<ActiveRecording, String> {
    let env_force_gdi = std::env::var("FEATHERWEIGHT_FORCE_GDI")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let force_gdi = config.force_gdi || env_force_gdi;

    log::log(&format!(
        "start_recording: output = {}, mic = {}, system_audio = {}, separate_audio = {}, force_gdi = {}",
        output_path.display(),
        config.microphone,
        config.system_audio,
        config.separate_audio_tracks,
        force_gdi
    ));

    let (primary_output, temp_video_path, mic_pcm_path) =
        if config.microphone && config.separate_audio_tracks {
            (
                output_path.with_extension("temp.mp4"),
                Some(output_path.with_extension("temp.mp4")),
                Some(output_path.with_extension("mic.pcm")),
            )
        } else {
            (output_path.clone(), None, None)
        };

    let source = CaptureSource::resolve(config)?;
    let width = source.width();
    let height = source.height();

    // If force_gdi is false, probe Windows Graphics Capture (WGC). For window
    // targets WGC is the only option, so force_gdi is ignored with a note.
    let wgc_probe: Result<GraphicsCaptureItemType, windows::core::Error> = match &source {
        CaptureSource::Monitor { .. } => {
            if force_gdi {
                log::log("Force GDI requested; skipping Windows Graphics Capture.");
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
            } else {
                probe_source(&source)
            }
        }
        CaptureSource::Window { .. } => {
            if force_gdi {
                log::log(
                    "Force GDI has no effect on window capture (GDI cannot target a window); using WGC.",
                );
            }
            probe_source(&source)
        }
    };

    if let Ok(_) = wgc_probe {
        log::log("Windows Graphics Capture is available; starting WGC pipeline with audio...");

        let finalization_error = Arc::new(Mutex::new(None));
        let cursor = if config.show_cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };

        // `Settings` is generic over the capture item type, so each target
        // builds its own settings. Only one arm ever runs, so moving
        // `audio_pipeline` into the per-arm flags is unambiguous.
        let started = match &source {
            CaptureSource::Monitor { monitor, .. } => {
                let flags = WgcFlags {
                    output: primary_output.clone(),
                    width,
                    height,
                    fps: config.fps.max(1),
                    bitrate: config.bitrate_mbps.max(1) * 1_000_000,
                    stop: stop_signal.clone(),
                    audio: start_audio_pipeline(config, &stop_signal, "WGC"),
                    mic_pcm_path: mic_pcm_path.clone(),
                    finalization_error: finalization_error.clone(),
                };
                let settings = Settings::new(
                    *monitor,
                    cursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    flags,
                );
                WgcCapture::start_free_threaded(settings)
            }
            CaptureSource::Window { window, .. } => {
                let flags = WgcFlags {
                    output: primary_output.clone(),
                    width,
                    height,
                    fps: config.fps.max(1),
                    bitrate: config.bitrate_mbps.max(1) * 1_000_000,
                    stop: stop_signal.clone(),
                    audio: start_audio_pipeline(config, &stop_signal, "WGC"),
                    mic_pcm_path: mic_pcm_path.clone(),
                    finalization_error: finalization_error.clone(),
                };
                let settings = Settings::new(
                    *window,
                    cursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    flags,
                );
                WgcCapture::start_free_threaded(settings)
            }
        };

        match started {
            Ok(control) => {
                let stop_for_worker = stop_signal.clone();
                let worker = thread::Builder::new()
                    .name("featherweight-wgc-supervisor".into())
                    .spawn(move || {
                        loop {
                            if control.is_finished() {
                                let _ = control.wait();
                                break;
                            }
                            if stop_for_worker.load(Ordering::Acquire) {
                                let _ = control.stop();
                                break;
                            }
                            thread::sleep(Duration::from_millis(20));
                        }
                    })
                    .map_err(|e| format!("Failed to spawn WGC supervisor thread: {e}"))?;

                return Ok(ActiveRecording {
                    stop_signal,
                    output_path,
                    temp_video_path,
                    mic_pcm_path,
                    worker: Some(worker),
                    finalization_error,
                });
            }
            Err(e) => {
                // The handler runs on its own thread; a failed start means the
                // audio pipeline was never connected. Its Drop cleans up.
                log::log(&format!(
                    "WGC start_free_threaded failed ({e}); falling back to GDI mode..."
                ));
            }
        }
    } else {
        let err = wgc_probe.err().unwrap();
        log::log(&format!(
            "Windows Graphics Capture probe failed (code: 0x{:08X}, {err}); using GDI compatibility mode.",
            err.code().0
        ));
    }

    // -----------------------------------------------------------------------
    // GDI Compatibility Fallback Pipeline with WASAPI Audio (monitors only)
    // -----------------------------------------------------------------------
    let CaptureSource::Monitor {
        hmonitor,
        width,
        height,
        ..
    } = source
    else {
        return Err(
            "Window capture requires Windows Graphics Capture, which is unavailable on this system."
                .into(),
        );
    };

    log::log("Starting GDI capture pipeline with WASAPI audio...");

    let fps = config.fps.clamp(1, 30); // GDI compatibility mode caps at 30 FPS
    let bitrate = 6_000_000u32; // 6 Mbps for GDI compatibility

    let audio_pipeline = start_audio_pipeline(config, &stop_signal, "GDI");

    let audio_enabled = audio_pipeline.is_some();
    let encoder = VideoEncoder::new(
        VideoSettingsBuilder::new(width, height)
            .sub_type(VideoSettingsSubType::H264)
            .bitrate(bitrate)
            .frame_rate(fps),
        AudioSettingsBuilder::new()
            .disabled(!audio_enabled)
            .sample_rate(SAMPLE_RATE)
            .channel_count(CHANNELS)
            .bit_per_sample(16),
        ContainerSettingsBuilder::new(),
        &primary_output,
    )
    .map_err(|e| format!("VideoEncoder creation failed in GDI mode: {e}"))?;

    let shared_encoder = Arc::new(Mutex::new(Some(encoder)));

    // Begin routing audio samples into the encoder (and optional mic file)
    let mut audio = audio_pipeline;
    if let Some(ref mut audio_pipe) = audio {
        let audio_encoder = shared_encoder.clone();
        let primary_sink = move |data: &[u8]| {
            audio_encoder
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .ok_or_else(|| "Encoder already finalized".to_string())?
                .send_audio_buffer(data, 0)
                .map_err(|e| e.to_string())
        };

        let secondary_sink: Option<Box<dyn FnMut(&[u8]) -> Result<(), String> + Send>> =
            if let Some(ref mic_path) = mic_pcm_path {
                let mut file = std::fs::File::create(mic_path)
                    .map_err(|e| format!("Failed to create mic pcm file: {e}"))?;
                Some(Box::new(move |data: &[u8]| {
                    use std::io::Write;
                    file.write_all(data)
                        .map_err(|e| format!("Mic write error: {e}"))
                }))
            } else {
                None
            };

        if let Err(e) = audio_pipe.begin_with_sinks(primary_sink, secondary_sink) {
            log::log(&format!("Failed to begin audio sink in GDI mode: {e}"));
        } else {
            log::log(&format!(
                "WASAPI audio sink actively connected to VideoEncoder in GDI mode (separate_tracks={}).",
                mic_pcm_path.is_some()
            ));
        }
    }

    let stop_for_worker = stop_signal.clone();
    let final_err_for_worker = Arc::new(Mutex::new(None));
    let finalization_error = Arc::clone(&final_err_for_worker);
    let gdi_encoder = shared_encoder.clone();
    let show_cursor = config.show_cursor;

    let worker = thread::Builder::new()
        .name("featherweight-gdi-worker".into())
        .spawn(move || {
            let mut gdi_capture = match GdiCapture::new(
                hmonitor,
                width,
                height,
                show_cursor,
            ) {
                Ok(c) => c,
                Err(e) => {
                    log::log(&format!("GdiCapture init error: {e}"));
                    *final_err_for_worker.lock().unwrap() = Some(e);
                    return;
                }
            };

            let started_at = Instant::now();
            let frame_interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            let mut next_frame = started_at;

            log::log("GDI worker loop entered.");

            while !stop_for_worker.load(Ordering::Acquire) {
                let now = Instant::now();
                if now < next_frame {
                    thread::sleep((next_frame - now).min(Duration::from_millis(5)));
                    continue;
                }

                match gdi_capture.grab() {
                    Ok(buffer) => {
                        let timestamp = (started_at.elapsed().as_nanos() / 100) as i64;
                        let mut enc_guard = gdi_encoder.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(enc) = enc_guard.as_mut() {
                            if let Err(e) = enc.send_frame_buffer(buffer, timestamp) {
                                log::log(&format!("GDI encoder error sending frame: {e}"));
                                *final_err_for_worker.lock().unwrap() = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        log::log(&format!("GDI grab error: {e}"));
                        *final_err_for_worker.lock().unwrap() = Some(e);
                        break;
                    }
                }

                next_frame += frame_interval;
                if next_frame < Instant::now() {
                    next_frame = Instant::now() + frame_interval;
                }
            }

            log::log("GDI worker exiting loop; stopping audio and finalizing MP4...");
            if let Some(mut audio_pipe) = audio {
                if let Err(e) = audio_pipe.stop_and_join() {
                    log::log(&format!("audio stop_and_join error: {e}"));
                }
            }

            if let Some(enc) = gdi_encoder.lock().unwrap_or_else(|e| e.into_inner()).take() {
                if let Err(e) = enc.finish() {
                    let err_msg = format!("GDI finish error: {e}");
                    log::log(&err_msg);
                    *final_err_for_worker.lock().unwrap() = Some(err_msg);
                } else {
                    log::log("GDI MP4 finalized successfully with audio.");
                }
            }
        })
        .map_err(|e| format!("Failed to spawn GDI worker: {e}"))?;

    Ok(ActiveRecording {
        stop_signal,
        output_path,
        temp_video_path,
        mic_pcm_path,
        worker: Some(worker),
        finalization_error,
    })
}

/// Probes whether the resolved source can be converted into a WGC capture
/// item. Consumes a copy of the source (`Monitor` and `Window` are both
/// `Copy`); the original stays usable for `Settings::new`.
fn probe_source(
    source: &CaptureSource,
) -> Result<GraphicsCaptureItemType, windows::core::Error> {
    match source {
        CaptureSource::Monitor { monitor, .. } => (*monitor).try_into(),
        CaptureSource::Window { window, .. } => (*window).try_into(),
    }
}

/// Starts the WASAPI pipeline for the configured sources, applies the
/// config's volumes and mic gate, and returns `None` (video-only) if audio
/// is disabled or fails to start.
fn start_audio_pipeline(
    config: &AppConfig,
    stop_signal: &Arc<AtomicBool>,
    mode: &str,
) -> Option<AudioPipeline> {
    match AudioPipeline::start(config.system_audio, config.microphone, stop_signal.clone()) {
        Ok(Some(mut pipeline)) => {
            log::log(&format!(
                "{mode}: WASAPI AudioPipeline started (system_loopback={}, mic={}).",
                config.system_audio, config.microphone
            ));
            pipeline.set_volumes([config.system_volume, config.microphone_volume]);
            pipeline.set_mic_gate_db(config.microphone_threshold_db);
            Some(pipeline)
        }
        Ok(None) => {
            log::log(&format!("{mode}: audio disabled; recording video only"));
            None
        }
        Err(e) => {
            log::log(&format!(
                "{mode}: WASAPI AudioPipeline failed to start: {e}; recording video only"
            ));
            None
        }
    }
}
