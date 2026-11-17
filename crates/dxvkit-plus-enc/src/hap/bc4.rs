//! BC4 single-channel block encoders — Swift GlEnc's `BC4AlphaBlockEncoder.swift` (the two-candidate
//! "simple" search, GlEnc's default) and `BC4AlphaBlockEncoderRefined.swift` (the 7×7 endpoint search behind
//! the session-only `BC4Config.useRefinement`; GlEnc's clean-room implementation after the ideas of Rich
//! Geldreich's rgbcx / bc7enc_rdo, MIT or public domain). Integer-only.

/// The 8-entry palette of an endpoint pair (`bc4Palette` / `bc4FillPaletteInt`).
#[inline(always)]
fn palette(a0: i32, a1: i32, eight_mode: bool) -> [i32; 8] {
    if eight_mode {
        [
            a0,
            a1,
            (6 * a0 + a1 + 3) / 7,
            (5 * a0 + 2 * a1 + 3) / 7,
            (4 * a0 + 3 * a1 + 3) / 7,
            (3 * a0 + 4 * a1 + 3) / 7,
            (2 * a0 + 5 * a1 + 3) / 7,
            (a0 + 6 * a1 + 3) / 7,
        ]
    } else {
        [a0, a1, (4 * a0 + a1 + 2) / 5, (3 * a0 + 2 * a1 + 2) / 5, (2 * a0 + 3 * a1 + 2) / 5, (a0 + 4 * a1 + 2) / 5, 0, 255]
    }
}

/// `bc4WriteBlock`: endpoints, then 16 × 3-bit indices LSB-first.
#[inline(always)]
fn write_block(a0: u8, a1: u8, indices: &[u8; 16]) -> [u8; 8] {
    let mut bits = 0u64;
    for (i, &k) in indices.iter().enumerate() {
        bits |= u64::from(k & 7) << (i * 3);
    }
    let b = bits.to_le_bytes();
    [a0, a1, b[0], b[1], b[2], b[3], b[4], b[5]]
}

/// `bc4ClosestIndex`: first index of least absolute error.
#[inline(always)]
fn closest(pal: &[i32; 8], v: u8) -> (u8, i32) {
    let v = i32::from(v);
    let (mut best_k, mut best_e) = (0u8, i32::MAX);
    for (k, &p) in pal.iter().enumerate() {
        let e = (v - p).abs();
        if e < best_e {
            best_e = e;
            best_k = k as u8;
        }
    }
    (best_k, best_e)
}

/// `encodeBC4BlockSimple`: 8-mode (max, min) against 6-mode (min, max of the values other than 0 and 255).
pub(crate) fn encode_simple(px: &[u8; 16]) -> [u8; 8] {
    let (mut min_a, mut max_a) = (255u8, 0u8);
    for &v in px {
        min_a = min_a.min(v);
        max_a = max_a.max(v);
    }
    if min_a == max_a {
        return write_block(min_a, min_a, &[0; 16]);
    }

    let pal8 = palette(i32::from(max_a), i32::from(min_a), true);
    let mut idx8 = [0u8; 16];
    let mut err8 = 0i32;
    for (i, &v) in px.iter().enumerate() {
        let (k, e) = closest(&pal8, v);
        idx8[i] = k;
        err8 += e;
    }

    let (mut min2, mut max2, mut has_mid) = (255u8, 0u8, false);
    for &v in px {
        if v != 0 && v != 255 {
            min2 = min2.min(v);
            max2 = max2.max(v);
            has_mid = true;
        }
    }
    let mut err6 = i32::MAX;
    let mut idx6 = [0u8; 16];
    let mut pal6 = [0i32; 8];
    if has_mid && min2 < max2 {
        pal6 = palette(i32::from(min2), i32::from(max2), false);
        err6 = 0;
        for (i, &v) in px.iter().enumerate() {
            let (k, e) = closest(&pal6, v);
            idx6[i] = k;
            err6 += e;
        }
    } else if !has_mid {
        // Only 0 and 255: exact in 6-mode through the reserved entries.
        pal6 = palette(0, 255, false);
        err6 = 0;
        for (i, &v) in px.iter().enumerate() {
            idx6[i] = if v == 0 { 6 } else { 7 };
        }
    }

    if err6 < err8 { write_block(pal6[0] as u8, pal6[1] as u8, &idx6) } else { write_block(pal8[0] as u8, pal8[1] as u8, &idx8) }
}

/// `encodeBC4BlockRefined`: both modes × endpoint deltas in −3…3 around (max, min), least squared error
/// with GlEnc's early-out, first best kept.
pub(crate) fn encode_refined(px: &[u8; 16]) -> [u8; 8] {
    let (mut min_v, mut max_v) = (255i32, 0i32);
    for &v in px {
        min_v = min_v.min(i32::from(v));
        max_v = max_v.max(i32::from(v));
    }
    if min_v == max_v {
        return write_block(min_v as u8, min_v as u8, &[0; 16]);
    }
    const RADIUS: i32 = 3;
    let mut best_err = i64::MAX;
    let (mut best_a0, mut best_a1) = (max_v, min_v);
    let mut best_idx = [0u8; 16];
    let mut trial = [0u8; 16];
    for mode in 0..2 {
        for lo in -RADIUS..=RADIUS {
            for hi in -RADIUS..=RADIUS {
                let mut a0 = (max_v + hi).clamp(0, 255);
                let mut a1 = (min_v + lo).clamp(0, 255);
                if a0 == a1 {
                    continue;
                }
                if (mode == 0 && a0 <= a1) || (mode == 1 && a0 > a1) {
                    std::mem::swap(&mut a0, &mut a1);
                }
                let pal = palette(a0, a1, mode == 0);
                let err = squared_error_early_out(px, &pal, &mut trial, best_err);
                if err < best_err {
                    best_err = err;
                    best_a0 = a0;
                    best_a1 = a1;
                    best_idx = trial;
                }
            }
        }
    }
    write_block(best_a0 as u8, best_a1 as u8, &best_idx)
}

/// `bc4SquaredErrorEarlyOut`: per pixel the first index of least squared error (stopping at an exact hit);
/// returns as soon as the running total reaches `cutoff`.
#[inline(always)]
fn squared_error_early_out(px: &[u8; 16], pal: &[i32; 8], out: &mut [u8; 16], cutoff: i64) -> i64 {
    let mut total = 0i64;
    for (i, &v) in px.iter().enumerate() {
        let v = i32::from(v);
        let (mut best_e, mut best_k) = (i32::MAX, 0u8);
        for (k, &p) in pal.iter().enumerate() {
            let d = v - p;
            let e = d * d;
            if e < best_e {
                best_e = e;
                best_k = k as u8;
                if e == 0 {
                    break;
                }
            }
        }
        out[i] = best_k;
        total += i64::from(best_e);
        if total >= cutoff {
            return total;
        }
    }
    total
}
