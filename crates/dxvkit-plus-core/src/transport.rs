//! Transport — the playhead of one clip, as exact integer time.
//!
//! Pure logic, no I/O and no GPU: given the clip ([`Clip`]: frame count + exact frame rate), the
//! transport settings ([`Settings`]) and the engine clock's `Tick` each evaluation, it answers which
//! frame to show. The playhead `pos` is a [`Tick`] of media time measured from the in-point; it is
//! integrated from the clock delta times the speed, where the speed is held as an exact rational
//! (nano-units), so `speed = 1` advances exactly with the clock and frame `k` starts exactly at
//! `rate.tick_of(k)` — frame-accurate by construction, with no float accumulation.
//!
//! Capabilities (feature rows in the handoff):
//! - play modes: loop, bounce (ping-pong; each end frame shown once per cycle), once-and-hold
//!   (freeze on the last frame), once-and-eject (output cleared at the end), random frame;
//! - direction: forward, backward, pause, random (forward with random jumps every `random_interval`
//!   seconds — or beats in beat sync — of up to `random_max` frames);
//! - speed: any rational multiplier incl. negative (= reverse) up to ±[`MAX_SPEED`]; a duration
//!   from which the speed is derived; the non-linear 0–10 speed knob ([`speed_from_knob`]);
//! - in/out range (loop range), frame-accurate seek, scrub (0..1), frame step, restart, cue points;
//! - sync: timeline (the clock), beats (the clip spans `beats` beats at the incoming BPM, with a
//!   beat-count guess, double/halve and power-of-two multipliers, and the instant beat-quantised
//!   loop with catch-up on release), or timecode input 1 / 2 (the position chases an incoming
//!   time in seconds — LTC / MTC readers live in the host application);
//! - stills: a one-frame clip whose length is a duration in seconds or beats (`still_duration`).

use crate::time::{FrameRate, TICKS_PER_SECOND, Tick};
use serde::{Deserialize, Serialize};

/// Largest |speed| (multiplier). Sized past "clearly excessive" for VJ use (Arena's knob tops out
/// at 10x); the transport itself is exact at any rational speed.
pub const MAX_SPEED: f64 = 100.0;
/// Speed is held as `speed * SPEED_DEN` (exact rational, nano-units).
pub const SPEED_DEN: i64 = 1_000_000_000;
/// Longest clock step integrated in one evaluation (a stalled or jumping clock re-anchors instead
/// of fast-forwarding the playhead through hours of media).
pub const MAX_STEP: Tick = Tick(TICKS_PER_SECOND.saturating_mul(10));

/// What happens at the ends of the play range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum PlayMode {
    #[default]
    Loop = 0,
    /// Ping-pong: 0, 1, … N-1, N-2, … 1, 0, 1 … (each end frame once per cycle).
    Bounce = 1,
    /// Play once, then freeze on the last frame (first frame when playing backward).
    OnceHold = 2,
    /// Play once, then clear the output (Arena's "play once and eject", Millumin's "once then stop").
    OnceEject = 3,
    /// Show a random frame of the range each frame step (MadMapper's "random frame").
    RandomFrame = 4,
}

impl PlayMode {
    pub const ALL: [PlayMode; 5] = [PlayMode::Loop, PlayMode::Bounce, PlayMode::OnceHold, PlayMode::OnceEject, PlayMode::RandomFrame];
    pub fn from_index(i: i64) -> Option<PlayMode> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }
    pub fn index(self) -> i64 {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0) as i64
    }
}

/// Playback direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Direction {
    #[default]
    Forward = 0,
    Backward = 1,
    Pause = 2,
    /// Forward with random jumps (interval and max distance are settings).
    Random = 3,
}

impl Direction {
    pub const ALL: [Direction; 4] = [Direction::Forward, Direction::Backward, Direction::Pause, Direction::Random];
    pub fn from_index(i: i64) -> Option<Direction> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }
    pub fn index(self) -> i64 {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0) as i64
    }
}

/// What drives the playhead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum SyncMode {
    /// The engine clock times the speed.
    #[default]
    Timeline = 0,
    /// The clip's range spans `beats` beats at the incoming tempo.
    Beats = 1,
    /// The position chases timecode input 1 (seconds).
    Timecode1 = 2,
    /// The position chases timecode input 2 (seconds).
    Timecode2 = 3,
}

impl SyncMode {
    pub const ALL: [SyncMode; 4] = [SyncMode::Timeline, SyncMode::Beats, SyncMode::Timecode1, SyncMode::Timecode2];
    pub fn from_index(i: i64) -> Option<SyncMode> {
        Self::ALL.get(usize::try_from(i).ok()?).copied()
    }
    pub fn index(self) -> i64 {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0) as i64
    }
}

/// The clip as the transport sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clip {
    /// Frames in the file (>= 1).
    pub frames: u32,
    /// Exact nominal rate.
    pub rate: FrameRate,
}

/// Transport settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub mode: PlayMode,
    pub direction: Direction,
    pub sync: SyncMode,
    /// Multiplier, -MAX_SPEED..=MAX_SPEED; negative plays backward.
    pub speed: f64,
    /// First frame of the play range (inclusive).
    pub in_frame: u32,
    /// End of the play range (exclusive); 0 = the clip's end.
    pub out_frame: u32,
    /// Random direction: seconds (timeline) or beats (beat sync) between jumps.
    pub random_interval: f64,
    /// Random direction: largest jump in frames (0 = anywhere in the range).
    pub random_max: u32,
    /// Beat sync: the range's length in beats (0 = guess from the tempo, see [`guess_beats`]).
    pub beats: f64,
    /// Beat sync: power-of-two speed multiplier (1/64..=64).
    pub beat_multiplier: f64,
    /// Beat sync: instant loop length in beats (0 = off).
    pub instant_loop: f64,
    /// Timecode sync: subtracted from the incoming time (seconds).
    pub timecode_offset: f64,
    /// Stills: the length of a one-frame clip, in seconds (timeline) or beats (beat sync).
    pub still_duration: f64,
    /// RNG seed for random frame / random direction (deterministic, reproducible).
    pub seed: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: PlayMode::Loop,
            direction: Direction::Forward,
            sync: SyncMode::Timeline,
            speed: 1.0,
            in_frame: 0,
            out_frame: 0,
            random_interval: 1.0,
            random_max: 0,
            beats: 0.0,
            beat_multiplier: 1.0,
            instant_loop: 0.0,
            timecode_offset: 0.0,
            still_duration: 5.0,
            seed: 0,
        }
    }
}

/// What the transport says for this evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// The file frame to show.
    pub frame: u32,
    /// False after a once-and-eject end: the output is cleared.
    pub visible: bool,
    /// True once a once-mode clip (or a still's duration) has ended. Edge for auto-follow.
    pub ended: bool,
    /// Position in the play range, 0..=1.
    pub position: f64,
    /// +1 / -1: the direction frames are being consumed (read-ahead hint).
    pub heading: i8,
}

/// External inputs for one evaluation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Inputs {
    /// Tempo (beats per minute) from the control input, if connected.
    pub bpm: Option<f64>,
    /// Timecode inputs 1 and 2 (seconds), if connected.
    pub timecode: [Option<f64>; 2],
}

/// The instant loop (beat sync): playback shows `start + (shadow - anchor) mod len` while the
/// shadow playhead keeps running; releasing jumps to the shadow (catch-up).
#[derive(Clone, Copy, Debug, PartialEq)]
struct InstantLoop {
    start: Tick,
    len: Tick,
    anchor: Tick,
}

/// The playhead state.
#[derive(Clone, Debug, PartialEq)]
pub struct Transport {
    /// Media time from the in-point (loop: in `[0, L)`, bounce: in `[0, P)`).
    pos: Tick,
    last: Option<Tick>,
    ended: bool,
    rng: u64,
    /// Time since the last random jump (random direction).
    since_jump: Tick,
    /// Random frame mode: the frame step last drawn and the frame shown for it.
    random_step: i64,
    random_frame: u32,
    /// Unwrapped media time advanced (random frame steps).
    advanced: Tick,
    instant: Option<InstantLoop>,
    /// Shadow playhead while an instant loop runs.
    shadow: Tick,
    cues: [Option<u32>; 6],
    /// The last non-zero heading (+1 / -1). On a reversal the position is mirrored inside its frame,
    /// so the frame on screen keeps the display time it had left (no skipped frame on reverse).
    last_heading: i64,
}

/// Settings + clip resolved for one evaluation.
#[derive(Clone, Copy, Debug)]
struct Range {
    first: u32,
    /// Frames in the play range (>= 1).
    n: u32,
    rate: FrameRate,
    /// Range length (exact; a still uses its duration).
    len: Tick,
    still: bool,
}

/// splitmix64: a small deterministic generator (no external crate; reproducible from `seed`).
fn next_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform in `0..n` (n >= 1).
fn rand_below(state: &mut u64, n: u64) -> u64 {
    next_rand(state).checked_rem(n.max(1)).unwrap_or(0)
}

/// The non-linear 0–10 speed knob: knob `x` in 0..=1 maps to speed `10 * x^log2(10)`, so the knob's
/// middle is exactly 1x, its top 10x and its bottom a stop; fine control near 1x.
pub fn speed_from_knob(x: f64) -> f64 {
    let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.5 };
    10.0 * x.powf(10f64.log2())
}

/// Inverse of [`speed_from_knob`] for speeds in 0..=10.
pub fn knob_from_speed(speed: f64) -> f64 {
    let s = if speed.is_finite() { speed.clamp(0.0, 10.0) } else { 1.0 };
    (s / 10.0).powf(1.0 / 10f64.log2())
}

/// Beat-count guess: the power of two nearest (in log scale) to the range's length in beats at
/// `bpm` (at least 1).
pub fn guess_beats(range_seconds: f64, bpm: f64) -> f64 {
    if !(range_seconds.is_finite() && bpm.is_finite()) || range_seconds <= 0.0 || bpm <= 0.0 {
        return 4.0;
    }
    let beats = range_seconds * bpm / 60.0;
    2f64.powf(beats.log2().round()).clamp(1.0, 4096.0)
}

/// Exact rational speed (nano-units), clamped to ±MAX_SPEED; NaN = 0.
fn speed_units(s: f64) -> i64 {
    if !s.is_finite() {
        return 0;
    }
    (s.clamp(-MAX_SPEED, MAX_SPEED) * SPEED_DEN as f64).round() as i64
}

/// Saturating tick arithmetic (explicit, so no operator hides an overflow).
fn tadd(a: Tick, b: Tick) -> Tick {
    Tick(a.0.saturating_add(b.0))
}
fn tsub(a: Tick, b: Tick) -> Tick {
    Tick(a.0.saturating_sub(b.0))
}

fn seconds(s: f64) -> Tick {
    Tick::from_seconds_f64(if s.is_finite() { s } else { 0.0 })
}

impl Default for Transport {
    fn default() -> Self {
        Transport::new(0)
    }
}

impl Transport {
    pub fn new(seed: i64) -> Self {
        Transport {
            pos: Tick::ZERO,
            last: None,
            ended: false,
            rng: seed as u64,
            since_jump: Tick::ZERO,
            random_step: i64::MIN,
            random_frame: 0,
            advanced: Tick::ZERO,
            instant: None,
            shadow: Tick::ZERO,
            cues: [None; 6],
            last_heading: 1,
        }
    }

    /// +1, -1 or 0 (paused) for the settings (backward and a negative speed cancel out).
    fn heading_of(s: &Settings) -> i64 {
        let sign: i64 = if s.speed < 0.0 { -1 } else { 1 };
        match s.direction {
            Direction::Pause => 0,
            Direction::Backward => sign.saturating_neg(),
            _ => sign,
        }
    }

    /// Mirror the position inside its frame: start of frame k <-> last tick of frame k.
    fn mirror_in_frame(&mut self, rate: FrameRate) {
        let k = rate.frame_at(self.pos);
        let (a, b) = (rate.tick_of(k), rate.tick_of(k.saturating_add(1)));
        self.pos = tsub(tadd(a, b), tadd(self.pos, Tick(1)));
    }

    /// The cue points (frame numbers).
    pub fn cues(&self) -> [Option<u32>; 6] {
        self.cues
    }
    pub fn set_cue(&mut self, i: usize, frame: Option<u32>) {
        if let Some(c) = self.cues.get_mut(i) {
            *c = frame;
        }
    }

    fn range(clip: &Clip, s: &Settings, bpm: f64) -> Range {
        let frames = clip.frames.max(1);
        let first = s.in_frame.min(frames.saturating_sub(1));
        let end = if s.out_frame == 0 { frames } else { s.out_frame.min(frames) };
        let n = end.saturating_sub(first).max(1);
        let rate = if clip.rate.num > 0 && clip.rate.den > 0 { clip.rate } else { FrameRate::FPS_30 };
        let still = clip.frames <= 1;
        let len = if still {
            let d = if s.sync == SyncMode::Beats { s.still_duration * 60.0 / bpm } else { s.still_duration };
            seconds(d).max(Tick(1))
        } else {
            rate.tick_of(i64::from(n))
        };
        Range { first, n, rate, len, still }
    }

    /// Tempo in use: the control input, else the fallback; clamped to 1..=999 BPM.
    fn bpm(inputs: &Inputs, fallback: f64) -> f64 {
        let b = inputs.bpm.filter(|b| b.is_finite() && *b > 0.0).unwrap_or(fallback);
        if b.is_finite() { b.clamp(1.0, 999.0) } else { 120.0 }
    }

    /// The effective speed (nano-units) for timeline / beat sync.
    fn speed(s: &Settings, r: &Range, bpm: f64) -> i64 {
        let base = match s.sync {
            SyncMode::Beats if !r.still => {
                let range_s = r.len.seconds();
                let beats = if s.beats > 0.0 && s.beats.is_finite() { s.beats } else { guess_beats(range_s, bpm) };
                let mult = if s.beat_multiplier.is_finite() && s.beat_multiplier > 0.0 { s.beat_multiplier.clamp(1.0 / 64.0, 64.0) } else { 1.0 };
                // The range plays in `beats` beats: speed = range seconds / (beats * 60 / bpm).
                range_s / (beats * 60.0 / bpm) * mult
            }
            _ => s.speed,
        };
        let sign = match s.direction {
            Direction::Backward => -1.0,
            Direction::Pause => 0.0,
            _ => 1.0,
        };
        speed_units(base * sign)
    }

    /// Period of the position for the mode: loop = range length; bounce = 2(N-1) frames.
    fn period(mode: PlayMode, r: &Range) -> Tick {
        match mode {
            PlayMode::Bounce if r.n > 1 && !r.still => r.rate.tick_of(i64::from(r.n.saturating_sub(1)).saturating_mul(2)),
            _ => r.len,
        }
    }

    fn wrap(t: Tick, period: Tick) -> Tick {
        if period.0 <= 0 { Tick::ZERO } else { Tick(t.0.rem_euclid(period.0)) }
    }

    /// Bring `pos` into the mode's canonical range; sets `ended` for once modes.
    fn normalise(&mut self, mode: PlayMode, r: &Range, heading: i64) {
        match mode {
            PlayMode::Loop | PlayMode::RandomFrame => self.pos = Self::wrap(self.pos, r.len),
            PlayMode::Bounce => self.pos = Self::wrap(self.pos, Self::period(mode, r)),
            PlayMode::OnceHold | PlayMode::OnceEject => {
                if self.pos >= r.len {
                    self.pos = r.len;
                    if heading >= 0 {
                        self.ended = true;
                    }
                } else if self.pos < Tick::ZERO {
                    self.pos = Tick::ZERO;
                    if heading < 0 {
                        self.ended = true;
                    }
                }
            }
        }
    }

    /// Range-relative frame index for the current position (0..n).
    fn index(&self, mode: PlayMode, r: &Range) -> u32 {
        if r.still {
            return 0;
        }
        let last = i64::from(r.n.saturating_sub(1));
        let k = r.rate.frame_at(self.pos);
        let k = match mode {
            PlayMode::Bounce if r.n > 1 => {
                let k = k.rem_euclid(last.saturating_mul(2).max(1));
                if k > last { last.saturating_mul(2).saturating_sub(k) } else { k }
            }
            _ => k,
        };
        k.clamp(0, last) as u32
    }

    /// Evaluate at clock time `now`.
    pub fn advance(&mut self, now: Tick, clip: &Clip, s: &Settings, inputs: &Inputs) -> Frame {
        let bpm = Self::bpm(inputs, 120.0);
        let r = Self::range(clip, s, bpm);
        let dt = match self.last {
            Some(last) if now >= last && tsub(now, last) <= MAX_STEP => tsub(now, last),
            _ => Tick::ZERO,
        };
        self.last = Some(now);
        let mut heading = 1i64;
        match s.sync {
            SyncMode::Timecode1 | SyncMode::Timecode2 => {
                let idx = if s.sync == SyncMode::Timecode1 { 0 } else { 1 };
                if let Some(tc) = inputs.timecode.get(idx).copied().flatten().filter(|t| t.is_finite()) {
                    let off = if s.timecode_offset.is_finite() { s.timecode_offset } else { 0.0 };
                    self.pos = seconds(tc - off);
                    self.ended = false;
                }
                // No timecode connected: hold the last position.
            }
            SyncMode::Timeline | SyncMode::Beats => {
                let speed = Self::speed(s, &r, bpm);
                heading = speed.signum();
                if heading != 0 && heading != self.last_heading {
                    if !r.still && self.instant.is_none() {
                        self.mirror_in_frame(r.rate);
                    }
                    self.last_heading = heading;
                }
                let delta = dt.mul_ratio(speed, SPEED_DEN);
                self.advanced = tadd(self.advanced, delta);
                let looping = s.sync == SyncMode::Beats && s.instant_loop > 0.0 && s.instant_loop.is_finite() && !r.still;
                if looping {
                    if self.instant.is_none() {
                        // Start: quantise the loop start down to the beat grid of the range.
                        let beat_len = Self::beat_len(&r, s, bpm);
                        let len = beat_len.mul_ratio((s.instant_loop * 1_000_000.0).round() as i64, 1_000_000).max(Tick(1));
                        let start = if beat_len.0 > 0 { Tick(self.pos.0.div_euclid(beat_len.0).saturating_mul(beat_len.0)) } else { self.pos };
                        // The loop is the beat(s) holding the playhead; its phase carries on (no jump).
                        self.shadow = self.pos;
                        self.instant = Some(InstantLoop { start, len, anchor: start });
                    }
                    self.shadow = tadd(self.shadow, delta);
                } else if self.instant.take().is_some() {
                    // Release: catch up to where the playhead would have been.
                    self.pos = tadd(self.shadow, delta);
                } else if s.mode != PlayMode::OnceHold && s.mode != PlayMode::OnceEject || !self.ended {
                    self.pos = tadd(self.pos, delta);
                }
                if s.direction == Direction::Random && !looping {
                    self.random_jump(dt, s, &r, bpm);
                }
            }
        }
        if let Some(il) = self.instant {
            let off = Self::wrap(tsub(self.shadow, il.anchor), il.len);
            self.pos = tadd(il.start, off);
        }
        self.normalise(s.mode, &r, heading);
        let mut k = self.index(s.mode, &r);
        if s.mode == PlayMode::RandomFrame && !r.still {
            let step = r.rate.frame_at(self.advanced);
            if step != self.random_step {
                self.random_step = step;
                self.random_frame = rand_below(&mut self.rng, u64::from(r.n)) as u32;
            }
            k = self.random_frame.min(r.n.saturating_sub(1));
        }
        let ended = self.ended || (r.still && matches!(s.mode, PlayMode::OnceHold | PlayMode::OnceEject) && self.pos >= r.len);
        let visible = !(ended && s.mode == PlayMode::OnceEject);
        let position = if r.n > 1 {
            f64::from(k) / f64::from(r.n.saturating_sub(1))
        } else if r.len.0 > 0 {
            (self.pos.seconds() / r.len.seconds()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Frame { frame: r.first.saturating_add(k), visible, ended, position, heading: if heading < 0 { -1 } else { 1 } }
    }

    fn beat_len(r: &Range, s: &Settings, bpm: f64) -> Tick {
        // In beat sync the range spans `beats` beats; one beat of MEDIA time = len / beats.
        let beats = if s.beats > 0.0 && s.beats.is_finite() { s.beats } else { guess_beats(r.len.seconds(), bpm) };
        r.len.mul_ratio(1_000_000, (beats * 1_000_000.0).round().max(1.0) as i64)
    }

    fn random_jump(&mut self, dt: Tick, s: &Settings, r: &Range, bpm: f64) {
        let interval = if s.random_interval.is_finite() && s.random_interval > 0.0 { s.random_interval } else { 1.0 };
        let every = if s.sync == SyncMode::Beats { seconds(interval * 60.0 / bpm) } else { seconds(interval) };
        if every.0 <= 0 {
            return;
        }
        self.since_jump = tadd(self.since_jump, dt);
        let due = self.since_jump.0.checked_div(every.0).unwrap_or(0);
        self.since_jump = Tick(self.since_jump.0.rem_euclid(every.0));
        // At most 16 jumps per evaluation (a tiny interval on a long step cannot spin).
        for _ in 0..due.clamp(0, 16) {
            let n = i64::from(r.n);
            let max = if s.random_max == 0 { n } else { i64::from(s.random_max).min(n) };
            let span = max.saturating_mul(2).saturating_add(1) as u64;
            let jump = (rand_below(&mut self.rng, span) as i64).saturating_sub(max);
            self.pos = tadd(self.pos, r.rate.tick_of(jump));
            self.ended = false;
        }
    }

    // ----- actions -----

    /// Seek to file frame `frame` exactly (clamped into the play range). Clears `ended`.
    pub fn seek(&mut self, frame: u32, clip: &Clip, s: &Settings) {
        let r = Self::range(clip, s, 120.0);
        let k = frame.saturating_sub(r.first).min(r.n.saturating_sub(1));
        self.pos = r.rate.tick_of(i64::from(k));
        self.ended = false;
        self.instant = None;
        // Re-anchor: the next evaluation shows exactly this frame (the jump happens "now"). Playing
        // backward, land on the frame's last tick so it is shown for a whole frame too.
        self.last = None;
        let h = Self::heading_of(s);
        if h != 0 {
            self.last_heading = h;
        }
        if self.last_heading < 0 {
            self.mirror_in_frame(r.rate);
        }
    }

    /// Scrub to `x` in 0..=1 of the play range (nearest frame).
    pub fn scrub(&mut self, x: f64, clip: &Clip, s: &Settings) {
        let r = Self::range(clip, s, 120.0);
        let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
        let k = (x * f64::from(r.n.saturating_sub(1))).round() as u32;
        self.seek(r.first.saturating_add(k), clip, s);
    }

    /// Step `n` frames (negative = back) from the current frame; wraps in loop / bounce, clamps
    /// in once modes.
    pub fn step(&mut self, n: i64, clip: &Clip, s: &Settings) {
        let r = Self::range(clip, s, 120.0);
        let k = i64::from(self.index(PlayMode::Loop, &r)).saturating_add(n);
        let n_frames = i64::from(r.n);
        let k = match s.mode {
            PlayMode::OnceHold | PlayMode::OnceEject => k.clamp(0, n_frames.saturating_sub(1)),
            _ => k.rem_euclid(n_frames.max(1)),
        };
        self.pos = r.rate.tick_of(k);
        self.ended = false;
        self.instant = None;
        self.last = None;
        if self.last_heading < 0 {
            self.mirror_in_frame(r.rate);
        }
    }

    /// Restart from the beginning of the range (the end, when playing backward).
    pub fn restart(&mut self, clip: &Clip, s: &Settings) {
        let r = Self::range(clip, s, 120.0);
        let backward = s.direction == Direction::Backward || (s.direction != Direction::Pause && s.speed < 0.0);
        self.pos = if backward { tsub(r.len, Tick(1)) } else { Tick::ZERO };
        self.ended = false;
        self.instant = None;
        self.since_jump = Tick::ZERO;
        self.last = None;
        self.last_heading = if backward { -1 } else { 1 };
    }

    /// Trigger cue `i` (0..6): an unset cue is set to the current frame; a set cue is jumped to.
    /// Returns the cue's frame.
    pub fn trigger_cue(&mut self, i: usize, clip: &Clip, s: &Settings) -> Option<u32> {
        let r = Self::range(clip, s, 120.0);
        let current = r.first.saturating_add(self.index(s.mode, &r));
        let slot = self.cues.get_mut(i)?;
        match *slot {
            Some(f) => {
                self.seek(f, clip, s);
                Some(f)
            }
            None => {
                *slot = Some(current);
                Some(current)
            }
        }
    }

    /// Re-seed the random generator (random frame / random direction), keeping the playhead.
    pub fn reseed(&mut self, seed: i64) {
        self.rng = seed as u64;
        self.random_step = i64::MIN;
    }

    pub fn ended(&self) -> bool {
        self.ended
    }
    /// The raw playhead (media time from the in-point).
    pub fn position_ticks(&self) -> Tick {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: FrameRate = FrameRate::FPS_30;

    fn clip(n: u32) -> Clip {
        Clip { frames: n, rate: FPS }
    }

    /// Frames shown at each program frame `t = i / prog_rate` for `count` ticks.
    fn run(c: &Clip, s: &Settings, prog: FrameRate, count: i64) -> Vec<u32> {
        let mut t = Transport::new(s.seed);
        (0..count).map(|i| t.advance(prog.tick_of(i), c, s, &Inputs::default()).frame).collect()
    }

    #[test]
    fn speed_one_is_frame_accurate_and_loop_wraps_exactly() {
        let s = Settings::default();
        let got = run(&clip(5), &s, FPS, 12);
        assert_eq!(got, vec![0, 1, 2, 3, 4, 0, 1, 2, 3, 4, 0, 1]);
        // 60 Hz program, 30 fps clip: every frame twice.
        assert_eq!(run(&clip(3), &s, FrameRate::FPS_60, 8), vec![0, 0, 1, 1, 2, 2, 0, 0]);
        // NTSC: 29.97 clip on a 59.94 clock stays exact after a long time (no drift).
        let c = Clip { frames: 100_000, rate: FrameRate::FPS_29_97 };
        let mut t = Transport::new(0);
        let mut f = 0;
        for i in 0..=36_000i64 {
            f = t.advance(FrameRate::FPS_59_94.tick_of(i), &c, &s, &Inputs::default()).frame;
        }
        assert_eq!(f, 18_000);
    }

    #[test]
    fn bounce_shows_each_end_once_per_cycle() {
        let s = Settings { mode: PlayMode::Bounce, ..Settings::default() };
        assert_eq!(run(&clip(4), &s, FPS, 10), vec![0, 1, 2, 3, 2, 1, 0, 1, 2, 3]);
        // Negative control: plain loop is a different sequence.
        assert_ne!(run(&clip(4), &Settings::default(), FPS, 10), vec![0, 1, 2, 3, 2, 1, 0, 1, 2, 3]);
        // One-frame range: always 0.
        assert_eq!(run(&clip(1), &s, FPS, 3), vec![0, 0, 0]);
    }

    #[test]
    fn once_hold_and_eject_end_exactly() {
        let hold = Settings { mode: PlayMode::OnceHold, ..Settings::default() };
        let mut t = Transport::new(0);
        let fr: Vec<Frame> = (0..6).map(|i| t.advance(FPS.tick_of(i), &clip(4), &hold, &Inputs::default())).collect();
        assert_eq!(fr.iter().map(|f| f.frame).collect::<Vec<_>>(), vec![0, 1, 2, 3, 3, 3]);
        assert_eq!(fr.iter().map(|f| f.ended).collect::<Vec<_>>(), vec![false, false, false, false, true, true]);
        assert!(fr.iter().all(|f| f.visible));
        let eject = Settings { mode: PlayMode::OnceEject, ..Settings::default() };
        let mut t = Transport::new(0);
        let vis: Vec<bool> = (0..6).map(|i| t.advance(FPS.tick_of(i), &clip(4), &eject, &Inputs::default()).visible).collect();
        assert_eq!(vis, vec![true, true, true, true, false, false]);
        // Restart re-arms.
        t.restart(&clip(4), &eject);
        assert!(t.advance(FPS.tick_of(6), &clip(4), &eject, &Inputs::default()).visible);
    }

    #[test]
    fn reverse_and_negative_speed_play_backward() {
        let back = Settings { direction: Direction::Backward, ..Settings::default() };
        let mut t = Transport::new(0);
        t.restart(&clip(4), &back);
        let got: Vec<u32> = (0..6).map(|i| t.advance(FPS.tick_of(i), &clip(4), &back, &Inputs::default()).frame).collect();
        assert_eq!(got, vec![3, 2, 1, 0, 3, 2]);
        let neg = Settings { speed: -1.0, ..Settings::default() };
        let mut t = Transport::new(0);
        t.restart(&clip(4), &neg);
        let got2: Vec<u32> = (0..6).map(|i| t.advance(FPS.tick_of(i), &clip(4), &neg, &Inputs::default()).frame).collect();
        assert_eq!(got, got2);
        // Once-hold backward ends on frame 0.
        let s = Settings { mode: PlayMode::OnceHold, direction: Direction::Backward, ..Settings::default() };
        let mut t = Transport::new(0);
        t.restart(&clip(3), &s);
        let f: Vec<Frame> = (0..5).map(|i| t.advance(FPS.tick_of(i), &clip(3), &s, &Inputs::default())).collect();
        assert_eq!(f.iter().map(|x| x.frame).collect::<Vec<_>>(), vec![2, 1, 0, 0, 0]);
        assert!(f[4].ended && !f[1].ended);
    }

    #[test]
    fn speeds_quarter_to_four_are_exact() {
        for (speed, want) in [
            (0.25, vec![0, 0, 0, 0, 1, 1, 1, 1, 2]),
            (0.5, vec![0, 0, 1, 1, 2, 2, 3, 3, 4]),
            (2.0, vec![0, 2, 4, 6, 8, 0, 2, 4, 6]),
            (4.0, vec![0, 4, 8, 2, 6, 0, 4, 8, 2]),
        ] {
            let s = Settings { speed, ..Settings::default() };
            assert_eq!(run(&clip(10), &s, FPS, 9), want, "speed {speed}");
        }
        // Speed changes mid-play continue from the current position (no jump).
        let mut t = Transport::new(0);
        let c = clip(100);
        let s1 = Settings::default();
        for i in 0..10 {
            t.advance(FPS.tick_of(i), &c, &s1, &Inputs::default());
        }
        let s2 = Settings { speed: 2.0, ..Settings::default() };
        assert_eq!(t.advance(FPS.tick_of(10), &c, &s2, &Inputs::default()).frame, 11);
        assert_eq!(t.advance(FPS.tick_of(11), &c, &s2, &Inputs::default()).frame, 13);
    }

    #[test]
    fn in_out_range_seek_step_scrub_and_cues() {
        let s = Settings { in_frame: 10, out_frame: 13, ..Settings::default() };
        assert_eq!(run(&clip(20), &s, FPS, 7), vec![10, 11, 12, 10, 11, 12, 10]);
        let c = clip(20);
        let mut t = Transport::new(0);
        t.seek(12, &c, &s);
        assert_eq!(t.advance(Tick::ZERO, &c, &s, &Inputs::default()).frame, 12);
        let p = Settings { direction: Direction::Pause, ..Settings::default() };
        let mut t = Transport::new(0);
        t.seek(7, &c, &p);
        assert_eq!(t.advance(FPS.tick_of(50), &c, &p, &Inputs::default()).frame, 7);
        t.step(1, &c, &p);
        assert_eq!(t.advance(FPS.tick_of(51), &c, &p, &Inputs::default()).frame, 8);
        t.step(-2, &c, &p);
        assert_eq!(t.advance(FPS.tick_of(52), &c, &p, &Inputs::default()).frame, 6);
        t.scrub(1.0, &c, &p);
        assert_eq!(t.advance(FPS.tick_of(53), &c, &p, &Inputs::default()).frame, 19);
        t.scrub(0.5, &c, &p);
        assert_eq!(t.advance(FPS.tick_of(54), &c, &p, &Inputs::default()).frame, 10);
        // Cue 0 unset -> set to 10; move away; trigger -> back to 10.
        assert_eq!(t.trigger_cue(0, &c, &p), Some(10));
        t.seek(3, &c, &p);
        assert_eq!(t.trigger_cue(0, &c, &p), Some(10));
        assert_eq!(t.advance(FPS.tick_of(55), &c, &p, &Inputs::default()).frame, 10);
        assert_eq!(t.trigger_cue(9, &c, &p), None);
    }

    #[test]
    fn random_frame_and_random_direction_are_seeded() {
        let s = Settings { mode: PlayMode::RandomFrame, seed: 7, ..Settings::default() };
        let a = run(&clip(50), &s, FPS, 20);
        assert_eq!(a, run(&clip(50), &s, FPS, 20), "same seed, same frames");
        assert!(a.windows(2).any(|w| w[0].abs_diff(w[1]) > 1), "random frames jump: {a:?}");
        let s2 = Settings { seed: 8, ..s.clone() };
        assert_ne!(a, run(&clip(50), &s2, FPS, 20));
        let r = Settings { direction: Direction::Random, random_interval: 0.1, random_max: 5, seed: 3, ..Settings::default() };
        let c = clip(1000);
        let mut t = Transport::new(3);
        t.seek(500, &c, &r);
        let b: Vec<u32> = (0..60).map(|i| t.advance(FPS.tick_of(i), &c, &r, &Inputs::default()).frame).collect();
        let jumps: Vec<i64> = b.windows(2).map(|w| i64::from(w[1]) - i64::from(w[0])).filter(|d| *d != 1).collect();
        assert!(!jumps.is_empty() && jumps.iter().all(|d| d.abs() <= 6), "{jumps:?}");
    }

    #[test]
    fn beat_sync_maps_the_range_onto_beats() {
        // 2 s clip (60 frames at 30 fps), 4 beats at 120 BPM = 2 s: speed 1.
        let c = clip(60);
        let s = Settings { sync: SyncMode::Beats, beats: 4.0, ..Settings::default() };
        let inp = Inputs { bpm: Some(120.0), ..Inputs::default() };
        let mut t = Transport::new(0);
        let f: Vec<u32> = (0..4).map(|i| t.advance(FPS.tick_of(i), &c, &s, &inp).frame).collect();
        assert_eq!(f, vec![0, 1, 2, 3]);
        // 240 BPM: twice as fast.
        let fast = Inputs { bpm: Some(240.0), ..Inputs::default() };
        let mut t = Transport::new(0);
        let f: Vec<u32> = (0..4).map(|i| t.advance(FPS.tick_of(i), &c, &s, &fast).frame).collect();
        assert_eq!(f, vec![0, 2, 4, 6]);
        // Beat guess: 2 s at 120 BPM is 4 beats.
        assert_eq!(guess_beats(2.0, 120.0), 4.0);
        assert_eq!(guess_beats(2.5, 120.0), 4.0);
        assert_eq!(guess_beats(3.1, 120.0), 8.0);
    }

    #[test]
    fn instant_loop_catches_up_on_release() {
        // 4 s clip, 8 beats at 120 BPM (speed 1); loop 1 beat (0.5 s = 15 frames).
        let c = clip(120);
        let base = Settings { sync: SyncMode::Beats, beats: 8.0, ..Settings::default() };
        let inp = Inputs { bpm: Some(120.0), ..Inputs::default() };
        let mut t = Transport::new(0);
        for i in 0..20 {
            t.advance(FPS.tick_of(i), &c, &base, &inp);
        }
        let looped = Settings { instant_loop: 1.0, ..base.clone() };
        let frames: Vec<u32> = (20..60).map(|i| t.advance(FPS.tick_of(i), &c, &looped, &inp).frame).collect();
        // Quantised start = beat 1 (frame 15); loops 15..29.
        assert!(frames.iter().all(|f| (15..30).contains(f)), "{frames:?}");
        // Release: catch up to where it would have been (frame 60).
        assert_eq!(t.advance(FPS.tick_of(60), &c, &base, &inp).frame, 60);
    }

    #[test]
    fn timecode_sync_chases_the_input() {
        let c = clip(300);
        let s = Settings { sync: SyncMode::Timecode2, timecode_offset: 1.0, ..Settings::default() };
        let mut t = Transport::new(0);
        let inp = Inputs { timecode: [Some(99.0), Some(3.5)], ..Inputs::default() };
        assert_eq!(t.advance(Tick::ZERO, &c, &s, &inp).frame, 75);
        // Negative control: input 1 would give a different frame.
        let s1 = Settings { sync: SyncMode::Timecode1, ..s.clone() };
        assert_ne!(Transport::new(0).advance(Tick::ZERO, &c, &s1, &inp).frame, 75);
    }

    #[test]
    fn stills_end_after_their_duration() {
        let c = clip(1);
        let s = Settings { mode: PlayMode::OnceHold, still_duration: 0.1, ..Settings::default() };
        let mut t = Transport::new(0);
        let ended: Vec<bool> = (0..5).map(|i| t.advance(FPS.tick_of(i), &c, &s, &Inputs::default()).ended).collect();
        assert_eq!(ended, vec![false, false, false, true, true]);
        // In beats: 1 beat at 120 BPM = 0.5 s = 15 frames.
        let b = Settings { sync: SyncMode::Beats, still_duration: 1.0, ..s };
        let mut t = Transport::new(0);
        let inp = Inputs { bpm: Some(120.0), ..Inputs::default() };
        let e: Vec<bool> = (0..17).map(|i| t.advance(FPS.tick_of(i), &c, &b, &inp).ended).collect();
        assert_eq!(e.iter().position(|x| *x), Some(15));
    }

    #[test]
    fn speed_knob_is_nonlinear_with_1x_in_the_middle() {
        assert!((speed_from_knob(0.5) - 1.0).abs() < 1e-12);
        assert!((speed_from_knob(1.0) - 10.0).abs() < 1e-12);
        assert_eq!(speed_from_knob(0.0), 0.0);
        assert!((knob_from_speed(speed_from_knob(0.3)) - 0.3).abs() < 1e-12);
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let s = Settings {
            speed: f64::NAN,
            in_frame: u32::MAX,
            out_frame: 3,
            random_interval: -1.0,
            beats: f64::INFINITY,
            still_duration: f64::NAN,
            ..Settings::default()
        };
        let mut t = Transport::new(0);
        for c in [clip(0), clip(1), clip(u32::MAX), Clip { frames: 5, rate: FrameRate { num: 0, den: 0 } }] {
            for now in [Tick::MIN, Tick::ZERO, Tick::MAX, Tick(-5)] {
                let f = t.advance(now, &c, &s, &Inputs { bpm: Some(f64::NAN), timecode: [Some(f64::INFINITY), None] });
                assert!(f.frame < c.frames.max(1));
            }
        }
    }
}
