//! Enumeration of capturable application windows.
//!
//! [`enumerate_windows`] returns every top-level, visible, non-tool window
//! that has a non-empty title — the same set Windows Graphics Capture exposes
//! in its picker. Each entry carries the HWND (as `u64`) and the window title.
//!
//! The enumeration runs synchronously on the caller's thread and is fast
//! enough for a settings-panel refresh (no I/O, pure Win32 callbacks).

use std::sync::Mutex;

use windows::Win32::{
    Foundation::{HWND, LPARAM, TRUE},
    UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        GetWindowLongPtrW, GWL_EXSTYLE,
    },
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A capturable application window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Win32 HWND as `u64` (safe to store in config, compare, etc.).
    pub id: u64,
    /// Window title at the time of enumeration.
    pub title: String,
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// Returns all visible, titled, non-tool top-level windows in z-order.
///
/// Filters applied:
/// - `IsWindowVisible` must be true.
/// - Title length must be > 0.
/// - `WS_EX_TOOLWINDOW` style absent (excludes palettes, overlays, etc.).
/// - `WS_EX_NOACTIVATE` style absent (excludes always-on-top HUDs etc.).
/// - Has a valid process ID (excludes ghost/orphaned windows).
///
/// Any per-window error is silently skipped so a single bad HWND doesn't
/// abort the whole list.
pub fn enumerate_windows() -> Vec<WindowInfo> {
    let collector: Mutex<Vec<WindowInfo>> = Mutex::new(Vec::new());
    let lp = &collector as *const _ as isize;

    // SAFETY: `collector` outlives the EnumWindows call; the callback only
    // writes through the pointer while EnumWindows holds the thread.
    unsafe {
        let _ = EnumWindows(Some(enum_callback), LPARAM(lp));
    }

    let mut list = collector.into_inner().unwrap_or_default();

    // Sort alphabetically so the ComboBox is easy to scan.
    list.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    list
}

/// Win32 `EnumWindows` callback. Returns `TRUE` to continue enumeration.
unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam is a valid *const Mutex<Vec<WindowInfo>> for the
    // duration of the EnumWindows call (see the caller).
    let collector = unsafe { &*(lparam.0 as *const Mutex<Vec<WindowInfo>>) };

    // Skip invisible windows.
    if unsafe { !IsWindowVisible(hwnd).as_bool() } {
        return TRUE;
    }

    // Skip minimized windows (optional — remove if you want them).
    if unsafe { IsIconic(hwnd).as_bool() } {
        return TRUE;
    }

    // Skip windows without a process (orphaned / ghost handles).
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return TRUE;
    }

    // Extended style check: skip tool windows and no-activate overlays.
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 || ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return TRUE;
    }

    // Read the title.
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len <= 0 {
        return TRUE;
    }

    let mut buf = vec![0u16; (title_len as usize) + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if written <= 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buf[..written as usize]);

    if let Ok(mut guard) = collector.lock() {
        guard.push(WindowInfo {
            id: hwnd.0 as u64,
            title,
        });
    }

    TRUE
}
