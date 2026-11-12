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
