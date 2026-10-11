//! The DXV3 block-copy layers for DXT1 (BC1) and DXT5 (BC3) textures (spec 5 and 6).
//! Port of DXVKit `src/blockcopy.c` (MIT; see `super`).

use super::DxvError;
use crate::bytes::{byte, copy_within, le16, le32, put};

/// Flag reader: 2-bit flags, 16 per little-endian u32, least significant first, interleaved with the
/// bytes they own.
struct FlagReader<'a> {
    input: &'a [u8],
    pos: usize,
    word: u32,
    left: u32,
}

impl<'a> FlagReader<'a> {
    fn new(input: &'a [u8], pos: usize) -> Self {
        FlagReader { input, pos, word: 0, left: 0 }
    }
    #[inline]
    fn take_flag(&mut self) -> Option<u32> {
        if self.left == 0 {
            self.word = le32(self.input, self.pos)?;
            self.pos = self.pos.checked_add(4)?;
            self.left = 16;
        }
        let flag = self.word & 3;
        self.word >>= 2;
        self.left = self.left.saturating_sub(1);
        Some(flag)
    }
    #[inline]
    fn u8(&mut self) -> Option<usize> {
        let v = byte(self.input, self.pos)?;
        self.pos = self.pos.checked_add(1)?;
        Some(usize::from(v))
    }
    #[inline]
    fn u16(&mut self) -> Option<usize> {
        let v = le16(self.input, self.pos)?;
        self.pos = self.pos.checked_add(2)?;
        Some(v as usize)
    }
    /// `n` literal bytes.
    #[inline]
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.input.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    /// Distance for a non-zero flag (spec 5.2): 1; u8 + 2; u16 + 258.
    #[inline]
    fn distance(&mut self, flag: u32) -> Option<usize> {
        match flag {
            1 => Some(1),
            2 => self.u8()?.checked_add(2),
            _ => self.u16()?.checked_add(258),
        }
    }
}

/// One 4-byte unit (a DXT1 half or a DXT5 colour quarter) at byte `dst` of `out`: copy from
/// `stride`-byte blocks back, or 4 literal bytes. `index` is the block the unit belongs to.
#[inline]
fn unit4(r: &mut FlagReader<'_>, out: &mut [u8], dst: usize, index: usize, stride: usize) -> Result<(), DxvError> {
    let flag = r.take_flag().ok_or(DxvError::PayloadEnd)?;
    if flag == 0 {
        let lit = r.bytes(4).ok_or(DxvError::PayloadEnd)?;
        return put(out, dst, lit).ok_or(DxvError::BufferTooSmall);
    }
    let d = r.distance(flag).ok_or(DxvError::PayloadEnd)?;
    if d > index {
        return Err(DxvError::BadDistance);
    }
    let src = dst.checked_sub(d.checked_mul(stride).ok_or(DxvError::BadDistance)?).ok_or(DxvError::BadDistance)?;
    copy_within(out, src, dst, 4).ok_or(DxvError::BufferTooSmall)
}

fn check_out(out: &[u8], blocks: usize, block_bytes: usize) -> Result<(), DxvError> {
    match blocks.checked_mul(block_bytes) {
        Some(n) if n <= out.len() && blocks > 0 => Ok(()),
        _ => Err(DxvError::BufferTooSmall),
    }
}

/// DXT1 (spec 5). Returns the payload bytes used.
pub(super) fn decode_dxt1(payload: &[u8], out: &mut [u8], blocks: usize) -> Result<usize, DxvError> {
    check_out(out, blocks, 8)?;
    let first = payload.get(..8).ok_or(DxvError::PayloadEnd)?;
    put(out, 0, first).ok_or(DxvError::BufferTooSmall)?;
    let mut r = FlagReader::new(payload, 8);
    for b in 1..blocks {
        // b < blocks and blocks * 8 <= out.len(): no overflow.
        let dst = b.saturating_mul(8);
        let flag = r.take_flag().ok_or(DxvError::PayloadEnd)?;
        if flag != 0 {
            let d = r.distance(flag).ok_or(DxvError::PayloadEnd)?;
            if d > b {
                return Err(DxvError::BadDistance);
            }
            let src = dst.saturating_sub(d.saturating_mul(8));
            copy_within(out, src, dst, 8).ok_or(DxvError::BufferTooSmall)?;
            continue;
        }
        unit4(&mut r, out, dst, b, 8)?;
        unit4(&mut r, out, dst.saturating_add(4), b, 8)?;
    }
    Ok(r.pos)
}

/// Count extension (spec 6.4): add u16 values while the one just read is 0xFFFF.
fn extend_count(r: &mut FlagReader<'_>, count: &mut usize) -> Option<()> {
    loop {
        let v = r.u16()?;
        *count = count.checked_add(v)?;
        if v != 0xFFFF {
            return Some(());
        }
    }
}

/// DXT5 (spec 6). Returns the payload bytes used.
pub(super) fn decode_dxt5(payload: &[u8], out: &mut [u8], blocks: usize) -> Result<usize, DxvError> {
    check_out(out, blocks, 16)?;
    let first = payload.get(..16).ok_or(DxvError::PayloadEnd)?;
    put(out, 0, first).ok_or(DxvError::BufferTooSmall)?;
    let mut r = FlagReader::new(payload, 16);
    let mut alpha_run = 0usize;
    let mut b = 1usize;
    while b < blocks {
        let dst = b.saturating_mul(16);
        // alpha half (bytes 0..7)
        if alpha_run > 0 {
            alpha_run = alpha_run.saturating_sub(1);
            copy_within(out, dst.saturating_sub(16), dst, 8).ok_or(DxvError::BufferTooSmall)?;
        } else {
            let flag = r.take_flag().ok_or(DxvError::PayloadEnd)?;
            match flag {
                0 => {
                    // block run: n whole blocks equal to their previous block
                    let mut n = r.u8().ok_or(DxvError::PayloadEnd)?.saturating_add(1);
                    if n == 256 {
                        extend_count(&mut r, &mut n).ok_or(DxvError::PayloadEnd)?;
                    }
                    n = n.min(blocks.saturating_sub(b));
                    for _ in 0..n {
                        let d = b.saturating_mul(16);
                        copy_within(out, d.saturating_sub(16), d, 16).ok_or(DxvError::BufferTooSmall)?;
                        b = b.saturating_add(1);
                    }
                    continue;
                }
                1 => {
                    // alpha run
                    alpha_run = r.u8().ok_or(DxvError::PayloadEnd)?;
                    if alpha_run == 255 {
                        extend_count(&mut r, &mut alpha_run).ok_or(DxvError::PayloadEnd)?;
                    }
                    copy_within(out, dst.saturating_sub(16), dst, 8).ok_or(DxvError::BufferTooSmall)?;
                }
                2 => {
                    // alpha half from u16 + 2 blocks back
                    let d = r.u16().ok_or(DxvError::PayloadEnd)?.saturating_add(2);
                    if d > b {
                        return Err(DxvError::BadDistance);
                    }
                    copy_within(out, dst.saturating_sub(d.saturating_mul(16)), dst, 8).ok_or(DxvError::BufferTooSmall)?;
                }
                _ => {
                    // 8 literal bytes
                    let lit = r.bytes(8).ok_or(DxvError::PayloadEnd)?;
                    put(out, dst, lit).ok_or(DxvError::BufferTooSmall)?;
                }
            }
        }
        // colour half (bytes 8..15)
        let flag = r.take_flag().ok_or(DxvError::PayloadEnd)?;
        let c = dst.saturating_add(8);
        if flag != 0 {
            let d = r.distance(flag).ok_or(DxvError::PayloadEnd)?;
            if d > b {
                return Err(DxvError::BadDistance);
            }
            copy_within(out, c.saturating_sub(d.saturating_mul(16)), c, 8).ok_or(DxvError::BufferTooSmall)?;
        } else {
            unit4(&mut r, out, c, b, 16)?;
            unit4(&mut r, out, c.saturating_add(4), b, 16)?;
        }
        b = b.saturating_add(1);
    }
    Ok(r.pos)
}
