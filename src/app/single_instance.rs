//! Single-instance protection.
//!
//! Launching Featherweight while one is already running must not start a
//! second copy — it should surface the running instance's window (whether
//! hidden, minimized, or just behind other windows) and exit. This is what
//! users expect from a tray app and what double-clicking the exe out of
//! habit does.
//!
//! Mechanism, all named after the installer's UpgradeCode GUID so the names
//! are stable across versions:
//!
//! - `Local\Featherweight-instance-{GUID}` — a named mutex the first instance
//!   holds for its lifetime (the handle is deliberately leaked: closing it
//!   would release the claim while the app is still running). The object
//!   dies with the process, so a crashed instance never wedges the next
//!   launch. The `Local\` namespace scopes it to the logon session.
//! - `Local\Featherweight-show-{GUID}` — an auto-reset event the first
//!   instance creates; a wait thread wakes on it and flags the hub, which the
//!   window turns into Visible + Focus on the next frame.
//! - `Local\Featherweight-pid-{GUID}` — a 4-byte file mapping carrying the
//!   first instance's PID. The second instance reads it and calls
//!   `AllowSetForegroundWindow` before signaling: the second process is the
//!   one the user just launched, so *it* can grant foreground rights — the
//!   first cannot grant them to itself, and without the grant the surfaced
//!   window would appear behind whatever had focus.
//!
//! The second instance signals and exits before any UI is created, so there
//! is no tray-icon flicker.

use std::sync::Arc;

use windows::core::HSTRING;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcessId, OpenEventW, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use super::session::SessionShared;

/// The installer's UpgradeCode (wix/main.wxs). Reusing it keeps the kernel
/// object names stable across versions without introducing a second GUID to
/// keep in sync.
const GUID: &str = "4721429C-FCE3-40BF-B553-7E8B1FFE0F19";

const MUTEX_NAME: &str = "Local\\Featherweight-instance-";
const SHOW_EVENT_NAME: &str = "Local\\Featherweight-show-";
const PID_MAPPING_NAME: &str = "Local\\Featherweight-pid-";

/// Result of claiming single-instance ownership.
pub enum Acquisition {
    /// This process is the running instance and holds the claim.
    Acquired,
    /// Another instance is already running; this process must not start.
    AlreadyRunning,
}

/// Claims single-instance ownership. Fail-open: if the OS objects to creating
/// the objects (denied namespace, wedged state), the app still starts and the
/// condition is logged — refusing to start at all would be worse than a rare
/// duplicate.
pub fn acquire() -> Acquisition {
    let name = HSTRING::from(format!("{MUTEX_NAME}{GUID}"));
    // SAFETY: no attributes, no initial owner, string name; the returned
    // handle is leaked below on purpose (see module docs).
    match unsafe { CreateMutexW(None, false, &name) } {
        Ok(_handle) => {
            let already = std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_ALREADY_EXISTS.0 as i32);
            if already {
                return Acquisition::AlreadyRunning;
            }
            // The mutex object lives until the process exits: `HANDLE` is a
            // plain `Copy` wrapper with no `Drop`, so nothing here can
            // release the claim early. No action needed.
            Acquisition::Acquired
        }
        Err(e) => {
            crate::log::log(&format!(
                "single-instance: could not create the instance mutex ({e}); starting anyway"
            ));
            Acquisition::Acquired
        }
    }
}

/// Wakes the running instance so it surfaces its window. Called by the second
/// instance right before it exits.
pub fn signal_previous() {
    unsafe {
        // Grant foreground rights first: this process is foreground-eligible
        // (the user just launched it), the running one is not.
        let mapping_name = HSTRING::from(format!("{PID_MAPPING_NAME}{GUID}"));
        if let Ok(mapping) = OpenFileMappingW(FILE_MAP_READ.0, false, &mapping_name) {
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 4);
            if !view.Value.is_null() {
                let pid = std::ptr::read(view.Value.cast::<u32>());
                let _ = UnmapViewOfFile(view);
                if pid != 0 {
                    let _ = AllowSetForegroundWindow(pid);
                }
            }
        }

        let event_name = HSTRING::from(format!("{SHOW_EVENT_NAME}{GUID}"));
        match OpenEventW(EVENT_MODIFY_STATE, false, &event_name) {
            Ok(event) => {
                if let Err(e) = SetEvent(event) {
                    crate::log::log(&format!("single-instance: could not signal the running instance: {e}"));
                }
            }
            Err(e) => crate::log::log(&format!(
                "single-instance: running instance has no show event ({e}); exiting without surfacing it"
            )),
        }
    }
}

/// First-instance side: publishes this process's PID and spawns the wait
/// thread that turns the show event into a surfaced window. `main_hwnd` is
/// the main window's Win32 handle (0 = unknown, e.g. in tests: the window is
/// then not touched and only the hub flag is set). Called once, when the
/// session hub is created. Object-creation failures are logged; the app
/// still runs (only the "surface on relaunch" behavior would be lost).
pub fn spawn_show_listener(main_hwnd: isize, shared: Arc<SessionShared>) {
    unsafe {
        let mapping_name = HSTRING::from(format!("{PID_MAPPING_NAME}{GUID}"));
        // SAFETY: anonymous (INVALID_HANDLE_VALUE) 4-byte read/write mapping.
        match CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, 4, &mapping_name) {
            Ok(mapping) => {
                let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, 4);
                if view.Value.is_null() {
                    crate::log::log("single-instance: could not map the PID view; relaunches may not focus the window");
                } else {
                    std::ptr::write(view.Value.cast::<u32>(), GetCurrentProcessId());
                }
            }
            Err(e) => crate::log::log(&format!(
                "single-instance: could not create the PID mapping: {e}"
            )),
        }

        let event_name = HSTRING::from(format!("{SHOW_EVENT_NAME}{GUID}"));
        let Ok(event) = CreateEventW(None, false, false, &event_name) else {
            crate::log::log("single-instance: could not create the show event; relaunches will not surface this instance");
            return;
        };
        // The event object lives until the process exits (`HANDLE` has no
        // `Drop`), and the wait thread below holds the raw value.

        // `HANDLE` is a raw pointer and not `Send`; the raw value is, and the
        // handle stays valid for the process lifetime (it is never closed).
        let event_raw = event.0 as usize;

        let spawned = std::thread::Builder::new()
            .name("instance-show-watch".into())
            .spawn(move || {
                let event = HANDLE(event_raw as *mut core::ffi::c_void);
                // Auto-reset event: every signal wakes exactly one loop pass.
                loop {
                    WaitForSingleObject(event, INFINITE);
                    // Show directly via Win32: while the window is hidden no
                    // egui frames run (see tray/mod.rs), so a repaint-driven
                    // surface would never be consumed. SW_RESTORE shows and
                    // activates whether the window is hidden or minimized —
                    // the same primitives the tray's left-click show uses.
                    if main_hwnd != 0 {
                        unsafe {
                            ShowWindow(main_hwnd as _, SW_RESTORE);
                            SetForegroundWindow(main_hwnd as _);
                        }
                    }
                    // Sync the app's own visibility state on the next frame
                    // it happens to paint (idempotent: Visible + Focus).
                    shared
                        .show_requested
                        .store(true, std::sync::atomic::Ordering::Release);
                    shared.egui.request_repaint();
                }
            });
        if spawned.is_err() {
            crate::log::log("single-instance: could not spawn the show-wait thread; relaunches will not surface this instance");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    use crate::app::session::{SessionShared, SessionStats};
    use crate::capture::recorder::{RecorderManager, SessionStatus};
    use eframe::egui::Context;
    use std::sync::Mutex as Mutex;

    // The kernel objects are process-global: this module's tests must not run
    // concurrently with each other, so they are serialized through this lock
    // (tests in other modules never touch single_instance).
    static SINGLE_INSTANCE_LOCK: StdMutex<()> = StdMutex::new(());

    fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        done()
    }

    fn test_shared() -> Arc<SessionShared> {
        Arc::new(SessionShared {
            egui: Context::default(),
            status: Mutex::new(SessionStatus::Idle),
            stats: Mutex::new(SessionStats::default()),
            stop_reason: Mutex::new(None),
            last_error: Mutex::new(None),
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mic_track_on: std::sync::atomic::AtomicBool::new(false),
            mic_muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            prefs_dirty: std::sync::atomic::AtomicBool::new(false),
            config: Mutex::new(crate::config::AppConfig::default()),
            recorder: Mutex::new(RecorderManager::default()),
            show_requested: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Second acquisition detects the running instance, and signaling it
    /// flips the hub's show-request flag — the exact contract the second
    /// process and the window rely on.
    #[test]
    fn second_acquire_signals_the_running_instance() {
        let _guard = SINGLE_INSTANCE_LOCK.lock().unwrap();

        // First launch in this (test) process claims ownership.
        match acquire() {
            Acquisition::Acquired => {}
            Acquisition::AlreadyRunning => panic!("first acquire should claim the mutex"),
        }

        // A "second launch" must be detected...
        match acquire() {
            Acquisition::Acquired => panic!("second acquire must see the running instance"),
            Acquisition::AlreadyRunning => {}
        }

        // ...and signaling it must surface the running instance's window.
        // hwnd 0 = no window in the test process; only the hub flag is set.
        let shared = test_shared();
        spawn_show_listener(0, shared.clone());
        signal_previous();
        let surfaced = wait_until(Duration::from_secs(2), || {
            shared.show_requested.load(Ordering::Acquire)
        });
        assert!(surfaced, "show request never reached the hub");
    }
}
