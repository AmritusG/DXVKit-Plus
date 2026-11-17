//! HAP encoder structure: section headers, chunk tables and the split rule, the raw-chunk fallback, padding, the
//! HapY transform, the alpha rules per codec, chunk-count and thread-count invariance (decoded with
//! `dxvkit_plus_core::hap`), and the 8-byte header past 16 MiB. Generated patterns only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use dxvkit_plus_core::hap::{HapTexture, decode};
use dxvkit_plus_enc::EncodeError;
use dxvkit_plus_enc::alpha::{FrameAlpha, unpremultiply};
use dxvkit_plus_enc::hap::{HapCodec, HapEncoder};

const CODECS: [HapCodec; 5] = [HapCodec::Hap1, HapCodec::Hap5, HapCodec::HapY, HapCodec::HapA, HapCodec::HapM];

fn noise(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..w * h * 4)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

fn smooth(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) % 256) as u8, ((x * 2 + y) % 256) as u8]);
        }
    }
    v
}

fn enc(codec: HapCodec, chunks: u32, w: u32, h: u32, rgba: &[u8], alpha: FrameAlpha) -> Vec<u8> {
    HapEncoder::new(codec, chunks, w, h).unwrap().encode(rgba, alpha).unwrap()
}

/// (type, body start, body length) of the section at `at`.
fn section(b: &[u8], at: usize) -> (u8, usize, usize) {
    let len24 = u32::from_le_bytes([b[at], b[at + 1], b[at + 2], 0]) as usize;
    if len24 == 0 { (b[at + 3], at + 8, u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize) } else { (b[at + 3], at + 4, len24) }
}

/// A chunked texture section at `at`: (outer type, compressor bytes, stored sizes, chunk data start, end).
fn chunked(b: &[u8], at: usize) -> (u8, Vec<u8>, Vec<u32>, usize, usize) {
    let (ty, body, len) = section(b, at);
    let (ity, ibody, ilen) = section(b, body);
    assert_eq!(ity, 0x01, "decode instructions container first");
    let (cty, cbody, clen) = section(b, ibody);
    assert_eq!(cty, 0x02, "compressor table first in 0x01");
    let (sty, sbody, slen) = section(b, cbody + clen);
    assert_eq!(sty, 0x03, "size table second in 0x01");
    assert_eq!(sbody + slen, ibody + ilen, "0x01 holds exactly 0x02 and 0x03 (no 0x04 offset table)");
    let sizes: Vec<u32> = b[sbody..sbody + slen].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(clen, sizes.len());
    (ty, b[cbody..cbody + clen].to_vec(), sizes, ibody + ilen, body + len)
}

#[test]
fn fourcc_and_alpha_flags() {
    let f: Vec<[u8; 4]> = CODECS.iter().map(|c| c.fourcc()).collect();
    assert_eq!(f, [*b"Hap1", *b"Hap5", *b"HapY", *b"HapA", *b"HapM"]);
    let a: Vec<bool> = CODECS.iter().map(|c| c.has_alpha()).collect();
    assert_eq!(a, [false, true, false, true, true]);
}

#[test]
fn settings_are_validated() {
    assert!(matches!(HapEncoder::new(HapCodec::Hap1, 0, 8, 8), Err(EncodeError::Invalid(_))));
    assert!(matches!(HapEncoder::new(HapCodec::Hap1, 65, 8, 8), Err(EncodeError::Invalid(_))));
    assert!(matches!(HapEncoder::new(HapCodec::Hap1, 1, 0, 8), Err(EncodeError::Invalid(_))));
    assert!(matches!(HapEncoder::new(HapCodec::Hap1, 1, 8, 16_385), Err(EncodeError::Invalid(_))));
    assert!(HapEncoder::new(HapCodec::Hap1, 64, 16_384, 1).is_ok());
    let mut e = HapEncoder::new(HapCodec::Hap5, 1, 8, 8).unwrap();
    assert!(matches!(e.encode(&[0; 8 * 8 * 4 - 1], FrameAlpha::Straight), Err(EncodeError::Frame(_))));
    assert!(!e.refined_bc4());
}

#[test]
fn single_section_headers() {
    let (w, h) = (37, 21);
    let px = smooth(w, h);
    for (codec, ty) in [(HapCodec::Hap1, 0xBB), (HapCodec::Hap5, 0xBE), (HapCodec::HapY, 0xBF), (HapCodec::HapA, 0xB1)] {
        let p = enc(codec, 1, w, h, &px, FrameAlpha::Straight);
        let (t, body, len) = section(&p, 0);
        assert_eq!((t, body, body + len), (ty, 4, p.len()), "{codec:?}");
        // The body is one Snappy block whose preamble is the block stream's length.
        let tex = match codec {
            HapCodec::Hap1 => HapTexture::Bc1Rgb,
            HapCodec::HapA => HapTexture::Bc4Alpha,
            _ => HapTexture::Bc3Rgba,
        };
        let want = tex.bytes(w, h).unwrap();
        let (mut n, mut shift, mut i) = (0usize, 0, body);
        loop {
            n |= usize::from(p[i] & 0x7F) << shift;
            shift += 7;
            i += 1;
            if p[i - 1] < 0x80 {
                break;
            }
        }
        assert_eq!(n, want, "{codec:?} Snappy preamble");
    }
    // HapM: 0x0D around a 0xBF then a 0xB1, nothing else.
    let p = enc(HapCodec::HapM, 1, w, h, &px, FrameAlpha::Straight);
    let (t, body, len) = section(&p, 0);
    assert_eq!((t, body + len), (0x0D, p.len()));
    let (ty, yb, yl) = section(&p, body);
    let (ta, ab, al) = section(&p, yb + yl);
    assert_eq!((ty, ta, ab + al), (0xBF, 0xB1, p.len()));
}

#[test]
fn chunk_tables_split_rule_and_raw_fallback() {
    // 64x64 Hap1 = 256 blocks of 8 bytes; 7 chunks → 36 blocks each, the last 40. Noise does not compress, so
    // every chunk is stored raw (0x0A) and the size table shows the split directly.
    let px = noise(64, 64, 9);
    let p = enc(HapCodec::Hap1, 7, 64, 64, &px, FrameAlpha::Straight);
    let (ty, comp, sizes, data, end) = chunked(&p, 0);
    assert_eq!(ty, 0xCB);
    assert_eq!(comp, vec![0x0A; 7]);
    assert_eq!(sizes, [288, 288, 288, 288, 288, 288, 320]);
    assert_eq!((end, data + sizes.iter().sum::<u32>() as usize), (p.len(), p.len()));

    // Flat colour compresses: every chunk Snappy (0x0B), stored sizes smaller than raw.
    let flat = [10u8, 200, 30, 255].repeat(64 * 64);
    for (codec, ty, bs) in [(HapCodec::Hap1, 0xCB, 8), (HapCodec::Hap5, 0xCE, 16), (HapCodec::HapY, 0xCF, 16), (HapCodec::HapA, 0xC1, 8)] {
        let p = enc(codec, 4, 64, 64, &flat, FrameAlpha::Straight);
        let (t, comp, sizes, data, end) = chunked(&p, 0);
        assert_eq!((t, comp), (ty, vec![0x0B; 4]), "{codec:?}");
        assert!(sizes.iter().all(|&s| (s as usize) < 64 * bs), "{codec:?}");
        assert_eq!(data + sizes.iter().sum::<u32>() as usize, end);
    }

    // Mixed: noise in the top half, flat in the bottom → raw chunks first, Snappy chunks last.
    let mut half = noise(64, 64, 3);
    half[64 * 32 * 4..].copy_from_slice(&flat[64 * 32 * 4..]);
    let p = enc(HapCodec::Hap5, 4, 64, 64, &half, FrameAlpha::Straight);
    let (_, comp, ..) = chunked(&p, 0);
    assert_eq!(comp, [0x0A, 0x0A, 0x0B, 0x0B]);

    // More chunks than blocks: clamped to the block count (a 1x1 frame is one chunk, still the chunked form).
    let p = enc(HapCodec::Hap5, 64, 1, 1, &[1, 2, 3, 4], FrameAlpha::Straight);
    let (ty, comp, sizes, ..) = chunked(&p, 0);
    assert_eq!((ty, comp.len(), sizes.len()), (0xCE, 1, 1));
    let p = enc(HapCodec::Hap1, 64, 9, 5, &noise(9, 5, 1), FrameAlpha::Straight);
    assert_eq!(chunked(&p, 0).1.len(), 6);
}

#[test]
fn hapm_chunked_wraps_two_chunked_inners() {
    let px = smooth(40, 24);
    let p = enc(HapCodec::HapM, 3, 40, 24, &px, FrameAlpha::Straight);
    let (t, body, len) = section(&p, 0);
    assert_eq!((t, body + len), (0x0D, p.len()));
    let (ty, _, ysizes, _, yend) = chunked(&p, body);
    let (ta, _, asizes, _, aend) = chunked(&p, yend);
    assert_eq!((ty, ta, aend), (0xCF, 0xC1, p.len()));
    assert_eq!((ysizes.len(), asizes.len()), (3, 3));
}

#[test]
fn every_chunk_count_decodes_to_the_same_blocks() {
    for (w, h) in [(1, 1), (17, 9), (64, 36)] {
        let mut px = smooth(w, h);
        px[..(w * 4) as usize].copy_from_slice(&noise(w, 1, 5));
        for codec in CODECS {
            let one = enc(codec, 1, w, h, &px, FrameAlpha::Straight);
            let want = decode(&one, w, h, false).unwrap();
            assert_eq!(want.images.len(), if codec == HapCodec::HapM { 2 } else { 1 });
            for chunks in 2..=64 {
                let p = enc(codec, chunks, w, h, &px, FrameAlpha::Straight);
                assert_eq!(decode(&p, w, h, true).unwrap(), want, "{codec:?} {w}x{h} chunks {chunks}");
            }
        }
    }
}

#[test]
fn output_does_not_depend_on_thread_count() {
    let (w, h) = (203, 157);
    let px = smooth(w, h);
    for codec in CODECS {
        for chunks in [1, 5] {
            let mut want = None;
            for threads in [1, 2, 3, 8, 0] {
                let mut e = HapEncoder::new(codec, chunks, w, h).unwrap();
                e.set_threads(threads);
                let p = e.encode(&px, FrameAlpha::Premultiplied).unwrap();
                assert_eq!(want.get_or_insert_with(|| p.clone()), &p, "{codec:?} chunks {chunks} threads {threads}");
            }
        }
    }
}

/// The frame copied into a zero-filled canvas of `cw` x `ch`.
fn padded(px: &[u8], w: u32, h: u32, cw: u32, ch: u32) -> Vec<u8> {
    let mut out = vec![0u8; (cw * ch * 4) as usize];
    for y in 0..h as usize {
        let (s, d) = (y * w as usize * 4, y * cw as usize * 4);
        out[d..d + w as usize * 4].copy_from_slice(&px[s..s + w as usize * 4]);
    }
    out
}

#[test]
fn padding_is_zero_pixels() {
    // Hap1 / Hap5 / HapA: a non-aligned frame encodes like the same frame on a zero canvas of the coded size.
    let (w, h) = (13, 6);
    let px = smooth(w, h);
    let canvas = padded(&px, w, h, 16, 8);
    for codec in [HapCodec::Hap1, HapCodec::Hap5, HapCodec::HapA] {
        for alpha in [FrameAlpha::Straight, FrameAlpha::Premultiplied] {
            let a = decode(&enc(codec, 1, w, h, &px, alpha), w, h, false).unwrap();
            let b = decode(&enc(codec, 1, 16, 8, &canvas, alpha), 16, 8, false).unwrap();
            assert_eq!(a, b, "{codec:?} {alpha:?}");
        }
    }
    // Hap5 opaque: alpha is forced to 255 on the frame, while padding stays alpha 0.
    let opaque = decode(&enc(HapCodec::Hap5, 1, w, h, &px, FrameAlpha::Opaque), w, h, false).unwrap();
    let mut forced = px.clone();
    forced.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    let b = decode(&enc(HapCodec::Hap5, 1, 16, 8, &padded(&forced, w, h, 16, 8), FrameAlpha::Straight), 16, 8, false).unwrap();
    assert_eq!(opaque, b);
}

/// HapYBlockPacker's transform, written out: the intermediate RGBA (Co·s + 128, Cg·s + 128, scale byte, Y) at the
/// coded size, padding pixels (128, 128, scale, 0) inside a block that has frame pixels.
fn hapy_intermediate(px: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    let (cw, ch) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    let mut out = vec![0u8; (cw * ch * 4) as usize];
    for by in (0..ch).step_by(4) {
        for bx in (0..cw).step_by(4) {
            let mut cells = [(0i32, 0i32, 0i32); 16];
            let mut max_abs = 0;
            for t in 0..16u32 {
                let (x, y) = (bx + t % 4, by + t / 4);
                if x < w && y < h {
                    let i = ((y * w + x) * 4) as usize;
                    let (r, g, b) = (i32::from(px[i]), i32::from(px[i + 1]), i32::from(px[i + 2]));
                    let c = ((r + 2 * g + b + 2) >> 2, (r - b) >> 1, (-r + 2 * g - b) >> 2);
                    max_abs = max_abs.max(c.1.abs()).max(c.2.abs());
                    cells[t as usize] = c;
                }
            }
            let (s, byte) = if max_abs <= 31 {
                (4, 24)
            } else if max_abs <= 63 {
                (2, 8)
            } else {
                (1, 0)
            };
            for (t, &(yy, co, cg)) in cells.iter().enumerate() {
                let (x, y) = (bx + t as u32 % 4, by + t as u32 / 4);
                let i = ((y * cw + x) * 4) as usize;
                out[i..i + 4].copy_from_slice(&[(co * s + 128).clamp(0, 255) as u8, (cg * s + 128).clamp(0, 255) as u8, byte, yy as u8]);
            }
        }
    }
    (out, cw, ch)
}

#[test]
fn hapy_is_bc3_of_the_scaled_ycocg_transform() {
    // HapY's BC3 = BC4 (as HapA, straight) of the Y plane + BC1 (as Hap1) of (Co·s+128, Cg·s+128, scale), with the
    // alpha byte = Y taking part in BC1's constant-colour test exactly as Hap1's alpha does. Saturated, medium and
    // low chroma blocks exercise all three scales; 21x10 is not 4-aligned.
    let (w, h) = (21, 10);
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let g = 60 + (x * 7 + y * 3) % 120;
            let p = match (x / 4 + y / 4) % 4 {
                0 => [255, 0, (y * 20) as u8, 255],
                1 => [g as u8 + 40, g as u8, g as u8 - 40, 255],
                2 => [g as u8 + 15, g as u8, g as u8 - 10, 255],
                _ => [g as u8, g as u8, g as u8, 255],
            };
            px.extend_from_slice(&p);
        }
    }
    let (inter, cw, ch) = hapy_intermediate(&px, w, h);
    let y = decode(&enc(HapCodec::HapY, 1, w, h, &px, FrameAlpha::Opaque), w, h, false).unwrap();
    let c = decode(&enc(HapCodec::Hap1, 1, cw, ch, &inter, FrameAlpha::Opaque), cw, ch, false).unwrap();
    let a = decode(&enc(HapCodec::HapA, 1, cw, ch, &inter, FrameAlpha::Straight), cw, ch, false).unwrap();
    let blocks = &y.images[0].blocks;
    assert_eq!(y.images[0].texture, HapTexture::Bc3YCoCg);
    let n = blocks.len() / 16;
    assert_eq!(n, (cw / 4 * ch / 4) as usize);
    let scales: std::collections::BTreeSet<u8> = inter.chunks_exact(4).map(|p| p[2]).collect();
    assert_eq!(scales.into_iter().collect::<Vec<_>>(), [0, 8, 24], "all three scales exercised");
    for b in 0..n {
        assert_eq!(&blocks[b * 16..b * 16 + 8], &a.images[0].blocks[b * 8..b * 8 + 8], "block {b} Y");
        assert_eq!(&blocks[b * 16 + 8..b * 16 + 16], &c.images[0].blocks[b * 8..b * 8 + 8], "block {b} CoCg");
    }
    // The alpha tag does not matter to HapY.
    for alpha in [FrameAlpha::Straight, FrameAlpha::Premultiplied] {
        assert_eq!(decode(&enc(HapCodec::HapY, 1, w, h, &px, alpha), w, h, false).unwrap(), y);
    }
}

#[test]
fn alpha_rules() {
    let (w, h) = (24, 16);
    let mut px = smooth(w, h);
    // A valid premultiplied frame (c <= a), with a = 0 pixels.
    px.chunks_exact_mut(4).for_each(|p| {
        let a = p[3];
        for c in &mut p[..3] {
            *c = (u32::from(*c) * u32::from(a) / 255) as u8;
        }
    });
    let straight: Vec<u8> = px.chunks_exact(4).flat_map(|p| unpremultiply([p[0], p[1], p[2], p[3]])).collect();
    let opaque: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect();

    // Hap5: premultiplied is un-premultiplied; opaque forces alpha 255.
    assert_eq!(enc(HapCodec::Hap5, 1, w, h, &px, FrameAlpha::Premultiplied), enc(HapCodec::Hap5, 1, w, h, &straight, FrameAlpha::Straight));
    assert_eq!(enc(HapCodec::Hap5, 1, w, h, &px, FrameAlpha::Opaque), enc(HapCodec::Hap5, 1, w, h, &opaque, FrameAlpha::Straight));

    // Hap1 ignores the tag (the alpha bytes still reach BC1's constant-colour test, as in GlEnc).
    let h1 = enc(HapCodec::Hap1, 1, w, h, &px, FrameAlpha::Straight);
    assert_eq!(enc(HapCodec::Hap1, 1, w, h, &px, FrameAlpha::Premultiplied), h1);
    assert_eq!(enc(HapCodec::Hap1, 1, w, h, &px, FrameAlpha::Opaque), h1);

    // HapA: no alpha is refused; premultiplied alpha is the alpha itself.
    let mut e = HapEncoder::new(HapCodec::HapA, 1, w, h).unwrap();
    assert!(matches!(e.encode(&px, FrameAlpha::Opaque), Err(EncodeError::NoAlpha)));
    assert_eq!(e.encode(&px, FrameAlpha::Premultiplied).unwrap(), e.encode(&px, FrameAlpha::Straight).unwrap());

    // HapM: colour is HapY's of the pixels as given (no un-premultiply), alpha is HapA's; opaque → alpha 255.
    for alpha in [FrameAlpha::Straight, FrameAlpha::Premultiplied, FrameAlpha::Opaque] {
        let m = decode(&enc(HapCodec::HapM, 1, w, h, &px, alpha), w, h, false).unwrap();
        let y = decode(&enc(HapCodec::HapY, 1, w, h, &px, alpha), w, h, false).unwrap();
        assert_eq!(m.images[0], y.images[0], "{alpha:?} colour");
        assert_eq!(m.images[1].texture, HapTexture::Bc4Alpha);
        if alpha == FrameAlpha::Opaque {
            assert!(m.images[1].blocks.chunks_exact(8).all(|b| b == [255, 255, 0, 0, 0, 0, 0, 0]));
        } else {
            let a = decode(&enc(HapCodec::HapA, 1, w, h, &px, alpha), w, h, false).unwrap();
            assert_eq!(m.images[1], a.images[0], "{alpha:?} alpha");
        }
    }
}

#[test]
fn refined_bc4_changes_bc4_codecs_only() {
    let (w, h) = (32, 32);
    let px = noise(w, h, 77);
    for codec in CODECS {
        let mut e = HapEncoder::new(codec, 1, w, h).unwrap();
        let plain = e.encode(&px, FrameAlpha::Straight).unwrap();
        e.set_refined_bc4(true);
        let refined = e.encode(&px, FrameAlpha::Straight).unwrap();
        assert_eq!(plain == refined, codec == HapCodec::Hap1, "{codec:?}");
        // Same decoded colour for the BC1 half: the refined search only touches BC4.
        let (a, b) = (decode(&plain, w, h, false).unwrap(), decode(&refined, w, h, false).unwrap());
        if codec == HapCodec::Hap5 {
            let bc1 = |f: &dxvkit_plus_core::hap::HapFrame| -> Vec<u8> { f.images[0].blocks.chunks_exact(16).flat_map(|b| b[8..].to_vec()).collect() };
            assert_eq!(bc1(&a), bc1(&b));
        }
    }
}

#[test]
fn eight_byte_header_past_16_mib() {
    // 4096x4096 Hap5 = 16 MiB of blocks; noise does not compress, so the Snappy payload is past 2^24 bytes.
    let (w, h) = (4096, 4096);
    let px = noise(w, h, 4242);
    let p = enc(HapCodec::Hap5, 1, w, h, &px, FrameAlpha::Straight);
    assert_eq!(&p[..4], [0, 0, 0, 0xBE]);
    let len = u32::from_le_bytes(p[4..8].try_into().unwrap()) as usize;
    assert!(len >= 1 << 24);
    assert_eq!(len + 8, p.len());
    // Chunked: raw chunks, extended outer header, short inner tables.
    let p = enc(HapCodec::Hap5, 2, w, h, &px, FrameAlpha::Straight);
    assert_eq!(&p[..4], [0, 0, 0, 0xCE]);
    let (_, comp, sizes, data, end) = chunked(&p, 0);
    assert_eq!((comp, sizes), (vec![0x0A, 0x0A], vec![1 << 23, 1 << 23]));
    assert_eq!((data + (1 << 24), end), (p.len(), p.len()));
}
