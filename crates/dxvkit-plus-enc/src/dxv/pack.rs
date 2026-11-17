//! The DXV3 block-copy layers for DXT1 (BC1) and DXT5 (BC3): block planes → packet payload.
//!
//! Ported to safe Rust from DXVKit `src/enc_pack.c` (github.com/AmritusG/dxvkit @ fb506da, MIT,
//! Copyright (c) 2026 Amrit Rosell — `LICENSES/DXVKit-MIT.txt`). The packer emits exactly what the
//! decoder reads, in the decoder's order (DXV spec §5, §6): 2-bit flags packed 16 per little-endian
//! u32, each word placed where the decoder reads its first flag.
//!
//! Lint note: block indices are below the plane's block count (≤ (16384 / 4)² for a 16K frame),
//! so `index × 16` and the distance arithmetic cannot overflow `usize`; every block read goes
//! through `get`.
#![allow(clippy::arithmetic_side_effects)]

/// Largest whole-block / half distance (u16 + 258).
const MAX_DISTANCE: usize = 65_793;
/// Largest DXT5 alpha-half distance (u16 + 2).
const MAX_ALPHA_DISTANCE: usize = 65_537;
/// Whole-block run length that makes a block run cheaper than continuing an alpha run (best).
const BEST_RUN_CUT: usize = 11;

/// The most recent block holding a value: open hash table of block index + 1 (0 = empty).
pub(super) struct MatchTable {
    slot: Vec<u32>,
    bits: u32,
}

impl MatchTable {
    pub(super) fn new(bits: u32) -> MatchTable {
        MatchTable { slot: vec![0; 1usize << bits], bits }
    }
    fn hash(&self, v: u64) -> usize {
        (v.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - self.bits)) as usize
    }
    pub(super) fn get(&self, v: u64) -> u32 {
        self.slot.get(self.hash(v)).copied().unwrap_or(0)
    }
    pub(super) fn set(&mut self, v: u64, b: usize) {
        let h = self.hash(v);
        if let Some(s) = self.slot.get_mut(h) {
            *s = (b as u32).wrapping_add(1);
        }
    }
    pub(super) fn clear(&mut self) {
        self.slot.fill(0);
    }
}

/// DXVKit `table_bits`: 10 bits, growing with the block count up to 16 (fast) / 20 (best).
pub(super) fn table_bits(blocks: usize, best: bool) -> u32 {
    let (mut bits, cap) = (10u32, if best { 20u32 } else { 16 });
    while bits < cap && (1usize << (bits - 1)) < blocks {
        bits += 1;
    }
    bits
}

struct FlagWriter {
    out: Vec<u8>,
    word_pos: usize,
    used: u32,
}

impl FlagWriter {
    fn new(cap: usize) -> FlagWriter {
        FlagWriter { out: Vec::with_capacity(cap), word_pos: 0, used: 16 }
    }
    fn flag(&mut self, flag: u32) {
        if self.used == 16 {
            self.word_pos = self.out.len();
            self.out.extend_from_slice(&[0; 4]);
            self.used = 0;
        }
        if let Some(b) = self.out.get_mut(self.word_pos + (self.used / 4) as usize) {
            *b |= ((flag & 3) << (2 * (self.used & 3))) as u8;
        }
        self.used += 1;
    }
    fn byte(&mut self, v: usize) {
        self.out.push(v as u8);
    }
    fn u16(&mut self, v: usize) {
        self.out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    fn bytes(&mut self, p: &[u8]) {
        self.out.extend_from_slice(p);
    }
    /// Count extension (spec §6.4): u16 values while each is 0xFFFF.
    fn extension(&mut self, mut rest: usize) {
        while rest >= 0xFFFF {
            self.u16(0xFFFF);
            rest -= 0xFFFF;
        }
        self.u16(rest);
    }
    fn distance(&mut self, d: usize) {
        if d == 1 {
            self.flag(1);
        } else if d <= 257 {
            self.flag(2);
            self.byte(d - 2);
        } else {
            self.flag(3);
            self.u16(d - 258);
        }
    }
    fn unit(&mut self, src: &[u8], dist: usize) {
        if dist != 0 {
            self.distance(dist);
        } else {
            self.flag(0);
            self.bytes(src);
        }
    }
}

/// Cost in bits of a distance code (flag included), or 0 for "not codable".
fn distance_bits(d: usize) -> u32 {
    match d {
        1 => 2,
        2..=257 => 10,
        258..=MAX_DISTANCE => 18,
        _ => 0,
    }
}

fn part(blocks: &[u8], stride: usize, b: usize, offset: usize, n: usize) -> &[u8] {
    let s = b * stride + offset;
    blocks.get(s..s + n).unwrap_or(&[])
}

fn key(p: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    for (d, s) in a.iter_mut().zip(p) {
        *d = *s;
    }
    u64::from_le_bytes(a)
}

/// Nearest earlier block (< b) whose `n` bytes at `offset` equal block b's, as a distance (0 = none).
fn find(t: &MatchTable, blocks: &[u8], stride: usize, b: usize, offset: usize, n: usize) -> usize {
    let cur = part(blocks, stride, b, offset, n);
    let j = t.get(key(cur)) as usize;
    if j == 0 || j > b {
        return 0;
    }
    if part(blocks, stride, j - 1, offset, n) != cur {
        return 0;
    }
    b - (j - 1)
}

fn remember(t: &mut MatchTable, blocks: &[u8], stride: usize, b: usize, offset: usize, n: usize) {
    t.set(key(part(blocks, stride, b, offset, n)), b);
}

fn same_as_previous(blocks: &[u8], stride: usize, b: usize, offset: usize, n: usize) -> bool {
    b > 0 && part(blocks, stride, b, offset, n) == part(blocks, stride, b - 1, offset, n)
}

/// The bits of a unit's cheapest coding and its distance (0 = literal).
fn unit_cost(t: &MatchTable, blocks: &[u8], stride: usize, b: usize, offset: usize, n: usize) -> (u32, usize) {
    if same_as_previous(blocks, stride, b, offset, n) {
        return (2, 1);
    }
    let d = find(t, blocks, stride, b, offset, n);
    if d != 0 && distance_bits(d) != 0 {
        return (distance_bits(d), d);
    }
    (2 + 8 * n as u32, 0)
}

/// DXT1 (BC1) block-copy payload for `count` 8-byte blocks.
pub(super) fn pack_dxt1(blocks: &[u8], count: usize, tables: &mut [MatchTable; 5]) -> Vec<u8> {
    let [whole, half0, half1, _, _] = tables;
    whole.clear();
    half0.clear();
    half1.clear();
    let mut w = FlagWriter::new(12 * count + 16);
    w.bytes(part(blocks, 8, 0, 0, 8));
    remember(whole, blocks, 8, 0, 0, 8);
    remember(half0, blocks, 8, 0, 0, 4);
    remember(half1, blocks, 8, 0, 4, 4);
    for b in 1..count {
        let (dw, whole_bits) = if same_as_previous(blocks, 8, b, 0, 8) {
            (1, 2)
        } else {
            let d = find(whole, blocks, 8, b, 0, 8);
            (d, if d != 0 { distance_bits(d) } else { 0 })
        };
        if whole_bits == 2 {
            w.flag(1);
        } else {
            let (c0, d0) = unit_cost(half0, blocks, 8, b, 0, 4);
            let (c1, d1) = unit_cost(half1, blocks, 8, b, 4, 4);
            let split_bits = 2 + c0 + c1;
            if whole_bits != 0 && whole_bits <= split_bits {
                w.distance(dw);
            } else {
                w.flag(0);
                w.unit(part(blocks, 8, b, 0, 4), d0);
                w.unit(part(blocks, 8, b, 4, 4), d1);
            }
        }
        remember(whole, blocks, 8, b, 0, 8);
        remember(half0, blocks, 8, b, 0, 4);
        remember(half1, blocks, 8, b, 4, 4);
    }
    w.out
}

/// Blocks from b on that each equal their predecessor in `n` bytes at `offset`.
fn repeats(blocks: &[u8], b: usize, count: usize, offset: usize, n: usize) -> usize {
    let mut k = b;
    while k < count && same_as_previous(blocks, 16, k, offset, n) {
        k += 1;
    }
    k - b
}

/// DXT5 (BC3) block-copy payload for `count` 16-byte blocks.
pub(super) fn pack_dxt5(blocks: &[u8], count: usize, best: bool, tables: &mut [MatchTable; 5]) -> Vec<u8> {
    let [alpha, colour, q0, q1, _] = tables;
    alpha.clear();
    colour.clear();
    q0.clear();
    q1.clear();
    let rem = |alpha: &mut MatchTable, colour: &mut MatchTable, q0: &mut MatchTable, q1: &mut MatchTable, b: usize| {
        remember(alpha, blocks, 16, b, 0, 8);
        remember(colour, blocks, 16, b, 8, 8);
        remember(q0, blocks, 16, b, 8, 4);
        remember(q1, blocks, 16, b, 12, 4);
    };
    let mut w = FlagWriter::new(24 * count + 32);
    w.bytes(part(blocks, 16, 0, 0, 16));
    rem(alpha, colour, q0, q1, 0);
    let (mut alpha_run, mut b) = (0usize, 1usize);
    while b < count {
        if alpha_run > 0 {
            alpha_run -= 1;
        } else {
            let n = repeats(blocks, b, count, 0, 16);
            if n > 0 {
                w.flag(0);
                if n <= 255 {
                    w.byte(n - 1);
                } else {
                    w.byte(255);
                    w.extension(n - 256);
                }
                for _ in 0..n {
                    rem(alpha, colour, q0, q1, b);
                    b += 1;
                }
                continue;
            }
            if same_as_previous(blocks, 16, b, 0, 8) {
                let (mut r, mut k) = (0usize, b + 1);
                while k < count && same_as_previous(blocks, 16, k, 0, 8) {
                    if best && repeats(blocks, k, count, 0, 16) >= BEST_RUN_CUT {
                        break;
                    }
                    r += 1;
                    k += 1;
                }
                w.flag(1);
                if r < 255 {
                    w.byte(r);
                } else {
                    w.byte(255);
                    w.extension(r - 255);
                }
                alpha_run = r;
            } else {
                let d = find(alpha, blocks, 16, b, 0, 8);
                if (2..=MAX_ALPHA_DISTANCE).contains(&d) {
                    w.flag(2);
                    w.u16(d - 2);
                } else {
                    w.flag(3);
                    w.bytes(part(blocks, 16, b, 0, 8));
                }
            }
        }
        let (colour_bits, dc) = unit_cost(colour, blocks, 16, b, 8, 8);
        if colour_bits == 2 {
            w.flag(1);
        } else {
            let (c0, dq0) = unit_cost(q0, blocks, 16, b, 8, 4);
            let (c1, dq1) = unit_cost(q1, blocks, 16, b, 12, 4);
            let split_bits = 2 + c0 + c1;
            if dc != 0 && colour_bits <= split_bits {
                w.distance(dc);
            } else {
                w.flag(0);
                w.unit(part(blocks, 16, b, 8, 4), dq0);
                w.unit(part(blocks, 16, b, 12, 4), dq1);
            }
        }
        rem(alpha, colour, q0, q1, b);
        b += 1;
    }
    w.out
}
