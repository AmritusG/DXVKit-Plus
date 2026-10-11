//! Procedural test footage — our own, CC0 (`tests/fixtures/media/*.attribution`).
//!
//! [`tiles`] is the fixture pattern every committed media fixture is encoded from
//! (`examples/mint_fixtures.rs`): flat 8x8 tiles whose colours are exactly representable in RGB565
//! (bit-replicated) and whose alpha is flat per tile, so BC1 / BC3 / BC4 encoders can reproduce them
//! losslessly and a decoded frame can be compared with its source to 1/255. The top tile row
//! carries the frame index in binary (tile `i` white = bit `i`), readable after any codec
//! ([`read_frame_id`]); the rest scrolls one tile per frame so every frame differs.
//! Every fixture in `tests/fixtures/` was minted from this pattern (`scripts/mint-fixtures.sh`).
//!
//! [`plasma`] is smooth, detailed footage for decode benchmarks (realistic compression ratios).
//
// Lint note: coordinates are bounded by the frame size (<= 16384) and colours are bytes; the
// arithmetic lint is relaxed for this module (no input-derived numbers reach it).
#![allow(clippy::arithmetic_side_effects)]

/// Tile edge in pixels.
pub const TILE: u32 = 8;

/// RGB565-exact levels (5-bit bit-replicated for R and B; 6-bit for G).
const R5: [u8; 5] = [0, 66, 132, 198, 255];
const G6: [u8; 5] = [0, 65, 130, 195, 255];
/// Flat per-tile alpha levels (BC4 / BC3 alpha reproduce flat blocks exactly).
pub const ALPHAS: [u8; 6] = [255, 204, 153, 102, 51, 0];
/// Calibration strip (tiles 8..15 of the top row): R, G, B, C, M, Y, grey, orange.
pub const STRIP: [[u8; 3]; 8] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [0, 255, 255], [255, 0, 255], [255, 255, 0], [132, 130, 132], [255, 130, 0]];

fn hash(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0xC2B2_AE3D);
    h ^ (h >> 13)
}

/// The colour (straight RGBA) of tile (`tx`, `ty`) in frame `f`.
pub fn tile_rgba(f: u32, tx: u32, ty: u32) -> [u8; 4] {
    if ty == 0 {
        if tx < 8 {
            let v = if (f >> tx) & 1 == 1 { 255 } else { 0 };
            return [v, v, v, 255];
        }
        let [r, g, b] = STRIP.get((tx - 8) as usize % 8).copied().unwrap_or([0, 0, 0]);
        return [r, g, b, 255];
    }
    let h = hash(tx.wrapping_add(f), ty);
    let r = R5.get((h % 5) as usize).copied().unwrap_or(0);
    let g = G6.get(((h / 5) % 5) as usize).copied().unwrap_or(0);
    let b = R5.get(((h / 25) % 5) as usize).copied().unwrap_or(0);
    let a = ALPHAS.get(((tx + 2 * ty) % 6) as usize).copied().unwrap_or(255);
    [r, g, b, a]
}

/// Frame `f` of the tile pattern, straight RGBA8, rows top first, `w * 4` stride.
pub fn tiles(f: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&tile_rgba(f, x / TILE, y / TILE));
        }
    }
    out
}

/// The frame index encoded in the top tile row of an RGBA frame (8 bits), or `None` if the frame is
/// too small. Threshold at mid-grey on the green channel (robust to lossy codecs).
pub fn read_frame_id(rgba: &[u8], w: u32, h: u32) -> Option<u32> {
    if w < 8 * TILE || h < TILE {
        return None;
    }
    let y = TILE / 2;
    let mut id = 0u32;
    for bit in 0..8 {
        let x = bit * TILE + TILE / 2;
        let at = ((y * w + x) * 4) as usize;
        let g = *rgba.get(at + 1)?;
        if g > 128 {
            id |= 1 << bit;
        }
    }
    Some(id)
}

/// Stamp frame index `f` into the top-left 8 tiles of an RGBA frame (white = bit set), the same
/// code [`read_frame_id`] reads — so benchmark footage stays identifiable after any codec.
pub fn stamp_frame_id(px: &mut [u8], w: u32, h: u32, f: u32) {
    for bit in 0..8u32 {
        let v = if (f >> bit) & 1 == 1 { 255 } else { 0 };
        for y in 0..TILE.min(h) {
            for x in bit * TILE..((bit + 1) * TILE).min(w) {
                let i = ((y * w + x) * 4) as usize;
                if let Some(p) = px.get_mut(i..i + 4) {
                    p.copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
    }
}

/// Smooth, detailed benchmark footage (straight RGBA8, opaque unless `alpha`), deterministic.
pub fn plasma(f: u32, w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    let t = f as f32 * 0.07;
    let (fw, fh) = (w.max(1) as f32, h.max(1) as f32);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / fw, y as f32 / fh);
            let a = (u * 9.0 + t).sin() + (v * 7.0 - t * 1.3).sin() + ((u + v) * 11.0 + t * 0.7).sin() + ((u * u + v * v).sqrt() * 23.0 - t).sin();
            let n = (hash(x, y.wrapping_add(f)) & 15) as f32 - 7.5; // film-grain-like detail
            let c = |phase: f32| (((a * 0.8 + phase).sin() * 0.5 + 0.5) * 235.0 + n).clamp(0.0, 255.0) as u8;
            let al = if alpha { (((u * 3.0 - t).sin() * 0.5 + 0.5) * 255.0) as u8 } else { 255 };
            out.extend_from_slice(&[c(0.0), c(2.1), c(4.2), al]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ids_round_trip_and_tiles_are_565_exact() {
        for f in [0, 1, 5, 23, 200, 255] {
            let px = tiles(f, 128, 72);
            assert_eq!(read_frame_id(&px, 128, 72), Some(f));
        }
        // Negative control: a different frame reads differently; too small reads nothing.
        assert_ne!(read_frame_id(&tiles(6, 128, 72), 128, 72), Some(5));
        assert_eq!(read_frame_id(&tiles(6, 32, 8), 32, 8), None);
        let mut p = plasma(0, 128, 72, false);
        stamp_frame_id(&mut p, 128, 72, 77);
        assert_eq!(read_frame_id(&p, 128, 72), Some(77));
        for &v in R5.iter() {
            let q = u32::from(v) >> 3;
            assert_eq!(((q << 3) | (q >> 2)) as u8, v);
        }
        for &v in G6.iter() {
            let q = u32::from(v) >> 2;
            assert_eq!(((q << 2) | (q >> 4)) as u8, v);
        }
    }
}
