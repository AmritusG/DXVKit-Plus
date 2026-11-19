//! Encoding through `AVAssetWriter` (VideoToolbox: ProRes, H.264, HEVC, Motion JPEG), exactly as
//! Swift GlEnc 1.6.2's `AVAssetWriterVideoSink` does it (glenc `Sources/GlEncCore/FrameSink.swift`;
//! the settings each codec gets are `CoreEncoder.makePipeline`'s and `VideoEncodeSettings`'s).
//!
//! - [`AvWriter::create`]: an existing file is deleted; video settings = codec, width, height and
//!   — only when not empty — `AVVideoCompressionPropertiesKey` with exactly the [`CompressionProps`]
//!   that are set; `canApplyOutputSettings` checks; video input not real-time; a pixel-buffer
//!   adaptor whose source attributes are 32BGRA only; the optional audio input (`.mp4`: AAC, the
//!   rate, the channels, 256 000 bit/s; `.mov`: LPCM 32-bit signed little-endian interleaved) added
//!   before writing starts.
//! - [`AvWriter::append`]: the first frame starts writing and the session at its presentation time
//!   and feeds the WHOLE PCM as one sample buffer ([`make_s32le_pcm_sample_buffer`], Swift's
//!   `makeS32LEPCMSampleBuffer`), then marks the audio input finished (Swift does this so the two
//!   inputs never throttle each other). Every frame waits for the input as Swift's `waitReady`
//!   does (1 ms sleeps, at most 60 000) and is appended at its own time. The pixels are copied into
//!   an IOSurface-backed 32BGRA `CVPixelBuffer` made as GlEnc's `FrameResizer` makes its buffers
//!   (`kCVPixelBufferIOSurfacePropertiesKey: {}`) — what GlEnc appends whenever it resizes.
//! - [`AvWriter::finish`]: inputs marked finished, `finishWriting` waited for (a completion block
//!   and a channel — no run loop needed), the status must be completed. Returns the audio warning
//!   in Swift's wording, if any.
//!
//! Where Swift would raise an Objective-C exception (and crash) this returns an error instead:
//! appending while the input is still not ready after the wait (or after the writer failed),
//! more than 2 audio channels (AVFoundation requires `AVChannelLayoutKey` then, which GlEnc does
//! not set: "Missing required key AVChannelLayoutKey"), finishing before any frame or after the
//! writer failed, and a second start after a failed one.
//!
//! Lint note: `arithmetic_side_effects` is allowed for the row copies, bounded by the checked
//! frame size and the buffer's own stride.

use crate::bgra::{DecodedBuffer, FrameAttachments, swift_optional_error};
use crate::objc::{cmtime, dict, ns, number_bool, number_u32};
use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie, AVMediaTypeAudio,
    AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeAppleProRes422, AVVideoCodecTypeAppleProRes422HQ,
    AVVideoCodecTypeAppleProRes422LT, AVVideoCodecTypeAppleProRes422Proxy, AVVideoCodecTypeAppleProRes4444, AVVideoCodecTypeH264, AVVideoCodecTypeHEVC,
    AVVideoCodecTypeJPEG, AVVideoCompressionPropertiesKey, AVVideoHeightKey, AVVideoMaxKeyFrameIntervalKey, AVVideoProfileLevelH264BaselineAutoLevel,
    AVVideoProfileLevelH264HighAutoLevel, AVVideoProfileLevelH264MainAutoLevel, AVVideoProfileLevelKey, AVVideoQualityKey, AVVideoWidthKey,
};
use objc2_avf_audio::{
    AVEncoderBitRateKey, AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsBigEndianKey, AVLinearPCMIsFloatKey, AVLinearPCMIsNonInterleaved,
    AVNumberOfChannelsKey, AVSampleRateKey,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
use objc2_core_media::{CMAudioFormatDescriptionCreate, CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;

pub use crate::pcm::PcmAudio;

/// Swift `CoreEncoder.mjpegDefaultQuality`: the Motion JPEG sink's `AVVideoQualityKey`.
pub const MJPEG_DEFAULT_QUALITY: f64 = 0.85;
/// Swift's AAC bit rate for `.mp4` audio (`AVEncoderBitRateKey: 256_000`).
pub const AAC_BIT_RATE: isize = 256_000;
/// Swift `waitReady`: at most this many 1 ms sleeps.
const WAIT_SPINS: u32 = 60_000;

/// The VideoToolbox codecs GlEnc writes through `AVAssetWriter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AvCodec {
    ProRes422,
    ProRes422HQ,
    ProRes422LT,
    ProRes422Proxy,
    ProRes4444,
    H264,
    Hevc,
    /// Motion JPEG (`AVVideoCodecTypeJPEG`).
    Jpeg,
}

impl AvCodec {
    /// The QuickTime sample-entry FourCC the codec writes (Swift `ProResVariant.codecTag`; H.264
    /// `avc1`, HEVC `hvc1`, JPEG `jpeg`).
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            AvCodec::ProRes422 => *b"apcn",
            AvCodec::ProRes422HQ => *b"apch",
            AvCodec::ProRes422LT => *b"apcs",
            AvCodec::ProRes422Proxy => *b"apco",
            AvCodec::ProRes4444 => *b"ap4h",
            AvCodec::H264 => *b"avc1",
            AvCodec::Hevc => *b"hvc1",
            AvCodec::Jpeg => *b"jpeg",
        }
    }

    fn codec_type(self) -> Option<&'static NSString> {
        // SAFETY: framework constants (valid for the process).
        unsafe {
            match self {
                AvCodec::ProRes422 => AVVideoCodecTypeAppleProRes422,
                AvCodec::ProRes422HQ => AVVideoCodecTypeAppleProRes422HQ,
                AvCodec::ProRes422LT => AVVideoCodecTypeAppleProRes422LT,
                AvCodec::ProRes422Proxy => AVVideoCodecTypeAppleProRes422Proxy,
                AvCodec::ProRes4444 => AVVideoCodecTypeAppleProRes4444,
                AvCodec::H264 => AVVideoCodecTypeH264,
                AvCodec::Hevc => AVVideoCodecTypeHEVC,
                AvCodec::Jpeg => AVVideoCodecTypeJPEG,
            }
        }
    }
}

/// The output container (Swift `OutputContainer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AvContainer {
    /// QuickTime (`AVFileTypeQuickTimeMovie`, "com.apple.quicktime-movie"); audio as LPCM.
    Mov,
    /// MPEG-4 (`AVFileTypeMPEG4`, "public.mpeg-4"); audio as AAC.
    Mp4,
}

impl AvContainer {
    fn file_type(self) -> Option<&'static NSString> {
        // SAFETY: framework constants (valid for the process).
        unsafe {
            match self {
                AvContainer::Mov => AVFileTypeQuickTimeMovie,
                AvContainer::Mp4 => AVFileTypeMPEG4,
            }
        }
    }
    /// The file type's UTI (Swift `fileType.rawValue`).
    pub fn uti(self) -> &'static str {
        match self {
            AvContainer::Mov => "com.apple.quicktime-movie",
            AvContainer::Mp4 => "public.mpeg-4",
        }
    }
}

/// H.264 profile (Swift `VideoEncodeSettings.H264Profile`, auto level).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum H264Profile {
    Baseline,
    Main,
    High,
}

/// The `AVVideoCompressionPropertiesKey` payload: only the keys that are `Some` are written, and
/// the key itself is left out when none is (Swift: `if !compressionProperties.isEmpty`).
///
/// GlEnc's choices (`CoreEncoder.makePipeline`): ProRes none; H.264 / HEVC
/// `VideoEncodeSettings.compressionProperties` (quality 0.6 or a bit rate, the keyframe interval
/// when > 0, the profile for H.264 only, High by default); Motion JPEG quality
/// [`MJPEG_DEFAULT_QUALITY`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompressionProps {
    /// `AVVideoQualityKey` (0…1).
    pub quality: Option<f64>,
    /// `AVVideoAverageBitRateKey` (bit/s; Swift `Int(mbps * 1_000_000)`).
    pub average_bitrate: Option<i64>,
    /// `AVVideoMaxKeyFrameIntervalKey` (frames).
    pub max_keyframe_interval: Option<i64>,
    /// `AVVideoProfileLevelKey` = `AVVideoProfileLevelH264{Baseline,Main,High}AutoLevel`.
    pub h264_profile: Option<H264Profile>,
}

impl CompressionProps {
    fn is_empty(&self) -> bool {
        self.quality.is_none() && self.average_bitrate.is_none() && self.max_keyframe_interval.is_none() && self.h264_profile.is_none()
    }
}

/// Swift's `FrameSinkError`, with its `description` texts, plus the cases Swift crashes on.
#[derive(Clone, Debug, PartialEq)]
pub enum WriterError {
    /// The path is not a UTF-8 file path (Rust-side).
    Path(String),
    /// "AVAssetWriter init failed: Optional(…)".
    WriterInitFailed(String),
    /// "AVAssetWriter rejected codec/container: AVVideoCodecType(_rawValue: apcn) in com.apple.quicktime-movie".
    CodecRejected(String),
    /// "AVAssetWriter cannot add video input".
    CannotAddInput,
    /// "AVAssetWriter.startWriting failed: Optional(…)".
    StartWritingFailed(String),
    /// "pixel-buffer append failed at 0.0s" (seconds formatted as Swift prints a Double).
    AppendFailed { seconds: f64 },
    /// "AVAssetWriter.finishWriting status=3 err=Optional(…)".
    FinishFailed { status: isize, error: String },
    /// "AVAssetWriter rejected the audio input (96000 Hz in public.mpeg-4) — audio would be dropped".
    AudioInputRejected { rate: u32, container: &'static str },
    /// "audio sample buffer failed: …" (only ever inside an audio warning; kept for completeness).
    AudioSampleBufferFailed(String),
    /// A frame whose size is not `width * height * 4` bytes, or a pixel buffer that could not be
    /// made (Swift: `CVPixelBufferCreate failed`).
    Frame(String),
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriterError::Path(m) | WriterError::Frame(m) => f.write_str(m),
            WriterError::WriterInitFailed(e) => write!(f, "AVAssetWriter init failed: {e}"),
            WriterError::CodecRejected(s) => write!(f, "AVAssetWriter rejected codec/container: {s}"),
            WriterError::CannotAddInput => f.write_str("AVAssetWriter cannot add video input"),
            WriterError::StartWritingFailed(e) => write!(f, "AVAssetWriter.startWriting failed: {e}"),
            WriterError::AppendFailed { seconds } => write!(f, "pixel-buffer append failed at {seconds:?}s"),
            WriterError::FinishFailed { status, error } => write!(f, "AVAssetWriter.finishWriting status={status} err={error}"),
            WriterError::AudioInputRejected { rate, container } => {
                write!(f, "AVAssetWriter rejected the audio input ({rate} Hz in {container}) — audio would be dropped")
            }
            WriterError::AudioSampleBufferFailed(s) => write!(f, "audio sample buffer failed: {s}"),
        }
    }
}

impl std::error::Error for WriterError {}

fn missing(what: &str) -> WriterError {
    WriterError::WriterInitFailed(format!("nil ({what} missing)"))
}

/// One `CMSampleBuffer` holding all of `audio`'s interleaved s32le frames — Swift's
/// `makeS32LEPCMSampleBuffer`: LinearPCM signed-integer packed (little-endian), one packet per
/// frame, a block buffer of the whole PCM, one timing entry (duration 1/rate, pts 0, no dts) and
/// one size entry (bytes per frame). The error text is the reason Swift puts in
/// `FrameSinkError.audioSampleBufferFailed`.
pub fn make_s32le_pcm_sample_buffer(audio: &PcmAudio) -> Result<CFRetained<CMSampleBuffer>, WriterError> {
    let fail = |s: String| WriterError::AudioSampleBufferFailed(s);
    let bytes_per_frame = audio.bytes_per_frame();
    if bytes_per_frame == 0 || audio.pcm.is_empty() {
        return Err(fail("empty PCM or zero bytes-per-frame".into()));
    }
    let frame_count = audio.frame_count();
    if frame_count == 0 {
        return Err(fail("PCM shorter than one audio frame".into()));
    }
    let bpf = u32::try_from(bytes_per_frame).unwrap_or(u32::MAX);
    let mut asbd = AudioStreamBasicDescription {
        mSampleRate: f64::from(audio.rate),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bpf,
        mFramesPerPacket: 1,
        mBytesPerFrame: bpf,
        mChannelsPerFrame: audio.channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut fmt: *const CMFormatDescription = std::ptr::null();
    // SAFETY: `asbd` and `fmt` are valid for the call; no layout, cookie or extensions (NULL with
    // size 0, as Swift passes); the out-parameter receives a +1 format description.
    let st = unsafe { CMAudioFormatDescriptionCreate(None, NonNull::from(&mut asbd), 0, std::ptr::null(), 0, std::ptr::null(), None, NonNull::from(&mut fmt)) };
    let Some(fmt) = NonNull::new(fmt.cast_mut()).filter(|_| st == 0) else {
        return Err(fail(format!("CMAudioFormatDescriptionCreate rejected the format (channels={}, rate={})", audio.channels, audio.rate)));
    };
    // SAFETY: a +1 reference from the Create call above, released when `format` drops.
    let format = unsafe { CFRetained::from_raw(fmt) };

    let len = audio.pcm.len();
    let mut bb: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: CoreMedia allocates a `len`-byte block itself (null memory block, default
    // allocators, no custom source); the out-parameter receives a +1 block buffer.
    let st = unsafe { CMBlockBuffer::create_with_memory_block(None, std::ptr::null_mut(), len, None, std::ptr::null(), 0, len, 0, NonNull::from(&mut bb)) };
    let Some(bb) = NonNull::new(bb).filter(|_| st == 0) else {
        return Err(fail(format!("CMBlockBuffer allocation failed ({len} bytes)")));
    };
    // SAFETY: a +1 reference from the Create call above.
    let block = unsafe { CFRetained::from_raw(bb) };
    let src = NonNull::new(audio.pcm.as_ptr().cast_mut().cast()).ok_or_else(|| fail("CMBlockBuffer data copy failed".into()))?;
    // SAFETY: `src` points at `len` readable bytes (`audio.pcm`), copied into the block's own
    // `len`-byte memory; CoreMedia only reads the source.
    if unsafe { CMBlockBuffer::replace_data_bytes(src, &block, 0, len) } != 0 {
        return Err(fail("CMBlockBuffer data copy failed".into()));
    }

    let timing = CMSampleTimingInfo {
        duration: cmtime(1, i32::try_from(audio.rate).unwrap_or(i32::MAX)),
        presentationTimeStamp: CMTime { value: 0, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 },
        decodeTimeStamp: CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 },
    };
    let size_per_sample: usize = bytes_per_frame;
    let mut sb: *mut CMSampleBuffer = std::ptr::null_mut();
    // SAFETY: the block buffer and format description are valid; one timing entry and one size
    // entry, each pointing at a live local; the out-parameter receives a +1 sample buffer.
    let st = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(&format),
            isize::try_from(frame_count).unwrap_or(isize::MAX),
            1,
            &raw const timing,
            1,
            &raw const size_per_sample,
            NonNull::from(&mut sb),
        )
    };
    match NonNull::new(sb).filter(|_| st == 0) {
        // SAFETY: a +1 reference from the Create call above.
        Some(sb) => Ok(unsafe { CFRetained::from_raw(sb) }),
        None => Err(fail("CMSampleBufferCreateReady failed".into())),
    }
}

/// GlEnc's `AVAssetWriterVideoSink`.
pub struct AvWriter {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    audio_input: Option<Retained<AVAssetWriterInput>>,
    audio: Option<PcmAudio>,
    audio_finished: bool,
    audio_warning: Option<String>,
    started: bool,
    start_attempted: bool,
    finished: bool,
    width: u32,
    height: u32,
}

// SAFETY: AVAssetWriter and its inputs may be driven from any (one) thread at a time; `AvWriter`
// is only used through `&mut self` / `self` (no `Sync`), so calls are never concurrent. The finish
// completion block only signals a channel. Retain/release is atomic.
unsafe impl Send for AvWriter {}

fn int(v: i64) -> Retained<NSNumber> {
    NSNumber::numberWithInteger(isize::try_from(v).unwrap_or(isize::MAX))
}

impl AvWriter {
    /// Create the writer, its video input and (with non-empty `audio`) its audio input.
    pub fn create(
        path: &Path,
        codec: AvCodec,
        container: AvContainer,
        width: u32,
        height: u32,
        props: CompressionProps,
        audio: Option<PcmAudio>,
    ) -> Result<AvWriter, WriterError> {
        let p = path.to_str().ok_or_else(|| WriterError::Path(format!("{} is not UTF-8", path.display())))?;
        if path.exists() {
            let _ = std::fs::remove_file(path); // Swift: `try? removeItem`
        }
        autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(p));
            let file_type = container.file_type().ok_or_else(|| missing("AVFileType"))?;
            // SAFETY: a file URL and a declared file type; the error becomes the Result.
            let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type) }
                .map_err(|e| WriterError::WriterInitFailed(format!("Optional({})", crate::bgra::ns_error_description(&e))))?;

            // SAFETY: framework constants (valid for the process).
            let (k_codec, k_w, k_h, k_props) = unsafe { (AVVideoCodecKey, AVVideoWidthKey, AVVideoHeightKey, AVVideoCompressionPropertiesKey) };
            let (Some(k_codec), Some(k_w), Some(k_h), Some(k_props)) = (k_codec, k_w, k_h, k_props) else {
                return Err(missing("AVVideoSettings keys"));
            };
            let codec_type = codec.codec_type().ok_or_else(|| missing("AVVideoCodecType"))?;
            let (v_w, v_h) = (int(i64::from(width)), int(i64::from(height)));
            let props_dict = compression_dict(&props)?;
            let mut pairs: Vec<(&NSString, &AnyObject)> = vec![(k_codec, codec_type as &AnyObject), (k_w, &*v_w), (k_h, &*v_h)];
            if let Some(d) = &props_dict {
                pairs.push((k_props, &**d));
            }
            let settings = dict(&pairs);
            // SAFETY: a framework constant.
            let video = unsafe { AVMediaTypeVideo }.ok_or_else(|| missing("AVMediaTypeVideo"))?;
            // SAFETY: a valid writer; settings are AVVideoSettings keys.
            if !unsafe { writer.canApplyOutputSettings_forMediaType(Some(&settings), video) } {
                return Err(WriterError::CodecRejected(format!("AVVideoCodecType(_rawValue: {codec_type}) in {}", container.uti())));
            }
            // SAFETY: settings the writer accepts for video (checked above).
            let input = unsafe { AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(video, Some(&settings)) };
            // SAFETY: plain property set before writing starts.
            unsafe { input.setExpectsMediaDataInRealTime(false) };
            let fmt = number_u32(kCVPixelFormatType_32BGRA);
            // SAFETY: a framework constant.
            let k_fmt: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
            let attrs = dict(&[(ns(k_fmt), &*fmt as &AnyObject)]);
            // SAFETY: a fresh input not attached to any adaptor, writer not started.
            let adaptor = unsafe {
                AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&input, Some(&attrs))
            };
            // SAFETY: valid writer and input.
            if !unsafe { writer.canAddInput(&input) } {
                return Err(WriterError::CannotAddInput);
            }
            // SAFETY: checked above; before startWriting.
            unsafe { writer.addInput(&input) };

            let audio = audio.filter(|a| a.bytes_per_frame() > 0 && !a.pcm.is_empty());
            let audio_input = match &audio {
                Some(a) => Some(add_audio_input(&writer, container, a)?),
                None => None,
            };
            Ok(AvWriter {
                writer,
                input,
                adaptor,
                audio_input,
                audio,
                audio_finished: false,
                audio_warning: None,
                started: false,
                start_attempted: false,
                finished: false,
                width,
                height,
            })
        })
    }

    /// Swift `waitReady`: spin (1 ms) until the input is ready, the writer stops writing, or 60 000
    /// spins.
    fn wait_ready(&self, input: &AVAssetWriterInput) {
        let mut spins = 0u32;
        // SAFETY: property reads on a valid input / writer.
        while !unsafe { input.isReadyForMoreMediaData() } && unsafe { self.writer.status() } == AVAssetWriterStatus::Writing && spins < WAIT_SPINS {
            std::thread::sleep(std::time::Duration::from_millis(1));
            spins = spins.saturating_add(1);
        }
    }

    fn status(&self) -> isize {
        // SAFETY: a property read.
        unsafe { self.writer.status() }.0
    }

    /// Start writing at the first frame's time and feed the whole audio track (Swift `consume`'s
    /// first-frame branch).
    fn start(&mut self, pts: CMTime) -> Result<(), WriterError> {
        if self.start_attempted {
            // A second startWriting raises an exception; report the first failure again.
            // SAFETY: property read.
            return Err(WriterError::StartWritingFailed(swift_optional_error(unsafe { self.writer.error() })));
        }
        self.start_attempted = true;
        // SAFETY: a configured writer with its inputs added.
        if !unsafe { self.writer.startWriting() } {
            // SAFETY: property read.
            return Err(WriterError::StartWritingFailed(swift_optional_error(unsafe { self.writer.error() })));
        }
        // SAFETY: the writer is writing; a numeric time.
        unsafe { self.writer.startSessionAtSourceTime(pts) };
        self.started = true;
        if let (Some(ai), Some(audio)) = (self.audio_input.clone(), self.audio.as_ref()) {
            match make_s32le_pcm_sample_buffer(audio) {
                Ok(sb) => {
                    self.wait_ready(&ai);
                    // SAFETY: property read.
                    if unsafe { ai.isReadyForMoreMediaData() } {
                        // SAFETY: the input is attached, writing has started, and it is ready.
                        if !unsafe { ai.appendSampleBuffer(&sb) } {
                            self.audio_warning = Some(format!("Audio unavailable: writer rejected the audio (status {})", self.status()));
                        }
                    } else {
                        self.audio_warning = Some("Audio unavailable: audio input never became ready".into());
                    }
                }
                Err(e) => self.audio_warning = Some(format!("Audio unavailable: {e}")),
            }
            // SAFETY: the writer has started; marking an input finished once.
            unsafe { ai.markAsFinished() };
            self.audio_finished = true;
        }
        Ok(())
    }

    /// Append one frame: tight BGRA8 `width * height * 4` bytes at `pts_value / pts_timescale`, in a
    /// buffer without attachments — what GlEnc appends for a frame it rebuilt (resized, cropped,
    /// un-premultiplied, or decoded by its own DXV / HAP readers).
    pub fn append(&mut self, bgra: &[u8], pts_value: i64, pts_timescale: i32) -> Result<(), WriterError> {
        self.append_with_attachments(bgra, pts_value, pts_timescale, None)
    }

    /// [`AvWriter::append`] with the decoded buffer's attachments ([`crate::bgra::BgraFrame`]'s):
    /// what GlEnc appends when it passes the decoder's own buffer through (no resize / crop). The
    /// encoder reads them (measured: without them ProRes gets no `colr` atom and different
    /// coefficients).
    #[allow(clippy::arithmetic_side_effects)]
    pub fn append_with_attachments(
        &mut self,
        bgra: &[u8],
        pts_value: i64,
        pts_timescale: i32,
        attachments: Option<&FrameAttachments>,
    ) -> Result<(), WriterError> {
        let (w, h) = (self.width as usize, self.height as usize);
        let row = w.saturating_mul(4);
        if bgra.len() != row.saturating_mul(h) || w == 0 || h == 0 {
            return Err(WriterError::Frame(format!("a {}×{} BGRA frame is {} bytes, got {}", self.width, self.height, row.saturating_mul(h), bgra.len())));
        }
        let pts = cmtime(pts_value, pts_timescale);
        let seconds = if pts_timescale > 0 { pts_value as f64 / f64::from(pts_timescale) } else { f64::NAN };
        autoreleasepool(|_| {
            if !self.started {
                self.start(pts)?;
            }
            let pb = make_bgra_buffer(w, h)?;
            if let Some(a) = attachments {
                a.apply(&pb);
            }
            // SAFETY: a buffer we just created; locked for writing, unlocked below on every path.
            let r = unsafe { CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0)) };
            if r != 0 {
                return Err(WriterError::Frame(format!("CVPixelBufferLockBaseAddress failed ({r})")));
            }
            let base = CVPixelBufferGetBaseAddress(&pb);
            let stride = CVPixelBufferGetBytesPerRow(&pb);
            let ok = !base.is_null() && stride >= row;
            if ok {
                // SAFETY: while locked, `base` points at `stride * h` writable bytes owned by the
                // buffer (32BGRA, `h` rows of at least `row` bytes); nothing else references them.
                let dst = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), stride * (h - 1) + row) };
                for (d, s) in dst.chunks_mut(stride).zip(bgra.chunks_exact(row)) {
                    if let Some(d) = d.get_mut(..row) {
                        d.copy_from_slice(s);
                    }
                }
            }
            // SAFETY: matches the lock above.
            unsafe { CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0)) };
            if !ok {
                return Err(WriterError::Frame("pixel buffer has no CPU base address".into()));
            }
            self.append_pixel_buffer(&pb, pts, seconds)
        })
    }

    /// Append the decoder's own buffer as it is, attachments included (0.15.1): Swift GlEnc's pass-through, for a
    /// frame it neither crops, resizes nor un-premultiplies. No copy. The buffer must be the output's size.
    pub fn append_buffer(&mut self, frame: &DecodedBuffer) -> Result<(), WriterError> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(WriterError::Frame(format!("a {}×{} buffer for a {}×{} output", frame.width, frame.height, self.width, self.height)));
        }
        let pts = cmtime(frame.pts_value, frame.pts_timescale);
        let seconds = if frame.pts_timescale > 0 { frame.pts_value as f64 / f64::from(frame.pts_timescale) } else { f64::NAN };
        autoreleasepool(|_| {
            if !self.started {
                self.start(pts)?;
            }
            self.append_pixel_buffer(&frame.buffer, pts, seconds)
        })
    }

    /// The common tail of an append: wait for the input, then append `pb` at `pts`.
    fn append_pixel_buffer(&mut self, pb: &CVPixelBuffer, pts: CMTime, seconds: f64) -> Result<(), WriterError> {
        self.wait_ready(&self.input);
        // Swift appends unconditionally here; AVFoundation raises an exception when the input
        // is not ready (a failed writer, or the wait timed out). That is an error here.
        // SAFETY: property read.
        if !unsafe { self.input.isReadyForMoreMediaData() } {
            return Err(WriterError::AppendFailed { seconds });
        }
        // SAFETY: the input is attached, the session started and the input is ready; the
        // buffer is a 32BGRA buffer of the output size, not written to after this call.
        if !unsafe { self.adaptor.appendPixelBuffer_withPresentationTime(pb, pts) } {
            return Err(WriterError::AppendFailed { seconds });
        }
        Ok(())
    }

    /// Finish the file; the audio warning (Swift's wording) if the audio could not be written.
    pub fn finish(mut self) -> Result<Option<String>, WriterError> {
        self.finished = true;
        if !self.started {
            // Swift would call finishWriting on a writer that never started (an exception);
            // the status it would report is the writer's own (Unknown = 0).
            return Err(WriterError::FinishFailed { status: self.status(), error: "nil".into() });
        }
        // SAFETY: property read.
        if unsafe { self.writer.status() } != AVAssetWriterStatus::Writing {
            // A writer that already failed: finishWriting would raise "Cannot call method when
            // status is 3"; report the status and error Swift's check after it would.
            // SAFETY: property read.
            let error = swift_optional_error(unsafe { self.writer.error() });
            return Err(WriterError::FinishFailed { status: self.status(), error });
        }
        if let Some(ai) = &self.audio_input
            && !self.audio_finished
        {
            // SAFETY: the writer has started; marking an input finished once.
            unsafe { ai.markAsFinished() };
        }
        // SAFETY: as above.
        unsafe { self.input.markAsFinished() };
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let block = RcBlock::new(move || {
            let _ = tx.send(());
        });
        // SAFETY: the block is `'static`, `Send`-safe (it only owns a channel sender) and called
        // once by AVFoundation on a queue of its own; the writer is writing.
        unsafe { self.writer.finishWritingWithCompletionHandler(&block) };
        let _ = rx.recv(); // a dropped block (no call) also ends the wait
        // SAFETY: property read.
        if unsafe { self.writer.status() } != AVAssetWriterStatus::Completed {
            // SAFETY: property read.
            let error = swift_optional_error(unsafe { self.writer.error() });
            return Err(WriterError::FinishFailed { status: self.status(), error });
        }
        Ok(self.audio_warning.take())
    }
}

impl Drop for AvWriter {
    fn drop(&mut self) {
        // SAFETY: property read; cancelling a writer that is still writing (not finished) is the
        // documented way to abandon it.
        if !self.finished && unsafe { self.writer.status() } == AVAssetWriterStatus::Writing {
            // SAFETY: as above.
            unsafe { self.writer.cancelWriting() };
        }
    }
}

/// The compression-properties dictionary, or `None` when no property is set.
fn compression_dict(props: &CompressionProps) -> Result<Option<Retained<NSDictionary<NSString, AnyObject>>>, WriterError> {
    if props.is_empty() {
        return Ok(None);
    }
    // SAFETY: framework constants (valid for the process).
    let (k_q, k_br, k_kf, k_prof) = unsafe { (AVVideoQualityKey, AVVideoAverageBitRateKey, AVVideoMaxKeyFrameIntervalKey, AVVideoProfileLevelKey) };
    let (Some(k_q), Some(k_br), Some(k_kf), Some(k_prof)) = (k_q, k_br, k_kf, k_prof) else {
        return Err(missing("AVVideoSettings compression keys"));
    };
    let q = props.quality.map(NSNumber::numberWithDouble);
    let br = props.average_bitrate.map(int);
    let kf = props.max_keyframe_interval.map(int);
    // SAFETY: framework constants.
    let profile = props.h264_profile.map(|p| unsafe {
        match p {
            H264Profile::Baseline => AVVideoProfileLevelH264BaselineAutoLevel,
            H264Profile::Main => AVVideoProfileLevelH264MainAutoLevel,
            H264Profile::High => AVVideoProfileLevelH264HighAutoLevel,
        }
    });
    let mut pairs: Vec<(&NSString, &AnyObject)> = Vec::new();
    if let Some(v) = &q {
        pairs.push((k_q, &**v));
    }
    if let Some(v) = &br {
        pairs.push((k_br, &**v));
    }
    if let Some(v) = &kf {
        pairs.push((k_kf, &**v));
    }
    if let Some(v) = profile {
        pairs.push((k_prof, v.ok_or_else(|| missing("AVVideoProfileLevelH264"))?));
    }
    Ok(Some(dict(&pairs)))
}

/// The audio input Swift adds for `.mp4` (AAC) or `.mov` (LPCM s32le interleaved).
fn add_audio_input(writer: &AVAssetWriter, container: AvContainer, audio: &PcmAudio) -> Result<Retained<AVAssetWriterInput>, WriterError> {
    let rejected = WriterError::AudioInputRejected { rate: audio.rate, container: container.uti() };
    // SAFETY: framework constants (valid for the process).
    let keys = unsafe {
        (
            AVFormatIDKey,
            AVSampleRateKey,
            AVNumberOfChannelsKey,
            AVEncoderBitRateKey,
            AVLinearPCMBitDepthKey,
            AVLinearPCMIsFloatKey,
            AVLinearPCMIsBigEndianKey,
            AVLinearPCMIsNonInterleaved,
        )
    };
    let (Some(k_fmt), Some(k_rate), Some(k_ch), Some(k_br), Some(k_bits), Some(k_float), Some(k_be), Some(k_nonint)) = keys else {
        return Err(missing("AVAudioSettings keys"));
    };
    let v_rate = int(i64::from(audio.rate));
    let v_ch = int(i64::from(audio.channels));
    let v_aac = number_u32(kAudioFormatMPEG4AAC);
    let v_lpcm = number_u32(kAudioFormatLinearPCM);
    let v_br = NSNumber::numberWithInteger(AAC_BIT_RATE);
    let v_bits = int(32);
    let v_false = number_bool(false);
    let settings = match container {
        AvContainer::Mp4 => dict(&[(k_fmt, &*v_aac as &AnyObject), (k_rate, &*v_rate), (k_ch, &*v_ch), (k_br, &*v_br)]),
        AvContainer::Mov => dict(&[
            (k_fmt, &*v_lpcm as &AnyObject),
            (k_rate, &*v_rate),
            (k_ch, &*v_ch),
            (k_bits, &*v_bits),
            (k_float, &*v_false),
            (k_be, &*v_false),
            (k_nonint, &*v_false),
        ]),
    };
    // SAFETY: a framework constant.
    let audio_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| missing("AVMediaTypeAudio"))?;
    // SAFETY: a valid writer; settings are AVAudioSettings keys.
    if !unsafe { writer.canApplyOutputSettings_forMediaType(Some(&settings), audio_type) } {
        return Err(rejected);
    }
    // More than two channels without AVChannelLayoutKey: AVAssetWriterInput's initializer raises
    // "Missing required key AVChannelLayoutKey" (measured; GlEnc 1.6.2 crashes there).
    if audio.channels > 2 {
        return Err(rejected);
    }
    // SAFETY: settings the writer accepts for audio (checked above), fully specified.
    let ai = unsafe { AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(audio_type, Some(&settings)) };
    // SAFETY: plain property set before writing starts.
    unsafe { ai.setExpectsMediaDataInRealTime(false) };
    // SAFETY: valid writer and input.
    if !unsafe { writer.canAddInput(&ai) } {
        return Err(rejected);
    }
    // SAFETY: checked above; before startWriting.
    unsafe { writer.addInput(&ai) };
    Ok(ai)
}

/// An IOSurface-backed 32BGRA pixel buffer, as GlEnc's `FrameResizer.makeBGRABuffer` makes one.
fn make_bgra_buffer(w: usize, h: usize) -> Result<CFRetained<CVPixelBuffer>, WriterError> {
    let empty: Retained<NSDictionary<NSString, AnyObject>> = dict(&[]);
    // SAFETY: a framework constant.
    let k_ios: &CFString = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attrs = dict(&[(ns(k_ios), &*empty as &AnyObject)]);
    // SAFETY: NSDictionary and CFDictionary are toll-free bridged; `attrs` outlives the call.
    let cf: &CFDictionary = unsafe { &*Retained::as_ptr(&attrs).cast::<CFDictionary>() };
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid size, format and attributes; the out-parameter receives a +1 buffer.
    let st = unsafe { CVPixelBufferCreate(None, w, h, kCVPixelFormatType_32BGRA, Some(cf), NonNull::from(&mut out)) };
    match NonNull::new(out).filter(|_| st == 0) {
        // SAFETY: a +1 reference from CVPixelBufferCreate.
        Some(p) => Ok(unsafe { CFRetained::from_raw(p) }),
        None => Err(WriterError::Frame(format!("CVPixelBufferCreate failed ({st})"))),
    }
}
