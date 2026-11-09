/// video/ — GPU video pipeline: BGRA capture texture → NV12 → H.264 via IMFSinkWriter.
///
/// Ported from the spike (`spike/src/mfgpu.rs`) into the module structure
/// defined in `Docs/production-spec.md` section 2.2.
///
/// Dependency boundary: this module must NOT import tao, tray-icon, or global-hotkey.
pub mod device;
pub mod convert;
pub mod pool;
pub mod encode;
