/// video/convert.rs — BGRA → NV12 conversion via ID3D11VideoProcessor.
///
/// Creates a video processor once, caches input/output views per texture, and
/// runs VideoProcessorBlt on the capture device's immediate context.
///
/// [V] Verified in the spike: Blt ~0.5 ms per frame, color space 0x00 input / 0x14 output.
use std::collections::HashMap;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;
use windows::core::{Interface, BOOL};

/// Video processor wrapper that converts BGRA capture textures to NV12.
///
/// Create once per recording. Input/output views are cached per texture to
/// avoid per-frame allocation overhead.
// SAFETY: all COM pointers are used only on the capture thread (MTA apartment).
unsafe impl Send for VideoProcessor {}

pub struct VideoProcessor {
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// Cached input views keyed by source texture raw pointer.
    /// Safety: raw pointers are identity keys only, never dereferenced.
    input_view_cache: HashMap<usize, ID3D11VideoProcessorInputView>,
    /// Cached output views keyed by pool texture raw pointer.
    output_view_cache: HashMap<usize, ID3D11VideoProcessorOutputView>,
}

impl VideoProcessor {
    /// Create a video processor for the given dimensions.
    ///
    /// Width and height must be even (NV12 requires 2x2 chroma subsampling).
    /// Odd dimensions should be rounded down before calling this.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> windows::core::Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        // Color space bitfield layout per D3D11_VIDEO_PROCESSOR_COLOR_SPACE docs:
        //   bit 0:      Usage          (0=playback, 1=video processing)
        //   bit 1:      RGB_Range      (0=full 0-255, 1=limited 16-235)
        //   bit 2:      YCbCr_Matrix   (0=BT.601, 1=BT.709)
        //   bit 3:      YCbCr_xvYCC   (0=conventional, 1=xvYCC extended)
        //   bits 4-5:   Nominal_Range  (0=unknown, 1=0-255, 2=16-235, 3=48-208)
        //
        // Input: full-range RGB (BGRA from GFX capture) → all bits 0
        // Output: studio-range (16-235) BT.709 → 0x14
        //   bit 2 (YCbCr_Matrix=BT.709) = 1
        //   bits 4-5 (Nominal_Range=16-235) = 2 → 0b10
        //   → 0b_0010_0100 = 0x14
        // [V] Wrong value gives green tint or crushed contrast.
        let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x00 };
        let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 };

        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_cs);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_cs);
        }

        Ok(Self {
            processor,
            enumerator,
            video_device,
            video_context,
            input_view_cache: HashMap::new(),
            output_view_cache: HashMap::new(),
        })
    }

    /// Get or create the output view for a pool texture (cached).
    fn output_view(
        &mut self,
        tex: &ID3D11Texture2D,
    ) -> windows::core::Result<&ID3D11VideoProcessorOutputView> {
        let key = tex.as_raw() as usize;
        if !self.output_view_cache.contains_key(&key) {
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view: Option<ID3D11VideoProcessorOutputView> = None;
            unsafe {
                self.video_device.CreateVideoProcessorOutputView(
                    tex,
                    &self.enumerator,
                    &desc,
                    Some(&mut view),
                )?
            };
            self.output_view_cache.insert(key, view.unwrap());
        }
        Ok(self.output_view_cache.get(&key).unwrap())
    }

    /// Get or create the input view for a source texture (cached).
    fn input_view(
        &mut self,
        tex: &ID3D11Texture2D,
    ) -> windows::core::Result<&ID3D11VideoProcessorInputView> {
        let key = tex.as_raw() as usize;
        if !self.input_view_cache.contains_key(&key) {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view: Option<ID3D11VideoProcessorInputView> = None;
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    tex,
                    &self.enumerator,
                    &desc,
                    Some(&mut view),
                )?
            };
            self.input_view_cache.insert(key, view.unwrap());
        }
        Ok(self.input_view_cache.get(&key).unwrap())
    }

    /// Convert a BGRA source texture to NV12 in the destination pool texture.
    ///
    /// This only queues GPU work on the immediate context; it does not wait for
    /// the GPU to finish. The caller should not Flush unless measurement shows
    /// a benefit.
    pub fn blt(
        &mut self,
        src: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
    ) -> windows::core::Result<()> {
        let out_view = self.output_view(dst)?.clone();
        let in_view = self.input_view(src)?.clone();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL(1),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &out_view, 0, &[stream])?
        };
        Ok(())
    }

    /// Log the input view cache size (diagnostic, called periodically).
    pub fn input_view_cache_size(&self) -> usize {
        self.input_view_cache.len()
    }
}
