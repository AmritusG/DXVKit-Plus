//! The committed HAP fixtures (our own CC0 tile pattern, `tests/fixtures/*.attribution`) decoded on
//! the CPU against the source frames: Hap1 / Hap5 / HapA / alpha-only exact to 1/255, HAP Q
//! (HapY, HapM) within 2/255 (its YCoCg chroma is lossy by design). Every decoded frame carries its
//! own index in the top tile row.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::container::{Codec, Movie};
use dxvkit_plus_core::hap;
use dxvkit_plus_core::testpattern::{read_frame_id, tiles};
use std::path::{Path, PathBuf};

const W: u32 = 128;
const H: u32 = 72;
const FRAMES: u32 = 24;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// (max abs diff RGB, max abs diff A, mean abs diff RGB) — colour only where the source alpha > 0.
fn diff(a: &[u8], b: &[u8], with_alpha: bool) -> (u8, u8, f64) {
    let (mut max_c, mut max_a, mut sum, mut n) = (0u8, 0u8, 0u64, 0u64);
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        if with_alpha {
            max_a = max_a.max(p[3].abs_diff(q[3]));
            if q[3] == 0 {
                continue;
            }
        }
        for c in 0..3 {
            let d = p[c].abs_diff(q[c]);
            max_c = max_c.max(d);
            sum += u64::from(d);
            n += 1;
        }
    }
    (max_c, max_a, sum as f64 / n.max(1) as f64)
}

fn opaque(mut v: Vec<u8>) -> Vec<u8> {
    for p in v.chunks_exact_mut(4) {
        p[3] = 255;
    }
    v
}

#[test]
fn hap_fixtures_decode_to_the_source() {
    // (file, has alpha, max RGB diff vs source, max alpha diff)
    let cases: &[(&str, bool, u8, u8)] = &[
        ("tiles_hap1.mov", false, 1, 0),
        ("tiles_hap1_chunks4.mov", false, 1, 0),
        ("tiles_hap5.mov", true, 1, 1),
        ("tiles_hapa.mov", true, 255, 1),
        ("tiles_hapy.mov", false, 2, 0),
        ("tiles_hapm.mov", true, 2, 1),
        ("tiles_hapm_chunks4.mov", true, 2, 1),
    ];
    for &(name, alpha, max_rgb, max_a) in cases {
        let movie = Movie::open(&fixtures().join(name)).unwrap();
        let track = movie.track(Codec::Hap).unwrap().clone();
        assert_eq!((track.width, track.height, track.frame_count()), (W, H, FRAMES), "{name}");
        let (mut worst_c, mut worst_a, mut mean) = (0u8, 0u8, 0f64);
        for f in 0..FRAMES {
            let mut pkt = Vec::new();
            movie.read_sample(&track, f, &mut pkt).unwrap();
            let frame = hap::decode(&pkt, W, H, f % 2 == 0).unwrap();
            let rgba = hap::to_rgba(&frame, W, H).unwrap();
            let src = tiles(f, W, H);
            let (c, a, m) = if name.contains("hapa") {
                // Alpha-only: the decoded grey is the source alpha.
                let got: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[0], p[0], p[0]]).collect();
                let want: Vec<u8> = src.chunks_exact(4).flat_map(|p| [p[3], p[3], p[3], p[3]]).collect();
                let (_, a, _) = diff(&got, &want, true);
                (0, a, 0.0)
            } else if alpha {
                diff(&rgba, &src, true)
            } else {
                diff(&rgba, &opaque(src), false)
            };
            if !name.contains("hapa") {
                assert_eq!(read_frame_id(&rgba, W, H), Some(f), "{name} frame {f}");
            }
            worst_c = worst_c.max(c);
            worst_a = worst_a.max(a);
            mean += m / f64::from(FRAMES);
        }
        eprintln!("{name}: vs source max RGB diff {worst_c}, max alpha diff {worst_a}, mean RGB diff {mean:.3}");
        assert!(worst_c <= max_rgb && worst_a <= max_a, "{name}: rgb {worst_c} (<= {max_rgb}), alpha {worst_a} (<= {max_a})");
    }
}

/// Negative control: frame f compared with the source of frame f+1 must exceed the tolerance.
#[test]
fn accuracy_check_detects_the_wrong_frame() {
    let movie = Movie::open(&fixtures().join("tiles_hap5.mov")).unwrap();
    let track = movie.track(Codec::Hap).unwrap().clone();
    let mut pkt = Vec::new();
    movie.read_sample(&track, 3, &mut pkt).unwrap();
    let rgba = hap::to_rgba(&hap::decode(&pkt, W, H, false).unwrap(), W, H).unwrap();
    let (c, _, _) = diff(&rgba, &tiles(4, W, H), true);
    assert!(c > 1, "a wrong frame must not pass the 1/255 check (got {c})");
}

/// Chunked and single-section encodes of the same footage decode to identical blocks.
#[test]
fn chunked_and_single_section_frames_are_identical() {
    for (a, b) in [("tiles_hap1.mov", "tiles_hap1_chunks4.mov"), ("tiles_hapm.mov", "tiles_hapm_chunks4.mov")] {
        let (ma, mb) = (Movie::open(&fixtures().join(a)).unwrap(), Movie::open(&fixtures().join(b)).unwrap());
        let (ta, tb) = (ma.track(Codec::Hap).unwrap().clone(), mb.track(Codec::Hap).unwrap().clone());
        for f in 0..FRAMES {
            let (mut pa, mut pb) = (Vec::new(), Vec::new());
            ma.read_sample(&ta, f, &mut pa).unwrap();
            mb.read_sample(&tb, f, &mut pb).unwrap();
            assert_eq!(hap::decode(&pa, W, H, false).unwrap(), hap::decode(&pb, W, H, true).unwrap(), "{a} vs {b} frame {f}");
        }
    }
}

/// Asset hygiene: every file in `tests/fixtures` is a media file with a `.attribution` sidecar (or
/// that sidecar), every sidecar says CC0-1.0, and its SHA-256 matches the file.
#[test]
fn every_fixture_has_a_cc0_sidecar_with_a_matching_hash() {
    use sha2::{Digest, Sha256};
    let mut media = 0;
    for e in std::fs::read_dir(fixtures()).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        if name.ends_with(".attribution") {
            let asset = fixtures().join(name.trim_end_matches(".attribution"));
            assert!(asset.exists(), "sidecar without asset: {name}");
            continue;
        }
        media += 1;
        let sidecar = std::fs::read_to_string(fixtures().join(format!("{name}.attribution"))).unwrap_or_else(|_| panic!("{name}: no sidecar"));
        assert!(sidecar.lines().any(|l| l == "license: CC0-1.0"), "{name}: not CC0");
        let digest: String = Sha256::digest(std::fs::read(&p).unwrap()).iter().map(|b| format!("{b:02x}")).collect();
        assert!(sidecar.lines().any(|l| l == format!("sha256: {digest}")), "{name}: sha256 mismatch");
    }
    assert_eq!(media, 19);
}
