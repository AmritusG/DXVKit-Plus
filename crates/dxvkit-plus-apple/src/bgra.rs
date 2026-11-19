//! Sequential BGRA frames through `AVAssetReader`, exactly as Swift GlEnc 1.6.2's
//! `AVAssetReaderSourceReader` reads them (glenc `Sources/GlEncCore/SourceFrameReader.swift`):
//! a plain `AVURLAsset` (no options), the FIRST video track, one `AVAssetReaderTrackOutput` whose
//! settings are `kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA` and nothing else,
//! `alwaysCopiesSampleData = false`, read from the start to the end.
//!
//! Unlike [`crate::decode::NativeReader`] (playback: IOSurface + Metal-compatible buffers, random
//! access through the core's frame timing) this reader is GlEnc's encode source: every decoded
//! sample in the order AVFoundation serves them (presentation order) with its own presentation
//! time and its pixels copied out tight (row padding removed), plus the buffer's attachments. The
//! frame rate / frame count (GlanceCore's `FrameTiming`) are not derived here; the core's `timing`
//! has them. Measured: the pixels equal Swift GlEnc's reader's and the playback reader's.
//!
//! Lint note: `arithmetic_side_effects` is allowed in this module for the row / size products,
//! bounded by the buffer's own size (checked before any slice is formed).

use crate::objc::{dict, ns, number_u32};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2_av_foundation::{AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeVideo, AVURLAsset};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_core_media::{
    CMFormatDescription, kCMFormatDescriptionAlphaChannelMode_PremultipliedAlpha, kCMFormatDescriptionExtension_AlphaChannelMode,
    kCMFormatDescriptionExtension_ContainsAlphaChannel,
};
use objc2_core_video::{
    CVAttachmentMode, CVBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimariesKey,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSString, NSURL};
use std::fmt;
use std::path::Path;

/// Swift's `SourceGeometryLimits` (SourceFrameReader.swift).
const MAX_DIMENSION: i64 = 65535;
const MAX_FRAME_PIXELS: i64 = 16384 * 16384;

/// Why a source could not be read — Swift's `SourceReaderError` cases this reader can raise, with
/// their `description` texts.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceReaderError {
    /// The path is not a readable UTF-8 file path (Rust-side; Swift takes a URL).
    Path(String),
    /// "source has no video track".
    NoVideoTrack,
    /// `AVAssetReader(asset:)` threw (Swift lets that error through unwrapped; its text here).
    ReaderInit(String),
    /// "AVAssetReader.startReading failed: …" (the text after the colon is Swift's
    /// `String(describing: reader.error)`, e.g. `Optional(Error Domain=…)` or `nil`).
    AvAssetReaderStartFailed(String),
    /// "AVAssetReader failed mid-stream: …".
    AvAssetReaderFailed(String),
    /// The decoded buffer is not the 32BGRA the output was asked for, or has no CPU address.
    PixelBuffer(String),
    /// "source reports unusable dimensions W×H — width and height must each be positive".
    SourceDimensionsInvalid { width: i64, height: i64 },
    /// "source dimensions W×H exceed the maximum encodable size of 65535 pixels per side".
    SourceDimensionsTooLarge { width: i64, height: i64 },
    /// "source frame W×H is too large to encode safely (over 268435456 pixels)".
    SourceFrameTooLarge { width: i64, height: i64 },
    /// "source reports an invalid duration (D seconds)".
    SourceDurationInvalid(f64),
}

impl fmt::Display for SourceReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceReaderError::Path(m) | SourceReaderError::ReaderInit(m) | SourceReaderError::PixelBuffer(m) => f.write_str(m),
            SourceReaderError::NoVideoTrack => f.write_str("source has no video track"),
            SourceReaderError::AvAssetReaderStartFailed(e) => write!(f, "AVAssetReader.startReading failed: {e}"),
            SourceReaderError::AvAssetReaderFailed(e) => write!(f, "AVAssetReader failed mid-stream: {e}"),
            SourceReaderError::SourceDimensionsInvalid { width, height } => {
                write!(f, "source reports unusable dimensions {width}×{height} — width and height must each be positive")
            }
            SourceReaderError::SourceDimensionsTooLarge { width, height } => {
                write!(f, "source dimensions {width}×{height} exceed the maximum encodable size of {MAX_DIMENSION} pixels per side")
            }
            SourceReaderError::SourceFrameTooLarge { width, height } => {
                write!(f, "source frame {width}×{height} is too large to encode safely (over {MAX_FRAME_PIXELS} pixels)")
            }
            SourceReaderError::SourceDurationInvalid(d) => write!(f, "source reports an invalid duration ({d} seconds)"),
        }
    }
}

impl std::error::Error for SourceReaderError {}

/// Swift's `String(describing: error)` of an `Error?`: `Optional(<NSError description>)` or `nil`.
pub(crate) fn swift_optional_error(e: Option<Retained<NSError>>) -> String {
    match e {
        Some(e) => format!("Optional({})", ns_error_description(&e)),
        None => "nil".into(),
    }
}

/// `-[NSError description]` (Swift's `String(describing:)` of a bridged error):
/// `Error Domain=… Code=… "…" UserInfo={…}`.
pub(crate) fn ns_error_description(e: &NSError) -> String {
    e.description().downcast::<NSString>().map(|s| s.to_string()).unwrap_or_default()
}

/// One decoded frame: its presentation time, its pixels, tight BGRA8 (`width * height * 4`), and
/// the decoded buffer's attachments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BgraFrame {
    pub pts_value: i64,
    pub pts_timescale: i32,
    /// The decoded buffer's own size (normally the track's rounded natural size).
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// The decoded buffer's attachments (colour primaries / transfer / matrix, colour space, field
    /// count, pixel aspect, …). GlEnc hands the decoder's own buffer to `AVAssetWriter` when it
    /// does not resize or crop, and the writer encodes with these (the `colr` atom, the
    /// RGB→YCbCr matrix): pass them to [`crate::writer::AvWriter::append_with_attachments`] for
    /// such frames, and nothing for frames GlEnc would rebuild (resized, cropped, un-premultiplied).
    pub attachments: Option<FrameAttachments>,
}

/// One decoded frame as the decoder's own pixel buffer (32BGRA), not copied (0.15.1): what Swift GlEnc hands to
/// `AVAssetWriter` when it neither crops, resizes nor un-premultiplies. [`AvWriter::append_buffer`] appends it as it
/// is (its attachments included); [`DecodedBuffer::to_frame`] copies it out when the pixels are needed.
///
/// [`AvWriter::append_buffer`]: crate::writer::AvWriter::append_buffer
#[derive(Clone)]
pub struct DecodedBuffer {
    pub pts_value: i64,
    pub pts_timescale: i32,
    pub width: u32,
    pub height: u32,
    pub(crate) buffer: CFRetained<objc2_core_video::CVPixelBuffer>,
}

// SAFETY: a CVPixelBuffer's retain / release are atomic, and nothing writes to a decoded buffer: the reader hands it
// over (AVAssetReader does not reuse a buffer while it is retained), it is only read (`to_frame`'s read-only lock) or
// appended to a writer, which reads it.
unsafe impl Send for DecodedBuffer {}
// SAFETY: as above — only reads through `&DecodedBuffer`.
unsafe impl Sync for DecodedBuffer {}

impl fmt::Debug for DecodedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedBuffer").field("pts", &(self.pts_value, self.pts_timescale)).field("size", &(self.width, self.height)).finish()
    }
}

impl DecodedBuffer {
    /// The pixels as tight BGRA8 with the buffer's attachments: [`BgraReader::next_frame`]'s frame.
    pub fn to_frame(&self) -> Result<BgraFrame, SourceReaderError> {
        let (width, height, data) = tight_bgra(&self.buffer)?;
        let attachments = FrameAttachments::of(&self.buffer);
        Ok(BgraFrame { pts_value: self.pts_value, pts_timescale: self.pts_timescale, width, height, data, attachments })
    }
}

/// A `CVBuffer`'s attachments, both modes (`kCVAttachmentMode_ShouldPropagate` and
/// `…ShouldNotPropagate`), as CoreVideo copies them. Opaque; compared with `CFEqual`.
#[derive(Clone)]
pub struct FrameAttachments {
    pub(crate) propagate: Option<CFRetained<CFDictionary>>,
    pub(crate) not_propagate: Option<CFRetained<CFDictionary>>,
}

// SAFETY: the dictionaries are CoreVideo's immutable copies (CVBufferCopyAttachments); CF objects
// have atomic retain/release and immutable CF collections are thread-safe to read.
unsafe impl Send for FrameAttachments {}
// SAFETY: as above — only reads through `&FrameAttachments`.
unsafe impl Sync for FrameAttachments {}

impl PartialEq for FrameAttachments {
    fn eq(&self, other: &Self) -> bool {
        self.propagate.as_deref() == other.propagate.as_deref() && self.not_propagate.as_deref() == other.not_propagate.as_deref()
    }
}
impl Eq for FrameAttachments {}

impl fmt::Debug for FrameAttachments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameAttachments")
            .field("color_primaries", &self.string(CVKey::Primaries))
            .field("transfer_function", &self.string(CVKey::Transfer))
            .field("ycbcr_matrix", &self.string(CVKey::Matrix))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum CVKey {
    Primaries,
    Transfer,
    Matrix,
}

impl FrameAttachments {
    /// The attachments of a buffer (None when it has none in either mode).
    pub(crate) fn of(pb: &CVBuffer) -> Option<FrameAttachments> {
        let propagate = pb.attachments(CVAttachmentMode::ShouldPropagate);
        let not_propagate = pb.attachments(CVAttachmentMode::ShouldNotPropagate);
        (propagate.is_some() || not_propagate.is_some()).then_some(FrameAttachments { propagate, not_propagate })
    }

    fn string(&self, key: CVKey) -> Option<String> {
        // SAFETY: framework constants (valid for the process).
        let k: &CFString = unsafe {
            match key {
                CVKey::Primaries => kCVImageBufferColorPrimariesKey,
                CVKey::Transfer => kCVImageBufferTransferFunctionKey,
                CVKey::Matrix => kCVImageBufferYCbCrMatrixKey,
            }
        };
        let d = self.propagate.as_deref()?;
        // SAFETY: CFDictionary and NSDictionary are toll-free bridged; attachment keys are CFStrings.
        let d: &NSDictionary<NSString, AnyObject> = unsafe { &*std::ptr::from_ref::<CFDictionary>(d).cast() };
        d.objectForKey(ns(k)).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string())
    }

    /// `kCVImageBufferColorPrimariesKey` (e.g. "ITU_R_709_2").
    pub fn color_primaries(&self) -> Option<String> {
        self.string(CVKey::Primaries)
    }
    /// `kCVImageBufferTransferFunctionKey`.
    pub fn transfer_function(&self) -> Option<String> {
        self.string(CVKey::Transfer)
    }
    /// `kCVImageBufferYCbCrMatrixKey`.
    pub fn ycbcr_matrix(&self) -> Option<String> {
        self.string(CVKey::Matrix)
    }

    /// Set these attachments on `pb` (both modes), as the decoder's buffer carried them.
    pub(crate) fn apply(&self, pb: &CVBuffer) {
        if let Some(d) = &self.propagate {
            // SAFETY: a valid buffer and a dictionary of CF keys/values copied from another buffer.
            unsafe { pb.set_attachments(d, CVAttachmentMode::ShouldPropagate) };
        }
        if let Some(d) = &self.not_propagate {
            // SAFETY: as above.
            unsafe { pb.set_attachments(d, CVAttachmentMode::ShouldNotPropagate) };
        }
    }
}

/// What the track's FIRST format description states about alpha (its extensions, as GlEnc and
/// Glance read them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlphaFacts {
    /// `kCMFormatDescriptionExtension_ContainsAlphaChannel`, when the extension is present.
    pub contains_alpha_channel: Option<bool>,
    /// `kCMFormatDescriptionExtension_AlphaChannelMode` == premultiplied (GlEnc un-premultiplies
    /// such frames on read when the output keeps alpha; this reader does not).
    pub premultiplied: bool,
    /// The codec FourCC (media subtype) of that description, e.g. `*b"ap4h"`.
    pub codec: [u8; 4],
}

/// GlEnc's `AVAssetReaderSourceReader`. Not `Sync`; use from one thread at a time.
pub struct BgraReader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    width: u32,
    height: u32,
    duration_seconds: f64,
    alpha: AlphaFacts,
}

// SAFETY: AVAssetReader / AVAssetReaderTrackOutput may be used from any thread as long as calls are
// not concurrent; `BgraReader` is only reachable through `&mut self` for reading (no `Sync`), so at
// most one thread calls into these objects at a time. Retain/release is atomic.
unsafe impl Send for BgraReader {}

fn alpha_facts(track: &AVAssetTrack) -> AlphaFacts {
    // SAFETY: a property read on a valid track.
    let descriptions = unsafe { track.formatDescriptions() };
    let Some(first) = descriptions.firstObject() else { return AlphaFacts::default() };
    // SAFETY: `formatDescriptions` is documented as an array of CMFormatDescriptions (CF objects in
    // an NSArray); `first` keeps the object alive while `desc` is used.
    let desc: &CMFormatDescription = unsafe { &*Retained::as_ptr(&first).cast::<CMFormatDescription>() };
    // SAFETY: plain getters on a valid format description.
    let (subtype, extensions) = unsafe { (desc.media_sub_type().to_be_bytes(), desc.extensions()) };
    // SAFETY: CFDictionary and NSDictionary are toll-free bridged; format-description extension
    // keys are CFStrings (CMFormatDescription.h), so NSString keys.
    let ext: Option<&NSDictionary<NSString, AnyObject>> = extensions.as_ref().map(|d| unsafe { &*CFRetained::as_ptr(d).as_ptr().cast() });
    // SAFETY: framework constants (valid for the process).
    let (k_contains, k_mode, premultiplied) = unsafe {
        (
            kCMFormatDescriptionExtension_ContainsAlphaChannel,
            kCMFormatDescriptionExtension_AlphaChannelMode,
            kCMFormatDescriptionAlphaChannelMode_PremultipliedAlpha,
        )
    };
    let value = |k: &CFString| ext.and_then(|d| d.objectForKey(ns(k)));
    let contains_alpha_channel = value(k_contains).and_then(|v| v.downcast::<NSNumber>().ok()).map(|n| n.boolValue());
    let premultiplied = value(k_mode).and_then(|v| v.downcast::<NSString>().ok()).is_some_and(|m| &*m == ns(premultiplied));
    AlphaFacts { contains_alpha_channel, premultiplied, codec: subtype }
}

/// Swift's `validateSourceGeometry` minus the frame-rate check (the rate is the core's timing).
fn validate(width: i64, height: i64, duration: f64) -> Result<(), SourceReaderError> {
    if width < 1 || height < 1 {
        return Err(SourceReaderError::SourceDimensionsInvalid { width, height });
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(SourceReaderError::SourceDimensionsTooLarge { width, height });
    }
    if width.saturating_mul(height) > MAX_FRAME_PIXELS {
        return Err(SourceReaderError::SourceFrameTooLarge { width, height });
    }
    if !duration.is_finite() || duration < 0.0 {
        return Err(SourceReaderError::SourceDurationInvalid(duration));
    }
    Ok(())
}

/// Swift's `Int(x.rounded())` for a CGFloat size: round half away from zero (out-of-range and NaN
/// saturate here instead of trapping; the validation then rejects them).
fn round_dim(x: f64) -> i64 {
    if x.is_nan() { 0 } else { x.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64 }
}

impl BgraReader {
    /// Open `path` and start reading its first video track from the beginning.
    pub fn open(path: &Path) -> Result<BgraReader, SourceReaderError> {
        let p = path.to_str().ok_or_else(|| SourceReaderError::Path(format!("{} is not UTF-8", path.display())))?;
        // Swift's async `loadTracks` throws for a missing file (and glenc-cli refuses it up front:
        // "input file not found"); the synchronous track list would just be empty.
        if !path.is_file() {
            return Err(SourceReaderError::Path(format!("input file not found: {p}")));
        }
        autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(p));
            // SAFETY: a valid file URL; no options (Swift: `AVURLAsset(url:)`).
            let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
            // SAFETY: a framework constant.
            let video = unsafe { AVMediaTypeVideo }.ok_or_else(|| SourceReaderError::ReaderInit("AVMediaTypeVideo missing".into()))?;
            // SAFETY: plain synchronous property read on a valid asset (the synchronous form of
            // Swift's `loadTracks(withMediaType:)`; deprecated, still supported).
            #[allow(deprecated)]
            let tracks = unsafe { asset.tracksWithMediaType(video) };
            let track = tracks.firstObject().ok_or(SourceReaderError::NoVideoTrack)?;
            // SAFETY: property reads on a valid track / asset.
            let (size, duration) = unsafe { (track.naturalSize(), asset.duration()) };
            let (w, h) = (round_dim(size.width), round_dim(size.height));
            // SAFETY: CMTimeGetSeconds on a plain struct.
            let seconds = unsafe { duration.seconds() };
            validate(w, h, seconds)?;
            let alpha = alpha_facts(&track);

            // SAFETY: a valid asset; the error out-parameter becomes the Result.
            let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }.map_err(|e| SourceReaderError::ReaderInit(ns_error_description(&e)))?;
            let fmt = number_u32(kCVPixelFormatType_32BGRA);
            // SAFETY: a framework constant (valid for the process).
            let k_fmt: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
            let settings = dict(&[(ns(k_fmt), &*fmt as &AnyObject)]);
            // SAFETY: the track belongs to this reader's asset; the settings are one CoreVideo key.
            let output = unsafe { AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&settings)) };
            // SAFETY: plain property set before reading starts.
            unsafe { output.setAlwaysCopiesSampleData(false) };
            // SAFETY: valid reader and output of the same asset. (Swift adds without asking; asking
            // first turns AVFoundation's exception for an unaddable output into an error.)
            if !unsafe { reader.canAddOutput(&output) } {
                return Err(SourceReaderError::AvAssetReaderStartFailed("nil".into()));
            }
            // SAFETY: as above; added before startReading.
            unsafe { reader.addOutput(&output) };
            // SAFETY: a configured reader.
            if !unsafe { reader.startReading() } {
                // SAFETY: property read.
                return Err(SourceReaderError::AvAssetReaderStartFailed(swift_optional_error(unsafe { reader.error() })));
            }
            let width = u32::try_from(w).unwrap_or(0);
            let height = u32::try_from(h).unwrap_or(0);
            Ok(BgraReader { reader, output, width, height, duration_seconds: seconds, alpha })
        })
    }

    /// `naturalSize.width` rounded (Swift `Int(naturalSize.width.rounded())`).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// `naturalSize.height` rounded.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// `CMTimeGetSeconds(asset.duration)`.
    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }

    /// The first format description's alpha facts.
    pub fn alpha(&self) -> AlphaFacts {
        self.alpha
    }

    /// The next frame, or `None` at the end — Swift's `readNextFrame`: a failed reader is an
    /// error; a reader that is no longer reading, no sample, or a sample without an image buffer
    /// ends the stream.
    pub fn next_frame(&mut self) -> Result<Option<BgraFrame>, SourceReaderError> {
        self.next_buffer()?.map(|b| b.to_frame()).transpose()
    }

    /// [`BgraReader::next_frame`] without the copy: the decoder's own buffer (0.15.1).
    pub fn next_buffer(&mut self) -> Result<Option<DecodedBuffer>, SourceReaderError> {
        autoreleasepool(|_| {
            // SAFETY: property reads on a started reader.
            let status = unsafe { self.reader.status() };
            if status == AVAssetReaderStatus::Failed {
                // SAFETY: property read.
                return Err(SourceReaderError::AvAssetReaderFailed(swift_optional_error(unsafe { self.reader.error() })));
            }
            if status != AVAssetReaderStatus::Reading {
                return Ok(None);
            }
            // SAFETY: the output was added to a reader that started reading.
            let Some(sample) = (unsafe { self.output.copyNextSampleBuffer() }) else {
                // SAFETY: property reads.
                if unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
                    // SAFETY: property read.
                    return Err(SourceReaderError::AvAssetReaderFailed(swift_optional_error(unsafe { self.reader.error() })));
                }
                return Ok(None);
            };
            // SAFETY: a valid sample buffer.
            let pts = unsafe { sample.presentation_time_stamp() };
            // SAFETY: as above; returns a +1 retained image buffer or None.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return Ok(None) };
            let format = CVPixelBufferGetPixelFormatType(&image);
            if format != kCVPixelFormatType_32BGRA {
                return Err(SourceReaderError::PixelBuffer(format!("decoded pixel format {:?} is not 32BGRA", format.to_be_bytes())));
            }
            let (width, height) = (CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image));
            Ok(Some(DecodedBuffer {
                pts_value: pts.value,
                pts_timescale: pts.timescale,
                width: u32::try_from(width).unwrap_or(0),
                height: u32::try_from(height).unwrap_or(0),
                buffer: image,
            }))
        })
    }
}

/// The buffer's pixels as tight BGRA8 (row padding removed), through a read-only CPU lock.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn tight_bgra(pb: &objc2_core_video::CVPixelBuffer) -> Result<(u32, u32, Vec<u8>), SourceReaderError> {
    let format = CVPixelBufferGetPixelFormatType(pb);
    if format != kCVPixelFormatType_32BGRA {
        return Err(SourceReaderError::PixelBuffer(format!("decoded pixel format {:?} is not 32BGRA", format.to_be_bytes())));
    }
    let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
    // SAFETY: a valid pixel buffer; read-only lock, released below on every path.
    let r = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if r != 0 {
        return Err(SourceReaderError::PixelBuffer(format!("CVPixelBufferLockBaseAddress failed ({r})")));
    }
    let base = CVPixelBufferGetBaseAddress(pb);
    let stride = CVPixelBufferGetBytesPerRow(pb);
    let row = w.saturating_mul(4);
    let out = if base.is_null() || stride < row || h == 0 {
        Err(SourceReaderError::PixelBuffer("pixel buffer has no CPU base address".into()))
    } else {
        let len = stride * (h - 1) + row;
        // SAFETY: while locked, `base` points at `bytes_per_row * height` readable bytes owned by
        // the buffer (`len` does not exceed that); we only read them, and only until the unlock.
        let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) };
        let mut data = Vec::with_capacity(row * h);
        for r in bytes.chunks(stride).take(h) {
            data.extend_from_slice(r.get(..row).unwrap_or_default());
        }
        Ok((u32::try_from(w).unwrap_or(0), u32::try_from(h).unwrap_or(0), data))
    };
    // SAFETY: matches the lock above with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    out
}
