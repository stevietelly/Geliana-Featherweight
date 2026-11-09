/// video/pool.rs — NV12 texture pool with tracked-sample lifetime management.
///
/// Pool textures are recycled via IMFTrackedSample callbacks: a slot is freed
/// only when the encoder releases its reference to the sample (the callback
/// fires), never immediately after WriteSample. The earlier immediate-release
/// approach raced with the async hardware encoder and produced corrupt frames.
///
/// [V] Verified in the spike: pool exhaustion was the cause of ~50% frame loss;
///     IMFTrackedSample fixed delivery from 14.3 fps to 22.7 fps.
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult,
};
use windows::core::{implement, Interface, Ref};

/// Pool size (constant). The spec requires testing 8/16/24/32 and reporting
/// results; 16 is the starting default. [GATE] T2.
pub const POOL_SIZE: usize = 16;

struct PoolTexture {
    texture: ID3D11Texture2D,
    in_use: bool,
}

/// NV12 texture pool. Shared between the capture thread (acquire) and the
/// MF thread pool (release via IMFTrackedSample callback).
pub struct TexturePool {
    textures: Vec<PoolTexture>,
}

impl TexturePool {
    /// Create a pool of `count` NV12 textures at the given dimensions.
    ///
    /// Textures are created with RENDER_TARGET + SHADER_RESOURCE bind flags.
    /// [V] These flags worked in the spike; add encoder-input flag only if the
    /// MFT rejects the texture.
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        count: usize,
    ) -> windows::core::Result<Self> {
        let mut textures = Vec::with_capacity(count);
        for _ in 0..count {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            textures.push(PoolTexture {
                texture: tex.unwrap(),
                in_use: false,
            });
        }
        Ok(Self { textures })
    }

    /// Acquire a free texture from the pool. Returns None if all slots are in use.
    pub fn acquire(&mut self) -> Option<ID3D11Texture2D> {
        for pt in self.textures.iter_mut() {
            if !pt.in_use {
                pt.in_use = true;
                return Some(pt.texture.clone());
            }
        }
        None
    }

    /// Release a texture slot back to the pool, identified by raw pointer.
    /// Called from the IMFTrackedSample callback when the encoder is done.
    pub fn release_by_ptr(&mut self, raw: *mut c_void) {
        for pt in self.textures.iter_mut() {
            if pt.texture.as_raw() == raw {
                pt.in_use = false;
                return;
            }
        }
    }
}

/// IMFAsyncCallback implementation that releases a pool slot when the encoder
/// is done with a tracked sample.
///
/// The raw texture pointer is stored as a usize (Send-safe integer) for
/// identity matching. The callback fires on an MF thread-pool thread.
#[implement(IMFAsyncCallback)]
struct PoolAllocator {
    pool: Arc<Mutex<TexturePool>>,
    tex_raw: usize,
}

impl IMFAsyncCallback_Impl for PoolAllocator_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        // Return E_NOTIMPL to use MF defaults (MFASYNC_CALLBACK_QUEUE_MULTITHREADED).
        Err(windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn Invoke(&self, _result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let raw = self.tex_raw as *mut c_void;
        if let Ok(mut pool) = self.pool.lock() {
            pool.release_by_ptr(raw);
        }
        Ok(())
    }
}

/// Register a tracked-sample callback so the pool slot is freed when the
/// encoder releases the sample. Called once per sample, before WriteSample.
///
/// [V] Re-arm with SetAllocator before reuse; see spike mfgpu.rs.
pub fn register_pool_callback(
    tracked: &windows::Win32::Media::MediaFoundation::IMFTrackedSample,
    pool: &Arc<Mutex<TexturePool>>,
    tex: &ID3D11Texture2D,
) -> windows::core::Result<()> {
    let tex_raw = tex.as_raw() as usize;
    let allocator: IMFAsyncCallback = PoolAllocator {
        pool: Arc::clone(pool),
        tex_raw,
    }
    .into();
    unsafe { tracked.SetAllocator(&allocator, None)? };
    Ok(())
}
