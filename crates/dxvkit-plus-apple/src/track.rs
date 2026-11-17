//! What AVFoundation states about a movie's first video track ([`track_facts`]), and the error
//! type shared by the CPU-side AVFoundation readers ([`crate::plane::PlaneReader`],
//! [`crate::grab::FrameGrabber`]).
//!
//! The asset is a plain `AVURLAsset` (no options) and its tracks are loaded with the asynchronous
//! `loadTracksWithMediaType:completionHandler:` (Swift's `loadTracks(withMediaType:)`), so a file
//! AVFoundation cannot open fails with the same `NSError` a Swift app sees: its `description`
//! (`Error Domain=… Code=… "…" UserInfo={…}`), domain and code ([`AvError::Failed`]).
//!
//! Every value is read the way Swift MaCrate 0.16.4's `AVFoundationSource.open()` reads it, so a
//! Rust port that branches on these facts takes the same branches.

use crate::objc::ns;
use block2::RcBlock;
use dxvkit_plus_core::timing::MediaTime;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2_av_foundation::{AVAssetTrack, AVMediaTypeVideo, AVURLAsset};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::{
    CMFormatDescription, CMTime, CMTimeFlags, kCMFormatDescriptionExtension_ContainsAlphaChannel, kCMFormatDescriptionExtension_Depth,
    kCMFormatDescriptionExtension_FullRangeVideo, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
};
use objc2_core_video::kCVImageBufferYCbCrMatrixKey;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};
use std::fmt;
use std::path::Path;
use std::sync::mpsc;

/// Why AVFoundation could not serve a file. [`AvError::code`] is a stable number per variant
/// (pinned in `tests/native_av.rs`; new variants are only appended).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvError {
    /// The path is not UTF-8 (AVFoundation takes a file URL built from a string).
    PathNotUtf8,
    /// A framework constant is missing (never on a supported OS).
    Unavailable,
    /// The asset has no video track (`loadTracks(withMediaType: .video).first == nil`).
    NoVideoTrack,
    /// Opening or loading failed with an `NSError`: its `description`, domain and code.
    Failed { description: String, domain: String, code: i64 },
    /// `AVAssetReader.startReading()` returned false: `reader.error`'s description, if any.
    StartFailed(Option<String>),
    /// The reader ended in `.failed`: `reader.error`'s description, if any.
    ReadFailed(Option<String>),
    /// A decoded buffer could not be read on the CPU (lock failed, no base address, a size that
    /// does not fit). The text names the step.
    PixelBuffer(String),
}

impl AvError {
    /// The stable code of the variant (C ABI).
    pub fn code(&self) -> u32 {
        match self {
            AvError::PathNotUtf8 => 1,
            AvError::Unavailable => 2,
            AvError::NoVideoTrack => 3,
            AvError::Failed { .. } => 4,
            AvError::StartFailed(_) => 5,
            AvError::ReadFailed(_) => 6,
            AvError::PixelBuffer(_) => 7,
        }
    }
}

impl fmt::Display for AvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AvError::PathNotUtf8 => f.write_str("path is not UTF-8"),
            AvError::Unavailable => f.write_str("AVFoundation is not available"),
            AvError::NoVideoTrack => f.write_str("no video track"),
            AvError::Failed { description, .. } => f.write_str(description),
            AvError::StartFailed(e) => write!(f, "AVAssetReader.startReading failed: {}", e.as_deref().unwrap_or("nil")),
            AvError::ReadFailed(e) => write!(f, "AVAssetReader failed: {}", e.as_deref().unwrap_or("nil")),
            AvError::PixelBuffer(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AvError {}

/// `-[NSError description]` (what Swift's `"\(error)"` prints for a bridged `NSError`).
pub(crate) fn error_description(e: &NSError) -> String {
    e.description().downcast::<NSString>().map(|s| s.to_string()).unwrap_or_default()
}

pub(crate) fn av_failed(e: &NSError) -> AvError {
    AvError::Failed { description: error_description(e), domain: e.domain().to_string(), code: e.code() as i64 }
}

/// A `CMTime` as the core's [`MediaTime`] (the value and time scale as stored; a negative time
/// scale, which is never valid, becomes 0).
pub(crate) fn media_time(t: CMTime) -> MediaTime {
    MediaTime { value: t.value, timescale: u32::try_from(t.timescale).unwrap_or(0) }
}

/// A presentation time as the readers hand it out: the time when it is valid, zero otherwise.
pub(crate) fn valid_or_zero(t: CMTime) -> MediaTime {
    if t.flags.contains(CMTimeFlags::Valid) { media_time(t) } else { MediaTime { value: 0, timescale: 1 } }
}

/// `AVURLAsset(url:)` (no options).
pub(crate) fn url_asset(path: &Path) -> Result<Retained<AVURLAsset>, AvError> {
    let p = path.to_str().ok_or(AvError::PathNotUtf8)?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(p));
    // SAFETY: a valid file URL and no options (Swift `AVURLAsset(url:)`).
    Ok(unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) })
}

/// The asset's first video track, loaded through the asynchronous `loadTracksWithMediaType:` so a
/// failure is the `NSError` Swift's `loadTracks(withMediaType:)` throws. Blocks until AVFoundation
/// calls back (on its own queue).
pub(crate) fn first_video_track(asset: &AVURLAsset) -> Result<Retained<AVAssetTrack>, AvError> {
    // SAFETY: a framework constant (valid for the process).
    let video = unsafe { AVMediaTypeVideo }.ok_or(AvError::Unavailable)?;
    let (tx, rx) = mpsc::channel::<Result<Option<Retained<AVAssetTrack>>, AvError>>();
    let block = RcBlock::new(move |tracks: *mut NSArray<AVAssetTrack>, err: *mut NSError| {
        // SAFETY: AVFoundation passes a valid array or a valid error (either may be null), alive
        // for the duration of the call; `as_ref` only borrows them and the track is retained.
        let r = unsafe {
            if let Some(e) = err.as_ref() {
                Err(av_failed(e))
            } else if let Some(t) = tracks.as_ref() {
                Ok(t.firstObject())
            } else {
                Ok(None)
            }
        };
        // The receiver is gone only if the caller stopped waiting, which it never does.
        let _ = tx.send(r);
    });
    // SAFETY: a valid asset and media type; the block is Send-safe (it only moves an mpsc sender
    // and retained objects) and AVFoundation copies it, so it outlives this call.
    unsafe { asset.loadTracksWithMediaType_completionHandler(video, &block) };
    rx.recv().map_err(|_| AvError::Unavailable)??.ok_or(AvError::NoVideoTrack)
}

/// What AVFoundation says about the first video track — the values Swift MaCrate's
/// `AVFoundationSource.open()` reads.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackFacts {
    /// The first format description's media subtype (`*b"avc1"`, `*b"png "`, `*b"ap4h"` …);
    /// `None` when the track has no format description.
    pub codec: Option<[u8; 4]>,
    /// `Int(naturalSize.width)`, `Int(naturalSize.height)` (truncated toward zero; the size
    /// before the preferred transform).
    pub width: i64,
    pub height: i64,
    /// `nominalFrameRate`.
    pub nominal_fps: f32,
    /// `CMTimeGetSeconds(timeRange.duration)`.
    pub duration_seconds: f64,
    /// The end of the track's leading empty edits (`segments` while `isEmpty`: the target time
    /// range's end); zero without any.
    pub leading_empty: MediaTime,
    /// `kCVImageBufferYCbCrMatrixKey` in the first format description's extensions
    /// (`ITU_R_709_2`, `ITU_R_601_4`, `ITU_R_2020`, `SMPTE_240M_1995` …).
    pub ycbcr_matrix: Option<String>,
    /// `SampleDescriptionExtensionAtoms` holds a `colr` atom.
    pub colr_atom: bool,
    /// `kCMFormatDescriptionExtension_FullRangeVideo` is true.
    pub full_range: bool,
    /// `kCMFormatDescriptionExtension_ContainsAlphaChannel`, when present.
    pub contains_alpha_channel: Option<bool>,
    /// `kCMFormatDescriptionExtension_Depth`; 0 when absent.
    pub depth: i64,
    /// The track states alpha: `ContainsAlphaChannel == true` or `Depth == 32`.
    pub alpha: bool,
}

impl TrackFacts {
    /// The codec as four characters (each byte as a `char`), or `"unknown"` without a format
    /// description — Swift's `fourCCString` / `"unknown"`.
    pub fn codec_string(&self) -> String {
        match self.codec {
            Some(c) => c.iter().map(|b| char::from(*b)).collect(),
            None => "unknown".into(),
        }
    }
}

/// The first format description's extension facts: (subtype, matrix, colr, full range,
/// contains-alpha, depth).
pub(crate) struct DescriptionFacts {
    pub codec: Option<[u8; 4]>,
    pub ycbcr_matrix: Option<String>,
    pub colr_atom: bool,
    pub full_range: bool,
    pub contains_alpha_channel: Option<bool>,
    pub depth: Option<i64>,
}

pub(crate) fn description_facts(track: &AVAssetTrack) -> DescriptionFacts {
    let mut d = DescriptionFacts { codec: None, ycbcr_matrix: None, colr_atom: false, full_range: false, contains_alpha_channel: None, depth: None };
    // SAFETY: a property read on a valid track.
    let descriptions = unsafe { track.formatDescriptions() };
    let Some(first) = descriptions.firstObject() else { return d };
    // SAFETY: `formatDescriptions` is documented as an array of CMFormatDescriptions (CF objects in
    // an NSArray); `first` keeps the object alive while `desc` is used.
    let desc: &CMFormatDescription = unsafe { &*Retained::as_ptr(&first).cast::<CMFormatDescription>() };
    // SAFETY: plain getters on a valid format description.
    let (subtype, extensions) = unsafe { (desc.media_sub_type().to_be_bytes(), desc.extensions()) };
    d.codec = Some(subtype);
    // SAFETY: CFDictionary and NSDictionary are toll-free bridged; format-description extension
    // keys are CFStrings (CMFormatDescription.h), so NSString keys. `extensions` keeps it alive.
    let ext: Option<&NSDictionary<NSString, AnyObject>> = extensions.as_ref().map(|e| unsafe { &*CFRetained::as_ptr(e).as_ptr().cast() });
    // SAFETY: framework constants (valid for the process).
    let (k_matrix, k_atoms, k_full, k_alpha, k_depth) = unsafe {
        (
            kCVImageBufferYCbCrMatrixKey,
            kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
            kCMFormatDescriptionExtension_FullRangeVideo,
            kCMFormatDescriptionExtension_ContainsAlphaChannel,
            kCMFormatDescriptionExtension_Depth,
        )
    };
    let get = |k: &CFString| ext.and_then(|e| e.objectForKey(ns(k)));
    d.ycbcr_matrix = get(k_matrix).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string());
    d.colr_atom = get(k_atoms).and_then(|v| v.downcast::<NSDictionary>().ok()).is_some_and(|atoms| atoms.objectForKey(&NSString::from_str("colr")).is_some());
    d.full_range = get(k_full).and_then(|v| v.downcast::<NSNumber>().ok()).is_some_and(|n| n.boolValue());
    d.contains_alpha_channel = get(k_alpha).and_then(|v| v.downcast::<NSNumber>().ok()).map(|n| n.boolValue());
    d.depth = get(k_depth).and_then(|v| v.downcast::<NSNumber>().ok()).map(|n| n.integerValue() as i64);
    d
}

impl DescriptionFacts {
    /// `ContainsAlphaChannel == true || Depth == 32`.
    pub(crate) fn alpha(&self) -> bool {
        self.contains_alpha_channel == Some(true) || self.depth == Some(32)
    }
}

/// The facts of `path`'s first video track ([`TrackFacts`]).
pub fn track_facts(path: &Path) -> Result<TrackFacts, AvError> {
    autoreleasepool(|_| {
        let asset = url_asset(path)?;
        let track = first_video_track(&asset)?;
        // SAFETY: property reads on a valid, loaded track.
        let (size, fps, range, segments) = unsafe { (track.naturalSize(), track.nominalFrameRate(), track.timeRange(), track.segments()) };
        let mut leading_empty = MediaTime { value: 0, timescale: 1 };
        for segment in segments.iter() {
            // SAFETY: a property read on a valid segment.
            if !unsafe { segment.isEmpty() } {
                break;
            }
            // SAFETY: a property read on a valid segment, then CMTimeRangeGetEnd on a plain struct.
            leading_empty = media_time(unsafe { segment.timeMapping().target.end() });
        }
        let d = description_facts(&track);
        Ok(TrackFacts {
            codec: d.codec,
            width: size.width as i64,
            height: size.height as i64,
            nominal_fps: fps,
            // SAFETY: CMTimeGetSeconds on a plain struct.
            duration_seconds: unsafe { range.duration.seconds() },
            leading_empty,
            ycbcr_matrix: d.ycbcr_matrix.clone(),
            colr_atom: d.colr_atom,
            full_range: d.full_range,
            contains_alpha_channel: d.contains_alpha_channel,
            depth: d.depth.unwrap_or(0),
            alpha: d.alpha(),
        })
    })
}
