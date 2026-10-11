//! Our encoder against the ORIGINAL: each compatibility vector in `tests/fixtures/lmbd_*.lmbd` was
//! written by the original C encoder from a CC0 pattern with the `params:` in its sidecar; encoding
//! the same pattern with the same parameters here must give the same file, byte for byte.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::lmbd::{Compressor, Header};
use dxvkit_plus_core::testpattern;
use dxvkit_plus_lmbd_enc::{EncodeParams, encode_frames};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// The frames `dxvkit-plus gen-pattern` writes (and the mint script encodes).
fn pattern(kind: &str, f: u32, w: u32, h: u32) -> Vec<u8> {
    match kind {
        "tiles" => testpattern::tiles(f, w, h),
        "plasma" | "plasma_alpha" => {
            let mut p = testpattern::plasma(f, w, h, kind == "plasma_alpha");
            testpattern::stamp_frame_id(&mut p, w, h, f);
            p
        }
        other => panic!("pattern {other}"),
    }
}

#[test]
fn encoding_the_vectors_patterns_reproduces_the_original_files() {
    let mut checked = 0;
    for e in std::fs::read_dir(fixtures()).unwrap() {
        let path = e.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if !(name.starts_with("lmbd_") && name.ends_with(".lmbd")) {
            continue;
        }
        let side = std::fs::read_to_string(fixtures().join(format!("{name}.attribution"))).unwrap();
        let line = side.lines().find_map(|l| l.strip_prefix("params: ")).unwrap();
        let kv: HashMap<&str, &str> = line.split(' ').map(|t| t.split_once('=').unwrap()).collect();
        let (w, h, n): (u32, u32, u32) = (kv["width"].parse().unwrap(), kv["height"].parse().unwrap(), kv["frames"].parse().unwrap());
        let mut p = EncodeParams::default();
        p.rdo.lambda = kv["lambda"].parse().unwrap();
        p.rdo.lookback = kv["lookback"].parse().unwrap();
        p.chunks = kv["chunks"].parse().unwrap();
        p.zstd_level = kv["level"].parse().unwrap();
        p.compressor = if kv["compressor"] == "none" { Compressor::None } else { Compressor::Zstd };
        let kind = kv["pattern"];
        let mut out = Cursor::new(Vec::new());
        let threads = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(4);
        encode_frames(&mut out, Header::new(w, h, 30, 1), &p, threads, |i| (i < n).then(|| Ok(pattern(kind, i, w, h))), |_, _| {}).unwrap();
        let ours = out.into_inner();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(ours.len(), original.len(), "{name}: size");
        let first_diff = ours.iter().zip(&original).position(|(a, b)| a != b);
        assert_eq!(first_diff, None, "{name}: first differing byte");
        checked += 1;
    }
    assert_eq!(checked, 5);
}

/// The thread count never changes the output (frames are independent; blocks are independent).
#[test]
fn output_does_not_depend_on_threads() {
    let p = EncodeParams::default();
    let enc = |threads: usize| {
        let mut out = Cursor::new(Vec::new());
        encode_frames(&mut out, Header::new(68, 36, 30, 1), &p, threads, |i| (i < 3).then(|| Ok(pattern("plasma", i, 68, 36))), |_, _| {}).unwrap();
        out.into_inner()
    };
    assert_eq!(enc(1), enc(7));
}

/// Negative control: a different lambda gives a different file (the identity test is not vacuous).
#[test]
fn a_different_lambda_changes_the_file() {
    let enc = |lambda: f32| {
        let mut p = EncodeParams::default();
        p.rdo.lambda = lambda;
        let mut out = Cursor::new(Vec::new());
        encode_frames(&mut out, Header::new(64, 32, 30, 1), &p, 2, |i| (i < 1).then(|| Ok(pattern("plasma", i, 64, 32))), |_, _| {}).unwrap();
        out.into_inner()
    };
    assert_ne!(enc(1.0), enc(2.0));
}
