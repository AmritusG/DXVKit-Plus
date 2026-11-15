//! Native decode: `AVURLAsset` + `AVAssetReader` + `AVAssetReaderTrackOutput`, frame-accurate.
//!
//! The reader asks VideoToolbox for IOSurface-backed, Metal-compatible pixel buffers
//! (`kCVPixelBufferIOSurfacePropertiesKey`, `kCVPixelBufferMetalCompatibilityKey`) in the requested
//! [`PixelLayout`] — `'420v'` for 4:2:0 codecs (no conversion), `'BGRA'` for alpha codecs.
//!
//! Frame `i` is frame `i` of the track's [`FrameTiming`] (0.10.0, `dxvkit_plus_core::timing`:
//! Swift Glance's FrameTiming rule — every sample's presentation time through the edit list, exact
//! rationals), built by core from the container's own tables, so the native path numbers frames
//! exactly as the DXV / HAP paths and Swift Glance do. AVAssetReader serves those very times
//! (measured: every served presentation time equals the timing's start on the W3 clips). A file
//! core cannot parse (fragmented MP4) gets the grid from Swift's rate rules over AVFoundation's own
//! minimum frame duration, nominal rate and duration. Up to 0.9.0 the grid of `minFrameDuration`
//! (the SHORTEST sample) was used: jittered clips seeked one frame off and the count was
//! duration / that minimum (W3 handoff).
//!
//! Sequential reads reuse one reader; any other request restarts the reader with `timeRange`
//! starting at the previous frame's start and skips samples until the target (the frame whose start
//! is the last at or before the sample's presentation time) — exact for intra-frame codecs (ProRes,
//! MJPEG) and long-GOP ones (AVFoundation decodes from the preceding sync sample internally).

use crate::objc::{PixelBuffer, cmtime, cmtime_numeric, cmtime_pos_inf, dict, ns, number_bool, number_u32};
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::{Backend, NativeDecoder, NativeFrame, NativeInfo, PixelLayout, YuvMatrix};
use dxvkit_plus_core::source::StatedAlpha;
use dxvkit_plus_core::timing::{FrameTiming, MediaTime, RateInputs};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeVideo, AVURLAsset, AVURLAssetPreferPreciseDurationAndTimingKey,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{
    CMFormatDescription, CMTime, CMTimeRange, kCMFormatDescriptionAlphaChannelMode_PremultipliedAlpha, kCMFormatDescriptionExtension_AlphaChannelMode,
    kCMFormatDescriptionExtension_ContainsAlphaChannel,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::path::Path;
use std::sync::Arc;

/// The macOS backend: AVFoundation / VideoToolbox.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppleBackend;

impl Backend for AppleBackend {
    fn name(&self) -> &'static str {
        "avfoundation"
    }
    fn open(&self, path: &Path, layout: PixelLayout) -> Result<Box<dyn NativeDecoder>, MediaError> {
        Ok(Box::new(NativeReader::open(path, layout)?))
    }
}

/// The concrete handle inside a [`NativeFrame`] produced here.
pub(crate) fn pixel_buffer(frame: &NativeFrame) -> Result<&PixelBuffer, MediaError> {
    frame.handle.downcast_ref::<PixelBuffer>().ok_or_else(|| MediaError::Invalid("not a frame from dxvkit-plus-apple".into()))
}

struct Reader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
}

/// A frame-accurate native reader. Not `Sync`: use from one thread at a time (`&mut self`).
pub struct NativeReader {
    asset: Retained<AVURLAsset>,
    track: Retained<AVAssetTrack>,
    reader: Option<Reader>,
    /// The index the next `copyNextSampleBuffer` will deliver, when reading sequentially.
    next: Option<u32>,
    info: NativeInfo,
    timing: FrameTiming,
}

// SAFETY: AVAsset / AVAssetTrack / AVAssetReader may be used from any thread as long as calls are
// not concurrent; `NativeReader` is only reachable through `&mut self` (no `Sync`), so at most one
// thread calls into these objects at a time. Retain/release of the objects is atomic.
unsafe impl Send for NativeReader {}

fn platform(m: impl Into<String>) -> MediaError {
    MediaError::Platform(m.into())
}

/// The timing of the movie's first video track from core's own parse of the container (the track
/// AVFoundation's `tracksWithMediaType` lists first); `None` when core cannot read it.
fn core_timing(path: &Path) -> Option<FrameTiming> {
    let movie = dxvkit_plus_core::container::Movie::open(path).ok()?;
    movie.track_at(0).map(|t| t.timing.clone()).filter(|t| t.frame_count() > 0)
}

/// The alpha the track's first format description states — Swift Glance's rule
/// (`AlphaTypeProbe.probeAVFoundation`, glance/Sources/GlanceCore/AlphaType.swift:155-171): alpha
/// when the `ContainsAlphaChannel` extension says so, or, without that extension, when the codec is
/// ProRes 4444 / 4444 XQ (`ap4h` / `ap4x`); premultiplied only when the `AlphaChannelMode`
/// extension says premultiplied, else straight.
fn stated_alpha(track: &AVAssetTrack) -> StatedAlpha {
    // SAFETY: a property read on a valid track.
    let descriptions = unsafe { track.formatDescriptions() };
    let Some(first) = descriptions.firstObject() else { return StatedAlpha::None };
    // SAFETY: `formatDescriptions` is documented as an array of CMFormatDescriptions (CF objects
    // stored in an NSArray); `first` keeps the object alive while `desc` is used.
    let desc: &CMFormatDescription = unsafe { &*Retained::as_ptr(&first).cast::<CMFormatDescription>() };
    // SAFETY: plain getters on a valid format description.
    let (subtype, extensions) = unsafe { (desc.media_sub_type().to_be_bytes(), desc.extensions()) };
    // SAFETY: CFDictionary and NSDictionary are toll-free bridged; the keys of a format
    // description's extensions are CFStrings (CMFormatDescription.h), so NSString keys.
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
    let contains = match value(k_contains).and_then(|v| v.downcast::<NSNumber>().ok()) {
        Some(n) => n.boolValue(),
        None => matches!(&subtype, b"ap4h" | b"ap4x"),
    };
    if !contains {
        return StatedAlpha::None;
    }
    match value(k_mode).and_then(|v| v.downcast::<NSString>().ok()) {
        Some(m) if &*m == ns(premultiplied) => StatedAlpha::Premultiplied,
        _ => StatedAlpha::Straight,
    }
}

impl NativeReader {
    /// Open `path` and read its first video track's timing.
    pub fn open(path: &Path, layout: PixelLayout) -> Result<NativeReader, MediaError> {
        let p = path.to_str().ok_or_else(|| MediaError::Invalid(format!("{} is not UTF-8", path.display())))?;
        if !path.exists() {
            return Err(MediaError::Invalid(format!("{p}: no such file")));
        }
        autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(p));
            // SAFETY: a framework constant (valid for the process).
            let precise: &NSString = unsafe { AVURLAssetPreferPreciseDurationAndTimingKey };
            let yes = number_bool(true);
            let opts = dict(&[(precise, &*yes as &AnyObject)]);
            // SAFETY: a valid file URL and an options dictionary of NSString -> NSNumber.
            let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, Some(&opts)) };
            // SAFETY: a framework constant.
            let video = unsafe { AVMediaTypeVideo }.ok_or_else(|| platform("AVMediaTypeVideo missing"))?;
            // SAFETY: plain synchronous property read on a valid asset (deprecated in favour of the
            // async loader, still supported; this runs off the program thread).
            #[allow(deprecated)]
            let tracks = unsafe { asset.tracksWithMediaType(video) };
            let track = tracks.firstObject().ok_or_else(|| MediaError::Unsupported(format!("{p}: no video track AVFoundation can read")))?;
            // SAFETY: property reads on a valid track.
            let (size, min, range, fps) = unsafe { (track.naturalSize(), track.minFrameDuration(), track.timeRange(), track.nominalFrameRate()) };
            let fps = f64::from(fps);
            let start = if cmtime_numeric(&range.start) { (range.start.value, range.start.timescale) } else { (0, 1) };
            let dur = range.duration;
            let seconds = if cmtime_numeric(&dur) && dur.timescale > 0 { dur.value as f64 / f64::from(dur.timescale) } else { 0.0 };
            let timing = core_timing(path).unwrap_or_else(|| {
                // Swift's rules without a sample table: AVFoundation's own inputs, then the grid.
                let min = (cmtime_numeric(&min) && min.value > 0).then(|| u32::try_from(min.timescale).ok().map(|ts| (min.value, ts))).flatten();
                let (rate, source) =
                    dxvkit_plus_core::timing::rate_of(&RateInputs { track_seconds: seconds, min_frame_duration: min, nominal: fps, ..RateInputs::default() });
                let count = (seconds * rate.num as f64 / rate.den as f64).round();
                let count = if count.is_finite() && count > 0.0 { count.min(f64::from(u32::MAX)) as u32 } else { 0 };
                FrameTiming::grid(rate, count, source)
            });
            let frame_duration = (timing.rate.den, i32::try_from(timing.rate.num).unwrap_or(i32::MAX));
            let frame_count = timing.frame_count();
            if frame_count == 0 {
                return Err(platform(format!("{p}: the video track has no frames")));
            }
            let (w, h) = (size.width, size.height);
            if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 {
                return Err(platform(format!("{p}: invalid natural size {w}x{h}")));
            }
            let alpha = stated_alpha(&track);
            let info = NativeInfo { width: w.round() as u32, height: h.round() as u32, frame_count, frame_duration, start, nominal_fps: fps, layout, alpha };
            Ok(NativeReader { asset, track, reader: None, next: None, info, timing })
        })
    }

    /// Start of frame `i` (the timing's exact start, clamped to the track).
    fn time_of(&self, i: u32) -> CMTime {
        let t = self.timing.time_of_frame(i);
        cmtime(t.value, i32::try_from(t.timescale).unwrap_or(i32::MAX))
    }

    /// The frame a presentation time shows: the last frame starting at or before it (floor), Swift
    /// FrameTiming's `frame(at:)`.
    fn index_of(&self, t: &CMTime) -> i64 {
        match u32::try_from(t.timescale) {
            Ok(ts) if ts > 0 => i64::from(self.timing.frame_at(MediaTime::new(t.value, ts))),
            _ => -1,
        }
    }

    fn start_reader(&mut self, from: CMTime) -> Result<(), MediaError> {
        if let Some(old) = self.reader.take() {
            // SAFETY: cancelling a reader we own; harmless if it already finished.
            unsafe { old.reader.cancelReading() };
        }
        // SAFETY: a valid asset; the error out-parameter is handled by the Result.
        let reader =
            unsafe { AVAssetReader::assetReaderWithAsset_error(&self.asset) }.map_err(|e| platform(format!("AVAssetReader: {}", e.localizedDescription())))?;
        let format = match self.info.layout {
            PixelLayout::Bgra => kCVPixelFormatType_32BGRA,
            PixelLayout::Yuv420 { full_range: false } => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            PixelLayout::Yuv420 { full_range: true } => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        };
        let fmt = number_u32(format);
        let empty: Retained<objc2_foundation::NSDictionary<NSString, AnyObject>> = dict(&[]);
        let yes = number_bool(true);
        // SAFETY: framework constants (valid for the process).
        let (k_fmt, k_ios, k_metal): (&CFString, &CFString, &CFString) =
            unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey) };
        let settings = dict(&[(ns(k_fmt), &*fmt as &AnyObject), (ns(k_ios), &*empty as &AnyObject), (ns(k_metal), &*yes as &AnyObject)]);
        // SAFETY: the track belongs to this reader's asset; settings are CoreVideo pixel-buffer keys.
        let output = unsafe { AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&self.track, Some(&settings)) };
        // SAFETY: plain property set before reading starts.
        unsafe { output.setAlwaysCopiesSampleData(false) };
        // SAFETY: the time range is set before startReading, as required.
        unsafe { reader.setTimeRange(CMTimeRange { start: from, duration: cmtime_pos_inf() }) };
        // SAFETY: valid reader and output of the same asset.
        if !unsafe { reader.canAddOutput(&output) } {
            return Err(platform("AVAssetReader cannot add the track output"));
        }
        // SAFETY: as above; added before startReading.
        unsafe { reader.addOutput(&output) };
        // SAFETY: a configured reader.
        if !unsafe { reader.startReading() } {
            // SAFETY: property read.
            let e = unsafe { reader.error() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
            return Err(platform(format!("AVAssetReader failed to start: {e}")));
        }
        self.reader = Some(Reader { reader, output });
        Ok(())
    }

    /// The next decoded sample: (its own presentation time, pixel buffer). `None` at the end.
    fn next_sample(&mut self) -> Result<Option<(CMTime, PixelBuffer)>, MediaError> {
        let r = self.reader.as_ref().ok_or_else(|| platform("reader not started"))?;
        loop {
            // SAFETY: the output was added to a reader that started reading.
            let Some(sb) = (unsafe { r.output.copyNextSampleBuffer() }) else {
                // SAFETY: property reads.
                let status = unsafe { r.reader.status() };
                if status == AVAssetReaderStatus::Failed {
                    // SAFETY: property read.
                    let e = unsafe { r.reader.error() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
                    return Err(platform(format!("decode failed: {e}")));
                }
                return Ok(None);
            };
            // SAFETY: a valid sample buffer.
            if unsafe { sb.num_samples() } == 0 {
                continue; // marker-only buffer
            }
            // SAFETY: as above.
            let pts = unsafe { sb.presentation_time_stamp() };
            // SAFETY: as above; returns a +1 retained image buffer or None.
            let Some(image) = (unsafe { sb.image_buffer() }) else { continue };
            return Ok(Some((pts, PixelBuffer(image))));
        }
    }

    /// Decode frame `index` exactly (frame `index` of the track's timing).
    pub fn decode_frame(&mut self, index: u32) -> Result<NativeFrame, MediaError> {
        if index >= self.info.frame_count {
            return Err(MediaError::Range { index, count: self.info.frame_count });
        }
        autoreleasepool(|_| {
            if self.next != Some(index) || self.reader.is_none() {
                // One whole frame early. A reader whose time range starts INSIDE a sample delivers
                // that sample with its presentation time moved up to the range start (measured: no
                // trim attachment on decoded output), which would make the previous frame look like
                // the target. Starting at the previous frame's start means any such trimmed sample is
                // identified as <= index - 1 and skipped; the target arrives with its own time.
                let from = self.time_of(index.saturating_sub(1));
                self.start_reader(from)?;
                self.next = None;
            }
            loop {
                let Some((pts, buffer)) = self.next_sample()? else {
                    self.reader = None;
                    self.next = None;
                    return Err(platform(format!("frame {index} not delivered (end of track)")));
                };
                let got = self.index_of(&pts);
                if got < i64::from(index) {
                    continue;
                }
                let got = u32::try_from(got).unwrap_or(index);
                self.next = got.checked_add(1);
                return describe(buffer, got);
            }
        })
    }
}

/// Size, layout, matrix and IOSurface id of a decoded buffer.
fn describe(buffer: PixelBuffer, index: u32) -> Result<NativeFrame, MediaError> {
    let pb = &*buffer.0;
    let width = u32::try_from(CVPixelBufferGetWidth(pb)).unwrap_or(0);
    let height = u32::try_from(CVPixelBufferGetHeight(pb)).unwrap_or(0);
    let layout = match CVPixelBufferGetPixelFormatType(pb) {
        f if f == kCVPixelFormatType_32BGRA => PixelLayout::Bgra,
        f if f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange => PixelLayout::Yuv420 { full_range: false },
        f if f == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange => PixelLayout::Yuv420 { full_range: true },
        other => return Err(MediaError::Unsupported(format!("pixel format {:?}", other.to_be_bytes()))),
    };
    let surface = CVPixelBufferGetIOSurface(Some(pb)).ok_or_else(|| platform("decoded pixel buffer is not IOSurface-backed"))?;
    let surface_id = surface.id();
    let matrix = matrix_of(pb, height);
    Ok(NativeFrame { index, width, height, layout, matrix, surface_id, handle: Arc::new(buffer) })
}

fn matrix_of(pb: &objc2_core_video::CVPixelBuffer, height: u32) -> YuvMatrix {
    // SAFETY: framework constants; a null attachment-mode pointer is allowed.
    let value = unsafe { pb.attachment(kCVImageBufferYCbCrMatrixKey, std::ptr::null_mut()) };
    let Some(value) = value else { return if height > 576 { YuvMatrix::Bt709 } else { YuvMatrix::Bt601 } };
    let Ok(s) = value.downcast::<CFString>() else { return YuvMatrix::Bt709 };
    // SAFETY: framework constants.
    let (m601, m2020) = unsafe { (kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrix_ITU_R_2020) };
    if *s == *m601 {
        YuvMatrix::Bt601
    } else if *s == *m2020 {
        YuvMatrix::Bt2020
    } else {
        YuvMatrix::Bt709
    }
}

impl NativeDecoder for NativeReader {
    fn info(&self) -> &NativeInfo {
        &self.info
    }
    fn frame(&mut self, index: u32) -> Result<NativeFrame, MediaError> {
        self.decode_frame(index)
    }
    fn timing(&self) -> Option<&FrameTiming> {
        Some(&self.timing)
    }
    fn cpu_rgba(&self, frame: &NativeFrame) -> Result<Vec<u8>, MediaError> {
        cpu_rgba(frame)
    }
}

/// The frame as tight RGBA8, read through a CPU lock — **only** for `Bgra` frames, and only as a
/// test oracle (Apple's own YCbCr->RGB conversion) or for thumbnails. The playback path never reads
/// pixels on the CPU ([`crate::surface::import`] is zero-copy).
pub fn cpu_rgba(frame: &NativeFrame) -> Result<Vec<u8>, MediaError> {
    if frame.layout != PixelLayout::Bgra {
        return Err(MediaError::Unsupported("cpu_rgba reads BGRA frames only".into()));
    }
    let pb = &*pixel_buffer(frame)?.0;
    // SAFETY: a valid pixel buffer; read-only lock, released below on every path.
    let r = unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    if r != 0 {
        return Err(platform(format!("CVPixelBufferLockBaseAddress failed ({r})")));
    }
    let base = CVPixelBufferGetBaseAddress(pb);
    let stride = CVPixelBufferGetBytesPerRow(pb);
    let (w, h) = (frame.width as usize, frame.height as usize);
    let out = if base.is_null() || stride < w.saturating_mul(4) {
        Err(platform("pixel buffer has no CPU base address"))
    } else {
        let len = stride.saturating_mul(h);
        // SAFETY: while locked, `base` points at `bytes_per_row * height` readable bytes owned by
        // the buffer; we only read them, and only until the unlock below.
        let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) };
        let mut rgba = Vec::with_capacity(w.saturating_mul(h).saturating_mul(4));
        for row in bytes.chunks(stride).take(h) {
            for px in row.chunks_exact(4).take(w) {
                if let [b, g, r, a] = *px {
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
        }
        Ok(rgba)
    };
    // SAFETY: matches the lock above with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    out
}
