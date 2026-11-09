/// video/encode.rs — Sink-writer video stream setup and sample creation.
///
/// Media type constructors for the IMFSinkWriter video stream, plus the
/// tracked-sample creation path that ties pool textures to the encoder's
/// async lifetime.
///
/// [V] Verified in the spike unless otherwise noted.
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, IMFTrackedSample, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateTrackedSample, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MFAudioFormat_AAC, MFAudioFormat_PCM, MFMediaType_Audio, MFMediaType_Video,
    MFNominalRange_16_235, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
    // Profile/level for H.264 negotiation
    MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE, eAVEncH264VLevel, eAVEncH264VProfile,
    // Audio type attributes
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_CHANNEL_MASK, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    // IMF2DBuffer for sample length fix
    IMF2DBuffer,
};
use windows::core::Interface;

use super::pool::{TexturePool, register_pool_callback};

// ---------------------------------------------------------------------------
// Presets (section 3.8)
// ---------------------------------------------------------------------------

/// Preset table. v1 has no preset UI; the active preset is a constant chosen
/// at build time (section 8: settings.rs). P0 (Balanced) is the default.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    /// 0 = native resolution; otherwise scale down to fit within this box.
    pub max_width: u32,
    pub max_height: u32,
    /// Target bitrate at 1080p; scaled by pixel count for other resolutions.
    pub bitrate_1080p: u32,
    pub fps_cap: u32,
}

pub const PRESET_BALANCED: Preset = Preset {
    name: "Balanced",
    max_width: 0,
    max_height: 0,
    bitrate_1080p: 6_000_000,
    fps_cap: 30,
};

pub const PRESET_LOW_IMPACT: Preset = Preset {
    name: "Low impact",
    max_width: 1280,
    max_height: 720,
    bitrate_1080p: 3_000_000,
    fps_cap: 30,
};

/// Scale bitrate by pixel count relative to 1080p.
/// Clamped to [1, 40] Mbps.
///
/// Note: this is a *request*. Hardware MFTs may ignore it; see ICodecAPI in
/// mux/mod.rs and [GATE] T4.
pub fn scale_bitrate(width: u32, height: u32) -> u32 {
    scale_bitrate_preset(width, height, PRESET_BALANCED.bitrate_1080p)
}

pub fn scale_bitrate_preset(width: u32, height: u32, bitrate_1080p: u32) -> u32 {
    let pixels = width as u64 * height as u64;
    let base = 1920u64 * 1080;
    ((bitrate_1080p as u64 * pixels / base).clamp(1_000_000, 40_000_000)) as u32
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pack_ratio(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

/// Ensure width and height are even (NV12 requires 2x2 chroma subsampling).
/// Rounds down.
pub fn round_even(n: u32) -> u32 {
    n & !1
}

// ---------------------------------------------------------------------------
// Video media types
// ---------------------------------------------------------------------------

/// Create the H.264 output media type with a profile and level hint.
///
/// The spec requires requesting High profile first, falling back to Main then
/// Baseline. The level hint for 1080p30 is 4.1.
///
/// [U] Profile/level negotiation: Intel Quick Sync negotiated Constrained
/// Baseline when no profile was set [V]. Requesting High is expected to work
/// but has not been tested.
pub fn make_video_output_type(
    width: u32,
    height: u32,
    bitrate: u32,
    profile: eAVEncH264VProfile,
    level: eAVEncH264VLevel,
) -> windows::core::Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(30, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32)?;
        t.SetUINT32(&MF_MT_MPEG2_LEVEL, level.0 as u32)?;
    }
    Ok(t)
}

/// Create the NV12 input media type with BT.709 studio-range color attributes.
///
/// [V] Color attributes match the VideoProcessor output (0x14 bitfield).
pub fn make_video_input_type(width: u32, height: u32) -> windows::core::Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_ratio(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(30, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Audio media types (used by mux/mod.rs during init)
// ---------------------------------------------------------------------------

/// Create the AAC output media type.
///
/// `bytes_per_second` values: 12000 (96 kbps mono), 16000 (128 kbps stereo),
/// 20000 (160 kbps), 24000 (192 kbps stereo). If the encoder rejects a value,
/// use the nearest accepted value and log it.
///
/// [U] The spike measured 163-166 kb/s with the spike's own settings; these
/// requested values have not been verified.
pub fn make_audio_output_type(
    channels: u32,
    bytes_per_second: u32,
) -> windows::core::Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)?;
        t.SetUINT32(&MF_MT_AUDIO_CHANNEL_MASK, if channels == 2 { 0x3 } else { 0x4 })?;
        // FL + FR for stereo; FC for mono
    }
    Ok(t)
}

/// Create the PCM input media type for audio (48 kHz, 16-bit).
pub fn make_audio_input_type(channels: u32) -> windows::core::Result<IMFMediaType> {
    let t = unsafe { MFCreateMediaType()? };
    let block_align = channels * 2; // 16-bit = 2 bytes per sample
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 48_000 * block_align)?;
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Sample creation
// ---------------------------------------------------------------------------

/// Create an IMFSample wrapping a D3D11 NV12 texture, with tracked-sample
/// pool release.
///
/// Steps:
/// 1. MFCreateDXGISurfaceBuffer over the NV12 texture.
/// 2. Fix CurrentLength via IMF2DBuffer::GetContiguousLength — without this,
///    the H.264 MFT rejects the sample as zero length [V].
/// 3. MFCreateTrackedSample → QI to IMFSample, add buffer, set time/duration.
/// 4. If a pool is provided, register the PoolAllocator callback so the slot
///    is freed when the encoder releases the sample [V].
pub fn make_video_sample(
    tex: &ID3D11Texture2D,
    time: i64,
    dur: i64,
    pool: Option<&Arc<Mutex<TexturePool>>>,
) -> windows::core::Result<IMFSample> {
    let buffer =
        unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)? };

    // [V] MFCreateDXGISurfaceBuffer leaves CurrentLength = 0; fix it.
    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
        if let Ok(len) = unsafe { buf2d.GetContiguousLength() } {
            unsafe { buffer.SetCurrentLength(len)? };
        }
    }

    // Use IMFTrackedSample so the pool slot is freed when the encoder releases
    // the sample, not immediately after WriteSample [V].
    let tracked: IMFTrackedSample = unsafe { MFCreateTrackedSample()? };
    let sample: IMFSample = tracked.cast()?;
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(dur)?;
    }

    // Register the pool callback only when we have a pool slot to release.
    if let Some(pool_arc) = pool {
        register_pool_callback(&tracked, pool_arc, tex)?;
    }

    Ok(sample)
}
