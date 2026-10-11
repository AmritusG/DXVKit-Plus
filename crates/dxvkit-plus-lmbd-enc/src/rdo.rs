//! One image to BC7 blocks with rate-distortion optimisation — the BC7 path of bc7enc_rdo's
//! `rdo_bc::rdo_bc_encoder` (Richard Geldreich, Jr.; MIT OR Unlicense, notice in
//! `LICENSES/bc7enc_rdo.txt`), ported faithfully:
//!
//! 1. pad the image to whole 4x4 blocks by repeating its last column and row;
//! 2. encode every block with the 4-mode encoder ([`crate::bc7enc`]) — blocks are independent, so
//!    this runs on any number of threads with identical output;
//! 3. if `lambda > 0`: mark "ultrasmooth" regions (flat, mid-luma areas of at least 64 blocks) whose
//!    error must stay low, then run the entropy-reduction pass ([`crate::ert`]) over the blocks in
//!    raster order. That pass is sequential; the original splits it into one independent run per
//!    OpenMP thread when it has more than one, which changes the output. The original LMBD build
//!    has no OpenMP (one run over the whole image), and so does the default here
//!    ([`RdoParams::partitions`] = 1). More partitions are faster and match an OpenMP build with
//!    that many threads.
//
// Lint note: block grids are bounded by MAX_DIMENSION / 4 and indices come from them; relaxed for
// this module only.
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::needless_range_loop)]

use crate::bc7enc::{Bc7Params, compress_block};
use crate::ert::{ErtParams, reduce_entropy};

/// Rate-distortion settings (the BC7 fields of `rdo_bc_params`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RdoParams {
    /// Lagrangian multiplier: 0 = no RDO (best quality, largest); higher = smaller, lower quality.
    pub lambda: f32,
    /// How far back (bytes) the RDO pass looks for matches (bc7enc `-zc`; LMBD uses 16384).
    pub lookback: u32,
    /// Independent RDO runs (1 = the original LMBD encoder exactly; see the module notes).
    pub partitions: u32,
    /// Override of the automatic smooth-block error scale (bc7enc `-zb`).
    pub smooth_block_error_scale: Option<f32>,
    pub max_smooth_block_std_dev: f32,
    pub try_two_matches: bool,
    pub ultrasmooth_block_handling: bool,
}

impl Default for RdoParams {
    fn default() -> Self {
        RdoParams {
            lambda: 1.0,
            lookback: 16384,
            partitions: 1,
            smooth_block_error_scale: None,
            max_smooth_block_std_dev: 18.0,
            try_two_matches: true,
            ultrasmooth_block_handling: true,
        }
    }
}

/// The source image as 4x4 blocks of RGBA pixels, padded by edge repetition (`crop_dup_borders`).
pub fn image_blocks(rgba: &[u8], width: u32, height: u32) -> Vec<[[u8; 4]; 16]> {
    let (w, h) = (width as usize, height as usize);
    let (bx, by) = (w.div_ceil(4), h.div_ceil(4));
    let mut out = vec![[[0u8; 4]; 16]; bx * by];
    for (b, blk) in out.iter_mut().enumerate() {
        let (x0, y0) = ((b % bx) * 4, (b / bx) * 4);
        for (i, p) in blk.iter_mut().enumerate() {
            let x = (x0 + i % 4).min(w - 1);
            let y = (y0 + i / 4).min(h - 1);
            let o = (y * w + x) * 4;
            p.copy_from_slice(&rgba[o..o + 4]);
        }
    }
    out
}

/// Luma used for the smooth-block mask (`color_quad_u8::get_luma`, Rec. 709 weights).
#[inline]
fn luma(p: &[u8; 4]) -> u32 {
    (13938 * u32::from(p[0]) + 46869 * u32::from(p[1]) + 4729 * u32::from(p[2]) + 32768) >> 16
}

/// `tracked_stat::get_std_dev` of 16 values.
#[inline]
fn std_dev16(total: u64, total2: u64) -> f32 {
    ((16 * total2 - total * total) as f32).sqrt() / 16.0
}

/// `compute_block_mse_scales`: 120 for blocks in large ultrasmooth regions, -1 (no override)
/// elsewhere.
pub fn block_mse_scales(blocks: &[[[u8; 4]; 16]], bx: usize, by: usize) -> Vec<f32> {
    const ULTRASMOOTH_BLOCK_STD_DEV_THRESHOLD: f32 = 2.9;
    const DARK_THRESHOLD: f32 = 13.0;
    const BRIGHT_THRESHOLD: f32 = 222.0;
    const ULTRASMOOTH_BLOCK_MSE_SCALE: f32 = 120.0;
    const REGION_TOO_SMALL: usize = 64;

    // A grey image, one pixel per block: 255 = not smooth, smaller = smoother (0 = ultrasmooth).
    let mut vis = vec![0u8; bx * by];
    for (i, blk) in blocks.iter().enumerate() {
        let yt: u64 = blk.iter().map(|p| u64::from(luma(p))).sum();
        let mut sd_max = 0.0f32;
        for c in 0..3 {
            let (mut t, mut t2) = (0u64, 0u64);
            for p in blk {
                let v = u64::from(p[c]);
                t += v;
                t2 += v * v;
            }
            let sd = std_dev16(t, t2);
            sd_max = if sd_max < sd { sd } else { sd_max };
        }
        let mut yl = sd_max / ULTRASMOOTH_BLOCK_STD_DEV_THRESHOLD;
        // (sd is a square root of a non-negative value, never NaN: clamp is the original's clamp.)
        yl = yl.clamp(0.0, 1.0);
        yl *= yl;
        let y_avg = yt as f32 / 16.0;
        if !(DARK_THRESHOLD..BRIGHT_THRESHOLD).contains(&y_avg) {
            yl = 1.0;
        }
        vis[i] = (yl.mul_add(255.0, 0.5) as i32).min(255) as u8;
    }
    let at = |v: &[u8], x: i64, y: i64| v[(y.clamp(0, by as i64 - 1) as usize) * bx + x.clamp(0, bx as i64 - 1) as usize];

    // Dilate the non-smooth (255) blocks by one.
    let mut next = vis.clone();
    for y in 0..by as i64 {
        for x in 0..bx as i64 {
            let mut m = 0u8;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if at(&vis, x + dx, y + dy) == 255 {
                        m = 255;
                    }
                }
            }
            next[y as usize * bx + x as usize] = m;
        }
    }
    vis = next;
    // Close small gaps: a block with at least 5 non-smooth neighbours (edge-clamped) becomes non-smooth.
    for _ in 0..32 {
        let mut next = vis.clone();
        for y in 0..by as i64 {
            for x in 0..bx as i64 {
                if at(&vis, x, y) < 255 {
                    let mut m = 0;
                    for dy in -1..=1 {
                        for dx in -1..=1 {
                            if at(&vis, x + dx, y + dy) == 255 {
                                m += 1;
                            }
                        }
                    }
                    if m >= 5 {
                        next[y as usize * bx + x as usize] = 255;
                    }
                }
            }
        }
        vis = next;
    }
    // Ultrasmooth regions (4-connected runs of 0) smaller than 64 blocks do not count.
    let mut work = vis.clone();
    let mut stack: Vec<usize> = Vec::new();
    let mut region: Vec<usize> = Vec::new();
    for start in 0..bx * by {
        if work[start] != 0 {
            continue;
        }
        region.clear();
        stack.push(start);
        work[start] = 255;
        while let Some(i) = stack.pop() {
            region.push(i);
            let (x, y) = (i % bx, i / bx);
            let mut visit = |j: usize, stack: &mut Vec<usize>| {
                if work[j] == 0 {
                    work[j] = 255;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1, &mut stack);
            }
            if x + 1 < bx {
                visit(i + 1, &mut stack);
            }
            if y > 0 {
                visit(i - bx, &mut stack);
            }
            if y + 1 < by {
                visit(i + bx, &mut stack);
            }
        }
        if region.len() < REGION_TOO_SMALL {
            for &i in &region {
                vis[i] = 255;
            }
        }
    }
    vis.iter().map(|&v| if v == 0 { ULTRASMOOTH_BLOCK_MSE_SCALE } else { -1.0 }).collect()
}

/// Encode an RGBA8 image (`width * 4` stride) to its BC7 block stream (`ceil(W/4) * ceil(H/4)`
/// blocks, raster order). `threads` parallelises the per-block stage only (identical output).
pub fn encode_image(rgba: &[u8], width: u32, height: u32, bc7: &Bc7Params, rdo: &RdoParams, threads: usize) -> Vec<[u8; 16]> {
    let (bx, by) = (width.div_ceil(4) as usize, height.div_ceil(4) as usize);
    let pixels = image_blocks(rgba, width, height);
    let mut blocks = vec![[0u8; 16]; pixels.len()];
    let threads = threads.clamp(1, 64);
    if threads > 1 && pixels.len() > 64 {
        let per = pixels.len().div_ceil(threads);
        std::thread::scope(|s| {
            for (dst, src) in blocks.chunks_mut(per).zip(pixels.chunks(per)) {
                s.spawn(move || {
                    for (d, p) in dst.iter_mut().zip(src) {
                        *d = compress_block(p, bc7);
                    }
                });
            }
        });
    } else {
        for (d, p) in blocks.iter_mut().zip(&pixels) {
            *d = compress_block(p, bc7);
        }
    }
    if rdo.lambda > 0.0 {
        let lambda = rdo.lambda;
        let smooth_max = rdo.smooth_block_error_scale.unwrap_or_else(|| {
            // lerp(15, 50, std::min(1, lambda / 4)) = 15 + 35 * s, fused.
            let q = lambda / 4.0;
            let s = if q < 1.0 { q } else { 1.0 };
            35.0f32.mul_add(s, 15.0)
        });
        let mut scales = block_mse_scales(&pixels, bx, by);
        // std::min(lambda, 3.0f) and std::max(smooth_max, s * that), with their comparison order.
        let lambda_cap = if 3.0 < lambda { 3.0 } else { lambda };
        for s in &mut scales {
            if *s > 0.0 {
                let v = *s * lambda_cap;
                *s = if smooth_max < v { v } else { smooth_max };
            }
        }
        let ert = ErtParams {
            lambda,
            lookback_window_size: rdo.lookback.max(16),
            max_allowed_rms_increase_ratio: 10.0,
            max_smooth_block_std_dev: rdo.max_smooth_block_std_dev,
            smooth_block_max_mse_scale: smooth_max,
            try_two_matches: rdo.try_two_matches,
        };
        let total = blocks.len();
        let parts = if rdo.partitions > 1 && total >= 4096 { rdo.partitions as usize } else { 1 };
        // Partition p covers total / parts blocks; the last takes the remainder.
        let mut bounds = Vec::with_capacity(parts);
        let mut first = 0usize;
        for p in 0..parts {
            let n = if p == parts - 1 { total - first } else { total / parts };
            bounds.push((first, n));
            first += n;
        }
        let scales_ref = if rdo.ultrasmooth_block_handling { Some(scales.as_slice()) } else { None };
        let mut rest: &mut [[u8; 16]] = &mut blocks;
        let mut slices = Vec::with_capacity(parts);
        for &(_, n) in &bounds {
            let (head, tail) = rest.split_at_mut(n);
            slices.push(head);
            rest = tail;
        }
        std::thread::scope(|s| {
            for (slice, &(f, n)) in slices.into_iter().zip(&bounds) {
                let px = &pixels[f..f + n];
                let sc = scales_ref.map(|v| &v[f..f + n]);
                let ert = &ert;
                s.spawn(move || reduce_entropy(slice, px, ert, sc));
            }
        });
    }
    blocks
}
