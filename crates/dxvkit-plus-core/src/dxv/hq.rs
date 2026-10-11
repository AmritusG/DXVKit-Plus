//! The DXV3 High Quality layer (YCG6, YG10): plane sections, opcode streams (stored, fill,
//! FSE / tANS) and block reconstruction from opcodes (spec 7). Port of DXVKit `src/hq.c` (MIT; see
//! `super`), which follows the spec and the public FSE descriptions (Yann Collet's FSE
//! documentation, RFC 8878 section 4.1) for the table-driven decoding.
//!
//! Every offset is into the payload or the plane and every access is bounds-checked; positions that
//! the C code holds as pointers are `usize` offsets here.

use super::{DxvError, Layout, Variant};
use crate::bytes::{arr, byte, le16, le24, le32, put};

const TABLE_LOG: u32 = 10;
const TABLE_SIZE: usize = 1 << TABLE_LOG;
const SPREAD_STEP: usize = 641;
const OPCODE_LIMIT: u32 = 18;

#[derive(Clone, Copy, Default)]
struct FseEntry {
    baseline: u16,
    symbol: u8,
    bits: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The opcodes are stored at this payload offset.
    Stored(usize),
    Fill(u8),
    Fse,
}

struct OpcodeSource {
    mode: Mode,
    /// Opcodes in the stream.
    count: u32,
    /// Opcodes handed out so far.
    taken: u32,
    /// FSE: the bitstream area (payload offset, length).
    area: usize,
    area_bytes: usize,
    /// Bits not yet consumed: the next bit read is `bit_pos - 1`.
    bit_pos: u64,
    state: u32,
    table: Vec<FseEntry>,
}

impl OpcodeSource {
    fn new() -> Self {
        OpcodeSource { mode: Mode::Fill(0), count: 0, taken: 0, area: 0, area_bytes: 0, bit_pos: 0, state: 0, table: Vec::new() }
    }

    /// Reads `n` (0..=16) bits ending just below `bit_pos`, most significant first.
    #[inline]
    fn bits(&mut self, payload: &[u8], n: u32) -> Option<u32> {
        if self.bit_pos < u64::from(n) {
            return None;
        }
        self.bit_pos = self.bit_pos.saturating_sub(u64::from(n));
        let byte_at = usize::try_from(self.bit_pos >> 3).ok()?;
        let shift = (self.bit_pos & 7) as u32;
        let v = if byte_at.checked_add(4)? <= self.area_bytes {
            le32(payload, self.area.checked_add(byte_at)?)?
        } else {
            let mut v = 0u32;
            for i in 0..4usize {
                let k = byte_at.checked_add(i)?;
                if k >= self.area_bytes {
                    break;
                }
                let b = byte(payload, self.area.checked_add(k)?)?;
                v |= u32::from(b) << (8u32.saturating_mul(i as u32));
            }
            v
        };
        let mask = if n == 0 { 0 } else { u32::MAX >> 32u32.saturating_sub(n) };
        Some((v >> shift) & mask)
    }

    /// Frequency table (spec 7.3.1) + decoding table (7.3.2) + bitstream setup (7.3.3). `p` is the
    /// stream start; returns the stream end.
    fn fse_setup(&mut self, payload: &[u8], p: usize) -> Result<usize, DxvError> {
        let end = payload.len();
        let mut counts = [0u16; 256];
        let mut next = [0u16; 256];
        let mut symbol_at = [0u8; TABLE_SIZE];
        let mut width = TABLE_LOG;
        let mut symbols = 0usize;
        let mut remaining = TABLE_SIZE as u32;
        let mut acc = u64::from(le32(payload, p).ok_or(DxvError::FseTable)? >> 2);
        let mut have = 30u32;
        let mut q = p.checked_add(4).ok_or(DxvError::FseTable)?;
        while remaining > 0 {
            if symbols == 256 {
                return Err(DxvError::FseTable);
            }
            if have < width {
                let w = le16(payload, q).ok_or(DxvError::FseTable)?;
                acc |= u64::from(w) << have;
                have = have.saturating_add(16);
                q = q.saturating_add(2);
            }
            let c = (acc & (1u64 << width).wrapping_sub(1)) as u32;
            acc >>= width;
            have = have.saturating_sub(width);
            if c > remaining {
                return Err(DxvError::FseTable);
            }
            *counts.get_mut(symbols).ok_or(DxvError::FseTable)? = c as u16;
            symbols = symbols.saturating_add(1);
            remaining = remaining.saturating_sub(c);
            if width > 0 && remaining < (1u32 << width.saturating_sub(1)) {
                width = width.saturating_sub(1);
            }
        }
        // spread
        let mut pos = 0usize;
        for sym in 0..symbols {
            let n = counts.get(sym).copied().unwrap_or(0);
            for _ in 0..n {
                *symbol_at.get_mut(pos).ok_or(DxvError::FseTable)? = sym as u8;
                pos = pos.wrapping_add(SPREAD_STEP) & (TABLE_SIZE - 1);
            }
            if let Some(x) = next.get_mut(sym) {
                *x = n;
            }
        }
        self.table.clear();
        self.table.reserve(TABLE_SIZE);
        for &sym in &symbol_at {
            let slot = next.get_mut(usize::from(sym)).ok_or(DxvError::FseTable)?;
            let x = u32::from(*slot);
            *slot = slot.saturating_add(1);
            if x == 0 {
                // Unreachable for a table that sums to TABLE_SIZE (every state's symbol has a count
                // >= 1); kept as an error instead of an assumption.
                return Err(DxvError::FseTable);
            }
            let log2x = 31u32.saturating_sub(x.leading_zeros());
            let bits = TABLE_LOG.saturating_sub(log2x);
            let baseline = (x << bits).saturating_sub(TABLE_SIZE as u32);
            self.table.push(FseEntry { baseline: baseline as u16, symbol: sym, bits: bits as u8 });
        }
        // bitstream area: starts at the u32 bit length, ceil(L / 8) bytes
        let length_bits = u64::from(le32(payload, q).ok_or(DxvError::FseStream)?);
        let area_bytes = length_bits.saturating_add(7) >> 3;
        let avail = end.saturating_sub(q) as u64;
        if area_bytes <= 4 || area_bytes > avail {
            return Err(DxvError::FseStream);
        }
        self.area = q;
        self.area_bytes = usize::try_from(area_bytes).map_err(|_| DxvError::FseStream)?;
        self.bit_pos = length_bits;
        self.state = self.bits(payload, TABLE_LOG).ok_or(DxvError::FseStream)?;
        Ok(q.saturating_add(self.area_bytes))
    }

    /// Sets up the opcode stream starting at `p` (spec 7.3). Returns the stream end.
    fn setup(&mut self, payload: &[u8], count: u32, p: usize) -> Result<usize, DxvError> {
        self.count = count;
        self.taken = 0;
        let end = payload.len();
        let sel = byte(payload, p).ok_or(DxvError::PayloadEnd)? & 3;
        match sel {
            0 => {
                if end.saturating_sub(p).saturating_sub(1) < count as usize {
                    return Err(DxvError::PayloadEnd);
                }
                self.mode = Mode::Stored(p.saturating_add(1));
                Ok(p.saturating_add(1).saturating_add(count as usize))
            }
            1 => {
                let fill = byte(payload, p.saturating_add(1)).ok_or(DxvError::PayloadEnd)?;
                self.mode = Mode::Fill(fill);
                Ok(p.saturating_add(2))
            }
            _ => {
                self.mode = Mode::Fse;
                self.fse_setup(payload, p)
            }
        }
    }

    #[inline]
    fn next(&mut self, payload: &[u8]) -> Result<u32, DxvError> {
        if self.taken >= self.count {
            return Err(DxvError::HqOpcodesUsedUp);
        }
        let index = self.taken as usize;
        self.taken = self.taken.saturating_add(1);
        match self.mode {
            Mode::Stored(at) => Ok(u32::from(byte(payload, at.saturating_add(index)).ok_or(DxvError::PayloadEnd)?)),
            Mode::Fill(op) => Ok(u32::from(op)),
            Mode::Fse => {
                let e = self.table.get(self.state as usize).copied().ok_or(DxvError::FseStream)?;
                let bits = self.bits(payload, u32::from(e.bits)).ok_or(DxvError::FseStream)?;
                self.state = u32::from(e.baseline).saturating_add(bits);
                Ok(u32::from(e.symbol))
            }
        }
    }
}

const SLOT_FULL_E: u32 = 0x1_0000;
const SLOT_FULL_I: u32 = 0x100_0000;

/// Literal bytes each opcode reads before any count extension (spec 7.4.3).
const LITERAL_BYTES: [usize; OPCODE_LIMIT as usize] = [1, 0, 2, 8, 6, 6, 4, 4, 7, 5, 5, 3, 3, 6, 4, 4, 2, 2];

struct Channel {
    ops: OpcodeSource,
    /// This channel's byte offset of block 0 inside its plane.
    base: usize,
    repeat: usize,
    dict_e: [u32; 256],
    dict_i: [u32; 256],
}

#[inline]
fn slot_of(v: u32) -> usize {
    (v.wrapping_mul(2_654_435_761) >> 24) as usize
}

impl Channel {
    fn new(base: usize) -> Self {
        Channel { ops: OpcodeSource::new(), base, repeat: 0, dict_e: [0; 256], dict_i: [0; 256] }
    }
    #[inline]
    fn insert_e(&mut self, plane: &[u8], blk: usize) -> Option<()> {
        let e = le16(plane, blk)?;
        *self.dict_e.get_mut(slot_of(e))? = e | SLOT_FULL_E;
        Some(())
    }
    #[inline]
    fn insert_i(&mut self, plane: &[u8], blk: usize) -> Option<()> {
        let i = le24(plane, blk.checked_add(2)?)?;
        *self.dict_i.get_mut(slot_of(i))? = i | SLOT_FULL_I;
        Some(())
    }
    #[inline]
    fn lookup_e(&self, slot: u8) -> Result<u32, DxvError> {
        let v = self.dict_e.get(usize::from(slot)).copied().unwrap_or(0);
        if v & SLOT_FULL_E == 0 {
            return Err(DxvError::HqEmptySlot);
        }
        Ok(v & 0xFFFF)
    }
    #[inline]
    fn lookup_i(&self, slot: u8) -> Result<u32, DxvError> {
        let v = self.dict_i.get(usize::from(slot)).copied().unwrap_or(0);
        if v & SLOT_FULL_I == 0 {
            return Err(DxvError::HqEmptySlot);
        }
        Ok(v & 0xFF_FFFF)
    }
}

/// Literal cursor over `[pos, end)` of the payload.
struct Lit {
    pos: usize,
    end: usize,
}

impl Lit {
    #[inline]
    fn left(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }
}

#[inline]
fn lit_u8(l: &[u8], i: usize) -> Result<u8, DxvError> {
    byte(l, i).ok_or(DxvError::HqLiterals)
}

#[inline]
fn lit_u16(l: &[u8], i: usize) -> Result<usize, DxvError> {
    le16(l, i).map(|v| v as usize).ok_or(DxvError::HqLiterals)
}

#[inline]
fn three(v: u32) -> [u8; 3] {
    let [a, b, c, _] = v.to_le_bytes();
    [a, b, c]
}

/// Block `index` (>= 1) of one channel (spec 7.4.3).
#[inline]
fn reconstruct(c: &mut Channel, payload: &[u8], lit: &mut Lit, plane: &mut [u8], index: usize, stride: usize) -> Result<(), DxvError> {
    const OOB: DxvError = DxvError::BufferTooSmall;
    let dst = index.checked_mul(stride).and_then(|o| o.checked_add(c.base)).ok_or(OOB)?;
    let prev = dst.checked_sub(stride).ok_or(OOB)?;
    let prev_block: [u8; 8] = arr(plane, prev).ok_or(OOB)?;
    if c.repeat > 0 {
        c.repeat = c.repeat.saturating_sub(1);
        return put(plane, dst, &prev_block).ok_or(OOB);
    }
    let op = c.ops.next(payload)?;
    if op >= OPCODE_LIMIT {
        return Err(DxvError::HqBadOpcode);
    }
    let n = LITERAL_BYTES.get(op as usize).copied().ok_or(DxvError::HqBadOpcode)?;
    if lit.left() < n {
        return Err(DxvError::HqLiterals);
    }
    let l = payload.get(lit.pos..lit.pos.saturating_add(n)).ok_or(DxvError::HqLiterals)?;
    lit.pos = lit.pos.saturating_add(n);
    // The block to write, assembled in a local and stored once.
    let back = |u16v: usize| -> Result<[u8; 8], DxvError> {
        let d = u16v.saturating_add(1);
        if d > index {
            return Err(DxvError::BadDistance);
        }
        let r = dst.checked_sub(d.checked_mul(stride).ok_or(DxvError::BadDistance)?).ok_or(DxvError::BadDistance)?;
        arr(plane, r).ok_or(OOB)
    };
    let mut blk = [0u8; 8];
    // (insert E, insert I) after writing.
    let (ins_e, ins_i);
    match op {
        0 => {
            let mut count = usize::from(lit_u8(l, 0)?);
            if count == 255 {
                loop {
                    let v = le16(payload, lit.pos).filter(|_| lit.left() >= 2).ok_or(DxvError::HqLiterals)? as usize;
                    lit.pos = lit.pos.saturating_add(2);
                    count = count.saturating_add(v);
                    if v != 0xFFFF {
                        break;
                    }
                }
            }
            c.repeat = count.saturating_add(3);
            return put(plane, dst, &prev_block).ok_or(OOB);
        }
        1 => return put(plane, dst, &prev_block).ok_or(OOB),
        2 => {
            blk = back(lit_u16(l, 0)?)?;
            (ins_e, ins_i) = (true, true);
        }
        3 => {
            blk.copy_from_slice(l.get(..8).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (true, true);
        }
        4 => {
            let g1 = c.lookup_i(lit_u8(l, 0)?)?;
            blk[..2].copy_from_slice(l.get(1..3).ok_or(DxvError::HqLiterals)?);
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(l.get(3..6).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (true, false);
        }
        5 => {
            let g2 = c.lookup_i(lit_u8(l, 0)?)?;
            blk[..5].copy_from_slice(l.get(1..6).ok_or(DxvError::HqLiterals)?);
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (true, true);
        }
        6 => {
            let g1 = c.lookup_i(lit_u8(l, 0)?)?;
            let g2 = c.lookup_i(lit_u8(l, 1)?)?;
            blk[..2].copy_from_slice(l.get(2..4).ok_or(DxvError::HqLiterals)?);
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (true, false);
        }
        7 => {
            let r = back(lit_u16(l, 0)?)?;
            blk[..2].copy_from_slice(l.get(2..4).ok_or(DxvError::HqLiterals)?);
            blk[2..8].copy_from_slice(&r[2..8]);
            (ins_e, ins_i) = (true, true);
        }
        8 => {
            let e = c.lookup_e(lit_u8(l, 0)?)?;
            blk[..2].copy_from_slice(&(e as u16).to_le_bytes());
            blk[2..8].copy_from_slice(l.get(1..7).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (false, true);
        }
        9 => {
            let e = c.lookup_e(lit_u8(l, 0)?)?;
            let g1 = c.lookup_i(lit_u8(l, 1)?)?;
            blk[..2].copy_from_slice(&(e as u16).to_le_bytes());
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(l.get(2..5).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (false, true);
        }
        10 => {
            let e = c.lookup_e(lit_u8(l, 0)?)?;
            let g2 = c.lookup_i(lit_u8(l, 1)?)?;
            blk[..2].copy_from_slice(&(e as u16).to_le_bytes());
            blk[2..5].copy_from_slice(l.get(2..5).ok_or(DxvError::HqLiterals)?);
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (false, true);
        }
        11 => {
            let e = c.lookup_e(lit_u8(l, 0)?)?;
            let g1 = c.lookup_i(lit_u8(l, 1)?)?;
            let g2 = c.lookup_i(lit_u8(l, 2)?)?;
            blk[..2].copy_from_slice(&(e as u16).to_le_bytes());
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (false, false);
        }
        12 => {
            let e = c.lookup_e(lit_u8(l, 0)?)?;
            let r = back(lit_u16(l, 1)?)?;
            blk[..2].copy_from_slice(&(e as u16).to_le_bytes());
            blk[2..8].copy_from_slice(&r[2..8]);
            (ins_e, ins_i) = (false, true);
        }
        13 => {
            blk[..2].copy_from_slice(&prev_block[..2]);
            blk[2..8].copy_from_slice(l.get(..6).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (false, true);
        }
        14 => {
            let g1 = c.lookup_i(lit_u8(l, 0)?)?;
            blk[..2].copy_from_slice(&prev_block[..2]);
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(l.get(1..4).ok_or(DxvError::HqLiterals)?);
            (ins_e, ins_i) = (false, true);
        }
        15 => {
            let g2 = c.lookup_i(lit_u8(l, 0)?)?;
            blk[..2].copy_from_slice(&prev_block[..2]);
            blk[2..5].copy_from_slice(l.get(1..4).ok_or(DxvError::HqLiterals)?);
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (false, true);
        }
        16 => {
            let g1 = c.lookup_i(lit_u8(l, 0)?)?;
            let g2 = c.lookup_i(lit_u8(l, 1)?)?;
            blk[..2].copy_from_slice(&prev_block[..2]);
            blk[2..5].copy_from_slice(&three(g1));
            blk[5..8].copy_from_slice(&three(g2));
            (ins_e, ins_i) = (false, false);
        }
        _ => {
            // 17
            let r = back(lit_u16(l, 0)?)?;
            blk[..2].copy_from_slice(&prev_block[..2]);
            blk[2..8].copy_from_slice(&r[2..8]);
            (ins_e, ins_i) = (false, true);
        }
    }
    put(plane, dst, &blk).ok_or(OOB)?;
    if ins_e {
        c.insert_e(plane, dst).ok_or(OOB)?;
    }
    if ins_i {
        c.insert_i(plane, dst).ok_or(OOB)?;
    }
    Ok(())
}

struct Section {
    /// 1 or 2.
    channels: usize,
    /// Blocks per channel.
    blocks: usize,
    /// Bytes per block position: 8 or 16.
    stride: usize,
    lit: Lit,
    /// End of the section (after its last opcode stream).
    end: usize,
    ch: [Channel; 2],
}

impl Section {
    fn new(channels: usize, blocks: usize, stride: usize) -> Self {
        Section { channels, blocks, stride, lit: Lit { pos: 0, end: 0 }, end: 0, ch: [Channel::new(0), Channel::new(8)] }
    }

    /// Reads a section's header and locates its opcode streams (spec 7.2).
    fn locate(&mut self, payload: &[u8], start: usize) -> Result<(), DxvError> {
        let header = if self.channels == 1 { 8 } else { 12 };
        let avail = payload.len().saturating_sub(start);
        if avail < header {
            return Err(DxvError::PayloadEnd);
        }
        let op_offset = le32(payload, start).ok_or(DxvError::PayloadEnd)? as usize;
        if op_offset < header || op_offset > avail {
            return Err(DxvError::HqSection);
        }
        self.lit = Lit { pos: start.saturating_add(header), end: start.saturating_add(op_offset) };
        let mut p = start.saturating_add(op_offset);
        for i in 0..self.channels {
            let count = le32(payload, start.saturating_add(4).saturating_add(4usize.saturating_mul(i))).ok_or(DxvError::PayloadEnd)?;
            if count as usize > self.blocks {
                return Err(DxvError::HqOpcodeCount);
            }
            let ch = self.ch.get_mut(i).ok_or(DxvError::Argument)?;
            p = ch.ops.setup(payload, count, p)?;
        }
        self.end = p;
        Ok(())
    }

    fn decode(&mut self, payload: &[u8], plane: &mut [u8]) -> Result<(), DxvError> {
        let first = 8usize.saturating_mul(self.channels);
        if self.lit.left() < first {
            return Err(DxvError::HqLiterals);
        }
        for i in 0..self.channels {
            let src: [u8; 8] = arr(payload, self.lit.pos.saturating_add(8usize.saturating_mul(i))).ok_or(DxvError::HqLiterals)?;
            let c = self.ch.get_mut(i).ok_or(DxvError::Argument)?;
            put(plane, c.base, &src).ok_or(DxvError::BufferTooSmall)?;
            c.insert_e(plane, c.base).ok_or(DxvError::BufferTooSmall)?;
            c.insert_i(plane, c.base).ok_or(DxvError::BufferTooSmall)?;
        }
        self.lit.pos = self.lit.pos.saturating_add(first);
        let (stride, blocks) = (self.stride, self.blocks);
        let [c0, c1] = &mut self.ch;
        if self.channels == 1 {
            for b in 1..blocks {
                reconstruct(c0, payload, &mut self.lit, plane, b, stride)?;
            }
        } else {
            for b in 1..blocks {
                reconstruct(c0, payload, &mut self.lit, plane, b, stride)?;
                reconstruct(c1, payload, &mut self.lit, plane, b, stride)?;
            }
        }
        // Decode (and discard) opcodes the texture did not need, so a malformed tail is still caught.
        for c in self.ch.iter_mut().take(self.channels) {
            while c.ops.taken < c.ops.count {
                c.ops.next(payload)?;
            }
        }
        Ok(())
    }
}

/// Decode both HQ sections. Returns the payload bytes after the last opcode stream (unused).
pub(super) fn decode(payload: &[u8], layout: &Layout, plane0: &mut [u8], plane1: &mut [u8], parallel: bool) -> Result<usize, DxvError> {
    let alpha = layout.variant == Variant::Yg10;
    // section 1: Y (one channel) or Y + A (two channels); section 2: Co + Cg
    let mut s0 = Section::new(if alpha { 2 } else { 1 }, layout.blocks(0), if alpha { 16 } else { 8 });
    let mut s1 = Section::new(2, layout.blocks(1), 16);
    s0.locate(payload, 0)?;
    s1.locate(payload, s0.end)?;
    if parallel {
        let (r0, r1) = std::thread::scope(|scope| {
            let h = scope.spawn(|| s1.decode(payload, plane1));
            let r0 = s0.decode(payload, plane0);
            // A panicking section thread is reported as an error, never propagated.
            let r1 = h.join().unwrap_or(Err(DxvError::Argument));
            (r0, r1)
        });
        r0?;
        r1?;
    } else {
        s0.decode(payload, plane0)?;
        s1.decode(payload, plane1)?;
    }
    Ok(payload.len().saturating_sub(s1.end))
}
