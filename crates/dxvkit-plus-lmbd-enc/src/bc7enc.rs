//! The 4-mode BC7 block encoder (modes 1 and 6 for opaque blocks, 5, 6 and 7 for blocks with
//! alpha) — a faithful Rust port of `bc7enc.cpp` from bc7enc_rdo (Richard Geldreich, Jr.; MIT OR
//! Unlicense, notice in `LICENSES/bc7enc_rdo.txt`), restricted to the configuration the LMBD
//! encoder uses: linear (non-perceptual) RGBA weights 1/1/1/1, uber level 4, 64 mode-1/7 partitions
//! with the partition-estimation filter bank, least-squares refinement, every mode and p-bit weight
//! 1.0.
//!
//! **Floating point, exactly.** The original is compiled by Apple clang with its default
//! `-ffp-contract=on`, which fuses `a * b + c` *within one source expression* into a single fused
//! multiply-add. That changes results (we measured 202 differing bytes on a 1080p frame between a
//! fused and an unfused build), so this port fuses at exactly those expressions — the list comes
//! from the compiler itself (`clang -S -emit-llvm`, every `llvm.fmuladd` with its source line) —
//! and nowhere else. Every such site below is a `mul_add`; every other operation is a plain IEEE
//! single-precision operation in the original's order. With that, output is byte-identical to the
//! original for the same pixels.
//
// Lint note: this module indexes fixed-size arrays (16 pixels, 64 partitions, 256-entry tables) with
// indices bounded by construction (selector values below the weight count, colour bytes, partition
// numbers from the tables), and its integer arithmetic stays far below overflow (colour bytes times
// 6-bit weights). The two lints are relaxed for this module only; every input is valid (any 16 RGBA
// pixels), and the property tests run random blocks through it.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::needless_range_loop)]

use crate::tables::{PARTITION2, WEIGHTS2X, WEIGHTS3X, WEIGHTS4X};
use std::sync::LazyLock;

type Color = [u8; 4];
type Vec4 = [f32; 4];

const WEIGHTS2: [u32; 4] = [0, 21, 43, 64];
const WEIGHTS3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const WEIGHTS4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

const MODE_1_OPTIMAL_INDEX: usize = 2;
const MODE_7_OPTIMAL_INDEX: usize = 1;
const MAX_PARTITIONS: u32 = 64;

const COLOR_INDEX_BITCOUNT: [u32; 8] = [3, 3, 2, 2, 2, 2, 4, 2];
const ALPHA_INDEX_BITCOUNT: [u32; 8] = [0, 0, 0, 0, 3, 2, 4, 2];
const MODE_HAS_P_BITS: [bool; 8] = [true, true, false, true, false, false, true, true];
const MODE_HAS_SHARED_P_BITS: [bool; 8] = [false, true, false, false, false, false, false, false];
const COLOR_PRECISION: [u32; 8] = [4, 6, 5, 7, 5, 7, 7, 5];
const ALPHA_PRECISION: [u32; 8] = [0, 0, 0, 0, 6, 8, 7, 5];
const NUM_SUBSETS: [usize; 8] = [3, 2, 3, 2, 1, 1, 1, 2];
const PARTITION_BITS: [u32; 8] = [4, 6, 6, 6, 0, 0, 0, 6];

/// Encoder settings (the fields of the original's `bc7enc_compress_block_params` that its RDO
/// driver sets). [`Bc7Params::default`] is the configuration LMBD uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bc7Params {
    pub mode_mask: u32,
    pub max_partitions: u32,
    pub weights: [u32; 4],
    pub uber_level: u32,
    pub try_least_squares: bool,
    pub mode17_partition_estimation_filterbank: bool,
    pub pbit1_weight: f32,
    pub mode1_error_weight: f32,
    pub mode5_error_weight: f32,
    pub mode6_error_weight: f32,
    pub mode7_error_weight: f32,
    pub low_frequency_partition_weight: f32,
}

impl Default for Bc7Params {
    fn default() -> Self {
        Bc7Params {
            mode_mask: u32::MAX,
            max_partitions: MAX_PARTITIONS,
            weights: [1, 1, 1, 1],
            // bc7enc's maximum (the RDO driver asks for 6 and clamps to BC7ENC_MAX_UBER_LEVEL = 4).
            uber_level: 4,
            try_least_squares: true,
            mode17_partition_estimation_filterbank: true,
            pbit1_weight: 1.0,
            mode1_error_weight: 1.0,
            mode5_error_weight: 1.0,
            mode6_error_weight: 1.0,
            mode7_error_weight: 1.0,
            low_frequency_partition_weight: 1.0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct EndpointErr {
    err: u16,
    lo: u8,
    hi: u8,
}

struct Tables {
    /// `[colour][p-bit]`: the best mode-1 endpoints for a single colour at selector 2.
    mode1_optimal: Vec<[EndpointErr; 2]>,
    /// `[colour][high p-bit][low p-bit]`: the best mode-7 endpoints at selector 1.
    mode7_optimal: Vec<[[EndpointErr; 2]; 2]>,
    mode1_midpoints: [[f32; 2]; 64],
    mode5_midpoints: [f32; 128],
    mode7_midpoints: [[f32; 2]; 32],
}

static TABLES: LazyLock<Tables> = LazyLock::new(init_tables);

/// `bc7enc_compress_block_init()`.
fn init_tables() -> Tables {
    let mut t = Tables {
        mode1_optimal: vec![[EndpointErr::default(); 2]; 256],
        mode7_optimal: vec![[[EndpointErr::default(); 2]; 2]; 256],
        mode1_midpoints: [[0.0; 2]; 64],
        mode5_midpoints: [0.0; 128],
        mode7_midpoints: [[0.0; 2]; 32],
    };
    for p in 0..2u32 {
        for i in 0..32u32 {
            let mut vl = ((i << 1) | p) << 2;
            vl |= vl >> 6;
            let lo = vl as f32 / 255.0;
            let mut vh = ((31u32.min(i + 1) << 1) | p) << 2;
            vh |= vh >> 6;
            let hi = vh as f32 / 255.0;
            t.mode7_midpoints[i as usize][p as usize] = if i == 31 { 1.0 } else { (lo + hi) / 2.0 };
        }
    }
    for p in 0..2u32 {
        for i in 0..64u32 {
            let mut vl = ((i << 1) | p) << 1;
            vl |= vl >> 7;
            let lo = vl as f32 / 255.0;
            let mut vh = ((63u32.min(i + 1) << 1) | p) << 1;
            vh |= vh >> 7;
            let hi = vh as f32 / 255.0;
            t.mode1_midpoints[i as usize][p as usize] = if i == 63 { 1.0 } else { (lo + hi) / 2.0 };
        }
    }
    for i in 0..128u32 {
        let mut vl = i << 1;
        vl |= vl >> 7;
        let lo = vl as f32 / 255.0;
        let mut vh = 127u32.min(i + 1) << 1;
        vh |= vh >> 7;
        let hi = vh as f32 / 255.0;
        t.mode5_midpoints[i as usize] = if i == 127 { 1.0 } else { (lo + hi) / 2.0 };
    }
    // (The mode-6 "reduced quant" table is only used with m_quant_mode6_endpoints, which LMBD
    // never sets; it is not ported.)
    for c in 0..256i32 {
        for lp in 0..2u32 {
            let mut best = EndpointErr { err: u16::MAX, lo: 0, hi: 0 };
            for l in 0..64u32 {
                let mut low = ((l << 1) | lp) << 1;
                low |= low >> 7;
                for h in 0..64u32 {
                    let mut high = ((h << 1) | lp) << 1;
                    high |= high >> 7;
                    let w = WEIGHTS3[MODE_1_OPTIMAL_INDEX];
                    let k = ((low * (64 - w) + high * w + 32) >> 6) as i32;
                    let err = (k - c) * (k - c);
                    if err < i32::from(best.err) {
                        best = EndpointErr { err: err as u16, lo: l as u8, hi: h as u8 };
                    }
                }
            }
            t.mode1_optimal[c as usize][lp as usize] = best;
        }
    }
    for c in 0..256i32 {
        for hp in 0..2u32 {
            for lp in 0..2u32 {
                let mut best = EndpointErr { err: u16::MAX, lo: 0, hi: 0 };
                for l in 0..32u32 {
                    let mut low = ((l << 1) | lp) << 2;
                    low |= low >> 6;
                    for h in 0..32u32 {
                        let mut high = ((h << 1) | hp) << 2;
                        high |= high >> 6;
                        let w = WEIGHTS2[MODE_7_OPTIMAL_INDEX];
                        let k = ((low * (64 - w) + high * w + 32) >> 6) as i32;
                        let err = (k - c) * (k - c);
                        if err < i32::from(best.err) {
                            best = EndpointErr { err: err as u16, lo: l as u8, hi: h as u8 };
                        }
                    }
                }
                t.mode7_optimal[c as usize][hp as usize][lp as usize] = best;
            }
        }
    }
    t
}

// --- Small helpers, with the original's exact semantics (comparisons, not f32::min / clamp) ---

#[inline]
fn clampi(v: i32, lo: i32, hi: i32) -> i32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}
#[inline]
fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}
#[inline]
fn saturate(v: f32) -> f32 {
    clampf(v, 0.0, 1.0)
}
#[inline]
fn minimumf(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}
#[inline]
fn maximumf(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
#[inline]
fn from_color(c: &Color) -> Vec4 {
    [f32::from(c[0]), f32::from(c[1]), f32::from(c[2]), f32::from(c[3])]
}
#[inline]
fn vadd(a: &Vec4, b: &Vec4) -> Vec4 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}
#[inline]
fn vsub(a: &Vec4, b: &Vec4) -> Vec4 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}
#[inline]
fn vmul(a: &Vec4, s: f32) -> Vec4 {
    [a[0] * s, a[1] * s, a[2] * s, a[3] * s]
}
/// `vec4F_dot`: `a0*b0 + a1*b1 + a2*b2 + a3*b3`, fused as clang fuses it.
#[inline]
fn vdot(a: &Vec4, b: &Vec4) -> f32 {
    a[3].mul_add(b[3], a[2].mul_add(b[2], a[0].mul_add(b[0], a[1] * b[1])))
}
/// `vec4F_normalize_in_place`.
#[inline]
fn vnormalize(v: &mut Vec4) {
    let mut s = v[3].mul_add(v[3], v[2].mul_add(v[2], v[0].mul_add(v[0], v[1] * v[1])));
    if s != 0.0 {
        s = 1.0 / s.sqrt();
        v[0] *= s;
        v[1] *= s;
        v[2] *= s;
        v[3] *= s;
    }
}
#[inline]
fn vsaturate(v: &Vec4) -> Vec4 {
    [saturate(v[0]), saturate(v[1]), saturate(v[2]), saturate(v[3])]
}

/// Linear colour distance (the perceptual branch is not used by LMBD and not ported).
#[inline]
fn dist_rgb(a: &Color, b: &Color, w: &[u32; 4]) -> u64 {
    let dr = i32::from(a[0]) - i32::from(b[0]);
    let dg = i32::from(a[1]) - i32::from(b[1]);
    let db = i32::from(a[2]) - i32::from(b[2]);
    u64::from(w[0] * (dr * dr) as u32 + w[1] * (dg * dg) as u32 + w[2] * (db * db) as u32)
}
#[inline]
fn dist_rgba(a: &Color, b: &Color, w: &[u32; 4]) -> u64 {
    let da = i32::from(a[3]) - i32::from(b[3]);
    dist_rgb(a, b, w) + u64::from(w[3] * (da * da) as u32)
}

// --- Least squares (normal equations) ---

fn least_squares_rgba(sel: &[u8], wx: &[[f32; 4]], px: &[Color]) -> (Vec4, Vec4) {
    let (mut z00, mut z10, mut z11) = (0.0f32, 0.0f32, 0.0f32);
    let (mut q00, mut t) = ([0.0f32; 4], [0.0f32; 4]);
    for (i, p) in px.iter().enumerate() {
        let w = wx[usize::from(sel[i])];
        z00 += w[0];
        z10 += w[1];
        z11 += w[2];
        for c in 0..4 {
            q00[c] = w[3].mul_add(f32::from(p[c]), q00[c]);
            t[c] += f32::from(p[c]);
        }
    }
    let q10 = [t[0] - q00[0], t[1] - q00[1], t[2] - q00[2], t[3] - q00[3]];
    let z01 = z10;
    let mut det = z00.mul_add(z11, -(z01 * z10));
    if det != 0.0 {
        det = 1.0 / det;
    }
    let (iz00, iz01, iz10, iz11) = (z11 * det, -z01 * det, -z10 * det, z00 * det);
    let (mut xl, mut xh) = ([0.0f32; 4], [0.0f32; 4]);
    for c in 0..4 {
        xl[c] = iz00.mul_add(q00[c], iz01 * q10[c]);
        xh[c] = iz10.mul_add(q00[c], iz11 * q10[c]);
    }
    for c in 0..4 {
        fix_flat_channel(&mut xl, &mut xh, c, px);
    }
    (xl, xh)
}

fn least_squares_rgb(sel: &[u8], wx: &[[f32; 4]], px: &[Color]) -> (Vec4, Vec4) {
    let (mut z00, mut z10, mut z11) = (0.0f32, 0.0f32, 0.0f32);
    let (mut q00, mut t) = ([0.0f32; 3], [0.0f32; 3]);
    for (i, p) in px.iter().enumerate() {
        let w = wx[usize::from(sel[i])];
        z00 += w[0];
        z10 += w[1];
        z11 += w[2];
        for c in 0..3 {
            q00[c] = w[3].mul_add(f32::from(p[c]), q00[c]);
            t[c] += f32::from(p[c]);
        }
    }
    let q10 = [t[0] - q00[0], t[1] - q00[1], t[2] - q00[2]];
    let z01 = z10;
    let mut det = z00.mul_add(z11, -(z01 * z10));
    if det != 0.0 {
        det = 1.0 / det;
    }
    let (iz00, iz01, iz10, iz11) = (z11 * det, -z01 * det, -z10 * det, z00 * det);
    let (mut xl, mut xh) = ([0.0f32, 0.0, 0.0, 255.0], [0.0f32, 0.0, 0.0, 255.0]);
    for c in 0..3 {
        xl[c] = iz00.mul_add(q00[c], iz01 * q10[c]);
        xh[c] = iz10.mul_add(q00[c], iz11 * q10[c]);
    }
    for c in 0..3 {
        fix_flat_channel(&mut xl, &mut xh, c, px);
    }
    (xl, xh)
}

/// If a fitted channel went out of range and the channel is flat, use the flat value.
fn fix_flat_channel(xl: &mut Vec4, xh: &mut Vec4, c: usize, px: &[Color]) {
    if xl[c] < 0.0 || xh[c] > 255.0 {
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        for p in px {
            lo = lo.min(u32::from(p[c]));
            hi = hi.max(u32::from(p[c]));
        }
        if lo == hi {
            xl[c] = lo as f32;
            xh[c] = hi as f32;
        }
    }
}

fn least_squares_a(sel: &[u8], wx: &[[f32; 4]], px: &[Color]) -> (f32, f32) {
    let (mut z00, mut z10, mut z11) = (0.0f32, 0.0f32, 0.0f32);
    let (mut q00_a, mut t_a) = (0.0f32, 0.0f32);
    for (i, p) in px.iter().enumerate() {
        let w = wx[usize::from(sel[i])];
        z00 += w[0];
        z10 += w[1];
        z11 += w[2];
        q00_a = w[3].mul_add(f32::from(p[3]), q00_a);
        t_a += f32::from(p[3]);
    }
    let q10_a = t_a - q00_a;
    let z01 = z10;
    let mut det = z00.mul_add(z11, -(z01 * z10));
    if det != 0.0 {
        det = 1.0 / det;
    }
    let (iz00, iz01, iz10, iz11) = (z11 * det, -z01 * det, -z10 * det, z00 * det);
    let mut xl = iz00.mul_add(q00_a, iz01 * q10_a);
    let mut xh = iz10.mul_add(q00_a, iz11 * q10_a);
    if xl < 0.0 || xh > 255.0 {
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        for p in px {
            lo = lo.min(u32::from(p[3]));
            hi = hi.max(u32::from(p[3]));
        }
        if lo == hi {
            xl = lo as f32;
            xh = hi as f32;
        }
    }
    (xl, xh)
}

// --- Colour cell compression (one subset) ---

struct CellParams<'a> {
    pixels: &'a [Color],
    sel_weights: &'static [u32],
    sel_weightsx: &'static [[f32; 4]],
    comp_bits: u32,
    weights: [u32; 4],
    has_alpha: bool,
    has_pbits: bool,
    endpoints_share_pbit: bool,
}

#[derive(Clone, Copy)]
struct CellResults {
    best_err: u64,
    low: Color,
    high: Color,
    pbits: [u32; 2],
    selectors: [u8; 16],
    selectors_temp: [u8; 16],
}

impl CellResults {
    fn new() -> Self {
        CellResults { best_err: u64::MAX, low: [0; 4], high: [0; 4], pbits: [0; 2], selectors: [0; 16], selectors_temp: [0; 16] }
    }
}

fn scale_color(c: &Color, p: &CellParams<'_>) -> Color {
    let n = p.comp_bits + u32::from(p.has_pbits);
    let mut out = [0u8; 4];
    for i in 0..4 {
        let mut v = u32::from(c[i]) << (8 - n);
        v |= v >> n;
        out[i] = v as u8;
    }
    out
}

/// `pack_mode1_to_one_color`: writes the endpoints and error into `r`, the selectors into
/// `r.selectors` (`to_temp` = false) or `r.selectors_temp` (true).
fn pack_mode1_to_one_color(p: &CellParams<'_>, r: &mut CellResults, cr: usize, cg: usize, cb: usize, to_temp: bool) -> u64 {
    let t = &*TABLES;
    let mut best_err = u32::MAX;
    let mut best_p = 0usize;
    for pb in 0..2usize {
        let err = u32::from(t.mode1_optimal[cr][pb].err) + u32::from(t.mode1_optimal[cg][pb].err) + u32::from(t.mode1_optimal[cb][pb].err);
        if err < best_err {
            best_err = err;
            best_p = pb;
            if best_err == 0 {
                break;
            }
        }
    }
    let (er, eg, eb) = (t.mode1_optimal[cr][best_p], t.mode1_optimal[cg][best_p], t.mode1_optimal[cb][best_p]);
    r.low = [er.lo, eg.lo, eb.lo, 0];
    r.high = [er.hi, eg.hi, eb.hi, 0];
    r.pbits = [best_p as u32, 0];
    let n = p.pixels.len();
    let sel = if to_temp { &mut r.selectors_temp } else { &mut r.selectors };
    sel[..n].fill(MODE_1_OPTIMAL_INDEX as u8);
    let mut c: Color = [0, 0, 0, 255];
    for i in 0..3 {
        let mut low = ((u32::from(r.low[i]) << 1) | r.pbits[0]) << 1;
        low |= low >> 7;
        let mut high = ((u32::from(r.high[i]) << 1) | r.pbits[0]) << 1;
        high |= high >> 7;
        let w = WEIGHTS3[MODE_1_OPTIMAL_INDEX];
        c[i] = ((low * (64 - w) + high * w + 32) >> 6) as u8;
    }
    let total: u64 = p.pixels.iter().map(|px| dist_rgb(&c, px, &p.weights)).sum();
    r.best_err = total;
    total
}

/// `pack_mode7_to_one_color` (same selector-target convention as mode 1).
fn pack_mode7_to_one_color(p: &CellParams<'_>, r: &mut CellResults, ca: [usize; 4], to_temp: bool) -> u64 {
    let t = &*TABLES;
    let mut best_err = u32::MAX;
    let mut best_p = 0usize;
    for pb in 0..4usize {
        let (hi_p, lo_p) = (pb >> 1, pb & 1);
        let err = ca.iter().map(|&c| u32::from(t.mode7_optimal[c][hi_p][lo_p].err)).sum::<u32>();
        if err < best_err {
            best_err = err;
            best_p = pb;
            if best_err == 0 {
                break;
            }
        }
    }
    let (hi_p, lo_p) = (best_p >> 1, best_p & 1);
    let e = ca.map(|c| t.mode7_optimal[c][hi_p][lo_p]);
    r.low = [e[0].lo, e[1].lo, e[2].lo, e[3].lo];
    r.high = [e[0].hi, e[1].hi, e[2].hi, e[3].hi];
    r.pbits = [lo_p as u32, hi_p as u32];
    let n = p.pixels.len();
    let sel = if to_temp { &mut r.selectors_temp } else { &mut r.selectors };
    sel[..n].fill(MODE_7_OPTIMAL_INDEX as u8);
    let mut c: Color = [0; 4];
    for i in 0..4 {
        let mut low = (u32::from(r.low[i]) << 1) | r.pbits[0];
        let mut high = (u32::from(r.high[i]) << 1) | r.pbits[1];
        low = (low << 2) | (low >> 6);
        high = (high << 2) | (high >> 6);
        let w = WEIGHTS2[MODE_7_OPTIMAL_INDEX];
        c[i] = ((low * (64 - w) + high * w + 32) >> 6) as u8;
    }
    let total: u64 = p.pixels.iter().map(|px| dist_rgba(&c, px, &p.weights)).sum();
    r.best_err = total;
    total
}

/// `evaluate_solution` (linear path): pick each pixel's selector for these endpoints; keep the
/// solution if it beats the best so far.
fn evaluate_solution(low: &Color, high: &Color, pbits: [u32; 2], p: &CellParams<'_>, r: &mut CellResults) -> u64 {
    let (mut qmin, mut qmax) = (*low, *high);
    if p.has_pbits {
        let (minp, maxp) = if p.endpoints_share_pbit { (pbits[0], pbits[0]) } else { (pbits[0], pbits[1]) };
        for c in 0..4 {
            qmin[c] = ((u32::from(low[c]) << 1) | minp) as u8;
            qmax[c] = ((u32::from(high[c]) << 1) | maxp) as u8;
        }
    }
    let amin = scale_color(&qmin, p);
    let amax = scale_color(&qmax, p);
    let n = p.sel_weights.len();
    let mut wc = [[0u8; 4]; 16];
    wc[0] = amin;
    wc[n - 1] = amax;
    let nc = if p.has_alpha { 4 } else { 3 };
    for i in 1..n - 1 {
        for j in 0..nc {
            let w = p.sel_weights[i];
            wc[i][j] = ((u32::from(amin[j]) * (64 - w) + u32::from(amax[j]) * w + 32) >> 6) as u8;
        }
    }
    let (lr, lg, lb) = (i32::from(amin[0]), i32::from(amin[1]), i32::from(amin[2]));
    let (dr, dg, db) = (i32::from(amax[0]) - lr, i32::from(amax[1]) - lg, i32::from(amax[2]) - lb);
    let mut total = 0u64;
    if p.has_alpha {
        let la = i32::from(amin[3]);
        let da = i32::from(amax[3]) - la;
        let f = n as f32 / ((dr * dr + dg * dg + db * db + da * da) as f32 + 0.00000125);
        for (i, c) in p.pixels.iter().enumerate() {
            let (r_, g, b, a) = (i32::from(c[0]), i32::from(c[1]), i32::from(c[2]), i32::from(c[3]));
            let dot = (r_ - lr) * dr + (g - lg) * dg + (b - lb) * db + (a - la) * da;
            let mut best_sel = clampi((dot as f32).mul_add(f, 0.5) as i32, 1, n as i32 - 1) as usize;
            let err0 = dist_rgba(&wc[best_sel - 1], c, &p.weights);
            let mut err1 = dist_rgba(&wc[best_sel], c, &p.weights);
            if err1 > err0 {
                err1 = err0;
                best_sel -= 1;
            }
            total += err1;
            r.selectors_temp[i] = best_sel as u8;
        }
    } else {
        let f = n as f32 / ((dr * dr + dg * dg + db * db) as f32 + 0.00000125);
        for (i, c) in p.pixels.iter().enumerate() {
            let (r_, g, b) = (i32::from(c[0]), i32::from(c[1]), i32::from(c[2]));
            let dot = (r_ - lr) * dr + (g - lg) * dg + (b - lb) * db;
            let sel = clampi((dot as f32).mul_add(f, 0.5) as i32, 1, n as i32 - 1) as usize;
            let err0 = dist_rgb(&wc[sel - 1], c, &p.weights);
            let err1 = dist_rgb(&wc[sel], c, &p.weights);
            let (mut best_sel, mut best_err) = (sel, err1);
            if err0 < best_err {
                best_err = err0;
                best_sel = sel - 1;
            }
            total += best_err;
            r.selectors_temp[i] = best_sel as u8;
        }
    }
    if total < r.best_err {
        r.best_err = total;
        r.low = *low;
        r.high = *high;
        r.pbits = pbits;
        let np = p.pixels.len();
        r.selectors[..np].copy_from_slice(&r.selectors_temp[..np]);
    }
    total
}

/// `fixDegenerateEndpoints` (mode 1 only; the mode-6 case needs m_quant_mode6_endpoints).
fn fix_degenerate_endpoints(mode: u32, tmin: &mut Color, tmax: &mut Color, xl: &Vec4, xh: &Vec4, iscale: u32) {
    if mode != 1 {
        return;
    }
    for i in 0..3 {
        if tmin[i] == tmax[i] && (xl[i] - xh[i]).abs() > 0.0 {
            if u32::from(tmin[i]) > (iscale >> 1) {
                if tmin[i] > 0 {
                    tmin[i] -= 1;
                } else if u32::from(tmax[i]) < iscale {
                    tmax[i] += 1;
                }
            } else if u32::from(tmax[i]) < iscale {
                tmax[i] += 1;
            } else if tmin[i] > 0 {
                tmin[i] -= 1;
            }
        }
    }
}

/// `find_optimal_solution`: quantise the endpoints `xl`/`xh` (0..1) for this mode (choosing p-bits),
/// evaluate if they differ from the current best, and return the best error.
fn find_optimal_solution(mode: u32, mut xl: Vec4, mut xh: Vec4, p: &CellParams<'_>, r: &mut CellResults, comp: &Bc7Params) -> u64 {
    let t = &*TABLES;
    xl = vsaturate(&xl);
    xh = vsaturate(&xh);
    if p.has_pbits {
        let iscalep = (1i32 << (p.comp_bits + 1)) - 1;
        let scalep = iscalep as f32;
        let total_comps = if p.has_alpha { 4 } else { 3 };
        let mut best_pbits = [0u32; 2];
        let (mut best_min, mut best_max) = ([0u8; 4], [0u8; 4]);
        if !p.endpoints_share_pbit {
            let (mut best_err0, mut best_err1) = (1e9f32, 1e9f32);
            for pb in 0..2i32 {
                let (mut xmin, mut xmax) = ([0u8; 4], [0u8; 4]);
                if p.comp_bits == 5 {
                    for c in 0..4 {
                        let mut vl = (xl[c] * 31.0) as i32;
                        vl += i32::from(xl[c] > t.mode7_midpoints[vl as usize][pb as usize]);
                        xmin[c] = clampi(vl * 2 + pb, pb, 63 - 1 + pb) as u8;
                        let mut vh = (xh[c] * 31.0) as i32;
                        vh += i32::from(xh[c] > t.mode7_midpoints[vh as usize][pb as usize]);
                        xmax[c] = clampi(vh * 2 + pb, pb, 63 - 1 + pb) as u8;
                    }
                } else {
                    for c in 0..4 {
                        let vl = (xl[c].mul_add(scalep, -(pb as f32)) / 2.0 + 0.5) as i32;
                        xmin[c] = clampi(vl * 2 + pb, pb, iscalep - 1 + pb) as u8;
                        let vh = (xh[c].mul_add(scalep, -(pb as f32)) / 2.0 + 0.5) as i32;
                        xmax[c] = clampi(vh * 2 + pb, pb, iscalep - 1 + pb) as u8;
                    }
                }
                let scaled_low = scale_color(&xmin, p);
                let scaled_high = scale_color(&xmax, p);
                let (mut err0, mut err1) = (0.0f32, 0.0f32);
                for i in 0..total_comps {
                    let d0 = (-xl[i]).mul_add(255.0, f32::from(scaled_low[i]));
                    err0 += d0 * d0;
                    let d1 = (-xh[i]).mul_add(255.0, f32::from(scaled_high[i]));
                    err1 += d1 * d1;
                }
                if pb == 1 {
                    err0 *= comp.pbit1_weight;
                    err1 *= comp.pbit1_weight;
                }
                if err0 < best_err0 {
                    best_err0 = err0;
                    best_pbits[0] = pb as u32;
                    best_min = xmin.map(|v| v >> 1);
                }
                if err1 < best_err1 {
                    best_err1 = err1;
                    best_pbits[1] = pb as u32;
                    best_max = xmax.map(|v| v >> 1);
                }
            }
        } else {
            // Endpoints share a p-bit (mode 1); m_bias_mode1_pbits is off.
            let mut best_err = 1e9f32;
            for pb in 0..2i32 {
                let (mut xmin, mut xmax) = ([0u8; 4], [0u8; 4]);
                if p.comp_bits == 6 {
                    for c in 0..4 {
                        let mut vl = (xl[c] * 63.0) as i32;
                        vl += i32::from(xl[c] > t.mode1_midpoints[vl as usize][pb as usize]);
                        xmin[c] = clampi(vl * 2 + pb, pb, 127 - 1 + pb) as u8;
                        let mut vh = (xh[c] * 63.0) as i32;
                        vh += i32::from(xh[c] > t.mode1_midpoints[vh as usize][pb as usize]);
                        xmax[c] = clampi(vh * 2 + pb, pb, 127 - 1 + pb) as u8;
                    }
                } else {
                    for c in 0..4 {
                        let vl = (xl[c].mul_add(scalep, -(pb as f32)) / 2.0 + 0.5) as i32;
                        xmin[c] = clampi(vl * 2 + pb, pb, iscalep - 1 + pb) as u8;
                        let vh = (xh[c].mul_add(scalep, -(pb as f32)) / 2.0 + 0.5) as i32;
                        xmax[c] = clampi(vh * 2 + pb, pb, iscalep - 1 + pb) as u8;
                    }
                }
                let scaled_low = scale_color(&xmin, p);
                let scaled_high = scale_color(&xmax, p);
                let mut err = 0.0f32;
                for i in 0..total_comps {
                    let a = f32::from(scaled_low[i]) / 255.0 - xl[i];
                    let b = f32::from(scaled_high[i]) / 255.0 - xh[i];
                    err += a * a + b * b;
                }
                if pb == 1 {
                    err *= comp.pbit1_weight;
                }
                if err < best_err {
                    best_err = err;
                    best_pbits = [pb as u32, pb as u32];
                    best_min = xmin.map(|v| v >> 1);
                    best_max = xmax.map(|v| v >> 1);
                }
            }
        }
        fix_degenerate_endpoints(mode, &mut best_min, &mut best_max, &xl, &xh, (iscalep >> 1) as u32);
        if r.best_err == u64::MAX || best_min != r.low || best_max != r.high || best_pbits[0] != r.pbits[0] || best_pbits[1] != r.pbits[1] {
            evaluate_solution(&best_min, &best_max, best_pbits, p, r);
        }
    } else {
        let iscale = (1i32 << p.comp_bits) - 1;
        let scale = iscale as f32;
        let (mut tmin, mut tmax) = ([0u8; 4], [0u8; 4]);
        if p.comp_bits == 7 {
            for c in 0..4 {
                let mut vl = (xl[c] * 127.0) as i32;
                vl += i32::from(xl[c] > t.mode5_midpoints[vl as usize]);
                tmin[c] = clampi(vl, 0, 127) as u8;
                let mut vh = (xh[c] * 127.0) as i32;
                vh += i32::from(xh[c] > t.mode5_midpoints[vh as usize]);
                tmax[c] = clampi(vh, 0, 127) as u8;
            }
        } else {
            for c in 0..4 {
                tmin[c] = clampi(xl[c].mul_add(scale, 0.5) as i32, 0, 255) as u8;
                tmax[c] = clampi(xh[c].mul_add(scale, 0.5) as i32, 0, 255) as u8;
            }
        }
        fix_degenerate_endpoints(mode, &mut tmin, &mut tmax, &xl, &xh, iscale as u32);
        if r.best_err == u64::MAX || tmin != r.low || tmax != r.high {
            let pbits = r.pbits;
            evaluate_solution(&tmin, &tmax, pbits, p, r);
        }
    }
    r.best_err
}

fn least_squares(p: &CellParams<'_>, sel: &[u8]) -> (Vec4, Vec4) {
    let (xl, xh) = if p.has_alpha { least_squares_rgba(sel, p.sel_weightsx, p.pixels) } else { least_squares_rgb(sel, p.sel_weightsx, p.pixels) };
    let k = 1.0f32 / 255.0;
    (vmul(&xl, k), vmul(&xh, k))
}

/// `color_cell_compression`: encode one subset in `mode`; returns the best error (0 ends early).
fn color_cell_compression(mode: u32, p: &CellParams<'_>, r: &mut CellResults, comp: &Bc7Params) -> u64 {
    r.best_err = u64::MAX;
    let np = p.pixels.len();
    let first = p.pixels[0];
    if mode == 1 {
        if p.pixels.iter().all(|c| c[0] == first[0] && c[1] == first[1] && c[2] == first[2]) {
            return pack_mode1_to_one_color(p, r, first[0].into(), first[1].into(), first[2].into(), false);
        }
    } else if mode == 7 && p.pixels.iter().all(|c| *c == first) {
        return pack_mode7_to_one_color(p, r, first.map(usize::from), false);
    }

    let mut mean = [0.0f32; 4];
    for c in p.pixels {
        mean = vadd(&mean, &from_color(c));
    }
    let mean_scaled = vmul(&mean, 1.0 / (np as f32));
    let mean_color = vsaturate(&vmul(&mean, 1.0 / (np as f32 * 255.0)));

    let mut axis: Vec4;
    if p.has_alpha {
        // Incremental PCA (RGBA).
        axis = [0.0; 4];
        for (i, c) in p.pixels.iter().enumerate() {
            let color = vsub(&from_color(c), &mean_scaled);
            let a = vmul(&color, color[0]);
            let b = vmul(&color, color[1]);
            let cc = vmul(&color, color[2]);
            let d = vmul(&color, color[3]);
            let mut n = if i != 0 { axis } else { color };
            vnormalize(&mut n);
            axis[0] += vdot(&a, &n);
            axis[1] += vdot(&b, &n);
            axis[2] += vdot(&cc, &n);
            axis[3] += vdot(&d, &n);
        }
        vnormalize(&mut axis);
    } else {
        // Covariance + power iteration (RGB).
        let mut cov = [0.0f32; 6];
        for c in p.pixels {
            let r_ = f32::from(c[0]) - mean_scaled[0];
            let g = f32::from(c[1]) - mean_scaled[1];
            let b = f32::from(c[2]) - mean_scaled[2];
            cov[0] = r_.mul_add(r_, cov[0]);
            cov[1] = r_.mul_add(g, cov[1]);
            cov[2] = r_.mul_add(b, cov[2]);
            cov[3] = g.mul_add(g, cov[3]);
            cov[4] = g.mul_add(b, cov[4]);
            cov[5] = b.mul_add(b, cov[5]);
        }
        let (mut vfr, mut vfg, mut vfb) = (0.9f32, 1.0f32, 0.7f32);
        for _ in 0..3 {
            let mut r_ = vfb.mul_add(cov[2], vfr.mul_add(cov[0], vfg * cov[1]));
            let mut g = vfb.mul_add(cov[4], vfr.mul_add(cov[1], vfg * cov[3]));
            let mut b = vfb.mul_add(cov[5], vfr.mul_add(cov[2], vfg * cov[4]));
            let mut m = maximumf(maximumf(r_.abs(), g.abs()), b.abs());
            if m > 1e-10 {
                m = 1.0 / m;
                r_ *= m;
                g *= m;
                b *= m;
            }
            vfr = r_;
            vfg = g;
            vfb = b;
        }
        let mut len = vfb.mul_add(vfb, vfr.mul_add(vfr, vfg * vfg));
        if len < 1e-10 {
            axis = [0.0; 4];
        } else {
            len = 1.0 / len.sqrt();
            vfr *= len;
            vfg *= len;
            vfb *= len;
            axis = [vfr, vfg, vfb, 0.0];
        }
    }
    if vdot(&axis, &axis) < 0.5 {
        axis = [1.0, 1.0, 1.0, if p.has_alpha { 1.0 } else { 0.0 }];
        vnormalize(&mut axis);
    }

    let (mut l, mut h) = (1e9f32, -1e9f32);
    for c in p.pixels {
        let q = vsub(&from_color(c), &mean_scaled);
        let d = vdot(&q, &axis);
        l = minimumf(l, d);
        h = maximumf(h, d);
    }
    let k = 1.0f32 / 255.0;
    l *= k;
    h *= k;
    let b0 = vmul(&axis, l);
    let b1 = vmul(&axis, h);
    let mut min_color = vsaturate(&vadd(&mean_color, &b0));
    let mut max_color = vsaturate(&vadd(&mean_color, &b1));
    let white = [1.0f32; 4];
    if vdot(&min_color, &white) > vdot(&max_color, &white) {
        std::mem::swap(&mut min_color, &mut max_color);
    }

    if find_optimal_solution(mode, min_color, max_color, p, r, comp) == 0 {
        return 0;
    }
    if comp.try_least_squares {
        let (xl, xh) = least_squares(p, &r.selectors[..np]);
        if find_optimal_solution(mode, xl, xh, p, r, comp) == 0 {
            return 0;
        }
    }
    if comp.uber_level > 0 {
        let mut sel_temp = [0u8; 16];
        sel_temp[..np].copy_from_slice(&r.selectors[..np]);
        let mut sel_temp1 = [0u8; 16];
        let nsw = p.sel_weights.len() as u32;
        let max_selector = nsw as i32 - 1;
        let (mut min_sel, mut max_sel) = (16u32, 0u32);
        for &s in &sel_temp[..np] {
            min_sel = min_sel.min(u32::from(s));
            max_sel = max_sel.max(u32::from(s));
        }
        for i in 0..np {
            let mut s = u32::from(sel_temp[i]);
            if s == min_sel && s < nsw - 1 {
                s += 1;
            }
            sel_temp1[i] = s as u8;
        }
        let (xl, xh) = least_squares(p, &sel_temp1[..np]);
        if find_optimal_solution(mode, xl, xh, p, r, comp) == 0 {
            return 0;
        }
        for i in 0..np {
            let mut s = u32::from(sel_temp[i]);
            if s == max_sel && s > 0 {
                s -= 1;
            }
            sel_temp1[i] = s as u8;
        }
        let (xl, xh) = least_squares(p, &sel_temp1[..np]);
        if find_optimal_solution(mode, xl, xh, p, r, comp) == 0 {
            return 0;
        }
        for i in 0..np {
            let mut s = u32::from(sel_temp[i]);
            if s == min_sel && s < nsw - 1 {
                s += 1;
            } else if s == max_sel && s > 0 {
                s -= 1;
            }
            sel_temp1[i] = s as u8;
        }
        let (xl, xh) = least_squares(p, &sel_temp1[..np]);
        if find_optimal_solution(mode, xl, xh, p, r, comp) == 0 {
            return 0;
        }
        let uber_err_thresh = ((np as u64) * 56) >> 4;
        if comp.uber_level >= 2 && r.best_err > uber_err_thresh {
            let q = if comp.uber_level >= 4 { comp.uber_level as i32 - 2 } else { 1 };
            for ly in -q..=1 {
                for hy in (max_selector - 1)..=(max_selector + q) {
                    if ly == 0 && hy == max_selector {
                        continue;
                    }
                    for i in 0..np {
                        let v = (max_selector as f32 * (f32::from(sel_temp[i]) - ly as f32) / (hy as f32 - ly as f32) + 0.5).floor();
                        sel_temp1[i] = clampf(v, 0.0, max_selector as f32) as u8;
                    }
                    let (xl, xh) = least_squares(p, &sel_temp1[..np]);
                    if find_optimal_solution(mode, xl, xh, p, r, comp) == 0 {
                        return 0;
                    }
                }
            }
        }
    }

    if mode == 1 {
        // Try the block's mean as a single colour.
        let mut avg = *r;
        let [cr, cg, cb] = [0, 1, 2].map(|c| mean_color[c].mul_add(255.0, 0.5) as i32 as u32 as usize);
        let avg_err = pack_mode1_to_one_color(p, &mut avg, cr, cg, cb, true);
        if avg_err < r.best_err {
            *r = avg;
            r.selectors[..np].copy_from_slice(&avg.selectors_temp[..np]);
            r.best_err = avg_err;
        }
    } else if mode == 7 {
        let mut avg = *r;
        let ca = [0, 1, 2, 3].map(|c| mean_color[c].mul_add(255.0, 0.5) as i32 as u32 as usize);
        let avg_err = pack_mode7_to_one_color(p, &mut avg, ca, true);
        if avg_err < r.best_err {
            *r = avg;
            r.selectors[..np].copy_from_slice(&avg.selectors_temp[..np]);
            r.best_err = avg_err;
        }
    }
    r.best_err
}

// --- Partition estimation (modes 1 and 7) ---

fn est_mode1(px: &[Color], w: &[u32; 4], best_so_far: u64) -> u64 {
    let (mut lo, mut hi) = ([255u32; 3], [0u32; 3]);
    for c in px {
        for k in 0..3 {
            lo[k] = lo[k].min(u32::from(c[k]));
            hi[k] = hi[k].max(u32::from(c[k]));
        }
    }
    const N: usize = 8;
    let mut wc = [[0u32; 3]; N];
    wc[0] = lo;
    wc[N - 1] = hi;
    for (i, row) in wc.iter_mut().enumerate().take(N - 1).skip(1) {
        for k in 0..3 {
            row[k] = (lo[k] * (64 - WEIGHTS3[i]) + hi[k] * WEIGHTS3[i] + 32) >> 6;
        }
    }
    let a = [hi[0] as i32 - lo[0] as i32, hi[1] as i32 - lo[1] as i32, hi[2] as i32 - lo[2] as i32];
    let mut dots = [0i32; N];
    for (d, c) in dots.iter_mut().zip(wc.iter()) {
        *d = c[0] as i32 * a[0] + c[1] as i32 * a[1] + c[2] as i32 * a[2];
    }
    let mut thresh = [0i32; N - 1];
    for i in 0..N - 1 {
        thresh[i] = (dots[i] + dots[i + 1] + 1) >> 1;
    }
    let mut total = 0u64;
    for c in px {
        let d = a[0] * i32::from(c[0]) + a[1] * i32::from(c[1]) + a[2] * i32::from(c[2]);
        let s = (0..N - 1).rev().find(|&t| d >= thresh[t]).map_or(0, |t| t + 1);
        let e = &wc[s];
        let (dr, dg, db) = (e[0] as i32 - i32::from(c[0]), e[1] as i32 - i32::from(c[1]), e[2] as i32 - i32::from(c[2]));
        total += u64::from(w[0] * (dr * dr) as u32 + w[1] * (dg * dg) as u32 + w[2] * (db * db) as u32);
        if total > best_so_far {
            break;
        }
    }
    total
}

fn est_mode7(px: &[Color], w: &[u32; 4], best_so_far: u64) -> u64 {
    let (mut lo, mut hi) = ([255u32; 4], [0u32; 4]);
    for c in px {
        for k in 0..4 {
            lo[k] = lo[k].min(u32::from(c[k]));
            hi[k] = hi[k].max(u32::from(c[k]));
        }
    }
    const N: usize = 4;
    let mut wc = [[0u32; 4]; N];
    wc[0] = lo;
    wc[N - 1] = hi;
    for (i, row) in wc.iter_mut().enumerate().take(N - 1).skip(1) {
        for k in 0..4 {
            row[k] = (lo[k] * (64 - WEIGHTS2[i]) + hi[k] * WEIGHTS2[i] + 32) >> 6;
        }
    }
    let a = [0, 1, 2, 3].map(|k| hi[k] as i32 - lo[k] as i32);
    let mut dots = [0i32; N];
    for (d, c) in dots.iter_mut().zip(wc.iter()) {
        *d = c[0] as i32 * a[0] + c[1] as i32 * a[1] + c[2] as i32 * a[2] + c[3] as i32 * a[3];
    }
    let mut thresh = [0i32; N - 1];
    for i in 0..N - 1 {
        thresh[i] = (dots[i] + dots[i + 1] + 1) >> 1;
    }
    let mut total = 0u64;
    for c in px {
        let d = a[0] * i32::from(c[0]) + a[1] * i32::from(c[1]) + a[2] * i32::from(c[2]) + a[3] * i32::from(c[3]);
        let s = (0..N - 1).rev().find(|&t| d >= thresh[t]).map_or(0, |t| t + 1);
        let e = &wc[s];
        let dd = [0, 1, 2, 3].map(|k| e[k] as i32 - i32::from(c[k]));
        total += u64::from(w[0] * (dd[0] * dd[0]) as u32 + w[1] * (dd[1] * dd[1]) as u32 + w[2] * (dd[2] * dd[2]) as u32 + w[3] * (dd[3] * dd[3]) as u32);
        if total > best_so_far {
            break;
        }
    }
    total
}

/// Which key partitions must rank best among the first 14 before partitions 14..=34 are tried.
const PARTITION_PREDICTORS: [u32; 35] = [
    u32::MAX,
    u32::MAX,
    u32::MAX,
    u32::MAX,
    u32::MAX,
    (1 << 1) | (1 << 2) | (1 << 8),
    (1 << 1) | (1 << 3) | (1 << 7),
    u32::MAX,
    u32::MAX,
    (1 << 2) | (1 << 8) | (1 << 16),
    (1 << 7) | (1 << 3) | (1 << 15),
    u32::MAX,
    (1 << 8) | (1 << 14) | (1 << 16),
    (1 << 7) | (1 << 14) | (1 << 15),
    u32::MAX,
    u32::MAX,
    u32::MAX,
    u32::MAX,
    (1 << 14) | (1 << 15),
    (1 << 16) | (1 << 22) | (1 << 14),
    (1 << 17) | (1 << 24) | (1 << 14),
    (1 << 2) | (1 << 14) | (1 << 15) | (1 << 1),
    u32::MAX,
    (1 << 1) | (1 << 3) | (1 << 14) | (1 << 16) | (1 << 22),
    u32::MAX,
    (1 << 1) | (1 << 2) | (1 << 15) | (1 << 17) | (1 << 24),
    (1 << 1) | (1 << 3) | (1 << 22),
    u32::MAX,
    u32::MAX,
    u32::MAX,
    (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17),
    u32::MAX,
    u32::MAX,
    (1 << 1) | (1 << 2) | (1 << 3) | (1 << 27) | (1 << 4) | (1 << 24),
    (1 << 14) | (1 << 15) | (1 << 16) | (1 << 11) | (1 << 17) | (1 << 27),
];

/// Partitions sorted by usefulness (slot 34 is the checkerboard, pattern 34).
const SORTED_PARTITION_ORDER: [u8; 64] = [
    0, 13, 1, 2, 15, 14, 10, 16, 3, 23, 26, 6, 7, 21, 19, 29, 8, 4, 9, 20, 5, 31, 22, 17, 18, 11, 12, 30, 24, 25, 28, 27, 32, 33, 34, 45, 46, 51, 49, 50, 48,
    38, 39, 37, 53, 52, 54, 36, 57, 58, 55, 41, 40, 42, 43, 59, 44, 56, 47, 35, 60, 63, 62, 61,
];

fn estimate_partition(px: &[Color; 16], comp: &Bc7Params, w: &[u32; 4], mode: u32) -> u32 {
    let total_partitions = comp.max_partitions.min(MAX_PARTITIONS);
    if total_partitions <= 1 {
        return 0;
    }
    let mut best_err = u64::MAX;
    let mut best_partition = 0u32;
    let mut best_key_partition = 0u32;
    let mut iter = 0u32;
    while iter < total_partitions && best_err > 0 {
        let partition = u32::from(SORTED_PARTITION_ORDER[iter as usize]);
        if comp.mode17_partition_estimation_filterbank && (14..=34).contains(&iter) {
            let mask = 1u32.checked_shl(best_key_partition + 1).unwrap_or(0);
            if PARTITION_PREDICTORS[partition as usize] & mask == 0 {
                if iter == 34 {
                    break;
                }
                iter += 1;
                continue;
            }
        }
        let part = &PARTITION2[partition as usize];
        let mut subset = [[[0u8; 4]; 16]; 2];
        let mut count = [0usize; 2];
        for (i, &s) in part.iter().enumerate() {
            let s = usize::from(s);
            subset[s][count[s]] = px[i];
            count[s] += 1;
        }
        let mut total = 0u64;
        let mut s = 0;
        while s < 2 && total < best_err {
            let pixels = &subset[s][..count[s]];
            total += if mode == 7 { est_mode7(pixels, w, best_err) } else { est_mode1(pixels, w, best_err) };
            s += 1;
        }
        if partition < 16 {
            total = (total as f64).mul_add(f64::from(comp.low_frequency_partition_weight), 0.5) as u64;
        }
        if total < best_err {
            best_err = total;
            best_partition = partition;
        }
        if partition == 34 && best_partition != 34 {
            break;
        }
        if iter == 13 {
            best_key_partition = best_partition;
        }
        iter += 1;
    }
    best_partition
}

// --- Block assembly ---

#[derive(Clone, Copy, Default)]
struct OptResults {
    mode: u32,
    partition: u32,
    selectors: [u8; 16],
    alpha_selectors: [u8; 16],
    low: [Color; 2],
    high: [Color; 2],
    pbits: [[u32; 2]; 2],
}

/// LSB-first bit writer over one 128-bit block.
struct BitWriter {
    bits: u128,
    at: u32,
}

impl BitWriter {
    fn put(&mut self, val: u32, n: u32) {
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        debug_assert!(val <= mask, "field value {val} wider than {n} bits");
        self.bits |= u128::from(val & mask) << self.at;
        self.at += n;
    }
}

/// `encode_bc7_block` for the modes this encoder emits (1, 5, 6, 7: one or two subsets, no index
/// selector, rotation 0).
fn encode_block(r: &OptResults) -> [u8; 16] {
    let mode = r.mode as usize;
    let subsets = NUM_SUBSETS[mode];
    let part: &[u8; 16] = if subsets == 1 { &[0; 16] } else { &PARTITION2[r.partition as usize] };
    let (mut csel, mut asel) = (r.selectors, r.alpha_selectors);
    let (mut low, mut high, mut pbits) = (r.low, r.high, r.pbits);
    let separate_alpha = mode == 4 || mode == 5;
    let mut anchor = [-1i32; 3];
    for k in 0..subsets {
        let anchor_index = if k == 0 { 0 } else { usize::from(crate::tables::ANCHOR2[r.partition as usize]) };
        anchor[k] = anchor_index as i32;
        let num_color_indices = 1u32 << COLOR_INDEX_BITCOUNT[mode];
        if u32::from(csel[anchor_index]) & (num_color_indices >> 1) != 0 {
            for i in 0..16 {
                if usize::from(part[i]) == k {
                    csel[i] = (num_color_indices - 1 - u32::from(csel[i])) as u8;
                }
            }
            if separate_alpha {
                for q in 0..3 {
                    std::mem::swap(&mut low[k][q], &mut high[k][q]);
                }
            } else {
                std::mem::swap(&mut low[k], &mut high[k]);
            }
            if !MODE_HAS_SHARED_P_BITS[mode] {
                pbits[k].swap(0, 1);
            }
        }
        if separate_alpha {
            let num_alpha_indices = 1u32 << ALPHA_INDEX_BITCOUNT[mode];
            if u32::from(asel[anchor_index]) & (num_alpha_indices >> 1) != 0 {
                for i in 0..16 {
                    if usize::from(part[i]) == k {
                        asel[i] = (num_alpha_indices - 1 - u32::from(asel[i])) as u8;
                    }
                }
                std::mem::swap(&mut low[k][3], &mut high[k][3]);
            }
        }
    }
    let mut w = BitWriter { bits: 0, at: 0 };
    w.put(1 << mode, mode as u32 + 1);
    if mode == 4 || mode == 5 {
        w.put(0, 2); // rotation
    }
    if PARTITION_BITS[mode] > 0 {
        w.put(r.partition, PARTITION_BITS[mode]);
    }
    let comps = if mode >= 4 { 4 } else { 3 };
    for comp in 0..comps {
        let prec = if comp == 3 { ALPHA_PRECISION[mode] } else { COLOR_PRECISION[mode] };
        for s in 0..subsets {
            w.put(u32::from(low[s][comp]), prec);
            w.put(u32::from(high[s][comp]), prec);
        }
    }
    if MODE_HAS_P_BITS[mode] {
        for pb in pbits.iter().take(subsets) {
            w.put(pb[0], 1);
            if !MODE_HAS_SHARED_P_BITS[mode] {
                w.put(pb[1], 1);
            }
        }
    }
    let is_anchor = |i: usize| anchor.contains(&(i as i32));
    for (i, &s) in csel.iter().enumerate() {
        let n = COLOR_INDEX_BITCOUNT[mode] - u32::from(is_anchor(i));
        w.put(u32::from(s), n);
    }
    if separate_alpha {
        for (i, &s) in asel.iter().enumerate() {
            let n = ALPHA_INDEX_BITCOUNT[mode] - u32::from(is_anchor(i));
            w.put(u32::from(s), n);
        }
    }
    debug_assert_eq!(w.at, 128);
    w.bits.to_le_bytes()
}

/// Two-subset compression shared by modes 1 and 7: the partition estimate, then each subset;
/// stops early once the weighted error exceeds `best_err`. Returns the weighted error.
fn two_subset(px: &[Color; 16], comp: &Bc7Params, w: &[u32; 4], mode: u32, best_err: u64, weight: f32) -> (u64, OptResults) {
    let partition = estimate_partition(px, comp, w, mode);
    let part = &PARTITION2[partition as usize];
    let mut subset = [[[0u8; 4]; 16]; 2];
    let mut index = [[0u8; 16]; 2];
    let mut count = [0usize; 2];
    for (i, &s) in part.iter().enumerate() {
        let s = usize::from(s);
        subset[s][count[s]] = px[i];
        index[s][count[s]] = i as u8;
        count[s] += 1;
    }
    let (sel_weights, sel_weightsx, comp_bits, has_alpha, share): (&'static [u32], &'static [[f32; 4]], u32, bool, bool) =
        if mode == 7 { (&WEIGHTS2, &WEIGHTS2X, 5, true, false) } else { (&WEIGHTS3, &WEIGHTS3X, 6, false, true) };
    let mut results = [CellResults::new(); 2];
    let mut trial_err = 0u64;
    for s in 0..2 {
        let p = CellParams {
            pixels: &subset[s][..count[s]],
            sel_weights,
            sel_weightsx,
            comp_bits,
            weights: *w,
            has_alpha,
            has_pbits: true,
            endpoints_share_pbit: share,
        };
        trial_err += color_cell_compression(mode, &p, &mut results[s], comp);
        if (trial_err as f32).mul_add(weight, 0.5) as u64 > best_err {
            break;
        }
    }
    let err = (trial_err as f32).mul_add(weight, 0.5) as u64;
    let mut opt = OptResults { mode, partition, ..OptResults::default() };
    for s in 0..2 {
        for i in 0..count[s] {
            opt.selectors[usize::from(index[s][i])] = results[s].selectors[i];
        }
        opt.low[s] = results[s].low;
        opt.high[s] = results[s].high;
        opt.pbits[s] = results[s].pbits;
    }
    (err, opt)
}

fn handle_alpha_block(px: &[Color; 16], comp: &Bc7Params, w: &[u32; 4]) -> [u8; 16] {
    let mut best_err = u64::MAX;
    let mut best: Option<OptResults> = None;
    if comp.mode_mask & (1 << 6) != 0 {
        let p = CellParams {
            pixels: px,
            sel_weights: &WEIGHTS4,
            sel_weightsx: &WEIGHTS4X,
            comp_bits: 7,
            weights: *w,
            has_alpha: true,
            has_pbits: true,
            endpoints_share_pbit: false,
        };
        let mut r6 = CellResults::new();
        best_err = (color_cell_compression(6, &p, &mut r6, comp) as f32).mul_add(comp.mode6_error_weight, 0.5) as u64;
        best = Some(OptResults {
            mode: 6,
            selectors: r6.selectors,
            low: [r6.low, [0; 4]],
            high: [r6.high, [0; 4]],
            pbits: [r6.pbits, [0; 2]],
            ..OptResults::default()
        });
    }
    if best_err > 0 && comp.mode_mask & (1 << 5) != 0 {
        let (mut lo_a, mut hi_a) = (255u32, 0u32);
        for c in px {
            lo_a = lo_a.min(u32::from(c[3]));
            hi_a = hi_a.max(u32::from(c[3]));
        }
        let (mode5_err, opt5) = mode5(px, comp, w, lo_a, hi_a);
        let mode5_err = (mode5_err as f32).mul_add(comp.mode5_error_weight, 0.5) as u64;
        if mode5_err < best_err {
            best_err = mode5_err;
            best = Some(opt5);
        }
    }
    if best_err > 0 && comp.mode_mask & (1 << 7) != 0 {
        let (err7, opt7) = two_subset(px, comp, w, 7, best_err, comp.mode7_error_weight);
        if err7 < best_err {
            best = Some(opt7);
        }
    }
    best.map_or([0; 16], |b| encode_block(&b))
}

/// `handle_alpha_block_mode5`: RGB through colour-cell compression (2-bit indices, 7-bit
/// endpoints), alpha by its own 2-bit fit refined by least squares.
fn mode5(px: &[Color; 16], comp: &Bc7Params, w: &[u32; 4], mut lo_a: u32, mut hi_a: u32) -> (u64, OptResults) {
    let p = CellParams {
        pixels: px,
        sel_weights: &WEIGHTS2,
        sel_weightsx: &WEIGHTS2X,
        comp_bits: 7,
        weights: *w,
        has_alpha: false,
        has_pbits: false,
        endpoints_share_pbit: false,
    };
    let mut r5 = CellResults::new();
    let mut err = color_cell_compression(5, &p, &mut r5, comp);
    let mut opt = OptResults { mode: 5, selectors: r5.selectors, low: [r5.low, [0; 4]], high: [r5.high, [0; 4]], ..OptResults::default() };
    if lo_a == hi_a {
        opt.low[0][3] = lo_a as u8;
        opt.high[0][3] = hi_a as u8;
        opt.alpha_selectors = [0; 16];
        return (err, opt);
    }
    let mut alpha_err = u64::MAX;
    let total_passes = if comp.uber_level >= 1 { 3 } else { 2 };
    for pass in 0..total_passes {
        let v0 = lo_a as i32;
        let v3 = hi_a as i32;
        let (ws1, ws2) = (21, 43);
        let vals = [v0, (v0 * (64 - ws1) + v3 * ws1 + 32) >> 6, (v0 * (64 - ws2) + v3 * ws2 + 32) >> 6, v3];
        let mut trial_sel = [0u8; 16];
        let mut trial_err = 0u64;
        for (i, c) in px.iter().enumerate() {
            let a = i32::from(c[3]);
            let mut s = 0;
            let mut be = (a - vals[0]).abs();
            for (k, &v) in vals.iter().enumerate().skip(1) {
                let e = (a - v).abs();
                if e < be {
                    be = e;
                    s = k;
                }
            }
            trial_sel[i] = s as u8;
            trial_err += u64::from((be * be) as u32 * w[3]);
        }
        if trial_err < alpha_err {
            alpha_err = trial_err;
            opt.low[0][3] = lo_a as u8;
            opt.high[0][3] = hi_a as u8;
            opt.alpha_selectors = trial_sel;
        }
        if pass != total_passes - 1 {
            let (xl, xh) = least_squares_a(&trial_sel, &WEIGHTS2X, px);
            let mut new_lo = clampi((xl + 0.5).floor() as i32, 0, 255) as u32;
            let mut new_hi = clampi((xh + 0.5).floor() as i32, 0, 255) as u32;
            if new_lo > new_hi {
                std::mem::swap(&mut new_lo, &mut new_hi);
            }
            if new_lo == lo_a && new_hi == hi_a {
                break;
            }
            lo_a = new_lo;
            hi_a = new_hi;
        }
    }
    err += alpha_err;
    (err, opt)
}

fn handle_opaque_block(px: &[Color; 16], comp: &Bc7Params, w: &[u32; 4]) -> [u8; 16] {
    let mut best_err = u64::MAX;
    let mut opt = OptResults::default();
    if comp.mode_mask & (1 << 6) != 0 {
        let p = CellParams {
            pixels: px,
            sel_weights: &WEIGHTS4,
            sel_weightsx: &WEIGHTS4X,
            comp_bits: 7,
            weights: *w,
            has_alpha: false,
            has_pbits: true,
            endpoints_share_pbit: false,
        };
        let mut r6 = CellResults::new();
        best_err = (color_cell_compression(6, &p, &mut r6, comp) as f32).mul_add(comp.mode6_error_weight, 0.5) as u64;
        opt =
            OptResults { mode: 6, selectors: r6.selectors, low: [r6.low, [0; 4]], high: [r6.high, [0; 4]], pbits: [r6.pbits, [0; 2]], ..OptResults::default() };
    }
    if best_err > 0 && comp.max_partitions > 0 && comp.mode_mask & (1 << 1) != 0 {
        let (err1, opt1) = two_subset(px, comp, w, 1, best_err, comp.mode1_error_weight);
        if err1 < best_err {
            opt = opt1;
        }
    }
    encode_block(&opt)
}

/// Encode one 4x4 block of straight RGBA pixels (row-major) to a 16-byte BC7 block
/// (`bc7enc_compress_block`). Blocks with any alpha below 255 use modes 5/6/7, others modes 1/6.
pub fn compress_block(px: &[Color; 16], comp: &Bc7Params) -> [u8; 16] {
    let w = comp.weights;
    if px.iter().any(|c| c[3] < 255) { handle_alpha_block(px, comp, &w) } else { handle_opaque_block(px, comp, &w) }
}
