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
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
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
/// - Not DWM-cloaked (see `passes_filters` — this is what keeps Windows'
///   pre-launched "Settings" and "Windows Input Experience" hosts out of the
///   list; cloak does not clear `WS_VISIBLE`, so `IsWindowVisible` alone
///   passes them).
/// - Title must be non-empty.
/// - `WS_EX_TOOLWINDOW` style absent (excludes palettes, overlays, etc.).
/// - `WS_EX_NOACTIVATE` style absent (excludes always-on-top HUDs etc.).
/// - Has a valid process ID (excludes ghost/orphaned windows).
/// - Not minimized.
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

/// The filter decision for one top-level window, from the raw Win32 facts
/// gathered for it. Split out of [`enum_callback`] so the rules can be
/// unit-tested without real windows existing on the system.
fn passes_filters(visible: bool, iconic: bool, pid: u32, ex_style: u32, cloaked: bool, title: &str) -> bool {
    // Must be shown and on-screen; cloaked windows are hidden by the DWM even
    // though their WS_VISIBLE bit is set (Windows' pre-launched Settings and
    // "Windows Input Experience" hosts live like this permanently).
    if !visible || iconic || cloaked {
        return false;
    }

    // Skip windows without a process (orphaned / ghost handles).
    if pid == 0 {
        return false;
    }

    // Skip tool windows and no-activate overlays.
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 || ex_style & WS_EX_NOACTIVATE.0 != 0 {
        return false;
    }

    // Skip windows without a title to show in the picker.
    !title.is_empty()
}

/// Win32 `EnumWindows` callback. Returns `TRUE` to continue enumeration.
unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam is a valid *const Mutex<Vec<WindowInfo>> for the
    // duration of the EnumWindows call (see the caller).
    let collector = unsafe { &*(lparam.0 as *const Mutex<Vec<WindowInfo>>) };

    let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
    let iconic = unsafe { IsIconic(hwnd).as_bool() };

    // DWM cloak state: nonzero means the Desktop Window Manager hides this
    // window from the screen (background hosts, pre-launched UWP apps) even
    // though it is "visible" in USER32 terms. Fail-open on a query error:
    // including a cloaked window is far less bad than dropping a real one.
    let mut cloak: u32 = 0;
    let cloaked = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloak as *mut u32 as *mut core::ffi::c_void,
            core::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloak != 0;

    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };

    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;

    // Read the title.
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    let title = if title_len > 0 {
        let mut buf = vec![0u16; (title_len as usize) + 1];
        let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if written > 0 {
            String::from_utf16_lossy(&buf[..written as usize])
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    if !passes_filters(visible, iconic, pid, ex_style, cloaked, &title) {
        return TRUE;
    }

    if let Ok(mut guard) = collector.lock() {
        guard.push(WindowInfo {
            id: hwnd.0 as u64,
            title,
        });
    }

    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_window_passes() {
        assert!(passes_filters(true, false, 4242, 0, false, "Notepad"));
    }

    #[test]
    fn invisible_or_minimized_or_orphan_fail() {
        assert!(!passes_filters(false, false, 4242, 0, false, "Notepad"));
        assert!(!passes_filters(true, true, 4242, 0, false, "Notepad"));
        assert!(!passes_filters(true, false, 0, 0, false, "Notepad"));
    }

    #[test]
    fn tool_and_noactivate_styles_fail() {
        assert!(!passes_filters(
            true,
            false,
            4242,
            WS_EX_TOOLWINDOW.0,
            false,
            "Palette"
        ));
        assert!(!passes_filters(
            true,
            false,
            4242,
            WS_EX_NOACTIVATE.0,
            false,
            "HUD"
        ));
    }

    #[test]
    fn untitled_window_fails() {
        assert!(!passes_filters(true, false, 4242, 0, false, ""));
    }

    /// The ghost-entry case: Windows' pre-launched Settings and "Windows
    /// Input Experience" hosts are top-level, titled, tool-style-free and
    /// `WS_VISIBLE` — the only thing wrong with them is that the DWM has
    /// cloaked them. They must be excluded.
    #[test]
    fn cloaked_window_fails_even_when_otherwise_visible() {
        assert!(!passes_filters(true, false, 4242, 0, true, "Settings"));
        assert!(!passes_filters(
            true,
            false,
            4242,
            0,
            true,
            "Windows Input Experience"
        ));
    }

    /// Fail-open contract: a failed DWM query reports "not cloaked", so a
    /// real window is never dropped because the query errored.
    #[test]
    fn cloak_query_failure_fails_open() {
        assert!(passes_filters(true, false, 4242, 0, false, "Notepad"));
    }
}
