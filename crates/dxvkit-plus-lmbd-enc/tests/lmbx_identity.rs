//! The LMBX encoder is deterministic: re-encoding each conformance vector's pattern with the
//! parameters in its sidecar (v2: with its tile modes) writes the same file at any thread count,
//! pinned by the sidecar's `reencoded_sha256`. The committed vectors are the 0.5.0 / 0.6.0
//! encoders' output and stay as they are (decoder conformance); 0.6.1's encoder (W6b: the exact
//! forward YCoCg-R and the colour guard) writes different, colour-accurate files from the same
//! patterns.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::lmbd::{ChunkData, Compressor, Header, Writer, frame_payload};
use dxvkit_plus_core::lmbx::Chroma;
use dxvkit_plus_core::testpattern;
use dxvkit_plus_lmbd_enc::lmbx::{LmbxParams, encode_frame};
use sha2::{Digest, Sha256};
use std::path::Path;

fn pattern(name: &str, f: u32) -> (Vec<u8>, u32, u32) {
    match name {
        "tiles" => (testpattern::tiles(f, 200, 130), 200, 130),
        "plasma_a" | "plasma_odd" => {
            let (w, h, alpha) = if name == "plasma_a" { (333, 141, true) } else { (97, 61, false) };
            let mut p = testpattern::plasma(f, w, h, alpha);
            testpattern::stamp_frame_id(&mut p, w, h, f);
            (p, w, h)
        }
        "graphics" | "graphics_a" => (testpattern::graphics(f, 333, 185, name == "graphics_a"), 333, 185),
        other => panic!("{other}"),
    }
}

#[test]
fn reencoding_the_vectors_is_deterministic_and_pinned() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let mut checked = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        if !(name.starts_with("lmbx_") && name.ends_with(".lmbd")) {
            continue;
        }
        let side = std::fs::read_to_string(dir.join(format!("{name}.attribution"))).unwrap();
        let params = side.lines().find_map(|l| l.strip_prefix("params: ")).unwrap();
        let kv = |k: &str| params.split(' ').find_map(|t| t.strip_prefix(&format!("{k}="))).unwrap_or_default().to_string();
        let modes = kv("modes");
        let has = |m: &str| modes.split(',').any(|x| x == m);
        let tile_modes = LmbxParams { flat: has("flat"), palette: has("palette"), palette_lossy: has("lossy"), ..LmbxParams::default() };
        let frames = side.lines().filter(|l| l.starts_with("decoded_rgba_q_sha256")).count() as u32;
        let chroma = if kv("chroma") == "444" { Chroma::Full444 } else { Chroma::Halo422 };
        let quality: f64 = kv("quality").parse().unwrap();
        let want = side.lines().find_map(|l| l.strip_prefix("reencoded_sha256: ")).unwrap().split(' ').next().unwrap().to_string();
        for threads in [1, 3, 8] {
            let (_, w, h) = pattern(&kv("pattern"), 0);
            let header = Header::new_lmbx(w, h, 30, 1);
            let mut out = std::io::Cursor::new(Vec::new());
            let mut wr = Writer::new(&mut out, header.clone()).unwrap();
            for f in 0..frames {
                let (rgba, w, h) = pattern(&kv("pattern"), f);
                let b = encode_frame(&rgba, w, h, &LmbxParams { chroma, quality, threads, ..tile_modes }).unwrap();
                let payload = frame_payload(&header, &[ChunkData { compressor: Compressor::None, raw_size: b.len() as u32, data: &b }]).unwrap();
                wr.push_payload(&payload).unwrap();
            }
            wr.finish().unwrap();
            let got: String = Sha256::digest(out.into_inner()).iter().map(|x| format!("{x:02x}")).collect();
            assert_eq!(got, want, "{name} with {threads} threads");
        }
        checked += 1;
    }
    assert_eq!(checked, 9);
}
