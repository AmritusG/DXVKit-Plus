//! Rate-distortion optimisation of BC7 blocks for an LZ back end: the "entropy reduction transform"
//! — a faithful Rust port of `ert::reduce_entropy` from bc7enc_rdo's `ert.cpp` (Richard Geldreich,
//! Jr.; MIT OR Unlicense, notice in `LICENSES/bc7enc_rdo.txt`), for 16-byte BC7 blocks with all four
//! channels weighted 1 and without "relative movement" (the configuration LMBD uses).
//!
//! For each block, in order, it tries replacing a run of 3..=16 bytes with the bytes at the same
//! offsets of an earlier (already processed) block inside the look-back window, so the LZ coder
//! finds a match; a candidate is kept when `MSE * smooth_scale + estimated_bits * lambda` drops
//! and its MSE stays within 10x the original's RMS error. A second non-overlapping match is then
//! tried. Floating-point operations follow the original exactly, including the fused
//! multiply-adds clang forms (see `bc7enc.rs`).
//
// Lint note: block and byte indices are bounded by the block count and 16; integer arithmetic is on
// small block offsets (the u32 hash uses explicit wrapping ops, as the original relies on unsigned
// wrap-around). Relaxed for this module only.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::needless_range_loop)]

use crate::tables::{LARGE_DIST_EXTRA, SMALL_DIST_EXTRA};

const BLOCK: usize = 16;
const MIN_MATCH_LEN: usize = 3;
const LITERAL_BITS: f32 = 13.0;
const MATCH_CONTINUE_BITS: f32 = 1.0;
const HASH_SIZE: usize = 8192;

/// ERT settings (the fields of `ert::reduce_entropy_params` the RDO driver sets).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErtParams {
    pub lambda: f32,
    pub lookback_window_size: u32,
    pub max_allowed_rms_increase_ratio: f32,
    pub max_smooth_block_std_dev: f32,
    pub smooth_block_max_mse_scale: f32,
    pub try_two_matches: bool,
}

/// The Deflate-style cost (bits) of a match of `len` bytes at distance `dist`.
fn match_cost(mut dist: u32, len: usize) -> u32 {
    let len_cost = if len >= 12 {
        9
    } else if len >= 8 {
        8
    } else if len >= 6 {
        7
    } else {
        6
    };
    let mut dist_cost = 5u32;
    if dist < 512 {
        dist_cost += u32::from(SMALL_DIST_EXTRA[(dist & 511) as usize]);
    } else {
        dist_cost += u32::from(LARGE_DIST_EXTRA[(dist.min(32767) >> 8) as usize]);
        while dist >= 32768 {
            dist_cost += 1;
            dist >>= 1;
        }
    }
    len_cost + dist_cost
}

/// Paul Hsieh's SuperFastHash, as `ert::hash_hsieh` (little-endian 16-bit reads; the 1- and
/// 3-byte tails read *signed* bytes).
fn hash_hsieh(buf: &[u8], salt: u32) -> u32 {
    let len = buf.len();
    if len == 0 {
        return 0;
    }
    let rd16 = |i: usize| u32::from(u16::from_le_bytes([buf[i], buf[i + 1]]));
    let mut h = (len as u32).wrapping_add(salt << 16);
    let mut p = 0usize;
    for _ in 0..len >> 2 {
        h = h.wrapping_add(rd16(p));
        let t = (rd16(p + 2) << 11) ^ h;
        h = (h << 16) ^ t;
        p += 4;
        h = h.wrapping_add(h >> 11);
    }
    match len & 3 {
        1 => {
            h = h.wrapping_add(i32::from(buf[p] as i8) as u32);
            h ^= h << 10;
            h = h.wrapping_add(h >> 1);
        }
        2 => {
            h = h.wrapping_add(rd16(p));
            h ^= h << 11;
            h = h.wrapping_add(h >> 17);
        }
        3 => {
            h = h.wrapping_add(rd16(p));
            h ^= h << 16;
            h ^= (i32::from(buf[p + 2] as i8) << 18) as u32;
            h = h.wrapping_add(h >> 11);
        }
        _ => {}
    }
    h ^= h << 3;
    h = h.wrapping_add(h >> 5);
    h ^= h << 4;
    h = h.wrapping_add(h >> 17);
    h ^= h << 25;
    h = h.wrapping_add(h >> 6);
    h
}

/// Unpack a BC7 block as `bc7decomp::unpack_bc7` does: `None` for the reserved mode (first byte 0).
/// (bc7decomp and our specification decoder agree on every block: 1 000 000 random blocks of every
/// mode were compared when porting; `bc7dec` is the specification decoder, specialised for speed.)
#[inline]
fn unpack(block: &[u8; BLOCK]) -> Option<[[u8; 4]; 16]> {
    crate::bc7dec::decode(block)
}

/// Mean squared error over 16 RGBA pixels, all channels weighted 1 (`compute_block_mse`).
#[inline]
fn block_mse(a: &[[u8; 4]; 16], b: &[[u8; 4]; 16]) -> f32 {
    let mut total = 0u64;
    for (p, q) in a.iter().zip(b.iter()) {
        for c in 0..4 {
            let d = i32::from(p[c]) - i32::from(q[c]);
            total += (d * d) as u64;
        }
    }
    // total_err * (one_over_total_color_weight / total_block_pixels), in single precision.
    total as f32 * ((1.0f32 / 4.0) / 16.0)
}

/// Largest per-channel standard deviation of a block (four channels; `tracked_stat`).
fn block_max_std_dev(px: &[[u8; 4]; 16]) -> f32 {
    let mut best = 0.0f32;
    for c in 0..4 {
        let (mut total, mut total2) = (0u64, 0u64);
        for p in px {
            let v = u64::from(p[c]);
            total += v;
            total2 += v * v;
        }
        let num = 16u64;
        let sd = ((num * total2 - total * total) as f32).sqrt() / num as f32;
        if sd > best {
            best = sd;
        }
    }
    best
}

/// `ert::reduce_entropy` over `blocks` (modified in place) whose source pixels are `pixels`.
/// `mse_scales[i] > 0` overrides block `i`'s smooth-block scale. Returns the number of blocks
/// changed; stops (as the original does) if a block does not unpack.
pub fn reduce_entropy(blocks: &mut [[u8; BLOCK]], pixels: &[[[u8; 4]; 16]], params: &ErtParams, mse_scales: Option<&[f32]>) -> u32 {
    let total_blocks_to_check = (params.lookback_window_size as usize / BLOCK).max(1);
    let mut prev_cont: i64 = -1;
    let mut hash = vec![0u32; HASH_SIZE];
    let mut modified = 0u32;
    let ratio2 = params.max_allowed_rms_increase_ratio * params.max_allowed_rms_increase_ratio;

    for bi in 0..blocks.len() {
        if bi & 0xFF == 0 {
            hash.fill(0);
        }
        let orig = blocks[bi];
        let px = &pixels[bi];
        let Some(decoded) = unpack(&orig) else {
            return modified;
        };
        let cur_mse = block_mse(px, &decoded);
        let max_std_dev = block_max_std_dev(px);
        let mut yl = clampf(max_std_dev / params.max_smooth_block_std_dev, 0.0, 1.0);
        yl *= yl;
        // lerp(smooth_max, 1.0, yl) = a + (b - a) * s, fused.
        let mut smooth_scale = (1.0 - params.smooth_block_max_mse_scale).mul_add(yl, params.smooth_block_max_mse_scale);
        if let Some(s) = mse_scales.and_then(|m| m.get(bi)).copied()
            && s > 0.0
        {
            smooth_scale = s;
        }
        let cur_bits = LITERAL_BITS * BLOCK as f32;
        let cur_t = cur_mse.mul_add(smooth_scale, cur_bits * params.lambda);

        let first = bi.saturating_sub(total_blocks_to_check);
        let mut best_block = orig;
        let mut best_t = cur_t;
        let (mut best_len, mut best_src_win, mut best_dst_win, mut best_dst_ofs) = (0usize, 0usize, 0usize, 0usize);
        let mut best_bits = 0.0f32;
        // std::max(cur_mse, 1.0f) is `(cur_mse < 1) ? 1 : cur_mse`.
        let thresh = ratio2 * if cur_mse < 1.0 { 1.0 } else { cur_mse };

        for prev in (first..bi).rev() {
            let prev_blk = blocks[prev];
            for len in (MIN_MATCH_LEN..=BLOCK).rev() {
                let dist = ((bi - prev) * BLOCK) as u32;
                let trial_match_bits = match_cost(dist, len) as f32;
                let trial_total_bits = ((BLOCK - len) as f32).mul_add(LITERAL_BITS, trial_match_bits);
                let trial_total_bits_lambda = trial_total_bits * params.lambda;
                for ofs in 0..=(BLOCK - len) {
                    let dst_win = bi * BLOCK + ofs;
                    let src_win = prev * BLOCK + ofs;
                    let mut bits_to_use = trial_match_bits;
                    let mut bits_lambda = trial_total_bits_lambda;
                    let hs = hash_hsieh(&prev_blk[ofs..ofs + len], ofs as u32);
                    if src_win as i64 == prev_cont && ofs == 0 {
                        // Continue the previous block's match across the block boundary.
                        let cont_bits = ((BLOCK - len) as f32).mul_add(LITERAL_BITS, MATCH_CONTINUE_BITS);
                        bits_to_use = MATCH_CONTINUE_BITS;
                        bits_lambda = cont_bits * params.lambda;
                    } else {
                        // (The REP0 branch is compiled out in the original: its distance is never set.)
                        let check = hash[hs as usize & (HASH_SIZE - 1)];
                        if check & 0xFF == (bi as u32 & 0xFF) && check >> 8 == hs >> 8 {
                            continue;
                        }
                    }
                    hash[hs as usize & (HASH_SIZE - 1)] = (hs & 0xFFFF_FF00) | (bi as u32 & 0xFF);
                    let mut trial = orig;
                    trial[ofs..ofs + len].copy_from_slice(&prev_blk[ofs..ofs + len]);
                    let Some(dt) = unpack(&trial) else { continue };
                    let trial_mse = block_mse(px, &dt);
                    if trial_mse < thresh {
                        let t = trial_mse.mul_add(smooth_scale, bits_lambda);
                        if t < best_t {
                            best_t = t;
                            best_block = trial;
                            best_len = len;
                            best_src_win = src_win;
                            best_dst_win = dst_win;
                            best_dst_ofs = ofs;
                            best_bits = bits_to_use;
                        }
                    }
                }
            }
        }

        if best_t < cur_t {
            let (mut second_len, mut second_src_win, mut second_dst_win, mut second_dst_ofs) = (0usize, 0usize, 0usize, 0usize);
            if params.try_two_matches && best_len <= BLOCK - 3 {
                let mut matched = [false; BLOCK];
                matched[best_dst_ofs..best_dst_ofs + best_len].fill(true);
                let orig_best = best_block;
                for prev in (first..bi).rev() {
                    let prev_blk = blocks[prev];
                    let dist = ((bi - prev) * BLOCK) as u32;
                    for len in 3..=(BLOCK - best_len) {
                        let trial_total_bits = ((BLOCK - len - best_len) as f32).mul_add(LITERAL_BITS, match_cost(dist, len) as f32) + best_bits;
                        let bits_lambda = trial_total_bits * params.lambda;
                        for ofs in 0..=(BLOCK - len) {
                            if matched[ofs..ofs + len].iter().any(|&m| m) {
                                continue;
                            }
                            let mut trial = orig_best;
                            trial[ofs..ofs + len].copy_from_slice(&prev_blk[ofs..ofs + len]);
                            let Some(dt) = unpack(&trial) else { continue };
                            let trial_mse = block_mse(px, &dt);
                            if trial_mse < thresh {
                                let t = trial_mse.mul_add(smooth_scale, bits_lambda);
                                if t < best_t {
                                    best_t = t;
                                    best_block = trial;
                                    second_len = len;
                                    second_src_win = prev * BLOCK + ofs;
                                    second_dst_win = bi * BLOCK + ofs;
                                    second_dst_ofs = ofs;
                                }
                            }
                        }
                    }
                }
            }
            blocks[bi] = best_block;
            modified += 1;
            // Remember where a match running to the end of this block could continue.
            if second_len == 0 || best_dst_win > second_dst_win {
                prev_cont = if best_dst_ofs + best_len == BLOCK { (best_src_win + best_len) as i64 } else { -1 };
            } else {
                prev_cont = if second_dst_ofs + second_len == BLOCK { (second_src_win + second_len) as i64 } else { -1 };
            }
        } else {
            prev_cont = -1;
        }
    }
    modified
}

/// `clampf` with the original's comparison semantics.
#[inline]
fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_cost_matches_the_deflate_model() {
        // Distance 16: 5 + 3 extra bits (tdefl small-distance table); a 16-byte match costs 9.
        assert_eq!(match_cost(16, 16), 9 + 5 + 3);
        assert_eq!(match_cost(16384, 3), 6 + 5 + 13);
        assert_eq!(match_cost(65536, 8), 8 + 5 + 13 + 2);
    }

    #[test]
    fn hash_is_salted_and_reads_signed_tails() {
        let a = hash_hsieh(&[1, 2, 3], 0);
        assert_ne!(a, hash_hsieh(&[1, 2, 3], 1));
        assert_ne!(hash_hsieh(&[1, 2, 0x80], 0), hash_hsieh(&[1, 2, 0x7F], 0));
    }
}
