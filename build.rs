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

    build_speexdsp();
}

/// Compiles the vendored SpeexDSP preprocessor (see `src/capture/speex.rs` and
/// `src/capture/denoise.rs`) into the binary.
///
/// Only the noise-suppression subset is built: preprocess.c plus its FFT
/// (smallft via fftwrap.c) and filterbank dependencies — no resampler, jitter
/// buffer, or standalone echo canceller (mdf.c is still linked because
/// preprocess.c references `speex_echo_get_residual`). Floating-point mode
/// (`FLOATING_POINT`) matches the pipeline's f32 samples; preprocess converts
/// the s16 API internally.
fn build_speexdsp() {
    let mut build = cc::Build::new();
    build
        .files([
            "vendor/speexdsp/libspeexdsp/preprocess.c",
            "vendor/speexdsp/libspeexdsp/fftwrap.c",
            "vendor/speexdsp/libspeexdsp/smallft.c",
            "vendor/speexdsp/libspeexdsp/filterbank.c",
            // preprocess.c references speex_echo_get_residual even when echo
            // cancellation is unused, so mdf.c must be in the link.
            "vendor/speexdsp/libspeexdsp/mdf.c",
        ])
        // Public headers (speex/speex_preprocess.h) and internal ones
        // (arch.h, fftwrap.h, filterbank.h, smallft.h, math_approx.h,
        // os_support.h) live in different roots.
        .include("vendor/speexdsp/include")
        .include("vendor/speexdsp/libspeexdsp")
        .define("FLOATING_POINT", None)
        // fftwrap.c's fallback branch is an #error; the vendored tree ships
        // smallft.c, so pin the FFT backend to it.
        .define("USE_SMALLFT", None)
        // Upstream's generated config.h normally defines this; for a static
        // build the symbols need no decoration.
        .define("EXPORT", "")
        .warnings(false)
        .cargo_metadata(true);
    // Track the vendored sources so edits rebuild.
    println!("cargo:rerun-if-changed=vendor/speexdsp/libspeexdsp/preprocess.c");
    build.compile("speexdsp");
}
