//! W3 (0.10.0): frame timing — Swift Glance's FrameTiming rule over the container's tables. The rate
//! cases are Swift's own (glance `Tests/GlanceCoreTests/FrameTimingTests.swift`), each with a negative
//! control; the table / edit-list cases are movies written here (`tests/support/qt_movie.rs`) and the
//! committed edit fixtures (`scripts/make-edit-list-fixtures.swift`), decoded through the HAP and DXV
//! paths, whose frames carry their source frame index (`testpattern`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[path = "../../../tests/support/qt_movie.rs"]
mod qt;

use dxvkit_plus_core::container::{MAX_EDITS, MAX_SAMPLES, MemSource, Movie, VideoTrack};
use dxvkit_plus_core::source::{self, ImportOptions};
use dxvkit_plus_core::testpattern::read_frame_id;
use dxvkit_plus_core::time::FrameRate;
use dxvkit_plus_core::timing::{Edit, FrameTiming, MediaTime, RateInputs, RateSource, Tables, rate_of, snap};
use std::path::PathBuf;
use std::sync::Arc;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn r(num: i64, den: i64) -> FrameRate {
    FrameRate::new(num, den)
}

fn rate(d: &[i64], ts: u32, seconds: f64, min: Option<(i64, u32)>, nominal: f64) -> (FrameRate, RateSource) {
    rate_of(&RateInputs { durations: d, timescale: ts, track_seconds: seconds, min_frame_duration: min, nominal })
}

fn reps(parts: &[(usize, i64)]) -> Vec<i64> {
    parts.iter().flat_map(|&(n, d)| std::iter::repeat_n(d, n)).collect()
}

// ---- Rate rules (Swift's cases) ----

#[test]
fn snap_is_exact_and_nearest_first() {
    assert_eq!(snap(29.97), Some(r(30000, 1001)));
    assert_eq!(snap(23.976), Some(r(24000, 1001)));
    assert_eq!(snap(59.94), Some(r(60000, 1001)));
    assert_eq!(snap(29.999998), Some(r(30, 1)));
    assert_eq!(snap(50.02), Some(r(50, 1)));
    // Controls: not standard, not a number.
    assert_eq!(snap(600.0 / 23.0), None);
    assert_eq!(snap(12.5), None);
    assert_eq!(snap(f64::NAN), None);
}

#[test]
fn jittered_prores_reads_its_mean_not_its_shortest_sample() {
    // GlEnc 1.5.0's Indigofera ProRes 4444 table (600: 297 x 24, 19 x 23, 19 x 25; 13.4 s).
    let d = reps(&[(297, 24), (19, 23), (19, 25)]);
    assert_eq!(rate(&d, 600, 13.4, Some((23, 600)), 25.0), (r(25, 1), RateSource::MeanRate));
    // Control: the shortest sample alone is 26.09 fps — the 0.9.0 native grid's rate.
    assert_eq!(rate(&[], 0, 0.0, Some((23, 600)), 0.0).0, r(600, 23));
    // CueGlance 1.3.0's 30 fps ProRes (600: 272 x 20, 14 x 19, 14 x 21; 10 s).
    let d = reps(&[(272, 20), (14, 19), (14, 21)]);
    assert_eq!(rate(&d, 600, 10.0, Some((19, 600)), 30.0), (r(30, 1), RateSource::MeanRate));
    assert_ne!(r(600, 19), r(30, 1));
}

#[test]
fn ntsc_rates_are_exact_rationals() {
    for (ts, want) in [(24000, r(24000, 1001)), (30000, r(30000, 1001)), (60000, r(60000, 1001))] {
        let d = vec![1001; 40];
        assert_eq!(rate(&d, ts, 0.0, Some((1001, ts)), 0.0), (want, RateSource::MeanRate));
        assert_ne!(want, FrameRate::from_f64(want.as_f64().round()), "control: the rounded rate is another rate");
    }
}

#[test]
fn short_clips_take_the_mean_from_the_samples() {
    // 40 frames at 60 fps: the track duration at 1/1000 (0.667 s) reads 59.97 -> 59.94 (wrong).
    assert_eq!(rate(&[1; 40], 60, 0.667, Some((1, 60)), 60.0), (r(60, 1), RateSource::MeanRate));
    assert_eq!(rate(&[], 0, 0.667, Some((1, 60)), 60.0), (r(60, 1), RateSource::MinFrameDuration));
    // Control: the mean over the track duration is 59.94.
    assert_eq!(snap(40.0 / 0.667), Some(r(60000, 1001)));
}

#[test]
fn dominant_delta_when_the_mean_is_not_standard() {
    let mut d = vec![1001; 50];
    d.extend([1000, 1002, 9009]);
    assert_eq!(rate(&d, 30000, 0.0, Some((1000, 30000)), 29.4), (r(30000, 1001), RateSource::DominantDelta));
    let mut g = vec![1001; 50];
    g.extend([1000, 1002]);
    assert_eq!(rate(&g, 30000, 0.0, Some((1000, 30000)), 29.97), (r(30000, 1001), RateSource::MeanRate));
    // Control: that mean (53 / 1.7343 s) is not standard.
    assert_eq!(snap(53.0 / (d.iter().sum::<i64>() as f64 / 30000.0)), None);
}

#[test]
fn clean_non_standard_and_variable_and_missing_timing() {
    assert_eq!(rate(&[2; 50], 25, 4.0, Some((2, 25)), 12.5), (r(25, 2), RateSource::MinFrameDuration));
    // Variable non-standard timing: the nominal (average) rate wins over the minimum (33.3).
    assert_eq!(rate(&[30, 45, 30, 45, 37, 37], 1000, 0.224, Some((30, 1000)), 27.0), (r(27, 1), RateSource::Nominal));
    assert_ne!(r(1000, 30), r(27, 1));
    assert_eq!(rate(&[], 0, 0.0, None, 24.0), (r(24, 1), RateSource::Nominal));
    assert_eq!(rate(&[], 0, 0.0, Some((1, 30)), 0.0), (r(30, 1), RateSource::MinFrameDuration));
    assert_eq!(rate(&[], 0, 0.0, None, 0.0), (r(30, 1), RateSource::Fallback));
}

// ---- Frame <-> time ----

#[test]
fn grid_time_and_floor() {
    let t = FrameTiming::grid(r(30000, 1001), 40, RateSource::MeanRate);
    assert_eq!(t.time_of_frame(7), MediaTime::new(7007, 30000));
    assert_eq!(t.frame_at(MediaTime::new(7007, 30000)), 7, "a frame's start is that frame");
    assert_eq!(t.frame_at(MediaTime::new(7006, 30000)), 6, "control: one tick earlier");
    assert_eq!(t.time_of_frame(999), t.time_of_frame(39), "clamped");
    assert_eq!(t.frame_at(MediaTime::new(99, 1)), 39);
    // 7 frames at 29.97 rounded to 1/600 (Glance 0.17.2's seek) lands before frame 7.
    assert_eq!(t.frame_at(MediaTime::new((7.0 * 1001.0 / 30000.0 * 600.0f64).round() as i64, 600)), 6);
    assert_eq!(t.sample_of_frame(39), Some(39));
    assert_eq!(t.sample_of_frame(40), None);
}

fn tables<'a>(n: u32, ts: u32, stts: &'a [(u32, u32)], ctts: &'a [i64], edits: Option<&'a [Edit]>, movie_ts: u32) -> Tables<'a> {
    Tables {
        samples: n,
        timescale: ts,
        media_duration: stts.iter().map(|&(c, d)| u64::from(c) * u64::from(d)).sum(),
        stts,
        ctts,
        edits,
        movie_timescale: movie_ts,
    }
}

#[test]
fn table_times_are_the_real_starts() {
    // Swift's testTableTimeAndFloor: starts 0, 24, 48, 73, 96 at 1/600.
    let stts = [(1, 24), (1, 24), (1, 25), (1, 23), (1, 24)];
    let t = FrameTiming::from_tables(&tables(5, 600, &stts, &[], None, 600));
    assert_eq!(t.frame_count(), 5);
    assert_eq!(t.time_of_frame(3), MediaTime::new(73, 600), "the real start, not 72");
    assert_eq!(t.frame_at(MediaTime::new(72, 600)), 2, "the grid time shows frame 2");
    assert_eq!(t.frame_at(MediaTime::new(73, 600)), 3);
    assert_eq!(t.frame_at(MediaTime::new(10, 1)), 4);
    assert_eq!(t.rate, r(25, 1));
    // Control: the 25 fps grid puts frame 3 at 72.
    let grid3 = FrameTiming::grid(r(25, 1), 5, RateSource::MeanRate).time_of_frame(3);
    assert_eq!(grid3.cmp_time(MediaTime::new(72, 600)), std::cmp::Ordering::Equal);
}

#[test]
fn composition_offsets_reorder_and_an_edit_maps_back_to_zero() {
    // An IBBP-like stream: decode order I P B B, presented I B B P (ctts 2, 5, 2, 2 frames of 20).
    let stts = [(4, 20)];
    let ctts = [40, 100, 0, 0];
    let pts: Vec<i64> = (0..4).map(|k| k * 20 + ctts[k as usize]).collect();
    assert_eq!(pts, [40, 120, 40, 60]);
    let edits = [Edit { duration: 80, media_time: 40, rate: Edit::RATE_ONE }];
    let t = FrameTiming::from_tables(&tables(4, 600, &stts, &ctts, Some(&edits), 600));
    // Edit [40, 120): samples 0 (40), 2 (40), 3 (60) -> 0, 0, 20; sample 1 (120) is outside.
    assert_eq!(t.frame_count(), 3);
    assert_eq!(t.starts().map(|s| s.value).collect::<Vec<_>>(), [0, 0, 20]);
    assert_eq!((0..3).map(|n| t.sample_of_frame(n).unwrap()).collect::<Vec<_>>(), [0, 2, 3]);
    // Control: without the edit list frame 0 starts at the B-frame delay, 4 frames.
    let raw = FrameTiming::from_tables(&tables(4, 600, &stts, &ctts, None, 600));
    assert_eq!((raw.frame_count(), raw.time_of_frame(0)), (4, MediaTime::new(40, 600)));
    assert_eq!(raw.sample_of_frame(3), Some(1));
}

#[test]
fn empty_edits_shift_and_repeated_edits_repeat() {
    // 10 samples of 512 at 15360 (30 fps); movie scale 1000: empty 7 ms, frames 2..5, empty 3 ms,
    // frames 0..2 again.
    let stts = [(10, 512)];
    let ms = |frames: u64| frames * 1000 / 30;
    let edits = [
        Edit { duration: 7, media_time: -1, rate: Edit::RATE_ONE },
        Edit { duration: ms(3), media_time: 2 * 512, rate: Edit::RATE_ONE },
        Edit { duration: 3, media_time: -1, rate: Edit::RATE_ONE },
        Edit { duration: ms(2), media_time: 0, rate: Edit::RATE_ONE },
    ];
    let t = FrameTiming::from_tables(&tables(10, 15360, &stts, &[], Some(&edits), 1000));
    assert_eq!((0..5).map(|n| t.sample_of_frame(n).unwrap()).collect::<Vec<_>>(), [2, 3, 4, 0, 1]);
    // 7 ms is between 1/15360 ticks: the starts count in lcm(15360, 1000) = 384000.
    assert_eq!(t.time_of_frame(0), MediaTime::new(7 * 384, 384000));
    assert_eq!(t.time_of_frame(0).cmp_time(MediaTime::new(7, 1000)), std::cmp::Ordering::Equal);
    // The fourth edit starts after 7 + 100 + 3 ms.
    assert_eq!(t.time_of_frame(3).cmp_time(MediaTime::new(110, 1000)), std::cmp::Ordering::Equal);
    assert_eq!(t.frame_at(MediaTime::new(6, 1000)), 0, "before the first frame: frame 0");
    // Control: Swift ignores empty edits when no edit plays: the stored times, unshifted.
    let only_empty = [Edit { duration: 7, media_time: -1, rate: Edit::RATE_ONE }];
    let u = FrameTiming::from_tables(&tables(10, 15360, &stts, &[], Some(&only_empty), 1000));
    assert_eq!((u.frame_count(), u.time_of_frame(0)), (10, MediaTime::new(0, 15360)));
}

#[test]
fn speed_edits_and_unmappable_lists_take_the_grid() {
    let stts = [(24, 512)];
    let grid_count = |edits: &[Edit]| {
        let t = FrameTiming::from_tables(&tables(24, 15360, &stts, &[], Some(edits), 1000));
        assert!(!t.has_table());
        (t.frame_count(), t.rate)
    };
    // An edit at rate 1/2 over 400 ms of media lasting 800 ms, then 400 ms: 1.2 s x 30 = 36 frames.
    let scaled = [Edit { duration: 800, media_time: 0, rate: 0x8000 }, Edit { duration: 400, media_time: 6144, rate: Edit::RATE_ONE }];
    assert_eq!(grid_count(&scaled), (36, r(30, 1)));
    // An edit past the media: nothing maps.
    assert_eq!(grid_count(&[Edit { duration: 500, media_time: 1 << 30, rate: Edit::RATE_ONE }]).0, 15);
    // A negative media time other than -1.
    assert_eq!(grid_count(&[Edit { duration: 500, media_time: -5, rate: Edit::RATE_ONE }]).0, 15);
    // Control: the same list at rate 1 maps.
    let ok = FrameTiming::from_tables(&tables(24, 15360, &stts, &[], Some(&[Edit { duration: 500, media_time: 0, rate: Edit::RATE_ONE }]), 1000));
    assert!(ok.has_table());
    assert_eq!(ok.frame_count(), 15);
}

#[test]
fn hostile_tables_never_panic_or_hang() {
    let stts = [(1000, 1)];
    // Every edit maps all 1000 samples, 65536 times: past MAX_SAMPLES, so the grid (bounded).
    let many = vec![Edit { duration: 1000, media_time: 0, rate: Edit::RATE_ONE }; MAX_EDITS as usize];
    let t0 = std::time::Instant::now();
    let t = FrameTiming::from_tables(&tables(1000, 1000, &stts, &[], Some(&many), 1000));
    assert!(!t.has_table());
    assert_eq!(t.frame_count(), MAX_SAMPLES, "grid count clamped to MAX_SAMPLES");
    assert!(t0.elapsed().as_secs() < 30, "bounded work");
    // Control: 64 such edits fit (64 000 frames from 1000 samples).
    let some = FrameTiming::from_tables(&tables(1000, 1000, &stts, &[], Some(&many[..64]), 1000));
    assert!(some.has_table());
    assert_eq!((some.frame_count(), some.sample_of_frame(63_999)), (64_000, Some(999)));
    // Overflowing durations and offsets, zero time scales.
    let huge = [Edit { duration: u64::MAX, media_time: i64::MAX, rate: Edit::RATE_ONE }, Edit { duration: u64::MAX, media_time: 0, rate: Edit::RATE_ONE }];
    for (ts, mts) in [(1000, 1000), (0, 1000), (1000, 0), (u32::MAX, 7), (0, 0)] {
        let t = FrameTiming::from_tables(&tables(1000, ts, &[(1000, u32::MAX)], &[i64::from(i32::MIN), i64::from(i32::MAX)], Some(&huge), mts));
        let _ = (t.time_of_frame(u32::MAX), t.frame_at(MediaTime::new(i64::MIN, 0)), t.frame_at(MediaTime::new(i64::MAX, 1)), t.sample_of_frame(0));
    }
    // No stts at all, no samples.
    let t = FrameTiming::from_tables(&tables(0, 600, &[], &[], None, 600));
    assert_eq!(t.frame_count(), 0);
    assert_eq!(t.sample_of_frame(0), None);
}

// ---- Container: elst / ctts as read ----

fn hap1_samples(n: u8) -> Vec<Vec<u8>> {
    (0..n).map(|i| qt::hap_frame(0xBB, &[i; 8])).collect()
}

fn movie(bytes: Vec<u8>) -> Movie {
    Movie::from_source(Arc::new(MemSource(Arc::from(bytes)))).unwrap()
}

fn track(m: &Movie) -> &VideoTrack {
    m.first().unwrap()
}

#[test]
fn the_container_reads_elst_ctts_and_mvhd() {
    let ctts = [(1u32, 2i32), (1, 4), (1, -2), (1, 0)];
    let t = qt::Timed {
        stts: &[(4, 1)],
        ctts: Some(&ctts),
        elst: Some(qt::elst_v0(&[(10, -1, 0x1_0000), (3, 1, 0x1_0000)])),
        media_timescale: 10,
        movie_timescale: 100,
    };
    let m = movie(qt::timed_movie(b"Hap1", 4, 4, &hap1_samples(4), &t));
    let v = track(&m);
    assert_eq!(v.movie_timescale, 100);
    assert_eq!(v.ctts, [2, 4, -2, 0]);
    assert_eq!(v.edits.as_deref(), Some(&[Edit { duration: 10, media_time: -1, rate: 0x1_0000 }, Edit { duration: 3, media_time: 1, rate: 0x1_0000 }][..]));
    // pts 2, 5, 0, 3; edit [1 media, +3 movie units = 0.3 media) at 0.1 s: only sample 2... pts in [1, 1.3): none
    // but sample 0 (pts 2) is outside too: nothing maps -> the grid.
    assert!(!v.timing.has_table());
    // Control: an edit covering the media maps every sample, in presentation order.
    let t2 = qt::Timed { elst: Some(qt::elst_v0(&[(10, -1, 0x1_0000), (60, 0, 0x1_0000)])), ..t };
    let m2 = movie(qt::timed_movie(b"Hap1", 4, 4, &hap1_samples(4), &t2));
    let v2 = track(&m2);
    assert_eq!((0..4).map(|n| v2.timing.sample_of_frame(n).unwrap()).collect::<Vec<_>>(), [2, 0, 3, 1]);
    assert_eq!(v2.timing.time_of_frame(0).cmp_time(MediaTime::new(1, 10)), std::cmp::Ordering::Equal, "after the empty 0.1 s");
    assert_eq!((v2.frames(), v2.frame_count()), (4, 4));
}

#[test]
fn unreadable_edit_lists_are_not_applied() {
    let base = |elst: Vec<u8>| qt::Timed { stts: &[(4, 1)], ctts: None, elst: Some(elst), media_timescale: 10, movie_timescale: 10 };
    // Count past the atom; count above MAX_EDITS; zero entries.
    let mut past = qt::elst_v0(&[(4, 1, 0x1_0000)]);
    past[4..8].copy_from_slice(&5u32.to_be_bytes());
    let mut over = qt::elst_v0(&[]);
    over[4..8].copy_from_slice(&(MAX_EDITS + 1).to_be_bytes());
    for (elst, want) in [(past, None), (over, None), (qt::elst_v0(&[]), Some(0usize))] {
        let m = movie(qt::timed_movie(b"Hap1", 4, 4, &hap1_samples(4), &base(elst)));
        let v = track(&m);
        assert_eq!(v.edits.as_ref().map(Vec::len), want);
        assert_eq!((v.frames(), v.timing.time_of_frame(0)), (4, MediaTime::new(0, 10)), "as without an edit list");
    }
    // Control: the same entry, count right, trims sample 0.
    let m = movie(qt::timed_movie(b"Hap1", 4, 4, &hap1_samples(4), &base(qt::elst_v0(&[(3, 1, 0x1_0000)]))));
    assert_eq!(track(&m).frames(), 3);
}

// ---- Decode paths: the committed edit fixtures ----

fn frame_ids(path: &str) -> (u32, Vec<u32>) {
    let opts = ImportOptions::default();
    let info = source::probe(path, &opts).unwrap();
    let mut src = source::open(&info, &opts, false).unwrap();
    let ids = (0..info.frames).map(|f| {
        let d = src.decode(f).unwrap();
        assert_eq!(d.index(), f);
        let (w, h) = d.size();
        read_frame_id(&d.to_rgba().unwrap(), w, h).unwrap()
    });
    (info.frames, ids.collect())
}

fn path(name: &str) -> String {
    fixtures().join(name).to_str().unwrap().to_string()
}

#[test]
fn hap_frames_follow_the_edit_list() {
    let (n, ids) = frame_ids(&path("edit_hap1_empty_trim_repeat.mov"));
    // The third edit starts 7 ms into the media: 108 / 15360, inside sample 0 (source frame 5), so
    // by Swift's rule (a frame is a sample whose START lies in the edit) it plays source 6..8.
    let want: Vec<u32> = (5..15).chain(2..6).chain(6..9).collect();
    assert_eq!((n, &ids), (17, &want));
    // Control: the samples in table order are only 14 and never repeat.
    let m = Movie::open(&fixtures().join("edit_hap1_empty_trim_repeat.mov")).unwrap();
    assert_eq!(track(&m).frame_count(), 14);
    let t = &track(&m).timing;
    assert_eq!(t.time_of_frame(0).cmp_time(MediaTime::new(7, 1000)), std::cmp::Ordering::Equal);
}

#[test]
fn dxv_frames_follow_the_edit_list() {
    let (n, ids) = frame_ids(&path("edit_dxt1_empty_trim.mov"));
    assert_eq!((n, ids), (9, (4..13).collect::<Vec<u32>>()));
    // Control: the file holds all 24 source samples; sample n is source frame n.
    let m = Movie::open(&fixtures().join("edit_dxt1_empty_trim.mov")).unwrap();
    assert_eq!(track(&m).frame_count(), 24);
}

#[test]
fn a_speed_edit_plays_on_the_grid() {
    let info = source::probe(&path("edit_hap1_scaled.mov"), &ImportOptions::default()).unwrap();
    assert_eq!((info.frames, info.rate), (36, r(30, 1)));
    let m = Movie::open(&fixtures().join("edit_hap1_scaled.mov")).unwrap();
    assert!(!track(&m).timing.has_table());
    // Grid fallback: frame n shows the n-th sample in presentation order, the last past the end.
    assert_eq!((track(&m).timing.sample_of_frame(5), track(&m).timing.sample_of_frame(35)), (Some(5), Some(23)));
}

#[test]
fn shuffled_seeks_equal_sequential_decode_on_edited_movies() {
    for name in ["edit_hap1_empty_trim_repeat.mov", "edit_dxt1_empty_trim.mov", "edit_hap1_scaled.mov"] {
        let opts = ImportOptions::default();
        let info = source::probe(&path(name), &opts).unwrap();
        let mut a = source::open(&info, &opts, false).unwrap();
        let seq: Vec<Vec<u8>> = (0..info.frames).map(|f| a.decode(f).unwrap().to_rgba().unwrap()).collect();
        let mut b = source::open(&info, &opts, false).unwrap();
        let mut order: Vec<u32> = (0..info.frames).collect();
        let mut s = 0x9E37_79B9u32;
        for i in (1..order.len()).rev() {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            order.swap(i, s as usize % (i + 1));
        }
        for f in order {
            assert_eq!(b.decode(f).unwrap().to_rgba().unwrap(), seq[f as usize], "{name} frame {f}");
        }
        assert!(b.decode(info.frames).is_err(), "{name}: past the last frame is an error");
        assert_eq!(b.timing().frame_count(), info.frames);
    }
}
