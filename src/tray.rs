/// tray.rs — icon generation, menu construction, tooltip/status formatting.
use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem},
};

// ---------------------------------------------------------------------------
// Icons (generated in code — no asset files, no `image` crate)
// ---------------------------------------------------------------------------

const ICON_SIZE: u32 = 32;

/// Generate a 32x32 RGBA icon with a filled circle of the given colour.
fn make_circle_icon(r: u8, g: u8, b: u8) -> tray_icon::Icon {
    let w = ICON_SIZE as i32;
    let cx = w / 2;
    let cy = w / 2;
    let radius = (w / 2 - 2) as i32;

    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    for py in 0..w {
        for px in 0..w {
            let dx = px - cx;
            let dy = py - cy;
            if dx * dx + dy * dy <= radius * radius {
                let idx = ((py * w + px) * 4) as usize;
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE)
        .expect("Failed to create tray icon from RGBA")
}

pub fn recording_icon() -> tray_icon::Icon {
    make_circle_icon(220, 30, 30) // red
}

pub fn stopped_icon() -> tray_icon::Icon {
    make_circle_icon(128, 128, 128) // gray
}

pub fn warn_icon() -> tray_icon::Icon {
    make_circle_icon(220, 165, 0) // amber
}

// ---------------------------------------------------------------------------
// Menu items (kept as owned IDs so app.rs can match on MenuEvent)
// ---------------------------------------------------------------------------

pub struct TrayMenu {
    pub status_item: MenuItem,
    pub stop_item: MenuItem,
    pub new_item: MenuItem,
    pub open_folder_item: MenuItem,
    pub quit_item: MenuItem,
}

impl TrayMenu {
    pub fn new(recording: bool) -> (Menu, Self) {
        let status_item = MenuItem::new("Starting…", false, None);
        let stop_item = MenuItem::new("Stop recording", recording, None);
        let new_item = MenuItem::new("New recording…", !recording, None);
        let open_folder_item = MenuItem::new("Open recordings folder", true, None);
        let quit_item = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&status_item).expect("Failed to append status item");
        menu.append(&PredefinedMenuItem::separator()).expect("Failed to append separator");
        menu.append(&stop_item).expect("Failed to append stop item");
        menu.append(&new_item).expect("Failed to append new item");
        menu.append(&open_folder_item).expect("Failed to append open folder item");
        menu.append(&PredefinedMenuItem::separator()).expect("Failed to append separator");
        menu.append(&quit_item).expect("Failed to append quit item");

        let items = TrayMenu { status_item, stop_item, new_item, open_folder_item, quit_item };
        (menu, items)
    }
}

// ---------------------------------------------------------------------------
// Tray icon builder
// ---------------------------------------------------------------------------

pub fn build_tray(icon: tray_icon::Icon, menu: Menu, tooltip: &str) -> TrayIcon {
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .build()
        .expect("Failed to build tray icon")
}

// ---------------------------------------------------------------------------
// Status line formatting
// ---------------------------------------------------------------------------

/// Format an elapsed duration as HH:MM:SS.
pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Build the recording status line shown in the menu and tooltip.
/// Truncated to 127 characters for tray tooltip compatibility.
pub fn recording_status(
    elapsed_secs: u64,
    file_size: u64,
    free_bytes: u64,
    eta_min: Option<f64>,
    disk_warn: bool,
) -> String {
    let prefix = if disk_warn { "Low disk | " } else { "" };
    let dur = format_duration(elapsed_secs);
    let size_str = crate::disk::format_bytes(file_size);
    let free_str = crate::disk::format_bytes(free_bytes);

    let eta = match eta_min {
        Some(m) if m > 0.0 => format!(" | ~{:.0} min left", m),
        _ => String::new(),
    };

    let s = format!("{prefix}Recording {dur} | {size_str} | {free_str} free{eta}");
    // Windows tooltip cap ~127 chars
    s.chars().take(127).collect()
}
