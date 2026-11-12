use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};
use eframe::{Renderer, UserEvent, WgpuConfiguration};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use winit::event_loop::{ControlFlow, EventLoop};

use crate::capture::recorder::{RecorderManager, SessionStatus};
use crate::config::AppConfig;
use crate::log;

pub mod session;
pub mod theme;
pub mod widgets;
pub mod window;

const APP_NAME: &str = "Featherweight";

pub fn run() -> eframe::Result {
    log::log_init();
    log::log("Initializing Featherweight");

    // Renderers to try, in order. Glow goes first by default: it is the most
    // compatible backend on Windows, where wgpu's Vulkan path can crash with
    // an access violation on older Intel iGPU drivers. The wgpu fallback
    // forces the DX12 backend for the same reason.
    //
    // Set FTHWT_RENDERER=wgpu to reverse the order.
    let try_wgpu_first = std::env::var("FTHWT_RENDERER").as_deref() == Ok("wgpu");
    let attempts: [(&str, bool); 2] = if try_wgpu_first {
        [("wgpu", true), ("glow", false)]
    } else {
        [("glow", false), ("wgpu", true)]
    };

    let mut last_err: Option<eframe::Error> = None;
    for (name, use_wgpu) in attempts {
        log::log(&format!("Trying renderer '{name}'"));
        match try_run(use_wgpu) {
            Ok(()) => return Ok(()),
            Err(err) => {
                log::log(&format!("Renderer '{name}' failed: {err}"));
                last_err = Some(err);
            }
        }
    }

    Err(last_err.unwrap_or(eframe::Error::AppCreation(
        "No renderer could start".into(),
    )))
}

/// Build a fresh event loop + app and run it once with the requested renderer.
///
/// A fresh event loop per attempt is required: an event loop cannot safely be
/// reused after `run_app` has returned.
fn try_run(use_wgpu: bool) -> eframe::Result {
    // Load persisted settings before the window exists: the viewport's initial
    // visibility and the tray menu's checkbox states both come from them.
    // Without this the UI silently ran on `AppConfig::default()` forever.
    let first_run = AppConfig::path().is_none_or(|path| !path.exists());
    let (config, load_warning) = match AppConfig::load() {
        Ok(loaded) => loaded,
        Err(error) => {
            log::log(&format!("could not load settings: {error}"));
            (AppConfig::default(), Some(error))
        }
    };

    // `hide_window` means "start in the notification area". The very first run
    // shows the window regardless, because there is nothing on screen telling
    // the user where the app went; that frame carries a hint instead.
    let start_visible = starts_visible(config.hide_window, first_run);

    if first_run {
        // Persist the defaults so the next launch is a normal one and the
        // settings file exists to inspect. Failure is non-fatal (read-only or
        // sandboxed environments deny %APPDATA% writes).
        if let Err(error) = config.save() {
            log::log(&format!("could not save initial settings: {error}"));
        }
    }

    let hint = first_run.then(|| {
        "Featherweight runs in the notification area. Click ^ next to the clock, then drag the \
         feather icon onto the taskbar to pin it. (Start hidden can be turned off in this window.)"
            .to_owned()
    });

    log::log(&format!(
        "settings: {} | start hidden = {} | first run = {}",
        AppConfig::path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unavailable>".to_owned()),
        config.hide_window,
        first_run
    ));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    log::log("Winit event loop created");

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([1000.0, 700.0])
        .with_visible(start_visible);
    // Taskbar/title-bar icon. `Featherweight.ico` is also embedded by
    // build.rs, which covers Explorer and alt-tab before the window exists;
    // this sets the live window's icon.
    if let Some(icon) = theme::load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        renderer: if use_wgpu {
            Renderer::Wgpu
        } else {
            Renderer::Glow
        },
        wgpu_options: wgpu_options(),
        ..Default::default()
    };

    let mut app = eframe::create_native(
        APP_NAME,
        native_options,
        Box::new(move |cc| {
            // The session hub is the meeting point between the recorder, the
            // UI, and the tray (see session.rs). The window mirrors its
            // config into the hub and pulls tray-made preference changes;
            // the pump thread writes recording status; the tray acts through
            // it (stop, new recording, folder, prefs).
            let shared = Arc::new(session::SessionShared {
                egui: cc.egui_ctx.clone(),
                status: Mutex::new(SessionStatus::Idle),
                stats: Mutex::new(session::SessionStats::default()),
                stop_reason: Mutex::new(None),
                last_error: Mutex::new(None),
                stop_flag: Arc::new(AtomicBool::new(false)),
                mic_track_on: AtomicBool::new(false),
                mic_muted: Arc::new(AtomicBool::new(false)),
                prefs_dirty: AtomicBool::new(false),
                // Seeded from disk so tray checkboxes and "New recording" see
                // the saved preferences before the window's first frame.
                config: Mutex::new(config.clone()),
                recorder: Mutex::new(RecorderManager::default()),
            });

            // Tray::new installs its own click/menu handlers (Windows) using
            // the window handle and egui context from `cc`.
            let tray = crate::tray::Tray::new(cc, shared.clone())?;

            Ok(Box::new(
                window::FeatherweightApp::new(cc, tray, shared, config, start_visible)
                    .with_startup_notices(load_warning, hint),
            ))
        }),
        &event_loop,
    );

    log::log("Starting Featherweight event loop");

    event_loop
        .run_app(&mut app)
        .map_err(eframe::Error::WinitEventLoop)
}

/// Whether the window is shown when the app starts.
///
/// `hide_window` means "launch into the notification area", but the first run
/// is always shown: the user needs somewhere to read the hint about where the
/// tray icon went.
fn starts_visible(hide_window: bool, first_run: bool) -> bool {
    !hide_window || first_run
}

/// wgpu options that pin the DX12 backend on Windows.
///
/// wgpu otherwise prefers Vulkan, which some Intel iGPU drivers mishandle
/// (process dies with STATUS_ACCESS_VIOLATION during adapter init).
/// On other platforms the default configuration is used unchanged.
#[cfg(windows)]
fn wgpu_options() -> WgpuConfiguration {
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(WgpuSetupCreateNew {
            instance_descriptor: eframe::wgpu::InstanceDescriptor {
                backends: eframe::wgpu::Backends::DX12,
                flags: eframe::wgpu::InstanceFlags::from_build_config().with_env(),
                backend_options: eframe::wgpu::BackendOptions::from_env_or_default(),
                memory_budget_thresholds: eframe::wgpu::MemoryBudgetThresholds::default(),
                display: None,
            },
            ..WgpuSetupCreateNew::without_display_handle()
        }),
        ..Default::default()
    }
}

#[cfg(not(windows))]
fn wgpu_options() -> WgpuConfiguration {
    WgpuConfiguration::default()
}

#[cfg(test)]
mod tests {
    use super::starts_visible;

    #[test]
    fn first_run_always_shows_the_window() {
        // Hidden-at-start is honoured from the second launch on.
        assert!(starts_visible(true, true));
        assert!(!starts_visible(true, false));
        // "Show the window" is honoured regardless.
        assert!(starts_visible(false, false));
        assert!(starts_visible(false, true));
    }
}