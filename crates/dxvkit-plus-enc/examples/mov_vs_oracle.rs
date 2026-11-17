//! Dev tool: the Rust QuickTime writer (`dxvkit_plus_enc::mov`) against Swift GlEnc 1.6.2's `VariantMOVWriter`
//! (the encoder oracle of glenc-rs `tools/swift-enc-oracle`, `enc-oracle mov ...`), whole file for whole file, over
//! generated packets and PCM. Reports, per case, identical / both refused / the first differing offset and the
//! atom path there.
//!
//!   cargo run --release -p dxvkit-plus-enc --example mov_vs_oracle -- <enc-oracle> <scratch-dir> [--large]
//!
//! `--golden` instead writes the generated set of `tests/common/golden_set.rs` through the oracle and prints the
//! SHA-256 pinned in `tests/mov_golden.rs`.
//!
//! `--large` adds the > 4 GiB cases (largesize `mdat`, `co64`, and both boundaries): each needs ~4 GiB of packets
//! on disk plus one ~4 GiB movie at a time (the oracle's file is read into memory and deleted before the Rust file
//! is written); everything is deleted afterwards.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

#[path = "../tests/common/golden_set.rs"]
mod golden_set;

use dxvkit_plus_enc::mov::MovWriter;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
}

/// How the packets of a case are made.
#[derive(Clone)]
enum Pkts {
    /// Random bytes of these sizes.
    Sizes(Vec<usize>),
    /// Large packets: a repeated 1 MiB random block, xored per MiB.
    Big(Vec<u64>),
}

#[derive(Clone)]
struct Case {
    name: String,
    codec: &'static str,
    w: u32,
    h: u32,
    fps: String,
    writer: String,
    pkts: Pkts,
    /// rate, channels, PCM byte length.
    audio: Option<(u32, u32, usize)>,
}

fn fourcc(codec: &str) -> [u8; 4] {
    match codec {
        "dxt1" | "dxt5" | "ycg6" | "yg10" => *b"DXD3",
        "hap1" => *b"Hap1",
        "hap5" => *b"Hap5",
        "hapy" => *b"HapY",
        "hapa" => *b"HapA",
        "hapm" => *b"HapM",
        _ => panic!("codec {codec}"),
    }
}

fn varied(n: usize, seed: usize) -> Vec<usize> {
    (0..n).map(|i| (i * 7919 + seed * 31) % 5000 + (i % 3) * 100).collect()
}

fn write_pkts(path: &Path, pkts: &Pkts, seed: u64) {
    let mut f = BufWriter::with_capacity(1 << 22, fs::File::create(path).unwrap());
    let mut rng = Lcg(seed);
    match pkts {
        Pkts::Sizes(sizes) => {
            for &n in sizes {
                f.write_all(&(n as u32).to_le_bytes()).unwrap();
                let p: Vec<u8> = (0..n).map(|_| rng.next()).collect();
                f.write_all(&p).unwrap();
            }
        }
        Pkts::Big(sizes) => {
            let block: Vec<u8> = (0..1 << 20).map(|_| rng.next()).collect();
            let mut mib = 0u64;
            for &n in sizes {
                f.write_all(&(n as u32).to_le_bytes()).unwrap();
                let mut left = n;
                while left > 0 {
                    let k = left.min(1 << 20) as usize;
                    let x = (mib % 251) as u8;
                    let chunk: Vec<u8> = block[..k].iter().map(|b| b ^ x).collect();
                    f.write_all(&chunk).unwrap();
                    left -= k as u64;
                    mib += 1;
                }
            }
        }
    }
    f.flush().unwrap();
}

fn pcm(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed ^ 0xa5a5);
    (0..len).map(|_| rng.next()).collect()
}

/// The Rust side: stream the packets from the `.pkts` file (one at a time) into a `MovWriter`.
fn write_rust(c: &Case, pkts: &Path, pcm_path: Option<&Path>, out: &Path) -> Result<(), String> {
    let fps: f64 = c.fps.parse().unwrap();
    let mut w = MovWriter::create(out, fourcc(c.codec), c.w, c.h, fps, &c.writer).map_err(|e| e.to_string())?;
    let mut f = std::io::BufReader::with_capacity(1 << 22, fs::File::open(pkts).unwrap());
    let mut len = [0u8; 4];
    while f.read_exact(&mut len).is_ok() {
        let mut p = vec![0u8; u32::from_le_bytes(len) as usize];
        f.read_exact(&mut p).unwrap();
        w.append(&p).map_err(|e| e.to_string())?;
    }
    if let (Some((rate, ch, _)), Some(pp)) = (c.audio, pcm_path) {
        w.attach_audio(rate, ch, fs::read(pp).unwrap());
    }
    w.finish().map_err(|e| e.to_string())
}

fn run_oracle(oracle: &Path, c: &Case, pkts: &Path, pcm_path: Option<&Path>, out: &Path) -> Result<(), String> {
    let mut cmd = Command::new(oracle);
    cmd.arg("mov").arg(c.codec).arg(c.w.to_string()).arg(c.h.to_string()).arg(&c.fps).arg(&c.writer).arg(pkts).arg(out);
    if let (Some((rate, ch, _)), Some(pp)) = (c.audio, pcm_path) {
        cmd.arg(rate.to_string()).arg(ch.to_string()).arg(pp);
    }
    let o = cmd.output().unwrap();
    if o.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        Err(err.lines().filter(|l| !l.starts_with("[GlEnc/writer-fps]")).collect::<Vec<_>>().join(" | ") + &format!(" ({})", o.status))
    }
}

fn be32(d: &[u8], o: usize) -> u64 {
    u64::from(u32::from_be_bytes(d[o..o + 4].try_into().unwrap()))
}

/// The atom path at `off` in a QuickTime file (containers walked; `stsd` entries named).
fn atom_path(d: &[u8], off: usize) -> String {
    const CONTAINERS: [&[u8; 4]; 9] = [b"moov", b"trak", b"edts", b"mdia", b"minf", b"dinf", b"stbl", b"udta", b"wave"];
    let mut path = Vec::new();
    let (mut pos, mut end) = (0usize, d.len());
    'outer: loop {
        while pos + 8 <= end {
            let size32 = be32(d, pos);
            let kind = &d[pos + 4..pos + 8];
            let (size, hdr) = if size32 == 1 { (be32(d, pos + 8) << 32 | be32(d, pos + 12), 16) } else { (size32, 8) };
            let size = size as usize;
            if size < 8 {
                break;
            }
            if off >= pos && off < pos + size {
                path.push(format!("{}@{}", kind.escape_ascii(), pos));
                if CONTAINERS.iter().any(|c| c.as_slice() == kind) {
                    (pos, end) = (pos + hdr, pos + size);
                    continue 'outer;
                }
                if kind == b"stsd" && off >= pos + 16 {
                    (pos, end) = (pos + 16, pos + size);
                    continue 'outer;
                }
                // SoundDescription v1: the extension atoms (wave, chan) follow 8 + 44 bytes.
                if kind == b"in32" && off >= pos + 52 {
                    (pos, end) = (pos + 52, pos + size);
                    continue 'outer;
                }
                break 'outer;
            }
            pos += size;
        }
        break;
    }
    path.join("/")
}

fn compare(a: &[u8], b: &[u8]) -> Option<String> {
    if a == b {
        return None;
    }
    let first = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    Some(format!(
        "DIFFER: swift {} B, rust {} B, first difference at {} ({}): swift {:02x?} rust {:02x?}",
        a.len(),
        b.len(),
        first,
        atom_path(a, first),
        &a[first..(first + 8).min(a.len())],
        &b[first..(first + 8).min(b.len())]
    ))
}

fn summary(d: &[u8]) -> String {
    // Top-level layout + the chunk-offset atom types.
    let mut s = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= d.len() {
        let size32 = be32(d, pos);
        let size = if size32 == 1 { (be32(d, pos + 8) << 32 | be32(d, pos + 12)) as usize } else { size32 as usize };
        s.push(format!("{}{}", d[pos + 4..pos + 8].escape_ascii(), if size32 == 1 { "(64)" } else { "" }));
        if size < 8 {
            break;
        }
        pos += size;
    }
    let co: Vec<&str> = d
        .windows(4)
        .rev()
        .take(4096)
        .filter_map(|w| match w {
            b"stco" => Some("stco"),
            b"co64" => Some("co64"),
            _ => None,
        })
        .collect();
    format!("{} [{}]", s.join(" "), co.into_iter().rev().collect::<Vec<_>>().join(","))
}

fn cases() -> Vec<Case> {
    let base = |name: &str| Case {
        name: name.into(),
        codec: "dxt1",
        w: 1920,
        h: 1080,
        fps: "30".into(),
        writer: "GlEnc 1.6.2".into(),
        pkts: Pkts::Sizes(varied(30, 1)),
        audio: None,
    };
    let mut v = Vec::new();
    for codec in ["dxt1", "dxt5", "ycg6", "yg10", "hap1", "hap5", "hapy", "hapa", "hapm"] {
        v.push(Case { codec, ..base(&format!("codec {codec}")) });
    }
    for (w, h) in [(1, 1), (17, 9), (1920, 1080), (65535, 1), (1, 65535), (65535, 65535)] {
        for codec in ["dxt5", "hapy"] {
            v.push(Case { codec, w, h, ..base(&format!("size {w}x{h} {codec}")) });
        }
    }
    let ntsc = |n: f64| format!("{}", n * 1000.0 / 1001.0);
    let rates = [
        ntsc(24.0),
        "24".into(),
        "25".into(),
        ntsc(30.0),
        "30".into(),
        "48".into(),
        "50".into(),
        ntsc(60.0),
        "60".into(),
        "100".into(),
        "120".into(),
        "30.0000001".into(),
        "29.9999995".into(),
        "1".into(),
        "8388607".into(),
    ];
    for (i, fps) in rates.iter().enumerate() {
        for n in [2usize, 300] {
            v.push(Case { fps: fps.clone(), pkts: Pkts::Sizes(varied(n, i)), codec: "hap5", ..base(&format!("fps {fps} n={n}")) });
        }
    }
    // Refused by both (the oracle crashes or throws; MovWriter returns an error).
    for fps in ["12.5", "47.952", "119.88", "29.97", "23.976", "59.94", "29.5", "0", "0.0000001", "-30", "8388608", "nan", "inf"] {
        v.push(Case { fps: fps.into(), ..base(&format!("refuse fps {fps}")) });
    }
    for (w, h) in [(0, 1080), (1920, 0), (65536, 1), (1, 65536)] {
        v.push(Case { w, h, ..base(&format!("refuse size {w}x{h}")) });
    }
    // Packet counts and empty packets.
    for (name, sizes) in [
        ("0 packets", vec![]),
        ("1 packet", vec![1000]),
        ("2 packets", vec![17, 4096]),
        ("300 packets", varied(300, 9)),
        ("1 empty packet", vec![0]),
        ("first empty", vec![0, 100, 200]),
        ("middle empties", vec![100, 0, 0, 200, 0]),
        ("all empty x5", vec![0; 5]),
        ("big packet 3 MiB", vec![3 << 20, 5]),
    ] {
        v.push(Case { pkts: Pkts::Sizes(sizes), ..base(name) });
    }
    // Writer strings.
    for wr in ["GlEnc", "GlEnc 1.6.2", "glenc-cli 1.0.0", "Glänc ✓ 日本語 🎬", "", "x"] {
        v.push(Case { writer: wr.into(), ..base(&format!("writer {wr:?}")) });
    }
    v.push(Case { writer: "W".repeat(65535), ..base("writer 65535 bytes") });
    // Audio.
    for rate in [44100u32, 48000, 88200, 96000] {
        for ch in [1u32, 2, 6] {
            for (tag, len) in [("1s", rate as usize * ch as usize * 4), ("odd", 12345 * ch as usize * 4 + 3)] {
                v.push(Case { audio: Some((rate, ch, len)), ..base(&format!("audio {rate} Hz {ch} ch {tag}")) });
            }
        }
    }
    for (rate, ch, len, name) in [
        (65535u32, 2u32, 65535 * 8, "audio 65535 Hz (v1 edge)"),
        (65536, 2, 65536 * 8, "audio 65536 Hz (v2 edge)"),
        (8000, 3, 8000 * 12, "audio 8000 Hz 3 ch"),
        (192000, 8, 1000 * 32, "audio 192000 Hz 8 ch"),
        (48000, 2, 5, "audio shorter than one frame"),
        (48000, 2, 48000 * 8 * 5, "audio longer than video (5 s)"),
        (48000, 0, 1000, "audio 0 channels (ignored)"),
        (48000, 2, 0, "audio empty (ignored)"),
        (0, 2, 800, "audio rate 0"),
        (1, 1, 4 * 7, "audio rate 1"),
        (48000, 65535, 65535 * 4 * 2, "audio 65535 ch v1"),
        (48000, 65536, 65536 * 4, "audio 65536 ch v1 (refused)"),
        (96000, 65536, 65536 * 4, "audio 65536 ch v2"),
    ] {
        v.push(Case { audio: Some((rate, ch, len)), ..base(name) });
    }
    v.push(Case { audio: Some((48000, 2, 48000 * 8)), pkts: Pkts::Sizes(vec![]), ..base("audio, 0 video packets") });
    v.push(Case {
        audio: Some((96000, 6, 96000 * 24)),
        codec: "hapm",
        fps: ntsc(30.0),
        pkts: Pkts::Sizes(varied(300, 4)),
        ..base("audio 96k 6ch hapm 29.97 300")
    });
    v
}

fn large_cases() -> Vec<Case> {
    // Swift GlEnc writes a packet with one `FileHandle.write(contentsOf:)`, which fails with EINVAL on macOS for
    // 2^31 bytes or more (write(2) refuses nbyte > INT_MAX), so the largest packet here is INT_MAX.
    const M: u64 = (1 << 31) - 1;
    let base = |name: &str, sizes: Vec<u64>, audio: Option<(u32, u32, usize)>| Case {
        name: name.into(),
        codec: "hap1",
        w: 3840,
        h: 2160,
        fps: "25".into(),
        writer: "GlEnc 1.6.2".into(),
        pkts: Pkts::Big(sizes),
        audio,
    };
    vec![
        base("large: mdat = u32::MAX exactly (32-bit)", vec![M, M - 7], None),
        base("large: mdat = u32::MAX + 1 (largesize, stco)", vec![M, M - 6], None),
        base("large: last offset = u32::MAX (32-bit mdat, stco)", vec![M, M - 35, 5], None),
        base("large: last offset = u32::MAX + 1 (32-bit mdat, co64)", vec![M, M - 34, 5], None),
        base("large: video stco + audio co64", vec![M, M], Some((48000, 2, 48000 * 8))),
        base("large: video co64 + audio co64 (lpcm v2)", vec![M, M, 1000, 2000], Some((96000, 2, 96000 * 8))),
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(args.len() >= 3, "usage: mov_vs_oracle <enc-oracle> <scratch-dir> [--large]");
    let oracle = PathBuf::from(&args[1]);
    let dir = PathBuf::from(&args[2]);
    let large = args.iter().any(|a| a == "--large");
    let only_large = args.iter().any(|a| a == "--only-large");
    fs::create_dir_all(&dir).unwrap();
    if args.iter().any(|a| a == "--golden") {
        golden(&oracle, &dir);
        return;
    }
    let mut list = if only_large { vec![] } else { cases() };
    if large || only_large {
        list.extend(large_cases());
    }
    let (mut same, mut refused, mut bad) = (0, 0, 0);
    for (i, c) in list.iter().enumerate() {
        let pkts = dir.join("case.pkts");
        let pcm_path = dir.join("case.s32le");
        let swift = dir.join("swift.mov");
        let rust = dir.join("rust.mov");
        write_pkts(&pkts, &c.pkts, i as u64 + 1);
        let pcm_arg = c.audio.map(|(_, _, len)| {
            fs::write(&pcm_path, pcm(len, i as u64)).unwrap();
            pcm_path.as_path()
        });
        let _ = fs::remove_file(&swift);
        let _ = fs::remove_file(&rust);
        let s = run_oracle(&oracle, c, &pkts, pcm_arg, &swift);
        let s_bytes = s.as_ref().ok().map(|_| fs::read(&swift).unwrap());
        let _ = fs::remove_file(&swift);
        let r = write_rust(c, &pkts, pcm_arg, &rust);
        let line = match (&s, &r) {
            (Ok(()), Ok(())) => {
                let rb = fs::read(&rust).unwrap();
                let sb = s_bytes.unwrap();
                match compare(&sb, &rb) {
                    None => {
                        same += 1;
                        format!("identical ({} B; {})", rb.len(), summary(&rb))
                    }
                    Some(d) => {
                        bad += 1;
                        d
                    }
                }
            }
            (Err(se), Err(re)) => {
                refused += 1;
                format!("both refuse — swift: {se} — rust: {re}")
            }
            (Ok(()), Err(re)) => {
                bad += 1;
                format!("MISMATCH: swift wrote, rust refused: {re}")
            }
            (Err(se), Ok(())) => {
                bad += 1;
                format!("MISMATCH: rust wrote, swift refused: {se}")
            }
        };
        println!("{:<52} {line}", c.name);
        let _ = fs::remove_file(&rust);
        let _ = fs::remove_file(&pkts);
        let _ = fs::remove_file(&pcm_path);
    }
    println!("\n{} cases: {same} identical, {refused} refused by both, {bad} different", list.len());
    if bad > 0 {
        std::process::exit(1);
    }
}

/// Write the golden set through the oracle; print (name, SHA-256) and check the Rust writer agrees.
fn golden(oracle: &Path, dir: &Path) {
    for c in golden_set::golden_cases() {
        let pkts = dir.join("golden.pkts");
        let pcm_path = dir.join("golden.s32le");
        let swift = dir.join("golden-swift.mov");
        let rust = dir.join("golden-rust.mov");
        let packets = golden_set::golden_packets(&c);
        let mut f = Vec::new();
        for p in &packets {
            f.extend_from_slice(&(p.len() as u32).to_le_bytes());
            f.extend_from_slice(p);
        }
        fs::write(&pkts, f).unwrap();
        let mut cmd = Command::new(oracle);
        cmd.arg("mov").arg(c.codec).arg(c.w.to_string()).arg(c.h.to_string()).arg(format!("{}", c.fps)).arg(c.writer).arg(&pkts).arg(&swift);
        if let Some((rate, ch, _)) = c.audio {
            fs::write(&pcm_path, golden_set::golden_pcm(&c)).unwrap();
            cmd.arg(rate.to_string()).arg(ch.to_string()).arg(&pcm_path);
        }
        assert!(cmd.output().unwrap().status.success(), "oracle failed on {}", c.name);
        let sb = fs::read(&swift).unwrap();
        let mut w = MovWriter::create(&rust, c.fourcc, c.w, c.h, c.fps, c.writer).unwrap();
        for p in &packets {
            w.append(p).unwrap();
        }
        if let Some((rate, ch, _)) = c.audio {
            w.attach_audio(rate, ch, golden_set::golden_pcm(&c));
        }
        w.finish().unwrap();
        let rb = fs::read(&rust).unwrap();
        let hex: String = Sha256::digest(&sb).iter().map(|b| format!("{b:02x}")).collect();
        println!("(\"{}\", {}, \"{hex}\"), // {}", c.name, sb.len(), if sb == rb { "rust identical" } else { "RUST DIFFERS" });
        for p in [&pkts, &pcm_path, &swift, &rust] {
            let _ = fs::remove_file(p);
        }
    }
}
