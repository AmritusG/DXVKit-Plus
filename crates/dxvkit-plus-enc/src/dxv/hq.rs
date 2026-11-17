//! The DXV3 High Quality layer (YCG6, YG10): opcode choice over BC4 blocks with the decoder's two
//! dictionaries, plane sections, and the opcode-stream codings (stored, fill, FSE / tANS).
//!
//! Ported to safe Rust from DXVKit `src/enc_hq.c` (github.com/AmritusG/dxvkit @ fb506da, MIT,
//! Copyright (c) 2026 Amrit Rosell — `LICENSES/DXVKit-MIT.txt`), which was written from the DXV spec
//! §7 and the public FSE descriptions (RFC 8878 §4.1). Same choices, same bytes.
//!
//! Lint note: block indices are below the section's block count (≤ (16384 / 4)² for a 16K frame) and
//! literal / opcode buffers are `Vec`s that only grow by push; the 18-entry and 256-entry tables are
//! indexed by values masked to their length (an opcode < 18, a slot `>> 24`), the 1024-entry FSE
//! tables by states below 1024.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use super::pack::{MatchTable, table_bits};

const OPS: usize = 18;
const TABLE_LOG: u32 = 10;
const TABLE_SIZE: u32 = 1 << TABLE_LOG;
const SPREAD_STEP: u32 = 641;
const MAX_BACK: usize = 65_536;
const SLOT_FULL_E: u32 = 0x1_0000;
const SLOT_FULL_I: u32 = 0x100_0000;

/// Literal bytes each opcode reads before any count extension (spec §7.4.3).
const LITERAL_BYTES: [u32; OPS] = [1, 0, 2, 8, 6, 6, 4, 4, 7, 5, 5, 3, 3, 6, 4, 4, 2, 2];
/// Dictionary inserts per opcode: 1 = E, 2 = I.
const INSERTS: [u8; OPS] = [0, 0, 3, 3, 1, 3, 1, 3, 2, 2, 2, 0, 2, 2, 2, 2, 0, 2];
/// Opcode for (endpoint source, index source).
const COMBO: [[u8; 5]; 3] = [[17, 16, 14, 15, 13], [12, 11, 9, 10, 8], [7, 6, 4, 5, 3]];
/// 16 × log2(1 + i / 16), rounded.
const LOG2_FRAC: [u32; 16] = [0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
/// First-pass opcode cost prior, in 1/16 bit.
const COST_PRIOR: [u32; OPS] = [72, 40, 72, 64, 72, 72, 72, 72, 72, 72, 72, 72, 72, 56, 64, 64, 64, 64];

fn floor_log2(x: u32) -> u32 {
    if x == 0 { 0 } else { 31 - x.leading_zeros() }
}

/// 16 × log2(x), x ≥ 1, within about 1/16.
fn log2_16(x: u32) -> u32 {
    let l = floor_log2(x);
    let m = if l >= 4 { (x >> (l - 4)) & 15 } else { (x << (4 - l)) & 15 };
    16 * l + LOG2_FRAC[m as usize]
}

fn slot_of(v: u32) -> usize {
    (v.wrapping_mul(2_654_435_761) >> 24) as usize
}

fn le16(p: &[u8]) -> u32 {
    u32::from(p[0]) | (u32::from(p[1]) << 8)
}
fn le24(p: &[u8]) -> u32 {
    le16(p) | (u32::from(p[2]) << 16)
}
fn le64(p: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&p[0..8]);
    u64::from_le_bytes(a)
}

struct Channel {
    whole: MatchTable,
    index6: MatchTable,
    ops: Vec<u8>,
    dict_e: [u32; 256],
    dict_i: [u32; 256],
    repeat: usize,
    hist: [u32; OPS],
    cost: [u32; OPS],
}

/// One HQ section (one plane: one or two BC4 channels).
pub(super) struct Section {
    channels: usize,
    /// Bytes per block position (8 = one channel, 16 = two).
    stride: usize,
    best: bool,
    stored_only: bool,
    ch: Vec<Channel>,
}

impl Section {
    pub(super) fn new(channels: usize, blocks: usize, best: bool, stored_only: bool) -> Section {
        let bits = table_bits(blocks, best);
        let ch = (0..channels)
            .map(|_| Channel {
                whole: MatchTable::new(bits),
                index6: MatchTable::new(bits),
                ops: Vec::with_capacity(blocks),
                dict_e: [0; 256],
                dict_i: [0; 256],
                repeat: 0,
                hist: [0; OPS],
                cost: COST_PRIOR,
            })
            .collect();
        Section { channels, stride: 8 * channels, best, stored_only, ch }
    }

    fn block<'a>(&self, src: &'a [u8], ch: usize, b: usize) -> &'a [u8] {
        let s = b * self.stride + 8 * ch;
        src.get(s..s + 8).unwrap_or(&[0; 8])
    }

    fn remember(&mut self, ch: usize, blk: &[u8], b: usize) {
        let c = &mut self.ch[ch];
        c.whole.set(le64(blk), b);
        c.index6.set(le64(blk) >> 16, b);
    }

    /// Nearest earlier block of this channel with the same key (bytes `offset`..8), 1..=MAX_BACK, or 0.
    fn find_back(&self, src: &[u8], ch: usize, by_index: bool, b: usize) -> usize {
        let cur = self.block(src, ch, b);
        let (t, key, offset) = if by_index { (&self.ch[ch].index6, le64(cur) >> 16, 2) } else { (&self.ch[ch].whole, le64(cur), 0) };
        let j = t.get(key) as usize;
        if j == 0 || j > b {
            return 0;
        }
        let d = b - (j - 1);
        if d > MAX_BACK {
            return 0;
        }
        if self.block(src, ch, j - 1)[offset..] != cur[offset..] {
            return 0;
        }
        d
    }

    /// Chooses and emits the opcode for block b (≥ 1) of one channel (`code_block`).
    fn code_block(&mut self, src: &[u8], ch: usize, b: usize, lit: &mut Vec<u8>) {
        let blocks = src.len() / self.stride;
        let cur = self.block(src, ch, b);
        let prev = self.block(src, ch, b - 1);
        if self.ch[ch].repeat > 0 {
            self.ch[ch].repeat -= 1;
            self.remember(ch, cur, b);
            return;
        }
        if cur == prev {
            let mut n = 1usize;
            while b + n < blocks && self.block(src, ch, b + n) == prev {
                n += 1;
            }
            let c = &mut self.ch[ch];
            if n >= 4 && 128u32.wrapping_add(c.cost[0]) <= (n as u32).wrapping_mul(c.cost[1]) {
                put_run_count(lit, n - 4);
                c.ops.push(0);
                c.hist[0] += 1;
                c.repeat = n - 1;
            } else {
                c.ops.push(1);
                c.hist[1] += 1;
            }
            self.remember(ch, cur, b);
            return;
        }
        let e = le16(cur);
        let i1 = le24(&cur[2..]);
        let i2 = le24(&cur[5..]);
        let (es, i1s, i2s) = (slot_of(e), slot_of(i1), slot_of(i2));
        let c = &self.ch[ch];
        let e_prev = e == le16(prev);
        let e_hit = c.dict_e[es] == (e | SLOT_FULL_E);
        let i1_hit = c.dict_i[i1s] == (i1 | SLOT_FULL_I);
        let i2_hit = c.dict_i[i2s] == (i2 | SLOT_FULL_I);
        let dw = self.find_back(src, ch, false, b);
        let di = self.find_back(src, ch, true, b);
        let c = &self.ch[ch];
        let (mut best_cost, mut op) = (u32::MAX, 3usize);
        if dw != 0 {
            best_cost = 2 * 128 + c.cost[2];
            op = 2;
        }
        for (esrc, row) in COMBO.iter().enumerate() {
            if (esrc == 0 && !e_prev) || (esrc == 1 && !e_hit) {
                continue;
            }
            for (isrc, &o) in row.iter().enumerate() {
                if (isrc == 0 && di == 0) || (isrc == 1 && !(i1_hit && i2_hit)) || (isrc == 2 && !i1_hit) || (isrc == 3 && !i2_hit) {
                    continue;
                }
                let o = usize::from(o);
                let cost = 128 * LITERAL_BYTES[o] + c.cost[o];
                if cost < best_cost {
                    best_cost = cost;
                    op = o;
                }
            }
        }
        let u16le = |v: usize| (v as u16).to_le_bytes();
        match op {
            2 => lit.extend_from_slice(&u16le(dw - 1)),
            3 => lit.extend_from_slice(cur),
            4 => {
                lit.push(i1s as u8);
                lit.extend_from_slice(&cur[0..2]);
                lit.extend_from_slice(&cur[5..8]);
            }
            5 => {
                lit.push(i2s as u8);
                lit.extend_from_slice(&cur[0..5]);
            }
            6 => {
                lit.push(i1s as u8);
                lit.push(i2s as u8);
                lit.extend_from_slice(&cur[0..2]);
            }
            7 => {
                lit.extend_from_slice(&u16le(di - 1));
                lit.extend_from_slice(&cur[0..2]);
            }
            8 => {
                lit.push(es as u8);
                lit.extend_from_slice(&cur[2..8]);
            }
            9 => {
                lit.push(es as u8);
                lit.push(i1s as u8);
                lit.extend_from_slice(&cur[5..8]);
            }
            10 => {
                lit.push(es as u8);
                lit.push(i2s as u8);
                lit.extend_from_slice(&cur[2..5]);
            }
            11 => {
                lit.push(es as u8);
                lit.push(i1s as u8);
                lit.push(i2s as u8);
            }
            12 => {
                lit.push(es as u8);
                lit.extend_from_slice(&u16le(di - 1));
            }
            13 => lit.extend_from_slice(&cur[2..8]),
            14 => {
                lit.push(i1s as u8);
                lit.extend_from_slice(&cur[5..8]);
            }
            15 => {
                lit.push(i2s as u8);
                lit.extend_from_slice(&cur[2..5]);
            }
            16 => {
                lit.push(i1s as u8);
                lit.push(i2s as u8);
            }
            _ => lit.extend_from_slice(&u16le(di - 1)),
        }
        let c = &mut self.ch[ch];
        c.ops.push(op as u8);
        c.hist[op] += 1;
        if INSERTS[op] & 1 != 0 {
            c.dict_e[es] = e | SLOT_FULL_E;
        }
        if INSERTS[op] & 2 != 0 {
            c.dict_i[slot_of(i1)] = i1 | SLOT_FULL_I;
        }
        self.remember(ch, cur, b);
    }

    /// One pass: every opcode of the section with the current per-channel costs; returns the literals.
    fn pass(&mut self, src: &[u8]) -> Vec<u8> {
        let blocks = src.len() / self.stride;
        for c in &mut self.ch {
            c.dict_e = [0; 256];
            c.dict_i = [0; 256];
            c.hist = [0; OPS];
            c.repeat = 0;
            c.ops.clear();
            c.whole.clear();
            c.index6.clear();
        }
        let mut lit = Vec::with_capacity(8 * self.channels * blocks + 16);
        for ch in 0..self.channels {
            let blk = self.block(src, ch, 0);
            lit.extend_from_slice(blk);
            let (e, i) = (le16(blk), le24(&blk[2..]));
            let c = &mut self.ch[ch];
            c.dict_e[slot_of(e)] = e | SLOT_FULL_E;
            c.dict_i[slot_of(i)] = i | SLOT_FULL_I;
            self.remember(ch, blk, 0);
        }
        for b in 1..blocks {
            for ch in 0..self.channels {
                self.code_block(src, ch, b, &mut lit);
            }
        }
        lit
    }

    /// The section's bytes: header, literals, then each channel's opcode stream.
    pub(super) fn encode(&mut self, src: &[u8]) -> Vec<u8> {
        for c in &mut self.ch {
            c.cost = COST_PRIOR;
        }
        let mut lit = self.pass(src);
        if self.best {
            for c in &mut self.ch {
                c.cost = costs_from(&c.hist);
            }
            lit = self.pass(src);
        }
        let header = if self.channels == 1 { 8 } else { 12 };
        let mut out = Vec::with_capacity(header + lit.len() + 2 * src.len() / self.stride + 8);
        out.extend_from_slice(&((header + lit.len()) as u32).to_le_bytes());
        for c in &self.ch {
            out.extend_from_slice(&(c.ops.len() as u32).to_le_bytes());
        }
        out.extend_from_slice(&lit);
        for c in &self.ch {
            write_opcodes(&c.ops, self.stored_only, &mut out);
        }
        out
    }
}

/// Literal bytes of opcode 0's count (count = blocks covered − 4).
fn put_run_count(lit: &mut Vec<u8>, count: usize) {
    if count < 255 {
        lit.push(count as u8);
        return;
    }
    lit.push(255);
    let mut rest = count - 255;
    while rest >= 0xFFFF {
        lit.extend_from_slice(&0xFFFFu16.to_le_bytes());
        rest -= 0xFFFF;
    }
    lit.extend_from_slice(&(rest as u16).to_le_bytes());
}

/// Per-channel opcode costs (1/16 bit) from a histogram.
fn costs_from(hist: &[u32; OPS]) -> [u32; OPS] {
    let total: u32 = hist.iter().fold(0u32, |a, h| a.wrapping_add(*h));
    let mut cost = [0u32; OPS];
    for (k, c) in cost.iter_mut().enumerate() {
        *c = log2_16(total.wrapping_mul(2).wrapping_add(OPS as u32)).wrapping_sub(log2_16(hist[k].wrapping_mul(2).wrapping_add(1)));
    }
    cost
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    have: u32,
}

impl BitWriter<'_> {
    fn put(&mut self, v: u32, n: u32) {
        self.acc |= u64::from(v) << self.have;
        self.have += n;
        while self.have >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.have -= 8;
        }
    }
    fn flush(&mut self) {
        if self.have > 0 {
            self.out.push(self.acc as u8);
        }
        self.acc = 0;
        self.have = 0;
    }
}

/// Counts summing to exactly 1024, every used opcode at least 1.
fn normalise(hist: &[u32; OPS], total: u32, symbols: usize) -> [u32; OPS] {
    let mut norm = [0u32; OPS];
    let mut sum = 0u32;
    for s in 0..symbols {
        if hist[s] != 0 {
            let v = (((u64::from(hist[s]) * u64::from(TABLE_SIZE) + u64::from(total / 2)) / u64::from(total.max(1))) as u32).clamp(1, TABLE_SIZE - 1);
            norm[s] = v;
        }
        sum += norm[s];
    }
    while sum > TABLE_SIZE {
        let mut pick = symbols;
        for s in 0..symbols {
            if norm[s] > 1 && (pick == symbols || u64::from(hist[s]) * u64::from(norm[pick]) < u64::from(hist[pick]) * u64::from(norm[s])) {
                pick = s;
            }
        }
        if pick == symbols {
            break;
        }
        norm[pick] -= 1;
        sum -= 1;
    }
    while sum < TABLE_SIZE {
        let mut pick = symbols;
        for s in 0..symbols {
            if hist[s] != 0 && (pick == symbols || u64::from(hist[s]) * u64::from(norm[pick]) > u64::from(hist[pick]) * u64::from(norm[s])) {
                pick = s;
            }
        }
        if pick == symbols {
            break;
        }
        norm[pick] += 1;
        sum += 1;
    }
    norm
}

/// FSE-codes the opcodes (spec §7.3). `None` when they use fewer than two values (use fill).
fn fse_write(ops: &[u8]) -> Option<Vec<u8>> {
    let mut hist = [0u32; OPS];
    for &o in ops {
        hist[usize::from(o) % OPS] += 1;
    }
    let (mut symbols, mut used) = (0usize, 0usize);
    for (s, h) in hist.iter().enumerate() {
        if *h != 0 {
            symbols = s + 1;
            used += 1;
        }
    }
    if used < 2 {
        return None;
    }
    let norm = normalise(&hist, ops.len() as u32, symbols);

    let mut out = Vec::with_capacity(32 + 2 * ops.len());
    let mut tbits = 2usize;
    {
        let mut w = BitWriter { out: &mut out, acc: 0, have: 0 };
        w.put(2, 2);
        let (mut remaining, mut width) = (TABLE_SIZE, TABLE_LOG);
        for &n in norm.iter().take(symbols) {
            if remaining == 0 {
                break;
            }
            w.put(n, width);
            tbits += width as usize;
            remaining = remaining.saturating_sub(n);
            if width > 0 && remaining < (1 << (width - 1)) {
                width -= 1;
            }
        }
        w.flush();
    }
    let table_bytes = 4 + 2 * ((tbits.saturating_sub(32)).div_ceil(16));
    out.resize(table_bytes, 0);

    // the decoder's state table (spread step 641), inverted: state for (symbol, x)
    let (mut start, mut next) = ([0u32; OPS], [0u32; OPS]);
    let mut symbol_at = [0u8; TABLE_SIZE as usize];
    let mut state_for = [0u16; TABLE_SIZE as usize];
    {
        let (mut pos, mut acc) = (0u32, 0u32);
        for s in 0..symbols {
            start[s] = acc;
            acc += norm[s];
            next[s] = norm[s];
            for _ in 0..norm[s] {
                symbol_at[pos as usize] = s as u8;
                pos = (pos + SPREAD_STEP) & (TABLE_SIZE - 1);
            }
        }
        for t in 0..TABLE_SIZE {
            let s = usize::from(symbol_at[t as usize]);
            let x = next[s];
            next[s] += 1;
            let i = (start[s] + x - norm[s]) as usize;
            if let Some(slot) = state_for.get_mut(i) {
                *slot = t as u16;
            }
        }
    }

    // bitstream: u32 bit length, then the transitions of the last opcode first, the initial state last
    let len_at = out.len();
    out.extend_from_slice(&[0; 4]);
    let mut total_bits = 0u32;
    {
        let mut w = BitWriter { out: &mut out, acc: 0, have: 0 };
        let mut v = TABLE_SIZE - 1;
        for &o in ops.iter().rev() {
            let s = usize::from(o) % OPS;
            let c = norm[s].max(1);
            let y = v + TABLE_SIZE;
            let k = TABLE_LOG - floor_log2(c);
            let nb = if (y >> k) < c { k - 1 } else { k };
            let x = y >> nb;
            w.put(y & ((1u32 << nb) - 1), nb);
            total_bits += nb;
            v = u32::from(state_for.get((start[s] + x - c) as usize).copied().unwrap_or(0));
        }
        w.put(v, TABLE_LOG);
        total_bits += TABLE_LOG;
        w.flush();
    }
    let length_bits = 32 + total_bits;
    out[len_at..len_at + 4].copy_from_slice(&length_bits.to_le_bytes());
    out.truncate(table_bytes + length_bits.div_ceil(8) as usize);
    Some(out)
}

/// The cheapest coding of one channel's opcodes, appended to `out`.
fn write_opcodes(ops: &[u8], stored_only: bool, out: &mut Vec<u8>) {
    let n = ops.len();
    let single = ops.iter().all(|o| Some(o) == ops.first());
    if single && n > 1 {
        out.push(0x01);
        out.push(ops[0]);
        return;
    }
    let fse = if stored_only { None } else { fse_write(ops) };
    if let Some(f) = fse
        && f.len() < 1 + n
    {
        out.extend_from_slice(&f);
        return;
    }
    out.push(0x00);
    out.extend_from_slice(ops);
}
