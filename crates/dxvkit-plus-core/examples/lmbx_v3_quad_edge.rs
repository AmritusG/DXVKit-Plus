//! Write the hand-built LMBX v3 conformance vector `lmbx_v3_quad_edge_422.lmbd`: quad tiles with
//! edge alpha — flat and palette sub-tiles whose colours have partial alpha, made exact by
//! edge-alpha residuals — beside a palette tile with edge alpha, in 4:2:2. The reference encoder
//! does not choose quad tiles with edge alpha on the procedural cards, so this vector is built
//! directly from the bitstream definition (`docs/LMBD.md` §11.6a). CC0, our own content.
//!
//! ```text
//! cargo run --release -p dxvkit-plus-core --example lmbx_v3_quad_edge -- <out.lmbd>
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use dxvkit_plus_core::lmbd::{ChunkData, Compressor, Header, Writer, frame_payload};
use dxvkit_plus_core::lmbx::{Chroma, FrameHeader, IDX_BITS, PAL3_CTX, PAL3_SYMBOLS, PROB_SCALE, RANS_L, VERSION_3};

type Px = [u8; 4];

/// A rANS stream (as the reference encoder writes them) of `(frequency, cumulative)` symbols
/// with precision `IDX_BITS`.
fn rans(syms: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut x = RANS_L;
    for &(f, c) in syms.iter().rev() {
        let xmax = ((RANS_L >> IDX_BITS) << 8) * f;
        while x >= xmax {
            out.push((x & 0xff) as u8);
            x >>= 8;
        }
        x = ((x / f) << IDX_BITS) + (x % f) + c;
    }
    out.extend_from_slice(&x.to_be_bytes());
    out.reverse();
    out
}

/// The index tables (every context alike): symbols 0..63 at 16 each, 1024 in all.
fn index_sym(s: u32) -> (u32, u32) {
    assert!(s < 64);
    (16, 16 * s)
}

/// The edge-alpha table: every residual 0..255 at 4.
fn edge_sym(e: u8) -> (u32, u32) {
    (4, 4 * u32::from(e))
}

/// A palette region's symbols in decode order (§11.6a): each pixel's index symbol (from its left /
/// above neighbours; every context's table is the same here), then its edge residual when its
/// colour has partial alpha.
fn region(idx: &[usize], size: usize, cols: &[Px], target_alpha: &dyn Fn(usize) -> u8, syms: &mut Vec<(u32, u32)>) {
    let n = cols.len();
    for i in 0..size * size {
        let (x, y) = (i % size, i / size);
        let l = if x > 0 {
            idx[i - 1]
        } else if y > 0 {
            idx[i - size]
        } else {
            0
        };
        let a = if y > 0 { idx[i - size] } else { l };
        let v = idx[i];
        let s = if v == l {
            0
        } else if v == a {
            1
        } else {
            1 + (v + n - l) % n
        };
        syms.push(index_sym(s as u32));
        let c = cols[v];
        if c[3] != 0 && c[3] != 255 {
            syms.push(edge_sym(target_alpha(i).wrapping_sub(c[3])));
        }
    }
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: lmbx_v3_quad_edge <out.lmbd>");
    let (w, h) = (64u32, 32u32);
    let header = Header::new_lmbx(w, h, 30, 1);
    let mut file = std::io::Cursor::new(Vec::new());
    let mut wr = Writer::new(&mut file, header.clone()).unwrap();
    for frame in 0..2u8 {
        let fh = FrameHeader { version: VERSION_3, chroma: Chroma::Halo422, alpha: true, width: w, height: h, chroma_ratio: 256, qt: [16; 16] };
        let mut b = fh.to_bytes().to_vec();
        // Coefficient table (no DCT tile uses it): EOB only.
        b.extend([1, 0, 0xff, 0x07]);
        b.extend((PROB_SCALE as u16).to_le_bytes());
        // Eight index tables (symbols 0..63 at 16) and the edge table (0..255 at 4).
        assert_eq!((PAL3_CTX, PAL3_SYMBOLS), (8, 65));
        for _ in 0..PAL3_CTX {
            b.extend(64u16.to_le_bytes());
            (0..64u16).for_each(|s| b.extend([s.to_le_bytes(), 16u16.to_le_bytes()].concat()));
        }
        b.extend(256u16.to_le_bytes());
        (0..256u16).for_each(|s| b.extend([s.to_le_bytes(), 4u16.to_le_bytes()].concat()));
        // Mode map: tile 0 quad + edge, tile 1 palette + edge. Quantiser map: zero.
        b.extend([0x13, 0x12]);
        b.extend([0u8; 16]);
        // Tile 0 (quad + edge): top-left flat partial colour (alpha ramps by residual), top-right
        // palette of 3 (two partial), bottom-left flat opaque, bottom-right palette of 6 diagonals.
        let f = frame;
        let mut s0 = Vec::new();
        let mut syms = Vec::new();
        let tl: Px = [200, 40 + f, 40, 128];
        s0.push(0);
        s0.extend(tl);
        let tr: Vec<Px> = vec![[10, 20, 200, 255], [250, 250, 0, 64], [0, 200, 100, 192]];
        s0.extend([1, tr.len() as u8]);
        tr.iter().for_each(|c| s0.extend(c));
        let bl: Px = [30, 30, 30, 255];
        s0.push(0);
        s0.extend(bl);
        let br: Vec<Px> = (0..6u8).map(|k| [k * 40, 255 - k * 30, 100 + f, if k.is_multiple_of(2) { 255 } else { 32 + k * 30 }]).collect();
        s0.extend([1, br.len() as u8]);
        br.iter().for_each(|c| s0.extend(c));
        // TL: every pixel partial; its alpha a gradient within 1..=254.
        for i in 0..256usize {
            let target = (1 + (i % 16) * 8 + (i / 16) * 7 + usize::from(f)).min(254) as u8;
            syms.push(edge_sym(target.wrapping_sub(tl[3])));
        }
        let tri: Vec<usize> = (0..256)
            .map(|i| {
                if (i % 16) + (i / 16) < 10 {
                    0
                } else if (i % 16) > (i / 16) {
                    1
                } else {
                    2
                }
            })
            .collect();
        region(&tri, 16, &tr, &|i| (20 + i % 200) as u8, &mut syms);
        // BL: opaque flat — no symbols.
        let diag: Vec<usize> = (0..256).map(|i| ((i % 16) + (i / 16) + usize::from(f)) / 6 % 6).collect();
        region(&diag, 16, &br, &|i| (1 + (i * 7) % 253) as u8, &mut syms);
        s0.extend(rans(&syms));
        // Tile 1 (palette + edge): 20 colours in bands, a partial one every third.
        let cols: Vec<Px> = (0..20u8).map(|k| [k * 12, 255 - k * 12, (k * 37) % 255, if k.is_multiple_of(3) { 40 + k * 9 } else { 255 }]).collect();
        let mut s1 = vec![cols.len() as u8];
        cols.iter().for_each(|c| s1.extend(c));
        let bands: Vec<usize> = (0..1024).map(|i| ((i % 32) / 2 + (i / 32) / 8 + usize::from(f)) % 20).collect();
        let mut syms1 = Vec::new();
        region(&bands, 32, &cols, &|i| (1 + (i * 13) % 253) as u8, &mut syms1);
        s1.extend(rans(&syms1));
        // Streams: 2 tiles x 4 planes; the direct payloads in stream 0, the rest empty.
        let streams: Vec<Vec<u8>> = vec![s0, Vec::new(), Vec::new(), Vec::new(), s1, Vec::new(), Vec::new(), Vec::new()];
        b.extend((streams.len() as u32).to_le_bytes());
        streams.iter().for_each(|s| b.extend((s.len() as u32).to_le_bytes()));
        streams.iter().for_each(|s| b.extend(s));
        dxvkit_plus_core::lmbx::Frame::parse(&b, w, h).unwrap();
        // Self-check: the edge residuals made every partial alpha what was intended.
        let rgba = dxvkit_plus_core::lmbx::decode_rgba8(&b, w, h, false).unwrap();
        let alpha = |x: u32, y: u32| rgba[((y * w + x) * 4 + 3) as usize];
        for i in 0..256u32 {
            let target = (1 + (i % 16) * 8 + (i / 16) * 7 + u32::from(f)).min(254) as u8;
            assert_eq!(alpha(i % 16, i / 16), target, "top-left sub-tile pixel {i}");
        }
        for i in 0..1024u32 {
            let k = (((i % 32) / 2 + (i / 32) / 8 + u32::from(f)) % 20) as u8;
            let want = if k.is_multiple_of(3) { (1 + (i * 13) % 253) as u8 } else { 255 };
            assert_eq!(alpha(32 + i % 32, i / 32), want, "tile 1 pixel {i}");
        }
        let payload = frame_payload(&header, &[ChunkData { compressor: Compressor::None, raw_size: b.len() as u32, data: &b }]).unwrap();
        wr.push_payload(&payload).unwrap();
    }
    wr.finish().unwrap();
    std::fs::write(&out, file.into_inner()).unwrap();
    println!("wrote {out}");
}
