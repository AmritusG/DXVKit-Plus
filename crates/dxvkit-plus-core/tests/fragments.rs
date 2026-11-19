//! Fragmented MP4 (0.15.0): the samples of `moof` / `traf` / `trun` fragments, with the `trex` and `tfhd`
//! defaults, build the same sample table as the same stream stored in a `moov` (ISO/IEC 14496-12 §8.8).
//! Fixtures: `tiles_h264.mp4` and four remuxes of it, sample for sample the same stream (their sidecars), and
//! `frag_h264.mp4`, written by AVFoundation as a CMAF file.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use dxvkit_plus_core::container::{Codec, MemSource, Movie, VideoTrack};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name)
}

fn track(m: &Movie) -> &VideoTrack {
    m.track(Codec::Native).unwrap()
}

/// Every sample's bytes, decode order.
fn samples(m: &Movie) -> Vec<Vec<u8>> {
    let t = track(m);
    (0..t.frame_count())
        .map(|i| {
            let mut b = Vec::new();
            m.read_sample(t, i, &mut b).unwrap();
            b
        })
        .collect()
}

/// Per-sample durations, decode order (the `stts` runs expanded).
fn durations(t: &VideoTrack) -> Vec<u32> {
    t.stts.iter().flat_map(|&(n, d)| std::iter::repeat_n(d, n as usize)).collect()
}

#[test]
fn fragments_hold_the_same_samples_as_the_moov() {
    let twin = Movie::open(&fixture("tiles_h264.mp4")).unwrap();
    let (tt, want) = (track(&twin), samples(&twin));
    assert_eq!(want.len(), 24);
    for name in ["tiles_h264_fragmented.mp4", "tiles_h264_frag_base_offset.mp4", "tiles_h264_frag_moov_first.mp4", "tiles_h264_frag_sidx.mp4"] {
        let m = Movie::open(&fixture(name)).unwrap();
        let t = track(&m);
        assert!(m.damaged.is_empty(), "{name}: {:?}", m.damaged);
        assert!(m.fragments > 0 && twin.fragments == 0, "{name}: fragments");
        assert_eq!(samples(&m), want, "{name}: sample bytes");
        // The remux changes the media time scale, so times are compared as exact fractions of a second.
        let (ta, tb) = (i128::from(t.timescale), i128::from(tt.timescale));
        let same = |a: i128, b: i128| a * tb == b * ta;
        let (da, db) = (durations(t), durations(tt));
        assert!(da.len() == db.len() && da.iter().zip(&db).all(|(&a, &b)| same(a.into(), b.into())), "{name}: sample durations {da:?} vs {db:?}");
        assert!(same(t.duration.into(), tt.duration.into()), "{name}: media duration");
        assert!(t.ctts.len() == tt.ctts.len() && t.ctts.iter().zip(&tt.ctts).all(|(&a, &b)| same(a.into(), b.into())), "{name}: composition offsets");
        assert_eq!(t.max_sample, tt.max_sample, "{name}");
        assert_eq!((t.frames(), t.timing.rate), (24, tt.timing.rate), "{name}: frames and rate");
        // The remux keeps no edit list, which the twin has (it shifts the B-frame delay out): relative to
        // frame 0 every frame starts where the twin's does and shows the same sample.
        let (fa, fb) = (t.timing.time_of_frame(0), tt.timing.time_of_frame(0));
        for f in 0..24 {
            let (a, b) = (t.timing.time_of_frame(f), tt.timing.time_of_frame(f));
            // Starts relative to frame 0, in seconds as fractions: (a - a0) / ts_a == (b - b0) / ts_b.
            let (ra, rb) = (i128::from(a.value - fa.value), i128::from(b.value - fb.value));
            assert_eq!(ra * i128::from(b.timescale), rb * i128::from(a.timescale), "{name}: frame {f} start");
            assert_eq!(t.timing.sample_of_frame(f), tt.timing.sample_of_frame(f), "{name}: the sample frame {f} shows");
        }
    }
}

/// AVFoundation's CMAF writer (default-base-is-moof, tfdt, sample flags, two segments): 60 frames at 30 fps;
/// up to 0.14.0 the track was damaged (empty `moov` tables) and the file went to the platform.
#[test]
fn an_avfoundation_cmaf_file_reads() {
    let m = Movie::open(&fixture("frag_h264.mp4")).unwrap();
    let t = track(&m);
    assert_eq!((t.frame_count(), t.frames(), t.timing.rate), (60, 60, dxvkit_plus_core::time::FrameRate::FPS_30));
    assert!(t.timing.has_table());
    assert_eq!((t.width, t.height), (64, 48));
    // Every sample lies inside the file and the samples tile the media data without overlapping.
    let mut spans = t.samples.clone();
    spans.sort_unstable();
    assert!(spans.windows(2).all(|w| w[0].0 + u64::from(w[0].1) <= w[1].0));
    assert_eq!(samples(&m).len(), 60);
}

/// Negative controls on the hostile side: a `trun` whose count runs past its box, and a fragmented `moov`
/// without fragments, make the track damaged (the other paths still see the file); without `mvex` the
/// fragments are not read at all.
#[test]
fn broken_fragments_damage_the_track() {
    let bytes = std::fs::read(fixture("tiles_h264_fragmented.mp4")).unwrap();
    let find = |b: &[u8], tag: &[u8; 4]| b.windows(4).position(|w| w == tag).unwrap();
    // The first trun's sample count (after size, type, version/flags).
    let mut long = bytes.clone();
    let at = find(&long, b"trun") + 8;
    long[at..at + 4].copy_from_slice(&1000u32.to_be_bytes());
    let m = parse(long);
    assert!(m.tracks.is_empty() && m.damaged.len() == 1, "{m:?}");
    // Only the moov (ftyp + moov): mvex says fragmented, no moof follows.
    let moov_end = find(&bytes, b"moof") - 4;
    let m = parse(bytes[..moov_end].to_vec());
    assert!(m.tracks.is_empty() && m.damaged.len() == 1, "{m:?}");
    // mvex renamed: not fragmented, so the empty moov tables are the track (damaged, as up to 0.14.0).
    let mut plain = bytes.clone();
    let at = find(&plain, b"mvex");
    plain[at..at + 4].copy_from_slice(b"free");
    let m = parse(plain);
    assert!(m.tracks.is_empty() && m.damaged.len() == 1, "{m:?}");
}

/// A user's fragmented H.264 capture (110,964 samples in 694 fragments, trex + tfhd defaults, one sidx; its
/// `mdhd` and edit say 3698.8 s, AVFoundation says 7397.6 s). Runs when `DXVKIT_PLUS_FRAGMENTED_CLIP` names a
/// copy of it; never committed.
#[test]
fn a_long_fragmented_capture_counts_its_samples() {
    let Some(path) = std::env::var_os("DXVKIT_PLUS_FRAGMENTED_CLIP") else {
        eprintln!("skip: DXVKIT_PLUS_FRAGMENTED_CLIP not set");
        return;
    };
    let m = Movie::open(Path::new(&path)).unwrap();
    let t = track(&m);
    assert_eq!((t.frame_count(), t.frames(), t.timing.rate), (110_964, 110_964, dxvkit_plus_core::time::FrameRate::FPS_30));
    assert_eq!((t.timescale, t.duration, m.fragments), (15360, 56_813_568, 694));
    // The length the apps show: where the last frame ends.
    assert_eq!(t.timing.end_time().seconds(), 3698.8);
}

/// A movie none of whose tracks parsed is kept (as [`Movie::open_all`] keeps one).
fn parse(bytes: Vec<u8>) -> Movie {
    Movie::from_source_all(Arc::new(MemSource(bytes.into()))).unwrap()
}
