use eframe::egui_wgpu::{WgpuSetup, WgpuSetupCreateNew};
use eframe::{Renderer, UserEvent, WgpuConfiguration};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::raw_window_handle::HasWindowHandle;

use crate::capture::recorder::{RecorderManager, SessionStatus};
use crate::config::AppConfig;
use crate::log;

pub mod session;
pub mod theme;
pub mod widgets;
pub mod window;

/// Windows session-end watcher (shutdown/logoff finalize).
#[cfg(windows)]
pub mod shutdown;

/// Single-instance protection (second launch surfaces the running window).
#[cfg(windows)]
pub mod single_instance;

const APP_NAME: &str = "Featherweight";

pub fn run() -> eframe::Result {
    log::log_init();
    log::log("Initializing Featherweight");

    // Single-instance protection: when an instance is already running, a
    // second launch must not start — it surfaces the running instance's
    // window (whether visible or not) and exits before any UI is created,
    // so there is no tray-icon flicker.
    #[cfg(windows)]
    match single_instance::acquire() {
        single_instance::Acquisition::Acquired => {}
        single_instance::Acquisition::AlreadyRunning => {
            log::log("another Featherweight instance is running; surfacing it and exiting");
            single_instance::signal_previous();
            return Ok(());
        }
    }

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

    // The session hub is the meeting point between the recorder, the UI, and
    // the tray (see session.rs). It is built inside the creator closure below
    // because it needs the real egui context from `cc`; the closure stashes a
    // clone in this handle so `try_run` can still reach it after the event
    // loop ends and finalize a still-running session (see
    // `finalize_active_session`).
    let hub_handle: Arc<Mutex<Option<Arc<session::SessionShared>>>> = Arc::default();
    let hub_for_closure = hub_handle.clone();

    let mut app = eframe::create_native(
        APP_NAME,
        native_options,
        Box::new(move |cc| {
            // The window mirrors its config into the hub and pulls tray-made
            // preference changes; the pump thread writes recording status;
            // the tray acts through it (stop, new recording, folder, prefs).
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
                hotkeys: Arc::new(crate::hotkeys::HotkeyController::new(config.shortcuts)),
                show_requested: AtomicBool::new(false),
            });
            *hub_for_closure.lock().unwrap() = Some(shared.clone());

            // Watch for Windows ending the session (shutdown, logoff, or a
            // restart-manager close such as installing our own MSI over a
            // running app): those paths bypass the eframe event loop, so a
            // hidden window watches for WM_ENDSESSION and finalizes any
            // running recording before the process is terminated.
            #[cfg(windows)]
            crate::app::shutdown::spawn(shared.clone());

            // Single-instance show listener: surfaces this instance's window
            // when a second launch signals it. Needs the main window's Win32
            // handle because no egui frames run while the window is hidden.
            #[cfg(windows)]
            {
                let main_hwnd = match cc.window_handle().map(|h| h.as_raw()) {
                    Ok(winit::raw_window_handle::RawWindowHandle::Win32(w)) => w.hwnd.get(),
                    _ => {
                        crate::log::log("single-instance: no Win32 window handle; relaunches will not surface this instance");
                        0
                    }
                };
                crate::app::single_instance::spawn_show_listener(main_hwnd, shared.clone());
            }

            // Global hotkeys: the polling thread watches the keyboard every
            // 8 ms and sends events; the consumer thread below turns them
            // into hub actions. Start/stop must work while the window is
            // hidden — and no egui frames run while it is hidden — so the
            // events deliberately do not go through the window's frame
            // handler.
            {
                // The poller sets the shared stop flag directly on the stop
                // hotkey (lowest latency; it does not wait for a repaint).
                shared.hotkeys.set_stop_signal(shared.stop_flag.clone());
                let (tx, rx) = std::sync::mpsc::channel::<crate::hotkeys::HotkeyEvent>();
                let egui = shared.egui.clone();
                match crate::hotkeys::spawn(tx, shared.hotkeys.clone(), move || {
                    egui.request_repaint()
                }) {
                    Ok(_) => {
                        let consumer = shared.clone();
                        let _ = std::thread::Builder::new()
                            .name("hotkey-actions".into())
                            .spawn(move || {
                                for event in rx {
                                    apply_hotkey_event(&consumer, event);
                                }
                            });
                    }
                    Err(error) => {
                        crate::log::log(&format!("could not start hotkey polling: {error}"))
                    }
                }
            }

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

    let run_result = event_loop.run_app(&mut app);

    // The event loop has ended (tray Quit, close-with-quit, or a renderer
    // error). If a session is still running, stop it and wait for the worker
    // to finalize the encoder and run the mux — quitting mid-encode must not
    // leave an unplayable file behind. The handle is None only if the app was
    // never created, in which case nothing could have been recording.
    if let Some(shared) = hub_handle.lock().unwrap().take() {
        session::finalize_active_session(&shared);
    }

    run_result.map_err(eframe::Error::WinitEventLoop)
}

/// Turns a hotkey event into a hub action, with no UI involvement.
///
/// Runs on the `hotkey-actions` consumer thread (spawned by `try_run`), not
/// the window: hotkeys must work while the window is hidden, and no egui
/// frames run while it is hidden, so an event routed through the window's
/// frame handler would never be seen.
pub(crate) fn apply_hotkey_event(
    shared: &Arc<session::SessionShared>,
    event: crate::hotkeys::HotkeyEvent,
) {
    use crate::hotkeys::HotkeyEvent;

    match event {
        HotkeyEvent::Start => {
            // The hub's config snapshot is what the tray's "New recording"
            // uses, so a hotkey start honors exactly the same preferences —
            // including changes made from the tray while the window was
            // hidden.
            let config = shared.config.lock().unwrap().clone();
            if shared.start_recording(config) {
                crate::log::log("hotkey: recording started");
                shared.egui.request_repaint();
            }
        }
        HotkeyEvent::Stop => {
            let idle = matches!(*shared.status.lock().unwrap(), SessionStatus::Idle);
            if idle {
                // Stray stop with no session running: the poller set the
                // shared stop flag directly for latency, and `finish` (which
                // re-arms the flag) never runs for a session that never was.
                // Re-arm it here, or the next session would stop the instant
                // it starts.
                shared.stop_flag.store(false, Ordering::Release);
            } else {
                // Explain the stop like a manual one — but never clobber a
                // real reason an automatic guard (disk, duration) may have
                // recorded first.
                let mut reason = shared.stop_reason.lock().unwrap();
                if reason.is_none() {
                    *reason = Some(session::StopReason::User);
                }
            }
        }
        HotkeyEvent::Availability { start, stop } => {
            crate::log::log(&format!("hotkeys active: start={start} stop={stop}"));
        }
    }
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