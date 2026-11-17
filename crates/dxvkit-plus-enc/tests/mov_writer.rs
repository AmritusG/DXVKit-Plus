//! The QuickTime writer's atoms, field by field — Swift GlEnc 1.6.2's writer tests (`DXVMOVWriterTests`,
//! `LargeFileWriterTests`, `RationalTimescaleWriterTests`, `FrameTimingAdoptionTests`, the writer parts of
//! `AudioEncodeTests`) ported to generated packets, plus the layout of every atom `VariantMOVWriter.swift` writes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::arithmetic_side_effects, clippy::indexing_slicing)]

mod common;

use common::*;
use dxvkit_plus_core::container::Movie;
use dxvkit_plus_enc::EncodeError;
use dxvkit_plus_enc::mov::{
    DEFAULT_LARGE_FILE_THRESHOLD, MovWriter, chunk_offset_body, derive_timescale, largesize_mdat_header, needs_co64, timescale_for_rate,
};
use std::path::Path;

const IDENTITY: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

struct Spec<'a> {
    fourcc: [u8; 4],
    w: u32,
    h: u32,
    fps: f64,
    writer: &'a str,
    packets: Vec<Vec<u8>>,
    audio: Option<(u32, u32, Vec<u8>)>,
    threshold: Option<u64>,
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Spec {
            fourcc: *b"DXD3",
            w: 1920,
            h: 1080,
            fps: 30.0,
            writer: "GlEnc 1.6.2",
            packets: (0..30).map(|i| bytes(100 + i * 37, i as u64)).collect(),
            audio: None,
            threshold: None,
        }
    }
}

fn write(s: &Spec) -> Vec<u8> {
    let out = tmp("writer");
    let mut w = MovWriter::create(&out, s.fourcc, s.w, s.h, s.fps, s.writer).unwrap();
    if let Some(t) = s.threshold {
        w.set_large_file_threshold(t);
    }
    for p in &s.packets {
        w.append(p).unwrap();
    }
    if let Some((rate, ch, pcm)) = &s.audio {
        w.attach_audio(*rate, *ch, pcm.clone());
    }
    w.finish().unwrap();
    let d = std::fs::read(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    d
}

fn words(d: &[u8]) -> Vec<u32> {
    d.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect()
}

fn invalid(r: Result<MovWriter, EncodeError>) -> String {
    match r {
        Err(EncodeError::Invalid(s)) => s,
        Err(e) => panic!("expected Invalid, got {e:?}"),
        Ok(_) => panic!("expected an error"),
    }
}

// MARK: - timescale (RationalTimescaleWriterTests, FrameTimingAdoptionTests)

#[test]
fn timescale_table() {
    let ntsc = |n: f64| n * 1000.0 / 1001.0;
    for (fps, want) in [
        (24.0, (12288, 512)),
        (25.0, (12800, 512)),
        (30.0, (15360, 512)),
        (48.0, (24576, 512)),
        (50.0, (25600, 512)),
        (60.0, (30720, 512)),
        (100.0, (51200, 512)),
        (120.0, (61440, 512)),
        (1.0, (512, 512)),
        (30.0000009, (15360, 512)),
        (29.9999991, (15360, 512)),
        (8_388_607.0, (8_388_607 * 512, 512)),
        (ntsc(30.0), (30000, 1001)),
        (ntsc(24.0), (24000, 1001)),
        (ntsc(60.0), (60000, 1001)),
        (ntsc(30.0) + 9e-7, (30000, 1001)),
    ] {
        assert_eq!(derive_timescale(fps).unwrap(), want, "{fps}");
    }
    // Refused: non-integer, non-NTSC (29.5 is Swift's near-miss; 47.952 / 119.88 deliberately unvalidated; the
    // decimal 29.97 is 3e-5 from 30000/1001), and what Swift traps on or divides by zero with.
    for fps in [12.5, 47.952, 119.88, 29.5, 29.97, 23.976, 59.94, 30.000002, ntsc(30.0) + 2e-6, 0.0, 1e-7, -30.0, 8_388_608.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(derive_timescale(fps), Err(EncodeError::Invalid(_))), "{fps} must be refused");
    }
    let msg = derive_timescale(12.5).unwrap_err().to_string();
    assert!(msg.contains("fps 12.5 is not an integer"), "{msg}");
}

#[test]
fn exact_rates_land_on_the_writer_timescale() {
    for (num, den, want) in [
        (30000, 1001, (30000, 1001)),
        (24000, 1001, (24000, 1001)),
        (60000, 1001, (60000, 1001)),
        (25, 1, (25 * 512, 512)),
        (60, 1, (60 * 512, 512)),
        (30, 1, (30 * 512, 512)),
        (50, 2, (25 * 512, 512)),
    ] {
        assert_eq!(timescale_for_rate(num, den).unwrap(), want, "{num}/{den}");
    }
    for (num, den) in [(2997, 100), (25, 2), (48000, 1001), (120000, 1001), (30, 0), (0, 1)] {
        assert!(timescale_for_rate(num, den).is_err(), "{num}/{den}");
    }
}

#[test]
fn ntsc_and_integer_timing_atoms() {
    for (fps, ts, delta) in [(30000.0 / 1001.0, 30000, 1001), (24000.0 / 1001.0, 24000, 1001), (60000.0 / 1001.0, 60000, 1001), (25.0, 12800, 512)] {
        let n = 90usize;
        let d = write(&Spec { fps, packets: vec![vec![1; 10]; n], ..Spec::default() });
        let t = &traks(&d)[0];
        let mdhd = path(&d, t, &[b"mdia", b"mdhd"]).body(&d);
        assert_eq!((be32(mdhd, 12), be32(mdhd, 16)), (ts, n as u32 * delta));
        let stts = child(&d, &stbl(&d, t), b"stts").body(&d);
        assert_eq!(words(stts), vec![0, 1, n as u32, delta]);
        // Movie durations: frames × 1000 × delta / timescale, truncated.
        let ms = (n as u64 * 1000 * u64::from(delta) / u64::from(ts)) as u32;
        assert_eq!(be32(child(&d, &moov(&d), b"mvhd").body(&d), 16), ms);
        assert_eq!(be32(child(&d, t, b"tkhd").body(&d), 20), ms);
        assert_eq!(be32(path(&d, t, &[b"edts", b"elst"]).body(&d), 8), ms);
    }
}

// MARK: - layout (DXVMOVWriterTests)

#[test]
fn top_level_layout_and_preamble() {
    let s = Spec::default();
    let d = write(&s);
    let atoms = top(&d);
    assert_eq!(kinds(&atoms), ["ftyp", "wide", "mdat", "moov"]);
    assert_eq!(&d[..20], b"\0\0\0\x14ftypqt  \0\0\x02\0qt  ");
    assert_eq!(&d[20..28], b"\0\0\0\x08wide");
    assert_eq!(atoms[2].body, 36);
    let body: Vec<u8> = s.packets.concat();
    assert_eq!(be32(&d, 28) as usize, 8 + body.len());
    assert_eq!(&d[36..36 + body.len()], body.as_slice());
    // moov: mvhd, one trak, udta.
    let m = moov(&d);
    assert_eq!(kinds(&children(&d, m.body, m.end)), ["mvhd", "trak", "udta"]);
}

#[test]
fn movie_and_track_headers() {
    let d = write(&Spec::default());
    let mvhd = child(&d, &moov(&d), b"mvhd").body(&d);
    assert_eq!(mvhd.len(), 100);
    let w = words(mvhd);
    assert_eq!(w[..5], [0, 0, 0, 1000, 1000]); // 30 frames at 30 fps = 1000 ms
    assert_eq!(w[5], 0x0001_0000);
    assert_eq!(be16(mvhd, 24), 0x0100);
    assert_eq!(&mvhd[26..36], &[0; 10]);
    assert_eq!(w[9..18], IDENTITY);
    assert_eq!(&mvhd[72..96], &[0; 24]);
    assert_eq!(w[24], 2); // next track id

    let t = &traks(&d)[0];
    assert_eq!(kinds(&children(&d, t.body, t.end)), ["tkhd", "edts", "mdia"]);
    let tkhd = child(&d, t, b"tkhd").body(&d);
    assert_eq!(tkhd.len(), 84);
    let w = words(tkhd);
    assert_eq!(w[..6], [3, 0, 0, 1, 0, 1000]);
    assert_eq!(&tkhd[24..40], &[0; 16]); // reserved, layer, group, volume 0, reserved
    assert_eq!(w[10..19], IDENTITY);
    assert_eq!(w[19..], [1920 << 16, 1080 << 16]);
    assert_eq!(words(path(&d, t, &[b"edts", b"elst"]).body(&d)), [0, 1, 1000, 0, 0x0001_0000]);

    let mdia = child(&d, t, b"mdia");
    assert_eq!(kinds(&children(&d, mdia.body, mdia.end)), ["mdhd", "hdlr", "minf"]);
    let mdhd = child(&d, &mdia, b"mdhd").body(&d);
    assert_eq!(mdhd, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x3c, 0, 0, 0, 0x3c, 0, 0x7f, 0xff, 0, 0]);
    assert_eq!(child(&d, &mdia, b"hdlr").body(&d), b"\0\0\0\0mhlrvide\0\0\0\0\0\0\0\0\0\0\0\0\x0cVideoHandler");
    let minf = child(&d, &mdia, b"minf");
    assert_eq!(kinds(&children(&d, minf.body, minf.end)), ["vmhd", "hdlr", "dinf", "stbl"]);
    assert_eq!(child(&d, &minf, b"vmhd").body(&d), [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(child(&d, &minf, b"hdlr").body(&d), b"\0\0\0\0dhlrurl \0\0\0\0\0\0\0\0\0\0\0\0\x0bDataHandler");
    assert_eq!(child(&d, &minf, b"dinf").body(&d), b"\0\0\0\x1cdref\0\0\0\0\0\0\0\x01\0\0\0\x0curl \0\0\0\x01");
}

#[test]
fn sample_table() {
    let s = Spec { packets: vec![bytes(10, 1), vec![], bytes(300, 2), vec![], vec![], bytes(7, 3)], ..Spec::default() };
    let d = write(&s);
    let st = stbl(&d, &traks(&d)[0]);
    assert_eq!(kinds(&children(&d, st.body, st.end)), ["stsd", "stts", "stsc", "stsz", "stco"]);
    let stsd = child(&d, &st, b"stsd").body(&d);
    assert_eq!(stsd.len(), 94);
    assert_eq!(words(&stsd[..12]), [0, 1, 86]);
    let e = &stsd[12..];
    assert_eq!(&e[..4], b"DXD3");
    assert_eq!(&e[4..12], &[0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(&e[12..16], &[0; 4]); // version, revision
    assert_eq!(&e[16..20], b"FFMP");
    assert_eq!(words(&e[20..28]), [512, 512]);
    assert_eq!((be16(e, 28), be16(e, 30)), (1920, 1080));
    assert_eq!(words(&e[32..44]), [0x0048_0000, 0x0048_0000, 0]);
    assert_eq!(be16(e, 44), 1);
    assert_eq!(&e[46..78], &[0; 32]);
    assert_eq!(&e[78..82], &[0, 24, 0xff, 0xff]);
    assert_eq!(words(child(&d, &st, b"stts").body(&d)), [0, 1, 6, 512]);
    assert_eq!(words(child(&d, &st, b"stsc").body(&d)), [0, 1, 1, 1, 1]);
    assert_eq!(words(child(&d, &st, b"stsz").body(&d)), [0, 0, 6, 10, 0, 300, 0, 0, 7]);
    // One chunk per sample; an empty sample shares its offset with the next.
    assert_eq!(words(child(&d, &st, b"stco").body(&d)), [0, 6, 36, 46, 46, 346, 346, 346]);
}

#[test]
fn no_packets() {
    let d = write(&Spec { packets: vec![], ..Spec::default() });
    assert_eq!(be32(&d, 28), 8);
    let st = stbl(&d, &traks(&d)[0]);
    assert_eq!(words(child(&d, &st, b"stsz").body(&d)), [0, 0, 0]);
    assert_eq!(words(child(&d, &st, b"stco").body(&d)), [0, 0]);
    assert_eq!(be32(child(&d, &moov(&d), b"mvhd").body(&d), 16), 0);
}

#[test]
fn every_fourcc_and_size_routes_to_stsd() {
    for fourcc in [b"DXD3", b"Hap1", b"Hap5", b"HapY", b"HapA", b"HapM"] {
        for (w, h) in [(1, 1), (17, 9), (65535, 1), (1, 65535), (65535, 65535)] {
            let d = write(&Spec { fourcc: *fourcc, w, h, packets: vec![vec![0xab; 16]], ..Spec::default() });
            let t = &traks(&d)[0];
            let stsd = child(&d, &stbl(&d, t), b"stsd").body(&d);
            assert_eq!(&stsd[12..16], fourcc);
            assert_eq!((be16(stsd, 40), be16(stsd, 42)), (w as u16, h as u16));
            let tkhd = child(&d, t, b"tkhd").body(&d);
            assert_eq!((be32(tkhd, 76), be32(tkhd, 80)), (w << 16, h << 16));
        }
    }
}

fn swr(d: &[u8]) -> Vec<u8> {
    let udta = child(d, &moov(d), b"udta");
    let s = children(d, udta.body, udta.end);
    assert_eq!(s.len(), 1);
    assert_eq!(&s[0].kind, &[0xa9, b's', b'w', b'r']);
    let b = s[0].body(d);
    assert_eq!(be16(b, 0) as usize, b.len() - 4);
    assert_eq!(&b[2..4], &[0x55, 0xc4]);
    b[4..].to_vec()
}

#[test]
fn writer_string_round_trips() {
    for s in ["GlEnc", "GlEnc 1.6.2", "glenc-cli 1.0.0", "GlEnc 9.9.9-test", "Glänc ✓ 日本語", ""] {
        assert_eq!(swr(&write(&Spec { writer: s, packets: vec![vec![0; 32]], ..Spec::default() })), s.as_bytes());
    }
    let long = "W".repeat(65535);
    assert_eq!(swr(&write(&Spec { writer: &long, ..Spec::default() })).len(), 65535);
    invalid(MovWriter::create(&tmp("long"), *b"DXD3", 64, 64, 30.0, &"W".repeat(65536)));
}

#[test]
fn refused_settings() {
    let p = tmp("refused");
    for (w, h) in [(0, 1080), (1920, 0), (65536, 1), (1, 65536)] {
        invalid(MovWriter::create(&p, *b"DXD3", w, h, 30.0, "GlEnc"));
    }
    invalid(MovWriter::create(&p, *b"DXD3", 100, 100, 12.5, "GlEnc"));
    // codecFourCC must be ASCII (Swift: "Hap€" is refused; "TooLong" / "abc" cannot be a [u8; 4]).
    let msg = invalid(MovWriter::create(&p, [b'H', b'a', b'p', 0x80], 100, 100, 30.0, "GlEnc"));
    assert!(msg.contains("codecFourCC must be exactly 4 ASCII bytes"), "{msg}");
    assert!(!p.exists(), "nothing is created when the settings are refused");
    // A directory is refused, never deleted.
    let dir = std::env::temp_dir().join(format!("dxvkit-plus-enc-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    invalid(MovWriter::create(&dir, *b"DXD3", 100, 100, 30.0, "GlEnc"));
    assert!(dir.is_dir());
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn an_existing_file_is_replaced() {
    let p = tmp("replace");
    std::fs::write(&p, vec![0x55; 1 << 16]).unwrap();
    let mut w = MovWriter::create(&p, *b"Hap1", 64, 64, 30.0, "GlEnc").unwrap();
    w.append(&[1, 2, 3]).unwrap();
    w.finish().unwrap();
    let d = std::fs::read(&p).unwrap();
    std::fs::remove_file(&p).unwrap();
    assert_eq!(kinds(&top(&d)), ["ftyp", "wide", "mdat", "moov"]);
    assert_eq!(&d[36..39], &[1, 2, 3]);
}

#[test]
fn an_abandoned_writer_leaves_the_partial_file() {
    let p = tmp("abandoned");
    {
        let mut w = MovWriter::create(&p, *b"DXD3", 64, 64, 30.0, "GlEnc").unwrap();
        w.append(&[9; 100]).unwrap();
    }
    let d = std::fs::read(&p).unwrap();
    std::fs::remove_file(&p).unwrap();
    assert_eq!(d.len(), 136);
    assert_eq!(&d[28..36], b"\0\0\0\0mdat");
}

// MARK: - audio (AudioEncodeTests, VariantMOVWriter audio trak)

fn audio_spec(rate: u32, ch: u32, len: usize) -> Spec<'static> {
    Spec { audio: Some((rate, ch, bytes(len, 77))), ..Spec::default() }
}

#[test]
fn audio_in32_v1_entry() {
    for (ch, tag) in [(1u32, 0x0064_0001u32), (2, 0x0065_0002), (6, 0x0093_0006), (3, 0x0093_0003)] {
        let rate = 48000;
        let s = audio_spec(rate, ch, rate as usize * ch as usize * 4 + 3); // 1 s + a partial frame
        let d = write(&s);
        let m = moov(&d);
        assert_eq!(kinds(&children(&d, m.body, m.end)), ["mvhd", "trak", "trak", "udta"]);
        let mvhd = child(&d, &m, b"mvhd").body(&d);
        assert_eq!((be32(mvhd, 16), be32(mvhd, 96)), (1000, 3));
        let t = &traks(&d)[1];
        let tkhd = words(child(&d, t, b"tkhd").body(&d));
        assert_eq!(tkhd[..6], [3, 0, 0, 2, 0, 1000]);
        assert_eq!(tkhd[6..10], [0, 0, 0, 0x0100_0000]); // reserved, layer + group, volume 1.0 (8.8) + reserved
        assert_eq!(tkhd[19..], [0, 0]);
        assert_eq!(words(path(&d, t, &[b"edts", b"elst"]).body(&d)), [0, 1, 1000, 0, 0x0001_0000]);
        let mdhd = path(&d, t, &[b"mdia", b"mdhd"]).body(&d);
        assert_eq!((be32(mdhd, 12), be32(mdhd, 16), be32(mdhd, 20)), (rate, rate, 0x7fff_0000));
        assert_eq!(path(&d, t, &[b"mdia", b"hdlr"]).body(&d), b"\0\0\0\0mhlrsoun\0\0\0\0\0\0\0\0\0\0\0\0\x0cSoundHandler");
        let minf = path(&d, t, &[b"mdia", b"minf"]);
        assert_eq!(kinds(&children(&d, minf.body, minf.end)), ["smhd", "hdlr", "dinf", "stbl"]);
        assert_eq!(child(&d, &minf, b"smhd").body(&d), [0; 8]);
        let st = stbl(&d, t);
        assert_eq!(kinds(&children(&d, st.body, st.end)), ["stsd", "stts", "stsc", "stsz", "stco"]);
        let stsd = child(&d, &st, b"stsd").body(&d);
        assert_eq!(words(&stsd[..8]), [0, 1]);
        let e = &stsd[8..];
        assert_eq!(be32(e, 0) as usize, e.len());
        assert_eq!(e.len(), 114);
        assert_eq!(&e[4..8], b"in32");
        assert_eq!(&e[8..16], &[0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!((be16(e, 16), be16(e, 18), be32(e, 20)), (1, 0, 0)); // version 1, revision, vendor
        assert_eq!((be16(e, 24), be16(e, 26), be16(e, 28), be16(e, 30)), (ch as u16, 16, 0, 0));
        assert_eq!(be32(e, 32), rate << 16);
        assert_eq!(words(&e[36..52]), [1, 4, ch * 4, 2]);
        assert_eq!(&e[52..90], b"\0\0\0\x26wave\0\0\0\x0cfrmain32\0\0\0\x0aenda\0\x01\0\0\0\x08\0\0\0\0");
        assert_eq!(&e[90..98], b"\0\0\0\x18chan");
        assert_eq!(words(&e[98..]), [0, tag, 0, 0]);
        assert_eq!(words(child(&d, &st, b"stts").body(&d)), [0, 1, rate, 1]);
        assert_eq!(words(child(&d, &st, b"stsc").body(&d)), [0, 1, 1, rate, 1]);
        assert_eq!(words(child(&d, &st, b"stsz").body(&d)), [0, ch * 4, rate]);
        // The PCM follows the last video sample, whole (the partial frame included).
        let video: usize = s.packets.iter().map(Vec::len).sum();
        assert_eq!(words(child(&d, &st, b"stco").body(&d)), [0, 1, 36 + video as u32]);
        let pcm = &s.audio.as_ref().unwrap().2;
        assert_eq!(&d[36 + video..36 + video + pcm.len()], pcm.as_slice());
        assert_eq!(be32(&d, 28) as usize, 8 + video + pcm.len());
    }
}

#[test]
fn audio_lpcm_v2_entry_above_65535_hz() {
    for (rate, ch) in [(88200u32, 2u32), (96000, 6), (65536, 1), (192000, 8)] {
        let d = write(&audio_spec(rate, ch, rate as usize * ch as usize * 4));
        let t = &traks(&d)[1];
        let stsd = child(&d, &stbl(&d, t), b"stsd").body(&d);
        let e = &stsd[8..];
        assert_eq!(e.len(), 72);
        assert_eq!(be32(e, 0), 72);
        assert_eq!(&e[4..8], b"lpcm");
        assert_eq!(&e[8..16], &[0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!((be16(e, 16), be16(e, 18), be32(e, 20)), (2, 0, 0));
        assert_eq!(&e[24..32], &[0, 3, 0, 16, 0xff, 0xfe, 0, 0]);
        assert_eq!(words(&e[32..40]), [0x0001_0000, 72]);
        assert_eq!(f64::from_bits(be64(e, 40)), f64::from(rate));
        assert_eq!(words(&e[48..]), [ch, 0x7f00_0000, 32, 0x0c, ch * 4, 1]);
    }
    // 65535 Hz is still v1.
    let d = write(&audio_spec(65535, 2, 65535 * 8));
    let stsd = child(&d, &stbl(&d, &traks(&d)[1]), b"stsd").body(&d);
    assert_eq!(&stsd[12..16], b"in32");
    assert_eq!(be32(stsd, 8 + 32), 65535 << 16);
}

#[test]
fn audio_durations_and_ignored_audio() {
    // Audio longer than the video: mvhd takes the longer track; durations truncate.
    let d = write(&audio_spec(44100, 2, 44100 * 8 * 3 + 8 * 7));
    let mvhd = child(&d, &moov(&d), b"mvhd").body(&d);
    assert_eq!(be32(mvhd, 16), (44100 * 3 + 7) * 1000 / 44100);
    assert_eq!(be32(child(&d, &traks(&d)[0], b"tkhd").body(&d), 20), 1000);
    // Shorter than one frame: a track of 0 frames, the bytes still in mdat.
    let d = write(&audio_spec(48000, 2, 5));
    let st = stbl(&d, &traks(&d)[1]);
    assert_eq!(words(child(&d, &st, b"stsz").body(&d)), [0, 8, 0]);
    // Ignored: empty PCM or 0 channels — byte-identical to no audio.
    let plain = write(&Spec::default());
    assert_eq!(write(&audio_spec(48000, 2, 0)), plain);
    assert_eq!(write(&audio_spec(48000, 0, 4000)), plain);
    // Audio with no video: the PCM starts at 36.
    let d = write(&Spec { packets: vec![], ..audio_spec(48000, 2, 4800) });
    assert_eq!(words(child(&d, &stbl(&d, &traks(&d)[1]), b"stco").body(&d)), [0, 1, 36]);
    assert_eq!(words(child(&d, &stbl(&d, &traks(&d)[0]), b"stco").body(&d)), [0, 0]);
}

#[test]
fn audio_channels_swift_cannot_write_are_refused() {
    // Swift's UInt16(channels) traps above 65535 in the v1 entry; v2 takes a 32-bit count.
    let p = tmp("channels");
    let mut w = MovWriter::create(&p, *b"DXD3", 64, 64, 30.0, "GlEnc").unwrap();
    w.attach_audio(48000, 65536, vec![0; 65536 * 4]);
    assert!(matches!(w.finish(), Err(EncodeError::Invalid(_))));
    std::fs::remove_file(&p).unwrap();
    let d = write(&audio_spec(96000, 65536, 65536 * 4));
    let stsd = child(&d, &stbl(&d, &traks(&d)[1]), b"stsd").body(&d);
    assert_eq!(be32(stsd, 8 + 48), 65536);
}

// MARK: - > 4 GB (LargeFileWriterTests): the pure seams, then the forced path through the threshold seam

#[test]
fn large_file_seams() {
    assert_eq!(DEFAULT_LARGE_FILE_THRESHOLD, u64::from(u32::MAX));
    let t = DEFAULT_LARGE_FILE_THRESHOLD;
    assert!(!needs_co64(0, t));
    assert!(!needs_co64(1_000_000, t));
    assert!(!needs_co64(u64::from(u32::MAX), t));
    assert!(needs_co64(u64::from(u32::MAX) + 1, t));
    assert!(needs_co64(5_000_000_000, t));
    assert!(!needs_co64(100, 100));
    assert!(needs_co64(101, 100));
    assert_eq!(chunk_offset_body(&[36, 0x0102_0304], false).unwrap(), [0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 36, 1, 2, 3, 4]);
    assert_eq!(chunk_offset_body(&[36, 0x1_2345_6789], true).unwrap(), [0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 36, 0, 0, 0, 1, 0x23, 0x45, 0x67, 0x89]);
    let h = largesize_mdat_header(5_000_000_000).unwrap();
    assert_eq!(h.len(), 16);
    assert_eq!(&h[..8], b"\0\0\0\x01mdat");
    assert_eq!(be64(&h, 8), 5_000_000_016);
}

struct Shape {
    wide: bool,
    largesize: bool,
    body: usize,
    offsets: Vec<String>,
}

fn shape(d: &[u8]) -> Shape {
    let atoms = top(d);
    let mdat = atoms.iter().find(|a| &a.kind == b"mdat").unwrap();
    Shape {
        wide: atoms.iter().any(|a| &a.kind == b"wide"),
        largesize: mdat.largesize,
        body: mdat.body,
        offsets: traks(d).iter().map(|t| kinds(&[children(d, stbl(d, t).body, stbl(d, t).end).last().copied().unwrap()])[0].clone()).collect(),
    }
}

fn large_spec(threshold: Option<u64>) -> Spec<'static> {
    Spec { packets: (0..20).map(|i| bytes(500 + i * 13, i as u64)).collect(), audio: Some((48000, 2, bytes(48000 * 8, 5))), threshold, ..Spec::default() }
}

/// Every packet and the PCM read back at the offsets `dxvkit_plus_core` resolves.
fn assert_reads_back(d: &[u8], s: &Spec) {
    let p = tmp("readback");
    std::fs::write(&p, d).unwrap();
    let movie = Movie::open(Path::new(&p)).unwrap();
    std::fs::remove_file(&p).unwrap();
    let t = &movie.tracks[0];
    assert_eq!(t.samples.len(), s.packets.len());
    for ((off, size), pk) in t.samples.iter().zip(&s.packets) {
        assert_eq!(&d[*off as usize..*off as usize + *size as usize], pk.as_slice());
    }
    if let Some((_, _, pcm)) = &s.audio {
        let off = movie.audio[0].chunks[0].offset as usize;
        assert_eq!(&d[off..off + pcm.len()], pcm.as_slice());
    }
}

#[test]
fn forced_large_path() {
    // Threshold below every offset: largesize mdat over `wide`, video + audio co64, the body unmoved at 36.
    let s = large_spec(Some(1024));
    let d = write(&s);
    let sh = shape(&d);
    assert!(sh.largesize && !sh.wide);
    assert_eq!(sh.body, 36);
    assert_eq!(sh.offsets, ["co64", "co64"]);
    let body = s.packets.iter().map(Vec::len).sum::<usize>() + s.audio.as_ref().unwrap().2.len();
    assert_eq!(&d[20..28], b"\0\0\0\x01mdat");
    assert_eq!(be64(&d, 28) as usize, 16 + body);
    // co64 entries: one per sample, 8 bytes each.
    let co64 = child(&d, &stbl(&d, &traks(&d)[0]), b"co64").body(&d);
    assert_eq!(be32(co64, 4), 20);
    assert_eq!((be64(co64, 8), be64(co64, 16)), (36, 536));
    assert_reads_back(&d, &s);
    // The same file at the default threshold differs only in the forms: equal outside mdat's header and moov.
    let plain = write(&large_spec(None));
    assert_eq!(&plain[36..36 + body], &d[36..36 + body]);
}

#[test]
fn forced_mixed_path() {
    // Threshold = the last video offset: video keeps stco, audio (past it) goes co64, mdat is largesize.
    let base = large_spec(None);
    let last_video = 36 + base.packets[..base.packets.len() - 1].iter().map(Vec::len).sum::<usize>();
    let s = large_spec(Some(last_video as u64));
    let d = write(&s);
    let sh = shape(&d);
    assert!(sh.largesize && !sh.wide);
    assert_eq!(sh.offsets, ["stco", "co64"]);
    assert_reads_back(&d, &s);
    // mdat size exactly at the threshold stays 32-bit; one above promotes.
    let body = s.packets.iter().map(Vec::len).sum::<usize>() + s.audio.as_ref().unwrap().2.len();
    let at = write(&large_spec(Some(8 + body as u64)));
    let sh = shape(&at);
    assert!(!sh.largesize && sh.wide);
    assert_eq!(sh.offsets, ["stco", "stco"]);
    let above = write(&large_spec(Some(7 + body as u64)));
    assert!(shape(&above).largesize);
}

#[test]
fn default_threshold_keeps_32_bit_forms() {
    let s = large_spec(None);
    let d = write(&s);
    let sh = shape(&d);
    assert!(!sh.largesize && sh.wide);
    assert_eq!(sh.body, 36);
    assert_eq!(sh.offsets, ["stco", "stco"]);
    assert_reads_back(&d, &s);
}
