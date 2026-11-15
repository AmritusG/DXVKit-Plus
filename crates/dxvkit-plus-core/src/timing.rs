//! Frame timing: what "frame n" means, for every decode path (0.10.0).
//!
//! The one rule is Swift Glance's `GlanceCore.FrameTiming` (glance@a9dc601
//! `Sources/GlanceCore/FrameTiming.swift`, the rule in glance `CLAUDE.md` "Frame timing of an
//! AVFoundation track has ONE rule"), ported from AVFoundation's sample cursor and track segments
//! onto the container's own tables (Apple, QuickTime File Format: `stts` time-to-sample, `ctts`
//! composition offsets, `elst` edit list, `mdhd` media time scale, `mvhd` movie time scale):
//!
//! - **Frame starts.** Every sample's presentation time (decode time from `stts` plus its `ctts`
//!   offset, in the media time scale) is mapped through the edit list onto the movie timeline: for
//!   each non-empty edit, the samples whose presentation time lies in `[media_time, media_time +
//!   duration)` start at `time - media_time + (the edit's start on the movie timeline)`. Samples
//!   outside every edit are dropped; a sample inside two edits is two frames. The starts, sorted, are
//!   the frames: frame `n` is the `n`-th of them, [`FrameTiming::time_of_frame`] its real start
//!   (an exact rational), [`FrameTiming::frame_at`] the inverse (floor: a frame's own start is that
//!   frame), [`FrameTiming::sample_of_frame`] the sample that shows it. Empty edits only shift the
//!   later edits (they are not frames). Without non-empty edits the frames are the samples'
//!   presentation times as stored (no shift).
//! - **Rate**, an exact rational, by Swift's rules in order: the mean (sample count / the samples'
//!   summed duration, else / the track duration) snapped to a standard rate within 0.1 %; the
//!   dominant sample duration, snapped the same way; the shortest sample duration when it agrees
//!   with the nominal (mean) rate within 0.2 %; the nominal rate, snapped when standard, else in
//!   thousandths; the shortest sample duration alone; else 30. Never the shortest sample alone when
//!   a better rule applies (25 fps read as 26.09), never seconds rounded to 1/600.
//! - **Grid fallback** (Swift's too): when an edit plays at another speed (rate != 1), when no
//!   sample falls in any edit, or when the times do not fit (see below), the starts are the grid
//!   `n x frame duration` and the count is `round(track seconds x rate)`; frame `n` then shows the
//!   `n`-th sample in presentation order (the last one past the end).
//!
//! AVFoundation's inputs, from the container: a sample's duration is what AVFoundation's sample
//! cursor reports — the next presentation time minus its own, the last sample presented keeping its
//! `stts` delta (measured on 173 movies; it differs from `stts` on B-frame streams); the "nominal"
//! rate is the mean (sample count / summed duration, unsnapped), the minimum frame duration the
//! shortest sample duration, the track duration the sum of the edit durations (movie scale) or,
//! without an edit list, the media duration. The cross-test against Swift FrameTiming
//! on every movie of the test folder is `docs/data/w3-crosstest-*.jsonl`.
//!
//! Hostile tables never panic or hang: every count is bounded by the sample count (at most
//! [`crate::container::MAX_SAMPLES`]), mapped frames are capped there too, edits are binary-searched
//! (no samples x edits loop), and the common time scale must fit a `CMTime` (i32); anything else
//! takes the grid fallback.

use crate::container::MAX_SAMPLES;
use crate::time::FrameRate;
use std::cmp::Ordering;

/// An exact time: `value / timescale` seconds. Compare with [`MediaTime::cmp_time`] (two
/// spellings of the same time compare equal there, not with `==`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[repr(C)]
pub struct MediaTime {
    pub value: i64,
    pub timescale: u32,
}

impl MediaTime {
    pub fn new(value: i64, timescale: u32) -> MediaTime {
        MediaTime { value, timescale }
    }
    /// Seconds (display only; never compute with it).
    pub fn seconds(self) -> f64 {
        self.value as f64 / f64::from(self.timescale.max(1))
    }
    /// Exact order of two times (a zero time scale counts as 1).
    pub fn cmp_time(self, other: MediaTime) -> Ordering {
        let a = i128::from(self.value).saturating_mul(i128::from(other.timescale.max(1)));
        let b = i128::from(other.value).saturating_mul(i128::from(self.timescale.max(1)));
        a.cmp(&b)
    }
}

/// One edit-list entry (`elst`), as stored. Ported from Loombda-rs `loombda-media`
/// (`container.rs` `Edit`, Amrit Rosell, MIT by the author's leave — `ATTRIBUTION.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[repr(C)]
pub struct Edit {
    /// Length on the movie timeline, in movie (`mvhd`) time-scale units.
    pub duration: u64,
    /// Where the edit starts in the media, in media (`mdhd`) units; -1 = an empty edit.
    pub media_time: i64,
    /// Playback rate, 16.16 fixed point ([`Edit::RATE_ONE`] = normal speed).
    pub rate: i32,
}

impl Edit {
    /// Rate 1.0 in 16.16 fixed point.
    pub const RATE_ONE: i32 = 0x1_0000;
    /// An empty edit (QTFF: media time -1) plays nothing for its duration.
    pub fn is_empty(&self) -> bool {
        self.media_time == -1
    }
}

/// Which rule gave the rate (Swift's `FrameTiming.Source`). `#[repr(u32)]`, values pinned by
/// `tests/abi.rs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum RateSource {
    MeanRate = 0,
    DominantDelta = 1,
    MinFrameDuration = 2,
    Nominal = 3,
    #[default]
    Fallback = 4,
}

/// The rate inputs AVFoundation hands Swift, from whatever the caller has.
#[derive(Clone, Copy, Debug, Default)]
pub struct RateInputs<'a> {
    /// Sample durations in `timescale` units, decode order (empty when unknown).
    pub durations: &'a [i64],
    pub timescale: u32,
    /// The track's duration in seconds (0 when unknown).
    pub track_seconds: f64,
    /// The shortest frame duration as (value, timescale), when known.
    pub min_frame_duration: Option<(i64, u32)>,
    /// The nominal frame rate (0 when unknown).
    pub nominal: f64,
}

/// Standard rates, exact (Swift's `FrameTiming.standardRates`, same order).
pub const STANDARD_RATES: [FrameRate; 11] = [
    FrameRate::FPS_23_976,
    FrameRate::FPS_24,
    FrameRate::FPS_25,
    FrameRate::FPS_29_97,
    FrameRate::FPS_30,
    FrameRate::FPS_48,
    FrameRate::FPS_50,
    FrameRate::FPS_59_94,
    FrameRate::FPS_60,
    FrameRate { num: 100, den: 1 },
    FrameRate::FPS_120,
];

/// The nearest standard rate to `fps` when within 0.1 % of it (the nearest first: 29.97 and 30
/// differ by 0.1 % too). Swift's `FrameTiming.snap`, same `f64` arithmetic.
pub fn snap(fps: f64) -> Option<FrameRate> {
    if !fps.is_finite() || fps <= 0.0 {
        return None;
    }
    let mut best = FrameRate::FPS_23_976;
    for r in STANDARD_RATES {
        if (r.as_f64() - fps).abs() < (best.as_f64() - fps).abs() {
            best = r;
        }
    }
    let r = best.as_f64();
    ((r - fps).abs() / r <= 0.001).then_some(best)
}

/// A rate as `num / den` frames per second, if both fit (`num` in u32 so a frame time fits a
/// [`MediaTime`]).
fn rate_checked(num: i64, den: i64) -> Option<FrameRate> {
    (num > 0 && den > 0 && u32::try_from(num).is_ok()).then(|| FrameRate::new(num, den))
}

/// The rate from a track's timing: Swift's `FrameTiming.rate`, rules in the module doc.
pub fn rate_of(i: &RateInputs<'_>) -> (FrameRate, RateSource) {
    let summed: i128 = i.durations.iter().map(|&d| i128::from(d)).sum();
    let summed_s = if i.timescale > 0 { summed as f64 / f64::from(i.timescale) } else { 0.0 };
    let span = if summed_s > 0.0 { summed_s } else { i.track_seconds };
    if !i.durations.is_empty()
        && span > 0.0
        && let Some(s) = snap(i.durations.len() as f64 / span)
    {
        return (s, RateSource::MeanRate);
    }
    if i.timescale > 0 && !i.durations.is_empty() {
        let mut sorted: Vec<i64> = i.durations.iter().copied().filter(|&d| d > 0).collect();
        sorted.sort_unstable();
        // The most frequent duration; among equally frequent ones the shortest (Swift's ordering).
        let (mut best, mut best_n, mut k) = (None::<i64>, 0usize, 0usize);
        while let Some(&d) = sorted.get(k) {
            let n = sorted.get(k..).map_or(0, |rest| rest.iter().take_while(|&&x| x == d).count()).max(1);
            if n > best_n {
                (best, best_n) = (Some(d), n);
            }
            k = k.saturating_add(n);
        }
        if let Some(d) = best
            && let Some(s) = snap(f64::from(i.timescale) / d as f64)
        {
            return (s, RateSource::DominantDelta);
        }
    }
    let nominal = i.nominal;
    let min = i.min_frame_duration.filter(|&(v, ts)| v > 0 && ts > 0);
    if let Some((v, ts)) = min
        && nominal > 0.0
        && (v as f64 / f64::from(ts) * nominal - 1.0).abs() < 0.002
        && let Some(r) = rate_checked(i64::from(ts), v)
    {
        return (r, RateSource::MinFrameDuration);
    }
    if nominal > 0.0 && nominal.is_finite() {
        if let Some(s) = snap(nominal) {
            return (s, RateSource::Nominal);
        }
        if let Some(r) = rate_checked((nominal * 1000.0).round() as i64, 1000) {
            return (r, RateSource::Nominal);
        }
    }
    if let Some((v, ts)) = min
        && let Some(r) = rate_checked(i64::from(ts), v)
    {
        return (r, RateSource::MinFrameDuration);
    }
    (FrameRate::FPS_30, RateSource::Fallback)
}

/// A track's frame timing: the rate, the frame count, every frame's real start and the sample
/// that shows it. Build it with [`FrameTiming::from_tables`] (a QuickTime / MP4 track),
/// [`FrameTiming::grid`] (LMBD, image sequences, tracks without tables).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameTiming {
    pub rate: FrameRate,
    pub source: RateSource,
    /// Time scale of `starts` (the media time scale, or the least common multiple with the movie
    /// time scale when an edit starts between media ticks).
    timescale: u32,
    /// Start of every frame, presentation order; empty = the grid.
    starts: Vec<i64>,
    /// Table: the sample (decode order) of every frame. Grid: the samples in presentation order.
    samples: Vec<u32>,
    count: u32,
}

/// Inputs of [`FrameTiming::from_tables`]: a track's tables as stored.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tables<'a> {
    /// Number of samples (the `stsz` count).
    pub samples: u32,
    /// Media (`mdhd`) time scale.
    pub timescale: u32,
    /// Media duration in media units (`mdhd`).
    pub media_duration: u64,
    /// `stts` runs: (sample count, delta).
    pub stts: &'a [(u32, u32)],
    /// Composition offsets per sample, decode order (may be shorter than the samples: 0 after).
    pub ctts: &'a [i64],
    /// The edit list, `None` without one.
    pub edits: Option<&'a [Edit]>,
    /// Movie (`mvhd`) time scale.
    pub movie_timescale: u32,
}

impl FrameTiming {
    /// `count` frames at `rate` on the grid (`n x frame duration`); frame `n` is sample `n`.
    pub fn grid(rate: FrameRate, count: u32, source: RateSource) -> FrameTiming {
        let samples = (0..count.min(MAX_SAMPLES)).collect();
        FrameTiming { rate, source, timescale: 0, starts: Vec::new(), samples, count }
    }

    /// The timing of a track from its tables (module doc).
    pub fn from_tables(t: &Tables<'_>) -> FrameTiming {
        let n = t.samples.min(MAX_SAMPLES) as usize;
        let mut durations: Vec<i64> = Vec::with_capacity(n);
        for &(count, delta) in t.stts {
            let left = n.saturating_sub(durations.len());
            if left == 0 {
                break;
            }
            durations.extend(std::iter::repeat_n(i64::from(delta), (count as usize).min(left)));
        }
        // Presentation times, decode order: decode time (stts) + composition offset (ctts).
        let mut pts: Vec<i64> = Vec::with_capacity(n);
        let mut dts = 0i64;
        for k in 0..n {
            let off = t.ctts.get(k).copied().unwrap_or(0);
            pts.push(dts.saturating_add(off));
            dts = dts.saturating_add(durations.get(k).copied().unwrap_or(0));
        }
        // Samples in presentation order (ties by sample: deterministic).
        let mut order: Vec<u32> = (0..u32::try_from(n).unwrap_or(0)).collect();
        order.sort_by_key(|&s| (pts.get(s as usize).copied().unwrap_or(0), s));
        // Each sample's duration as AVFoundation's sample cursor reports it (measured on 173 movies,
        // docs/data/w3-avf-sample-durations.txt): the next presentation time minus its own; the last
        // sample presented keeps its stts delta. B-frame streams differ from stts there.
        let mut shown: Vec<i64> = vec![0; n];
        for (k, &s) in order.iter().enumerate() {
            let own = pts.get(s as usize).copied().unwrap_or(0);
            let d = match order.get(k.saturating_add(1)) {
                Some(&next) => pts.get(next as usize).copied().unwrap_or(0).saturating_sub(own),
                None => durations.get(s as usize).copied().unwrap_or(0),
            };
            if let Some(slot) = shown.get_mut(s as usize) {
                *slot = d;
            }
        }
        let durations = if durations.is_empty() { durations } else { shown };
        let summed: i128 = durations.iter().map(|&d| i128::from(d)).sum();
        let min = durations.iter().copied().filter(|&d| d > 0).min();
        let nominal = if summed > 0 && t.timescale > 0 { durations.len() as f64 / (summed as f64 / f64::from(t.timescale)) } else { 0.0 };
        let edits = t.edits.filter(|e| !e.is_empty());
        let track_seconds = match edits {
            Some(e) if t.movie_timescale > 0 => e.iter().map(|x| x.duration as f64).sum::<f64>() / f64::from(t.movie_timescale),
            _ if t.timescale > 0 => t.media_duration as f64 / f64::from(t.timescale),
            _ => 0.0,
        };
        let (rate, source) =
            rate_of(&RateInputs { durations: &durations, timescale: t.timescale, track_seconds, min_frame_duration: min.map(|v| (v, t.timescale)), nominal });
        match map_times(&pts, &order, t, edits) {
            Some((timescale, mapped)) => {
                let count = u32::try_from(mapped.len()).unwrap_or(0);
                let (starts, samples) = mapped.into_iter().unzip();
                FrameTiming { rate, source, timescale, starts, samples, count }
            }
            None => {
                let count = (track_seconds * rate.num as f64 / rate.den as f64).round();
                let count = if count.is_finite() && count > 0.0 { (count.min(f64::from(MAX_SAMPLES))) as u32 } else { 0 };
                FrameTiming { rate, source, timescale: 0, starts: Vec::new(), samples: order, count }
            }
        }
    }

    /// Number of frames.
    pub fn frame_count(&self) -> u32 {
        self.count
    }
    /// Whether the starts come from the sample table (not the grid fallback).
    pub fn has_table(&self) -> bool {
        !self.starts.is_empty()
    }
    /// One frame period at [`FrameTiming::rate`], exact.
    pub fn frame_duration(&self) -> MediaTime {
        MediaTime::new(self.rate.den, u32::try_from(self.rate.num).unwrap_or(u32::MAX))
    }
    fn last(&self) -> u32 {
        self.count.saturating_sub(1)
    }
    /// Start of frame `n` on the movie timeline (clamped to the track), exact.
    pub fn time_of_frame(&self, n: u32) -> MediaTime {
        let n = n.min(self.last());
        match self.starts.get(n as usize) {
            Some(&v) => MediaTime::new(v, self.timescale),
            None => {
                let d = self.frame_duration();
                MediaTime::new(i64::from(n).saturating_mul(d.value), d.timescale)
            }
        }
    }
    /// The frame showing at `t`: the last frame starting at or before it (floor; a frame's own
    /// start is that frame), clamped to the track; 0 before the first frame.
    pub fn frame_at(&self, t: MediaTime) -> u32 {
        if self.has_table() {
            let ts = self.timescale;
            // First frame starting after t, minus one.
            let after = self.starts.partition_point(|&s| MediaTime::new(s, ts).cmp_time(t) != Ordering::Greater);
            return u32::try_from(after.saturating_sub(1)).unwrap_or(0);
        }
        // floor(t x num / (den x t.timescale)), exact.
        let num = i128::from(t.value).saturating_mul(i128::from(self.rate.num));
        let den = i128::from(self.rate.den).saturating_mul(i128::from(t.timescale.max(1))).max(1);
        let f = num.div_euclid(den).max(0);
        u32::try_from(f).unwrap_or(u32::MAX).min(self.last())
    }
    /// The sample (decode order, the container's sample index) that shows frame `n`; `None` past
    /// the last frame.
    pub fn sample_of_frame(&self, n: u32) -> Option<u32> {
        if n >= self.count {
            return None;
        }
        if self.has_table() {
            return self.samples.get(n as usize).copied();
        }
        let k = (n as usize).min(self.samples.len().saturating_sub(1));
        self.samples.get(k).copied()
    }
    /// Every frame's start, presentation order, in [`FrameTiming::time_of_frame`]'s units (empty
    /// on the grid).
    pub fn starts(&self) -> impl Iterator<Item = MediaTime> + '_ {
        self.starts.iter().map(|&v| MediaTime::new(v, self.timescale))
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a.checked_rem(b).unwrap_or(0));
    }
    a
}

/// Media presentation times -> sorted movie-timeline times through the edit list (Swift's
/// `FrameTiming.movieTimes`), with the sample of each. `None` = the grid fallback.
fn map_times(pts: &[i64], order: &[u32], t: &Tables<'_>, edits: Option<&[Edit]>) -> Option<(u32, Vec<(i64, u32)>)> {
    let m = t.timescale;
    if m == 0 {
        return None;
    }
    let all = edits.unwrap_or_default();
    if all.iter().all(Edit::is_empty) {
        // No non-empty edit: the presentation times as stored.
        let out = order.iter().map(|&s| (pts.get(s as usize).copied().unwrap_or(0), s)).collect::<Vec<_>>();
        return (!out.is_empty()).then_some((m, out));
    }
    let v = t.movie_timescale;
    if v == 0 || all.iter().any(|e| !e.is_empty() && (e.rate != Edit::RATE_ONE || e.media_time < 0)) {
        return None;
    }
    // Each edit's start on the movie timeline (movie units), empty edits included.
    let mut targets = Vec::with_capacity(all.len());
    let mut acc = 0u64;
    for e in all {
        targets.push(acc);
        acc = acc.checked_add(e.duration)?;
    }
    // The common time scale: the media's, unless an edit starts between media ticks.
    let exact = all.iter().zip(&targets).all(|(e, &s)| e.is_empty() || u128::from(s).saturating_mul(u128::from(m)).checked_rem(u128::from(v)) == Some(0));
    let ts = if exact { u64::from(m) } else { u64::from(m).checked_div(gcd(u64::from(m), u64::from(v)))?.checked_mul(u64::from(v))? };
    let ts = u32::try_from(ts).ok().filter(|&x| i32::try_from(x).is_ok())?;
    let (fm, fv) = (i128::from(ts.checked_div(m)?), i128::from(ts));
    let sorted: Vec<i64> = order.iter().map(|&s| pts.get(s as usize).copied().unwrap_or(0)).collect();
    let mut out: Vec<(i64, u32)> = Vec::new();
    for (e, &start) in all.iter().zip(&targets) {
        if e.is_empty() {
            continue;
        }
        // Samples with media_time <= pts < media_time + duration (duration in movie units):
        // pts * v < media_time * v + duration * m.
        let end = i128::from(e.media_time).saturating_mul(i128::from(v)).saturating_add(i128::from(e.duration).saturating_mul(i128::from(m)));
        let lo = sorted.partition_point(|&p| p < e.media_time);
        let hi = sorted.partition_point(|&p| i128::from(p).saturating_mul(i128::from(v)) < end);
        // Edit start in the common scale: start (movie units) * ts / v.
        let base = i128::from(start).saturating_mul(fv).checked_div(i128::from(v))?;
        for k in lo..hi.max(lo) {
            if out.len() >= MAX_SAMPLES as usize {
                return None;
            }
            let (Some(&p), Some(&s)) = (sorted.get(k), order.get(k)) else { break };
            let time = i128::from(p).saturating_sub(i128::from(e.media_time)).saturating_mul(fm).saturating_add(base);
            out.push((i64::try_from(time).ok()?, s));
        }
    }
    if out.is_empty() {
        return None;
    }
    out.sort_unstable();
    Some((ts, out))
}
