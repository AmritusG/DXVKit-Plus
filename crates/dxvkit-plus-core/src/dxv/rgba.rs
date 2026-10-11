//! Block streams to RGBA8 (CPU path) — the oracle for the GPU conversion. Port of DXVKit
//! `src/pixels.c` (MIT; see `super`): BC1 / BC3 per spec 8.1 / 8.2, YCG6 / YG10 per spec 8.4 (exact
//! BC4 palettes, nearest chroma, midpoint 127.5, out-of-gamut folding: what Resolume Arena 7.28
//! shows). Single-threaded (DXVKit's optional banding is not ported).
//
// Lint note: pixel arithmetic is on values bounded by 255 * 70 (fixed point below) and block
// coordinates bounded by the layout (<= 16384 / 4), so it cannot overflow; the arithmetic lint is
// relaxed for this module only. Every buffer access is bounds-checked.
#![allow(clippy::arithmetic_side_effects)]

use super::{BlockFormat, Blocks, DxvError, Layout};
use crate::bc::{bc1_block, bc4_block, bc4_block_exact, premul};
use crate::bytes::put;

/// Fixed point for the HQ path: units of 1/70 (the 1/35 palette values doubled, so that the chroma
/// midpoint 127.5 is a whole number).
const HQ_ONE: i32 = 70;
const HQ_MAX: i32 = 255 * HQ_ONE;
const HQ_MID: i32 = 255 * HQ_ONE / 2;

/// Round to nearest (halves up) and clamp: units of 1/70 -> 0..255.
#[inline]
fn hq_round(v: i32) -> u8 {
    if v <= 0 {
        0
    } else if v >= HQ_MAX {
        255
    } else {
        ((v + HQ_ONE / 2) / HQ_ONE) as u8
    }
}

/// YCoCg -> RGB with Resolume Arena's convention (spec 8.4): chroma midpoint 127.5, out-of-gamut R / B
/// overflow carried into the other channel, then rounding. Y, co, cg in units of 1/70.
#[inline]
pub fn hq_rgb(y: i32, co: i32, cg: i32) -> [u8; 3] {
    let t = y - cg;
    let (mut r, g, mut b) = (t + co, y + cg, t - co);
    if r < 0 {
        b += r;
        r = 0;
    }
    if b < 0 {
        r += b;
        b = 0;
    }
    if r > HQ_MAX {
        b += r - HQ_MAX;
        r = HQ_MAX;
    }
    if b > HQ_MAX {
        r += b - HQ_MAX;
        b = HQ_MAX;
    }
    [hq_round(r), hq_round(g), hq_round(b)]
}

/// RGBA8 of the display area, rows top first, `width * 4` stride.
pub fn blocks_to_rgba(blocks: &Blocks, premultiply: bool) -> Result<Vec<u8>, DxvError> {
    let l = Layout::new(blocks.layout.variant, blocks.layout.display_width, blocks.layout.display_height)?;
    let stride = l.display_width as usize * 4;
    let mut rgba = vec![0u8; stride * l.display_height as usize];
    let premultiply = premultiply && l.variant.has_alpha();
    let p0 = blocks.planes[0].get(..l.bytes[0]).ok_or(DxvError::BufferTooSmall)?;
    if l.plane_count == 2 {
        let p1 = blocks.planes[1].get(..l.bytes[1]).ok_or(DxvError::BufferTooSmall)?;
        rows_hq(&l, p0, p1, &mut rgba, stride, premultiply)?;
    } else {
        rows_bc(&l, p0, &mut rgba, stride, premultiply)?;
    }
    Ok(rgba)
}

fn rows_bc(l: &Layout, plane: &[u8], rgba: &mut [u8], stride: usize, premultiply: bool) -> Result<(), DxvError> {
    let bc3 = l.format[0] == BlockFormat::Bc3;
    let block_bytes = if bc3 { 16 } else { 8 };
    let rows_of_blocks = l.display_height.div_ceil(4);
    for by in 0..rows_of_blocks {
        let y0 = by * 4;
        let rows = (l.display_height - y0).min(4);
        for bx in 0..l.blocks_x[0] {
            let x0 = bx * 4;
            if x0 >= l.display_width {
                break;
            }
            let cols = (l.display_width - x0).min(4) as usize;
            let at = (by as usize * l.blocks_x[0] as usize + bx as usize) * block_bytes;
            let blk = plane.get(at..at + block_bytes).ok_or(DxvError::BufferTooSmall)?;
            let mut px = if bc3 { bc1_block(blk.get(8..).unwrap_or_default(), true) } else { bc1_block(blk, false) };
            if bc3 {
                let alpha = bc4_block(blk);
                for (p, a) in px.iter_mut().zip(alpha) {
                    p[3] = a;
                    if premultiply {
                        for c in p.iter_mut().take(3) {
                            *c = premul(*c, a);
                        }
                    }
                }
            }
            for y in 0..rows as usize {
                let row: Vec<u8> = px.iter().skip(y * 4).take(cols).flatten().copied().collect();
                put(rgba, (y0 as usize + y) * stride + x0 as usize * 4, &row).ok_or(DxvError::BufferTooSmall)?;
            }
        }
    }
    Ok(())
}

fn rows_hq(l: &Layout, plane0: &[u8], plane1: &[u8], rgba: &mut [u8], stride: usize, premultiply: bool) -> Result<(), DxvError> {
    let ya = l.format[0] == BlockFormat::Bc5;
    let y_bytes = if ya { 16 } else { 8 };
    let chroma_rows = l.display_height.div_ceil(8);
    for cy in 0..chroma_rows {
        for cx in 0..l.blocks_x[1] {
            if cx * 8 >= l.display_width {
                break;
            }
            let cat = (cy as usize * l.blocks_x[1] as usize + cx as usize) * 16;
            let cblk = plane1.get(cat..cat + 16).ok_or(DxvError::BufferTooSmall)?;
            let co = bc4_block_exact(cblk);
            let cg = bc4_block_exact(cblk.get(8..).unwrap_or_default());
            for sub in 0..4u32 {
                let (bx, by) = (cx * 2 + (sub & 1), cy * 2 + (sub >> 1));
                let (x0, y0) = (bx * 4, by * 4);
                if x0 >= l.display_width || y0 >= l.display_height {
                    continue;
                }
                let cols = (l.display_width - x0).min(4);
                let rows = (l.display_height - y0).min(4);
                let yat = (by as usize * l.blocks_x[0] as usize + bx as usize) * y_bytes;
                let yblk = plane0.get(yat..yat + y_bytes).ok_or(DxvError::BufferTooSmall)?;
                let yv = bc4_block_exact(yblk);
                let av = if ya { bc4_block_exact(yblk.get(8..).unwrap_or_default()) } else { [0; 16] };
                for y in 0..rows {
                    // chroma sample of pixel (x0 + x, y0 + y) inside this chroma block: nearest
                    let crow = ((by & 1) * 4 + y) / 2;
                    for x in 0..cols {
                        let ci = (crow * 4 + ((bx & 1) * 4 + x) / 2) as usize;
                        let pi = (y * 4 + x) as usize;
                        let a = if ya { hq_round(2 * i32::from(av.get(pi).copied().unwrap_or(0))) } else { 255 };
                        let c = |v: &[u16; 16]| 2 * i32::from(v.get(ci).copied().unwrap_or(0)) - HQ_MID;
                        let mut px = hq_rgb(2 * i32::from(yv.get(pi).copied().unwrap_or(0)), c(&co), c(&cg));
                        if premultiply && ya {
                            px = px.map(|v| premul(v, a));
                        }
                        let at = (y0 + y) as usize * stride + (x0 + x) as usize * 4;
                        put(rgba, at, &[px[0], px[1], px[2], a]).ok_or(DxvError::BufferTooSmall)?;
                    }
                }
            }
        }
    }
    Ok(())
}
