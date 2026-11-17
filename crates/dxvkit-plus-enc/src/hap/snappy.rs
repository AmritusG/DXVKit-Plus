//! Snappy compressor — Swift GlEnc's own `SnappyCompressor.swift` (SPDX-License-Identifier: BSD-3-Clause in
//! GlEnc; this port keeps that licence noted). Not Google's compressor: a greedy LZ77 over a 14-bit hash of
//! each 4-byte key (multiplier `0x1E35A7BD`), one step per miss (no skip heuristic), matches only within
//! 64 KiB, literal / 1-byte-offset / 2-byte-offset tokens. Output is standard Snappy (raw, unframed).

const HASH_BITS: u32 = 14;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_OFFSET: usize = 1 << 16;

/// Compress `input` to one raw Snappy block (varint length preamble + tokens).
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    let n = input.len();
    let mut out = Vec::with_capacity(n + 32 + n / 6);
    let mut v = n as u64;
    while v >= 0x80 {
        out.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    if n == 0 {
        return out;
    }

    // Most recent position + 1 of each hash (0 = unseen; GlEnc stores −1 for unseen).
    let mut table = vec![0u32; HASH_SIZE];
    let mut next_emit = 0usize;
    let mut ip = 0usize;
    while ip + 4 <= n {
        let key = load32(input, ip);
        let h = (key.wrapping_mul(0x1E35_A7BD) >> (32 - HASH_BITS)) as usize;
        let seen = table[h];
        table[h] = (ip + 1) as u32;
        if seen != 0 {
            let candidate = seen as usize - 1;
            if ip - candidate < MAX_OFFSET && load32(input, candidate) == key {
                let mut len = 4usize;
                while ip + len < n && input[candidate + len] == input[ip + len] {
                    len += 1;
                }
                if ip > next_emit {
                    emit_literal(&input[next_emit..ip], &mut out);
                }
                emit_copy(len, ip - candidate, &mut out);
                ip += len;
                next_emit = ip;
                continue;
            }
        }
        ip += 1;
    }
    if next_emit < n {
        emit_literal(&input[next_emit..], &mut out);
    }
    out
}

#[inline(always)]
fn load32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn emit_literal(lit: &[u8], out: &mut Vec<u8>) {
    let n = lit.len() - 1;
    if n < 60 {
        out.push((n << 2) as u8);
    } else if n < 256 {
        out.extend_from_slice(&[0xF0, n as u8]);
    } else if n < 65_536 {
        out.extend_from_slice(&[0xF4, n as u8, (n >> 8) as u8]);
    } else if n < 16_777_216 {
        out.extend_from_slice(&[0xF8, n as u8, (n >> 8) as u8, (n >> 16) as u8]);
    } else {
        out.extend_from_slice(&[0xFC, n as u8, (n >> 8) as u8, (n >> 16) as u8, (n >> 24) as u8]);
    }
    out.extend_from_slice(lit);
}

/// GlEnc's `emitCopy`: 2-byte-offset tokens of up to 64 (60 when 64 would leave 1…3 bytes for a 1-byte-offset
/// token), 1-byte-offset tokens for 4…11 bytes at offsets below 2048.
fn emit_copy(length: usize, offset: usize, out: &mut Vec<u8>) {
    let mut remaining = length;
    while remaining > 0 {
        if remaining >= 12 || offset >= 2048 {
            let chunk = if offset < 2048 && remaining > 64 && remaining < 68 { 60 } else { remaining.min(64) };
            out.extend_from_slice(&[(((chunk - 1) << 2) | 0b10) as u8, offset as u8, (offset >> 8) as u8]);
            remaining -= chunk;
        } else {
            let chunk = remaining.min(11);
            out.extend_from_slice(&[(0b01 | ((chunk - 4) << 2) | (((offset >> 8) & 7) << 5)) as u8, offset as u8]);
            remaining -= chunk;
        }
    }
}
