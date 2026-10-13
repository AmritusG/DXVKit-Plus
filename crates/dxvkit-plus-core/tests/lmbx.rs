//! LMBX (profile 0x10): the CC0 conformance vectors in `tests/fixtures/lmbx_*.lmbd` decode to the
//! normative hashes in their sidecars; the container and bitstream reject what they must
//! (prototype files, malformed headers and tables) and stay deterministic on hostile tile data;
//! tiles decode independently.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::lmbd::{Header, LmbdFile, Profile};
use dxvkit_plus_core::lmbx::{self, Chroma, Frame};
use dxvkit_plus_core::source::{self, ImportOptions, Shading, SourceKind};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// (file, sidecar params, per-frame normative hashes)
fn vectors() -> Vec<(PathBuf, String, Vec<String>)> {
    let mut v: Vec<_> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("lmbx_") && p.extension().and_then(|e| e.to_str()) == Some("lmbd"))
        .map(|p| {
            let side = std::fs::read_to_string(p.with_extension("lmbd.attribution")).unwrap();
            let params = side.lines().find_map(|l| l.strip_prefix("params: ")).unwrap().to_string();
            let hashes =
                side.lines().filter_map(|l| l.strip_prefix("decoded_rgba_q_sha256 frame ")).map(|l| l.split(": ").nth(1).unwrap().to_string()).collect();
            (p, params, hashes)
        })
        .collect();
    v.sort();
    v
}

fn blob(f: &LmbdFile, n: u32) -> Vec<u8> {
    let (mut s, mut b) = (Vec::new(), Vec::new());
    f.decode_frame(n, &mut s, &mut b, false).unwrap();
    b
}

#[test]
fn conformance_vectors_decode_to_their_hashes() {
    let vs = vectors();
    assert_eq!(vs.len(), 14);
    let (mut frames, mut modes, mut alpha) = (0, std::collections::BTreeSet::new(), 0);
    let (mut versions, mut tile_modes, mut alpha_modes) =
        (std::collections::BTreeSet::new(), std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for (path, params, hashes) in &vs {
        let f = LmbdFile::open(path).unwrap();
        let h = f.header().clone();
        assert_eq!(h.profile, Profile::Lmbx, "{params}");
        assert_eq!(f.frame_count() as usize, hashes.len());
        for (n, want) in hashes.iter().enumerate() {
            let b = blob(&f, n as u32);
            let fr = Frame::parse(&b, h.width, h.height).unwrap();
            modes.insert(fr.header.chroma as u32);
            versions.insert(fr.header.version);
            tile_modes.extend((0..fr.header.tile_count()).map(|t| fr.mode(t) as u32));
            alpha_modes.extend((0..fr.header.tile_count()).map(|t| (fr.mode(t) as u32, fr.alpha_mode(t) as u32)));
            alpha += usize::from(fr.header.alpha);
            for parallel in [false, true] {
                let q = lmbx::decode_rgba_q(&b, h.width, h.height, parallel).unwrap();
                let bytes: Vec<u8> = q.iter().flat_map(|v| v.to_le_bytes()).collect();
                assert_eq!(&sha(&bytes), want, "{} frame {n} parallel {parallel}", path.display());
            }
            frames += 1;
        }
    }
    assert!(frames == 30 && modes.len() == 2 && alpha == 22, "{frames} frames, modes {modes:?}, {alpha} alpha frames");
    assert_eq!((versions.len(), tile_modes.len()), (3, 4), "bitstream versions {versions:?}, tile modes {tile_modes:?}");
    // v3: every (colour mode, alpha mode) the format has — DCT, DCT + alpha palette, flat, palette,
    // palette + edge, quad, quad + edge.
    for m in [(0, 0), (0, 2), (1, 0), (2, 0), (2, 1), (3, 0), (3, 1)] {
        assert!(alpha_modes.contains(&m), "(colour, alpha) mode {m:?} in no vector: {alpha_modes:?}");
    }
}

/// Through `source`: probe names the profile, frames come out as one bitstream plane with
/// `Shading::Lmbx`, the CPU RGBA is the 8-bit form of the normative output, and the tile vectors
/// carry their frame index.
#[test]
fn vectors_through_source() {
    for (path, params, _) in vectors() {
        let info = source::probe(path.to_str().unwrap(), &ImportOptions::default()).unwrap();
        assert_eq!((info.kind, info.codec.as_str()), (SourceKind::Lmbd, "LMBX"));
        let mut src = source::open(&info, &ImportOptions::default(), true).unwrap();
        assert!(src.layout().is_none(), "LMBX frames vary in size");
        for n in 0..info.frames {
            let d = src.decode(n).unwrap();
            let dxvkit_plus_core::source::Decoded::Planes { shading, planes, .. } = &d else { panic!() };
            assert_eq!((*shading, planes.len()), (Shading::Lmbx, 1));
            let rgba = d.to_rgba().unwrap();
            let q = lmbx::decode_rgba_q(&planes[0].data, info.width, info.height, false).unwrap();
            assert_eq!(rgba, q.iter().map(|&v| lmbx::to_8bit(v)).collect::<Vec<u8>>());
            if (params.contains("tiles") || params.contains("plasma")) && !params.contains("modes=") {
                assert_eq!(dxvkit_plus_core::testpattern::read_frame_id(&rgba, info.width, info.height), Some(n), "{params} frame {n}");
            }
        }
    }
}

/// Tiles decode independently: wrecking every stream but tile `keep`'s changes nothing in that
/// tile's samples (every plane).
#[test]
fn tiles_are_independent() {
    for (path, _, _) in vectors().iter().filter(|v| {
        let n = v.0.to_str().unwrap();
        n.contains("plasma_alpha_422") || n.contains("mixed") || n.contains("v3_graphics_big") || n.contains("v3_quad_edge")
    }) {
        tile_independence(path);
    }
}

fn tile_independence(path: &std::path::Path) {
    let f = LmbdFile::open(path).unwrap();
    let h = f.header().clone();
    let good = blob(&f, 0);
    let fr = Frame::parse(&good, h.width, h.height).unwrap();
    let full = lmbx::decode_planes(&fr, false);
    let planes = fr.header.planes() as usize;
    let tiles = fr.header.tile_count() as usize;
    for keep in 0..tiles {
        let mut b = good.clone();
        for st in (0..tiles * planes).filter(|&st| st / planes != keep) {
            let s = fr.sections.data + fr.offsets[st] as usize;
            let e = fr.offsets.get(st + 1).map_or(b.len(), |&o| fr.sections.data + o as usize);
            b[s..e].iter_mut().for_each(|x| *x = !*x);
        }
        let f2 = Frame::parse(&b, h.width, h.height).unwrap();
        let p = lmbx::decode_planes(&f2, false);
        let t = lmbx::TILE;
        let (tx, ty) = (keep as u32 % fr.header.tiles_x(), keep as u32 / fr.header.tiles_x());
        for y in ty * t..ty * t + t {
            for x in tx * t..tx * t + t {
                let i = (y * full.coded_width + x) as usize;
                assert_eq!((p.y[i], p.a.get(i)), (full.y[i], full.a.get(i)), "tile {keep} at {x},{y}");
            }
            let cw = fr.header.chroma.tile_width();
            for x in tx * cw..tx * cw + cw {
                let i = (y * full.chroma_width + x) as usize;
                assert_eq!((p.co[i], p.cg[i]), (full.co[i], full.cg[i]), "tile {keep} chroma at {x},{y}");
            }
        }
    }
}

/// Hostile data never panics and is deterministic: truncated tiles read zeros, random tile bytes
/// decode to something, the same every time.
#[test]
fn hostile_tile_data_is_deterministic() {
    let (path, _, _) = &vectors()[2];
    let f = LmbdFile::open(path).unwrap();
    let h = f.header().clone();
    let good = blob(&f, 0);
    let fr = Frame::parse(&good, h.width, h.height).unwrap();
    let mut b = good[..fr.sections.data].to_vec();
    b.extend(std::iter::repeat_n(0u8, 0)); // every tile empty: reads zeros
    // Rebuild the directory so every tile is empty (all offsets 0).
    let nt = fr.offsets.len();
    for t in 0..nt {
        let at = fr.sections.directory + 4 + t * 4;
        b[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    let a = lmbx::decode_rgba_q(&b, h.width, h.height, true).unwrap();
    assert_eq!(a, lmbx::decode_rgba_q(&b, h.width, h.height, false).unwrap());
    let mut r = good.clone();
    let mut state = 1u64;
    for x in r[fr.sections.data..].iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x = (state >> 56) as u8;
    }
    assert_eq!(lmbx::decode_rgba_q(&r, h.width, h.height, true).unwrap(), lmbx::decode_rgba_q(&r, h.width, h.height, false).unwrap());
    // v2: every tile given a random (valid) mode over random data, and every stream empty.
    let (path, _, _) = vectors().into_iter().find(|v| v.0.to_str().unwrap().contains("mixed")).unwrap();
    let f = LmbdFile::open(&path).unwrap();
    let h = f.header().clone();
    let good = blob(&f, 0);
    let fr = Frame::parse(&good, h.width, h.height).unwrap();
    let mut r = good.clone();
    for t in 0..fr.header.tile_count() as usize {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        r[fr.sections.modes + t] = ((state >> 60) % 3) as u8;
    }
    for x in r[fr.sections.data..].iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x = (state >> 56) as u8;
    }
    assert_eq!(lmbx::decode_rgba_q(&r, h.width, h.height, true).unwrap(), lmbx::decode_rgba_q(&r, h.width, h.height, false).unwrap());
    let mut e = r[..fr.sections.data].to_vec();
    for t in 0..fr.offsets.len() {
        let at = fr.sections.directory + 4 + t * 4;
        e[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    assert_eq!(lmbx::decode_rgba_q(&e, h.width, h.height, true).unwrap(), lmbx::decode_rgba_q(&e, h.width, h.height, false).unwrap());
    // v3: every tile a random valid mode byte (every colour and alpha mode) over random data, and
    // every stream empty.
    for name in ["v3_graphics_big", "v3_quad_edge"] {
        let (path, _, _) = vectors().into_iter().find(|v| v.0.to_str().unwrap().contains(name)).unwrap();
        let f = LmbdFile::open(&path).unwrap();
        let h = f.header().clone();
        let good = blob(&f, 0);
        let fr = Frame::parse(&good, h.width, h.height).unwrap();
        let valid = [0x00u8, 0x20, 0x01, 0x02, 0x12, 0x03, 0x13];
        let mut r = good.clone();
        for t in 0..fr.header.tile_count() as usize {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            r[fr.sections.modes + t] = valid[((state >> 33) % valid.len() as u64) as usize];
        }
        for x in r[fr.sections.data..].iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *x = (state >> 56) as u8;
        }
        assert_eq!(lmbx::decode_rgba_q(&r, h.width, h.height, true).unwrap(), lmbx::decode_rgba_q(&r, h.width, h.height, false).unwrap());
        let mut e = r[..fr.sections.data].to_vec();
        for t in 0..fr.offsets.len() {
            let at = fr.sections.directory + 4 + t * 4;
            e[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        assert_eq!(lmbx::decode_rgba_q(&e, h.width, h.height, true).unwrap(), lmbx::decode_rgba_q(&e, h.width, h.height, false).unwrap());
    }
}

/// What must be refused: the never-frozen prototype (container texture format 0x10, bitstream
/// "LMXS") as `Unsupported`; bad bitstream headers, tables and directories as errors.
#[test]
fn refusals() {
    let (path, _, _) = &vectors()[0];
    let bytes = std::fs::read(path).unwrap();
    // Prototype container: texture format 0x10.
    let mut proto = bytes.clone();
    proto[7] = 0x10;
    assert!(matches!(LmbdFile::from_source(std::sync::Arc::new(dxvkit_plus_core::container::MemSource(proto.into()))).err(), Some(MediaError::Unsupported(_))));
    // Wrong pairing: LMB7 profile with the LMBX format, and the reverse.
    let mut mix = bytes.clone();
    mix[6] = 0x01;
    assert!(matches!(Header::parse(&mix).err(), Some(MediaError::Lmbd(_))));
    let f = LmbdFile::open(path).unwrap();
    let h = f.header().clone();
    let good = blob(&f, 0);
    let parse = |b: &[u8]| Frame::parse(b, h.width, h.height).err();
    assert!(parse(&good).is_none());
    let mut p = good.clone();
    p[..4].copy_from_slice(b"LMXS");
    assert!(matches!(parse(&p), Some(MediaError::Unsupported(_))), "prototype bitstream");
    let mut v = good.clone();
    v[4] = 4;
    assert!(matches!(parse(&v), Some(MediaError::Unsupported(_))), "future version");
    for (at, val, what) in [(6usize, 2u8, "chroma"), (7, 2, "flags"), (8, 0, "width"), (18, 1, "reserved"), (20, 0, "q step 0")] {
        let mut c = good.clone();
        c[at] = val;
        if at == 20 {
            c[21] = 0;
        }
        assert!(matches!(parse(&c), Some(MediaError::Lmbd(_))), "{what}");
    }
    let fr = Frame::parse(&good, h.width, h.height).unwrap();
    let mut t = good.clone();
    t[fr.sections.coef_table + 4] ^= 1; // a frequency: the table no longer adds up to 16384
    assert!(parse(&t).is_some(), "table sum");
    let mut d = good.clone();
    d[fr.sections.directory] ^= 1; // tile count
    assert!(parse(&d).is_some(), "tile count");
    if fr.offsets.len() > 2 {
        let mut o = good.clone();
        let at = fr.sections.directory + 4 + 8; // tile 2 starts before tile 1
        o[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        let at1 = fr.sections.directory + 4 + 4;
        o[at1..at1 + 4].copy_from_slice(&5u32.to_le_bytes());
        assert!(parse(&o).is_some(), "offsets decreasing");
    }
    assert!(parse(&good[..40]).is_some(), "truncated");
    let _ = Chroma::Halo422;
    // v2: an unknown tile mode; stream lengths running past the data.
    let (path, _, _) = vectors().into_iter().find(|v| v.0.to_str().unwrap().contains("mixed")).unwrap();
    let f = LmbdFile::open(&path).unwrap();
    let h = f.header().clone();
    let good = blob(&f, 0);
    let parse = |b: &[u8]| Frame::parse(b, h.width, h.height).err();
    assert!(parse(&good).is_none());
    let fr = Frame::parse(&good, h.width, h.height).unwrap();
    let mut m = good.clone();
    m[fr.sections.modes] = 3;
    assert!(matches!(parse(&m), Some(MediaError::Lmbd(_))), "tile mode 3");
    let mut l = good.clone();
    let at = fr.sections.directory + 4;
    l[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(parse(&l), Some(MediaError::Lmbd(_))), "stream length past the data");
    // v3: invalid mode bytes (colour mode 4; alpha mode 3; edge on DCT or flat; an alpha palette on
    // a direct tile; edge or alpha palette without an alpha plane), an index table that does not
    // add up to 1024.
    for (name, bad) in [("v3_graphics_big", [0x04u8, 0x30, 0x10, 0x11, 0x22, 0x23].as_slice()), ("v3_graphics_444", [0x12u8, 0x13, 0x20].as_slice())] {
        let (path, _, _) = vectors().into_iter().find(|v| v.0.to_str().unwrap().contains(name)).unwrap();
        let f = LmbdFile::open(&path).unwrap();
        let h = f.header().clone();
        let good = blob(&f, 0);
        let parse = |b: &[u8]| Frame::parse(b, h.width, h.height).err();
        assert!(parse(&good).is_none());
        let fr = Frame::parse(&good, h.width, h.height).unwrap();
        for &m in bad {
            let mut x = good.clone();
            x[fr.sections.modes] = m;
            assert!(matches!(parse(&x), Some(MediaError::Lmbd(_))), "{name}: v3 mode byte {m:#04x}");
        }
        let mut t = good.clone();
        t[fr.sections.tables3[0] + 4] ^= 1;
        assert!(matches!(parse(&t), Some(MediaError::Lmbd(_))), "{name}: v3 index table sum");
    }
}
