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
    /// Frames the decoder serves (0.10.0: the frame count of its [`NativeDecoder::timing`]; up to
    /// 0.9.0 round(duration / minimum frame duration)).
    pub frame_count: u32,
    /// Exact frame period as (value, timescale): one frame at the timing's rate (0.10.0; up to
    /// 0.9.0 the track's minimum frame duration, its shortest sample).
    pub frame_duration: (i64, i32),
    /// Track start as (value, timescale).
    pub start: (i64, i32),
    pub nominal_fps: f64,
    pub layout: PixelLayout,
    /// The alpha the track's format description states (0.9.0): none, straight or premultiplied
    /// (`crate::source::StatedAlpha`). Backends that cannot tell say `None`.
    pub alpha: crate::source::StatedAlpha,
}

impl Default for NativeInfo {
    fn default() -> Self {
        NativeInfo {
            width: 0,
            height: 0,
            frame_count: 0,
            frame_duration: (1, 30),
            start: (0, 1),
            nominal_fps: 0.0,
            layout: PixelLayout::Bgra,
            alpha: crate::source::StatedAlpha::None,
        }
    }
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
    /// Decode frame `index` exactly: the frame [`NativeDecoder::timing`] numbers `index` (0.10.0;
    /// up to 0.9.0 the nominal grid).
    fn frame(&mut self, index: u32) -> Result<NativeFrame, MediaError>;
    /// The frame timing the decoder numbers its frames by (`crate::timing`), when it has one.
    /// 0.10.0; the default is `None`.
    fn timing(&self) -> Option<&crate::timing::FrameTiming> {
        None
    }
    /// The frame as tight straight RGBA8 through a CPU read — for `Bgra` frames, as a test oracle
    /// or for thumbnails. Playback never reads pixels on the CPU.
    fn cpu_rgba(&self, frame: &NativeFrame) -> Result<Vec<u8>, MediaError> {
        let _ = frame;
        Err(MediaError::Unsupported("this backend has no CPU read-back".into()))
    }
}

/// A platform audio packet decoder (0.11.0): compressed packets in, interleaved `f32` out. The
/// core demuxes, trims the edit list and priming, and seeks (`crate::audio::AudioReader`); the
/// decoder only turns one packet into its frames, at the track's own rate and channel count.
pub trait AudioDecoder: Send {
    /// Output frames per second.
    fn rate(&self) -> u32;
    fn channels(&self) -> u32;
    /// Frames one packet decodes to (AAC 1024).
    fn frames_per_packet(&self) -> u32;
    /// Decode one packet, appending its frames (interleaved) to `out`; returns the frame count.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<u32, MediaError>;
    /// Forget the state carried between packets (before decoding from another place).
    fn reset(&mut self) -> Result<(), MediaError>;
}

/// A platform decoder backend.
pub trait Backend: Send + Sync {
    /// A short name for logs and `probe` output (`"avfoundation"`, `"media-foundation"`).
    fn name(&self) -> &'static str;
    /// Open `path`'s first video track, delivering frames in `layout`. (`source::probe` offers a
    /// backend only the movie's first video track, or a file its own parser cannot read.)
    fn open(&self, path: &Path, layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError>;
    /// A decoder for a compressed sound track (0.11.0; macOS: AAC through AudioToolbox). The
    /// default: `Unsupported`, naming the codec.
    fn audio_decoder(&self, track: &crate::container::AudioTrack) -> Result<Box<dyn AudioDecoder>, MediaError> {
        Err(MediaError::Unsupported(format!("{} audio: the {} backend has no audio decoder", crate::names::audio_codec_label(track), self.name())))
    }
}
