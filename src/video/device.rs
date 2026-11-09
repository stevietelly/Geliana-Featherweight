/// video/device.rs — D3D11 device access and multithread protection.
///
/// The D3D11 device comes from the first capture frame (`Frame::device()`).
/// Media Foundation uses this device from its own threads (encoder, converter),
/// so multithread protection must be enabled before any MF component touches it.
/// The DXGI device manager is what tells the sink writer which D3D device to use.
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::Interface;

/// Enable D3D11 multithread protection on the capture device.
///
/// Without this, concurrent access to the device context from MF encoder threads
/// and the capture callback can corrupt GPU state.
///
/// [V] Verified in the spike: SetMultithreadProtected(true) is required.
pub fn enable_multithread(device: &ID3D11Device) -> windows::core::Result<()> {
    let mt: ID3D11Multithread = device.cast()?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(())
}

/// Create an MF DXGI device manager and reset it with the capture device.
///
/// The returned manager is set as the `MF_SINK_WRITER_D3D_MANAGER` attribute on
/// the sink writer, giving the encoder MFT access to the D3D device for
/// hardware-accelerated encoding.
///
/// [V] Verified in the spike: MFCreateDXGIDeviceManager + ResetDevice.
pub fn create_device_manager(
    device: &ID3D11Device,
) -> windows::core::Result<(IMFDXGIDeviceManager, u32)> {
    let mut token = 0u32;
    let mut out: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut out)? };
    let manager = out.unwrap();
    unsafe { manager.ResetDevice(device, token)? };
    Ok((manager, token))
}
