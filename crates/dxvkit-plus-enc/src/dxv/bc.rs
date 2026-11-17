//! BC1 colour and BC4 block encoders for the DXV3 encoder (fast / best effort).
//!
//! Ported to safe Rust from DXVKit `src/enc_bc.c` and `tools/gen_bc_tables.c`
//! (github.com/AmritusG/dxvkit @ fb506da, MIT, Copyright (c) 2026 Amrit Rosell — `LICENSES/DXVKit-MIT.txt`).
//! The search order and the float arithmetic follow the C code operation by operation, with the fused
//! multiply-adds exactly where Apple clang forms them when SwiftPM builds DXVKit for Swift GlEnc
//! (`-ffp-contract=on`: 24 `fmadd` / `fnmadd` in `enc_bc.o` — the principal-axis search of `dxvk_bc1_encode` and
//! the least-squares refit of `dxvk_bc4_encode_fine`), so the same pixels give the same blocks as GlEnc's.
//!
//! Lint note: every value is a byte, a 5/6-bit endpoint, a palette entry or a 16-texel block sum,
//! so the arithmetic cannot overflow; arrays are fixed-size and indexed by bounded loop counters or
//! values masked to their length.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing, clippy::needless_range_loop)]

use std::sync::OnceLock;

/// The single-colour endpoint tables and the nearest 5 / 6-bit quantisers (`gen_bc_tables.c`).
struct Tables {
    four5: [[u8; 2]; 256],
    four6: [[u8; 2]; 256],
    three5: [[u8; 2]; 256],
    three6: [[u8; 2]; 256],
    quant5: [u8; 256],
    quant6: [u8; 256],
}

fn expand(v: i32, bits: u32) -> i32 {
    if bits == 5 { (v << 3) | (v >> 2) } else { (v << 2) | (v >> 4) }
}

fn solid(bits: u32, three: bool) -> [[u8; 2]; 256] {
    let mut t = [[0u8; 2]; 256];
    for (v, e) in t.iter_mut().enumerate() {
        let v = v as i32;
        let (mut best, mut e0, mut e1) = (1i32 << 30, 0i32, 0i32);
        for a in 0..(1i32 << bits) {
            for b in 0..(1i32 << bits) {
                let (ea, eb) = (expand(a, bits), expand(b, bits));
                let val = if three { (ea + eb) / 2 } else { (2 * ea + eb) / 3 };
                let score = (val - v).abs() * 256 + (a - b).abs();
                if score < best {
                    best = score;
                    e0 = a;
                    e1 = b;
                }
            }
        }
        *e = [e0 as u8, e1 as u8];
    }
    t
}

fn quant(bits: u32) -> [u8; 256] {
    let mut t = [0u8; 256];
    for (v, q) in t.iter_mut().enumerate() {
        let v = v as i32;
        let mut best = 0i32;
        for c in 1..(1i32 << bits) {
            if (expand(c, bits) - v).abs() < (expand(best, bits) - v).abs() {
                best = c;
            }
        }
        *q = best as u8;
    }
    t
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| Tables {
        four5: solid(5, false),
        four6: solid(6, false),
        three5: solid(5, true),
        three6: solid(6, true),
        quant5: quant(5),
        quant6: quant(6),
    })
}

fn exp5(v: i32) -> i32 {
    (v << 3) | (v >> 2)
}
fn exp6(v: i32) -> i32 {
    (v << 2) | (v >> 4)
}
fn pack565(q: &[i32; 3]) -> u16 {
    ((q[0] << 11) | (q[1] << 5) | q[2]) as u16
}

struct Bc1 {
    px: [[i32; 3]; 16],
    best_err: u32,
    best: [u8; 8],
    best_q: [[i32; 3]; 2],
    best_three: bool,
    best_idx: [u8; 16],
}

impl Bc1 {
    /// `bc1_try`: endpoints in the requested mode, ordered as the decoder needs; keeps the best.
    fn try_pair(&mut self, q0: &[i32; 3], q1: &[i32; 3], mut three: bool) -> u32 {
        let (p0, p1) = (pack565(q0), pack565(q1));
        let (mut a, mut b) = (q0, q1);
        let (mut usable, mut swapped) = (4usize, false);
        if three {
            if p0 > p1 {
                a = q1;
                b = q0;
                swapped = true;
            }
            usable = 3;
        } else if p0 < p1 {
            a = q1;
            b = q0;
            swapped = true;
        } else if p0 == p1 {
            usable = 1;
            three = true;
        }
        let mut pal = [[0i32; 3]; 4];
        for k in 0..3 {
            let (ea, eb) = if k == 1 { (exp6(a[k]), exp6(b[k])) } else { (exp5(a[k]), exp5(b[k])) };
            pal[0][k] = ea;
            pal[1][k] = eb;
            if three {
                pal[2][k] = (ea + eb) / 2;
                pal[3][k] = 0;
            } else {
                pal[2][k] = (2 * ea + eb) / 3;
                pal[3][k] = (ea + 2 * eb) / 3;
            }
        }
        let (mut err, mut idx) = (0u32, 0u32);
        let mut chosen = [0u8; 16];
        for i in 0..16 {
            let (mut be, mut bi) = (u32::MAX, 0usize);
            for (j, p) in pal.iter().enumerate().take(usable) {
                let (dr, dg, db) = (self.px[i][0] - p[0], self.px[i][1] - p[1], self.px[i][2] - p[2]);
                let e = (dr * dr + dg * dg + db * db) as u32;
                if e < be {
                    be = e;
                    bi = j;
                }
            }
            err = err.wrapping_add(be);
            chosen[i] = bi as u8;
            idx |= (bi as u32) << (2 * i);
            if err >= self.best_err {
                return err;
            }
        }
        self.best_err = err;
        self.best[0..2].copy_from_slice(&pack565(a).to_le_bytes());
        self.best[2..4].copy_from_slice(&pack565(b).to_le_bytes());
        self.best[4..8].copy_from_slice(&idx.to_le_bytes());
        self.best_q = [*q0, *q1];
        self.best_three = three && usable == 3;
        for i in 0..16 {
            let mut s = chosen[i];
            // 0 <-> 1 when the endpoints were swapped; four-colour also 2 <-> 3.
            if swapped && (s < 2 || !three) {
                s ^= 1;
            }
            self.best_idx[i] = s;
        }
        err
    }

    /// Least-squares endpoints for the current best indices (`bc1_refit`).
    fn refit(&self, three: bool) -> Option<([i32; 3], [i32; 3])> {
        const W4: [i64; 4] = [3, 0, 2, 1];
        const W3: [i64; 4] = [2, 0, 1, 0];
        let w = if three { &W3 } else { &W4 };
        let unit: i64 = if three { 2 } else { 3 };
        let (mut aa, mut ab, mut bb) = (0i64, 0i64, 0i64);
        let (mut ax, mut bx) = ([0i64; 3], [0i64; 3]);
        for i in 0..16 {
            let s = usize::from(self.best_idx[i] & 3);
            let (a, b) = (w[s], unit - w[s]);
            aa += a * a;
            ab += a * b;
            bb += b * b;
            for k in 0..3 {
                ax[k] += a * i64::from(self.px[i][k]);
                bx[k] += b * i64::from(self.px[i][k]);
            }
        }
        let det = aa * bb - ab * ab;
        if det == 0 {
            return None;
        }
        let (mut e0, mut e1) = ([0f32; 3], [0f32; 3]);
        for k in 0..3 {
            e0[k] = unit as f32 * ((bb * ax[k] - ab * bx[k]) as f32) / det as f32;
            e1[k] = unit as f32 * ((aa * bx[k] - ab * ax[k]) as f32) / det as f32;
        }
        Some((quantize(&e0), quantize(&e1)))
    }

    /// ±1 steps on each endpoint component while the error drops (`bc1_local_search`).
    fn local_search(&mut self, three: bool) {
        const LIMIT: [i32; 3] = [31, 63, 31];
        for _ in 0..8 {
            let mut improved = false;
            for e in 0..2 {
                for k in 0..3 {
                    for dir in [-1i32, 1] {
                        let mut q = self.best_q;
                        let before = self.best_err;
                        q[e][k] += dir;
                        if q[e][k] < 0 || q[e][k] > LIMIT[k] {
                            continue;
                        }
                        let (a, b) = (q[0], q[1]);
                        self.try_pair(&a, &b, three);
                        if self.best_err < before {
                            improved = true;
                        }
                    }
                }
            }
            if !improved || self.best_err == 0 {
                break;
            }
        }
    }
}

fn clamp255(v: i32) -> usize {
    v.clamp(0, 255) as usize
}

fn quantize(c: &[f32; 3]) -> [i32; 3] {
    let t = tables();
    [i32::from(t.quant5[clamp255((c[0] + 0.5) as i32)]), i32::from(t.quant6[clamp255((c[1] + 0.5) as i32)]), i32::from(t.quant5[clamp255((c[2] + 0.5) as i32)])]
}

/// One BC1 colour block from 16 RGBA pixels (pixel (x, y) at `4y + x`). `bc3_colour` = the colour
/// half of a BC3 block (four-colour only).
pub fn bc1_encode(px: &[[u8; 4]; 16], best: bool, bc3_colour: bool) -> [u8; 8] {
    let mut c = Bc1 { px: [[0; 3]; 16], best_err: u32::MAX, best: [0; 8], best_q: [[0; 3]; 2], best_three: false, best_idx: [0; 16] };
    let (mut mn, mut mx) = ([255i32; 3], [0i32; 3]);
    let mut mean = [0f32; 3];
    for i in 0..16 {
        for k in 0..3 {
            let v = i32::from(px[i][k]);
            c.px[i][k] = v;
            mn[k] = mn[k].min(v);
            mx[k] = mx[k].max(v);
            mean[k] += v as f32;
        }
    }
    let t = tables();
    if mn == mx {
        let pick = |t5: &[[u8; 2]; 256], t6: &[[u8; 2]; 256]| {
            let (r, g, b) = (t5[mn[0] as usize], t6[mn[1] as usize], t5[mn[2] as usize]);
            ([i32::from(r[0]), i32::from(g[0]), i32::from(b[0])], [i32::from(r[1]), i32::from(g[1]), i32::from(b[1])])
        };
        let (a, b) = pick(&t.four5, &t.four6);
        c.try_pair(&a, &b, false);
        if best && !bc3_colour && c.best_err != 0 {
            let (a, b) = pick(&t.three5, &t.three6);
            c.try_pair(&a, &b, true);
        }
        return c.best;
    }

    for m in mean.iter_mut() {
        *m *= 1.0f32 / 16.0f32;
    }
    let mut cov = [0f32; 6];
    for i in 0..16 {
        let d0 = c.px[i][0] as f32 - mean[0];
        let d1 = c.px[i][1] as f32 - mean[1];
        let d2 = c.px[i][2] as f32 - mean[2];
        // `cov[k] += a * b`: one fused multiply-add each in Swift GlEnc's DXVKit build (enc_bc.o).
        cov[0] = d0.mul_add(d0, cov[0]);
        cov[1] = d0.mul_add(d1, cov[1]);
        cov[2] = d0.mul_add(d2, cov[2]);
        cov[3] = d1.mul_add(d1, cov[3]);
        cov[4] = d1.mul_add(d2, cov[4]);
        cov[5] = d2.mul_add(d2, cov[5]);
    }
    let mut axis = [(mx[0] - mn[0]) as f32, (mx[1] - mn[1]) as f32, (mx[2] - mn[2]) as f32];
    for _ in 0..4 {
        // `a*b + c*d + e*f` as clang contracts it: fma(e, f, fma(a, b, c*d)).
        let x = cov[2].mul_add(axis[2], cov[0].mul_add(axis[0], cov[1] * axis[1]));
        let y = cov[4].mul_add(axis[2], cov[1].mul_add(axis[0], cov[3] * axis[1]));
        let z = cov[5].mul_add(axis[2], cov[2].mul_add(axis[0], cov[4] * axis[1]));
        let mut m = if x < 0.0 { -x } else { x };
        let ay = if y < 0.0 { -y } else { y };
        if ay > m {
            m = ay;
        }
        let az = if z < 0.0 { -z } else { z };
        if az > m {
            m = az;
        }
        if m < 1e-6f32 {
            break;
        }
        axis = [x / m, y / m, z / m];
    }
    let (mut q0, mut q1);
    {
        let (mut lo, mut hi) = (1e30f32, -1e30f32);
        let mut len2 = axis[2].mul_add(axis[2], axis[0].mul_add(axis[0], axis[1] * axis[1]));
        if len2 < 1e-12f32 {
            axis = [1.0; 3];
            len2 = 3.0;
        }
        for i in 0..16 {
            let (d0, d1, d2) = (c.px[i][0] as f32 - mean[0], c.px[i][1] as f32 - mean[1], c.px[i][2] as f32 - mean[2]);
            let p = d2.mul_add(axis[2], d0.mul_add(axis[0], d1 * axis[1]));
            if p < lo {
                lo = p;
            }
            if p > hi {
                hi = p;
            }
        }
        let (mut e0, mut e1) = ([0f32; 3], [0f32; 3]);
        for k in 0..3 {
            e0[k] = mean[k] + axis[k] * hi / len2;
            e1[k] = mean[k] + axis[k] * lo / len2;
        }
        q0 = quantize(&e0);
        q1 = quantize(&e1);
    }
    c.try_pair(&q0, &q1, false);

    for _ in 0..(if best { 4 } else { 1 }) {
        let before = c.best_err;
        if c.best_err == 0 {
            break;
        }
        let Some((a, b)) = c.refit(false) else { break };
        q0 = a;
        q1 = b;
        c.try_pair(&q0, &q1, false);
        if c.best_err >= before {
            break;
        }
    }
    if best && c.best_err != 0 {
        c.local_search(false);
        if !bc3_colour {
            let four_err = c.best_err;
            let four_block = c.best;
            let a = c.best_q;
            c.best_err = u32::MAX;
            c.try_pair(&a[0], &a[1], true);
            for _ in 0..3 {
                let before = c.best_err;
                if c.best_err == 0 {
                    break;
                }
                let Some((x, y)) = c.refit(true) else { break };
                q0 = x;
                q1 = y;
                c.try_pair(&q0, &q1, true);
                if c.best_err >= before {
                    break;
                }
            }
            if c.best_err != 0 && c.best_three {
                c.local_search(true);
            }
            if c.best_err >= four_err {
                c.best = four_block;
            }
        }
    }
    c.best
}

// ---------------------------------------------------------------------------------------------
// BC4
// ---------------------------------------------------------------------------------------------

struct Bc4Best<E> {
    err: E,
    a0: i32,
    a1: i32,
    bits: u64,
}

fn bc4_eval(v: &[u8; 16], a0: i32, a1: i32, limit: u32) -> (u32, u64) {
    let mut pal = [0i32; 8];
    pal[0] = a0;
    pal[1] = a1;
    if a0 > a1 {
        for i in 1..=6 {
            pal[1 + i] = ((7 - i as i32) * a0 + i as i32 * a1) / 7;
        }
    } else {
        for i in 1..=4 {
            pal[1 + i] = ((5 - i as i32) * a0 + i as i32 * a1) / 5;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let (mut err, mut idx) = (0u32, 0u64);
    for i in 0..16 {
        let (mut be, mut bi) = (1i32 << 30, 0u64);
        for (j, p) in pal.iter().enumerate() {
            let d = i32::from(v[i]) - p;
            let d = d * d;
            if d < be {
                be = d;
                bi = j as u64;
            }
        }
        err = err.wrapping_add(be as u32);
        idx |= bi << (3 * i);
        if err > limit {
            return (err, 0);
        }
    }
    (err, idx)
}

fn bc4_try(v: &[u8; 16], a0: i32, a1: i32, b: &mut Bc4Best<u32>) {
    if !(0..=255).contains(&a0) || !(0..=255).contains(&a1) {
        return;
    }
    let (e, bits) = bc4_eval(v, a0, a1, b.err);
    if e < b.err {
        *b = Bc4Best { err: e, a0, a1, bits };
    }
}

fn bc4_bytes(a0: i32, a1: i32, bits: u64) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0] = a0 as u8;
    out[1] = a1 as u8;
    out[2..8].copy_from_slice(&bits.to_le_bytes()[0..6]);
    out
}

/// One BC4 block from 16 byte values.
pub fn bc4_encode(v: &[u8; 16], best: bool) -> [u8; 8] {
    let mn = i32::from(*v.iter().min().unwrap_or(&0));
    let mx = i32::from(*v.iter().max().unwrap_or(&0));
    let mut b = Bc4Best { err: u32::MAX, a0: 0, a1: 0, bits: 0 };
    if mn == mx {
        b.a0 = mn;
        b.a1 = mn;
    } else {
        bc4_try(v, mx, mn, &mut b);
        if best && b.err != 0 {
            for d0 in 0..=2 {
                for d1 in 0..=2 {
                    if mx - d0 > mn + d1 {
                        bc4_try(v, mx - d0, mn + d1, &mut b);
                    }
                }
            }
            if b.a0 > b.a1 {
                let (mut aa, mut ab, mut bb, mut ax, mut bx) = (0i64, 0i64, 0i64, 0i64, 0i64);
                for i in 0..16 {
                    let s = ((b.bits >> (3 * i)) & 7) as i64;
                    let a = if s == 0 {
                        7
                    } else if s == 1 {
                        0
                    } else {
                        8 - s
                    };
                    let w = 7 - a;
                    aa += a * a;
                    ab += a * w;
                    bb += w * w;
                    ax += a * i64::from(v[i]);
                    bx += w * i64::from(v[i]);
                }
                let det = aa * bb - ab * ab;
                if det != 0 {
                    let r0 = ((7.0f32 * ((bb * ax - ab * bx) as f32)) / det as f32 + 0.5f32) as i32;
                    let r1 = ((7.0f32 * ((aa * bx - ab * ax) as f32)) / det as f32 + 0.5f32) as i32;
                    for d0 in -1..=1 {
                        for d1 in -1..=1 {
                            if r0 + d0 > r1 + d1 {
                                bc4_try(v, r0 + d0, r1 + d1, &mut b);
                            }
                        }
                    }
                }
            }
            if mn <= 8 || mx >= 247 {
                let (mut lo6, mut hi6) = (255i32, 0i32);
                for &x in v {
                    if x == 0 || x == 255 {
                        continue;
                    }
                    lo6 = lo6.min(i32::from(x));
                    hi6 = hi6.max(i32::from(x));
                }
                if lo6 > hi6 {
                    lo6 = 0;
                    hi6 = 0;
                }
                bc4_try(v, lo6, hi6, &mut b);
                if hi6 - lo6 >= 2 {
                    bc4_try(v, lo6 + 1, hi6 - 1, &mut b);
                }
            }
        }
    }
    bc4_bytes(b.a0, b.a1, b.bits)
}

/// Largest fractional target (255 in units of 1/16).
const FINE_MAX: i32 = 4080;

fn bc4f_eval(v: &[u16; 16], a0: i32, a1: i32, limit: u64) -> (u64, u64) {
    let mut pal = [0i64; 8];
    pal[0] = i64::from(a0) * 560;
    pal[1] = i64::from(a1) * 560;
    if a0 > a1 {
        for i in 1..=6i64 {
            pal[1 + i as usize] = ((7 - i) * i64::from(a0) + i * i64::from(a1)) * 80;
        }
    } else {
        for i in 1..=4i64 {
            pal[1 + i as usize] = ((5 - i) * i64::from(a0) + i * i64::from(a1)) * 112;
        }
        pal[6] = 0;
        pal[7] = 255 * 560;
    }
    let (mut err, mut idx) = (0u64, 0u64);
    for i in 0..16 {
        let t = i64::from(v[i]) * 35;
        let (mut be, mut bi) = (u64::MAX, 0u64);
        for (j, p) in pal.iter().enumerate() {
            let d = t - p;
            let dd = (d * d) as u64;
            if dd < be {
                be = dd;
                bi = j as u64;
            }
        }
        err = err.wrapping_add(be);
        idx |= bi << (3 * i);
        if err > limit {
            return (err, 0);
        }
    }
    (err, idx)
}

fn bc4f_try(v: &[u16; 16], a0: i32, a1: i32, b: &mut Bc4Best<u64>) {
    if !(0..=255).contains(&a0) || !(0..=255).contains(&a1) {
        return;
    }
    let (e, bits) = bc4f_eval(v, a0, a1, b.err);
    if e < b.err {
        *b = Bc4Best { err: e, a0, a1, bits };
    }
}

/// BC4 for fractional targets in units of 1/16 (0..=4080), error against the exact palette
/// (HQ Y / Co / Cg, DXVKit 1.0's exact YCoCg).
pub fn bc4_encode_fine(v: &[u16; 16], best: bool) -> [u8; 8] {
    let mn = i32::from(*v.iter().min().unwrap_or(&0)).min(FINE_MAX);
    let mx = i32::from(*v.iter().max().unwrap_or(&0)).max(0);
    let (lo, hi) = (mn / 16, (mx + 15) / 16);
    let mut b = Bc4Best { err: u64::MAX, a0: 0, a1: 0, bits: 0 };
    if lo == hi {
        b.a0 = lo;
        b.a1 = lo;
    } else {
        bc4f_try(v, hi, lo, &mut b);
        if best && b.err != 0 {
            for d0 in 0..=2 {
                for d1 in 0..=2 {
                    if hi - d0 > lo + d1 {
                        bc4f_try(v, hi - d0, lo + d1, &mut b);
                    }
                }
            }
            if b.a0 > b.a1 {
                let (mut aa, mut ab, mut bb, mut ax, mut bx) = (0f64, 0f64, 0f64, 0f64, 0f64);
                for i in 0..16 {
                    let s = ((b.bits >> (3 * i)) & 7) as i32;
                    let a = if s == 0 {
                        7
                    } else if s == 1 {
                        0
                    } else {
                        8 - s
                    };
                    let w = 7 - a;
                    let t = f64::from(v[i]) / 16.0;
                    aa += f64::from(a * a);
                    ab += f64::from(a * w);
                    bb += f64::from(w * w);
                    // Fused in Swift GlEnc's DXVKit build (dxvk_bc4_encode_fine).
                    ax = f64::from(a).mul_add(t, ax);
                    bx = f64::from(w).mul_add(t, bx);
                }
                // `x*y - u*v` as clang contracts it: fma(x, y, -(u*v)).
                let det = aa.mul_add(bb, -(ab * ab));
                if det != 0.0 {
                    let r0 = (7.0 * bb.mul_add(ax, -(ab * bx)) / det + 0.5).floor() as i32;
                    let r1 = (7.0 * aa.mul_add(bx, -(ab * ax)) / det + 0.5).floor() as i32;
                    for d0 in -1..=1 {
                        for d1 in -1..=1 {
                            if r0 + d0 > r1 + d1 {
                                bc4f_try(v, r0 + d0, r1 + d1, &mut b);
                            }
                        }
                    }
                }
            }
            if mn <= 8 * 16 || mx >= 247 * 16 {
                let (mut lo6, mut hi6) = (FINE_MAX, 0i32);
                for &x in v {
                    let x = i32::from(x);
                    if x == 0 || x == FINE_MAX {
                        continue;
                    }
                    lo6 = lo6.min(x);
                    hi6 = hi6.max(x);
                }
                if lo6 > hi6 {
                    lo6 = 0;
                    hi6 = 0;
                }
                let (lo6, hi6) = (lo6 / 16, (hi6 + 15) / 16);
                bc4f_try(v, lo6, hi6, &mut b);
                if hi6 - lo6 >= 2 {
                    bc4f_try(v, lo6 + 1, hi6 - 1, &mut b);
                }
            }
        }
    }
    bc4_bytes(b.a0, b.a1, b.bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_match_the_generator_rules() {
        let t = tables();
        // value 255 is exactly representable: (31, 31) four-colour, error 0
        assert_eq!(t.four5[255], [31, 31]);
        assert_eq!(t.quant5[255], 31);
        assert_eq!(t.quant6[128], 32);
        // DXVKit's generated table, first entries: {0,0},{0,0},{0,1},{0,1},{1,0}
        assert_eq!(&t.four5[0..5], &[[0, 0], [0, 0], [0, 1], [0, 1], [1, 0]]);
        assert_eq!(t.four5[11], [0, 4]);
    }

    #[test]
    fn a_solid_block_is_exact_where_representable() {
        let px = [[255u8, 255, 255, 255]; 16];
        let b = bc1_encode(&px, false, false);
        assert_eq!(&b[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        let a = bc4_encode(&[77u8; 16], true);
        assert_eq!((a[0], a[1]), (77, 77));
    }
}
