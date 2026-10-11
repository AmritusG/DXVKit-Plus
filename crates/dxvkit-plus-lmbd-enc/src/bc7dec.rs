//! Fast BC7 block decode for the RDO pass, which decodes millions of trial blocks per frame. The
//! modes this encoder writes (1, 5, 6, 7) have hand-specialised decoders with every field at a
//! fixed bit position; the other modes go through [`dxvkit_plus_core::bc7::bc7_block`] (written from
//! the BPTC specification). The result is identical to that decoder for every block — the unit test
//! compares the two on random blocks of every mode — so the RDO decisions, and the encoder's output,
//! do not change.
//
// Lint note: fixed 16-pixel arrays indexed by selector values that are masked to the weight-table
// size; relaxed for this module only.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::needless_range_loop)]

use crate::tables::{ANCHOR2, PARTITION2};

const W2: [u32; 4] = [0, 21, 43, 64];
const W3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

#[inline(always)]
fn bits(v: u128, at: u32, n: u32) -> u32 {
    ((v >> at) as u32) & ((1u32 << n) - 1)
}

#[inline(always)]
fn interp(e0: u32, e1: u32, w: u32) -> u8 {
    (((64 - w) * e0 + w * e1 + 32) >> 6) as u8
}

/// Read 16 indices of `n` bits starting at `at`; the pixels in `anchors` have one bit fewer.
#[inline(always)]
fn indices(v: u128, mut at: u32, n: u32, anchors: [usize; 2]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        let k = if i == anchors[0] || i == anchors[1] { n - 1 } else { n };
        *o = bits(v, at, k) as u8;
        at += k;
    }
    out
}

/// Decode a block, or `None` for the reserved mode (first byte 0), as `bc7decomp::unpack_bc7`.
#[inline]
pub fn decode(block: &[u8; 16]) -> Option<[[u8; 4]; 16]> {
    let b0 = block[0];
    if b0 == 0 {
        return None;
    }
    let v = u128::from_le_bytes(*block);
    Some(match b0.trailing_zeros() {
        1 => mode1(v),
        5 => mode5(v),
        6 => mode6(v),
        7 => mode7(v),
        _ => dxvkit_plus_core::bc7::bc7_block(block),
    })
}

fn mode6(v: u128) -> [[u8; 4]; 16] {
    let (p0, p1) = (bits(v, 63, 1), bits(v, 64, 1));
    let mut e = [[0u32; 4]; 2];
    for c in 0..4 {
        e[0][c] = (bits(v, 7 + 14 * c as u32, 7) << 1) | p0;
        e[1][c] = (bits(v, 14 + 14 * c as u32, 7) << 1) | p1;
    }
    let mut pal = [[0u8; 4]; 16];
    for (i, p) in pal.iter_mut().enumerate() {
        for c in 0..4 {
            p[c] = interp(e[0][c], e[1][c], W4[i]);
        }
    }
    let idx = indices(v, 65, 4, [0, 0]);
    idx.map(|i| pal[usize::from(i)])
}

fn mode5(v: u128) -> [[u8; 4]; 16] {
    let rot = bits(v, 6, 2);
    let mut e = [[0u32; 4]; 2];
    for c in 0..3 {
        for k in 0..2 {
            let x = bits(v, 8 + 14 * c as u32 + 7 * k as u32, 7);
            e[k][c] = (x << 1) | (x >> 6);
        }
    }
    e[0][3] = bits(v, 50, 8);
    e[1][3] = bits(v, 58, 8);
    let ci = indices(v, 66, 2, [0, 0]);
    let ai = indices(v, 97, 2, [0, 0]);
    let mut out = [[0u8; 4]; 16];
    for i in 0..16 {
        let (wc, wa) = (W2[usize::from(ci[i])], W2[usize::from(ai[i])]);
        let mut p = [interp(e[0][0], e[1][0], wc), interp(e[0][1], e[1][1], wc), interp(e[0][2], e[1][2], wc), interp(e[0][3], e[1][3], wa)];
        if rot >= 1 {
            p.swap(3, rot as usize - 1);
        }
        out[i] = p;
    }
    out
}

fn mode1(v: u128) -> [[u8; 4]; 16] {
    let part = bits(v, 2, 6) as usize;
    let sp = [bits(v, 80, 1), bits(v, 81, 1)];
    let mut e = [[[0u32; 3]; 2]; 2]; // [subset][endpoint][channel]
    for c in 0..3 {
        for s in 0..2 {
            for k in 0..2 {
                let x = (bits(v, 8 + 24 * c as u32 + 12 * s as u32 + 6 * k as u32, 6) << 1) | sp[s];
                e[s][k][c] = (x << 1) | (x >> 6);
            }
        }
    }
    let idx = indices(v, 82, 3, [0, usize::from(ANCHOR2[part])]);
    let p = &PARTITION2[part];
    let mut out = [[0u8; 4]; 16];
    for i in 0..16 {
        let s = usize::from(p[i]);
        let w = W3[usize::from(idx[i])];
        out[i] = [interp(e[s][0][0], e[s][1][0], w), interp(e[s][0][1], e[s][1][1], w), interp(e[s][0][2], e[s][1][2], w), 255];
    }
    out
}

fn mode7(v: u128) -> [[u8; 4]; 16] {
    let part = bits(v, 8, 6) as usize;
    let mut e = [[[0u32; 4]; 2]; 2]; // [subset][endpoint][channel]
    for c in 0..4 {
        for s in 0..2 {
            for k in 0..2 {
                let p = bits(v, 94 + 2 * s as u32 + k as u32, 1);
                let x = (bits(v, 14 + 20 * c as u32 + 10 * s as u32 + 5 * k as u32, 5) << 1) | p;
                e[s][k][c] = (x << 2) | (x >> 4);
            }
        }
    }
    let idx = indices(v, 98, 2, [0, usize::from(ANCHOR2[part])]);
    let p = &PARTITION2[part];
    let mut out = [[0u8; 4]; 16];
    for i in 0..16 {
        let s = usize::from(p[i]);
        let w = W2[usize::from(idx[i])];
        out[i] = [0, 1, 2, 3].map(|c| interp(e[s][0][c], e[s][1][c], w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equal to the specification decoder on 200 000 random blocks of every mode (and `None`
    /// exactly for the reserved mode).
    #[test]
    fn matches_the_specification_decoder_on_random_blocks() {
        let mut s = 0x1234_5678_9ABC_DEF0u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for mode in 0..9u32 {
            for _ in 0..200_000 {
                let mut v = (u128::from(next()) << 64) | u128::from(next());
                if mode < 8 {
                    v &= !((1u128 << (mode + 1)) - 1);
                    v |= 1u128 << mode;
                } else {
                    v &= !0xFF;
                }
                let b = v.to_le_bytes();
                match decode(&b) {
                    None => assert_eq!(b[0], 0),
                    Some(px) => assert_eq!(px, dxvkit_plus_core::bc7::bc7_block(&b), "mode {mode} block {b:02x?}"),
                }
            }
        }
    }
}
