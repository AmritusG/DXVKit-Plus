//! # dxvkit-plus-apple — the macOS native island
//!
//! The only DXVKit Plus crate allowed `unsafe` (every block carries a `// SAFETY:` comment). It
//! decodes every codec that is not decoded in pure Rust — H.264, HEVC, ProRes incl. 4444 alpha,
//! MJPEG and whatever else the OS decodes — with AVFoundation / VideoToolbox into IOSurface-backed
//! `CVPixelBuffer`s, and imports those zero-copy into wgpu Metal textures.
//!
//! - [`AppleBackend`] implements `dxvkit_plus_core::backend::Backend`; its decoder
//!   ([`decode::NativeReader`]) gives frame-accurate random access over `AVAssetReader`
//!   (sequential reads reuse one reader; a seek restarts it and skips to the exact frame).
//! - [`surface::TextureCacheImporter`] (the default) maps the frame's planes through a
//!   `CVMetalTextureCache` (Apple's documented route, the technique Cutlass uses) and adopts them
//!   as wgpu textures; [`surface::import`] does the same directly from the IOSurface:
//!   `newTextureWithDescriptor:iosurface:plane:` → `wgpu_hal::metal::Device::texture_from_raw` →
//!   `wgpu::Device::create_texture_from_hal`. No pixel is copied: 4:2:0 frames arrive as an `R8`
//!   luma plane and an `RG8` chroma plane, alpha codecs as one `BGRA8` plane.
//! - Both routes are measured by `dxvkit-plus bench --import`; the faster one (the cache route) is
//!   the default (numbers in `docs/ARCHITECTURE.md`).
//!
//! On other targets the crate builds empty: callers never see objc2, CoreVideo or Metal types.

#[cfg(target_os = "macos")]
pub mod decode;
#[cfg(target_os = "macos")]
mod objc;
#[cfg(target_os = "macos")]
pub mod surface;

#[cfg(target_os = "macos")]
pub use decode::{AppleBackend, NativeReader};
