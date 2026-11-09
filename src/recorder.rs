/// recorder.rs — capture handler and start/stop helpers.
///
/// Uses the video/ and mux/ modules (IMFSinkWriter pipeline) instead of
/// windows-capture's VideoEncoder. The capture handler converts each BGRA
/// frame to NV12 via VideoProcessorBlt, wraps it in a tracked-sample from
/// the NV12 pool, and writes it to the MuxWriter.
///
/// Dependency boundary: this file must NOT import tao, tray-icon, or global-hotkey.
/// It communicates with app.rs only through mpsc channels and CaptureControl.
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::graphics_capture_picker::PickedGraphicsCaptureItem;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::mux::container::ContainerMode;
use crate::mux::{MuxConfig, MuxWriter};
use crate::video::convert::VideoProcessor;
use crate::video::encode;
use crate::video::pool::{POOL_SIZE, TexturePool};

// ---------------------------------------------------------------------------
// Send wrapper for PickedGraphicsCaptureItem
//
// PickedGraphicsCaptureItem is !Send because it contains a raw HWND pointer.
// However, start_free_threaded initialises a fresh WinRT multi-threaded
// apartment on the capture thread before consuming the item, so crossing the
// thread boundary here is safe in practice.
// ---------------------------------------------------------------------------
struct SendItem(PickedGraphicsCaptureItem);

// SAFETY: we only move the item across thread boundaries to hand it to
// windows-capture's own start_free_threaded, which sets up the correct WinRT
// apartment on the receiving thread before using it.
unsafe impl Send for SendItem {}
unsafe impl Sync for SendItem {}

impl TryInto<GraphicsCaptureItemType> for SendItem {
    type Error = <PickedGraphicsCaptureItem as TryInto<GraphicsCaptureItemType>>::Error;

    fn try_into(self) -> Result<GraphicsCaptureItemType, Self::Error> {
        self.0.try_into()
    }
}

/// Target frame interval — approximately 30 FPS.
const TARGET_FRAME_INTERVAL_MS: u32 = 33;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum StopReason {
    UserStopped,
    UserQuit,
    TargetClosed,
    DiskLow { free_bytes: u64 },
    EncoderError(String),
}

impl StopReason {
    pub fn description(&self) -> String {
        match self {
            StopReason::UserStopped => "Stopped by user".into(),
            StopReason::UserQuit => "Quit by user".into(),
            StopReason::TargetClosed => "Window closed".into(),
            StopReason::DiskLow { free_bytes } => {
                format!("Disk space low ({} free)", crate::disk::format_bytes(*free_bytes))
            }
            StopReason::EncoderError(e) => format!("Encoder error: {e}"),
        }
    }
}

/// Status events sent from the capture thread to the main thread for tray display.
/// (Production spec section 6.)
#[derive(Debug, Clone)]
pub enum StatusKind {
    /// Encoder friendly name (e.g. "Intel Quick Sync Video H.264 Encoder MFT").
    EncoderName(String),
    /// Fires when the encoder is Microsoft's software fallback.
    SoftwareEncodingWarning,
    /// Container mode in use ("fMP4" or "MP4").
    ContainerMode(String),
}

#[derive(Debug)]
pub enum RecorderEvent {
    CaptureEnded { reason: StopReason, path: PathBuf },
    Status(StatusKind),
}

pub struct RecordingFlags {
    pub size: (u32, u32),
    pub output: PathBuf,
    pub events: Sender<RecorderEvent>,
    pub mux: Arc<MuxWriter>,
}

// ---------------------------------------------------------------------------
// Capture handler
// ---------------------------------------------------------------------------

pub struct CaptureHandler {
    mux: Arc<MuxWriter>,
    video_proc: Option<VideoProcessor>,
    pool: Option<Arc<Mutex<TexturePool>>>,
    flags: RecordingFlags,
    started: Instant,
    /// Recording origin T0: timestamp of the first video frame (100 ns units).
    origin_100ns: Option<i64>,
    /// Previous frame timestamp (for sample duration calculation).
    prev_ts_100ns: i64,
    width: u32,
    height: u32,
    initialized: bool,
    /// Frames dropped due to pool exhaustion (no free NV12 texture).
    dropped_pool: u64,
}

// SAFETY: COM pointers are created and used only on the capture thread (MTA).
// The Arc<MuxWriter> is Send + Sync by construction.
unsafe impl Send for CaptureHandler {}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = RecordingFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let flags = ctx.flags;
        let (width, height) = flags.size;
        Ok(Self {
            mux: flags.mux.clone(),
            video_proc: None,
            pool: None,
            flags,
            started: Instant::now(),
            origin_100ns: None,
            prev_ts_100ns: 0,
            width,
            height,
            initialized: false,
            dropped_pool: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let ts = frame.timestamp()?.Duration;

        // --- First frame: lazy init (section 3.2) ---
        if !self.initialized {
            let device = frame.device();
            let context = frame.device_context();

            self.mux.initialize(device, context).map_err(|e| {
                let msg = format!("MuxWriter init failed: {e}");
                crate::log::log(&msg);
                // Send CaptureEnded so the main thread isn't left waiting.
                let _ = self.flags.events.send(RecorderEvent::CaptureEnded {
                    reason: StopReason::EncoderError(msg.clone()),
                    path: self.flags.output.clone(),
                });
                capture_control.stop();
                msg
            })?;

            // Create the video processor (BGRA → NV12 conversion).
            self.video_proc = Some(VideoProcessor::new(device, context, self.width, self.height).map_err(|e| {
                let msg = format!("VideoProcessor init: 0x{:08X} ({e})", e.code().0 as u32);
                crate::log::log(&msg);
                msg
            })?);

            // Create the NV12 texture pool.
            self.pool = Some(Arc::new(Mutex::new(
                TexturePool::new(device, self.width, self.height, POOL_SIZE).map_err(|e| {
                    let msg = format!("TexturePool init: 0x{:08X} ({e})", e.code().0 as u32);
                    crate::log::log(&msg);
                    msg
                })?,
            )));

            // Set T0 = first frame timestamp (section 3.6).
            self.origin_100ns = Some(ts);
            self.prev_ts_100ns = ts;
            self.initialized = true;

            crate::log::log(&format!(
                "First frame: T0={ts}, {}x{}, pool={POOL_SIZE}",
                self.width, self.height
            ));

            // Send status events with encoder info and container mode.
            if let Some(info) = self.mux.encoder_info() {
                let _ = self.flags.events.send(RecorderEvent::Status(
                    StatusKind::EncoderName(info.name),
                ));
                if info.is_software {
                    let _ = self.flags.events.send(RecorderEvent::Status(
                        StatusKind::SoftwareEncodingWarning,
                    ));
                }
            }
            let _ = self.flags.events.send(RecorderEvent::Status(
                StatusKind::ContainerMode(self.mux.container_mode().as_str().to_string()),
            ));

            // Don't encode the first frame — init time consumed this callback.
            return Ok(());
        }

        // Guard against frames arriving after finalize (e.g. late delivery).
        if !self.mux.is_ready() {
            return Ok(());
        }

        // --- Timestamps (section 3.6) ---
        let origin = self.origin_100ns.unwrap();
        let sample_time = ts - origin;
        let sample_dur = (ts - self.prev_ts_100ns).max(1);
        self.prev_ts_100ns = ts;

        let src = frame.as_raw_texture();

        // --- Acquire NV12 pool texture (section 3.4) ---
        let dst = {
            let pool = self.pool.as_ref().unwrap();
            let mut pool_guard = pool.lock().unwrap();
            match pool_guard.acquire() {
                Some(t) => t,
                None => {
                    self.dropped_pool += 1;
                    return Ok(());
                }
            }
        };

        // --- BGRA → NV12 conversion [V] ~0.5 ms ---
        if let Some(vp) = self.video_proc.as_mut() {
            if let Err(e) = vp.blt(src, &dst) {
                crate::log::log(&format!(
                    "VideoProcessorBlt error: 0x{:08X} ({e})",
                    e.code().0 as u32
                ));
                self.dropped_pool += 1;
                return Ok(());
            }
        }

        // Flush submits queued GPU work; it does not wait for completion.
        unsafe { frame.device_context().Flush() };

        // --- Create tracked sample and write [V] ~20-80 µs ---
        let pool_arc = self.pool.as_ref().unwrap();
        let sample = match encode::make_video_sample(&dst, sample_time, sample_dur, Some(pool_arc)) {
            Ok(s) => s,
            Err(e) => {
                crate::log::log(&format!(
                    "make_video_sample error: 0x{:08X} ({e})",
                    e.code().0 as u32
                ));
                return Ok(());
            }
        };

        if let Err(e) = self.mux.write_video(&sample) {
            let msg = format!("WriteSample error: 0x{:08X} ({e})", e.code().0 as u32);
            crate::log::log(&msg);

            // Attempt finalization before stopping.
            let _ = self.mux.finalize();

            let _ = self.flags.events.send(RecorderEvent::CaptureEnded {
                reason: StopReason::EncoderError(msg),
                path: self.flags.output.clone(),
            });

            capture_control.stop();
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        let elapsed = self.started.elapsed().as_secs();
        crate::log::log(&format!(
            "Capture item closed after {elapsed}s; finalizing mux writer"
        ));

        // Finalize before sending the event so the file is complete when the
        // main thread receives the notification.
        if let Err(e) = self.mux.finalize() {
            crate::log::log(&format!(
                "MuxWriter finalize error: 0x{:08X} ({e})",
                e.code().0 as u32
            ));
        }

        if self.dropped_pool > 0 {
            crate::log::log(&format!(
                "Recording stats: dropped_pool={}",
                self.dropped_pool
            ));
        }

        // Ignore send errors — receiver may be gone during shutdown.
        let _ = self.flags.events.send(RecorderEvent::CaptureEnded {
            reason: StopReason::TargetClosed,
            path: self.flags.output.clone(),
        });

        Ok(())
    }
}

impl Drop for CaptureHandler {
    fn drop(&mut self) {
        // Safety net: if finalize was not called by on_closed or on_frame_arrived,
        // finalize now. MuxWriter::finalize() is idempotent (checks state.finalized).
        let _ = self.mux.finalize();
    }
}

// ---------------------------------------------------------------------------
// start / stop helpers
// ---------------------------------------------------------------------------

/// Start free-threaded capture with the IMFSinkWriter pipeline.
///
/// Returns the `CaptureControl` handle. The caller is responsible for keeping
/// it alive for the duration of the recording.
pub fn start_capture(
    item: PickedGraphicsCaptureItem,
    size: (u32, u32),
    output: PathBuf,
    events: Sender<RecorderEvent>,
) -> Result<
    CaptureControl<CaptureHandler, Box<dyn std::error::Error + Send + Sync>>,
    Box<dyn std::error::Error>,
> {
    // NV12 requires even dimensions (section 3.3).
    let width = encode::round_even(size.0);
    let height = encode::round_even(size.1);

    let config = MuxConfig {
        width,
        height,
        output_path: output.to_str().ok_or("Invalid output path")?.to_string(),
        // Video-only for now; audio tracks added in the audio phase.
        audio_track_count: 0,
        container_mode: ContainerMode::Fragmented,
        low_latency: false,
        mic_track_enabled: false,
    };

    let mux = Arc::new(MuxWriter::new(config));

    let flags = RecordingFlags {
        size: (width, height),
        output: output.clone(),
        events,
        mux: mux.clone(),
    };

    let send_item = SendItem(item);
    let draw_border = DrawBorderSettings::WithoutBorder;

    let settings = Settings::new(
        send_item,
        CursorCaptureSettings::Default,
        draw_border,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(
            TARGET_FRAME_INTERVAL_MS as u64,
        )),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8, // WGC's native format (section 3.5)
        flags,
    );

    match CaptureHandler::start_free_threaded(settings) {
        Ok(control) => Ok(control),
        Err(e) => {
            crate::log::log(&format!("start_free_threaded failed: {e}"));
            Err(Box::new(e))
        }
    }
}
