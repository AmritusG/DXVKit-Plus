//! Dev tool: the Rust HAP encoders against the Swift GlEnc 1.6.2 encoder oracle (glenc-rs `tools/swift-enc-oracle`,
//! GlEnc's own `HapFrameEncoder`), packet for packet, over generated frames (and PNG frames given on the command line).
//!
//!   cargo run --release -p dxvkit-plus-enc --example hap_vs_oracle -- <enc-oracle> <scratch-dir> [options] [frame.png ...]
//!
//! Options:
//!   --refined        set `GLENC_BC4_REFINED=1` for the oracle and turn on the refined BC4 search here (needs an
//!                    oracle built with that switch: `BC4Config.useRefinement = true` when the variable is "1")
//!   --quick          small sizes only
//!   --big            only the > 16 MiB section cases (4096x4096 noise)
//!   --jobs N         oracle processes at once (default 6)
//!   --time           no oracle run: time the Rust encoders on 1920x1080 frames (all threads, then one)
//!
//! Prints one line per case (codec, size, chunks, alpha tag, frames, result) and a total; exit status 1 on any
//! difference. On a difference it names the first differing byte and, through `dxvkit_plus_core::hap::decode`,
//! the first differing block.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use dxvkit_plus_enc::EncodeError;
use dxvkit_plus_enc::alpha::FrameAlpha;
use dxvkit_plus_enc::hap::{HapCodec, HapEncoder};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
}

const PATTERNS: [&str; 8] = ["grad", "noise", "blocks", "sat", "alpha", "premul", "soft", "mixed"];

/// One generated RGBA frame.
fn pattern(kind: &str, w: u32, h: u32, f: u32) -> Vec<u8> {
    let mut rng = Lcg(u64::from(w) * 7919 + u64::from(h) * 31 + u64::from(f) * 1_000_003 + kind.len() as u64);
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let p: [u8; 4] = match kind {
                "grad" => [
                    (x * 255 / w.max(2).saturating_sub(1).max(1)).min(255) as u8,
                    (y * 255 / h.max(2).saturating_sub(1).max(1)).min(255) as u8,
                    ((x + y + f * 7) % 256) as u8,
                    ((x * 3 + y) % 256) as u8,
                ],
                "noise" => [rng.next(), rng.next(), rng.next(), rng.next()],
                "blocks" => {
                    // Flat cells, 4-aligned and 5-wide, alpha 0 / 255 / partial.
                    let (cx, cy) = if (x / 64 + y / 64) % 2 == 0 { (x / 4, y / 4) } else { (x / 5, y / 5) };
                    let c = (cx + cy * 7 + f) % 6;
                    let a = match c {
                        0 => 0,
                        1 | 2 => 255,
                        3 => 128,
                        4 => 1,
                        _ => 254,
                    };
                    [(c * 50) as u8, (255 - c * 40) as u8, (c * 20 + 3) as u8, a]
                }
                "sat" => {
                    // HapY scale branches: pure primaries (|Co|,|Cg| > 63), medium chroma (32..63), near grey (<= 31),
                    // and mixes inside one block. Alpha 0 / 255 only (BC4 6-mode with the reserved entries).
                    let cell = (x / 4 + y / 4 * 3 + f) % 8;
                    let base = ((x * 5 + y * 3) % 160) as i32 + 40;
                    let (r, g, b) = match cell {
                        0 => (255, 0, 0),
                        1 => (0, 255, 0),
                        2 => (0, 0, 255),
                        3 => (base + 20, base, base - 20),
                        4 => (base + 60, base - 10, base - 60),
                        5 => (base + 120, base, base - 120),
                        6 => (255, 255, 0),
                        _ => (if (x + y) % 2 == 0 { 255 } else { 0 }, 128, if x % 2 == 0 { 255 } else { 0 }),
                    };
                    let c = |v: i32| v.clamp(0, 255) as u8;
                    [c(r), c(g), c(b), if (x / 2 + y / 3) % 3 == 0 { 0 } else { 255 }]
                }
                "alpha" => {
                    // Smooth colour; alpha mixing 0, 255 and mid values inside blocks.
                    let a = match (x / 4 + y / 4 + f) % 5 {
                        0 => [0u8, 255][((x + y) % 2) as usize],
                        1 => ((x * 16 + y * 4) % 256) as u8,
                        2 => [0u8, 128, 255, 77][((x + y * 2) % 4) as usize],
                        3 => 200 + ((x + y) % 10) as u8,
                        _ => rng.next(),
                    };
                    [(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8, a]
                }
                "premul" => {
                    // Valid premultiplied colour (c <= a), alpha 0 included.
                    let a = ((x * 7 + y * 3 + f * 11) % 256) as u8;
                    let m = |c: u32| ((c % 256) * u32::from(a) / 255) as u8;
                    [m(x * 3), m(y * 5), m(x + y), a]
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
                "mixed" => {
                    // Noise in some blocks, flat in others, gradient in the rest (compressible + not, per chunk).
                    match (x / 8 + y / 8 * 5 + f) % 3 {
                        0 => [rng.next(), rng.next(), rng.next(), rng.next()],
                        1 => [90, 30, 200, 255],
                        _ => [(x % 256) as u8, ((x + y) % 256) as u8, (y % 256) as u8, (x % 256) as u8],
                    }
                }
                _ => panic!("unknown pattern {kind}"),
            };
            v[i..i + 4].copy_from_slice(&p);
        }
    }
    v
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
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        other => panic!("unsupported PNG colour type {other:?}"),
    };
    assert_eq!(info.bit_depth, png::BitDepth::Eight, "8-bit PNGs only");
    (w, h, rgba)
}

fn codec_name(c: HapCodec) -> &'static str {
    match c {
        HapCodec::Hap1 => "hap1",
        HapCodec::Hap5 => "hap5",
        HapCodec::HapY => "hapy",
        HapCodec::HapA => "hapa",
        HapCodec::HapM => "hapm",
    }
}

fn alpha_name(a: FrameAlpha) -> &'static str {
    match a {
        FrameAlpha::Straight => "straight",
        FrameAlpha::Premultiplied => "premul",
        FrameAlpha::Opaque => "none",
    }
}

struct Case {
    codec: HapCodec,
    w: u32,
    h: u32,
    chunks: u32,
    alpha: FrameAlpha,
    label: String,
    frames: std::sync::Arc<Vec<Vec<u8>>>,
}

/// The first differing block between two packets, through the core decoder.
fn locate(ours: &[u8], theirs: &[u8], w: u32, h: u32) -> String {
    use dxvkit_plus_core::hap::decode;
    let (Ok(a), Ok(b)) = (decode(ours, w, h, false), decode(theirs, w, h, false)) else {
        return "one of the packets does not decode".into();
    };
    if a.images.len() != b.images.len() {
        return format!("image count {} vs {}", a.images.len(), b.images.len());
    }
    for (k, (ia, ib)) in a.images.iter().zip(&b.images).enumerate() {
        if ia.texture != ib.texture {
            return format!("image {k}: texture {} vs {}", ia.texture, ib.texture);
        }
        let bs = ia.texture.block_bytes();
        let bw = w.div_ceil(4) as usize;
        if let Some(blk) = ia.blocks.chunks(bs).zip(ib.blocks.chunks(bs)).position(|(x, y)| x != y) {
            return format!(
                "image {k} ({}): first differing block #{blk} (bx {}, by {}): ours {:02x?} oracle {:02x?}",
                ia.texture,
                blk % bw,
                blk / bw,
                &ia.blocks[blk * bs..blk * bs + bs],
                &ib.blocks[blk * bs..blk * bs + bs]
            );
        }
    }
    "blocks equal: the difference is in the packing (Snappy / sections)".into()
}

fn run_case(oracle: &Path, scratch: &Path, refined: bool, id: usize, c: &Case) -> (usize, usize, Vec<String>) {
    let fin = scratch.join(format!("case{id}.bgra"));
    let fout = scratch.join(format!("case{id}.pkts"));
    let mut bgra = Vec::with_capacity(c.frames.iter().map(Vec::len).sum());
    for f in c.frames.iter() {
        bgra.extend(f.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]));
    }
    std::fs::write(&fin, &bgra).unwrap();
    let mut cmd = Command::new(oracle);
    cmd.args(["encode", codec_name(c.codec), &c.w.to_string(), &c.h.to_string(), &c.chunks.to_string(), alpha_name(c.alpha)]).arg(&fin).arg(&fout);
    if refined {
        cmd.env("GLENC_BC4_REFINED", "1");
    }
    let status = cmd.output().unwrap();
    let _ = std::fs::remove_file(&fin);

    let mut enc = HapEncoder::new(c.codec, c.chunks, c.w, c.h).unwrap();
    enc.set_refined_bc4(refined);
    enc.set_threads(2);
    let mut notes = Vec::new();

    if !status.status.success() {
        // The oracle refuses (HapA without alpha): ours must refuse the same way.
        let msg = String::from_utf8_lossy(&status.stderr).trim().to_string();
        let ours = enc.encode(&c.frames[0], c.alpha);
        let ok = matches!(ours, Err(EncodeError::NoAlpha)) && msg.contains("HapA requires a source with an alpha channel");
        if !ok {
            notes.push(format!("oracle failed ({msg}); ours: {:?}", ours.map(|p| p.len())));
        }
        return (1, usize::from(ok), notes);
    }
    let data = std::fs::read(&fout).unwrap();
    let _ = std::fs::remove_file(&fout);
    let mut pkts = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        pkts.push(&data[i + 4..i + 4 + n]);
        i += 4 + n;
    }
    assert_eq!(pkts.len(), c.frames.len(), "oracle packet count");
    let mut same = 0;
    for (k, (frame, theirs)) in c.frames.iter().zip(&pkts).enumerate() {
        let ours = enc.encode(frame, c.alpha).unwrap();
        if ours.as_slice() == *theirs {
            same += 1;
        } else {
            let first = ours.iter().zip(theirs.iter()).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
            notes.push(format!("frame {k}: {} vs {} bytes, first difference at byte {first}; {}", ours.len(), theirs.len(), locate(&ours, theirs, c.w, c.h)));
        }
    }
    (c.frames.len(), same, notes)
}

/// Per-frame encode time at 1920x1080 (soft + noise patterns), all threads and one thread.
fn time_1080p(codecs: &[HapCodec]) {
    let frames = [pattern("soft", 1920, 1080, 0), pattern("mixed", 1920, 1080, 1), pattern("noise", 1920, 1080, 0)];
    for &codec in codecs {
        for threads in [0usize, 1] {
            let mut enc = HapEncoder::new(codec, 1, 1920, 1080).unwrap();
            enc.set_threads(threads);
            let _ = enc.encode(&frames[0], FrameAlpha::Straight).unwrap();
            let reps = 10;
            let t = std::time::Instant::now();
            for i in 0..reps {
                let _ = enc.encode(&frames[i % frames.len()], FrameAlpha::Straight).unwrap();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / reps as f64;
            println!("{:5} 1920x1080 threads {:>3}: {ms:7.1} ms/frame", codec_name(codec), if threads == 0 { "all".to_string() } else { threads.to_string() });
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(args.len() >= 2, "usage: hap_vs_oracle <enc-oracle> <scratch-dir> [--refined] [--quick] [--big] [--jobs N] [frame.png ...]");
    let oracle = PathBuf::from(&args[0]);
    let scratch = PathBuf::from(&args[1]);
    std::fs::create_dir_all(&scratch).unwrap();
    let (mut refined, mut quick, mut big, mut time, mut jobs) = (false, false, false, false, 6usize);
    let mut pngs = Vec::new();
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--refined" => refined = true,
            "--quick" => quick = true,
            "--big" => big = true,
            "--time" => time = true,
            "--jobs" => jobs = it.next().unwrap().parse().unwrap(),
            p => pngs.push(PathBuf::from(p)),
        }
    }

    let codecs = [HapCodec::Hap1, HapCodec::Hap5, HapCodec::HapY, HapCodec::HapA, HapCodec::HapM];
    if time {
        time_1080p(&codecs);
        return;
    }
    let alphas = [FrameAlpha::Straight, FrameAlpha::Premultiplied, FrameAlpha::Opaque];
    let chunk_counts = [1u32, 2, 3, 4, 7, 64];
    let mut cases = Vec::new();
    let mut add = |codec, w, h, chunks, alpha, label: &str, frames: &std::sync::Arc<Vec<Vec<u8>>>| {
        cases.push(Case { codec, w, h, chunks, alpha, label: label.to_string(), frames: frames.clone() })
    };

    if big {
        let frames = std::sync::Arc::new(vec![pattern("noise", 4096, 4096, 0)]);
        for codec in [HapCodec::Hap5, HapCodec::HapM, HapCodec::HapY] {
            for chunks in [1, 2] {
                add(codec, 4096, 4096, chunks, FrameAlpha::Straight, "noise", &frames);
            }
        }
    } else {
        let sizes: &[(u32, u32)] = if quick {
            &[(1, 1), (3, 5), (4, 4), (17, 9), (64, 64)]
        } else {
            &[(1, 1), (3, 5), (4, 4), (17, 9), (64, 64), (255, 129), (1081, 607), (1920, 1080), (1080, 1920)]
        };
        for &(w, h) in sizes {
            let large = w * h > 500_000;
            // Large frames: one frame per pattern; small ones: two (the frame index moves the patterns).
            let n = if large { 1 } else { 2 };
            let frames: std::sync::Arc<Vec<Vec<u8>>> = std::sync::Arc::new(PATTERNS.iter().flat_map(|k| (0..n).map(move |f| pattern(k, w, h, f))).collect());
            for codec in codecs {
                for alpha in alphas {
                    for &chunks in &chunk_counts {
                        add(codec, w, h, chunks, alpha, "patterns", &frames);
                    }
                }
            }
        }
        for p in &pngs {
            let (w, h, rgba) = png_rgba(p);
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let frames = std::sync::Arc::new(vec![rgba]);
            for codec in codecs {
                for alpha in alphas {
                    for chunks in [1, 4, 64] {
                        add(codec, w, h, chunks, alpha, &name, &frames);
                    }
                }
            }
        }
    }

    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cases.get(id) else { break };
                    let r = run_case(&oracle, &scratch, refined, id, c);
                    results.lock().unwrap().push((id, r));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(id, _)| *id);
    let (mut total, mut same, mut bad_cases) = (0, 0, 0);
    for (id, (n, ok, notes)) in &results {
        let c = &cases[*id];
        total += n;
        same += ok;
        let verdict = if ok == n { "identical" } else { "DIFFERENT" };
        if ok != n {
            bad_cases += 1;
        }
        println!(
            "{:5} {:>9} chunks {:2} {:8} {:10} frames {:3}  {}",
            codec_name(c.codec),
            format!("{}x{}", c.w, c.h),
            c.chunks,
            alpha_name(c.alpha),
            c.label,
            n,
            verdict
        );
        for note in notes {
            println!("      {note}");
        }
    }
    println!(
        "TOTAL: {} cases, {total} packets compared (refusals count as one), {same} identical, {bad_cases} cases with differences{}",
        results.len(),
        if refined { " [refined BC4]" } else { "" }
    );
    if same != total {
        std::process::exit(1);
    }
}
