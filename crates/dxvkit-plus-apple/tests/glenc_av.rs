//! Swift GlEnc 1.6.2's AVFoundation / Accelerate pieces, mirrored: `BgraReader`
//! (`AVAssetReaderSourceReader`), `read_pcm_s32` (`SourceAudioReader.readInterleavedPCM`),
//! `AvWriter` (`AVAssetWriterVideoSink`) and `scale_bgra` (`FrameResizer`'s vImage path).
//!
//! No private media: the committed fixtures (`tests/fixtures/`, the `tiles` pattern) and movies
//! this test writes itself with `AvWriter` into cargo's per-target temp dir.
//!
//! Golden hashes (FNV-1a 64 over the bytes) were produced by Swift scripts that call AVFoundation /
//! vImage exactly as GlEnc does (`bgra_ref.swift`, `pcm_ref.swift`, `scale_ref.swift`; GlEnc's
//! reader settings, `readInterleavedPCM`'s settings, `vImageScale_ARGB8888` on IOSurface-backed
//! `CVPixelBuffer`s), on macOS 27.0 (Darwin 27.0.0, Apple silicon), 2026-11-17. Decoder output can
//! change with an OS update (GlEnc saw ProRes 4444 move by 1 LSB on macOS 26.6.2); a golden that
//! fails after one needs the Swift script re-run, not a code change. Encoder output (VideoToolbox)
//! and resampled audio are not pinned: H.264 / HEVC and AVFoundation's sample-rate converter vary
//! run to run in Swift GlEnc itself (measured), so those are checked structurally.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_apple::pcm::resolve_sample_rate;
use dxvkit_plus_apple::scale::scale_bgra_strided;
use dxvkit_plus_apple::writer::make_s32le_pcm_sample_buffer;
use dxvkit_plus_apple::{
    AvCodec, AvContainer, AvWriter, BgraReader, CompressionProps, H264Profile, MJPEG_DEFAULT_QUALITY, PcmAudio, PcmError, ScaleError, ScaleFilter,
    SourceReaderError, WriterError, read_pcm_s32, scale_bgra,
};
use dxvkit_plus_core::testpattern::{read_frame_id, tiles};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name)
}

fn out(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("glenc_av");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    rgba_to_bgra(bgra) // the swap is its own inverse
}

/// Every frame of a movie through `BgraReader`: (pts value, timescale, tight BGRA).
fn read_all(path: &Path) -> (BgraReader, Vec<(i64, i32, Vec<u8>)>) {
    let mut r = BgraReader::open(path).unwrap();
    let mut frames = Vec::new();
    while let Some(f) = r.next_frame().unwrap() {
        assert_eq!((f.width, f.height), (r.width(), r.height()));
        frames.push((f.pts_value, f.pts_timescale, f.data));
    }
    (r, frames)
}

// ---------------------------------------------------------------------------------------------
// BgraReader

#[test]
fn bgra_reader_serves_what_glencs_avassetreader_serves() {
    // (fixture, frames, FNV of all frames' tight BGRA as Swift's bgra_ref.swift read them, alpha)
    for (name, count, golden, alpha) in [
        ("tiles_h264.mp4", 24, 0x8d60_52fb_0320_0ea5u64, None),
        ("tiles_hevc.mp4", 24, 0x9e41_8cd1_065d_93c0, None),
        ("tiles_prores422.mov", 24, 0xa758_3afc_275a_6e85, None),
        ("tiles_prores4444.mov", 24, 0xf8d7_9341_c873_daa5, Some(true)),
        ("tiles_mjpeg.mov", 24, 0xdf2e_d949_82b0_5688, None),
        ("jitter_prores4444_30.mov", 24, 0x2c54_cff4_6e13_c825, Some(true)),
        ("edit_h264_bframes.mov", 18, 0x9540_4f2a_c226_62cc, None),
    ] {
        let (r, frames) = read_all(&fixture(name));
        assert_eq!((r.width(), r.height(), frames.len()), (128, 72, count), "{name}");
        assert_eq!(r.alpha().contains_alpha_channel, alpha, "{name}");
        assert!(!r.alpha().premultiplied, "{name}");
        let all: Vec<u8> = frames.iter().flat_map(|f| f.2.iter().copied()).collect();
        assert_eq!(fnv(&all), golden, "{name}: frames differ from Swift GlEnc's reader");
        // Presentation times are the samples' own (the jittered fixture's 61/600, 201/600, …).
        assert!(frames.iter().all(|f| f.1 == 600), "{name}");
        // Frame order: the pattern's stamped frame index, consecutive (the edit-list fixture's
        // edit starts at source frame 3).
        let first = read_frame_id(&bgra_to_rgba(&frames[0].2), 128, 72).unwrap();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(read_frame_id(&bgra_to_rgba(&f.2), 128, 72), Some(first + i as u32), "{name} frame {i}");
        }
    }
    let (_, jitter) = read_all(&fixture("jitter_prores4444_30.mov"));
    assert_eq!(jitter[3].0, 61);
    assert_eq!(jitter[10].0, 201);
}

#[test]
fn bgra_reader_matches_the_playback_reader_pixels() {
    // The playback reader asks for IOSurface + Metal-compatible buffers; GlEnc's reader for 32BGRA
    // only. The pixels are the same (measured on 1 948 frames of 18 movies; here the alpha fixture).
    use dxvkit_plus_core::backend::{NativeDecoder, PixelLayout};
    let path = fixture("tiles_prores4444.mov");
    let (_, frames) = read_all(&path);
    let mut native = dxvkit_plus_apple::NativeReader::open(&path, PixelLayout::Bgra).unwrap();
    for (i, f) in frames.iter().enumerate() {
        let nf = native.frame(i as u32).unwrap();
        assert_eq!(rgba_to_bgra(&native.cpu_rgba(&nf).unwrap()), f.2, "frame {i}");
    }
}

#[test]
fn bgra_reader_errors_read_as_glencs() {
    assert_eq!(SourceReaderError::NoVideoTrack.to_string(), "source has no video track");
    let e = BgraReader::open(&fixture("no_such_file.mov")).err().unwrap();
    assert!(e.to_string().starts_with("input file not found:"), "{e}");
    // HAP video: AVFoundation has no decoder (GlEnc routes HAP to its own reader).
    let e = BgraReader::open(&fixture("tiles_hap1.mov")).err().unwrap();
    assert!(matches!(e, SourceReaderError::AvAssetReaderStartFailed(_)), "{e:?}");
    assert!(e.to_string().starts_with("AVAssetReader.startReading failed: Optional(Error Domain=AVFoundationErrorDomain Code=-11833"), "{e}");
    assert_eq!(
        SourceReaderError::SourceDimensionsTooLarge { width: 70000, height: 10 }.to_string(),
        "source dimensions 70000×10 exceed the maximum encodable size of 65535 pixels per side"
    );
}

// ---------------------------------------------------------------------------------------------
// read_pcm_s32

#[test]
fn pcm_reads_as_glencs_read_interleaved_pcm() {
    // (fixture, rate, channels, bytes, FNV of the PCM as Swift's pcm_ref.swift read it at the
    // source rate; deterministic, unlike a resample)
    for (name, rate, channels, len, golden) in [
        ("audio_pcm_edits.mov", 44100, 1, 11876, 0x036d_711c_5863_3377u64),
        ("audio_aac_priming.mov", 44100, 2, 176_400, 0x99bb_5521_cc1f_6908),
        ("audio_aac_noroll.mov", 44100, 2, 162_816, 0x6239_d4cd_07e2_56f1),
        ("audio_aac_trim.mp4", 44100, 2, 91728, 0x250d_a651_bcdd_a773),
        ("audio_pcm_formats.mov", 48000, 2, 8064, 0x1bf8_2c0d_fdf5_7b45),
    ] {
        let a = read_pcm_s32(&fixture(name), None).unwrap().unwrap();
        assert_eq!((a.rate, a.channels, a.pcm.len()), (rate, channels, len), "{name}");
        assert_eq!(fnv(&a.pcm), golden, "{name}: PCM differs from Swift GlEnc's reader");
        // Asking for the source rate is the same read.
        assert_eq!(read_pcm_s32(&fixture(name), Some(rate)).unwrap().unwrap(), a, "{name}");
    }
    // A resample: AVFoundation's converter is not run-to-run deterministic (in Swift too), so the
    // format and the length (within one converter block) are checked, not the bytes.
    let a = read_pcm_s32(&fixture("audio_aac_priming.mov"), Some(48000)).unwrap().unwrap();
    assert_eq!((a.rate, a.channels), (48000, 2));
    let frames = a.frame_count() as i64;
    assert!((frames - 22050 * 48000 / 44100).abs() < 2048, "{frames}"); // 22 050 source frames
    // No audio track: None (not an error).
    assert_eq!(read_pcm_s32(&fixture("tiles_h264.mp4"), None).unwrap(), None);
}

#[test]
fn pcm_errors_and_rate_rules_read_as_glencs() {
    assert_eq!(PcmError::UnsupportedChannelCount(4_294_967_295).to_string(), "SourceAudioReader: source declares 4294967295 audio channels (supported: 1…64)");
    assert_eq!(PcmError::ReaderStartFailed.to_string(), "SourceAudioReader: startReading() failed");
    assert_eq!(PcmError::UndeterminedSampleRate.to_string(), "SourceAudioReader: could not determine the source audio sample rate");
    assert_eq!(resolve_sample_rate(44100, 48000, true), Ok(44100));
    assert_eq!(resolve_sample_rate(0, 48000, true), Ok(48000));
    assert_eq!(resolve_sample_rate(0, 0, false), Ok(48000));
    assert_eq!(resolve_sample_rate(0, 0, true), Err(PcmError::UndeterminedSampleRate));
    assert!(read_pcm_s32(&fixture("no_such_file.mov"), None).is_err());
}

// ---------------------------------------------------------------------------------------------
// AvWriter

const W: u32 = 128;
const H: u32 = 72;

/// A stereo s32le tone, `frames` long.
fn tone(rate: u32, channels: u32, frames: usize) -> PcmAudio {
    let mut pcm = Vec::with_capacity(frames * channels as usize * 4);
    for i in 0..frames {
        for c in 0..channels {
            let v = ((i as f64 * (440.0 + 110.0 * f64::from(c)) * std::f64::consts::TAU / f64::from(rate)).sin() * 1.0e9) as i32;
            pcm.extend_from_slice(&v.to_le_bytes());
        }
    }
    PcmAudio { rate, channels, pcm }
}

/// Write `n` tiles frames at 30 fps (timescale 600) and finish; the audio warning.
fn write(path: &Path, codec: AvCodec, container: AvContainer, props: CompressionProps, audio: Option<PcmAudio>, n: u32) -> Option<String> {
    let mut w = AvWriter::create(path, codec, container, W, H, props, audio).unwrap();
    for f in 0..n {
        w.append(&rgba_to_bgra(&tiles(f, W, H)), i64::from(f) * 20, 600).unwrap();
    }
    w.finish().unwrap()
}

#[test]
fn every_codec_glenc_writes_round_trips() {
    // GlEnc's codec / container combinations and its compression properties per codec.
    let h264 = CompressionProps { quality: Some(0.6), h264_profile: Some(H264Profile::High), ..Default::default() };
    let hevc = CompressionProps { quality: Some(0.6), ..Default::default() };
    let mjpeg = CompressionProps { quality: Some(MJPEG_DEFAULT_QUALITY), ..Default::default() };
    for (codec, container, props) in [
        (AvCodec::ProRes422, AvContainer::Mov, CompressionProps::default()),
        (AvCodec::ProRes422HQ, AvContainer::Mov, CompressionProps::default()),
        (AvCodec::ProRes422LT, AvContainer::Mov, CompressionProps::default()),
        (AvCodec::ProRes422Proxy, AvContainer::Mov, CompressionProps::default()),
        (AvCodec::ProRes4444, AvContainer::Mov, CompressionProps::default()),
        (AvCodec::H264, AvContainer::Mov, h264),
        (AvCodec::H264, AvContainer::Mp4, h264),
        (AvCodec::Hevc, AvContainer::Mov, hevc),
        (AvCodec::Hevc, AvContainer::Mp4, hevc),
        (AvCodec::Jpeg, AvContainer::Mov, mjpeg),
    ] {
        let name = format!("{codec:?}.{}", if container == AvContainer::Mov { "mov" } else { "mp4" });
        let path = out(&name);
        assert_eq!(write(&path, codec, container, props, None, 12), None);
        let (r, frames) = read_all(&path);
        assert_eq!(&r.alpha().codec, &codec.fourcc(), "{name}");
        assert_eq!((r.width(), r.height(), frames.len()), (W, H, 12), "{name}");
        assert_eq!(r.alpha().contains_alpha_channel.unwrap_or(false), codec == AvCodec::ProRes4444, "{name}");
        for (i, f) in frames.iter().enumerate() {
            assert_eq!((f.0, f.1), (i as i64 * 20, 600), "{name} frame {i}");
            assert_eq!(read_frame_id(&bgra_to_rgba(&f.2), W, H), Some(i as u32), "{name} frame {i}");
        }
        assert!((r.duration_seconds() - 12.0 / 30.0).abs() < 1e-9, "{name}: {}", r.duration_seconds());
    }
}

#[test]
fn prores_4444_keeps_alpha_and_lpcm_audio_round_trips_in_mov() {
    let path = out("alpha_audio.mov");
    let audio = tone(48000, 2, 48000 * 12 / 30);
    assert_eq!(write(&path, AvCodec::ProRes4444, AvContainer::Mov, CompressionProps::default(), Some(audio.clone()), 12), None);
    let (_, frames) = read_all(&path);
    for (i, f) in frames.iter().enumerate() {
        let src = rgba_to_bgra(&tiles(i as u32, W, H));
        let alpha_diff = f.2.iter().zip(&src).skip(3).step_by(4).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(alpha_diff <= 1, "frame {i}: alpha off by {alpha_diff}");
    }
    // The file holds the PCM it was given, bit for bit (the track is LPCM s32le interleaved; its
    // first 4 KiB found verbatim in the mdat)...
    let file = std::fs::read(&path).unwrap();
    assert!(file.windows(4096).any(|w| w == &audio.pcm[..4096]), "the PCM is stored verbatim");
    // ...but reading it back through AVFoundation (GlEnc's readInterleavedPCM settings) is not
    // bit-exact: the reader carries 32-bit integer samples through Float32 (24-bit mantissa), so
    // the low bits of large samples move (measured; ffmpeg reads the stored bytes exactly).
    let back = read_pcm_s32(&path, None).unwrap().unwrap();
    assert_eq!((back.rate, back.channels, back.pcm.len()), (audio.rate, audio.channels, audio.pcm.len()));
    let sample = |b: &[u8], i: usize| i32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let worst = (0..audio.pcm.len()).step_by(4).map(|i| sample(&back.pcm, i).abs_diff(sample(&audio.pcm, i))).max().unwrap();
    assert!(worst <= 128, "max sample difference {worst}");
    assert!(worst > 0, "control: the float path is visible on these samples");
}

#[test]
fn mp4_audio_is_aac() {
    let path = out("aac.mp4");
    let audio = tone(44100, 1, 44100 / 2);
    let h264 = CompressionProps { quality: Some(0.6), h264_profile: Some(H264Profile::Main), ..Default::default() };
    assert_eq!(write(&path, AvCodec::H264, AvContainer::Mp4, h264, Some(audio), 12), None);
    // The esds carries AAC (object type 0x40) at 256 000 bit/s, as GlEnc asks.
    let bytes = std::fs::read(&path).unwrap();
    let esds = bytes.windows(4).position(|w| w == b"esds").unwrap();
    assert!(bytes[esds..esds + 64].windows(4).any(|w| w == 256_000u32.to_be_bytes()), "256 kbit/s in the esds");
    let back = read_pcm_s32(&path, None).unwrap().unwrap();
    assert_eq!((back.rate, back.channels), (44100, 1));
    assert!(back.frame_count() >= 44100 / 2, "{}", back.frame_count());
}

#[test]
fn the_compression_properties_reach_the_encoder() {
    let (lo, hi) = (out("q_lo.mov"), out("q_hi.mov"));
    write(&lo, AvCodec::Jpeg, AvContainer::Mov, CompressionProps { quality: Some(0.1), ..Default::default() }, None, 6);
    write(&hi, AvCodec::Jpeg, AvContainer::Mov, CompressionProps { quality: Some(MJPEG_DEFAULT_QUALITY), ..Default::default() }, None, 6);
    let (lo, hi) = (std::fs::metadata(&lo).unwrap().len(), std::fs::metadata(&hi).unwrap().len());
    assert!(hi > lo + 6 * 1000, "quality 0.85 {hi} bytes vs 0.1 {lo}");
    // Every key at once (bit rate, keyframe interval, profile) is accepted for H.264.
    let all = CompressionProps { quality: None, average_bitrate: Some(2_000_000), max_keyframe_interval: Some(5), h264_profile: Some(H264Profile::Baseline) };
    assert_eq!(write(&out("all_props.mp4"), AvCodec::H264, AvContainer::Mp4, all, None, 12), None);
}

#[test]
fn prores_is_deterministic_and_the_decoded_attachments_give_the_colr_atom() {
    // GlEnc hands the decoder's own buffer to the writer when it does not resize: its attachments
    // (here ITU-R 709 primaries / transfer / matrix) reach the file as a `colr` atom (and set the
    // encoder's matrix). A rebuilt buffer (resize) has none: no `colr`.
    let mut r = BgraReader::open(&fixture("tiles_prores422.mov")).unwrap();
    let mut frames = Vec::new();
    while let Some(f) = r.next_frame().unwrap() {
        frames.push(f);
    }
    assert!(frames[0].attachments.is_some());
    let enc = |name: &str, tagged: bool| {
        let path = out(name);
        let mut w = AvWriter::create(&path, AvCodec::ProRes422, AvContainer::Mov, W, H, CompressionProps::default(), None).unwrap();
        for f in &frames {
            let a = if tagged { f.attachments.as_ref() } else { None };
            w.append_with_attachments(&f.data, f.pts_value, f.pts_timescale, a).unwrap();
        }
        w.finish().unwrap();
        std::fs::read(&path).unwrap()
    };
    let (tagged, tagged2, bare) = (enc("tagged.mov", true), enc("tagged2.mov", true), enc("bare.mov", false));
    let has_colr = |b: &[u8]| b.windows(8).any(|w| w == b"colrnclc");
    assert!(has_colr(&tagged) && !has_colr(&bare));
    // Same frames, same attachments: the same file apart from the creation / modification times
    // (measured: byte-identical to glenc-cli's ProRes and MJPEG output except those).
    assert_eq!(tagged.len(), tagged2.len());
    let differing = tagged.iter().zip(&tagged2).filter(|(a, b)| a != b).count();
    assert!(differing <= 24, "{differing} bytes differ");
}

#[test]
fn writer_errors_read_as_glencs_and_never_crash() {
    // AAC tops out at 48 kHz: GlEnc's canApply check refuses 96 kHz in .mp4.
    let e = AvWriter::create(&out("aac96.mp4"), AvCodec::H264, AvContainer::Mp4, W, H, CompressionProps::default(), Some(tone(96000, 2, 960))).err().unwrap();
    assert_eq!(e.to_string(), "AVAssetWriter rejected the audio input (96000 Hz in public.mpeg-4) — audio would be dropped");
    // More than two channels: AVFoundation would raise "Missing required key AVChannelLayoutKey"
    // (GlEnc 1.6.2 crashes there); here it is the same refusal as above.
    let e =
        AvWriter::create(&out("six.mov"), AvCodec::ProRes422, AvContainer::Mov, W, H, CompressionProps::default(), Some(tone(48000, 6, 480))).err().unwrap();
    assert!(matches!(e, WriterError::AudioInputRejected { rate: 48000, container: "com.apple.quicktime-movie" }), "{e:?}");
    // Finishing before any frame: an error, not an exception.
    let w = AvWriter::create(&out("empty.mov"), AvCodec::ProRes422, AvContainer::Mov, W, H, CompressionProps::default(), None).unwrap();
    assert!(matches!(w.finish(), Err(WriterError::FinishFailed { status: 0, .. })));
    // A frame of the wrong size.
    let mut w = AvWriter::create(&out("short.mov"), AvCodec::ProRes422, AvContainer::Mov, W, H, CompressionProps::default(), None).unwrap();
    assert!(matches!(w.append(&[0; 16], 0, 600), Err(WriterError::Frame(_))));
    drop(w); // cancels the writer
    // An existing file is replaced (Swift deletes it first; AVAssetWriter refuses to overwrite).
    let path = out("replaced.mov");
    std::fs::write(&path, b"not a movie").unwrap();
    write(&path, AvCodec::ProRes422Proxy, AvContainer::Mov, CompressionProps::default(), None, 2);
    assert_eq!(read_all(&path).1.len(), 2);
    assert_eq!(WriterError::AppendFailed { seconds: 0.0 }.to_string(), "pixel-buffer append failed at 0.0s");
    assert_eq!(
        WriterError::CodecRejected("AVVideoCodecType(_rawValue: apcn) in public.mpeg-4".into()).to_string(),
        "AVAssetWriter rejected codec/container: AVVideoCodecType(_rawValue: apcn) in public.mpeg-4"
    );
}

#[test]
fn the_pcm_sample_buffer_is_glencs() {
    let a = tone(48000, 2, 1000);
    let sb = make_s32le_pcm_sample_buffer(&a).unwrap();
    // SAFETY: a valid sample buffer.
    assert_eq!(unsafe { sb.num_samples() }, 1000);
    let empty = PcmAudio { rate: 48000, channels: 2, pcm: Vec::new() };
    assert_eq!(make_s32le_pcm_sample_buffer(&empty).err().unwrap().to_string(), "audio sample buffer failed: empty PCM or zero bytes-per-frame");
    let short = PcmAudio { rate: 48000, channels: 2, pcm: vec![0; 4] };
    assert_eq!(make_s32le_pcm_sample_buffer(&short).err().unwrap().to_string(), "audio sample buffer failed: PCM shorter than one audio frame");
}

// ---------------------------------------------------------------------------------------------
// scale_bgra

/// The test image of `scale_ref.swift`'s goldens: xorshift32 noise on a 4x4 checkerboard, ramps
/// elsewhere (one PRNG step per byte).
fn pattern(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut s = seed;
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..4u32 {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                v.push(if (x / 4 + y / 4) % 2 == 0 { (s >> 24) as u8 } else { ((x * 7 + y * 3 + c * 50) & 255) as u8 });
            }
        }
    }
    v
}

#[test]
fn vimage_scale_is_glencs() {
    let small = pattern(37, 23, 0x0123_4567);
    let big = pattern(80, 51, 0x89ab_cdef);
    assert_eq!((fnv(&small), fnv(&big)), (0xf3ac_9e0a_c6f9_9b26, 0x8178_6131_4f63_264b), "the pattern is the goldens' input");
    // (source, size, target, filter, FNV of Swift's vImageScale_ARGB8888 output, read tight)
    for (src, (w, h), (dw, dh), filter, golden) in [
        (&small, (37, 23), (80, 51), ScaleFilter::Bilinear, 0xd234_0d0e_6e39_bb25u64),
        (&small, (37, 23), (80, 51), ScaleFilter::Lanczos, 0x4e48_c0c8_35e5_e0d0),
        (&big, (80, 51), (29, 17), ScaleFilter::Bilinear, 0xad0f_4348_8297_c4e6),
        (&big, (80, 51), (29, 17), ScaleFilter::Lanczos, 0x4896_7105_bf63_0a02),
        (&big, (80, 51), (1, 1), ScaleFilter::Lanczos, 0xb05b_773f_55b5_eadb),
    ] {
        let tight = scale_bgra(src, w, h, dw, dh, filter).unwrap();
        assert_eq!(fnv(&tight), golden, "{w}x{h} -> {dw}x{dh} {filter:?}");
        // GlEnc's IOSurface buffers pad their rows (e.g. 37 px = 148 bytes -> 192): the output does
        // not depend on the stride.
        let (row, drow, pad) = (w as usize * 4, dw as usize * 4, 44);
        let padded: Vec<u8> = src.chunks_exact(row).flat_map(|r| r.iter().copied().chain([0xAB; 44])).collect();
        let (o, stride) = scale_bgra_strided(&padded, w, h, row + pad, dw, dh, drow + pad, filter).unwrap();
        let o: Vec<u8> = o.chunks(stride).flat_map(|r| r[..drow].iter().copied()).collect();
        assert_eq!(o, tight, "{w}x{h} -> {dw}x{dh} {filter:?} padded");
    }
    // Control: the two kernels differ.
    assert_ne!(scale_bgra(&small, 37, 23, 80, 51, ScaleFilter::Bilinear).unwrap(), scale_bgra(&small, 37, 23, 80, 51, ScaleFilter::Lanczos).unwrap());
    // Equal sizes: the frame unchanged (Swift's fast path).
    assert_eq!(scale_bgra(&small, 37, 23, 37, 23, ScaleFilter::Lanczos).unwrap(), small);
    assert_eq!(
        scale_bgra(&small, 37, 23, 0, 5, ScaleFilter::Bilinear).err().unwrap().to_string(),
        "FrameResizer: target dimensions must be positive — got (0×5)"
    );
    assert!(matches!(scale_bgra(&small[..100], 37, 23, 5, 5, ScaleFilter::Bilinear), Err(ScaleError::InvalidSource { .. })));
}
