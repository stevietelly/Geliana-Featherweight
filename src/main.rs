// Windowed subsystem in release builds: this is a tray/GUI app, so the
// console window must not appear alongside it. Debug builds keep the
// console for developer diagnostics (log.rs mirrors logs to stderr).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod log;
mod tray;
mod config;
mod capture;
mod hotkeys;
mod disk;
mod filename;


fn main() -> eframe::Result {
    app::run()
}
