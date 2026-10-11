//! BC7 (BPTC UNORM) CPU block decoder, written from the public format definition (Khronos Data
//! Format Specification 1.3, "BPTC Compressed Texture Image Formats"; Microsoft "BC7 Format").
//! Used for HAP R (`Hap7`) CPU decode and as the CPU oracle for the GPU path (the test suite checks
//! it against the GPU's own BC7 decode, bit for bit, on random blocks of every mode).
//!
//! Decode rules (all from the specification): the mode is the position of the lowest set bit of
//! the first byte; fields follow LSB-first; endpoints are quantised per mode, extended by their
//! p-bit, then expanded to 8 bits by bit replication; colours interpolate with the 6-bit weights
//! `((64 - w) * e0 + w * e1 + 32) >> 6`; each subset's anchor index has an implicit 0 MSB; modes 4
//! and 5 rotate a channel with alpha after interpolation. A reserved mode (first byte 0) decodes
//! to transparent black, as the specification requires.
//
// Lint note: every arithmetic operand is a field of at most 8 bits, a 6-bit weight or a bit index
// below 128, so no expression can overflow; the arithmetic lint is relaxed for this module only.
// Dynamic indices still go through `get`.
#![allow(clippy::arithmetic_side_effects)]

/// Per mode: subsets, partition bits, rotation bits, index-selection bits, colour bits, alpha bits,
/// endpoint p-bits (per endpoint), shared p-bits (per subset), index bits, secondary index bits.
#[derive(Clone, Copy)]
struct Mode {
    ns: usize,
    pb: u32,
    rb: u32,
    isb: u32,
    cb: u32,
    ab: u32,
    epb: bool,
    spb: bool,
    ib: u32,
    ib2: u32,
}

const MODES: [Mode; 8] = [
    Mode { ns: 3, pb: 4, rb: 0, isb: 0, cb: 4, ab: 0, epb: true, spb: false, ib: 3, ib2: 0 },
    Mode { ns: 2, pb: 6, rb: 0, isb: 0, cb: 6, ab: 0, epb: false, spb: true, ib: 3, ib2: 0 },
    Mode { ns: 3, pb: 6, rb: 0, isb: 0, cb: 5, ab: 0, epb: false, spb: false, ib: 2, ib2: 0 },
    Mode { ns: 2, pb: 6, rb: 0, isb: 0, cb: 7, ab: 0, epb: true, spb: false, ib: 2, ib2: 0 },
    Mode { ns: 1, pb: 0, rb: 2, isb: 1, cb: 5, ab: 6, epb: false, spb: false, ib: 2, ib2: 3 },
    Mode { ns: 1, pb: 0, rb: 2, isb: 0, cb: 7, ab: 8, epb: false, spb: false, ib: 2, ib2: 2 },
    Mode { ns: 1, pb: 0, rb: 0, isb: 0, cb: 7, ab: 7, epb: true, spb: false, ib: 4, ib2: 0 },
    Mode { ns: 2, pb: 6, rb: 0, isb: 0, cb: 5, ab: 5, epb: true, spb: false, ib: 2, ib2: 0 },
];

const W2: [u32; 4] = [0, 21, 43, 64];
const W3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// Two-subset partitions: the subset of each pixel (row-major).
#[rustfmt::skip]
pub(crate) const P2: [[u8; 16]; 64] = [
    [0,0,1,1,0,0,1,1,0,0,1,1,0,0,1,1], [0,0,0,1,0,0,0,1,0,0,0,1,0,0,0,1], [0,1,1,1,0,1,1,1,0,1,1,1,0,1,1,1], [0,0,0,1,0,0,1,1,0,0,1,1,0,1,1,1],
    [0,0,0,0,0,0,0,1,0,0,0,1,0,0,1,1], [0,0,1,1,0,1,1,1,0,1,1,1,1,1,1,1], [0,0,0,1,0,0,1,1,0,1,1,1,1,1,1,1], [0,0,0,0,0,0,0,1,0,0,1,1,0,1,1,1],
    [0,0,0,0,0,0,0,0,0,0,0,1,0,0,1,1], [0,0,1,1,0,1,1,1,1,1,1,1,1,1,1,1], [0,0,0,0,0,0,0,1,0,1,1,1,1,1,1,1], [0,0,0,0,0,0,0,0,0,0,0,1,0,1,1,1],
    [0,0,0,1,0,1,1,1,1,1,1,1,1,1,1,1], [0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,1], [0,0,0,0,1,1,1,1,1,1,1,1,1,1,1,1], [0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1],
    [0,0,0,0,1,0,0,0,1,1,1,0,1,1,1,1], [0,1,1,1,0,0,0,1,0,0,0,0,0,0,0,0], [0,0,0,0,0,0,0,0,1,0,0,0,1,1,1,0], [0,1,1,1,0,0,1,1,0,0,0,1,0,0,0,0],
    [0,0,1,1,0,0,0,1,0,0,0,0,0,0,0,0], [0,0,0,0,1,0,0,0,1,1,0,0,1,1,1,0], [0,0,0,0,0,0,0,0,1,0,0,0,1,1,0,0], [0,1,1,1,0,0,1,1,0,0,1,1,0,0,0,1],
    [0,0,1,1,0,0,0,1,0,0,0,1,0,0,0,0], [0,0,0,0,1,0,0,0,1,0,0,0,1,1,0,0], [0,1,1,0,0,1,1,0,0,1,1,0,0,1,1,0], [0,0,1,1,0,1,1,0,0,1,1,0,1,1,0,0],
    [0,0,0,1,0,1,1,1,1,1,1,0,1,0,0,0], [0,0,0,0,1,1,1,1,1,1,1,1,0,0,0,0], [0,1,1,1,0,0,0,1,1,0,0,0,1,1,1,0], [0,0,1,1,1,0,0,1,1,0,0,1,1,1,0,0],
    [0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1], [0,0,0,0,1,1,1,1,0,0,0,0,1,1,1,1], [0,1,0,1,1,0,1,0,0,1,0,1,1,0,1,0], [0,0,1,1,0,0,1,1,1,1,0,0,1,1,0,0],
    [0,0,1,1,1,1,0,0,0,0,1,1,1,1,0,0], [0,1,0,1,0,1,0,1,1,0,1,0,1,0,1,0], [0,1,1,0,1,0,0,1,0,1,1,0,1,0,0,1], [0,1,0,1,1,0,1,0,1,0,1,0,0,1,0,1],
    [0,1,1,1,0,0,1,1,1,1,0,0,1,1,1,0], [0,0,0,1,0,0,1,1,1,1,0,0,1,0,0,0], [0,0,1,1,0,0,1,0,0,1,0,0,1,1,0,0], [0,0,1,1,1,0,1,1,1,1,0,1,1,1,0,0],
    [0,1,1,0,1,0,0,1,1,0,0,1,0,1,1,0], [0,0,1,1,1,1,0,0,1,1,0,0,0,0,1,1], [0,1,1,0,0,1,1,0,1,0,0,1,1,0,0,1], [0,0,0,0,0,1,1,0,0,1,1,0,0,0,0,0],
    [0,1,0,0,1,1,1,0,0,1,0,0,0,0,0,0], [0,0,1,0,0,1,1,1,0,0,1,0,0,0,0,0], [0,0,0,0,0,0,1,0,0,1,1,1,0,0,1,0], [0,0,0,0,0,1,0,0,1,1,1,0,0,1,0,0],
    [0,1,1,0,1,1,0,0,1,0,0,1,0,0,1,1], [0,0,1,1,0,1,1,0,1,1,0,0,1,0,0,1], [0,1,1,0,0,0,1,1,1,0,0,1,1,1,0,0], [0,0,1,1,1,0,0,1,1,1,0,0,0,1,1,0],
    [0,1,1,0,1,1,0,0,1,1,0,0,1,0,0,1], [0,1,1,0,0,0,1,1,0,0,1,1,1,0,0,1], [0,1,1,1,1,1,1,0,1,0,0,0,0,0,0,1], [0,0,0,1,1,0,0,0,1,1,1,0,0,1,1,1],
    [0,0,0,0,1,1,1,1,0,0,1,1,0,0,1,1], [0,0,1,1,0,0,1,1,1,1,1,1,0,0,0,0], [0,0,1,0,0,0,1,0,1,1,1,0,1,1,1,0], [0,1,0,0,0,1,0,0,0,1,1,1,0,1,1,1],
];

/// Three-subset partitions.
#[rustfmt::skip]
pub(crate) const P3: [[u8; 16]; 64] = [
    [0,0,1,1,0,0,1,1,0,2,2,1,2,2,2,2], [0,0,0,1,0,0,1,1,2,2,1,1,2,2,2,1], [0,0,0,0,2,0,0,1,2,2,1,1,2,2,1,1], [0,2,2,2,0,0,2,2,0,0,1,1,0,1,1,1],
    [0,0,0,0,0,0,0,0,1,1,2,2,1,1,2,2], [0,0,1,1,0,0,1,1,0,0,2,2,0,0,2,2], [0,0,2,2,0,0,2,2,1,1,1,1,1,1,1,1], [0,0,1,1,0,0,1,1,2,2,1,1,2,2,1,1],
    [0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2], [0,0,0,0,1,1,1,1,1,1,1,1,2,2,2,2], [0,0,0,0,1,1,1,1,2,2,2,2,2,2,2,2], [0,0,1,2,0,0,1,2,0,0,1,2,0,0,1,2],
    [0,1,1,2,0,1,1,2,0,1,1,2,0,1,1,2], [0,1,2,2,0,1,2,2,0,1,2,2,0,1,2,2], [0,0,1,1,0,1,1,2,1,1,2,2,1,2,2,2], [0,0,1,1,2,0,0,1,2,2,0,0,2,2,2,0],
    [0,0,0,1,0,0,1,1,0,1,1,2,1,1,2,2], [0,1,1,1,0,0,1,1,2,0,0,1,2,2,0,0], [0,0,0,0,1,1,2,2,1,1,2,2,1,1,2,2], [0,0,2,2,0,0,2,2,0,0,2,2,1,1,1,1],
    [0,1,1,1,0,1,1,1,0,2,2,2,0,2,2,2], [0,0,0,1,0,0,0,1,2,2,2,1,2,2,2,1], [0,0,0,0,0,0,1,1,0,1,2,2,0,1,2,2], [0,0,0,0,1,1,0,0,2,2,1,0,2,2,1,0],
    [0,1,2,2,0,1,2,2,0,0,1,1,0,0,0,0], [0,0,1,2,0,0,1,2,1,1,2,2,2,2,2,2], [0,1,1,0,1,2,2,1,1,2,2,1,0,1,1,0], [0,0,0,0,0,1,1,0,1,2,2,1,1,2,2,1],
    [0,0,2,2,1,1,0,2,1,1,0,2,0,0,2,2], [0,1,1,0,0,1,1,0,2,0,0,2,2,2,2,2], [0,0,1,1,0,1,2,2,0,1,2,2,0,0,1,1], [0,0,0,0,2,0,0,0,2,2,1,1,2,2,2,1],
    [0,0,0,0,0,0,0,2,1,1,2,2,1,2,2,2], [0,2,2,2,0,0,2,2,0,0,1,2,0,0,1,1], [0,0,1,1,0,0,1,2,0,0,2,2,0,2,2,2], [0,1,2,0,0,1,2,0,0,1,2,0,0,1,2,0],
    [0,0,0,0,1,1,1,1,2,2,2,2,0,0,0,0], [0,1,2,0,1,2,0,1,2,0,1,2,0,1,2,0], [0,1,2,0,2,0,1,2,1,2,0,1,0,1,2,0], [0,0,1,1,2,2,0,0,1,1,2,2,0,0,1,1],
    [0,0,1,1,1,1,2,2,2,2,0,0,0,0,1,1], [0,1,0,1,0,1,0,1,2,2,2,2,2,2,2,2], [0,0,0,0,0,0,0,0,2,1,2,1,2,1,2,1], [0,0,2,2,1,1,2,2,0,0,2,2,1,1,2,2],
    [0,0,2,2,0,0,1,1,0,0,2,2,0,0,1,1], [0,2,2,0,1,2,2,1,0,2,2,0,1,2,2,1], [0,1,0,1,2,2,2,2,2,2,2,2,0,1,0,1], [0,0,0,0,2,1,2,1,2,1,2,1,2,1,2,1],
    [0,1,0,1,0,1,0,1,0,1,0,1,2,2,2,2], [0,2,2,2,0,1,1,1,0,2,2,2,0,1,1,1], [0,0,0,2,1,1,1,2,0,0,0,2,1,1,1,2], [0,0,0,0,2,1,1,2,2,1,1,2,2,1,1,2],
    [0,2,2,2,0,1,1,1,0,1,1,1,0,2,2,2], [0,0,0,2,1,1,1,2,1,1,1,2,0,0,0,2], [0,1,1,0,0,1,1,0,0,1,1,0,2,2,2,2], [0,0,0,0,0,0,0,0,2,1,1,2,2,1,1,2],
    [0,1,1,0,0,1,1,0,2,2,2,2,2,2,2,2], [0,0,2,2,0,0,1,1,0,0,1,1,0,0,2,2], [0,0,2,2,1,1,2,2,1,1,2,2,0,0,2,2], [0,0,0,0,0,0,0,0,0,0,0,0,2,1,1,2],
    [0,0,0,2,0,0,0,1,0,0,0,2,0,0,0,1], [0,2,2,2,1,2,2,2,0,2,2,2,1,2,2,2], [0,1,0,1,2,2,2,2,2,2,2,2,2,2,2,2], [0,1,1,1,2,0,1,1,2,2,0,1,2,2,2,0],
];

/// Anchor index of subset 1 in the two-subset partitions.
#[rustfmt::skip]
pub(crate) const A2: [u8; 64] = [
    15,15,15,15,15,15,15,15, 15,15,15,15,15,15,15,15, 15, 2, 8, 2, 2, 8, 8,15,  2, 8, 2, 2, 8, 8, 2, 2,
    15,15, 6, 8, 2, 8,15,15,  2, 8, 2, 2, 2,15,15, 6,  6, 2, 6, 8,15,15, 2, 2, 15,15,15,15,15, 2, 2,15,
];
/// Anchor index of subset 1 in the three-subset partitions.
#[rustfmt::skip]
pub(crate) const A3A: [u8; 64] = [
     3, 3,15,15, 8, 3,15,15,  8, 8, 6, 6, 6, 5, 3, 3,  3, 3, 8,15, 3, 3, 6,10,  5, 8, 8, 6, 8, 5,15,15,
     8,15, 3, 5, 6,10, 8,15, 15, 3,15, 5,15,15,15,15,  3,15, 5, 5, 5, 8, 5,10,  5,10, 8,13,15,12, 3, 3,
];
/// Anchor index of subset 2 in the three-subset partitions.
#[rustfmt::skip]
pub(crate) const A3B: [u8; 64] = [
    15, 8, 8, 3,15,15, 3, 8, 15,15,15,15,15,15,15, 8, 15, 8,15, 3,15, 8,15, 8,  3,15, 6,10,15,15,10, 8,
    15, 3,15,10,10, 8, 9,10,  6,15, 8,15, 3, 6, 6, 8, 15, 3,15,15,15,15,15,15, 15,15,15,15, 3,15,15, 8,
];

/// LSB-first reader over one 128-bit block.
struct Bits {
    v: u128,
    at: u32,
}

impl Bits {
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 || self.at >= 128 {
            return 0;
        }
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        let out = ((self.v >> self.at) as u32) & mask;
        self.at = self.at.saturating_add(n);
        out
    }
}

/// Expand an `n`-bit value (4 <= n <= 8) to 8 bits by bit replication.
fn expand(v: u32, n: u32) -> u32 {
    if n >= 8 {
        return v & 0xFF;
    }
    let v = v << (8 - n);
    v | (v >> n)
}

fn interp(e0: u32, e1: u32, w: u32) -> u8 {
    (((64 - w) * e0 + w * e1 + 32) >> 6) as u8
}

fn weight(bits: u32, i: u32) -> u32 {
    let i = i as usize;
    match bits {
        2 => W2.get(i).copied().unwrap_or(0),
        3 => W3.get(i).copied().unwrap_or(0),
        _ => W4.get(i).copied().unwrap_or(0),
    }
}

/// 16 RGBA pixels (row-major) of one BC7 block. A short block or a reserved mode decodes to
/// transparent black.
pub fn bc7_block(blk: &[u8]) -> [[u8; 4]; 16] {
    let Some(bytes) = blk.get(..16).and_then(|b| <[u8; 16]>::try_from(b).ok()) else { return [[0; 4]; 16] };
    let v = u128::from_le_bytes(bytes);
    let first = (v & 0xFF) as u32;
    if first == 0 {
        return [[0; 4]; 16];
    }
    let mode_index = first.trailing_zeros();
    let Some(&m) = MODES.get(mode_index as usize) else { return [[0; 4]; 16] };
    let mut b = Bits { v, at: mode_index + 1 };
    let partition = b.take(m.pb) as usize;
    let rotation = b.take(m.rb);
    let index_sel = b.take(m.isb);
    // Endpoints: [subset][endpoint][channel], channel order R, G, B, A.
    let mut ep = [[[0u32; 4]; 2]; 3];
    for ch in 0..4 {
        let n = if ch < 3 { m.cb } else { m.ab };
        for s in ep.iter_mut().take(m.ns) {
            for e in s.iter_mut() {
                if let Some(c) = e.get_mut(ch) {
                    *c = b.take(n);
                }
            }
        }
    }
    // P-bits extend every channel (including alpha when present) by one LSB.
    if m.epb || m.spb {
        let mut pbits = [[0u32; 2]; 3];
        for s in pbits.iter_mut().take(m.ns) {
            if m.epb {
                s[0] = b.take(1);
                s[1] = b.take(1);
            } else {
                let p = b.take(1);
                *s = [p, p];
            }
        }
        for (s, p) in ep.iter_mut().zip(pbits).take(m.ns) {
            for (e, pb) in s.iter_mut().zip(p) {
                for c in e.iter_mut() {
                    *c = (*c << 1) | pb;
                }
            }
        }
    }
    let pbit = u32::from(m.epb || m.spb);
    for s in ep.iter_mut().take(m.ns) {
        for e in s.iter_mut() {
            for (ch, c) in e.iter_mut().enumerate() {
                *c = if ch < 3 {
                    expand(*c, m.cb + pbit)
                } else if m.ab == 0 {
                    255
                } else {
                    expand(*c, m.ab + pbit)
                };
            }
        }
    }
    let subset_of = |px: usize| -> usize {
        match m.ns {
            2 => P2.get(partition).and_then(|p| p.get(px)).copied().unwrap_or(0) as usize,
            3 => P3.get(partition).and_then(|p| p.get(px)).copied().unwrap_or(0) as usize,
            _ => 0,
        }
    };
    let is_anchor = |px: usize| -> bool {
        if px == 0 {
            return true;
        }
        match m.ns {
            2 => A2.get(partition).is_some_and(|&a| usize::from(a) == px),
            3 => A3A.get(partition).is_some_and(|&a| usize::from(a) == px) || A3B.get(partition).is_some_and(|&a| usize::from(a) == px),
            _ => false,
        }
    };
    let mut idx = [0u32; 16];
    for (px, i) in idx.iter_mut().enumerate() {
        *i = b.take(if is_anchor(px) { m.ib - 1 } else { m.ib });
    }
    let mut idx2 = [0u32; 16];
    if m.ib2 > 0 {
        for (px, i) in idx2.iter_mut().enumerate() {
            *i = b.take(if px == 0 { m.ib2 - 1 } else { m.ib2 });
        }
    }
    let mut out = [[0u8; 4]; 16];
    for (px, o) in out.iter_mut().enumerate() {
        let s = subset_of(px);
        let Some(e) = ep.get(s) else { continue };
        let (i1, i2) = (idx.get(px).copied().unwrap_or(0), idx2.get(px).copied().unwrap_or(0));
        // Mode 4 index selection: colour and alpha swap which index set (and width) they use.
        let ((ci, cbits), (ai, abits)) = if m.ib2 == 0 {
            ((i1, m.ib), (i1, m.ib))
        } else if index_sel == 0 {
            ((i1, m.ib), (i2, m.ib2))
        } else {
            ((i2, m.ib2), (i1, m.ib))
        };
        let cw = weight(cbits, ci);
        let aw = weight(abits, ai);
        let [e0, e1] = e;
        let mut p = [interp(e0[0], e1[0], cw), interp(e0[1], e1[1], cw), interp(e0[2], e1[2], cw), interp(e0[3], e1[3], aw)];
        match rotation {
            1 => p.swap(0, 3),
            2 => p.swap(1, 3),
            3 => p.swap(2, 3),
            _ => {}
        }
        *o = p;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_lie_in_their_subsets() {
        for p in 0..64 {
            assert_eq!(P2[p][usize::from(A2[p])], 1, "2-subset partition {p}");
            assert_eq!(P3[p][usize::from(A3A[p])], 1, "3-subset partition {p}");
            assert_eq!(P3[p][usize::from(A3B[p])], 2, "3-subset partition {p}");
            // Pixel 0 is always subset 0 (its anchor).
            assert_eq!((P2[p][0], P3[p][0]), (0, 0));
            // Every listed subset occurs.
            assert!(P2[p].contains(&1));
            assert!(P3[p].contains(&1) && P3[p].contains(&2));
        }
    }

    #[test]
    fn mode6_solid_block_decodes_to_its_endpoint() {
        // Mode 6, endpoints (c << 1) | p with p = 1, every index 0.
        let mut v: u128 = 1 << 6;
        let mut at = 7u32;
        let mut put = |x: u128, n: u32| {
            v |= x << at;
            at += n;
        };
        for c in [100u128, 50, 25, 127] {
            put(c, 7);
            put(c, 7);
        }
        put(1, 1);
        put(1, 1);
        let px = bc7_block(&v.to_le_bytes());
        assert!(px.iter().all(|p| *p == [201, 101, 51, 255]), "{:?}", px[0]);
        // Negative controls: reserved mode and a short block are transparent black.
        assert_eq!(bc7_block(&[0u8; 16])[5], [0, 0, 0, 0]);
        assert_eq!(bc7_block(&[0x40u8; 15])[0], [0, 0, 0, 0]);
    }

    #[test]
    fn expansion_replicates_bits() {
        assert_eq!(expand(0b11111, 5), 255);
        assert_eq!(expand(0b10000, 5), 0b1000_0100);
        assert_eq!(expand(0x7F, 7), 0xFF);
        assert_eq!(expand(0x40, 7), 0x81);
    }
}
