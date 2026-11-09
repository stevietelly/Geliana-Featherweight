/// app.rs — State enum, UserEvent, tao event loop, menu handling, tick logic.
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tray_icon::TrayIconEvent;
use tray_icon::menu::MenuEvent;
use windows_capture::capture::CaptureControl;
use windows_capture::graphics_capture_picker::GraphicsCapturePicker;

use crate::disk::{GrowthEstimator, format_bytes, free_bytes_for_path, hard_floor, WARN_FLOOR_BYTES};
use crate::paths::{output_dir, output_path};
use crate::recorder::{CaptureHandler, RecorderEvent, StatusKind, StopReason, start_capture};
use windows_capture::graphics_capture_picker::PickedGraphicsCaptureItem;use crate::tray::{TrayMenu, build_tray, recording_icon, recording_status, stopped_icon, warn_icon};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum UserEvent {
    Tick,
    /// Menu events are forwarded by the bridge thread but currently routed
    /// directly via the menu channel in the tick handler.
    #[allow(dead_code)]
    Menu(MenuEvent),
    Recorder(RecorderEvent),
}

type Handler = CaptureHandler;
type HandlerError = Box<dyn std::error::Error + Send + Sync>;
type Control = CaptureControl<Handler, HandlerError>;

enum State {
    Recording {
        started: Instant,
        path: PathBuf,
        control: Option<Control>,
        growth: GrowthEstimator,
        disk_warn: bool,
    },
    Stopping {
        #[allow(dead_code)]
        reason: StopReason,
        exit_after: bool,
        deadline: Instant,
    },
    Stopped {
        reason: StopReason,
        #[allow(dead_code)]
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the application event loop. Returns when the user quits.
pub fn run() {
    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::<UserEvent>::with_user_event()
        .build();

    let proxy = event_loop.create_proxy();

    // MPSC channel: capture thread → bridge thread → main thread
    let (event_tx, event_rx) = mpsc::channel::<RecorderEvent>();

    // Bridge thread: wakes the tao event loop when a RecorderEvent arrives.
    {
        let proxy2 = proxy.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = event_rx.recv() {
                let _ = proxy2.send_event(UserEvent::Recorder(ev));
            }
            // All senders dropped — bridge exits.
        });
    }

    // Tick thread: sends a Tick every second to drive disk checks and tooltip updates.
    {
        let proxy3 = proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if proxy3.send_event(UserEvent::Tick).is_err() {
                break;
            }
        });
    }

    let output_directory = output_dir();

    // Pick the initial capture item.
    let (item, size) = match pick_item() {
        Some(v) => v,
        None => {
            crate::log::log("User cancelled picker at startup — exiting");
            return;
        }
    };

    // Pre-flight disk check.
    let out_path = output_path(&output_directory);
    if let Some(free) = free_bytes_for_path(&out_path) {
        if free <= hard_floor() {
            crate::log::log(&format!(
                "Disk space too low at startup ({} free) — exiting",
                format_bytes(free)
            ));
            return;
        }
    }

    crate::log::log(&format!(
        "Starting capture: size={:?}, output={}",
        size,
        out_path.display()
    ));

    let control = match start_capture(item, size, out_path.clone(), event_tx.clone()) {
        Ok(c) => c,
        Err(e) => {
            crate::log::log(&format!("Failed to start capture: {e}"));
            return;
        }
    };

    let mut state = State::Recording {
        started: Instant::now(),
        path: out_path,
        control: Some(control),
        growth: GrowthEstimator::new(),
        disk_warn: false,
    };

    // Build tray
    let (menu, tray_items) = TrayMenu::new(true);
    let mut tray = build_tray(recording_icon(), menu, "Recording…");

    // MenuEvent channel set up by tray-icon crate
    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    event_loop.run(move |event, _, control_flow| {
        // Default: wait until we send a Tick event (driven by the tick thread).
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {}

            Event::UserEvent(UserEvent::Tick) => {
                // Drain any pending menu events first.
                while let Ok(ev) = menu_channel.try_recv() {
                    handle_menu_event(
                        ev,
                        &mut state,
                        &tray_items,
                        &mut tray,
                        &event_tx,
                        &output_directory,
                        control_flow,
                    );
                }
                // Drain tray icon events (clicks etc.) — we don't act on them but
                // the channel must be drained to avoid a build-up.
                while tray_channel.try_recv().is_ok() {}

                tick(
                    &mut state,
                    &tray_items,
                    &mut tray,
                    &event_tx,
                    control_flow,
                );
            }

            Event::UserEvent(UserEvent::Menu(ev)) => {
                handle_menu_event(
                    ev,
                    &mut state,
                    &tray_items,
                    &mut tray,
                    &event_tx,
                    &output_directory,
                    control_flow,
                );
            }

            Event::UserEvent(UserEvent::Recorder(ev)) => {
                handle_recorder_event(ev, &mut state, &tray_items, &mut tray, control_flow);
            }

            Event::LoopDestroyed => {
                crate::log::log("Event loop destroyed");
            }

            _ => {}
        }
    });
}

// ---------------------------------------------------------------------------
// Picker helper
// ---------------------------------------------------------------------------

fn pick_item() -> Option<(PickedGraphicsCaptureItem, (u32, u32))> {
    let item = GraphicsCapturePicker::pick_item().ok()??;
    let size = item.size().ok()?;
    let w = size.0 as u32;
    let h = size.1 as u32;
    crate::log::log(&format!("Picked capture item: {w}x{h}"));
    Some((item, (w, h)))
}

// ---------------------------------------------------------------------------
// Tick — disk guard, tooltip update
// ---------------------------------------------------------------------------

fn tick(
    state: &mut State,
    items: &TrayMenu,
    tray: &mut tray_icon::TrayIcon,
    event_tx: &Sender<RecorderEvent>,
    control_flow: &mut ControlFlow,
) {
    match state {
        State::Recording { started, path, control: _, growth, disk_warn } => {
            let elapsed = started.elapsed().as_secs();

            // Query disk space.
            let free = free_bytes_for_path(path).unwrap_or(u64::MAX);
            let floor = hard_floor();

            // Compute all values we need while still in the match arm.
            let disk_low = free <= floor;
            *disk_warn = free < WARN_FLOOR_BYTES;

            // Growth estimate.
            let file_size = std::fs::metadata(&*path).map(|m| m.len()).unwrap_or(0);
            let bps = growth.push(file_size);
            let eta_min = bps.and_then(|r| {
                if r > 0.0 {
                    Some((free.saturating_sub(floor)) as f64 / r / 60.0)
                } else {
                    None
                }
            });

            let warn = *disk_warn;

            if !disk_low {
                let status = recording_status(elapsed, file_size, free, eta_min, warn);
                let _ = items.status_item.set_text(&status);
                let _ = tray.set_tooltip(Some(&status));
                if warn {
                    let _ = tray.set_icon(Some(warn_icon()));
                } else {
                    let _ = tray.set_icon(Some(recording_icon()));
                }
            }

            if disk_low {
                crate::log::log(&format!(
                    "Disk guard triggered: {} free, stopping recording",
                    format_bytes(free)
                ));
                // match arm borrow ends here before we call request_stop
            } else {
                return;
            }

            // Fall through only when disk_low == true.
            // Match arm is no longer borrowed.
            request_stop(state, StopReason::DiskLow { free_bytes: free }, false, event_tx);
            let _ = tray.set_icon(Some(stopped_icon()));
            update_menu_stopped(items);
        }

        State::Stopping { deadline, exit_after, .. } => {
            // Safety timeout: if finalization takes more than 10 seconds, bail out.
            if Instant::now() >= *deadline {
                crate::log::log("Stop deadline exceeded — forcing exit");
                if *exit_after {
                    *control_flow = ControlFlow::Exit;
                }
            }
        }

        State::Stopped { reason, .. } => {
            let desc = reason.description();
            let _ = items.status_item.set_text(&desc);
            let _ = tray.set_tooltip(Some(&desc));
        }
    }
}

// ---------------------------------------------------------------------------
// Menu event handling
// ---------------------------------------------------------------------------

fn handle_menu_event(
    ev: MenuEvent,
    state: &mut State,
    items: &TrayMenu,
    tray: &mut tray_icon::TrayIcon,
    event_tx: &Sender<RecorderEvent>,
    output_directory: &PathBuf,
    control_flow: &mut ControlFlow,
) {
    let id = ev.id();

    if id == items.stop_item.id() {
        if matches!(state, State::Recording { .. }) {
            crate::log::log("User chose Stop from tray");
            request_stop(state, StopReason::UserStopped, false, event_tx);
            let _ = tray.set_icon(Some(stopped_icon()));
            update_menu_stopped(items);
        }
    } else if id == items.quit_item.id() {
        crate::log::log("User chose Quit from tray");
        if matches!(state, State::Recording { .. }) {
            request_stop(state, StopReason::UserQuit, true, event_tx);
            let _ = tray.set_icon(Some(stopped_icon()));
            update_menu_stopped(items);
        } else {
            *control_flow = ControlFlow::Exit;
        }
    } else if id == items.new_item.id() {
        if matches!(state, State::Stopped { .. }) {
            crate::log::log("User chose New recording…");
            if let Some((item, size)) = pick_item() {
                let out = output_path(output_directory);

                // Pre-flight disk check.
                if let Some(free) = free_bytes_for_path(&out) {
                    if free <= hard_floor() {
                        let desc = format!("Disk too low to record ({} free)", format_bytes(free));
                        crate::log::log(&desc);
                        let _ = items.status_item.set_text(&desc);
                        return;
                    }
                }

                crate::log::log(&format!("Starting new capture: output={}", out.display()));

                match start_capture(item, size, out.clone(), event_tx.clone()) {
                    Ok(control) => {
                        *state = State::Recording {
                            started: Instant::now(),
                            path: out,
                            control: Some(control),
                            growth: GrowthEstimator::new(),
                            disk_warn: false,
                        };
                        let _ = tray.set_icon(Some(recording_icon()));
                        update_menu_recording(items);
                    }
                    Err(e) => {
                        let desc = format!("Failed to start: {e}");
                        crate::log::log(&desc);
                        let _ = items.status_item.set_text(&desc);
                    }
                }
            }
        }
    } else if id == items.open_folder_item.id() {
        let dir = output_directory.to_str().unwrap_or(".");
        let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
    }
}

// ---------------------------------------------------------------------------
// Recorder event handling
// ---------------------------------------------------------------------------

fn handle_recorder_event(
    ev: RecorderEvent,
    state: &mut State,
    items: &TrayMenu,
    tray: &mut tray_icon::TrayIcon,
    control_flow: &mut ControlFlow,
) {
    match ev {
        RecorderEvent::CaptureEnded { reason, path } => {
            crate::log::log(&format!(
                "CaptureEnded received: reason={:?}, path={}",
                reason,
                path.display()
            ));

            let exit_after = matches!(
                (&reason, &*state),
                (StopReason::UserQuit, _)
                    | (_, State::Stopping { exit_after: true, .. })
            );

            *state = State::Stopped { reason: reason.clone(), path };

            let desc = reason.description();
            let _ = items.status_item.set_text(&desc);
            let _ = tray.set_tooltip(Some(&desc));
            let _ = tray.set_icon(Some(stopped_icon()));
            update_menu_stopped(items);

            if exit_after {
                crate::log::log("Exiting after capture ended");
                *control_flow = ControlFlow::Exit;
            }
        }
        RecorderEvent::Status(kind) => {
            // Log status events for now; tray status display in the UI phase.
            match &kind {
                StatusKind::EncoderName(name) => {
                    crate::log::log(&format!("Encoder: {name}"));
                }
                StatusKind::SoftwareEncodingWarning => {
                    crate::log::log("WARNING: Software encoder in use");
                }
                StatusKind::ContainerMode(mode) => {
                    crate::log::log(&format!("Container: {mode}"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stop helpers
// ---------------------------------------------------------------------------

/// Initiate a stop: consumes `CaptureControl` on a short-lived thread so that
/// the (blocking) join during finalization doesn't freeze the tray.
/// Transitions `state` to `Stopping` and spawns the join thread.
/// No-ops if `state` is not `Recording` or control has already been taken.
fn request_stop(
    state: &mut State,
    reason: StopReason,
    exit_after: bool,
    event_tx: &Sender<RecorderEvent>,
) {
    // Extract control and path out of the Recording state without borrowing
    // state twice. We take ownership of control here.
    let (control, path) = match state {
        State::Recording { control, path, .. } => (control.take(), path.clone()),
        _ => return,
    };

    let Some(control) = control else { return };

    // Transition to Stopping immediately so the UI reflects it.
    *state = State::Stopping {
        reason: reason.clone(),
        exit_after,
        deadline: Instant::now() + Duration::from_secs(10),
    };

    let tx = event_tx.clone();
    std::thread::spawn(move || {
        crate::log::log(&format!("Stopping capture: reason={reason:?}"));
        if let Err(e) = control.stop() {
            crate::log::log(&format!("CaptureControl::stop error: {e}"));
        }
        // Send the event so the main thread can finalize state.
        let _ = tx.send(RecorderEvent::CaptureEnded { reason, path });
    });
}

fn update_menu_recording(items: &TrayMenu) {
    let _ = items.stop_item.set_enabled(true);
    let _ = items.new_item.set_enabled(false);
}

fn update_menu_stopped(items: &TrayMenu) {
    let _ = items.stop_item.set_enabled(false);
    let _ = items.new_item.set_enabled(true);
}
