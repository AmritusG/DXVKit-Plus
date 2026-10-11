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

/// 5x7 glyphs (rows top first, bit 4 = leftmost column) for [`graphics`]' text.
const FONT: [(char, [u8; 7]); 16] = [
    ('D', [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E]),
    ('X', [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11]),
    ('V', [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04]),
    ('K', [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11]),
    ('I', [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('T', [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04]),
    ('P', [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10]),
    ('L', [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F]),
    ('U', [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E]),
    ('S', [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E]),
    ('M', [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11]),
    ('B', [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E]),
    ('1', [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('0', [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
    ('8', [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
    (' ', [0; 7]),
];

/// Flat graphics, as VJ overlays use them (straight RGBA8, deterministic): a flat background
/// (transparent with `alpha`), UI bars and panels with 1-2 px borders, four coloured buttons, an
/// anti-aliased ring-and-triangle logo, blocky 5x7 text, and a card with a soft vertical
/// gradient; the logo and a bar move with `f`. Few colours, hard edges, some anti-aliasing.
pub fn graphics(f: u32, w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let (fw, fh) = (w.max(1) as f32, h.max(1) as f32);
    let px = |u: f32| (u * fw) as i32;
    let py = |v: f32| (v * fh) as i32;
    let bg = if alpha { [0u8, 0, 0, 0] } else { [18, 20, 32, 255] };
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let put = |out: &mut Vec<u8>, x: i32, y: i32, c: [u8; 4]| {
        if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            if let Some(d) = out.get_mut(i..i + 4) {
                d.copy_from_slice(&c);
            }
        }
    };
    let rect = |out: &mut Vec<u8>, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]| {
        for y in y0..y1 {
            for x in x0..x1 {
                put(out, x, y, c);
            }
        }
    };
    rect(&mut out, 0, 0, w as i32, h as i32, bg);
    // Top bar with a bright line, side panel with a border, buttons.
    rect(&mut out, 0, 0, w as i32, py(0.08), [40, 44, 70, 255]);
    rect(&mut out, 0, py(0.08), w as i32, py(0.08) + 2, [90, 200, 255, 255]);
    let (x0, y0, x1, y1) = (px(0.05), py(0.15), px(0.30), py(0.85));
    rect(&mut out, x0 - 2, y0 - 2, x1 + 2, y1 + 2, [70, 80, 120, 255]);
    rect(&mut out, x0, y0, x1, y1, [28, 32, 52, 255]);
    for (i, c) in [[230u8, 60, 60, 255], [60, 200, 90, 255], [250, 200, 40, 255], [80, 140, 255, 255]].into_iter().enumerate() {
        let by = py(0.20 + 0.15 * i as f32);
        rect(&mut out, px(0.08), by, px(0.27), by + py(0.10), c);
    }
    // A moving bar.
    let mx = (f as i32 * 6) % (w as i32 / 2);
    rect(&mut out, px(0.35) + mx, py(0.12), px(0.35) + mx + px(0.12), py(0.15), [255, 255, 255, 255]);
    // Gradient card.
    let (gx0, gy0, gx1, gy1) = (px(0.35), py(0.70), px(0.95), py(0.92));
    for y in gy0..gy1 {
        let t = (y - gy0) as f32 / (gy1 - gy0).max(1) as f32;
        let c = [(40.0 + 100.0 * t) as u8, (60.0 - 20.0 * t) as u8, (140.0 - 20.0 * t) as u8, 255];
        rect(&mut out, gx0, y, gx1, y + 1, c);
    }
    // Logo: anti-aliased ring and triangle (4x4 supersampled coverage), moving.
    let (cx, cy, r) = (0.62 * fw + f as f32 * 2.0, 0.42 * fh, 0.17 * fh);
    for y in (cy - r - 2.0) as i32..(cy + r + 2.0) as i32 {
        for x in (cx - r - 2.0) as i32..(cx + r + 2.0) as i32 {
            let (mut ring, mut tri) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (u, v) = (x as f32 + (sx as f32 + 0.5) / 4.0 - cx, y as f32 + (sy as f32 + 0.5) / 4.0 - cy);
                    let d = (u * u + v * v).sqrt();
                    ring += u32::from(d < r && d > r * 0.82);
                    // Triangle pointing up, inside 0.6 r.
                    let k = 0.6 * r;
                    tri += u32::from(v < 0.5 * k && v > -k && u.abs() < (v + k) * 0.6);
                }
            }
            if ring + tri == 0 {
                continue;
            }
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            let Some(dst) = out.get(i..i + 4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else { continue };
            let (c, cov) = if ring > 0 { ([255u8, 255, 255], ring) } else { ([255, 120, 0], tri) };
            let a = cov as f32 / 16.0;
            let mix = |s: u8, d: u8| (f32::from(s) * a + f32::from(d) * (1.0 - a)).round() as u8;
            let al = if alpha { (255.0 * a + f32::from(dst[3]) * (1.0 - a)).round() as u8 } else { 255 };
            let rgb = if alpha && dst[3] == 0 { c } else { [mix(c[0], dst[0]), mix(c[1], dst[1]), mix(c[2], dst[2])] };
            put(&mut out, x, y, [rgb[0], rgb[1], rgb[2], al]);
        }
    }
    // Blocky text.
    let scale = (h / 135).max(1) as i32;
    let (mut tx, ty) = (px(0.36), py(0.55));
    for ch in "DXVKIT PLUS LMBX 1080".chars() {
        let rows = FONT.iter().find(|(c, _)| *c == ch).map(|(_, r)| *r).unwrap_or([0; 7]);
        for (ry, bits) in rows.iter().enumerate() {
            for cx_ in 0..5 {
                if bits & (0x10 >> cx_) != 0 {
                    let (gx, gy) = (tx + cx_ * scale, ty + ry as i32 * scale);
                    rect(&mut out, gx, gy, gx + scale, gy + scale, [235, 235, 235, 255]);
                }
            }
        }
        tx += 6 * scale;
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
