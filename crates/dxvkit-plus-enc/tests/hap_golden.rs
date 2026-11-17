//! Golden packets: SHA-256 of the HAP packets Swift GlEnc 1.6.2's own encoder (`HapFrameEncoder` in GlEncCore,
//! AmritusG/glenc @ ed63fd6, built with SwiftPM release) writes for a deterministic generated input set. The hashes
//! were taken from that encoder's output (glenc-rs `tools/swift-enc-oracle`), not from this crate; the Rust
//! encoder must reproduce them byte for byte.
//!
//! Each hash covers two frames, written as the oracle writes them: per frame a u32 LE length, then the packet.
//! Rows marked `refined` come from the same oracle with GlEnc's `BC4Config.useRefinement = true`.
//!
//! To re-mint: `HAP_GOLDEN_DIR=<dir> cargo test --release -p dxvkit-plus-enc --test hap_golden -- --ignored`
//! writes `<dir>/<w>x<h>.bgra` (BGRA, the oracle's input order); run
//! `enc-oracle encode <codec> <w> <h> <chunks> <straight|premul|none> <w>x<h>.bgra out.pkts` per row and hash
//! `out.pkts`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use dxvkit_plus_enc::alpha::FrameAlpha;
use dxvkit_plus_enc::hap::{HapCodec, HapEncoder};
use sha2::{Digest, Sha256};

const SIZES: [(u32, u32); 2] = [(17, 9), (130, 66)];
const FRAMES: u32 = 2;

/// A position hash (deterministic, independent of iteration order).
fn hash(x: u32, y: u32, f: u32, salt: u32) -> u32 {
    let mut v = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ f.wrapping_mul(0xC2B2_AE3D) ^ salt;
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^= v >> 12;
    v = v.wrapping_mul(0x297A_2D39);
    v ^ (v >> 15)
}

/// One RGBA frame: each 4×4 block (by block coordinate) is one of six contents — gradient, noise, a flat colour
/// with alpha 0 / 128 / 255, saturated primaries with alpha 0 / 255, low / medium chroma around grey (HapY's
/// scale 4 / 2), and mixed alpha.
fn frame(w: u32, h: u32, f: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (bx, by) = (x / 4, y / 4);
            let n = hash(x, y, f, 7);
            let p: [u8; 4] = match (bx * 3 + by * 5 + f) % 6 {
                0 => [(x * 9) as u8, (y * 13) as u8, ((x + y) * 5) as u8, (x * 7 + 40) as u8],
                1 => n.to_le_bytes(),
                2 => {
                    let c = hash(bx, by, f, 1);
                    [c as u8, (c >> 8) as u8, (c >> 16) as u8, [0, 128, 255][(c >> 24) as usize % 3]]
                }
                3 => {
                    let k = hash(bx, by, f, 2) % 3;
                    let mut p = [0u8, 0, 0, if n & 1 == 0 { 0 } else { 255 }];
                    p[k as usize] = 255;
                    if n & 6 == 0 {
                        p[((k + 1) % 3) as usize] = 255;
                    }
                    p
                }
                4 => {
                    let g = 60 + (hash(bx, by, f, 3) % 120) as i32;
                    let spread = if (bx + by) % 2 == 0 { 25 } else { 55 };
                    let d = (n % (2 * spread as u32 + 1)) as i32 - spread;
                    [(g + d).clamp(0, 255) as u8, g as u8, (g - d).clamp(0, 255) as u8, 255]
                }
                _ => [(x * 3) as u8, (y * 3) as u8, 100, [0, 255, (n >> 8) as u8, 77][(n % 4) as usize]],
            };
            v.extend_from_slice(&p);
        }
    }
    v
}

fn packets(codec: HapCodec, w: u32, h: u32, chunks: u32, alpha: FrameAlpha, refined: bool) -> Vec<u8> {
    let mut enc = HapEncoder::new(codec, chunks, w, h).unwrap();
    enc.set_refined_bc4(refined);
    let mut out = Vec::new();
    for f in 0..FRAMES {
        let p = enc.encode(&frame(w, h, f), alpha).unwrap();
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&p);
    }
    out
}

use FrameAlpha::{Opaque as None_, Premultiplied as Premul, Straight};
use HapCodec::{Hap1, Hap5, HapA, HapM, HapY};

/// (codec, width, height, chunks, alpha tag, refined BC4, SHA-256 of the oracle's packets).
#[rustfmt::skip]
const GOLDEN: &[(HapCodec, u32, u32, u32, FrameAlpha, bool, &str)] = &[
    (Hap1, 17, 9, 1, Straight, false, "748270bb2c0a04e8553dab12ae9492a42a5d2d5131c35e1664ebf94f3a72af5b"),
    (Hap1, 17, 9, 5, Straight, false, "3d4c00c974accbc7e1b07689495b52c04b2b5e5f177d47b1c9f1584919336e46"),
    (Hap1, 17, 9, 1, None_, false, "748270bb2c0a04e8553dab12ae9492a42a5d2d5131c35e1664ebf94f3a72af5b"),
    (Hap1, 17, 9, 5, None_, false, "3d4c00c974accbc7e1b07689495b52c04b2b5e5f177d47b1c9f1584919336e46"),
    (Hap5, 17, 9, 1, Straight, false, "78909ce6bacb41c7015ec30a0cbb98c531a1a29f259698a45f542208c780e8ac"),
    (Hap5, 17, 9, 5, Straight, false, "be8f6d3c5d9e9c4bc855ff87661fd4ae9136c8ce1f4acf012b9876f180507d88"),
    (Hap5, 17, 9, 1, Premul, false, "fe93976371495ecd9cddf89caefcfefa60dd970b5e579f328c0d58eba24ba572"),
    (Hap5, 17, 9, 5, Premul, false, "9b1c9d06876361ee6fdf6d95969c196bb93eb6430008d4f80faec7ca97c27846"),
    (Hap5, 17, 9, 1, None_, false, "f207dc301f759dd212635a9a85b54e16502fba5e25e77648850b27f9244a8fa6"),
    (Hap5, 17, 9, 5, None_, false, "300d0920e906641e4e1cf1ce1a5bcf6756ec4af2454869218dbf232b0212ff6a"),
    (HapY, 17, 9, 1, Straight, false, "6cbc653d3deace481eccabda975160881c957d565c9dc0378c69d6070ff43e2d"),
    (HapY, 17, 9, 5, Straight, false, "7ec2c3892de569e337f5f144e5e3b36d9d3339cd447e8e2f3a590cd15871a7e2"),
    (HapY, 17, 9, 1, Premul, false, "6cbc653d3deace481eccabda975160881c957d565c9dc0378c69d6070ff43e2d"),
    (HapY, 17, 9, 5, Premul, false, "7ec2c3892de569e337f5f144e5e3b36d9d3339cd447e8e2f3a590cd15871a7e2"),
    (HapA, 17, 9, 1, Straight, false, "85a6663dc87ad2246f548be4f25d1f77c9e9da3d9f52108221073e1c1a98f640"),
    (HapA, 17, 9, 5, Straight, false, "e8cb5aa5c7bd5da88f9399f9cb3fc73a87548096c501a3461ed5d134c1b238b3"),
    (HapA, 17, 9, 1, Premul, false, "85a6663dc87ad2246f548be4f25d1f77c9e9da3d9f52108221073e1c1a98f640"),
    (HapA, 17, 9, 5, Premul, false, "e8cb5aa5c7bd5da88f9399f9cb3fc73a87548096c501a3461ed5d134c1b238b3"),
    (HapM, 17, 9, 1, Straight, false, "ba10f35be57f175244826b023910401b6961101cf5026098abf25431917f03af"),
    (HapM, 17, 9, 5, Straight, false, "f0375ee273cc669608b60bde65ba066626d34ffd6a57feca35b4e183793490d9"),
    (HapM, 17, 9, 1, Premul, false, "ba10f35be57f175244826b023910401b6961101cf5026098abf25431917f03af"),
    (HapM, 17, 9, 5, Premul, false, "f0375ee273cc669608b60bde65ba066626d34ffd6a57feca35b4e183793490d9"),
    (HapM, 17, 9, 1, None_, false, "ba7f5ae397e37071f6ce29318b10cbc2ab6f6a4e056569918508871045725966"),
    (HapM, 17, 9, 5, None_, false, "e72d2b36de693d282e5e5903747e14f93256eb26ec3f895a525cac25e4dcba31"),
    (Hap1, 130, 66, 1, Straight, false, "ab3526bd56aa3268a9fda9ea879e5ff72cbe3dad9c08637896196a4083284874"),
    (Hap1, 130, 66, 5, Straight, false, "1aed8683f80e6205c09646997d9cf945dbe547ec3b2d2a30b7e999e18d7f7da5"),
    (Hap1, 130, 66, 1, None_, false, "ab3526bd56aa3268a9fda9ea879e5ff72cbe3dad9c08637896196a4083284874"),
    (Hap1, 130, 66, 5, None_, false, "1aed8683f80e6205c09646997d9cf945dbe547ec3b2d2a30b7e999e18d7f7da5"),
    (Hap5, 130, 66, 1, Straight, false, "1fb916b48b1b3b0db3b04c58c916678df529aed21c592c6f4823c0549dc525f6"),
    (Hap5, 130, 66, 5, Straight, false, "c36a5247b0a80b970b572d8238675ca5f1b18ece229b4d821012eb7da2e6715a"),
    (Hap5, 130, 66, 1, Premul, false, "e1ae86dcd3050b395b5c50c007fb669a5b457a5a9d3fb5bfda23030deb332d48"),
    (Hap5, 130, 66, 5, Premul, false, "8144b57e749349b52ace91912e6703c6c0375ccf93230dbdcb7a6b46bf08a856"),
    (Hap5, 130, 66, 1, None_, false, "49e51bec79b6d4650afe509fdfb04d5c341848ff845030ddf759726b82336252"),
    (Hap5, 130, 66, 5, None_, false, "90072af50502c38dd6b22b517f2eeba3b1c1773edea114bb1f13b3f123a06404"),
    (HapY, 130, 66, 1, Straight, false, "eba753bb4b7c0701c268ac64c8f1eac3b2b2bf13e9c46692e191e407904d6681"),
    (HapY, 130, 66, 5, Straight, false, "6d63cc1505e6ece234f6317944613cc6dc3991319e6e3273dc857a9e42fd0c72"),
    (HapY, 130, 66, 1, Premul, false, "eba753bb4b7c0701c268ac64c8f1eac3b2b2bf13e9c46692e191e407904d6681"),
    (HapY, 130, 66, 5, Premul, false, "6d63cc1505e6ece234f6317944613cc6dc3991319e6e3273dc857a9e42fd0c72"),
    (HapA, 130, 66, 1, Straight, false, "dd4ca1553a34224ca868d6a399c79f58ed99b6fbc70987857edff3e361dc5793"),
    (HapA, 130, 66, 5, Straight, false, "95ed3dbb1de36b53821a053d6d97878cf91f07931436c596a4cd04703dc0e415"),
    (HapA, 130, 66, 1, Premul, false, "dd4ca1553a34224ca868d6a399c79f58ed99b6fbc70987857edff3e361dc5793"),
    (HapA, 130, 66, 5, Premul, false, "95ed3dbb1de36b53821a053d6d97878cf91f07931436c596a4cd04703dc0e415"),
    (HapM, 130, 66, 1, Straight, false, "f6d4aa63c60c594aee434950fdd604abc2d9b9616a5124b0aa2e99f3e8a0a21e"),
    (HapM, 130, 66, 5, Straight, false, "91e931ae8d369c44186e216464dddd5294489a3a2a3944cb7b7ebfbc484999ce"),
    (HapM, 130, 66, 1, Premul, false, "f6d4aa63c60c594aee434950fdd604abc2d9b9616a5124b0aa2e99f3e8a0a21e"),
    (HapM, 130, 66, 5, Premul, false, "91e931ae8d369c44186e216464dddd5294489a3a2a3944cb7b7ebfbc484999ce"),
    (HapM, 130, 66, 1, None_, false, "b62291f40decb8231b29cd5349e50e7fcf2fe8fb2dced129dd731665c8220001"),
    (HapM, 130, 66, 5, None_, false, "5ff9c0359df7363242f7bf5529cbf2a9597ff0bf9cbd64622adeffaf09919b49"),
    (Hap5, 130, 66, 1, Straight, true, "84ac94d9ea9573c81e6143dcbe3693e49df38a545854b63d9fc4c1a8be7811b7"),
    (Hap5, 130, 66, 5, Straight, true, "b4763671a6c20f8a8c1f873f6776a9263cbb4fd3c1ac22511917ddb9f81b5b93"),
    (HapY, 130, 66, 1, Straight, true, "06a13c65ed4dd0a89616e02a97ad54ad9b8ffd365f1dcf8e950ec796009daaf7"),
    (HapY, 130, 66, 5, Straight, true, "a58ea7eb89a3e57d721344914101ea9eafa7519985d50b94be818ed8152d2511"),
    (HapA, 130, 66, 1, Straight, true, "234fb617109f162d9299a0d4c229ea3733f593dae7eee327c8fa7c9653868c79"),
    (HapA, 130, 66, 5, Straight, true, "23bd0cb9f4b66395844c6e693bbe28db30c07c436895368f47373bf90a82732a"),
    (HapM, 130, 66, 1, Straight, true, "74e395294159014f7535b49c095d437a3d5a891576712daf379d2a1bd585643a"),
    (HapM, 130, 66, 5, Straight, true, "bedfb144a5e0ba7e5e2f146b7f1a3107c53a9c71bc856b4df75c6bb5675aa974"),
];

#[test]
fn packets_match_swift_glenc() {
    assert!(!GOLDEN.is_empty());
    let mut bad = Vec::new();
    for &(codec, w, h, chunks, alpha, refined, want) in GOLDEN {
        let got = format!("{:x}", Sha256::digest(packets(codec, w, h, chunks, alpha, refined)));
        if got != want {
            bad.push(format!("{codec:?} {w}x{h} chunks {chunks} {alpha:?} refined {refined}: {got}"));
        }
    }
    assert!(bad.is_empty(), "packets differ from Swift GlEnc's:\n{}", bad.join("\n"));
}

/// Writes the golden inputs (BGRA) and this crate's hashes, for re-minting against the oracle.
#[test]
#[ignore]
fn dump_golden_inputs() {
    let dir = std::path::PathBuf::from(std::env::var("HAP_GOLDEN_DIR").expect("HAP_GOLDEN_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    for (w, h) in SIZES {
        let mut bgra = Vec::new();
        for f in 0..FRAMES {
            bgra.extend(frame(w, h, f).chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]));
        }
        std::fs::write(dir.join(format!("{w}x{h}.bgra")), bgra).unwrap();
    }
}
