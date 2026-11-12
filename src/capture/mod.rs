//! Screen and audio capture pipeline.
//!
//! All capture concerns live here, decoupled from the UI layer. Sub-modules:
//!
//! - [`monitors`]: enumerate available display monitors ([`MonitorInfo`],
//!   [`enumerate_monitors`]).
//! - [`windows`]: enumerate capturable application windows ([`WindowInfo`],
//!   [`enumerate_windows`]).
//! - [`audio`]: the WASAPI [`AudioPipeline`] used during recordings (system
//!   loopback + microphone capture, mixing, and encoder sinks).
//! - [`speex`]: minimal owned FFI bindings to the vendored SpeexDSP
//!   preprocessor (compiled by `build.rs` from `vendor/speexdsp`).
//! - [`denoise`]: the microphone noise-suppression engine selector
//!   ([`denoise::MicDenoiser`]) combining SpeexDSP and RNNoise (nnnoiseless).
//! - [`preview`]: idle [`AudioPreview`] streams that only measure live
//!   signal peaks, feeding the settings-window volume meters.
//! - [`pipeline`]: the real capture engine — Windows Graphics Capture with a
//!   GDI compatibility fallback, the encoder, and the multi-track muxer.
//! - [`recorder`]: top-level recording orchestration ([`RecorderManager`],
//!   [`RecorderEvent`], [`SessionStatus`]).

pub mod monitors;
pub mod audio;
pub mod denoise;
pub mod pipeline;
pub mod preview;
pub mod recorder;
pub(crate) mod speex;
pub mod windows;
