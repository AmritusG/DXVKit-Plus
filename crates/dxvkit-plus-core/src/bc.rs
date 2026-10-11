//! CPU BCn block decoding — the CPU oracle for the GPU path (BC1 / BC3 / BC4 per the public S3TC and
//! RGTC definitions). The integer conventions are DXVKit's (`src/pixels.c`, spec 8.1–8.3; DXVKit
//! 1.0.0, MIT, Copyright (c) 2026 Amrit Rosell, `LICENSES/DXVKit-MIT.txt`): bit-replicated 565 expansion, truncating interpolation, BC3 colour always
//! four-colour. A GPU decoder may differ by 1 in some values (spec 8: "a GPU or another CPU decoder
//! may differ by 1"); comparisons against the GPU use a 1/255 tolerance where they do.
//
// Lint note: every integer operand in this module is a byte (<= 255) or a palette weight (<= 7), so
// no expression can exceed 255 * 35 * 2 and overflow is impossible by type; the arithmetic lint is
// relaxed for this module only. Dynamic indices still go through `get`.
#![allow(clippy::arithmetic_side_effects)]

use crate::bytes::{le16, le24, le32};

/// 16 RGBA pixels of a BC1 colour block (row-major 4x4). `four_colour` forces the four-colour
/// palette (the BC3 colour half). A short block decodes as transparent black.
pub fn bc1_block(blk: &[u8], four_colour: bool) -> [[u8; 4]; 16] {
    let (Some(c0), Some(c1), Some(mut idx)) = (le16(blk, 0), le16(blk, 2), le32(blk, 4)) else { return [[0; 4]; 16] };
    let expand = |c: u32| -> [u32; 3] {
        let (r, g, b) = (c >> 11, (c >> 5) & 63, c & 31);
        [(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]
    };
    let (p0, p1) = (expand(c0), expand(c1));
    let mut pal = [[0u8, 0, 0, 255]; 4];
    for (k, (a, b)) in p0.into_iter().zip(p1).enumerate().take(3) {
        let (e2, e3) = if four_colour || c0 > c1 { ((2 * a + b) / 3, (a + 2 * b) / 3) } else { ((a + b) / 2, 0) };
        for (entry, v) in pal.iter_mut().zip([a, b, e2, e3]) {
            if let Some(ch) = entry.get_mut(k) {
                *ch = v as u8;
            }
        }
    }
    let mut out = [[0u8; 4]; 16];
    for px in &mut out {
        *px = pal.get((idx & 3) as usize).copied().unwrap_or_default();
        idx >>= 2;
    }
    out
}

fn bc4_parts(blk: &[u8]) -> Option<(u32, u32, u64)> {
    let a0 = u32::from(*blk.first()?);
    let a1 = u32::from(*blk.get(1)?);
    let idx = u64::from(le24(blk, 2)?) | (u64::from(le24(blk, 5)?) << 24);
    Some((a0, a1, idx))
}

fn bc4_with<T: Copy + Default>(blk: &[u8], pal_of: impl Fn(u32, u32) -> [T; 8]) -> [T; 16] {
    let Some((a0, a1, mut idx)) = bc4_parts(blk) else { return [T::default(); 16] };
    let pal = pal_of(a0, a1);
    let mut out = [T::default(); 16];
    for v in &mut out {
        *v = pal.get((idx & 7) as usize).copied().unwrap_or_default();
        idx >>= 3;
    }
    out
}

/// 16 values of a BC4 block (also the BC3 alpha half), integer palette.
pub fn bc4_block(blk: &[u8]) -> [u8; 16] {
    bc4_with(blk, |a0, a1| {
        if a0 > a1 {
            let w = |i: u32| (((7 - i) * a0 + i * a1) / 7) as u8;
            [a0 as u8, a1 as u8, w(1), w(2), w(3), w(4), w(5), w(6)]
        } else {
            let w = |i: u32| (((5 - i) * a0 + i * a1) / 5) as u8;
            [a0 as u8, a1 as u8, w(1), w(2), w(3), w(4), 0, 255]
        }
    })
}

/// 16 values of a BC4 block with the exact (untruncated) palette, in units of 1/35: eight-value
/// entries are sevenths, six-value entries fifths, so every palette value is a whole number of 35ths.
pub fn bc4_block_exact(blk: &[u8]) -> [u16; 16] {
    bc4_with(blk, |a0, a1| {
        if a0 > a1 {
            let w = |i: u32| (((7 - i) * a0 + i * a1) * 5) as u16;
            [(a0 * 35) as u16, (a1 * 35) as u16, w(1), w(2), w(3), w(4), w(5), w(6)]
        } else {
            let w = |i: u32| (((5 - i) * a0 + i * a1) * 7) as u16;
            [(a0 * 35) as u16, (a1 * 35) as u16, w(1), w(2), w(3), w(4), 0, 255 * 35]
        }
    })
}

/// 16 RGBA pixels of a BC3 block with the exact (unrounded) palettes, normalised to 0..1: colour
/// entries are exact thirds of the expanded endpoints (always four-colour in BC3), alpha entries
/// exact sevenths / fifths. The HAP Q oracle uses this so the YCoCg transform sees the same values
/// a float GPU decoder does (integer truncation would compound across three channels).
pub fn bc3_block_exact(blk: &[u8]) -> [[f64; 4]; 16] {
    let mut out = [[0f64; 4]; 16];
    let alpha = bc4_block_exact(blk);
    let c = blk.get(8..).unwrap_or_default();
    let (Some(c0), Some(c1), Some(mut idx)) = (le16(c, 0), le16(c, 2), le32(c, 4)) else { return out };
    let expand = |c: u32| -> [f64; 3] {
        let (r, g, b) = (c >> 11, (c >> 5) & 63, c & 31);
        [f64::from((r << 3) | (r >> 2)), f64::from((g << 2) | (g >> 4)), f64::from((b << 3) | (b >> 2))]
    };
    let (p0, p1) = (expand(c0), expand(c1));
    for (i, px) in out.iter_mut().enumerate() {
        let w = f64::from(idx & 3);
        idx >>= 2;
        // index 0 = c0, 1 = c1, 2 = (2 c0 + c1) / 3, 3 = (c0 + 2 c1) / 3
        let t = match w as u32 {
            0 => 0.0,
            1 => 1.0,
            2 => 1.0 / 3.0,
            _ => 2.0 / 3.0,
        };
        for k in 0..3 {
            if let (Some(d), Some(a), Some(b)) = (px.get_mut(k), p0.get(k), p1.get(k)) {
                *d = (a + (b - a) * t) / 255.0;
            }
        }
        px[3] = f64::from(alpha.get(i).copied().unwrap_or(0)) / (255.0 * 35.0);
    }
    out
}

/// Straight -> premultiplied, rounded (DXVKit `premul`).
#[inline]
pub fn premul(c: u8, a: u8) -> u8 {
    ((u32::from(c) * u32::from(a) + 127) / 255) as u8
}

/// Premultiplied -> straight, rounded to nearest and clamped: `min(255, round(c * 255 / a))`; a
/// transparent texel (`a = 0`) keeps its colour. The GPU conversion does the same integer
/// arithmetic, so the two agree exactly.
pub fn unpremul(c: u8, a: u8) -> u8 {
    if a == 0 {
        return c;
    }
    let (c, a) = (u32::from(c), u32::from(a));
    u8::try_from((c * 255 + a / 2) / a).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bc1_palettes() {
        // c0 = pure red 565 (0xF800) > c1 = black: four-colour.
        let blk = [0x00, 0xF8, 0x00, 0x00, 0b1110_0100, 0, 0, 0];
        let px = bc1_block(&blk, false);
        assert_eq!(px[0], [255, 0, 0, 255]);
        assert_eq!(px[1], [0, 0, 0, 255]);
        assert_eq!(px[2], [170, 0, 0, 255]);
        assert_eq!(px[3], [85, 0, 0, 255]);
        // Three-colour mode when c0 <= c1 (index 3 = black), unless forced to four-colour.
        let blk3 = [0x00, 0x00, 0x00, 0xF8, 0b1110_0100, 0, 0, 0];
        assert_eq!(bc1_block(&blk3, false)[2], [127, 0, 0, 255]);
        assert_eq!(bc1_block(&blk3, false)[3], [0, 0, 0, 255]);
        assert_eq!(bc1_block(&blk3, true)[2], [85, 0, 0, 255]);
        // Negative control: a short block is transparent black, not a panic.
        assert_eq!(bc1_block(&blk[..5], false)[0], [0, 0, 0, 0]);
    }

    #[test]
    fn bc4_palettes() {
        // indices 2, 1, 0, ...
        let blk = [200, 100, 0b0000_1010, 0, 0, 0, 0, 0];
        let v = bc4_block(&blk);
        assert_eq!(&v[..3], &[185, 100, 200]);
        let e = bc4_block_exact(&blk);
        assert_eq!((e[0], e[1], e[2]), ((6 * 200 + 100) * 5, 100 * 35, 200 * 35));
        // Six-value mode ends in 0 and 255.
        let six = [10, 20, 0b0011_1110, 0, 0, 0, 0, 0];
        let v6 = bc4_block(&six);
        assert_eq!((v6[0], v6[1]), (0, 255));
        assert_eq!(premul(255, 128), 128);
    }
}
