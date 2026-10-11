//! Media sources: probe a path, then decode frame `n` into something a GPU uploads directly.
//!
//! | Path | Decoder | Decoded as |
//! |---|---|---|
//! | `.mov` with a `Hap*` track | [`crate::hap`] (Vidvox spec) | BC1 / BC3 / BC7 / BC4 / BC6H block planes |
//! | `.lmbd` (LMB7) | [`crate::lmbd`] (`docs/LMBD.md`) | one BC7 block plane |
//! | any other movie the platform reads | a [`Backend`] (macOS: `dxvkit-plus-apple`) | a platform frame, imported zero-copy |
//! | a folder of numbered PNGs | image sequence, frame rate chosen on import | RGBA8 |
//! | one PNG | a still | RGBA8 |
//! | `.mov` with a `DXD3`/`DXD4`/`DXDI` track | probed; decoding is on the roadmap | — |
//!
//! [`Decoded::to_rgba`] turns any CPU-decoded frame (blocks or RGBA8) into straight RGBA8 on the
//! CPU when asked — the oracle for the GPU path, and the route for tools without a GPU.

use crate::backend::{Backend, NativeDecoder, NativeFrame, PixelLayout};
use crate::container::{Codec, Movie, VideoTrack};
use crate::error::MediaError;
use crate::hap;
use crate::time::FrameRate;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Kind of a probed source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum SourceKind {
    Hap = 0,
    Native = 1,
    ImageSequence = 2,
    Still = 3,
    /// A DXV movie: probed (size, frames, rate, timecode) but not decodable in this release.
    Dxv = 4,
    /// An LMBD file (profile LMB7): BC7 blocks.
    Lmbd = 5,
}

/// What a probe learned about a path.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MediaInfo {
    pub path: String,
    pub kind: SourceKind,
    /// Sample-entry FourCC, or the image extension.
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    #[serde(serialize_with = "ser_rate")]
    pub rate: FrameRate,
    /// Whether the source can carry alpha (HAP Alpha / Q Alpha / Alpha-only / R, ProRes 4444 with
    /// depth 32, RGBA images).
    pub has_alpha: bool,
    /// The embedded timecode track's start (QuickTime `tmcd`), if any.
    pub timecode: Option<crate::container::TimecodeStart>,
}

fn ser_rate<S: serde::Serializer>(r: &FrameRate, s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    let mut st = s.serialize_struct("rate", 2)?;
    st.serialize_field("num", &r.num)?;
    st.serialize_field("den", &r.den)?;
    st.end()
}

/// Import options: the image-sequence frame rate (chosen on import) and the platform backend for
/// native codecs (none = native codecs are `Unsupported`).
#[derive(Clone)]
pub struct ImportOptions {
    pub sequence_rate: FrameRate,
    pub backend: Option<Arc<dyn Backend>>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions { sequence_rate: FrameRate::FPS_30, backend: None }
    }
}

impl std::fmt::Debug for ImportOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportOptions").field("sequence_rate", &self.sequence_rate).field("backend", &self.backend.as_ref().map(|b| b.name())).finish()
    }
}

/// Image extensions a sequence / still may use (only PNG decodes; the rest report `Unsupported`).
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "tif", "tiff", "exr", "gif", "bmp", "webp", "heic"];
/// Most frames an image sequence may have.
pub const MAX_SEQUENCE: usize = 1 << 20;

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default()
}

/// The numbered images of a folder, in numeric order of the LAST digit run of each name
/// (`shot_0009.png` < `shot_0010.png`); files without digits sort after, by name.
pub fn sequence_files(dir: &Path) -> Result<Vec<PathBuf>, MediaError> {
    let rd = std::fs::read_dir(dir).map_err(|e| MediaError::Io(format!("{}: {e}", dir.display())))?;
    let mut files: Vec<(u64, String, PathBuf)> = Vec::new();
    for entry in rd {
        let p = entry.map_err(|e| MediaError::Io(e.to_string()))?.path();
        if !p.is_file() || !IMAGE_EXTS.contains(&ext_of(&p).as_str()) {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let digits: String =
            stem.chars().rev().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
        let n = digits.parse::<u64>().unwrap_or(u64::MAX);
        files.push((n, stem, p));
        if files.len() > MAX_SEQUENCE {
            return Err(MediaError::Invalid(format!("{}: more than {MAX_SEQUENCE} images", dir.display())));
        }
    }
    files.sort();
    Ok(files.into_iter().map(|(_, _, p)| p).collect())
}

/// Decode one image file to straight RGBA8.
pub fn load_image(path: &Path) -> Result<(u32, u32, Vec<u8>), MediaError> {
    let ext = ext_of(path);
    if ext != "png" {
        return Err(MediaError::Unsupported(format!("{}: .{ext} images (only PNG decodes in this build)", path.display())));
    }
    let file = std::fs::File::open(path).map_err(|e| MediaError::Io(format!("{}: {e}", path.display())))?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| MediaError::Image(format!("{}: {e}", path.display())))?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || w > crate::MAX_DIMENSION || h > crate::MAX_DIMENSION {
        return Err(MediaError::Image(format!("{}: size {w}x{h} out of range", path.display())));
    }
    let size = reader.output_buffer_size().ok_or_else(|| MediaError::Image("image too large".into()))?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| MediaError::Image(format!("{}: {e}", path.display())))?;
    let px = (w as usize).saturating_mul(h as usize);
    let src = buf.get(..info.buffer_size()).unwrap_or_default();
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => src.chunks_exact(3).filter_map(|p| <[u8; 3]>::try_from(p).ok()).flat_map(|[r, g, b]| [r, g, b, 255]).collect(),
        png::ColorType::GrayscaleAlpha => src.chunks_exact(2).filter_map(|p| <[u8; 2]>::try_from(p).ok()).flat_map(|[v, a]| [v, v, v, a]).collect(),
        png::ColorType::Grayscale => src.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => return Err(MediaError::Image("indexed PNG not expanded".into())),
    };
    if rgba.len() != px.saturating_mul(4) {
        return Err(MediaError::Image(format!("{}: decoded {} bytes for {w}x{h}", path.display(), rgba.len())));
    }
    Ok((w, h, rgba))
}

fn png_size(path: &Path) -> Result<(u32, u32), MediaError> {
    let file = std::fs::File::open(path).map_err(|e| MediaError::Io(format!("{}: {e}", path.display())))?;
    let reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().map_err(|e| MediaError::Image(format!("{}: {e}", path.display())))?;
    Ok((reader.info().width, reader.info().height))
}

/// Probe `path` without decoding pixels (movies: the container + the backend; images: headers).
pub fn probe(path: &str, opts: &ImportOptions) -> Result<MediaInfo, MediaError> {
    let p = Path::new(path);
    if p.is_dir() {
        let files = sequence_files(p)?;
        let first = files.first().ok_or_else(|| MediaError::Invalid(format!("{path}: no images in folder")))?;
        if ext_of(first) != "png" {
            return Err(MediaError::Unsupported(format!("{path}: .{} sequences (only PNG decodes in this build)", ext_of(first))));
        }
        let (width, height) = png_size(first)?;
        let frames = u32::try_from(files.len()).unwrap_or(u32::MAX);
        return Ok(MediaInfo {
            path: path.into(),
            kind: SourceKind::ImageSequence,
            codec: "png".into(),
            width,
            height,
            frames,
            rate: opts.sequence_rate,
            has_alpha: true,
            timecode: None,
        });
    }
    if !p.exists() {
        return Err(MediaError::Io(format!("{path}: no such file")));
    }
    let ext = ext_of(p);
    if ext == "lmbd" || starts_with_lmbd_magic(p) {
        let f = crate::lmbd::LmbdFile::open(p)?;
        let h = f.header();
        return Ok(MediaInfo {
            path: path.into(),
            kind: SourceKind::Lmbd,
            codec: "LMB7".into(),
            width: h.width,
            height: h.height,
            frames: h.frame_count,
            rate: h.rate(),
            has_alpha: true,
            timecode: None,
        });
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        if ext != "png" {
            return Err(MediaError::Unsupported(format!("{path}: .{ext} images (only PNG decodes in this build)")));
        }
        let (width, height) = png_size(p)?;
        return Ok(MediaInfo {
            path: path.into(),
            kind: SourceKind::Still,
            codec: "png".into(),
            width,
            height,
            frames: 1,
            rate: opts.sequence_rate,
            has_alpha: true,
            timecode: None,
        });
    }
    match Movie::open(p) {
        Ok(movie) => {
            let track = movie.track(Codec::Hap).or_else(|| movie.track(Codec::Dxv)).or_else(|| movie.first()).ok_or(MediaError::NoVideoTrack)?;
            let kind = match track.codec {
                Codec::Dxv => SourceKind::Dxv,
                Codec::Hap => SourceKind::Hap,
                Codec::Native => SourceKind::Native,
            };
            let codec = String::from_utf8_lossy(&track.fourcc).into_owned();
            let rate = track.frame_rate().unwrap_or(FrameRate::FPS_30);
            let has_alpha = match track.codec {
                Codec::Hap => hap_alpha(&track.fourcc),
                Codec::Dxv => true, // decided per packet
                Codec::Native => native_alpha(track),
            };
            let mut info = MediaInfo {
                path: path.into(),
                kind,
                codec,
                width: track.width,
                height: track.height,
                frames: track.frame_count(),
                rate,
                has_alpha,
                timecode: movie.timecode,
            };
            if kind == SourceKind::Native {
                native_probe(&mut info, track, opts)?;
            }
            Ok(info)
        }
        Err(MediaError::NotMovie | MediaError::NoVideoTrack) => {
            // Not a QuickTime / MP4 we parse: the platform may still read it.
            let mut info = MediaInfo {
                path: path.into(),
                kind: SourceKind::Native,
                codec: ext,
                width: 0,
                height: 0,
                frames: 0,
                rate: FrameRate::FPS_30,
                has_alpha: false,
                timecode: None,
            };
            let backend = opts.backend.as_ref().ok_or_else(|| no_backend(path))?;
            let dec = backend.open(p, PixelLayout::Bgra)?;
            let n = dec.info();
            info.width = n.width;
            info.height = n.height;
            info.frames = n.frame_count;
            info.rate = FrameRate::new(i64::from(n.frame_duration.1), n.frame_duration.0);
            Ok(info)
        }
        Err(e) => Err(e),
    }
}

/// Whether the file begins with the LMBD magic (so a renamed `.lmbd` still opens as one).
fn starts_with_lmbd_magic(p: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 4];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut b)).is_ok() && crate::lmbd::is_lmbd(&b)
}

/// Whether a HAP FourCC can carry alpha (HAP Alpha, HAP Q Alpha, HAP Alpha-Only, HAP R).
fn hap_alpha(fourcc: &[u8; 4]) -> bool {
    matches!(fourcc, b"Hap5" | b"HapM" | b"HapA" | b"Hap7")
}

/// Whether a native track carries alpha: ProRes 4444 / 4444 XQ (and the other alpha-capable
/// entries) with depth 32, QuickTime's rule.
fn native_alpha(track: &VideoTrack) -> bool {
    track.depth == 32 && matches!(&track.fourcc, b"ap4h" | b"ap4x" | b"png " | b"raw " | b"rle " | b"BGRA" | b"hvc1" | b"hev1")
}

/// The layout a native clip is decoded in. 8-bit 4:2:0 planes (no conversion at all) only for the
/// codecs that are 4:2:0 (H.264, HEVC) and carry no alpha; everything else — ProRes 422 / 4444,
/// MJPEG (4:2:2 / 4:4:4), unknown files — as BGRA converted inside the platform decoder, so no
/// chroma resolution is thrown away. Both layouts are imported zero-copy.
pub fn layout_for(fourcc: &[u8], has_alpha: bool) -> PixelLayout {
    let yuv420 = matches!(fourcc, b"avc1" | b"avc3" | b"hvc1" | b"hev1");
    if yuv420 && !has_alpha { PixelLayout::Yuv420 { full_range: false } } else { PixelLayout::Bgra }
}

fn no_backend(path: &str) -> MediaError {
    MediaError::Unsupported(format!("{path}: native codecs need a platform backend (macOS: dxvkit-plus-apple; Windows: planned)"))
}

fn native_probe(info: &mut MediaInfo, track: &VideoTrack, opts: &ImportOptions) -> Result<(), MediaError> {
    // The platform must be able to open it; the container's sample count and stts rate are exact.
    let backend = opts.backend.as_ref().ok_or_else(|| no_backend(&info.path))?;
    let dec = backend.open(Path::new(&info.path), layout_for(info.codec.as_bytes(), info.has_alpha))?;
    if track.frame_rate().is_none() {
        let (v, ts) = dec.info().frame_duration;
        info.rate = FrameRate::new(i64::from(ts), v);
    }
    Ok(())
}

/// GPU-ready pixel formats of a CPU-decoded plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum PixelFormat {
    /// DXT1, 8 bytes per 4x4 block.
    Bc1 = 0,
    /// DXT5, 16 bytes per block.
    Bc3 = 1,
    /// One channel, 8 bytes per block.
    Bc4 = 2,
    /// Two channels, 16 bytes per block.
    Bc5 = 3,
    /// HDR RGB, unsigned half float, 16 bytes per block.
    Bc6hUfloat = 4,
    /// HDR RGB, signed half float, 16 bytes per block.
    Bc6hSfloat = 5,
    /// BPTC, 16 bytes per block.
    Bc7 = 6,
    /// Uncompressed straight RGBA8.
    Rgba8 = 7,
}

impl PixelFormat {
    pub fn is_compressed(self) -> bool {
        self != PixelFormat::Rgba8
    }
    /// Bytes per 4x4 block (BC) or per pixel (RGBA8).
    pub fn unit_bytes(self) -> u32 {
        match self {
            PixelFormat::Bc1 | PixelFormat::Bc4 => 8,
            PixelFormat::Rgba8 => 4,
            _ => 16,
        }
    }
}

/// How a decoded frame becomes RGBA (on the GPU, or on the CPU by [`Decoded::to_rgba`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shading {
    /// RGBA as stored; `opaque` forces alpha to 1 (HAP "RGB").
    Rgba { opaque: bool },
    /// HAP Q: scaled YCoCg in BC3 (+ a BC4 alpha plane for HAP Q Alpha).
    HapYcocg { alpha: bool },
    /// HAP Alpha-Only: one BC4 channel (shown as grey).
    AlphaOnly,
    /// HAP HDR (BC6H).
    Hdr,
}

/// One plane to upload (block-compressed or RGBA8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    pub format: PixelFormat,
    /// Texture size (BC: multiples of 4).
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Plane {
    /// Bytes per row of 4x4 blocks (BC) or of pixels (RGBA8).
    pub fn bytes_per_row(&self) -> u32 {
        if self.format.is_compressed() { self.width.div_ceil(4).saturating_mul(self.format.unit_bytes()) } else { self.width.saturating_mul(4) }
    }
    pub fn rows(&self) -> u32 {
        if self.format.is_compressed() { self.height.div_ceil(4) } else { self.height }
    }
}

/// A decoded frame, ready for the GPU.
#[derive(Clone, Debug)]
pub enum Decoded {
    /// CPU-decoded planes (HAP blocks, RGBA8 images), uploaded as-is.
    Planes { index: u32, width: u32, height: u32, shading: Shading, planes: Vec<Plane> },
    /// A platform frame, imported zero-copy by the backend's crate.
    Native(NativeFrame),
}

impl Decoded {
    pub fn index(&self) -> u32 {
        match self {
            Decoded::Planes { index, .. } => *index,
            Decoded::Native(f) => f.index,
        }
    }
    /// Display size.
    pub fn size(&self) -> (u32, u32) {
        match self {
            Decoded::Planes { width, height, .. } => (*width, *height),
            Decoded::Native(f) => (f.width, f.height),
        }
    }
    /// Bytes held in CPU memory (cache accounting).
    pub fn cpu_bytes(&self) -> usize {
        match self {
            Decoded::Planes { planes, .. } => planes.iter().map(|p| p.data.len()).sum(),
            Decoded::Native(_) => 0,
        }
    }
    /// The frame as straight RGBA8 (`width * 4` stride), decoded on the CPU. Block planes decode
    /// with the integer BC1 / BC3 / BC4 conventions in [`crate::bc`] and the BC7 decoder in
    /// [`crate::bc7`]; HDR (BC6H) and native frames are `Unsupported` here (their GPU paths are
    /// verified separately).
    pub fn to_rgba(&self) -> Result<Vec<u8>, MediaError> {
        match self {
            Decoded::Planes { width, height, shading, planes, .. } => planes_to_rgba(*shading, planes, *width, *height),
            Decoded::Native(_) => Err(MediaError::Unsupported("CPU RGBA of a native frame (read it through its backend)".into())),
        }
    }
}

/// 16 RGBA pixels of block `i` of a plane (alpha-only planes come back as `[a, a, a, a]`).
fn block_rgba(plane: &Plane, i: usize) -> Result<[[u8; 4]; 16], MediaError> {
    use crate::bc::{bc1_block, bc4_block};
    let n = plane.format.unit_bytes() as usize;
    let s = i.saturating_mul(n);
    let blk = plane.data.get(s..s.saturating_add(n)).ok_or_else(|| MediaError::Invalid("block stream too short".into()))?;
    Ok(match plane.format {
        PixelFormat::Bc1 => bc1_block(blk, false),
        PixelFormat::Bc3 => {
            let mut c = bc1_block(blk.get(8..).unwrap_or_default(), true);
            for (p, a) in c.iter_mut().zip(bc4_block(blk)) {
                p[3] = a;
            }
            c
        }
        PixelFormat::Bc4 => bc4_block(blk).map(|a| [a, a, a, a]),
        PixelFormat::Bc7 => crate::bc7::bc7_block(blk),
        other => return Err(MediaError::Unsupported(format!("CPU decode of {other:?} blocks"))),
    })
}

/// CPU decode of block / RGBA8 planes to straight RGBA8 (the oracle of the GPU conversion).
pub fn planes_to_rgba(shading: Shading, planes: &[Plane], width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    let (w, h) = (width as usize, height as usize);
    let first = planes.first().ok_or_else(|| MediaError::Invalid("frame without planes".into()))?;
    if first.format == PixelFormat::Rgba8 {
        let mut v = first.data.get(..w.saturating_mul(h).saturating_mul(4)).ok_or_else(|| MediaError::Invalid("RGBA plane too short".into()))?.to_vec();
        if shading == (Shading::Rgba { opaque: true }) {
            v.chunks_exact_mut(4).filter_map(|p| p.get_mut(3)).for_each(|a| *a = 255);
        }
        return Ok(v);
    }
    let mut out = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    let bw = w.div_ceil(4);
    for by in 0..h.div_ceil(4) {
        for bx in 0..bw {
            let block = by.saturating_mul(bw).saturating_add(bx);
            let mut px = block_rgba(first, block)?;
            match shading {
                Shading::Rgba { opaque: true } => px.iter_mut().for_each(|p| p[3] = 255),
                Shading::Rgba { opaque: false } => {}
                Shading::HapYcocg { alpha } => {
                    // Exact (unrounded) BC3 palette, as a float GPU decoder sees it.
                    let n = first.format.unit_bytes() as usize;
                    let s = block.saturating_mul(n);
                    let exact = crate::bc::bc3_block_exact(first.data.get(s..s.saturating_add(n)).unwrap_or_default());
                    let a = if alpha { planes.get(1).map(|p| block_rgba(p, block)).transpose()? } else { None };
                    for (i, p) in px.iter_mut().enumerate() {
                        let [r, g, b] = hap::ycocg_scaled_to_rgb_f(exact.get(i).copied().unwrap_or_default());
                        let al = a.as_ref().and_then(|a| a.get(i)).map(|q| q[0]).unwrap_or(255);
                        *p = [r, g, b, al];
                    }
                }
                Shading::AlphaOnly => px.iter_mut().for_each(|p| p[3] = 255),
                Shading::Hdr => return Err(MediaError::Unsupported("CPU decode of HDR (BC6H) frames".into())),
            }
            for (i, p) in px.iter().enumerate() {
                let (x, y) = (bx.saturating_mul(4).saturating_add(i % 4), by.saturating_mul(4).saturating_add(i / 4));
                if x < w && y < h {
                    let o = y.saturating_mul(w).saturating_add(x).saturating_mul(4);
                    if let Some(d) = out.get_mut(o..o.saturating_add(4)) {
                        d.copy_from_slice(p);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// A decoder for one source. `fork` gives an independent decoder for a worker thread (stateless
/// codecs) or `None` when the decoder is sequential (native readers share one).
pub trait FrameSource: Send {
    fn info(&self) -> &MediaInfo;
    fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError>;
    fn fork(&self) -> Option<Box<dyn FrameSource>>;
    /// CPU RGBA of a decoded frame, through the backend for native frames (test oracle,
    /// thumbnails). Never used by playback.
    fn cpu_rgba(&self, decoded: &Decoded) -> Result<Vec<u8>, MediaError> {
        decoded.to_rgba()
    }
}

/// Open a decoder for a probed source. `parallel` lets HAP decompress a frame's chunks on worker
/// threads.
pub fn open(info: &MediaInfo, opts: &ImportOptions, parallel: bool) -> Result<Box<dyn FrameSource>, MediaError> {
    match info.kind {
        SourceKind::Hap => {
            let movie = Movie::open(Path::new(&info.path))?;
            let track = movie.track(Codec::Hap).cloned().ok_or(MediaError::NoVideoTrack)?;
            Ok(Box::new(HapSource { info: info.clone(), movie, track: Arc::new(track), buf: Vec::new(), parallel }))
        }
        SourceKind::ImageSequence => {
            let files = sequence_files(Path::new(&info.path))?;
            Ok(Box::new(ImageSource { info: info.clone(), files: Arc::new(files) }))
        }
        SourceKind::Still => Ok(Box::new(ImageSource { info: info.clone(), files: Arc::new(vec![PathBuf::from(&info.path)]) })),
        SourceKind::Native => {
            let backend = opts.backend.as_ref().ok_or_else(|| no_backend(&info.path))?;
            let decoder = backend.open(Path::new(&info.path), layout_for(info.codec.as_bytes(), info.has_alpha))?;
            Ok(Box::new(NativeSource { info: info.clone(), decoder }))
        }
        SourceKind::Dxv => Err(MediaError::Unsupported(format!("{}: DXV decoding is on the roadmap (not in this release)", info.path))),
        SourceKind::Lmbd => {
            let file = crate::lmbd::LmbdFile::open(Path::new(&info.path))?;
            Ok(Box::new(LmbdSource { info: info.clone(), file, scratch: Vec::new(), parallel }))
        }
    }
}

/// LMBD (LMB7): each frame decodes to one BC7 block plane at the coded (block-rounded) size,
/// shaded as stored RGBA. The values are sRGB-encoded unless the header says linear; renderers
/// that want linear light sample the texture through an sRGB view.
struct LmbdSource {
    info: MediaInfo,
    file: crate::lmbd::LmbdFile,
    scratch: Vec<u8>,
    parallel: bool,
}

impl FrameSource for LmbdSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError> {
        let mut data = Vec::new();
        self.file.decode_frame(frame, &mut self.scratch, &mut data, self.parallel)?;
        let h = self.file.header();
        let (cw, ch) = h.coded_size();
        Ok(Decoded::Planes {
            index: frame,
            width: h.width,
            height: h.height,
            shading: Shading::Rgba { opaque: false },
            planes: vec![Plane { format: PixelFormat::Bc7, width: cw, height: ch, data }],
        })
    }
    fn fork(&self) -> Option<Box<dyn FrameSource>> {
        Some(Box::new(LmbdSource { info: self.info.clone(), file: self.file.clone(), scratch: Vec::new(), parallel: self.parallel }))
    }
}

struct HapSource {
    info: MediaInfo,
    movie: Movie,
    track: Arc<VideoTrack>,
    buf: Vec<u8>,
    parallel: bool,
}

impl FrameSource for HapSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError> {
        let (w, h) = (self.track.width, self.track.height);
        self.movie.read_sample(&self.track, frame, &mut self.buf)?;
        let f = hap::decode(&self.buf, w, h, self.parallel)?;
        let (shading, planes) = hap::planes(f, w, h)?;
        Ok(Decoded::Planes { index: frame, width: w, height: h, shading, planes })
    }
    fn fork(&self) -> Option<Box<dyn FrameSource>> {
        Some(Box::new(HapSource {
            info: self.info.clone(),
            movie: self.movie.clone(),
            track: Arc::clone(&self.track),
            buf: Vec::new(),
            parallel: self.parallel,
        }))
    }
}

struct ImageSource {
    info: MediaInfo,
    files: Arc<Vec<PathBuf>>,
}

impl FrameSource for ImageSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError> {
        let path = self.files.get(frame as usize).ok_or(MediaError::Range { index: frame, count: self.info.frames })?;
        let (w, h, rgba) = load_image(path)?;
        Ok(Decoded::Planes {
            index: frame,
            width: w,
            height: h,
            shading: Shading::Rgba { opaque: false },
            planes: vec![Plane { format: PixelFormat::Rgba8, width: w, height: h, data: rgba }],
        })
    }
    fn fork(&self) -> Option<Box<dyn FrameSource>> {
        Some(Box::new(ImageSource { info: self.info.clone(), files: Arc::clone(&self.files) }))
    }
}

struct NativeSource {
    info: MediaInfo,
    decoder: Box<dyn NativeDecoder>,
}

impl FrameSource for NativeSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn decode(&mut self, frame: u32) -> Result<Decoded, MediaError> {
        // The container's sample count and the platform's grid can differ by a frame at the end;
        // the shorter one bounds the clip. Past it is an error, never a silent clamp.
        let count = self.decoder.info().frame_count.min(self.info.frames.max(1));
        if frame >= count {
            return Err(MediaError::Range { index: frame, count });
        }
        Ok(Decoded::Native(self.decoder.frame(frame)?))
    }
    fn fork(&self) -> Option<Box<dyn FrameSource>> {
        None
    }
    fn cpu_rgba(&self, decoded: &Decoded) -> Result<Vec<u8>, MediaError> {
        match decoded {
            Decoded::Native(f) => self.decoder.cpu_rgba(f),
            other => other.to_rgba(),
        }
    }
}
