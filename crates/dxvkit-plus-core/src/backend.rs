//! Platform decoder backends: every codec that is not decoded in pure Rust here (H.264, HEVC,
//! ProRes incl. 4444 alpha, MJPEG and whatever else the OS decodes) goes through the operating
//! system's own decoder, behind the [`Backend`] trait.
//!
//! | Platform | Crate | Status |
//! |---|---|---|
//! | macOS | `dxvkit-plus-apple` (AVFoundation / VideoToolbox, IOSurface, zero-copy into wgpu Metal) | implemented |
//! | Windows | Media Foundation + DX12 shared-handle import ([`windows`]) | planned; the stub returns `Unsupported` |
//!
//! A backend hands out [`NativeFrame`]s: the frame's size, pixel layout and YCbCr matrix plus an
//! opaque, platform-owned handle (on macOS a retained `CVPixelBuffer`). The pure-Rust core never
//! touches the pixels; the platform crate imports the handle into a GPU texture without copying.

use crate::error::MediaError;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

pub mod windows;

/// The pixel layout a native frame is delivered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelLayout {
    /// One `BGRA8` plane (alpha codecs such as ProRes 4444; also the safe fallback).
    Bgra,
    /// 8-bit 4:2:0, two planes: `R8` luma and `RG8` chroma (NV12).
    Yuv420 { full_range: bool },
}

/// The YCbCr matrix a frame was encoded with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum YuvMatrix {
    Bt601 = 0,
    Bt709 = 1,
    Bt2020 = 2,
}

impl YuvMatrix {
    /// The index the WGSL conversion uses (0 = BT.601, 1 = BT.709, 2 = BT.2020).
    pub fn index(self) -> u32 {
        match self {
            YuvMatrix::Bt601 => 0,
            YuvMatrix::Bt709 => 1,
            YuvMatrix::Bt2020 => 2,
        }
    }
}

/// What the platform says about a clip.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeInfo {
    pub width: u32,
    pub height: u32,
    /// Frames on the nominal grid: round(duration / frame_duration).
    pub frame_count: u32,
    /// Exact frame duration as (value, timescale).
    pub frame_duration: (i64, i32),
    /// Track start as (value, timescale).
    pub start: (i64, i32),
    pub nominal_fps: f64,
    pub layout: PixelLayout,
}

/// One decoded platform frame.
#[derive(Clone)]
pub struct NativeFrame {
    /// The frame index on the nominal grid.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub matrix: YuvMatrix,
    /// The platform surface id (macOS: the IOSurface's global id) — the zero-copy evidence: the GPU
    /// texture aliases this surface.
    pub surface_id: u32,
    /// The platform-owned frame (macOS: a retained `CVPixelBuffer`). Only the backend's own crate
    /// knows the concrete type; it downcasts the handle to import the frame into a GPU texture.
    pub handle: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for NativeFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrame")
            .field("index", &self.index)
            .field("size", &(self.width, self.height))
            .field("layout", &self.layout)
            .field("matrix", &self.matrix)
            .field("surface_id", &self.surface_id)
            .finish()
    }
}

/// A frame-accurate decoder for one clip. Used from one thread at a time (`&mut self`).
pub trait NativeDecoder: Send {
    fn info(&self) -> &NativeInfo;
    /// Decode frame `index` exactly (on the nominal grid).
    fn frame(&mut self, index: u32) -> Result<NativeFrame, MediaError>;
    /// The frame as tight straight RGBA8 through a CPU read — for `Bgra` frames, as a test oracle
    /// or for thumbnails. Playback never reads pixels on the CPU.
    fn cpu_rgba(&self, frame: &NativeFrame) -> Result<Vec<u8>, MediaError> {
        let _ = frame;
        Err(MediaError::Unsupported("this backend has no CPU read-back".into()))
    }
}

/// A platform decoder backend.
pub trait Backend: Send + Sync {
    /// A short name for logs and `probe` output (`"avfoundation"`, `"media-foundation"`).
    fn name(&self) -> &'static str;
    /// Open `path`'s first video track, delivering frames in `layout`.
    fn open(&self, path: &Path, layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError>;
}
