//! BC1 (DXT1) block encoder — Swift GlEnc's `BC1BlockEncoder.swift`, which is its line-by-line port of FFmpeg
//! `libavcodec/texturedspenc.c` (`compress_color`: `constant_color`, `optimize_colors`, `match_colors`,
//! `refine_colors`; MIT, Copyright (C) 2015 Vittorio Giovara, based on public-domain code by Fabian Giesen,
//! Sean Barrett and Yann Collet).
//!
//! Float semantics are GlEnc's, not C's: every `addingProduct` in the Swift code is a fused multiply-add here
//! ([`f32::mul_add`]) and every other product / sum is rounded on its own, in the same order (Swift does not
//! contract `a * b + c`). The `Int` arithmetic of the Swift code never leaves `i32` range for 8-bit pixels
//! (largest value: a covariance sum, 16 × 255² ≈ 1.04 M).

/// One 4×4 tile: 16 RGBA pixels, row-major (`y * 4 + x`). Alpha takes part only in the constant-colour test,
/// exactly like the Swift code's 32-bit compare.
pub(crate) type Tile = [[u8; 4]; 16];

#[rustfmt::skip]
const EXPAND5: [u8; 32] = [
    0, 8, 16, 24, 33, 41, 49, 57, 66, 74, 82, 90, 99, 107, 115, 123, 132, 140, 148, 156, 165, 173, 181, 189,
    198, 206, 214, 222, 231, 239, 247, 255,
];

#[rustfmt::skip]
const EXPAND6: [u8; 64] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 65, 69, 73, 77, 81, 85, 89, 93,
    97, 101, 105, 109, 113, 117, 121, 125, 130, 134, 138, 142, 146, 150, 154, 158, 162, 166, 170, 174, 178, 182, 186, 190,
    195, 199, 203, 207, 211, 215, 219, 223, 227, 231, 235, 239, 243, 247, 251, 255,
];

#[rustfmt::skip]
const MATCH5: [u8; 512] = [
    0, 0, 0, 0, 0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1, 2, 0, 2, 0, 0, 4,
    2, 1, 2, 1, 2, 1, 3, 0, 3, 0, 3, 0, 3, 1, 1, 5, 3, 2, 3, 2, 4, 0, 4, 0,
    4, 1, 4, 1, 4, 2, 4, 2, 4, 2, 3, 5, 5, 1, 5, 1, 5, 2, 4, 4, 5, 3, 5, 3,
    5, 3, 6, 2, 6, 2, 6, 2, 6, 3, 5, 5, 6, 4, 6, 4, 4, 8, 7, 3, 7, 3, 7, 3,
    7, 4, 7, 4, 7, 4, 7, 5, 5, 9, 7, 6, 7, 6, 8, 4, 8, 4, 8, 5, 8, 5, 8, 6,
    8, 6, 8, 6, 7, 9, 9, 5, 9, 5, 9, 6, 8, 8, 9, 7, 9, 7, 9, 7, 10, 6, 10, 6,
    10, 6, 10, 7, 9, 9, 10, 8, 10, 8, 8, 12, 11, 7, 11, 7, 11, 7, 11, 8, 11, 8, 11, 8,
    11, 9, 9, 13, 11, 10, 11, 10, 12, 8, 12, 8, 12, 9, 12, 9, 12, 10, 12, 10, 12, 10, 11, 13,
    13, 9, 13, 9, 13, 10, 12, 12, 13, 11, 13, 11, 13, 11, 14, 10, 14, 10, 14, 10, 14, 11, 13, 13,
    14, 12, 14, 12, 12, 16, 15, 11, 15, 11, 15, 11, 15, 12, 15, 12, 15, 12, 15, 13, 13, 17, 15, 14,
    15, 14, 16, 12, 16, 12, 16, 13, 16, 13, 16, 14, 16, 14, 16, 14, 15, 17, 17, 13, 17, 13, 17, 14,
    16, 16, 17, 15, 17, 15, 17, 15, 18, 14, 18, 14, 18, 14, 18, 15, 17, 17, 18, 16, 18, 16, 16, 20,
    19, 15, 19, 15, 19, 15, 19, 16, 19, 16, 19, 16, 19, 17, 17, 21, 19, 18, 19, 18, 20, 16, 20, 16,
    20, 17, 20, 17, 20, 18, 20, 18, 20, 18, 19, 21, 21, 17, 21, 17, 21, 18, 20, 20, 21, 19, 21, 19,
    21, 19, 22, 18, 22, 18, 22, 18, 22, 19, 21, 21, 22, 20, 22, 20, 20, 24, 23, 19, 23, 19, 23, 19,
    23, 20, 23, 20, 23, 20, 23, 21, 21, 25, 23, 22, 23, 22, 24, 20, 24, 20, 24, 21, 24, 21, 24, 22,
    24, 22, 24, 22, 23, 25, 25, 21, 25, 21, 25, 22, 24, 24, 25, 23, 25, 23, 25, 23, 26, 22, 26, 22,
    26, 22, 26, 23, 25, 25, 26, 24, 26, 24, 24, 28, 27, 23, 27, 23, 27, 23, 27, 24, 27, 24, 27, 24,
    27, 25, 25, 29, 27, 26, 27, 26, 28, 24, 28, 24, 28, 25, 28, 25, 28, 26, 28, 26, 28, 26, 27, 29,
    29, 25, 29, 25, 29, 26, 28, 28, 29, 27, 29, 27, 29, 27, 30, 26, 30, 26, 30, 26, 30, 27, 29, 29,
    30, 28, 30, 28, 30, 28, 31, 27, 31, 27, 31, 27, 31, 28, 31, 28, 31, 28, 31, 29, 31, 29, 31, 30,
    31, 30, 31, 30, 31, 31, 31, 31,
];

#[rustfmt::skip]
const MATCH6: [u8; 512] = [
    0, 0, 0, 1, 1, 0, 1, 0, 1, 1, 2, 0, 2, 1, 3, 0, 3, 0, 3, 1, 4, 0, 4, 0,
    4, 1, 5, 0, 5, 1, 6, 0, 6, 0, 6, 1, 7, 0, 7, 0, 7, 1, 8, 0, 8, 1, 8, 1,
    8, 2, 9, 1, 9, 2, 9, 2, 9, 3, 10, 2, 10, 3, 10, 3, 10, 4, 11, 3, 11, 4, 11, 4,
    11, 5, 12, 4, 12, 5, 12, 5, 12, 6, 13, 5, 13, 6, 8, 16, 13, 7, 14, 6, 14, 7, 9, 17,
    14, 8, 15, 7, 15, 8, 11, 16, 15, 9, 15, 10, 16, 8, 16, 9, 16, 10, 15, 13, 17, 9, 17, 10,
    17, 11, 15, 16, 18, 10, 18, 11, 18, 12, 16, 16, 19, 11, 19, 12, 19, 13, 17, 17, 20, 12, 20, 13,
    20, 14, 19, 16, 21, 13, 21, 14, 21, 15, 20, 17, 22, 14, 22, 15, 25, 10, 22, 16, 23, 15, 23, 16,
    26, 11, 23, 17, 24, 16, 24, 17, 27, 12, 24, 18, 25, 17, 25, 18, 28, 13, 25, 19, 26, 18, 26, 19,
    29, 14, 26, 20, 27, 19, 27, 20, 30, 15, 27, 21, 28, 20, 28, 21, 28, 21, 28, 22, 29, 21, 29, 22,
    24, 32, 29, 23, 30, 22, 30, 23, 25, 33, 30, 24, 31, 23, 31, 24, 27, 32, 31, 25, 31, 26, 32, 24,
    32, 25, 32, 26, 31, 29, 33, 25, 33, 26, 33, 27, 31, 32, 34, 26, 34, 27, 34, 28, 32, 32, 35, 27,
    35, 28, 35, 29, 33, 33, 36, 28, 36, 29, 36, 30, 35, 32, 37, 29, 37, 30, 37, 31, 36, 33, 38, 30,
    38, 31, 41, 26, 38, 32, 39, 31, 39, 32, 42, 27, 39, 33, 40, 32, 40, 33, 43, 28, 40, 34, 41, 33,
    41, 34, 44, 29, 41, 35, 42, 34, 42, 35, 45, 30, 42, 36, 43, 35, 43, 36, 46, 31, 43, 37, 44, 36,
    44, 37, 44, 37, 44, 38, 45, 37, 45, 38, 40, 48, 45, 39, 46, 38, 46, 39, 41, 49, 46, 40, 47, 39,
    47, 40, 43, 48, 47, 41, 47, 42, 48, 40, 48, 41, 48, 42, 47, 45, 49, 41, 49, 42, 49, 43, 47, 48,
    50, 42, 50, 43, 50, 44, 48, 48, 51, 43, 51, 44, 51, 45, 49, 49, 52, 44, 52, 45, 52, 46, 51, 48,
    53, 45, 53, 46, 53, 47, 52, 49, 54, 46, 54, 47, 57, 42, 54, 48, 55, 47, 55, 48, 58, 43, 55, 49,
    56, 48, 56, 49, 59, 44, 56, 50, 57, 49, 57, 50, 60, 45, 57, 51, 58, 50, 58, 51, 61, 46, 58, 52,
    59, 51, 59, 52, 62, 47, 59, 53, 60, 52, 60, 53, 60, 53, 60, 54, 61, 53, 61, 54, 61, 54, 61, 55,
    62, 54, 62, 55, 62, 55, 62, 56, 63, 55, 63, 56, 63, 56, 63, 57, 63, 58, 63, 59, 63, 59, 63, 60,
    63, 61, 63, 62, 63, 62, 63, 63,
];

/// `mul8(a, b)`: 8-bit fixed-point multiply (`bc1Mul8`).
#[inline(always)]
fn mul8(a: i32, b: i32) -> i32 {
    let t = a * b + 128;
    (t + (t >> 8)) >> 8
}

/// RGB888 → RGB565 (`bc1Rgb2Rgb565`).
#[inline(always)]
fn rgb565(p: [u8; 4]) -> u16 {
    ((mul8(i32::from(p[0]), 31) << 11) | (mul8(i32::from(p[1]), 63) << 5) | mul8(i32::from(p[2]), 31)) as u16
}

/// RGB565 → RGB888 through the expand tables (`bc1Rgb5652rgb`).
#[inline(always)]
fn rgb565_to_rgb(v: u16) -> [i32; 3] {
    [i32::from(EXPAND5[usize::from((v & 0xf800) >> 11)]), i32::from(EXPAND6[usize::from((v & 0x07e0) >> 5)]), i32::from(EXPAND5[usize::from(v & 0x001f)])]
}

/// `(2a + b) / 3` per channel (`bc1Lerp13rgb`).
#[inline(always)]
fn lerp13rgb(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [(2 * a[0] + b[0]) / 3, (2 * a[1] + b[1]) / 3, (2 * a[2] + b[2]) / 3]
}

/// The constant-colour endpoints of an 8-bit (r, g, b) through the match tables.
#[inline(always)]
fn match_endpoints(r: usize, g: usize, b: usize) -> (u16, u16) {
    let max16 = (u16::from(MATCH5[r * 2]) << 11) | (u16::from(MATCH6[g * 2]) << 5) | u16::from(MATCH5[b * 2]);
    let min16 = (u16::from(MATCH5[r * 2 + 1]) << 11) | (u16::from(MATCH6[g * 2 + 1]) << 5) | u16::from(MATCH5[b * 2 + 1]);
    (max16, min16)
}

#[inline(always)]
fn dot(p: [u8; 4], v: [i32; 3]) -> i32 {
    i32::from(p[0]) * v[0] + i32::from(p[1]) * v[1] + i32::from(p[2]) * v[2]
}

/// `match_colors`: 2-bit indices along the endpoints' axis (`bc1MatchColors`).
fn match_colors(px: &Tile, c0: u16, c1: u16) -> u32 {
    const INDEX_MAP: [u32; 8] = [0 << 30, 2 << 30, 0 << 30, 2 << 30, 3 << 30, 3 << 30, 1 << 30, 1 << 30];
    let p0 = rgb565_to_rgb(c0);
    let p1 = rgb565_to_rgb(c1);
    let color = [p0, p1, lerp13rgb(p0, p1), lerp13rgb(p1, p0)];
    let dir = [p0[0] - p1[0], p0[1] - p1[1], p0[2] - p1[2]];
    let stops = color.map(|c| c[0] * dir[0] + c[1] * dir[1] + c[2] * dir[2]);
    let c0_point = (stops[1] + stops[3]) >> 1;
    let half_point = (stops[3] + stops[2]) >> 1;
    let c3_point = (stops[2] + stops[0]) >> 1;
    let mut mask = 0u32;
    for &p in px {
        let d = dot(p, dir);
        let bits = (if d < half_point { 4 } else { 0 }) | (if d < c0_point { 2 } else { 0 }) | usize::from(d < c3_point);
        mask >>= 2;
        mask |= INDEX_MAP[bits];
    }
    mask
}

/// `optimize_colors`: principal axis by power iteration, endpoints at the extreme projections
/// (`bc1OptimizeColors`). Returns `(max16, min16)`.
fn optimize_colors(px: &Tile) -> (u16, u16) {
    let mut mu = [0i32; 3];
    let mut minc = [0i32; 3];
    let mut maxc = [0i32; 3];
    for ch in 0..3 {
        // FFmpeg's quirk, kept: the first pixel seeds the sum and is counted again in the loop (17 terms, / 16).
        let first = i32::from(px[0][ch]);
        let (mut muv, mut minv, mut maxv) = (first, first, first);
        for p in px {
            let v = i32::from(p[ch]);
            muv += v;
            if v < minv {
                minv = v;
            } else if v > maxv {
                maxv = v;
            }
        }
        mu[ch] = (muv + 8) >> 4;
        minc[ch] = minv;
        maxc[ch] = maxv;
    }

    let mut cov = [0i32; 6];
    for p in px {
        let r = i32::from(p[0]) - mu[0];
        let g = i32::from(p[1]) - mu[1];
        let b = i32::from(p[2]) - mu[2];
        cov[0] += r * r;
        cov[1] += r * g;
        cov[2] += r * b;
        cov[3] += g * g;
        cov[4] += g * b;
        cov[5] += b * b;
    }
    let covf = cov.map(|c| c as f32 / 255.0f32);

    let mut vfr = (maxc[0] - minc[0]) as f32;
    let mut vfg = (maxc[1] - minc[1]) as f32;
    let mut vfb = (maxc[2] - minc[2]) as f32;
    for _ in 0..4 {
        // Swift: `vfg * covf[i]` alone, then `.addingProduct(vfr, …)`, then `.addingProduct(vfb, …)`.
        let r = vfb.mul_add(covf[2], vfr.mul_add(covf[0], vfg * covf[1]));
        let g = vfb.mul_add(covf[4], vfr.mul_add(covf[1], vfg * covf[3]));
        let b = vfb.mul_add(covf[5], vfr.mul_add(covf[2], vfg * covf[4]));
        vfr = r;
        vfg = g;
        vfb = b;
    }

    let mut magn = f64::from(vfr.abs());
    if f64::from(vfg.abs()) > magn {
        magn = f64::from(vfg.abs());
    }
    if f64::from(vfb.abs()) > magn {
        magn = f64::from(vfb.abs());
    }
    let v = if magn < 4.0 {
        [299, 587, 114]
    } else {
        magn = 512.0 / magn;
        [(f64::from(vfr) * magn) as i32, (f64::from(vfg) * magn) as i32, (f64::from(vfb) * magn) as i32]
    };

    let mut mind = dot(px[0], v);
    let mut maxd = mind;
    let (mut minp, mut maxp) = (0usize, 0usize);
    for (i, &p) in px.iter().enumerate() {
        let d = dot(p, v);
        if d < mind {
            mind = d;
            minp = i;
        } else if d > maxd {
            maxd = d;
            maxp = i;
        }
    }
    (rgb565(px[maxp]), rgb565(px[minp]))
}

#[inline(always)]
fn clip5(a: i32) -> i32 {
    a.clamp(0, 31)
}

#[inline(always)]
fn clip6(a: i32) -> i32 {
    a.clamp(0, 63)
}

/// `refine_colors`: least-squares endpoints for the current indices (`bc1RefineColors`). Returns whether
/// the endpoints changed.
fn refine_colors(px: &Tile, max16: &mut u16, min16: &mut u16, mask: u32) -> bool {
    const W1TAB: [i32; 4] = [3, 0, 2, 1];
    const PRODS: [i32; 4] = [0x090000, 0x000900, 0x040102, 0x010402];
    let (old_min, old_max) = (*min16, *max16);
    let (new_max, new_min);
    if (mask ^ (mask << 2)) < 4 {
        // Every pixel has the same index: match the average colour.
        let (mut r, mut g, mut b) = (8usize, 8usize, 8usize);
        for p in px {
            r += usize::from(p[0]);
            g += usize::from(p[1]);
            b += usize::from(p[2]);
        }
        (new_max, new_min) = match_endpoints(r >> 4, g >> 4, b >> 4);
    } else {
        let mut cm = mask;
        let (mut at1, mut at2) = ([0i32; 3], [0i32; 3]);
        let mut akku = 0i32;
        for p in px {
            let step = (cm & 3) as usize;
            let w1 = W1TAB[step];
            akku += PRODS[step];
            for ch in 0..3 {
                at1[ch] += w1 * i32::from(p[ch]);
                at2[ch] += i32::from(p[ch]);
            }
            cm >>= 2;
        }
        for ch in 0..3 {
            at2[ch] = 3 * at2[ch] - at1[ch];
        }
        let xx = (akku >> 16) & 0xFF;
        let yy = (akku >> 8) & 0xFF;
        let xy = akku & 0xFF;

        let denom = (xx * yy - xy * xy) as f32;
        let fr1 = 3.0f32 * 31.0f32;
        let fr2 = fr1 / 255.0f32;
        let fr = fr2 / denom;
        let fg1 = fr * 63.0f32;
        let fg = fg1 / 31.0f32;
        let fb = fr;
        let f = [fr, fg, fb];
        // Swift: `Float(0.5).addingProduct(Float(n), f)`, truncated toward zero, then clipped.
        let solve = |n: i32, ch: usize| ((n as f32).mul_add(f[ch], 0.5f32)) as i32;
        let mr = clip5(solve(at1[0] * yy - at2[0] * xy, 0));
        let mg = clip6(solve(at1[1] * yy - at2[1] * xy, 1));
        let mb = clip5(solve(at1[2] * yy - at2[2] * xy, 2));
        new_max = ((mr << 11) | (mg << 5) | mb) as u16;
        let nr = clip5(solve(at2[0] * xx - at1[0] * xy, 0));
        let ng = clip6(solve(at2[1] * xx - at1[1] * xy, 1));
        let nb = clip5(solve(at2[2] * xx - at1[2] * xy, 2));
        new_min = ((nr << 11) | (ng << 5) | nb) as u16;
    }
    *min16 = new_min;
    *max16 = new_max;
    old_min != new_min || old_max != new_max
}

/// `encodeBC1BlockFFmpeg`: one 4×4 tile → 8 bytes `[max16 LE][min16 LE][indices LE32]`.
pub(crate) fn encode(px: &Tile) -> [u8; 8] {
    let (mut max16, mut min16, mut mask);
    if px.iter().all(|p| *p == px[0]) {
        mask = 0xAAAA_AAAAu32;
        (max16, min16) = match_endpoints(usize::from(px[0][0]), usize::from(px[0][1]), usize::from(px[0][2]));
    } else {
        (max16, min16) = optimize_colors(px);
        mask = if max16 != min16 { match_colors(px, max16, min16) } else { 0 };
        if refine_colors(px, &mut max16, &mut min16, mask) {
            mask = if max16 != min16 { match_colors(px, max16, min16) } else { 0 };
        }
    }
    if max16 < min16 {
        std::mem::swap(&mut max16, &mut min16);
        mask ^= 0x5555_5555;
    }
    let [a, b] = max16.to_le_bytes();
    let [c, d] = min16.to_le_bytes();
    let [e, f, g, h] = mask.to_le_bytes();
    [a, b, c, d, e, f, g, h]
}
