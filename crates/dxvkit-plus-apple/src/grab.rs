//! One picture of a movie at a chosen time, through `AVAssetImageGenerator` — Swift MaCrate
//! 0.16.4's thumbnail path for native clips (`ThumbnailExtractor.extractAVAsset`):
//! `appliesPreferredTrackTransform = true`, a `maximumSize` (Swift's default 1920×1080, see
//! [`THUMBNAIL_MAX_SIZE`]), zero tolerance before and after, `copyCGImage(at:actualTime:)`.
//!
//! The image is handed over as RGBA8 pixels, drawn the way Swift draws it: into a device-RGB
//! bitmap context of the image's own size, 8 bits per component, `premultipliedLast` (the
//! context Swift's luminance check and checkerboard composite draw into), so the colour is what
//! CoreGraphics matches the image to. [`GrabAlpha::Straight`] then divides the colour by alpha
//! (rounded), [`GrabAlpha::Premultiplied`] keeps the context's bytes. Opaque images are the same
//! either way. The walk-forward, the luminance check and the checkerboard are the caller's.
//!
//! Lint note: `arithmetic_side_effects` is allowed in this module for the pixel arithmetic
//! (bounded: sizes are checked before the buffer is made; colour values are ≤ 255 · 255).

use crate::track::{AvError, av_failed, description_facts, first_video_track, media_time, url_asset};
use dxvkit_plus_core::timing::MediaTime;
use objc2::rc::{Retained, autoreleasepool};
use objc2_av_foundation::AVAssetImageGenerator;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo};
use objc2_core_media::{CMTime, CMTimeFlags};
use std::path::Path;

/// Swift MaCrate's thumbnail size cap (`ThumbnailExtractor(targetSize:)`'s default): width, height.
pub const THUMBNAIL_MAX_SIZE: (f64, f64) = (1920.0, 1080.0);

/// How the colour of a grabbed image with alpha is handed over.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GrabAlpha {
    /// Colour divided by alpha (`round(c · 255 / a)`, 0 where alpha is 0).
    #[default]
    Straight = 0,
    /// The premultiplied bytes CoreGraphics drew.
    Premultiplied = 1,
}

/// A grabbed picture: tight RGBA8 rows, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrabbedImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, R G B A.
    pub rgba: Vec<u8>,
    /// How the colour is stored (as asked).
    pub alpha_mode: GrabAlpha,
    /// The CGImage itself carries an alpha channel (its alpha info is not none / skip).
    pub image_has_alpha: bool,
    /// The time of the frame AVFoundation decoded (`actualTime`).
    pub actual_time: MediaTime,
}

/// An `AVAssetImageGenerator` set up as Swift MaCrate's thumbnail extractor sets it up. Not
/// `Sync`; use from one thread at a time.
pub struct FrameGrabber {
    generator: Retained<AVAssetImageGenerator>,
    track_alpha: bool,
}

// SAFETY: AVAssetImageGenerator may be used from any thread as long as calls are not concurrent;
// `FrameGrabber` is not `Sync` and its grabs take `&mut self`, so at most one thread calls into the
// generator at a time. Retain/release is atomic.
unsafe impl Send for FrameGrabber {}

/// `kCMTimeZero`.
fn cm_zero() -> CMTime {
    CMTime { value: 0, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 }
}

impl FrameGrabber {
    /// A generator for `path` whose images fit in `max_width` × `max_height` (aspect kept; 0 × 0
    /// means no cap, as `CGSizeZero` does). Opening never decodes: an unreadable file fails at
    /// the first [`FrameGrabber::grab`], as Swift's does.
    pub fn open(path: &Path, max_width: f64, max_height: f64) -> Result<FrameGrabber, AvError> {
        autoreleasepool(|_| {
            let asset = url_asset(path)?;
            // SAFETY: a valid asset.
            let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
            // SAFETY: plain property sets on a fresh generator, before any request.
            unsafe {
                generator.setAppliesPreferredTrackTransform(true);
                generator.setMaximumSize(CGSize { width: max_width, height: max_height });
                generator.setRequestedTimeToleranceBefore(cm_zero());
                generator.setRequestedTimeToleranceAfter(cm_zero());
            }
            let track_alpha = first_video_track(&asset).map(|t| description_facts(&t).alpha()).unwrap_or(false);
            Ok(FrameGrabber { generator, track_alpha })
        })
    }

    /// The first video track's format description states alpha (`ContainsAlphaChannel == true`
    /// or `Depth == 32`); false when there is no readable video track.
    pub fn track_alpha(&self) -> bool {
        self.track_alpha
    }

    /// The picture at `time` (`copyCGImage(at:actualTime:)`; with zero tolerance AVFoundation
    /// decodes the frame showing at that time). A failure is the generator's `NSError`.
    pub fn grab(&mut self, time: MediaTime, alpha: GrabAlpha) -> Result<GrabbedImage, AvError> {
        let timescale = i32::try_from(time.timescale).map_err(|_| AvError::PixelBuffer("time scale too large".into()))?;
        self.grab_cm(CMTime { value: time.value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }, alpha)
    }

    /// The picture at `CMTime(seconds:preferredTimescale:)` (`CMTimeMakeWithSeconds`) — Swift's
    /// walk-forward times, e.g. `(i / 15.0, 600)`.
    pub fn grab_seconds(&mut self, seconds: f64, preferred_timescale: i32, alpha: GrabAlpha) -> Result<GrabbedImage, AvError> {
        // SAFETY: CMTimeMakeWithSeconds on plain values.
        let t = unsafe { CMTime::with_seconds(seconds, preferred_timescale) };
        self.grab_cm(t, alpha)
    }

    fn grab_cm(&mut self, time: CMTime, alpha: GrabAlpha) -> Result<GrabbedImage, AvError> {
        autoreleasepool(|_| {
            let mut actual = cm_zero();
            // SAFETY: a configured generator; `actual` is a valid out-pointer for the call. The
            // synchronous call is Swift's `copyCGImage(at:actualTime:)` (deprecated, supported).
            #[allow(deprecated)]
            let image = unsafe { self.generator.copyCGImageAtTime_actualTime_error(time, &mut actual) }.map_err(|e| av_failed(&e))?;
            let (width, height, rgba) = draw_rgba(&image, alpha)?;
            let info = CGImage::alpha_info(Some(&image));
            let image_has_alpha = !matches!(info, CGImageAlphaInfo::None | CGImageAlphaInfo::NoneSkipFirst | CGImageAlphaInfo::NoneSkipLast);
            Ok(GrabbedImage { width, height, rgba, alpha_mode: alpha, image_has_alpha, actual_time: media_time(actual) })
        })
    }
}

/// `image` drawn into a device-RGB, 8-bit, `premultipliedLast` context of its own size (tight
/// rows), then made straight if asked.
#[allow(clippy::arithmetic_side_effects)]
fn draw_rgba(image: &CGImage, alpha: GrabAlpha) -> Result<(u32, u32, Vec<u8>), AvError> {
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let (Ok(width), Ok(height)) = (u32::try_from(w), u32::try_from(h)) else { return Err(AvError::PixelBuffer("image size too large".into())) };
    let Some(len) = w.checked_mul(h).and_then(|n| n.checked_mul(4)).filter(|n| *n > 0) else {
        return Err(AvError::PixelBuffer("image size is empty or too large".into()));
    };
    let mut rgba = vec![0u8; len];
    let space = CGColorSpace::new_device_rgb().ok_or_else(|| AvError::PixelBuffer("CGColorSpaceCreateDeviceRGB".into()))?;
    // SAFETY: `rgba` holds `w * 4 * h` writable bytes and outlives the context, which is dropped
    // before `rgba` is read or moved; 8 bits per component, RGBA premultiplied-last is a
    // supported bitmap-context format.
    let ctx = unsafe { CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), CGImageAlphaInfo::PremultipliedLast.0) }
        .ok_or_else(|| AvError::PixelBuffer("CGBitmapContextCreate".into()))?;
    CGContext::draw_image(Some(&ctx), CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } }, Some(image));
    drop(ctx);
    if alpha == GrabAlpha::Straight {
        for p in rgba.chunks_exact_mut(4) {
            if let [r, g, b, a] = p {
                let av = u32::from(*a);
                if av == 0 {
                    (*r, *g, *b) = (0, 0, 0);
                } else if av < 255 {
                    let un = |c: u8| u8::try_from(((u32::from(c) * 255 + av / 2) / av).min(255)).unwrap_or(255);
                    (*r, *g, *b) = (un(*r), un(*g), un(*b));
                }
            }
        }
    }
    Ok((width, height, rgba))
}
