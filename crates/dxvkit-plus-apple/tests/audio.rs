//! W4 (0.11.0): AAC through AudioToolbox, with the core's edit list, priming and pre-roll, against
//! AVFoundation's own decode of the same fixtures (`tests/fixtures/audio_aac_*.avf7.f32`: every 7th
//! frame of what `AVAssetReader` serves, written by `scripts/swift-audio-probe.swift`). Frame counts
//! and first positions are exact; samples are compared within 1e-6 (they are bit-identical in the
//! cross-test runs, `docs/data/w4-crosstest-fixtures.jsonl`, but AudioToolbox's own output varied by
//! up to 5.7e-7 between runs on the machine that made them). Negative controls: the priming rule
//! off, one frame of offset.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_apple::AppleBackend;
use dxvkit_plus_core::audio::{AudioCodec, AudioReader};
use dxvkit_plus_core::container::Movie;
use dxvkit_plus_core::timing::MediaTime;
use std::path::PathBuf;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Every frame of track 0, interleaved, and the first block's position.
fn read(movie: &Movie, block: u32) -> (Vec<f32>, Option<MediaTime>) {
    let mut r = AudioReader::open(movie, 0, Some(&AppleBackend)).unwrap();
    let (mut v, mut first) = (Vec::new(), None);
    while let Some(b) = r.read(block).unwrap() {
        first.get_or_insert(b.position);
        v.extend(b.samples);
    }
    (v, first)
}

/// AVFoundation's every-7th-frame reference.
fn reference(name: &str) -> Vec<f32> {
    let b = std::fs::read(fixtures().join(format!("{name}.avf7.f32"))).unwrap();
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Largest difference between `ours` (all frames, stereo) taken every 7th frame from `offset` and
/// the reference; `None` when the lengths do not line up.
fn max_diff(ours: &[f32], reference: &[f32], offset: usize) -> Option<f32> {
    let sub: Vec<f32> = ours.chunks_exact(2).skip(offset).step_by(7).flatten().copied().collect();
    (sub.len() == reference.len()).then(|| sub.iter().zip(reference).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max))
}

#[test]
fn aac_decodes_as_avfoundation_serves_it() {
    // (fixture, frames AVFoundation serves, first position)
    for (name, frames, first) in [
        ("audio_aac_priming.mov", 22050usize, MediaTime::new(0, 44100)),
        ("audio_aac_trim.mp4", 441 + 11025, MediaTime::new(0, 44100)),
        ("audio_aac_noroll.mov", 24576 - 2 * 2112, MediaTime::new(0, 44100)),
    ] {
        let movie = Movie::open(&fixtures().join(name)).unwrap();
        assert_eq!(movie.audio[0].codec, AudioCodec::Aac);
        let (ours, pos) = read(&movie, 1000);
        assert_eq!(ours.len(), frames * 2, "{name}");
        assert!(pos.unwrap().cmp_time(first).is_eq(), "{name}: {pos:?}");
        let r = reference(name);
        let d = max_diff(&ours, &r, 0).unwrap();
        assert!(d <= 1e-6, "{name}: max difference {d}");
        // Control: one frame of offset is far from AVFoundation's samples.
        assert!(max_diff(&ours[2..], &r[..r.len() - 2], 0).is_none_or(|d| d > 1e-3), "{name}: control");
    }
    // The trim fixture starts with an empty edit: 441 frames of silence, then the edit's frames.
    let (ours, _) = read(&Movie::open(&fixtures().join("audio_aac_trim.mp4")).unwrap(), 4096);
    assert!(ours[..882].iter().all(|&s| s == 0.0) && ours[882..1000].iter().any(|&s| s != 0.0));
}

#[test]
fn without_a_roll_group_the_encoder_delay_is_trimmed_after_the_edit() {
    let movie = Movie::open(&fixtures().join("audio_aac_noroll.mov")).unwrap();
    assert_eq!((movie.audio[0].roll, movie.audio[0].timing.source_frame(0)), (None, Some(2112 + 2112)));
    let with_roll = Movie::open(&fixtures().join("audio_aac_priming.mov")).unwrap();
    assert_eq!((with_roll.audio[0].roll, with_roll.audio[0].timing.source_frame(0)), (Some(-1), Some(2112)));
    // Control: the no-roll track timed as if it had the group serves 22 050 frames, which are not
    // AVFoundation's 20 352.
    let mut m = movie.clone();
    let t = &mut m.audio[0];
    t.timing = dxvkit_plus_core::timing::AudioTiming::from_tables(&dxvkit_plus_core::timing::AudioTables {
        decoded_frames: u64::from(t.sample_count) * 1024,
        rate: 44100,
        timescale: t.timescale,
        media_duration: t.stts.iter().map(|&(c, d)| u64::from(c) * u64::from(d)).sum(),
        edits: t.edits.as_deref(),
        movie_timescale: t.movie_timescale,
        track_duration: 0,
        priming: 0,
    });
    let (ours, _) = read(&m, 1000);
    assert_eq!(ours.len(), 22050 * 2);
    assert!(max_diff(&ours, &reference("audio_aac_noroll.mov"), 0).is_none());
}

#[test]
fn a_seek_restarts_the_decoder_one_packet_early_and_lands_on_the_frame() {
    let movie = Movie::open(&fixtures().join("audio_aac_priming.mov")).unwrap();
    let (whole, _) = read(&movie, 4096);
    let mut r = AudioReader::open(&movie, 0, Some(&AppleBackend)).unwrap();
    for n in [10_000u64, 3, 21_000, 1024, 5_555] {
        r.seek_frame(n);
        let b = r.read(700).unwrap().unwrap();
        assert_eq!(b.first_frame, n);
        assert!(b.position.cmp_time(MediaTime::new(n as i64, 44100)).is_eq());
        // AVFoundation restarts its decoder on a seek too: the frames equal a straight read here
        // within AudioToolbox's run-to-run spread (a stream whose decoder state depends on the whole
        // history — AAC noise substitution — differs more; see docs/handoffs/w4-audio.md).
        let d = b.samples.iter().zip(&whole[n as usize * 2..]).map(|(a, w)| (a - w).abs()).fold(0.0f32, f32::max);
        assert!(d <= 1e-5, "frame {n}: {d}");
    }
    // Control: frames from one packet later are not these.
    r.seek_frame(10_000 + 1024);
    let b = r.read(700).unwrap().unwrap();
    assert!(b.samples.iter().zip(&whole[20_000..]).any(|(a, w)| (a - w).abs() > 1e-3));
}

#[test]
fn the_backend_names_what_it_cannot_decode() {
    // A PCM track asked of the AAC decoder directly: refused by name.
    let pcm = Movie::open(&fixtures().join("audio_pcm_edits.mov")).unwrap();
    let e = dxvkit_plus_apple::audio::AacDecoder::open(&pcm.audio[0]).err().unwrap();
    assert!(e.to_string().contains("PCM 16-bit integer LE"), "{e}");
    // Control: the AAC track opens.
    let aac = Movie::open(&fixtures().join("audio_aac_priming.mov")).unwrap();
    assert!(dxvkit_plus_apple::audio::AacDecoder::open(&aac.audio[0]).is_ok());
}
