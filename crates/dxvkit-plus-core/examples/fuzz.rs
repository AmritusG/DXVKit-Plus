//! Mutation fuzzer for the QuickTime demuxer, the HAP decoder, the DXV decoder (DXV3 block copy,
//! HQ opcode / FSE streams, legacy LZF) and the LMBD reader (container, chunk table, Zstd chunks),
//! including the BC7 CPU decoder
//! (no libFuzzer: the toolchain is stable-only; this is a seeded, multi-threaded,
//! structure-unaware mutator with a crash and hang detector, run for a fixed wall time).
//! Ported from Loombda-rs `crates/loombda-media/examples/fuzz_media.rs` (Amrit's).
//!
//! ```text
//! cargo run --release -p dxvkit-plus-core --example fuzz -- <seconds> <threads> [all|lmbd|dxv]
//! ```
//!
//! `lmbd` restricts the corpus to the LMBD vectors (the reader alone); `dxv` to DXV packets and
//! DXV movies. With `DXVKIT_DIR=<a DXVKit checkout>` its full vector streams join the corpus (the
//! Resolume-encoder streams carry the entropy-coded HQ opcodes; never committed here).
//!
//! Corpus: every committed fixture (whole files, their HAP frames, the LMBD vectors) plus synthetic
//! HAP R (BC7, all modes) and timecode movies written here. Each iteration picks a seed input, applies 1-8
//! mutations (bit flips, interesting values over u8/u16/u32 fields, insert / delete / duplicate /
//! shorten) and runs it through one target: `hap::decode` + CPU RGBA, `dxv::decode_blocks` (sometimes
//! at a random size, HQ sections sometimes in parallel) + CPU RGBA straight and premultiplied, `Movie::from_source` +
//! `read_sample` + the codec + CPU RGBA, or `LmbdFile::from_source` + `decode_frame` (and `decode_frame_into`
//! through a contract-checking sink, compared with it) + CPU RGBA. A panic is a crash (caught, counted, the input saved); an
//! input slower than 2 s is a hang. Prints one summary line; exit status 1 on any crash or hang.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::container::{Codec, MemSource, Movie};
use dxvkit_plus_core::lmbd::LmbdFile;
use dxvkit_plus_core::source::{PixelFormat, Plane, PlaneSink, Shading, planes_to_rgba};
use dxvkit_plus_core::{dxv, hap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;

#[derive(Clone)]
enum Seed {
    Hap { frame: Vec<u8>, w: u32, h: u32 },
    Dxv { packet: Vec<u8>, w: u32, h: u32 },
    File(Vec<u8>),
    Lmbd(Vec<u8>),
}

fn movies(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                movies(&p, out);
            } else if matches!(p.extension().and_then(|e| e.to_str()), Some("mov" | "mp4" | "lmbd")) {
                out.push(p);
            }
        }
    }
}

fn corpus(dirs: &[PathBuf]) -> Vec<Seed> {
    let mut files = Vec::new();
    for d in dirs {
        movies(d, &mut files);
    }
    let mut seeds = Vec::new();
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        if f.extension().and_then(|e| e.to_str()) == Some("lmbd") {
            seeds.push(Seed::Lmbd(bytes));
            continue;
        }
        seeds.push(Seed::File(bytes));
        let Ok(m) = Movie::open(&f) else { continue };
        for t in m.tracks.clone() {
            for i in 0..t.frame_count().min(3) {
                let mut pkt = Vec::new();
                if m.read_sample(&t, i, &mut pkt).is_err() {
                    continue;
                }
                match t.codec {
                    Codec::Hap => seeds.push(Seed::Hap { frame: pkt, w: t.width, h: t.height }),
                    Codec::Dxv => seeds.push(Seed::Dxv { packet: pkt, w: t.width, h: t.height }),
                    Codec::Native => {}
                }
            }
        }
    }
    // Synthetic HAP R frames: random blocks of every BC7 mode (exercises the BC7 decoder).
    let mut r = Rng(0xB7);
    for mode in 0..8u32 {
        let blocks: Vec<u8> = (0..16)
            .flat_map(|_| {
                let mut v = (u128::from(r.next()) << 64) | u128::from(r.next());
                v &= !((1u128 << (mode + 1)) - 1);
                v |= 1u128 << mode;
                v.to_le_bytes()
            })
            .collect();
        let frame = qt_movie::hap_frame(0xAC, &blocks);
        seeds.push(Seed::File(qt_movie::video_movie(b"Hap7", 16, 16, std::slice::from_ref(&frame))));
        seeds.push(Seed::Hap { frame, w: 16, h: 16 });
    }
    seeds.push(Seed::File(qt_movie::movie(Some((1, 30_000, 1_001, 30, 107_892)))));
    seeds
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

const INTERESTING32: [u32; 10] = [0, 1, 2, 0x7F, 0x80, 0xFF, 0xFFFF, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF];

fn mutate(data: &mut Vec<u8>, r: &mut Rng) {
    for _ in 0..1 + r.below(8) {
        let len = data.len();
        match r.below(9) {
            0 if len > 0 => {
                let i = r.below(len);
                data[i] ^= 1 << r.below(8);
            }
            1 if len > 0 => {
                let i = r.below(len);
                data[i] = r.next() as u8;
            }
            2 if len >= 4 => {
                let i = r.below(len - 3);
                let v = INTERESTING32[r.below(INTERESTING32.len())];
                let b = if r.below(2) == 0 { v.to_le_bytes() } else { v.to_be_bytes() };
                data[i..i + 4].copy_from_slice(&b);
            }
            3 if len >= 2 => {
                let i = r.below(len - 1);
                let v = (r.next() as u16).to_le_bytes();
                data[i..i + 2].copy_from_slice(&v);
            }
            4 => {
                let i = r.below(len + 1);
                let n = 1 + r.below(16);
                for _ in 0..n {
                    data.insert(i.min(data.len()), r.next() as u8);
                }
            }
            5 if len > 0 => {
                let i = r.below(len);
                let n = (1 + r.below(64)).min(len - i);
                data.drain(i..i + n);
            }
            6 if len > 0 => {
                let new = r.below(len);
                data.truncate(new);
            }
            7 if len > 8 => {
                let i = r.below(len - 8);
                let n = 1 + r.below(64.min(len - i));
                let chunk = data[i..i + n].to_vec();
                let at = r.below(data.len());
                data.splice(at..at, chunk);
            }
            _ if len > 0 => {
                let i = r.below(len);
                data[i] = data[i].wrapping_add(1);
            }
            _ => data.push(r.next() as u8),
        }
    }
}

/// A `PlaneSink` that panics (a counted crash) if a decoder breaks the sink contract: one plane,
/// writes in order without gaps or overlaps, never past the frame.
struct CheckedSink {
    out: Vec<u8>,
    limit: usize,
}

impl PlaneSink for CheckedSink {
    fn write(&mut self, plane: usize, offset: usize, bytes: &[u8]) -> Result<(), MediaError> {
        assert_eq!(plane, 0, "LMBD has one plane");
        assert_eq!(offset, self.out.len(), "write out of order");
        assert!(offset + bytes.len() <= self.limit, "write past the frame");
        self.out.extend_from_slice(bytes);
        Ok(())
    }
}

fn run_one(seed: &Seed, data: &[u8], r: &mut Rng) {
    match seed {
        Seed::Hap { w, h, .. } => {
            let (w, h) = if r.below(8) == 0 { (1 + r.below(300) as u32, 1 + r.below(300) as u32) } else { (*w, *h) };
            if let Ok(f) = hap::decode(data, w, h, r.below(2) == 0) {
                let _ = hap::to_rgba(&f, w, h);
            }
        }
        Seed::Dxv { w, h, .. } => {
            let (w, h) = if r.below(8) == 0 { (1 + r.below(300) as u32, 1 + r.below(300) as u32) } else { (*w, *h) };
            if let Ok(b) = dxv::decode_blocks(data, w, h, r.below(2) == 0) {
                assert!(b.used <= data.len(), "used more than the packet");
                let _ = dxv::rgba::blocks_to_rgba(&b, false);
                let _ = dxv::rgba::blocks_to_rgba(&b, true);
            }
        }
        Seed::Lmbd(_) => {
            let src: Arc<[u8]> = Arc::from(data);
            if let Ok(f) = LmbdFile::from_source(Arc::new(MemSource(src))) {
                let h = f.header().clone();
                // Same allocation bound as for movies below.
                if u64::from(h.width) * u64::from(h.height) > 4096 * 4096 {
                    return;
                }
                let (mut s, mut blocks) = (Vec::new(), Vec::new());
                for i in 0..f.frame_count().min(3) {
                    // decode_into (streamed chunk by chunk): writes stay in order and inside the
                    // frame, and whenever both decodes succeed they produce the same bytes.
                    let mut sink = CheckedSink { out: Vec::new(), limit: usize::try_from(h.frame_bytes()).unwrap_or(0) };
                    let (mut s2, mut chunk) = (Vec::new(), Vec::new());
                    let into = f.decode_frame_into(i, &mut s2, &mut chunk, &mut sink);
                    let plain = f.decode_frame(i, &mut s, &mut blocks, r.below(2) == 0);
                    if into.is_ok() && plain.is_ok() {
                        assert_eq!(sink.out, blocks, "decode_frame_into differs from decode_frame");
                    }
                    if plain.is_ok() {
                        let (cw, ch) = h.coded_size();
                        let planes = [Plane { format: PixelFormat::Bc7, width: cw, height: ch, data: std::mem::take(&mut blocks) }];
                        let _ = planes_to_rgba(Shading::Rgba { opaque: false }, &planes, h.width, h.height);
                    }
                }
            }
        }
        Seed::File(_) => {
            let src: Arc<[u8]> = Arc::from(data);
            if let Ok(m) = Movie::from_source(Arc::new(MemSource(src))) {
                for t in m.tracks.clone() {
                    // Bound the frame size the fuzzer lets a hostile header allocate (memory on a
                    // shared machine); the decoders' own limit is 16384 x 16384.
                    if u64::from(t.width) * u64::from(t.height) > 4096 * 4096 {
                        continue;
                    }
                    for i in 0..t.frame_count().min(2) {
                        let mut pkt = Vec::new();
                        if m.read_sample(&t, i, &mut pkt).is_err() {
                            continue;
                        }
                        if t.codec == Codec::Hap
                            && let Ok(f) = hap::decode(&pkt, t.width, t.height, false)
                        {
                            let _ = hap::to_rgba(&f, t.width, t.height);
                        }
                        if t.codec == Codec::Dxv
                            && let Ok(b) = dxv::decode_blocks(&pkt, t.width, t.height, false)
                        {
                            let _ = dxv::rgba::blocks_to_rgba(&b, false);
                        }
                    }
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let threads: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let only = args.get(3).map(String::as_str).unwrap_or("all");
    let mut dirs = vec![root];
    if let Ok(d) = std::env::var("DXVKIT_DIR") {
        dirs.push(Path::new(&d).join("test_vectors/streams"));
    }
    let mut all = corpus(&dirs);
    match only {
        "lmbd" => all.retain(|s| matches!(s, Seed::Lmbd(_))),
        // DXV packets, and the movies that carry them (the demuxer path).
        "dxv" => all.retain(|s| match s {
            Seed::Dxv { .. } => true,
            Seed::File(b) => Movie::from_source(Arc::new(MemSource(Arc::from(b.as_slice())))).is_ok_and(|m| m.track(Codec::Dxv).is_some()),
            _ => false,
        }),
        _ => {}
    }
    let seeds = Arc::new(all);
    let (n_hap, n_dxv, n_file, n_lmbd) = seeds.iter().fold((0, 0, 0, 0), |a, s| match s {
        Seed::Hap { .. } => (a.0 + 1, a.1, a.2, a.3),
        Seed::Dxv { .. } => (a.0, a.1 + 1, a.2, a.3),
        Seed::File(_) => (a.0, a.1, a.2 + 1, a.3),
        Seed::Lmbd(_) => (a.0, a.1, a.2, a.3 + 1),
    });
    std::panic::set_hook(Box::new(|_| {})); // crashes are counted, not printed per hit
    let execs = Arc::new(AtomicU64::new(0));
    let crashes = Arc::new(AtomicU64::new(0));
    let hangs = Arc::new(AtomicU64::new(0));
    let slowest_us = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let crash_dir = std::env::temp_dir().join("dxvkit-plus-fuzz-crashes");
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (seeds, execs, crashes, hangs, slowest, stop, crash_dir) =
                (seeds.clone(), execs.clone(), crashes.clone(), hangs.clone(), slowest_us.clone(), stop.clone(), crash_dir.clone());
            std::thread::spawn(move || {
                let mut r = Rng(0x5EED_0000 + t as u64);
                while !stop.load(Ordering::Relaxed) {
                    let seed = &seeds[r.below(seeds.len())];
                    let mut data = match seed {
                        Seed::Hap { frame, .. } | Seed::Dxv { packet: frame, .. } => frame.clone(),
                        Seed::File(b) | Seed::Lmbd(b) => b.clone(),
                    };
                    mutate(&mut data, &mut r);
                    let t0 = Instant::now();
                    let mut rr = Rng(r.next());
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(seed, &data, &mut rr))).is_ok();
                    let us = t0.elapsed().as_micros() as u64;
                    slowest.fetch_max(us, Ordering::Relaxed);
                    if !ok {
                        crashes.fetch_add(1, Ordering::Relaxed);
                        let _ = std::fs::create_dir_all(&crash_dir);
                        let _ = std::fs::write(crash_dir.join(format!("crash-{t}-{}.bin", execs.load(Ordering::Relaxed))), &data);
                    }
                    if us > 2_000_000 {
                        hangs.fetch_add(1, Ordering::Relaxed);
                    }
                    execs.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    let secs = start.elapsed().as_secs_f64();
    let (e, c, h) = (execs.load(Ordering::Relaxed), crashes.load(Ordering::Relaxed), hangs.load(Ordering::Relaxed));
    println!(
        "fuzz: {secs:.0} s, {threads} threads, corpus {} (hap frames {n_hap}, dxv packets {n_dxv}, files {n_file}, lmbd {n_lmbd}), execs {e} ({:.0}/s), crashes {c}, hangs {h}, slowest input {:.1} ms",
        seeds.len(),
        e as f64 / secs,
        slowest_us.load(Ordering::Relaxed) as f64 / 1000.0
    );
    if c > 0 || h > 0 {
        println!("crash inputs saved in {}", crash_dir.display());
        std::process::exit(1);
    }
}
