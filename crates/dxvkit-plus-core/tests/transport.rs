//! W4 (0.11.0): the transport on real frame starts. A clip with a table timing (jittered samples,
//! edit lists: the W3 fixtures) maps time to frame through `FrameTiming::frame_at`, at every frame
//! start and mid-frame; a grid clip behaves as before. Negative controls: the grid clip of the same
//! frame count and rate shows other frames on the jittered clip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[path = "../../../tests/support/qt_movie.rs"]
mod qt;

use dxvkit_plus_core::container::{MemSource, Movie};
use dxvkit_plus_core::time::{FrameRate, TICKS_PER_SECOND, Tick};
use dxvkit_plus_core::timing::{FrameTiming, MediaTime, RateSource};
use dxvkit_plus_core::transport::{Clip, Direction, Inputs, PlayMode, Settings, Transport};
use std::path::PathBuf;
use std::sync::Arc;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn timing(name: &str) -> Arc<FrameTiming> {
    Arc::new(Movie::open(&fixtures().join(name)).unwrap().tracks[0].timing.clone())
}

/// The first tick at or after a media time.
fn ceil_ticks(m: MediaTime) -> Tick {
    let n = i128::from(m.value) * i128::from(TICKS_PER_SECOND);
    let ts = i128::from(m.timescale);
    Tick((n / ts + i128::from(n % ts != 0)) as i64)
}

/// A media time for a tick, floored to `ts` units (exact for `start <= t`).
fn media(t: Tick, ts: u32) -> MediaTime {
    MediaTime::new((i128::from(t.0) * i128::from(ts)).div_euclid(i128::from(TICKS_PER_SECOND)) as i64, ts)
}

/// The frame a fresh transport (speed 1, loop) shows at movie time `t` (a clock running from 0).
fn shown(clip: &Clip, t: Tick) -> u32 {
    let mut tr = Transport::new(0);
    let s = Settings::default();
    // The clock advances in 5 s steps (the transport re-anchors on a jump past MAX_STEP, 10 s).
    let mut now = Tick::ZERO;
    tr.advance(now, clip, &s, &Inputs::default());
    while t.0 - now.0 > 5 * TICKS_PER_SECOND {
        now = Tick(now.0 + 5 * TICKS_PER_SECOND);
        tr.advance(now, clip, &s, &Inputs::default());
    }
    tr.advance(t, clip, &s, &Inputs::default()).frame
}

const CLIPS: [&str; 4] = ["jitter_prores4444_30.mov", "edit_hap1_empty_trim_repeat.mov", "edit_dxt1_empty_trim.mov", "edit_h264_bframes.mov"];

#[test]
fn time_to_frame_follows_frame_timing_at_every_start_and_mid_frame() {
    let mut grid_differs = 0;
    for name in CLIPS {
        let ft = timing(name);
        assert!(ft.has_table(), "{name}");
        let clip = Clip::from_timing(ft.clone());
        let grid = Clip::new(ft.frame_count(), ft.rate);
        let n = ft.frame_count();
        let mut checked = 0;
        for k in 0..n {
            let a = ceil_ticks(ft.time_of_frame(k));
            let b = if k + 1 < n { ceil_ticks(ft.time_of_frame(k + 1)) } else { ceil_ticks(ft.end_time()) };
            assert!(b > a, "{name}: starts increase");
            for t in [a, Tick((a.0 + b.0) / 2), Tick(b.0 - 1)] {
                let want = ft.frame_at(media(t, ft.timescale()));
                assert_eq!(want, k, "{name}: every tick of [start k, start k+1) is frame k");
                assert_eq!(shown(&clip, t), want, "{name}: frame {k}, tick {t:?}");
                assert_eq!(clip.frame_at(t), want, "{name}: Clip::frame_at");
                grid_differs += usize::from(shown(&grid, t) != want);
                checked += 1;
            }
        }
        assert_eq!(checked, 3 * n as usize);
    }
    // Negative control: on these clips the rate grid shows other frames somewhere.
    assert!(grid_differs > 0);
}

#[test]
fn the_clip_loops_at_the_track_end_and_a_late_first_frame_shows_from_zero() {
    // edit_hap1_empty_trim_repeat starts with an empty 7 ms edit: frame 0 starts at 7 ms.
    let ft = timing("edit_hap1_empty_trim_repeat.mov");
    assert!(ft.time_of_frame(0).cmp_time(MediaTime::new(7, 1000)).is_eq());
    let clip = Clip::from_timing(ft.clone());
    assert_eq!(shown(&clip, Tick(1)), 0);
    // The loop length is the track's end on the movie timeline (the summed edits).
    let end = ceil_ticks(ft.end_time());
    assert_eq!(shown(&clip, Tick(end.0 - 1)), ft.frame_count() - 1);
    assert_eq!(shown(&clip, end), 0);
    // Control: a grid clip of the same frames loops at n frames of the rate (7 ms earlier): just
    // before the track's end it has already wrapped, not still on the last frame.
    let grid = Clip::new(ft.frame_count(), ft.rate);
    assert_ne!(ceil_ticks(ft.end_time()), ft.rate.tick_of(i64::from(ft.frame_count())));
    assert_ne!(shown(&grid, Tick(end.0 - 1)), ft.frame_count() - 1);
}

#[test]
fn seek_step_and_bounce_land_on_real_starts() {
    let ft = timing("jitter_prores4444_30.mov");
    let clip = Clip::from_timing(ft.clone());
    let s = Settings::default();
    let mut tr = Transport::new(0);
    for k in [0, 3, 10, 17, 23, 5] {
        tr.seek(k, &clip, &s);
        assert_eq!(tr.advance(Tick::ZERO, &clip, &s, &Inputs::default()).frame, k);
        assert_eq!(tr.position_ticks(), ceil_ticks(ft.time_of_frame(k)), "frame {k}: the playhead is the real start");
    }
    let p = Settings { direction: Direction::Pause, ..Settings::default() };
    tr.seek(2, &clip, &p);
    tr.step(1, &clip, &p);
    assert_eq!(tr.advance(Tick(5), &clip, &p, &Inputs::default()).frame, 3);
    assert_eq!(tr.position_ticks(), ceil_ticks(ft.time_of_frame(3)));
    // Control: on the grid, frame 3 (one tick late in this clip) starts elsewhere.
    assert_ne!(ceil_ticks(ft.time_of_frame(3)), ft.rate.tick_of(3));
    // Bounce: up through every frame, then back down, each frame shown on its own times.
    let b = Settings { mode: PlayMode::Bounce, ..Settings::default() };
    let mut tr = Transport::new(0);
    let n = ft.frame_count();
    let step = Tick(TICKS_PER_SECOND / 240);
    let mut seen = Vec::new();
    let mut t = Tick::ZERO;
    tr.advance(t, &clip, &b, &Inputs::default());
    while seen.len() < 2 || seen.last() != Some(&0) || seen.len() < 2 * n as usize - 1 {
        t = Tick(t.0 + step.0);
        let f = tr.advance(t, &clip, &b, &Inputs::default()).frame;
        if seen.last() != Some(&f) {
            seen.push(f);
        }
        assert!(seen.len() < 4 * n as usize, "{seen:?}");
    }
    let want: Vec<u32> = (0..n).chain((0..n - 1).rev()).collect();
    assert_eq!(seen[..want.len()], want[..]);
}

#[test]
fn nearest_frame_is_swifts_round_on_the_grid_and_the_nearest_start_on_a_table() {
    let g = FrameTiming::grid(FrameRate::FPS_30, 100, RateSource::MeanRate);
    // round(seconds x 30): 1.5 frames -> 2 (a tie goes up, as Swift's `rounded()`), 1.49 -> 1.
    assert_eq!(g.nearest_frame(MediaTime::new(3, 60)), 2);
    assert_eq!(g.nearest_frame(MediaTime::new(149, 3000)), 1);
    assert_eq!(g.nearest_frame(MediaTime::new(-1, 30)), 0);
    assert_eq!(g.nearest_frame(MediaTime::new(1000, 1)), 99);
    // Control: frame_at floors.
    assert_eq!(g.frame_at(MediaTime::new(3, 60)), 1);
    // A table: frame 3 of the jitter clip starts at 61 / 600 (one tick late), frame 2 at 40 / 600.
    let ft = timing("jitter_prores4444_30.mov");
    assert!(ft.time_of_frame(3).cmp_time(MediaTime::new(61, 600)).is_eq());
    assert_eq!(ft.nearest_frame(MediaTime::new(50, 600)), 2, "10 / 600 after frame 2, 11 before frame 3");
    assert_eq!(ft.nearest_frame(MediaTime::new(51, 600)), 3, "a tie between 40 and 61 does not exist at 50.5; 51 is nearer 61");
    assert_eq!(g.nearest_frame(MediaTime::new(50, 600)), 3, "control: the grid's round(2.5) = 3");
    // The transport's Clip agrees at tick resolution.
    let clip = Clip::from_timing(ft.clone());
    assert_eq!(clip.nearest_frame(ceil_ticks(MediaTime::new(50, 600))), 2);
    assert_eq!(Clip::new(100, FrameRate::FPS_30).nearest_frame(ceil_ticks(MediaTime::new(3, 60))), 2);
}

#[test]
fn a_grid_clip_is_unchanged_whether_or_not_it_carries_its_timing() {
    let ft = Arc::new(FrameTiming::grid(FrameRate::FPS_29_97, 300, RateSource::MeanRate));
    let a = Clip::from_timing(ft);
    let b = Clip::new(300, FrameRate::FPS_29_97);
    for s in [
        Settings::default(),
        Settings { mode: PlayMode::Bounce, speed: 1.7, ..Settings::default() },
        Settings { direction: Direction::Backward, in_frame: 10, out_frame: 200, ..Settings::default() },
    ] {
        let (mut ta, mut tb) = (Transport::new(1), Transport::new(1));
        for i in 0..2000i64 {
            let t = FrameRate::FPS_60.tick_of(i);
            assert_eq!(ta.advance(t, &a, &s, &Inputs::default()), tb.advance(t, &b, &s, &Inputs::default()), "{s:?} at {i}");
        }
    }
    // Control: a clip carrying a table timing (one frame a tick late) does not play like its grid.
    let jitter = timing("jitter_prores4444_30.mov");
    let (t, g) = (Clip::from_timing(jitter.clone()), Clip::new(jitter.frame_count(), jitter.rate));
    let at = Tick(ceil_ticks(MediaTime::new(60, 600)).0);
    assert_ne!(shown(&t, at), shown(&g, at));
}

#[test]
fn a_tie_on_a_table_goes_to_the_later_frame() {
    // Frames at 0 and 2 / 600 (a table: stts deltas 2, 4): 1 / 600 is equally near both.
    let samples = vec![qt::hap_frame(0xAB, &[0u8; 8]); 3];
    let t = qt::Timed { stts: &[(1, 2), (2, 4)], ctts: None, elst: None, media_timescale: 600, movie_timescale: 600 };
    let movie = Movie::from_source(Arc::new(MemSource(Arc::from(qt::timed_movie(b"Hap1", 4, 4, &samples, &t))))).unwrap();
    let ft = &movie.tracks[0].timing;
    assert!(ft.has_table());
    assert!(ft.time_of_frame(1).cmp_time(MediaTime::new(2, 600)).is_eq());
    assert_eq!(ft.nearest_frame(MediaTime::new(1, 600)), 1);
    // Controls: just before the midpoint is frame 0; frame_at floors at the midpoint.
    assert_eq!(ft.nearest_frame(MediaTime::new(1999, 1_200_000)), 0);
    assert_eq!(ft.frame_at(MediaTime::new(1, 600)), 0);
}
