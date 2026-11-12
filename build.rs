//! Build script: embeds Windows resources into the executable.
//!
//! The icon set lives in `src/assets/`; `Featherweight.ico` is the
//! multi-size shell icon (taskbar, Explorer, alt-tab). The window and tray
//! icons are set at runtime (see `app::theme` and `tray`).

fn main() {
    // The resource compiler is Windows-only; skip elsewhere.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        winresource::WindowsResource::new()
            .set_icon("src/assets/Featherweight.ico")
            .compile()
            .expect("failed to embed Windows resources");
    }
    println!("cargo:rerun-if-changed=src/assets/Featherweight.ico");
}
