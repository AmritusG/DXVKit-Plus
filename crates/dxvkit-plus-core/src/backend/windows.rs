//! Windows: Media Foundation decode + DX12 zero-copy import — **planned, not built in this
//! release**. [`MediaFoundationBackend`] exists so callers can wire the trait today; every call
//! returns `Unsupported` (never a silent no-op).
//!
//! **Design (to build behind `cfg(target_os = "windows")` in a `dxvkit-plus-windows` crate, the
//! second crate allowed `unsafe`):**
//! - Decode with `IMFSourceReader` (`MF_SOURCE_READER_D3D_MANAGER` set to a DXGI device manager,
//!   `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` off) so H.264 / HEVC / (installed) ProRes /
//!   MJPEG decode on the GPU into `ID3D11Texture2D`s (`DXGI_FORMAT_NV12`, or `B8G8R8A8` for alpha).
//! - Frame-accurate seek: `IMFSourceReader::SetCurrentPosition` to the target's 100-ns time minus
//!   half a frame, then read and discard samples until the target presentation time — the same
//!   rule as the macOS reader.
//! - Zero-copy into wgpu (DX12 backend): share the decoder texture with
//!   `IDXGIResource1::CreateSharedHandle` (NT handle), open it on the DX12 device with
//!   `ID3D12Device::OpenSharedHandle`, then `wgpu_hal::dx12::Device::texture_from_raw` +
//!   `wgpu::Device::create_texture_from_hal` — the DX12 twin of the IOSurface route. NV12 planes
//!   are viewed as `R8` / `R8G8` (plane slices 0 / 1) and converted by `dxvkit-plus-wgpu`'s WGSL
//!   pass, exactly like the macOS `Yuv420` layout.

use crate::backend::{Backend, NativeDecoder, PixelLayout};
use crate::error::MediaError;
use std::path::Path;

/// The Windows backend (planned). Every call returns `Unsupported`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaFoundationBackend;

impl Backend for MediaFoundationBackend {
    fn name(&self) -> &'static str {
        "media-foundation"
    }
    fn open(&self, path: &Path, _layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError> {
        Err(MediaError::Unsupported(format!("{}: Media Foundation decode is planned, not built in this release", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_stub_is_unsupported() {
        let e = MediaFoundationBackend.open(Path::new("x.mp4"), PixelLayout::Bgra).err();
        assert!(matches!(e, Some(MediaError::Unsupported(_))));
        assert_eq!(MediaFoundationBackend.name(), "media-foundation");
    }
}
