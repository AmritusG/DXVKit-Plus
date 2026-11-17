//! The QuickTime writer against Swift GlEnc 1.6.2: whole-file SHA-256 of a deterministic generated set
//! (`tests/common/golden_set.rs`), and a round trip through `dxvkit_plus_core`'s demuxer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

mod common;

use common::golden_set::{GoldenCase, golden_cases, golden_packets, golden_pcm};
use common::tmp;
use dxvkit_plus_core::container::Movie;
use dxvkit_plus_enc::mov::MovWriter;
use sha2::{Digest, Sha256};
use std::path::Path;

/// (case, file size, SHA-256) — from Swift GlEnc 1.6.2's VariantMOVWriter via the oracle (glenc-rs
/// `tools/swift-enc-oracle`, `enc-oracle mov`), printed by `examples/mov_vs_oracle.rs --golden`.
const GOLDEN: [(&str, usize, &str); 9] = [
    ("dxt1 1920x1080 30", 61593, "cfe673e7eb50d139fcf2a446eaf6e6e8d5ad4bd7d04fc638c0f63aa010d1a4d3"),
    ("dxt5 17x9 23.976 empties", 668907, "0c742f53ee1571a3bc9cfbf1efa0530b67442a706a8bb58cb5586d82a8a893fb"),
    ("ycg6 1x1 25 one", 816, "1c21ec3560f652dd6c83a8493e269b55a3257caf29d3f8ade637c6493fba87e9"),
    ("yg10 65535x1 29.97 stereo 48k", 516942, "9d50adcc635fe162c2e4baa66cbd6e6ce591ef0a2bb48587b3f2aab671ca74cb"),
    ("hap1 3840x2160 24 mono 44.1k", 279854, "7d7d531100370eb24e590798b7b9880b0bb2bd772ff9f7d98ebfac99f2d1fed2"),
    ("hap5 640x360 59.94 6ch 96k (lpcm v2)", 1418887, "f6bc0e81f97c252ff8a9e7a4c3d074e1916eeda1a7eb8a4a766aa02f0f7ef8ee"),
    ("hapy 1280x720 120 2ch 88.2k (lpcm v2)", 1247876, "f25cc5468a7f82022de11063bf31769ed4b0691061fa5e05f5094e668aeb6443"),
    ("hapa 1x65535 100 no packets", 710, "cabee496f5d20c9aa3041434da5d98622295996d412c219f0b8dad5c74216b3e"),
    ("hapm 1920x1080 50 3ch 8k long audio", 311595, "1f443328ae53a89d3d56ae873d36b707c0455dece2140d1deda0b20846657c24"),
];

fn write(c: &GoldenCase, out: &Path) -> Vec<Vec<u8>> {
    let packets = golden_packets(c);
    let mut w = MovWriter::create(out, c.fourcc, c.w, c.h, c.fps, c.writer).unwrap();
    for p in &packets {
        w.append(p).unwrap();
    }
    assert_eq!(w.frame_count(), packets.len());
    assert_eq!(w.bytes_written(), 36 + packets.iter().map(|p| p.len() as u64).sum::<u64>());
    if let Some((rate, ch, _)) = c.audio {
        w.attach_audio(rate, ch, golden_pcm(c));
    }
    w.finish().unwrap();
    packets
}

#[test]
fn golden_files_equal_swift_glenc() {
    let cases = golden_cases();
    assert_eq!(cases.len(), GOLDEN.len());
    for (c, (name, size, sha)) in cases.iter().zip(GOLDEN) {
        assert_eq!(c.name, name);
        let out = tmp("golden");
        write(c, &out);
        let d = std::fs::read(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        let hex: String = Sha256::digest(&d).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!((d.len(), hex.as_str()), (size, sha), "{name}");
    }
}

/// Every golden file parses back with `dxvkit_plus_core`: FourCC, size, rate, sample sizes and the bytes at
/// each sample offset; the audio track's format, rate, channels and frame count.
#[test]
fn round_trip_through_dxvkit_plus_core() {
    for c in golden_cases() {
        if c.packets == 0 {
            continue; // no samples: core reports no playable video track
        }
        let out = tmp("roundtrip");
        let packets = write(&c, &out);
        let movie = Movie::open(&out).unwrap_or_else(|e| panic!("{}: {e:?}", c.name));
        let d = std::fs::read(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert_eq!(movie.tracks.len(), 1, "{}", c.name);
        let t = &movie.tracks[0];
        assert_eq!(t.fourcc, c.fourcc, "{}", c.name);
        assert_eq!((t.width, t.height), (c.w, c.h), "{}", c.name);
        assert_eq!(t.depth, 24);
        let (ts, delta) = dxvkit_plus_enc::mov::derive_timescale(c.fps).unwrap();
        assert_eq!(t.timescale, ts, "{}", c.name);
        assert_eq!(t.stts, vec![(packets.len() as u32, delta)], "{}", c.name);
        assert_eq!(t.movie_timescale, 1000);
        let rate = t.frame_rate().expect("rate");
        let expected = if delta == 1001 { (i64::from(ts / 1000) * 1000, 1001) } else { (i64::from(ts / 512), 1) };
        assert_eq!((rate.num, rate.den), expected, "{}", c.name);
        assert_eq!(t.samples.len(), packets.len());
        for (i, ((off, size), p)) in t.samples.iter().zip(&packets).enumerate() {
            assert_eq!(*size as usize, p.len(), "{} sample {i}", c.name);
            assert_eq!(&d[*off as usize..*off as usize + p.len()], p.as_slice(), "{} sample {i}", c.name);
        }
        match c.audio {
            None => assert!(movie.audio.is_empty(), "{}", c.name),
            Some((rate, ch, len)) => {
                assert_eq!(movie.audio.len(), 1, "{}", c.name);
                let a = &movie.audio[0];
                let v2 = rate > 65535;
                assert_eq!(&a.fourcc, if v2 { b"lpcm" } else { b"in32" }, "{}", c.name);
                assert_eq!(a.version, if v2 { 2 } else { 1 });
                assert_eq!(a.channels, ch);
                assert_eq!(a.stated_rate, f64::from(rate));
                assert_eq!(a.bits, 32);
                assert_eq!(a.bytes_per_frame, ch * 4);
                assert_eq!(a.timescale, rate);
                let frames = (len / (ch as usize * 4)) as u32;
                assert_eq!(a.sample_count, frames);
                assert_eq!(a.stts, vec![(frames, 1)]);
                let pcm = golden_pcm(&c);
                assert_eq!(a.chunks.len(), 1);
                let off = a.chunks[0].offset as usize;
                assert_eq!(off, 36 + packets.iter().map(Vec::len).sum::<usize>());
                assert_eq!(&d[off..off + pcm.len()], pcm.as_slice());
                let fmt = a.pcm.expect("pcm");
                assert!(fmt.signed && !fmt.float && !fmt.big_endian, "{}: {fmt:?}", c.name);
            }
        }
    }
}
