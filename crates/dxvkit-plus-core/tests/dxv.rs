//! DXV decoder vs DXVKit (the C library it is ported from).
//!
//! - DXVKit's test vectors: every `decode` row gives byte-identical block streams (SHA-256 of each
//!   plane) and RGBA; every `reject` row is rejected without a panic. The committed MIT subset in
//!   `tests/fixtures/dxvkit/` (GlEnc, synthetic and invalid groups: 74 rows) always runs; with
//!   `DXVKIT_DIR=<a DXVKit checkout>` the FULL set (334 rows, including the Resolume-encoder
//!   streams, which are never committed here) runs too.
//! - With `DXVTOOL=<a built dxvtool>`: a live differential against the C core on the CC0 tile
//!   fixtures and the committed vectors — block streams, straight RGBA and premultiplied RGBA.
//! - The alpha modes: straight is the stored colour; premultiplied un-premultiplies exactly.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::container::{Codec, Movie};
use dxvkit_plus_core::dxv::{self, rgba::blocks_to_rgba};
use dxvkit_plus_core::source::{self, AlphaMode, Decoded, ImportOptions, Shading, SourceKind};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

#[derive(Debug)]
struct Row {
    vector: String,
    group: String,
    frame: u32,
    variant: String,
    expected: String,
    hashes: Vec<(String, String)>,
}

fn rows(tsv: &Path) -> Vec<Row> {
    let text = std::fs::read_to_string(tsv).unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let hashes = f
                .get(9)
                .map(|h| h.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
                .unwrap_or_default();
            Row { vector: f[0].into(), group: f[1].into(), frame: f[2].parse().unwrap(), variant: f[6].into(), expected: f[8].into(), hashes }
        })
        .collect()
}

fn packet(path: &Path, frame: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let movie = Movie::open(path).map_err(|e| e.to_string())?;
    let track = movie.track(Codec::Dxv).ok_or("no DXV video track")?.clone();
    let mut p = Vec::new();
    movie.read_sample(&track, frame, &mut p).map_err(|e| e.to_string())?;
    Ok((p, track.width, track.height))
}

/// The reference kinds of a variant's planes (DXVKit's `dump-blocks` suffixes).
fn plane_kinds(v: dxv::Variant) -> &'static [&'static str] {
    match v {
        dxv::Variant::Dxt1 | dxv::Variant::LegacyDxt1 => &["bc1"],
        dxv::Variant::Dxt5 | dxv::Variant::LegacyDxt5 => &["bc3"],
        dxv::Variant::Ycg6 => &["y.bc4", "chroma.bc5"],
        dxv::Variant::Yg10 => &["ya.bc5", "chroma.bc5"],
    }
}

/// What one row decoded to: per-plane hashes + RGBA hash, or the rejection.
fn decode_row(dir: &Path, r: &Row) -> Result<Vec<(String, String)>, String> {
    let (p, w, h) = packet(&dir.join("streams").join(&r.group).join(format!("{}.mov", r.vector)), r.frame)?;
    let blocks = dxv::decode_blocks(&p, w, h, true).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let names: &[&str] = if blocks.layout.variant.is_hq() { &["plane-a", "plane-b"] } else { &[] };
    for (i, kind) in plane_kinds(blocks.layout.variant).iter().enumerate() {
        out.push((names.get(i).copied().unwrap_or(kind).to_string(), hex(&blocks.planes[i])));
    }
    out.push(("rgba".to_string(), hex(&blocks_to_rgba(&blocks, false).map_err(|e| e.to_string())?)));
    Ok(out)
}

struct Tally {
    rows: usize,
    planes: usize,
    rgba: usize,
    rejected: usize,
    failures: Vec<String>,
}

fn run(dir: &Path) -> Tally {
    let mut t = Tally { rows: 0, planes: 0, rgba: 0, rejected: 0, failures: Vec::new() };
    for r in rows(&dir.join("vectors.tsv")) {
        t.rows += 1;
        let got = std::panic::catch_unwind(|| decode_row(dir, &r)).unwrap_or_else(|_| Err("PANIC".into()));
        match (r.expected.as_str(), got) {
            ("decode", Ok(got)) => {
                for (k, want) in &r.hashes {
                    match got.iter().find(|(gk, _)| gk == k) {
                        Some((_, h)) if h == want => {
                            if k == "rgba" {
                                t.rgba += 1
                            } else {
                                t.planes += 1
                            }
                        }
                        other => t.failures.push(format!("{} f{} {} {k}: want {want}, got {other:?}", r.vector, r.frame, r.variant)),
                    }
                }
            }
            ("decode", Err(e)) => t.failures.push(format!("{} f{}: expected decode, got error {e}", r.vector, r.frame)),
            ("reject", Err(e)) if e != "PANIC" => t.rejected += 1,
            ("reject", other) => t.failures.push(format!("{} f{}: expected reject, got {other:?}", r.vector, r.frame)),
            (x, _) => t.failures.push(format!("{}: unknown expectation {x}", r.vector)),
        }
    }
    t
}

#[test]
fn committed_dxvkit_vectors_decode_bit_exact() {
    let t = run(&fixtures().join("dxvkit"));
    assert!(t.failures.is_empty(), "{} failures:\n{}", t.failures.len(), t.failures.join("\n"));
    // 74 rows: 64 decode rows (86 block streams: HQ rows carry two planes; 64 RGBA), 10 rejects.
    assert_eq!((t.rows, t.planes, t.rgba, t.rejected), (74, 86, 64, 10));
    eprintln!("committed DXVKit vectors: rows {} | block streams {} identical | rgba {} identical | rejected {}", t.rows, t.planes, t.rgba, t.rejected);
}

/// The full 334-row set (needs a DXVKit checkout). Without `DXVKIT_DIR` this test checks nothing and
/// says so.
#[test]
fn full_dxvkit_vectors_decode_bit_exact_when_available() {
    let Ok(dir) = std::env::var("DXVKIT_DIR") else {
        eprintln!("DXVKIT_DIR not set: the full 334-row run is skipped (the committed subset ran above)");
        return;
    };
    let t = run(&Path::new(&dir).join("test_vectors"));
    eprintln!(
        "FULL DXVKit vectors: rows {} | block streams {} identical | rgba {} identical | rejected {} | failures {}",
        t.rows,
        t.planes,
        t.rgba,
        t.rejected,
        t.failures.len()
    );
    assert!(t.failures.is_empty(), "{}", t.failures.join("\n"));
    assert_eq!((t.rows, t.planes, t.rgba, t.rejected), (334, 366, 287, 47));
}

/// Negative control: one flipped payload byte in a valid packet must change the block stream or be
/// rejected — the hash comparison can go RED.
#[test]
fn a_corrupted_packet_does_not_match_its_vector() {
    let dir = fixtures().join("dxvkit");
    let r = rows(&dir.join("vectors.tsv")).into_iter().find(|r| r.vector == "glenc_ycg6_pat64" && r.frame == 0).unwrap();
    let (mut p, w, h) = packet(&dir.join("streams/glenc/glenc_ycg6_pat64.mov"), 0).unwrap();
    let want_a = &r.hashes.iter().find(|(k, _)| k == "plane-a").unwrap().1;
    let want_b = &r.hashes.iter().find(|(k, _)| k == "plane-b").unwrap().1;
    let ok = dxv::decode_blocks(&p, w, h, false).unwrap();
    assert_eq!(&hex(&ok.planes[0]), want_a);
    let last = p.len() - 9;
    p[last] ^= 0x5A;
    if let Ok(b) = dxv::decode_blocks(&p, w, h, false) {
        assert!(hex(&b.planes[0]) != *want_a || hex(&b.planes[1]) != *want_b, "corruption went unnoticed");
    }
}

/// The DXV files the live differential runs on: the CC0 tile fixtures and the committed vectors
/// that decode.
fn differential_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["dxt1", "dxt5", "ycg6", "yg10"].iter().map(|n| fixtures().join(format!("tiles_{n}.mov"))).collect();
    let dir = fixtures().join("dxvkit");
    let mut seen = std::collections::BTreeSet::new();
    for r in rows(&dir.join("vectors.tsv")).into_iter().filter(|r| r.expected == "decode") {
        if seen.insert(r.vector.clone()) {
            v.push(dir.join("streams").join(&r.group).join(format!("{}.mov", r.vector)));
        }
    }
    v
}

/// Live differential against DXVKit's C core (`DXVTOOL=<built dxvtool>`): for every frame of every
/// file, the block streams (`dump-blocks`), straight RGBA (`dump-rgba`) and premultiplied RGBA
/// (`dump-rgba --premultiply`) are byte-identical to ours.
#[test]
fn equals_the_dxvkit_c_core_when_available() {
    let Ok(tool) = std::env::var("DXVTOOL") else {
        eprintln!("DXVTOOL not set: the live C-core differential is skipped");
        return;
    };
    let tmp = std::env::temp_dir().join(format!("dxvkit-plus-dxv-diff-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let (mut files, mut frames, mut planes, mut rgba) = (0, 0, 0, 0);
    for f in differential_files() {
        let movie = Movie::open(&f).unwrap();
        let track = movie.track(Codec::Dxv).unwrap().clone();
        files += 1;
        for frame in 0..track.frame_count() {
            let (p, w, h) = packet(&f, frame).unwrap();
            let b = dxv::decode_blocks(&p, w, h, true).unwrap();
            let prefix = tmp.join("f");
            let run = |args: &[&str]| {
                let out = std::process::Command::new(&tool).args(args).output().unwrap();
                assert!(out.status.success(), "{tool} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            };
            let (fs, ps) = (f.to_str().unwrap(), frame.to_string());
            run(&["dump-blocks", fs, &ps, prefix.to_str().unwrap()]);
            for (i, kind) in plane_kinds(b.layout.variant).iter().enumerate() {
                let c = std::fs::read(format!("{}.{kind}", prefix.display())).unwrap();
                assert!(c == b.planes[i], "{} frame {frame} {kind}: block stream differs from the C core", f.display());
                planes += 1;
            }
            for premultiply in [false, true] {
                let out = tmp.join("f.rgba");
                let mut args = vec!["dump-rgba", fs, &ps, out.to_str().unwrap()];
                if premultiply {
                    args.push("--premultiply");
                }
                run(&args);
                let c = std::fs::read(&out).unwrap();
                assert!(c == blocks_to_rgba(&b, premultiply).unwrap(), "{} frame {frame} rgba (premultiply {premultiply}) differs", f.display());
                rgba += 1;
            }
            frames += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("DXVKit C core vs ours: {files} files, {frames} frames, {planes} block streams and {rgba} RGBA images identical");
    assert!(files >= 20 && frames >= 100, "{files} files / {frames} frames");
}

/// Probe + open + decode through `source`, both alpha modes: straight shows the stored colour;
/// premultiplied un-premultiplies it exactly.
#[test]
fn alpha_modes_through_source() {
    for (name, alpha) in [("tiles_dxt1.mov", false), ("tiles_dxt5.mov", true), ("tiles_ycg6.mov", false), ("tiles_yg10.mov", true)] {
        let path = fixtures().join(name);
        let info = source::probe(path.to_str().unwrap(), &ImportOptions::default()).unwrap();
        assert_eq!((info.kind, info.has_alpha, info.frames, info.width, info.height), (SourceKind::Dxv, alpha, 24, 128, 72), "{name}");
        let mut straight = source::open(&info, &ImportOptions::default(), false).unwrap();
        let mut premul = source::open(&info, &ImportOptions { alpha_mode: AlphaMode::Premultiplied, ..Default::default() }, true).unwrap();
        let layout = straight.layout().unwrap();
        for f in [0, 11, 23] {
            let (s, p) = (straight.decode(f).unwrap(), premul.decode(f).unwrap());
            let Decoded::Planes { shading, planes, .. } = &s else { panic!() };
            assert!(layout.matches(128, 72, *shading, planes), "{name}");
            let (rs, rp) = (s.to_rgba().unwrap(), p.to_rgba().unwrap());
            assert_eq!(dxvkit_plus_core::testpattern::read_frame_id(&rs, 128, 72), Some(f), "{name}");
            for (a, b) in rs.chunks_exact(4).zip(rp.chunks_exact(4)) {
                let want = if a[3] == 0 { [a[0], a[1], a[2]] } else { [0, 1, 2].map(|i| dxvkit_plus_core::bc::unpremul(a[i], a[3])) };
                assert_eq!([b[0], b[1], b[2], b[3]], [want[0], want[1], want[2], a[3]], "{name} frame {f}");
            }
        }
        let Decoded::Planes { shading, .. } = premul.decode(0).unwrap() else { panic!() };
        let want = match name {
            "tiles_dxt5.mov" => Shading::RgbaPremultiplied,
            "tiles_yg10.mov" => Shading::DxvYcocg { alpha: true, premultiplied: true },
            "tiles_ycg6.mov" => Shading::DxvYcocg { alpha: false, premultiplied: false },
            _ => Shading::Rgba { opaque: true },
        };
        assert_eq!(shading, want, "{name}");
    }
}

/// `unpremul` against its definition on every (c, a), and premul -> unpremul round trip within the
/// quantisation step.
#[test]
fn unpremul_is_min_255_round_c_255_over_a() {
    use dxvkit_plus_core::bc::{premul, unpremul};
    for a in 0..=255u32 {
        for c in 0..=255u32 {
            let want = if a == 0 { c } else { ((f64::from(c) * 255.0 / f64::from(a)).round() as u32).min(255) };
            assert_eq!(u32::from(unpremul(c as u8, a as u8)), want, "c {c} a {a}");
            if a > 0 {
                let back = unpremul(premul(c as u8, a as u8), a as u8);
                assert!(u32::from(back).abs_diff(c) <= 255 / a + 1, "c {c} a {a} -> {back}");
            }
        }
    }
}

/// The invalid vectors fail cleanly through `source` too (an error value, never a panic).
#[test]
fn dxv_reject_vectors_fail_cleanly_through_source() {
    let dir = fixtures().join("dxvkit/streams/invalid");
    let mut checked = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("mov") {
            continue;
        }
        let r = std::panic::catch_unwind(|| {
            let info = source::probe(p.to_str().unwrap(), &ImportOptions::default())?;
            source::open(&info, &ImportOptions::default(), true)?.decode(0)
        });
        assert!(matches!(r, Ok(Err(_))), "{}: {r:?}", p.display());
        checked += 1;
    }
    assert_eq!(checked, 10);
}
