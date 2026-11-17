//! A movie's first audio track as interleaved signed 32-bit little-endian PCM, exactly as Swift
//! GlEnc 1.6.2's `SourceAudioReader.readInterleavedPCM` reads it (glenc
//! `Sources/GlEncCore/AudioSupport.swift`): `AVAssetReaderTrackOutput` with LinearPCM, 32 bits,
//! integer, little-endian, interleaved, the source's own channel count, and `AVSampleRateKey` only
//! when a target rate is asked for (AVFoundation resamples then).
//!
//! Swift spills more than 256 MB of PCM to a memory-mapped temp file; here the PCM is one `Vec`
//! (the bytes are the same; a caller that needs the bound can stream instead).
//!
//! Lint note: none needed — sizes come from CoreMedia and are only compared or copied.

use crate::objc::{dict, number_bool, number_u32};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{AVAssetReader, AVAssetReaderTrackOutput, AVMediaTypeAudio, AVURLAsset};
use objc2_avf_audio::{
    AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsBigEndianKey, AVLinearPCMIsFloatKey, AVLinearPCMIsNonInterleaved, AVNumberOfChannelsKey,
    AVSampleRateKey,
};
use objc2_core_audio_types::kAudioFormatLinearPCM;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription};
use objc2_foundation::{NSNumber, NSString, NSURL};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;

/// Swift's `SourceAudioReader.maxChannels`.
pub const MAX_CHANNELS: i64 = 64;

/// Decoded audio: interleaved s32le PCM (`channels * 4` bytes per frame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcmAudio {
    pub rate: u32,
    pub channels: u32,
    pub pcm: Vec<u8>,
}

impl PcmAudio {
    /// Bytes per interleaved frame (Swift `AudioStreamInfo.bytesPerFrame`, 32-bit samples).
    pub fn bytes_per_frame(&self) -> usize {
        (self.channels as usize).saturating_mul(4)
    }
    /// Whole frames in `pcm`.
    pub fn frame_count(&self) -> usize {
        self.pcm.len().checked_div(self.bytes_per_frame()).unwrap_or(0)
    }
}

/// Swift's `SourceAudioError`, with its `description` texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PcmError {
    /// The path is not a readable UTF-8 file path (Rust-side; Swift takes a URL).
    Path(String),
    /// "SourceAudioReader: reader init failed: …".
    ReaderInitFailed(String),
    /// "SourceAudioReader: startReading() failed".
    ReaderStartFailed,
    /// "SourceAudioReader: source declares N audio channels (supported: 1…64)".
    UnsupportedChannelCount(i64),
    /// "SourceAudioReader: could not determine the source audio sample rate".
    UndeterminedSampleRate,
}

impl fmt::Display for PcmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcmError::Path(m) => f.write_str(m),
            PcmError::ReaderInitFailed(e) => write!(f, "SourceAudioReader: reader init failed: {e}"),
            PcmError::ReaderStartFailed => f.write_str("SourceAudioReader: startReading() failed"),
            PcmError::UnsupportedChannelCount(c) => write!(f, "SourceAudioReader: source declares {c} audio channels (supported: 1…{MAX_CHANNELS})"),
            PcmError::UndeterminedSampleRate => f.write_str("SourceAudioReader: could not determine the source audio sample rate"),
        }
    }
}

impl std::error::Error for PcmError {}

/// Swift's `resolveSampleRate(primary:trackNominal:hasNonEmptyPCM:)`.
pub fn resolve_sample_rate(primary: i64, track_nominal: i64, has_non_empty_pcm: bool) -> Result<i64, PcmError> {
    if primary > 0 {
        return Ok(primary);
    }
    if track_nominal > 0 {
        return Ok(track_nominal);
    }
    if has_non_empty_pcm {
        return Err(PcmError::UndeterminedSampleRate);
    }
    Ok(48000)
}

/// Swift's `Int(asbd.mSampleRate)`: truncation toward zero (NaN / out of range saturate here
/// instead of trapping).
fn int_of(rate: f64) -> i64 {
    rate as i64
}

/// The ASBD of a format description, as (channels, sample rate).
fn asbd_of(desc: &CMFormatDescription) -> Option<(u32, f64)> {
    // SAFETY: a valid format description; the call returns NULL for a non-audio description, else a
    // pointer into the description, valid while `desc` is alive (we copy the two fields out now).
    let p = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(desc) };
    // SAFETY: non-null per the check; points at an AudioStreamBasicDescription owned by `desc`.
    NonNull::new(p.cast_mut()).map(|p| unsafe { (p.as_ref().mChannelsPerFrame, p.as_ref().mSampleRate) })
}

/// Read the first audio track of `path` as interleaved s32le PCM at `target_rate` (None = the
/// source rate). `Ok(None)` when the movie has no audio track.
pub fn read_pcm_s32(path: &Path, target_rate: Option<u32>) -> Result<Option<PcmAudio>, PcmError> {
    let p = path.to_str().ok_or_else(|| PcmError::Path(format!("{} is not UTF-8", path.display())))?;
    if !path.is_file() {
        return Err(PcmError::Path(format!("input file not found: {p}")));
    }
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(p));
        // SAFETY: a valid file URL; no options (Swift: `AVURLAsset(url:)`).
        let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
        // SAFETY: a framework constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return Ok(None) };
        // SAFETY: synchronous property read on a valid asset (Swift's `loadTracks`).
        #[allow(deprecated)]
        let tracks = unsafe { asset.tracksWithMediaType(audio) };
        let Some(track) = tracks.firstObject() else { return Ok(None) };

        let mut channels: i64 = 2;
        let mut track_nominal: i64 = 0;
        // SAFETY: a property read on a valid track.
        let descriptions = unsafe { track.formatDescriptions() };
        if let Some(first) = descriptions.firstObject() {
            // SAFETY: `formatDescriptions` holds CMFormatDescriptions (CF objects in an NSArray);
            // `first` keeps the object alive while `desc` is used.
            let desc: &CMFormatDescription = unsafe { &*Retained::as_ptr(&first).cast::<CMFormatDescription>() };
            if let Some((ch, rate)) = asbd_of(desc) {
                channels = i64::from(ch);
                track_nominal = int_of(rate);
            }
        }
        if channels < 1 {
            channels = 1;
        }
        if !(1..=MAX_CHANNELS).contains(&channels) {
            return Err(PcmError::UnsupportedChannelCount(channels));
        }

        // SAFETY: framework constants (valid for the process).
        let keys = unsafe {
            (
                AVFormatIDKey,
                AVLinearPCMBitDepthKey,
                AVLinearPCMIsFloatKey,
                AVLinearPCMIsBigEndianKey,
                AVLinearPCMIsNonInterleaved,
                AVNumberOfChannelsKey,
                AVSampleRateKey,
            )
        };
        let (Some(k_fmt), Some(k_bits), Some(k_float), Some(k_be), Some(k_nonint), Some(k_ch), Some(k_rate)) = keys else {
            return Err(PcmError::ReaderStartFailed);
        };
        let v_fmt = number_u32(kAudioFormatLinearPCM);
        let v_bits = NSNumber::numberWithInteger(32);
        let (v_false, v_ch) = (number_bool(false), NSNumber::numberWithInteger(channels as isize));
        let v_rate = target_rate.map(|r| NSNumber::numberWithInteger(r as isize));
        let mut pairs: Vec<(&NSString, &AnyObject)> =
            vec![(k_fmt, &*v_fmt), (k_bits, &*v_bits), (k_float, &*v_false), (k_be, &*v_false), (k_nonint, &*v_false), (k_ch, &*v_ch)];
        if let Some(r) = &v_rate {
            pairs.push((k_rate, &**r));
        }
        let settings = dict(&pairs);

        // SAFETY: a valid asset; the error out-parameter becomes the Result.
        let reader =
            unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }.map_err(|e| PcmError::ReaderInitFailed(crate::bgra::ns_error_description(&e)))?;
        // SAFETY: the track belongs to this reader's asset; the settings are AVAudioSettings keys.
        let output = unsafe { AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(&track, Some(&settings)) };
        // SAFETY: valid reader and output of the same asset.
        if !unsafe { reader.canAddOutput(&output) } {
            return Err(PcmError::ReaderStartFailed);
        }
        // SAFETY: as above; added before startReading.
        unsafe { reader.addOutput(&output) };
        // SAFETY: a configured reader.
        if !unsafe { reader.startReading() } {
            return Err(PcmError::ReaderStartFailed);
        }

        let mut pcm: Vec<u8> = Vec::new();
        let mut resolved: i64 = target_rate.map_or(0, i64::from);
        loop {
            let more = autoreleasepool(|_| {
                // SAFETY: the output was added to a reader that started reading.
                let Some(sb) = (unsafe { output.copyNextSampleBuffer() }) else { return false };
                if resolved == 0 {
                    // SAFETY: a valid sample buffer.
                    if let Some(fmt) = unsafe { sb.format_description() }
                        && let Some((_, rate)) = asbd_of(&fmt)
                    {
                        resolved = int_of(rate);
                    }
                }
                // SAFETY: a valid sample buffer.
                if let Some(bb) = unsafe { sb.data_buffer() } {
                    // SAFETY: a valid block buffer.
                    let len = unsafe { bb.data_length() };
                    if len > 0 {
                        let start = pcm.len();
                        pcm.resize(start.saturating_add(len), 0);
                        if let Some(dst) = pcm.get_mut(start..).and_then(|d| NonNull::new(d.as_mut_ptr().cast())) {
                            // SAFETY: `dst` points at `len` writable bytes of `pcm` (just resized);
                            // CoreMedia copies exactly `len` bytes from offset 0 of the buffer.
                            let st = unsafe { bb.copy_data_bytes(0, len, dst) };
                            if st != 0 {
                                // Swift ignores the status (its zeroed temp array is appended).
                                if let Some(d) = pcm.get_mut(start..) {
                                    d.fill(0);
                                }
                            }
                        }
                    }
                }
                true
            });
            if !more {
                break;
            }
        }
        let rate = resolve_sample_rate(resolved, track_nominal, !pcm.is_empty())?;
        Ok(Some(PcmAudio { rate: u32::try_from(rate).unwrap_or(u32::MAX), channels: channels as u32, pcm }))
    })
}
