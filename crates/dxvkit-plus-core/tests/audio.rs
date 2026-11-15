//! W4 (0.11.0): sound tracks. The PCM fixtures (`scripts/make-audio-fixtures.py`, ours) against what
//! AVFoundation serves for them (`docs/data/w4-avf-audio-fixtures.jsonl`, written by
//! `scripts/swift-audio-probe.swift`: SHA-256 of its f32 samples, frame counts, first times); the
//! timing rule on hand-made tables; descriptions in `MediaInfo`; undecodable tracks as errors. Every
//! assertion has a negative control.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::audio::{AudioCodec, AudioReader, PcmFormat};
use dxvkit_plus_core::container::{MemSource, Movie};
use dxvkit_plus_core::source::{self, ImportOptions};
use dxvkit_plus_core::timing::{AudioTables, AudioTiming, Edit, MediaTime};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// AVFoundation's line for (file, track) from the committed probe output.
fn avf(file: &str, track: u32) -> serde_json::Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/data/w4-avf-audio-fixtures.jsonl");
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["file"] == file && v["track"] == track)
        .unwrap_or_else(|| panic!("no AVFoundation line for {file} {track}"))
}

/// Every frame of a reader: (sha256 of the f32 little-endian samples, frames, first position).
fn read_all(r: &mut AudioReader, block: u32) -> (String, u64, Option<MediaTime>) {
    let mut h = Sha256::new();
    let (mut frames, mut first) = (0u64, None);
    while let Some(b) = r.read(block).unwrap() {
        first.get_or_insert(b.position);
        assert_eq!(b.samples.len(), b.frames as usize * b.channels as usize);
        for s in &b.samples {
            h.update(s.to_le_bytes());
        }
        frames += u64::from(b.frames);
    }
    (h.finalize().iter().map(|b| format!("{b:02x}")).collect(), frames, first)
}

#[test]
fn every_pcm_layout_equals_avfoundation() {
    let movie = Movie::open(&fixtures().join("audio_pcm_formats.mov")).unwrap();
    // 26 tracks parse; the non-interleaved one is listed as damaged with the reason.
    assert_eq!((movie.audio.len(), movie.damaged_audio.len(), movie.damaged_audio[0].index), (26, 1, 26));
    let mut compared = 0;
    for k in 0..27u32 {
        let a = avf("audio_pcm_formats.mov", k.min(25));
        match AudioReader::open(&movie, k, None) {
            Err(e) => {
                // Track 26: non-interleaved lpcm, refused by name (AVFoundation does not list it).
                assert_eq!(k, 26);
                assert!(matches!(&e, MediaError::Unsupported(m) if m.contains("non-interleaved")), "{e}");
            }
            Ok(mut r) => {
                let (sha, frames, first) = read_all(&mut r, 333);
                // Without an edit list a track plays its tkhd duration: 21 ms = 1008 of 1024 frames.
                assert_eq!(frames, 1008, "track {k}");
                assert_eq!(first, Some(MediaTime::new(0, 48000)));
                if k == 25 {
                    // 20 bits in 24, aligned high: AVFoundation refuses it ('fmt?' = 1718449215);
                    // ours reads it as a 20-bit track (control: the neighbouring track AVFoundation
                    // reads has no error).
                    assert!(a["error"].as_str().unwrap().contains("1718449215"), "{a}");
                    assert!(avf("audio_pcm_formats.mov", 24).get("error").is_none_or(serde_json::Value::is_null));
                    assert_eq!(r.track().bits, 20);
                    continue;
                }
                assert_eq!(a["frames"], frames, "track {k}");
                assert_eq!(a["sha256"], sha, "track {k}: {}", dxvkit_plus_core::names::audio_codec_label(r.track()));
                compared += 1;
                // Negative control: the same bytes read with the other byte order (or, one byte
                // wide, the other signedness) are not AVFoundation's samples.
                let mut m2 = movie.clone();
                let t = m2.audio.iter_mut().find(|t| t.index == k).unwrap();
                let f = t.pcm.unwrap();
                t.pcm = Some(if f.bytes == 1 { PcmFormat { signed: !f.signed, ..f } } else { PcmFormat { big_endian: !f.big_endian, ..f } });
                let (sha2, _, _) = read_all(&mut AudioReader::open(&m2, k, None).unwrap(), 333);
                assert_ne!(a["sha256"], sha2, "track {k}: control");
            }
        }
    }
    assert_eq!(compared, 25);
}

#[test]
fn integer_and_float_samples_convert_as_avfoundation_does() {
    let s16 = PcmFormat { bits: 16, bytes: 2, float: false, big_endian: false, signed: true, aligned_high: false };
    assert_eq!(s16.sample(&[0x00, 0x80]), -1.0);
    assert_eq!(s16.sample(&[0xFF, 0x7F]), 32767.0 / 32768.0);
    let u8f = PcmFormat { bits: 8, bytes: 1, signed: false, ..s16 };
    assert_eq!(u8f.sample(&[0x80]), 0.0);
    assert_eq!(u8f.sample(&[0x00]), -1.0);
    // 24 bits high in 32: the low byte is not part of the sample.
    let hi = PcmFormat { bits: 24, bytes: 4, aligned_high: true, ..s16 };
    assert_eq!(hi.sample(&[0xAB, 0x00, 0x00, 0x80]), -1.0);
    // 32-bit integers round to the nearest f32, as AVFoundation's converter does.
    let s32 = PcmFormat { bits: 32, bytes: 4, ..s16 };
    assert_eq!(s32.sample(&0x7FFF_FFFFi32.to_le_bytes()), 1.0);
    // Float: -0.0 becomes +0.0 (AVFoundation's output on the fixtures), f64 rounds to nearest.
    let f32f = PcmFormat { bits: 32, bytes: 4, float: true, ..s16 };
    assert_eq!(f32f.sample(&(-0.0f32).to_le_bytes()).to_bits(), 0);
    let f64f = PcmFormat { bits: 64, bytes: 8, float: true, ..s16 };
    assert_eq!(f64f.sample(&0.1f64.to_le_bytes()), 0.1f32);
    // Controls: an integer -0 does not exist, the signedness and the high alignment matter.
    assert_ne!(PcmFormat { signed: false, ..s16 }.sample(&[0x00, 0x80]), -1.0);
    assert_ne!(PcmFormat { aligned_high: false, ..hi }.sample(&[0xAB, 0x00, 0x00, 0x80]), -1.0);
    assert!(PcmFormat { bits: 33, bytes: 4, ..s16 }.supported().is_err());
    assert!(PcmFormat { bits: 16, bytes: 2, float: true, ..s16 }.supported().is_err());
}

#[test]
fn edits_trim_repeat_and_fill_empty_edits_with_silence_as_avfoundation_does() {
    let movie = Movie::open(&fixtures().join("audio_pcm_edits.mov")).unwrap();
    let t = &movie.audio[0];
    // empty 3 ms -> round(132.3) = 132 silent frames; 50 ms from media 441 = 2205 frames; empty
    // 2 ms -> up to round(55 ms x 44.1 kHz) = 2426 (89 silent); 3 ms from media 100 = 133 frames
    // (the edit ends inside frame 232: it is kept); 20 ms from media 4000: the media ends at 4410
    // (410 frames); the trailing empty edit adds nothing. Starts are the edits' exact times.
    let want = [(0, 132, true, 0), (441, 2205, false, 3), (0, 89, true, 53), (100, 133, false, 55), (4000, 410, false, 58)];
    assert_eq!(t.timing.segments().len(), want.len());
    for (s, &(first, frames, silent, ms)) in t.timing.segments().iter().zip(&want) {
        assert_eq!((s.first, s.frames, s.silent), (first, frames, silent));
        assert_eq!(s.start.cmp_time(MediaTime::new(ms, 1000)), std::cmp::Ordering::Equal, "{s:?}");
    }
    let a = avf("audio_pcm_edits.mov", 0);
    let mut r = AudioReader::open(&movie, 0, None).unwrap();
    let (sha, frames, _) = read_all(&mut r, 100);
    assert_eq!((a["frames"].as_u64().unwrap(), a["sha256"].as_str().unwrap()), (frames, sha.as_str()));
    // Positions: exact real starts; a block never crosses a segment.
    let mut r = AudioReader::open(&movie, 0, None).unwrap();
    r.seek_frame(130);
    let b = r.read(1000).unwrap().unwrap();
    assert_eq!((b.first_frame, b.frames, b.samples.iter().all(|&s| s == 0.0)), (130, 2, true));
    let b = r.read(1000).unwrap().unwrap();
    assert_eq!((b.position.cmp_time(MediaTime::new(3, 1000)), b.frames), (std::cmp::Ordering::Equal, 1000));
    // Controls: without the empty edits' silence, or with the edit list ignored, the samples
    // are not AVFoundation's.
    let mut m2 = movie.clone();
    m2.audio[0].timing = AudioTiming::from_tables(&AudioTables {
        decoded_frames: 4410,
        rate: 44100,
        timescale: 44100,
        media_duration: 4410,
        edits: Some(&[
            Edit { duration: 50, media_time: 441, rate: Edit::RATE_ONE },
            Edit { duration: 3, media_time: 100, rate: Edit::RATE_ONE },
            Edit { duration: 20, media_time: 4000, rate: Edit::RATE_ONE },
        ]),
        movie_timescale: 1000,
        track_duration: 0,
        priming: 0,
    });
    assert_ne!(read_all(&mut AudioReader::open(&m2, 0, None).unwrap(), 100).0, sha);
    m2.audio[0].timing =
        AudioTiming::from_tables(&AudioTables { decoded_frames: 4410, rate: 44100, timescale: 44100, media_duration: 4410, ..AudioTables::default() });
    assert_ne!(read_all(&mut AudioReader::open(&m2, 0, None).unwrap(), 100).0, sha);
}

#[test]
fn seeking_by_time_lands_on_the_frame_playing_then() {
    let movie = Movie::open(&fixtures().join("audio_pcm_edits.mov")).unwrap();
    let ti = &movie.audio[0].timing;
    for n in [0u64, 131, 132, 133, 2336, 2337, 2425, 2426, 2558, 2559, 2968] {
        let p = ti.position_of(n).unwrap();
        assert_eq!(ti.frame_at(p), n, "frame {n} at its own start");
    }
    // In the silence: frames of the silent segment; before 0 -> 0; past the end -> the count.
    assert_eq!(ti.frame_at(MediaTime::new(-5, 1000)), 0);
    assert_eq!(ti.frame_at(MediaTime::new(1, 1)), ti.frame_count());
    // Random access reads the same samples as a straight read (PCM).
    let whole = {
        let mut r = AudioReader::open(&movie, 0, None).unwrap();
        let mut v = Vec::new();
        while let Some(b) = r.read(4096).unwrap() {
            v.extend(b.samples);
        }
        v
    };
    let mut r = AudioReader::open(&movie, 0, None).unwrap();
    for n in [2900u64, 7, 2500, 132, 0] {
        r.seek(ti.position_of(n).unwrap());
        let b = r.read(50).unwrap().unwrap();
        assert_eq!(b.first_frame, n);
        assert_eq!(b.samples[..], whole[n as usize..n as usize + b.frames as usize]);
    }
    // Control: one frame period earlier is the frame before (positions count in 441 000 units here).
    let p = ti.position_of(2000).unwrap();
    assert_eq!(p.timescale % 44100, 0);
    assert_eq!(ti.frame_at(MediaTime::new(p.value - i64::from(p.timescale / 44100), p.timescale)), 1999);
}

fn tables(decoded: u64, media: u64, edits: Option<&[Edit]>, movie_ts: u32) -> AudioTiming {
    AudioTiming::from_tables(&AudioTables {
        decoded_frames: decoded,
        rate: 44100,
        timescale: 44100,
        media_duration: media,
        edits,
        movie_timescale: movie_ts,
        track_duration: 0,
        priming: 0,
    })
}

#[test]
fn the_timing_rule_matches_avfoundation_on_the_test_folder_shapes() {
    let e = |d: u64, t: i64| Edit { duration: d, media_time: t, rate: Edit::RATE_ONE };
    // 220 160 frames, edit 4.992 s at movie scale 1000: the frame starting at 220 147 (inside the
    // edit's last 0.2 frame) is kept: 220 148 (AVFoundation, test-hap1-audio_DXV...).
    assert_eq!(tables(220_160, 220_160, Some(&[e(4992, 0)]), 1000).frame_count(), 220_148);
    // AAC priming 1024 in the edit, last packet 340 frames (std_h264_audio.mov): 220 500.
    assert_eq!(tables(217 * 1024, 216 * 1024 + 340, Some(&[e(5000, 1024)]), 1000).frame_count(), 220_500);
    // An edit longer than the media keeps what exists (test-hap1-audio.mov: 220 160).
    assert_eq!(tables(220_160, 220_160, Some(&[e(4993, 0)]), 1000).frame_count(), 220_160);
    // Controls: the media end matters (the decoded last packet is longer than its stts delta)…
    assert_ne!(tables(217 * 1024, 217 * 1024, Some(&[e(5000, 1024)]), 1000).frame_count(), 220_500 + 1);
    // … and so does the edit: without it all 220 160 frames play.
    assert_ne!(tables(220_160, 220_160, None, 1000).frame_count(), 220_148);
    // An edit at another speed or a negative media time: the list is not applied.
    let fast = [Edit { duration: 1000, media_time: 0, rate: 2 * Edit::RATE_ONE }];
    assert_eq!(tables(44_100, 44_100, Some(&fast), 1000).frame_count(), 44_100);
    assert_eq!(tables(44_100, 44_100, Some(&[e(500, -7)]), 1000).frame_count(), 44_100);
    assert_eq!(tables(44_100, 44_100, Some(&[e(500, 0)]), 1000).frame_count(), 22_050);
    // Priming not stated (no roll group): 2112 more frames trimmed after the media time.
    let t = AudioTiming::from_tables(&AudioTables {
        decoded_frames: 24 * 1024,
        rate: 44100,
        timescale: 44100,
        media_duration: 24 * 1024,
        edits: Some(&[e(22050, 2112)]),
        movie_timescale: 44100,
        track_duration: 0,
        priming: 2112,
    });
    assert_eq!((t.frame_count(), t.source_frame(0)), (20_352, Some(4224)));
    // A media scale that is not the rate (exact through the cross products): 1000 units/s.
    let t = AudioTiming::from_tables(&AudioTables {
        decoded_frames: 48_000,
        rate: 48_000,
        timescale: 1000,
        media_duration: 1000,
        edits: Some(&[e(600, 250)]),
        movie_timescale: 600,
        track_duration: 0,
        priming: 0,
    });
    assert_eq!((t.frame_count(), t.source_frame(0), t.position_of(1)), (48_000 - 12_000, Some(12_000), Some(MediaTime::new(1, 48_000))));
}

#[test]
fn hostile_audio_tables_never_panic() {
    let e = |d: u64, t: i64| Edit { duration: d, media_time: t, rate: Edit::RATE_ONE };
    for edits in [vec![e(u64::MAX, i64::MAX)], vec![e(0, 0)], vec![e(1, -1); 70_000], vec![e(u64::MAX, -1), e(5, 3)]] {
        for (rate, ts, mts) in [(0, 1, 1), (1, 0, 1), (u32::MAX, u32::MAX, u32::MAX), (44_100, 1, u32::MAX)] {
            let t = AudioTiming::from_tables(&AudioTables {
                decoded_frames: u64::MAX,
                rate,
                timescale: ts,
                media_duration: u64::MAX,
                edits: Some(&edits),
                movie_timescale: mts,
                track_duration: u64::MAX,
                priming: u64::MAX,
            });
            for n in [0, t.frame_count() / 2, t.frame_count(), u64::MAX] {
                let _ = (t.position_of(n), t.source_frame(n));
            }
            let _ = t.frame_at(MediaTime::new(i64::MIN, 0));
            let _ = t.frame_at(MediaTime::new(i64::MAX, 1));
        }
    }
    // Control: the same loop on sane tables computes real frames.
    let t = tables(44_100, 44_100, Some(&[e(500, 0)]), 1000);
    assert_eq!((t.frame_count(), t.position_of(22_049).map(|p| p.cmp_time(MediaTime::new(22_049, 44_100)).is_eq())), (22_050, Some(true)));
}

#[test]
fn media_info_lists_every_sound_track() {
    let info = source::probe(fixtures().join("audio_pcm_formats.mov").to_str().unwrap(), &ImportOptions::default()).unwrap();
    assert_eq!(info.audio.len(), 27);
    let a = &info.audio[0];
    assert_eq!(
        (a.fourcc.as_str(), a.codec, a.codec_name.as_str(), a.channels, a.sample_rate, a.bits, a.frames),
        ("sowt", AudioCodec::Pcm, "PCM 16-bit integer LE", 2, 48000, 16, 1008)
    );
    let names: Vec<&str> = info.audio.iter().map(|a| a.codec_name.as_str()).collect();
    assert!(names.contains(&"PCM 64-bit float BE") && names.contains(&"PCM 8-bit unsigned integer") && names.contains(&"PCM 20-bit integer BE"));
    let aac = source::probe(fixtures().join("audio_aac_priming.mov").to_str().unwrap(), &ImportOptions::default()).unwrap();
    assert_eq!(aac.audio.len(), 1);
    assert_eq!(
        (aac.audio[0].codec, aac.audio[0].codec_name.as_str(), aac.audio[0].sample_rate, aac.audio[0].channels, aac.audio[0].bits),
        (AudioCodec::Aac, "AAC", 44100, 2, 0)
    );
    // Control: a movie without sound lists none.
    let none = source::probe(fixtures().join("tiles_hap1.mov").to_str().unwrap(), &ImportOptions::default()).unwrap();
    assert!(none.audio.is_empty());
}

/// The fixture's bytes with the last `from` FourCC (the sound track's, written after the video's)
/// replaced by `to`.
fn patched(name: &str, from: &[u8; 4], to: &[u8; 4]) -> Movie {
    let mut b = std::fs::read(fixtures().join(name)).unwrap();
    let at = b.windows(4).rposition(|w| w == from).unwrap();
    b[at..at + 4].copy_from_slice(to);
    Movie::from_source(Arc::new(MemSource(Arc::from(b)))).unwrap()
}

#[test]
fn an_undecodable_track_is_an_error_naming_its_codec_and_never_blocks_the_picture() {
    // A sowt track renamed fLaC: listed, refused by name; the video still opens and decodes.
    let m = patched("audio_pcm_edits.mov", b"sowt", b"fLaC");
    assert_eq!(m.audio[0].codec, AudioCodec::Other);
    let e = AudioReader::open(&m, 0, None).unwrap_err();
    assert!(matches!(&e, MediaError::Unsupported(s) if s.contains("FLAC")), "{e}");
    assert_eq!(m.tracks.len(), 1);
    let mut pkt = Vec::new();
    m.read_sample(&m.tracks[0], 0, &mut pkt).unwrap();
    // AAC without a platform decoder: refused by name.
    let aac = Movie::open(&fixtures().join("audio_aac_priming.mov")).unwrap();
    let e = AudioReader::open(&aac, 0, None).unwrap_err();
    assert!(matches!(&e, MediaError::Unsupported(s) if s.contains("AAC")), "{e}");
    // A track index past the movie's tracks.
    assert!(matches!(AudioReader::open(&aac, 5, None), Err(MediaError::Invalid(_))));
    // Damaged sound tables: listed as damaged with the error, the movie and its video still open.
    let d = patched("audio_pcm_edits.mov", b"stsc", b"xxxx");
    assert!(d.audio.is_empty() && d.damaged_audio.len() == 1 && d.tracks.len() == 1);
    assert!(matches!(AudioReader::open(&d, 0, None), Err(MediaError::Corrupt(_))));
    d.read_sample(&d.tracks[0], 0, &mut pkt).unwrap();
    // Control: the unpatched track reads.
    assert!(AudioReader::open(&Movie::open(&fixtures().join("audio_pcm_edits.mov")).unwrap(), 0, None).is_ok());
}

#[test]
fn a_classic_rate_wrapped_at_65536_is_read_as_the_media_time_scale() {
    // GlEnc's old 88.2 / 96 kHz version-1 entries stored the rate in 16.16: 96 000 became 30 464.
    // The audio_pcm_edits track (44.1 kHz) with its media time scale patched to 96 000 and its
    // entry rate to 30 464.
    let mut b = std::fs::read(fixtures().join("audio_pcm_edits.mov")).unwrap();
    // The entry's 16.16 rate: 28 bytes after its FourCC (QTFF sound sample description).
    let at = b.windows(4).rposition(|w| w == b"sowt").unwrap() + 28;
    assert_eq!(b[at..at + 4], (44100u32 << 16).to_be_bytes());
    b[at..at + 4].copy_from_slice(&(30464u32 << 16).to_be_bytes());
    let m = Movie::from_source(Arc::new(MemSource(Arc::from(b.clone())))).unwrap();
    assert_eq!(m.audio[0].timing.rate, 30464, "control: the media time scale is still 44 100, so the field stands");
    // Now the media time scale too.
    let mdhd = b.windows(4).rposition(|w| w == b"mdhd").unwrap();
    b[mdhd + 16..mdhd + 20].copy_from_slice(&96000u32.to_be_bytes());
    let m = Movie::from_source(Arc::new(MemSource(Arc::from(b)))).unwrap();
    assert_eq!((m.audio[0].timing.rate, m.audio[0].stated_rate), (96000, 96000.0));
}
