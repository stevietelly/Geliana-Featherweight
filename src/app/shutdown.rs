//! Windows session-end watcher: finalizes recordings when the OS ends the
//! session.
//!
//! Shutdown, logoff, and restart-manager closes (installers asking the app to
//! exit) never flow through the eframe event loop — Windows sends
//! `WM_ENDSESSION` straight to top-level windows and then terminates whatever
//! does not exit in time. The `try_run` exit hook (`finalize_active_session`)
//! only covers quits through the app's own event loop, so without this module
//! a hard shutdown mid-recording would kill the encoder mid-stream and leave
//! an unplayable MP4.
//!
//! This module runs a small background thread that owns one hidden top-level
//! window (hidden windows receive the broadcast; message-only windows do
//! not). On `WM_ENDSESSION` with a truthy `wParam` — the session really is
//! ending, or the restart manager demands a close — it finalizes any running
//! session exactly like a normal quit does, then exits the process.
//!
//! Timing honesty: Windows gives each process only a few seconds between
//! `WM_ENDSESSION` and termination. Encoder finalization is fast and the
//! pipeline finalizes the primary MP4 *before* muxing a separate mic track
//! (see `pipeline::ActiveRecording::stop`), so even a kill mid-mux leaves a
//! playable main file — the mux would be the only loss.

use std::sync::{Arc, OnceLock};

use super::session::SessionShared;

/// The hub this watcher finalizes sessions through. Bound once at spawn.
static HUB: OnceLock<Arc<SessionShared>> = OnceLock::new();

/// Spawns the watcher thread. Called once, when the session hub is created.
/// Safe to call again (a second renderer attempt creates a second hub): the
/// first binding wins, and an unbound-in hub can never have had a session.
pub fn spawn(shared: Arc<SessionShared>) {
    if HUB.set(shared).is_err() {
        crate::log::log("session-end watcher: hub already bound; keeping the first binding");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("session-end-watch".into())
        .spawn(watch_thread);
    if spawned.is_err() {
        crate::log::log("session-end watcher: could not spawn the watch thread; \
            shutdown or logoff during a recording may leave an unplayable file");
    }
}

/// Creates the hidden watcher window and pumps its messages until quit.
fn watch_thread() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage, MSG,
        WNDCLASSW, WS_OVERLAPPED,
    };

    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name: Vec<u16> = "FeatherweightSessionEnd\0".encode_utf16().collect();
        let window_name: Vec<u16> = "Featherweight session-end watcher\0"
            .encode_utf16()
            .collect();

        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            crate::log::log(
                "session-end watcher: RegisterClassW failed; shutdown or logoff during a \
                 recording may leave an unplayable file",
            );
            return;
        }

        // A plain WS_OVERLAPPED window that is never shown: hidden top-level
        // windows still receive session-end broadcasts. The parent must NOT
        // be HWND_MESSAGE — message-only windows are excluded from them.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            crate::log::log(
                "session-end watcher: CreateWindowExW failed; shutdown or logoff during a \
                 recording may leave an unplayable file",
            );
            return;
        }
        crate::log::log("session-end watcher: watching for session end (hidden window created)");

        let mut msg: MSG = std::mem::zeroed();
        loop {
            // 0 = WM_QUIT, -1 = error: either way the watcher is done.
            if GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) <= 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: *mut core::ffi::c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_DESTROY, WM_ENDSESSION,
    };

    match msg {
        // wParam != 0: the session really is ending (shutdown or logoff), or
        // the restart manager is asking the app to close (ENDSESSION_CLOSEAPP
        // in lParam — e.g. our own MSI being installed over a running app).
        // Either way the app must exit now, so finalize exactly like a quit.
        // WM_QUERYENDSESSION needs no handler: DefWindowProc answers TRUE.
        WM_ENDSESSION if wparam != 0 => {
            // A panic here must not wedge system shutdown: swallow it after
            // logging, then exit regardless.
            let finalized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(shared) = HUB.get() {
                    crate::app::session::finalize_active_session(shared);
                }
            }));
            if finalized.is_err() {
                crate::log::log("session-end watcher: finalizer panicked; exiting anyway");
            }
            crate::log::log("session-end watcher: output finalized; exiting for session end");
            std::process::exit(0);
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
