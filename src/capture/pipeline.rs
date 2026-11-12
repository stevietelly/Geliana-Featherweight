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

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, HBITMAP,
    HDC, HGDIOBJ, HMONITOR, MONITORINFO, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
    ICONINFO, IsIconic,
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
    fps: u32,
    bitrate: u32,
    stop: Arc<AtomicBool>,
    audio: Option<AudioPipeline>,
    mic_pcm_path: Option<PathBuf>,
    finalization_error: Arc<Mutex<Option<String>>>,
    /// Set once the first WGC frame has sized and created the encoder. The
    /// recorder polls it to move the session out of `WaitingForFirstFrame`,
    /// and the audio sinks gate on it so every track shares the video's
    /// origin (see the sink closures in [`WgcCapture::new`]).
    first_frame: Arc<AtomicBool>,
}

struct WgcCapture {
    /// Created lazily on the first frame — its even-rounded resolution is
    /// that frame's, never a value measured before capture started. `None`
    /// until then; taken and finalized in [`Drop for WgcCapture`].
    encoder: Arc<Mutex<Option<VideoEncoder>>>,
    /// Even-rounded (target) output dimensions, fixed by the first frame.
    target: Option<(u32, u32)>,
    fps: u32,
    bitrate: u32,
    output: PathBuf,
    audio_enabled: bool,
    first_frame: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    audio: Option<AudioPipeline>,
    finalization_error: Arc<Mutex<Option<String>>>,
    frame_interval: Duration,
    last_frame: Option<Instant>,
    /// Logs the first size mismatch once per recording, not per frame.
    letterbox_warned: bool,
}

impl GraphicsCaptureApiHandler for WgcCapture {
    type Flags = WgcFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(mut ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let audio_enabled = ctx.flags.audio.is_some();
        let frame_interval = Duration::from_secs_f64(1.0 / ctx.flags.fps.max(1) as f64);
        log::log(&format!(
            "WGC: awaiting the first frame to size the encoder @ {}fps (audio_enabled={audio_enabled}) to {}",
            ctx.flags.fps,
            ctx.flags.output.display()
        ));

        let shared_encoder: Arc<Mutex<Option<VideoEncoder>>> = Arc::new(Mutex::new(None));
        let mut audio = ctx.flags.audio.take();
        let video_started = ctx.flags.first_frame.clone();
        // Separate clone for the mic sink: the primary closure below moves
        // the first one.
        let video_started_for_mic = video_started.clone();

        if let Some(ref mut audio_pipe) = audio {
            let enc = shared_encoder.clone();
            let primary_sink = move |data: &[u8]| {
                let mut guard = enc.lock().unwrap_or_else(|e| e.into_inner());
                // A/V sync: the video origin (t=0 for every track) is the
                // first video frame's SystemRelativeTime (WGC timestamps are
                // QPC-derived 100 ns ticks). The encoder's audio path
                // timestamps buffers with a monotonic sample counter that
                // starts at its first buffer, so audio that arrives before
                // the first frame is dropped here rather than written with a
                // shifted offset — the counter then starts exactly at the
                // video origin. (WASAPI packets are stamped internally on
                // the same QPC clock, but the encoder ignores wall
                // timestamps for audio, so the gate is what aligns the
                // clocks.)
                let Some(encoder) = guard.as_mut() else {
                    if video_started.load(Ordering::Acquire) {
                        return Err("Encoder already finalized".to_string());
                    }
                    return Ok(()); // still waiting for the first video frame
                };
                encoder.send_audio_buffer(data, 0).map_err(|e| e.to_string())
            };

            let secondary_sink: Option<Box<dyn FnMut(&[u8]) -> Result<(), String> + Send>> =
                if let Some(ref mic_path) = ctx.flags.mic_pcm_path {
                    // The mic file is created on the first write after the
                    // video origin, so a session cancelled while waiting
                    // leaves no file behind.
                    let video_started = video_started_for_mic;
                    let file = Arc::new(Mutex::new(None::<std::fs::File>));
                    let path = mic_path.clone();
                    let mut create_failed = false;
                    Some(Box::new(move |data: &[u8]| {
                        if !video_started.load(Ordering::Acquire) {
                            return Ok(()); // dropped: before the A/V origin
                        }
                        let mut guard = file.lock().unwrap_or_else(|e| e.into_inner());
                        if guard.is_none() {
                            match std::fs::File::create(&path) {
                                Ok(created) => *guard = Some(created),
                                Err(e) => {
                                    if !create_failed {
                                        create_failed = true;
                                        log::log(&format!(
                                            "Failed to create mic pcm file {}: {e}; recording without the separate mic track",
                                            path.display()
                                        ));
                                    }
                                    return Ok(());
                                }
                            }
                        }
                        use std::io::Write;
                        guard
                            .as_mut()
                            .expect("file created above")
                            .write_all(data)
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
            target: None,
            fps: ctx.flags.fps.max(1),
            bitrate: ctx.flags.bitrate,
            output: ctx.flags.output,
            audio_enabled,
            first_frame: ctx.flags.first_frame,
            stop: ctx.flags.stop,
            audio,
            finalization_error: ctx.flags.finalization_error,
            frame_interval,
            last_frame: None,
            letterbox_warned: false,
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

        // The first frame decides the encoder resolution: WGC delivers the
        // window's real content size, whatever GetWindowRect claimed at
        // start (a minimized target reports its iconic rect, and a normal
        // one includes the DWM shadow frame). Once created, the MP4's
        // dimensions are fixed, and this frame's SystemRelativeTime (a
        // QPC-derived 100 ns tick count) becomes t=0 for the video track —
        // and, via the gated audio sinks, for every audio track.
        if self.target.is_none() {
            let width = even_dimension(frame.width());
            let height = even_dimension(frame.height());
            log::log(&format!(
                "WGC: first frame is {}x{}; creating encoder at {width}x{height}",
                frame.width(),
                frame.height()
            ));
            match VideoEncoder::new(
                VideoSettingsBuilder::new(width, height)
                    .sub_type(VideoSettingsSubType::H264)
                    .bitrate(self.bitrate)
                    .frame_rate(self.fps),
                AudioSettingsBuilder::new()
                    .disabled(!self.audio_enabled)
                    .sample_rate(SAMPLE_RATE)
                    .channel_count(CHANNELS)
                    .bit_per_sample(16),
                ContainerSettingsBuilder::new(),
                &self.output,
            ) {
                Ok(encoder) => {
                    self.target = Some((width, height));
                    *self.encoder.lock().unwrap_or_else(|e| e.into_inner()) = Some(encoder);
                    self.first_frame.store(true, Ordering::Release);
                }
                Err(e) => {
                    let msg = format!("Failed to create the video encoder on the first frame: {e}");
                    log::log(&msg);
                    *self.finalization_error.lock().unwrap() = Some(msg);
                    capture_control.stop();
                    return Ok(());
                }
            }
        }

        let (target_width, target_height) = self.target.expect("encoder created above");
        let mut enc = self.encoder.lock().unwrap_or_else(|e| e.into_inner());
        let Some(encoder) = enc.as_mut() else {
            return Ok(()); // shutting down
        };

        if frame.width() == target_width && frame.height() == target_height {
            encoder.send_frame(frame)?;
        } else {
            // Fixed-resolution policy: letterbox, never crop. The library's
            // own mismatch path clears to black and copies only the
            // top-left block of the frame, which would show a cropped
            // corner of the content, so the frame is scaled to fit and
            // centered on black here instead. Transitional frames during a
            // restore-from-minimized land here too. The first timestamp is
            // already fixed inside the encoder, so passing the raw
            // SystemRelativeTime keeps these frames in step with the
            // send_frame path.
            if !self.letterbox_warned {
                self.letterbox_warned = true;
                log::log(&format!(
                    "WGC: frame size changed to {}x{}; letterboxing into the fixed {}x{} output for the rest of the recording",
                    frame.width(),
                    frame.height(),
                    target_width,
                    target_height
                ));
            }
            let mut scratch = Vec::new();
            let frame_width = frame.width();
            let frame_height = frame.height();
            let buffer = frame.buffer()?;
            let source = buffer.as_nopadding_buffer(&mut scratch);
            let letterboxed =
                letterbox_bgra_bytes(source, frame_width, frame_height, target_width, target_height);
            drop(buffer);
            let timestamp = frame.timestamp()?.Duration;
            encoder.send_frame_buffer(&letterboxed, timestamp)?;
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
        } else {
            log::log("WGC: no video frame ever arrived; nothing was written to disk.");
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
    /// Set once the pipeline's first video frame arrived. `false` means the
    /// session never produced anything (a minimized target that was never
    /// restored, for instance) and stopping it must leave no file behind.
    first_frame: Arc<AtomicBool>,
}

impl ActiveRecording {
    /// Whether the capture pipeline has delivered its first video frame.
    pub fn first_frame_arrived(&self) -> bool {
        self.first_frame.load(Ordering::Acquire)
    }

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

        // Cancelled before the first frame: the (deferred) encoder never
        // created the video file and the gated audio sinks never created the
        // mic file, so there is nothing to finalize or mux. Clean any stray
        // staging files and report success — cancelling is not an error.
        if !self.first_frame_arrived() {
            log::log("ActiveRecording: stopped before the first frame; discarding the session with no output file.");
            if let Some(temp) = &self.temp_video_path {
                let _ = std::fs::remove_file(temp);
            }
            if let Some(mic) = &self.mic_pcm_path {
                let _ = std::fs::remove_file(mic);
            }
            return Ok(self.output_path.clone());
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
///
/// Neither variant carries a resolution: the encoder is sized from the first
/// frame the pipeline actually delivers (see [`WgcCapture::on_frame_arrived`]),
/// because pre-capture measurements lie — a minimized window reports its
/// iconic rect and a normal one includes the DWM shadow frame.
enum CaptureSource {
    Monitor {
        monitor: Monitor,
        hmonitor: isize,
        width: u32,
        height: u32,
    },
    Window {
        window: Window,
    },
}

/// Rounds down to an even pixel count (H.264 requires even dimensions);
/// clamps to at least 2.
fn even_dimension(value: u32) -> u32 {
    value.max(2) & !1
}

impl CaptureSource {
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
                // IsIconic is informational only: a minimized target is
                // perfectly valid — recording simply waits for it to deliver
                // its first frame once restored.
                let iconic = unsafe { IsIconic(HWND(*id as *mut std::ffi::c_void)) }.as_bool();
                log::log(&format!(
                    "Capture target: window \"{title}\"{}; the encoder will be sized from the first frame",
                    if iconic { " (minimized: waiting for restore)" } else { "" }
                ));
                Ok(Self::Window { window })
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
/// [`ActiveRecording::stop`] sets it before joining. `mic_muted` is the
/// shared live-mute flag; the audio mixer polls it every mix tick so the
/// tray's "Mute microphone" item silences the mic mid-session.
///
/// The encoder's resolution is decided by the first frame the pipeline
/// delivers (see [`WgcCapture::on_frame_arrived`]), never by measurements
/// taken at start: a minimized window reports its iconic rect and a normal
/// one includes the DWM shadow. Until that first frame — detectable via
/// [`ActiveRecording::first_frame_arrived`] — nothing is written to disk and
/// stopping the session cancels it cleanly with no output file. Returns
/// `Err` if the target is invalid, the encoder cannot be created, or (for
/// window targets) WGC cannot start — the GDI fallback only exists for
/// monitor capture.
pub fn start_recording(
    config: &AppConfig,
    output_path: PathBuf,
    stop_signal: Arc<AtomicBool>,
    mic_muted: &Arc<AtomicBool>,
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
        let first_frame = Arc::new(AtomicBool::new(false));
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
                    fps: config.fps.max(1),
                    bitrate: config.bitrate_mbps.max(1) * 1_000_000,
                    stop: stop_signal.clone(),
                    audio: start_audio_pipeline(config, &stop_signal, "WGC", mic_muted),
                    mic_pcm_path: mic_pcm_path.clone(),
                    finalization_error: finalization_error.clone(),
                    first_frame: first_frame.clone(),
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
            CaptureSource::Window { window } => {
                let flags = WgcFlags {
                    output: primary_output.clone(),
                    fps: config.fps.max(1),
                    bitrate: config.bitrate_mbps.max(1) * 1_000_000,
                    stop: stop_signal.clone(),
                    audio: start_audio_pipeline(config, &stop_signal, "WGC", mic_muted),
                    mic_pcm_path: mic_pcm_path.clone(),
                    finalization_error: finalization_error.clone(),
                    first_frame: first_frame.clone(),
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
                    first_frame,
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

    let audio_pipeline = start_audio_pipeline(config, &stop_signal, "GDI", mic_muted);

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
        // Monitors always deliver frames; no waiting state exists here.
        first_frame: Arc::new(AtomicBool::new(true)),
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
///
/// `mic_muted` is the shared live-mute flag; the mixer polls it every mix
/// tick, so the tray's "Mute microphone" item silences the mic mid-session.
fn start_audio_pipeline(
    config: &AppConfig,
    stop_signal: &Arc<AtomicBool>,
    mode: &str,
    mic_muted: &Arc<AtomicBool>,
) -> Option<AudioPipeline> {    match AudioPipeline::start(config.system_audio, config.microphone, stop_signal.clone()) {
        Ok(Some(mut pipeline)) => {
            log::log(&format!(
                "{mode}: WASAPI AudioPipeline started (system_loopback={}, mic={}).",
                config.system_audio, config.microphone
            ));
            pipeline.set_volumes([config.system_volume, config.microphone_volume]);
            pipeline.set_mic_gate_db(config.microphone_threshold_db);
            pipeline.set_mic_muted_flag(mic_muted.clone());
            pipeline.set_noise_suppression(
                config
                    .microphone_noise_suppression
                    .then(|| (config.microphone_noise_method, config.microphone_suppression_db)),
            );
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

// ---------------------------------------------------------------------------
// Fixed-resolution letterboxing
// ---------------------------------------------------------------------------

/// Computes the centered content rectangle that fits `src_w x src_h` inside
/// `dst_w x dst_h` preserving aspect ratio. Returns `(x, y, w, h)` in
/// destination pixels.
fn letterbox_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    let scale = (dst_w as f64 / src_w as f64).min(dst_h as f64 / src_h as f64);
    let w = ((src_w as f64 * scale).floor() as u32).max(1);
    let h = ((src_h as f64 * scale).floor() as u32).max(1);
    ((dst_w - w) / 2, (dst_h - h) / 2, w, h)
}

/// Scales `src` (tightly packed, top-down BGRA, `src_w x src_h`) to fit
/// `dst_w x dst_h`, centers it on opaque black, and returns the result as a
/// bottom-up BGRA buffer — the layout [`VideoEncoder::send_frame_buffer`]
/// expects.
///
/// Runs only on frames whose size differs from the fixed encoder target
/// (mid-recording resizes, transitional restore-from-minimized frames), so
/// the per-pixel nearest-neighbour sampling cost is confined to those.
fn letterbox_bgra_bytes(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for pixel in dst.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, 255]);
    }
    let (origin_x, origin_y, content_w, content_h) = letterbox_rect(src_w, src_h, dst_w, dst_h);
    for row in 0..content_h {
        let src_y = (row as u64 * src_h as u64 / content_h as u64) as usize;
        // Output is bottom-up: a destination row counted from the top maps
        // to the mirrored row index in the returned buffer.
        let dst_row = (dst_h - 1 - (origin_y + row)) as usize;
        for col in 0..content_w {
            let src_x = (col as u64 * src_w as u64 / content_w as u64) as usize;
            let s = (src_y * src_w as usize + src_x) * 4;
            let d = (dst_row * dst_w as usize + (origin_x + col) as usize) * 4;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_dimensions_round_down_and_clamp() {
        assert_eq!(even_dimension(1920), 1920);
        assert_eq!(even_dimension(1081), 1080);
        assert_eq!(even_dimension(1), 2);
        assert_eq!(even_dimension(0), 2);
    }

    #[test]
    fn matching_aspect_content_fills_target() {
        assert_eq!(letterbox_rect(1920, 1080, 1280, 720), (0, 0, 1280, 720));
    }

    #[test]
    fn wider_source_gets_side_bars() {
        // 2:1 source into a 1:1 target: content is full height, centered.
        assert_eq!(letterbox_rect(200, 100, 100, 100), (0, 25, 100, 50));
    }

    #[test]
    fn taller_source_gets_top_bottom_bars() {
        assert_eq!(letterbox_rect(100, 200, 100, 100), (25, 0, 50, 100));
    }

    #[test]
    fn letterboxed_pixels_are_scaled_flipped_and_centered() {
        // 2x2 source, four distinct colours, into a 4x2 target: scale = 1,
        // content 2x2 centered at x=1 — one black bar left and right. The
        // output is bottom-up, so the source's top row appears in the
        // output's bottom row.
        let src: Vec<u8> = [
            220, 0, 0, 255, // top row, left: red
            0, 220, 0, 255, // top row, right: green
            0, 0, 220, 255, // bottom row, left: blue
            220, 220, 0, 255, // bottom row, right: yellow
        ]
        .to_vec();
        let dst = letterbox_bgra_bytes(&src, 2, 2, 4, 2);
        let px = |x: usize, y_from_top: usize| {
            let row = 1 - y_from_top;
            &dst[(row * 4 + x) * 4..(row * 4 + x) * 4 + 4]
        };
        assert_eq!(px(0, 0), &[0, 0, 0, 255]);
        assert_eq!(px(3, 0), &[0, 0, 0, 255]);
        assert_eq!(px(0, 1), &[0, 0, 0, 255]);
        assert_eq!(px(3, 1), &[0, 0, 0, 255]);
        // Top image row (y_from_top=0) = source top row (red, green); the
        // bottom-up output puts it in the buffer's last row.
        assert_eq!(px(1, 0), &[220, 0, 0, 255]);
        assert_eq!(px(2, 0), &[0, 220, 0, 255]);
        // Bottom image row (y_from_top=1) = source bottom row (blue, yellow).
        assert_eq!(px(1, 1), &[0, 0, 220, 255]);
        assert_eq!(px(2, 1), &[220, 220, 0, 255]);
    }

    #[test]
    fn letterbox_bars_are_opaque_black() {
        // 1x1 red source into 4x2: scale = 2, content 2x2 centered at x=1.
        let src = [255u8, 0, 0, 255];
        let dst = letterbox_bgra_bytes(&src, 1, 1, 4, 2);
        let px = |x: usize, y_from_top: usize| {
            let row = 1 - y_from_top;
            &dst[(row * 4 + x) * 4..(row * 4 + x) * 4 + 4]
        };
        for y in 0..2 {
            assert_eq!(px(0, y), &[0, 0, 0, 255]);
            assert_eq!(px(3, y), &[0, 0, 0, 255]);
            assert_eq!(px(1, y), &[255, 0, 0, 255]);
            assert_eq!(px(2, y), &[255, 0, 0, 255]);
        }
    }
}
