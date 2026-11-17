//! Sequential CPU frames of a movie's first video track: plane 0 of each decoded buffer — the
//! BGRA plane, or the Y plane of 4:2:0 — with its own row stride, as an `AVAssetReader` serves
//! them.
//!
//! The settings are exactly Swift MaCrate 0.16.4's measurement reader (`AVFoundationSource`): a
//! plain `AVURLAsset`, the first video track (loaded asynchronously, so a failure is Swift's
//! `NSError`), one `AVAssetReaderTrackOutput` whose settings are ONLY
//! `kCVPixelBufferPixelFormatTypeKey` ([`PlaneFormat`]), `alwaysCopiesSampleData = false`, no time
//! range, read from the start in the order AVFoundation serves the samples. Samples without an
//! image buffer are skipped. The bytes are the buffer's own rows, padding included
//! (`row_bytes * height` bytes), so a caller sees what Swift's `CVPixelBufferGetBaseAddress…`
//! loop sees.
//!
//! Unlike [`crate::bgra::BgraReader`] (GlEnc's encode source: BGRA only, rows made tight, the
//! buffer's attachments) this reader also serves the luma plane and keeps the stride; unlike
//! [`crate::decode::NativeReader`] (playback, GPU handles) it never asks for IOSurface or Metal
//! buffers.
//!
//! Lint note: `arithmetic_side_effects` is allowed in this module for the row / size product,
//! checked before any slice is formed.

use crate::objc::{dict, ns, number_u32};
use crate::track::{AvError, av_failed, error_description, first_video_track, url_asset, valid_or_zero};
use dxvkit_plus_core::timing::MediaTime;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput};
use objc2_core_foundation::CFString;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use std::path::Path;

/// The pixel format the track output is asked for (`kCVPixelBufferPixelFormatTypeKey`).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlaneFormat {
    /// `kCVPixelFormatType_32BGRA`: plane 0 is the BGRA picture.
    Bgra = 0,
    /// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` (`420v`): plane 0 is Y.
    Yuv420VideoRange = 1,
    /// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` (`420f`): plane 0 is Y.
    Yuv420FullRange = 2,
}

impl PlaneFormat {
    fn cv_format(self) -> u32 {
        match self {
            PlaneFormat::Bgra => kCVPixelFormatType_32BGRA,
            PlaneFormat::Yuv420VideoRange => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            PlaneFormat::Yuv420FullRange => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        }
    }
}

/// Where a frame's plane 0 is and how it is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneInfo {
    /// The sample's presentation time stamp (zero when it is not valid).
    pub pts: MediaTime,
    /// Plane 0's size in pixels (the buffer's own size for BGRA).
    pub width: usize,
    pub height: usize,
    /// Plane 0's bytes per row, padding included; the data is `row_bytes * height` bytes.
    pub row_bytes: usize,
}

/// One frame: [`PlaneInfo`] and plane 0's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaneFrame {
    pub info: PlaneInfo,
    pub data: Vec<u8>,
}

/// The sequential plane-0 reader. Not `Sync`; use from one thread at a time.
pub struct PlaneReader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    done: bool,
}

// SAFETY: AVAssetReader / AVAssetReaderTrackOutput may be used from any thread as long as calls are
// not concurrent; `PlaneReader` is only reachable through `&mut self` for reading (no `Sync`), so
// at most one thread calls into these objects at a time. Retain/release is atomic.
unsafe impl Send for PlaneReader {}

impl PlaneReader {
    /// Open `path` and start reading its first video track from the beginning in `format`.
    pub fn open(path: &Path, format: PlaneFormat) -> Result<PlaneReader, AvError> {
        autoreleasepool(|_| {
            let asset = url_asset(path)?;
            let track = first_video_track(&asset)?;
            // SAFETY: a valid asset; the error out-parameter becomes the Result.
            let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }.map_err(|e| av_failed(&e))?;
            let fmt = number_u32(format.cv_format());
            // SAFETY: a framework constant (valid for the process).
            let k_fmt: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
            let settings = dict(&[(ns(k_fmt), &*fmt as &AnyObject)]);
            // SAFETY: the track belongs to this reader's asset; the settings are one CoreVideo key.
            let output = unsafe { AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&settings)) };
            // SAFETY: a plain property set before reading starts.
            unsafe { output.setAlwaysCopiesSampleData(false) };
            // SAFETY: a valid reader and an output of the same asset. (Swift adds without asking;
            // asking first turns AVFoundation's exception for an unaddable output into an error.)
            if !unsafe { reader.canAddOutput(&output) } {
                return Err(AvError::StartFailed(None));
            }
            // SAFETY: as above; added before startReading.
            unsafe { reader.addOutput(&output) };
            // SAFETY: a configured reader.
            if !unsafe { reader.startReading() } {
                // SAFETY: a property read.
                return Err(AvError::StartFailed(unsafe { reader.error() }.map(|e| error_description(&e))));
            }
            Ok(PlaneReader { reader, output, done: false })
        })
    }

    /// The next frame into `data` (cleared first; its capacity is reused), or `None` at the end of
    /// the stream. At the end, a reader whose status is `.failed` is [`AvError::ReadFailed`].
    pub fn next_frame_into(&mut self, data: &mut Vec<u8>) -> Result<Option<PlaneInfo>, AvError> {
        data.clear();
        while !self.done {
            let step = autoreleasepool(|_| -> Result<Option<Option<PlaneInfo>>, AvError> {
                // SAFETY: the output was added to a reader that started reading, and is not asked
                // again once it returned no sample.
                let Some(sample) = (unsafe { self.output.copyNextSampleBuffer() }) else { return Ok(None) };
                // SAFETY: a valid sample buffer.
                let pts = valid_or_zero(unsafe { sample.presentation_time_stamp() });
                // SAFETY: as above; a +1 retained image buffer or None.
                let Some(image) = (unsafe { sample.image_buffer() }) else { return Ok(Some(None)) };
                let (width, height, row_bytes) = copy_plane0(&image, data)?;
                Ok(Some(Some(PlaneInfo { pts, width, height, row_bytes })))
            })?;
            match step {
                None => self.done = true,
                Some(None) => {}
                Some(Some(info)) => return Ok(Some(info)),
            }
        }
        self.failure().map_or(Ok(None), Err)
    }

    /// The next frame, or `None` at the end of the stream (allocates; see
    /// [`PlaneReader::next_frame_into`]).
    pub fn next_frame(&mut self) -> Result<Option<PlaneFrame>, AvError> {
        let mut data = Vec::new();
        Ok(self.next_frame_into(&mut data)?.map(|info| PlaneFrame { info, data }))
    }

    /// Stop reading: [`AvError::ReadFailed`] when the reader's status is `.failed`, as a reader
    /// that ran to the end reports it. For a caller that stops before the end.
    pub fn finish(self) -> Result<(), AvError> {
        self.failure().map_or(Ok(()), Err)
    }

    fn failure(&self) -> Option<AvError> {
        // SAFETY: a property read on a valid reader.
        if unsafe { self.reader.status() } != AVAssetReaderStatus::Failed {
            return None;
        }
        // SAFETY: a property read.
        Some(AvError::ReadFailed(unsafe { self.reader.error() }.map(|e| error_description(&e))))
    }
}

impl Drop for PlaneReader {
    fn drop(&mut self) {
        // SAFETY: cancelling a reader this value owns (harmless when it already completed).
        unsafe { self.reader.cancelReading() };
    }
}

/// Plane 0 of `pb` (the Y plane of a planar buffer, else the whole buffer) copied into `out` with
/// its stride: (width, height, bytes per row).
#[allow(clippy::arithmetic_side_effects)]
fn copy_plane0(pb: &CVPixelBuffer, out: &mut Vec<u8>) -> Result<(usize, usize, usize), AvError> {
    // SAFETY: a valid pixel buffer; read-only lock, released below on every path.
    if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return Err(AvError::PixelBuffer("CVPixelBufferLockBaseAddress".into()));
    }
    let (w, h, rb, base) = if CVPixelBufferIsPlanar(pb) {
        (
            CVPixelBufferGetWidthOfPlane(pb, 0),
            CVPixelBufferGetHeightOfPlane(pb, 0),
            CVPixelBufferGetBytesPerRowOfPlane(pb, 0),
            CVPixelBufferGetBaseAddressOfPlane(pb, 0),
        )
    } else {
        (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb), CVPixelBufferGetBytesPerRow(pb), CVPixelBufferGetBaseAddress(pb))
    };
    let r = match rb.checked_mul(h) {
        _ if base.is_null() => Err(AvError::PixelBuffer("no base address".into())),
        None => Err(AvError::PixelBuffer("plane size overflows".into())),
        Some(len) => {
            // SAFETY: while locked, `base` points at plane 0's `bytes_per_row * height` readable
            // bytes, owned by the buffer; they are only read, and only until the unlock below.
            let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) };
            out.extend_from_slice(bytes);
            Ok((w, h, rb))
        }
    };
    // SAFETY: matches the lock above with the same flags.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    r
}
