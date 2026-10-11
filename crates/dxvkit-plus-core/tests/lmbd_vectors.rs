//! LMBD compatibility: every vector in `tests/fixtures/lmbd_*.lmbd` was written by the ORIGINAL C
//! encoder (`scripts/mint-lmbd-vectors.sh`); each frame must decode here to exactly the RGBA the
//! original decoder produced (its SHA-256 is in the sidecar).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::lmbd::{Compressor, LmbdFile};
use dxvkit_plus_core::source::{self, ImportOptions, SourceKind};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// (file, the per-frame RGBA SHA-256 the original decoder produced)
fn vectors() -> Vec<(PathBuf, Vec<String>)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(fixtures()).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        if !(name.starts_with("lmbd_") && name.ends_with(".lmbd")) {
            continue;
        }
        let side = std::fs::read_to_string(fixtures().join(format!("{name}.attribution"))).unwrap();
        let hashes = side.lines().filter_map(|l| l.strip_prefix("decoded_rgba_sha256 frame ")).map(|l| l.split(": ").nth(1).unwrap().to_string()).collect();
        v.push((p, hashes));
    }
    v.sort();
    v
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn original_encoder_files_decode_bit_exact() {
    let vs = vectors();
    assert_eq!(vs.len(), 5);
    let mut frames = 0;
    for (path, want) in &vs {
        let info = source::probe(path.to_str().unwrap(), &ImportOptions::default()).unwrap();
        assert_eq!(info.kind, SourceKind::Lmbd);
        assert_eq!(info.frames as usize, want.len(), "{path:?}");
        for parallel in [false, true] {
            let mut src = source::open(&info, &ImportOptions::default(), parallel).unwrap();
            for (f, h) in want.iter().enumerate() {
                let d = src.decode(f as u32).unwrap();
                let rgba = d.to_rgba().unwrap();
                assert_eq!(rgba.len(), (info.width * info.height * 4) as usize);
                assert_eq!(&sha(&rgba), h, "{path:?} frame {f} (parallel {parallel})");
            }
        }
        frames += want.len();
    }
    assert_eq!(frames, 13);
}

#[test]
fn vector_headers_and_chunk_tables() {
    let f = LmbdFile::open(&fixtures().join("lmbd_plasma_alpha_odd_l05_c3.lmbd")).unwrap();
    let h = f.header();
    assert_eq!((h.width, h.height, h.fps_num, h.fps_den, h.frame_count), (130, 74, 30, 1, 2));
    assert_eq!(h.coded_size(), (132, 76));
    assert_eq!(h.frame_bytes(), 33 * 19 * 16);
    let c = f.chunks(1).unwrap();
    assert_eq!(c.len(), 3);
    assert!(c.iter().all(|c| c.compressor == Compressor::Zstd));
    let none = LmbdFile::open(&fixtures().join("lmbd_tiles_none_c2.lmbd")).unwrap();
    assert!(none.chunks(0).unwrap().iter().all(|c| c.compressor == Compressor::None && c.raw_size == c.packed_size));
}

/// Negative control: a single flipped byte inside a Zstd chunk must not decode to the same frame.
#[test]
fn a_corrupted_chunk_never_decodes_silently_to_the_same_frame() {
    let path = fixtures().join("lmbd_plasma_l1_c1.lmbd");
    let mut bytes = std::fs::read(&path).unwrap();
    let f = LmbdFile::from_source(std::sync::Arc::new(dxvkit_plus_core::container::MemSource(bytes.clone().into()))).unwrap();
    let (mut s, mut good) = (Vec::new(), Vec::new());
    f.decode_frame(0, &mut s, &mut good, false).unwrap();
    let e = f.index()[0];
    let at = (e.offset as usize) + 3 + 13 + 40; // inside the first chunk's data
    bytes[at] ^= 0x5A;
    let f = LmbdFile::from_source(std::sync::Arc::new(dxvkit_plus_core::container::MemSource(bytes.into()))).unwrap();
    let mut out = Vec::new();
    match f.decode_frame(0, &mut s, &mut out, false) {
        Err(_) => {}
        Ok(()) => assert_ne!(out, good, "corruption went unnoticed"),
    }
}
