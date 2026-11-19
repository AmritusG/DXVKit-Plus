//! The DXV3 encoder: every packet decodes (DXVKit Plus's decoder) to exactly the blocks the encoder packed, and the
//! packets equal Swift GlEnc 1.6.2's for a generated set (golden SHA-256 from the encoder oracle: glenc-rs
//! `tools/swift-enc-oracle`, Swift GlEnc's own `DXVKitFrameEncoder` over DXVKit 1.0.0 as SwiftPM builds it).
#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use dxvkit_plus_enc::dxv::{DxvEffort, DxvEncoder, DxvVariant, compress};
use sha2::{Digest, Sha256};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
}

/// Deterministic frames: (name, w, h, rgba).
fn set() -> Vec<(String, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    for &(w, h) in &[(1u32, 1u32), (17, 9), (64, 64), (255, 129)] {
        for kind in ["grad", "noise", "blocks"] {
            let mut rng = Lcg(u64::from(w) * 7919 + u64::from(h));
            let mut v = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let p = match kind {
                        "grad" => [(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) % 256) as u8, ((x * 3 + y) % 256) as u8],
                        "noise" => [rng.next(), rng.next(), rng.next(), rng.next()],
                        _ => {
                            let c = ((x / 4 + y / 4 * 7) % 5) as u8;
                            [c * 60, 255 - c * 50, c * 20, if c == 0 { 0 } else { 255 - c * 40 }]
                        }
                    };
                    v[i..i + 4].copy_from_slice(&p);
                }
            }
            out.push((format!("{kind}-{w}x{h}"), w, h, v));
        }
    }
    out
}

fn tag(v: DxvVariant) -> &'static str {
    match v {
        DxvVariant::Dxt1 => "dxt1",
        DxvVariant::Dxt5 => "dxt5",
        DxvVariant::Ycg6 => "ycg6",
        DxvVariant::Yg10 => "yg10",
    }
}

/// Swift's fill: opaque variants take alpha 255 (`DXVKitFrameEncoder.swift:116-119`).
fn fill(v: DxvVariant, rgba: &[u8]) -> Vec<u8> {
    if v.alpha() { rgba.to_vec() } else { rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect() }
}

#[test]
fn packets_decode_to_the_packed_planes() {
    for (name, w, h, rgba) in set() {
        for v in DxvVariant::ALL {
            let px = fill(v, &rgba);
            let planes = compress(v, w, h, &px, DxvEffort::Best).unwrap();
            let packet = DxvEncoder::new(v, w, h, DxvEffort::Best).unwrap().encode(&px).unwrap();
            let blocks = dxvkit_plus_core::dxv::decode_blocks(&packet, w, h, false).unwrap();
            assert_eq!(blocks.planes, planes.planes, "{name} {}", tag(v));
            assert_eq!(&packet[0..4], &v.tag());
        }
    }
}

/// Golden: SHA-256 of each packet as Swift GlEnc's encoder oracle wrote it (`enc-oracle encode <v> <w> <h> 1
/// straight|none`), minted 2026-11-17. `cargo test -p dxvkit-plus-enc --release -- --ignored dump_golden_inputs`
/// writes the inputs (BGRA, as the oracle takes them) to re-mint.
const GOLDEN: &[(&str, &str)] = &include!("dxv_golden.in");

#[test]
fn packets_equal_swift_glencs() {
    let mut n = 0;
    for (name, w, h, rgba) in set() {
        for v in DxvVariant::ALL {
            let packet = DxvEncoder::new(v, w, h, DxvEffort::Best).unwrap().encode(&fill(v, &rgba)).unwrap();
            let key = format!("{}-{name}", tag(v));
            let want = GOLDEN.iter().find(|(k, _)| *k == key).map(|(_, h)| *h).unwrap_or_else(|| panic!("no golden for {key}"));
            assert_eq!(format!("{:x}", Sha256::digest(&packet)), want, "{key}");
            n += 1;
        }
    }
    assert_eq!(n, GOLDEN.len());
}

/// 0.15.0: the packets do not depend on the encoder's thread count (1, 3, 64, and the default 0).
#[test]
fn packets_do_not_depend_on_the_thread_count() {
    for (name, w, h, rgba) in set() {
        for v in DxvVariant::ALL {
            let px = fill(v, &rgba);
            let want = DxvEncoder::new(v, w, h, DxvEffort::Best).unwrap().encode(&px).unwrap();
            for t in [1, 3, 64] {
                let mut e = DxvEncoder::new(v, w, h, DxvEffort::Best).unwrap();
                e.set_threads(t);
                assert_eq!(e.encode(&px).unwrap(), want, "{name} {} on {t} threads", tag(v));
            }
        }
    }
}

#[test]
#[ignore]
fn dump_golden_inputs() {
    let dir = std::env::var("DXV_GOLDEN_DIR").unwrap();
    for (name, _w, _h, rgba) in set() {
        let bgra: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        std::fs::write(format!("{dir}/{name}.bgra"), bgra).unwrap();
    }
}
