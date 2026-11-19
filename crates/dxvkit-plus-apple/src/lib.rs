//! # dxvkit-plus-apple — the Apple native island (macOS; iOS builds)
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
//! - [`audio::AacDecoder`] (0.11.0): AAC packets to `f32` through AudioToolbox's `AudioConverter`,
//!   offered as `Backend::audio_decoder`; the core demuxes, trims the edit list and priming.
//! - GlEnc's AVFoundation / Accelerate pieces (macOS; mirroring Swift GlEnc 1.6.2 so a Rust GlEnc
//!   writes what the Swift one writes): [`bgra::BgraReader`] (sequential 32BGRA frames,
//!   `AVAssetReaderSourceReader`), [`pcm::read_pcm_s32`] (the first audio track as interleaved
//!   s32le PCM, `SourceAudioReader.readInterleavedPCM`), [`writer::AvWriter`] (ProRes, H.264, HEVC,
//!   Motion JPEG through `AVAssetWriter`, `AVAssetWriterVideoSink`) and [`scale::scale_bgra`]
//!   (`vImageScale_ARGB8888`, `FrameResizer`).
//! - MaCrate's AVFoundation pieces (0.13.0, macOS; mirroring Swift MaCrate 0.16.4):
//!   [`track::track_facts`] (the first video track's size, rate, duration, leading empty edits and
//!   format-description tags, `AVFoundationSource.open()`), [`plane::PlaneReader`] (sequential
//!   plane 0 — BGRA, or the Y plane of `420v` / `420f` — with its stride, the measurement reader)
//!   and [`grab::FrameGrabber`] (`AVAssetImageGenerator` pictures as RGBA8, the thumbnail path).
//! - Both routes are measured by `dxvkit-plus bench --import`; the faster one (the cache route) is
//!   the default (numbers in `docs/ARCHITECTURE.md`).
//!
//! The same code builds for iOS (0.9.0: `aarch64-apple-ios` compiles and links — AVFoundation,
//! VideoToolbox, CoreVideo's `CVMetalTextureCache`, IOSurface and wgpu's Metal backend all exist
//! there; not yet run on a device). On other targets the crate builds empty: callers never see
//! objc2, CoreVideo or Metal types.

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod bgra;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod decode;
#[cfg(target_os = "macos")]
pub mod grab;
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod objc;
#[cfg(target_os = "macos")]
pub mod pcm;
#[cfg(target_os = "macos")]
pub mod plane;
#[cfg(target_os = "macos")]
pub mod scale;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod surface;
#[cfg(target_os = "macos")]
pub mod track;
#[cfg(target_os = "macos")]
pub mod writer;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use decode::{AppleBackend, NativeReader};
#[cfg(target_os = "macos")]
pub use {
    bgra::{AlphaFacts, BgraFrame, BgraReader, DecodedBuffer, FrameAttachments, SourceReaderError},
    grab::{FrameGrabber, GrabAlpha, GrabbedImage, THUMBNAIL_MAX_SIZE},
    pcm::{PcmAudio, PcmError, read_pcm_s32},
    plane::{PlaneFormat, PlaneFrame, PlaneInfo, PlaneReader},
    scale::{ScaleError, ScaleFilter, scale_bgra},
    track::{AvError, TrackFacts, track_facts},
    writer::{AvCodec, AvContainer, AvWriter, CompressionProps, H264Profile, MJPEG_DEFAULT_QUALITY, WriterError},
};
