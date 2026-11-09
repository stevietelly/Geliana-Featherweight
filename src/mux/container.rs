/// mux/container.rs — Fragmented vs non-fragmented MP4 selection.
///
/// Fragmented MP4 writes self-contained fragments continuously, so a crash or
/// full disk still leaves a playable file. Standard MP4 stores its index at the
/// end, so a crash loses the file.
///
/// The spec defines a fallback ladder (section 5.2):
///   Route A: MFCreateSinkWriterFromURL with FMPEG4 container type
///   Route B: MFCreateFMPEG4MediaSink + MFCreateSinkWriterFromMediaSink
///   Route C: fragmented, single mixed audio track
///   Route D: non-fragmented multi-track MP4
///
/// For the initial implementation, Route A (fragmented via URL) and Route D
/// (non-fragmented) are supported. Route B can be added if Route A fails T9.

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, MF_TRANSCODE_CONTAINERTYPE, MF_MPEG4SINK_MIN_FRAGMENT_DURATION,
    MFTranscodeContainerType_FMPEG4,
};

/// Container format for the output file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMode {
    /// Fragmented MP4 (fMP4): crash-survivable, self-contained fragments.
    /// Route A: MFCreateSinkWriterFromURL with FMPEG4 container type.
    Fragmented,
    /// Standard (non-fragmented) MP4: index at end, requires finalize to play.
    /// Used as fallback if fragmented mode fails T9.
    NonFragmented,
}

impl Default for ContainerMode {
    fn default() -> Self {
        ContainerMode::Fragmented
    }
}

impl ContainerMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerMode::Fragmented => "fMP4",
            ContainerMode::NonFragmented => "MP4",
        }
    }
}

/// Apply container-specific attributes to the sink writer's attribute store.
///
/// For Fragmented mode, sets the transcode container type to FMPEG4 and a
/// minimum fragment duration of ~2 seconds (matching the GOP interval).
/// NonFragmented mode adds no container attributes (standard MP4 is the default).
pub fn apply_container_attributes(
    attrs: &IMFAttributes,
    mode: ContainerMode,
) -> windows::core::Result<()> {
    if mode == ContainerMode::Fragmented {
        unsafe {
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_FMPEG4)?;
            // Fragment duration: 2 seconds in 100ns units = 2 × 10,000,000.
            // Matches the ~2s GOP interval (section 3.5). [U] if present.
            let _ = attrs.SetUINT64(&MF_MPEG4SINK_MIN_FRAGMENT_DURATION, 20_000_000);
        }
    }
    Ok(())
}
