//! Dev tool: the Rust DXV3 encoder against the Swift GlEnc 1.6.2 encoder oracle (glenc-rs `tools/swift-enc-oracle`),
//! packet for packet, over generated frames (and PNG frames given on the command line).
//!
//!   cargo run --release -p dxvkit-plus-enc --example dxv_vs_oracle -- <enc-oracle> <scratch-dir> [frame.png ...]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use dxvkit_plus_enc::dxv::{DxvEffort, DxvEncoder, DxvVariant};
use std::path::Path;
use std::process::Command;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
}

fn frames(kind: &str, w: u32, h: u32, n: u32) -> Vec<Vec<u8>> {
    let mut rng = Lcg(u64::from(w) * 7919 + u64::from(h));
    (0..n)
        .map(|f| {
            let mut v = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let p: [u8; 4] = match kind {
                        "grad" => [(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, ((x + y + f * 7) % 256) as u8, ((x * 3 + y) % 256) as u8],
                        "noise" => [rng.next(), rng.next(), rng.next(), rng.next()],
                        "blocks" => {
                            let c = ((x / 4 + y / 4 * 7 + f) % 5) as u8;
                            [c * 60, 255 - c * 50, c * 20, if c == 0 { 0 } else { 255 - c * 40 }]
                        }
                        "soft" => {
                            let fx = x as f32 / w.max(1) as f32;
                            let fy = y as f32 / h.max(1) as f32;
                            let r = (128.0 + 127.0 * (fx * 9.0 + f as f32).sin()) as u8;
                            let g = (128.0 + 127.0 * (fy * 7.0).cos()) as u8;
                            let b = (128.0 + 127.0 * ((fx + fy) * 5.0).sin()) as u8;
                            let a = (255.0 * (1.0 - ((fx - 0.5).powi(2) + (fy - 0.5).powi(2)).sqrt() * 1.6).clamp(0.0, 1.0)) as u8;
                            [r, g, b, a]
                        }
                        _ => [0; 4],
                    };
                    v[i..i + 4].copy_from_slice(&p);
                }
            }
            v
        })
        .collect()
}

fn png_rgba(path: &Path) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        t => panic!("png {t:?}"),
    };
    (w, h, rgba)
}

fn read_pkts(p: &Path) -> Vec<Vec<u8>> {
    let d = std::fs::read(p).unwrap();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= d.len() {
        let n = u32::from_le_bytes(d[i..i + 4].try_into().unwrap()) as usize;
        out.push(d[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    out
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let oracle = &a[1];
    let dir = Path::new(&a[2]);
    std::fs::create_dir_all(dir).unwrap();
    let mut cases: Vec<(String, u32, u32, Vec<Vec<u8>>)> = Vec::new();
    for &(w, h) in &[(1, 1), (3, 5), (16, 16), (17, 9), (64, 64), (255, 129), (512, 288), (1920, 1080)] {
        for k in ["grad", "noise", "blocks", "soft"] {
            let n = if w * h > 100_000 { 2 } else { 3 };
            cases.push((format!("{k}-{w}x{h}"), w, h, frames(k, w, h, n)));
        }
    }
    if std::env::var_os("DXV_RAW_ONLY").is_some() {
        cases.clear();
    }
    // `raw:<w>x<h>:<file>` = consecutive RGBA8 frames (made with ffmpeg in a scratch dir); anything else is a PNG.
    for r in a[3..].iter().filter(|p| p.starts_with("raw:")) {
        let mut it = r[4..].splitn(2, ':');
        let (dims, file) = (it.next().unwrap(), it.next().unwrap());
        let (w, h) = dims.split_once('x').map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap())).unwrap();
        let d = std::fs::read(file).unwrap();
        let fs = (w * h * 4) as usize;
        let fr: Vec<Vec<u8>> = d.chunks_exact(fs).map(<[u8]>::to_vec).collect();
        cases.push((format!("raw-{file}"), w, h, fr));
    }
    let pngs: Vec<_> = a[3..].iter().filter(|p| !p.starts_with("raw:")).map(|p| png_rgba(Path::new(p))).collect();
    if let Some((w, h, _)) = pngs.first() {
        let (w, h) = (*w, *h);
        cases.push(("png".into(), w, h, pngs.iter().filter(|p| (p.0, p.1) == (w, h)).map(|p| p.2.clone()).collect()));
    }
    let (mut total, mut same) = (0, 0);
    for (name, w, h, fr) in &cases {
        for (v, tag) in [(DxvVariant::Dxt1, "dxt1"), (DxvVariant::Dxt5, "dxt5"), (DxvVariant::Ycg6, "ycg6"), (DxvVariant::Yg10, "yg10")] {
            let bgra: Vec<u8> = fr.iter().flat_map(|f| f.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect::<Vec<_>>()).collect();
            let inp = dir.join("in.bgra");
            let out = dir.join("out.pkts");
            std::fs::write(&inp, &bgra).unwrap();
            let alpha = if v.alpha() { "straight" } else { "none" };
            let st = Command::new(oracle)
                .args(["encode", tag, &w.to_string(), &h.to_string(), "1", alpha, inp.to_str().unwrap(), out.to_str().unwrap()])
                .status()
                .unwrap();
            assert!(st.success());
            let theirs = read_pkts(&out);
            let mut enc = DxvEncoder::new(v, *w, *h, DxvEffort::Best).unwrap();
            for (i, f) in fr.iter().enumerate() {
                // Swift's fill: opaque variants force alpha 255 (DXVKitFrameEncoder.swift:116-119).
                let f2: Vec<u8> = if v.alpha() { f.clone() } else { f.chunks(4).flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
                let ours = enc.encode(&f2).unwrap();
                total += 1;
                if ours == theirs[i] {
                    same += 1;
                } else {
                    let d = ours.iter().zip(theirs[i].iter()).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs[i].len()));
                    println!("DIFF {tag} {name} frame {i}: len {} vs {}, first diff at {d}", ours.len(), theirs[i].len());
                }
            }
        }
    }
    println!("{same} / {total} packets identical");
}
