//! Swift MaCrate 0.16.4's AVFoundation pieces, mirrored: `track_facts` (`AVFoundationSource.open()`),
//! `PlaneReader` (the measurement reader: plane 0 of 32BGRA / 420v / 420f) and `FrameGrabber`
//! (`ThumbnailExtractor`'s `AVAssetImageGenerator` path).
//!
//! No private media: the committed fixtures (`tests/fixtures/`, the `tiles` pattern) and rotated copies
//! this test writes into cargo's per-target temp dir (the `tkhd` matrix set to a 90° turn).
//!
//! Golden values were produced by `scripts/macrate-av-ref.swift`, which calls AVFoundation /
//! CoreGraphics exactly as Swift MaCrate does, on macOS 27.0 (Darwin 27.0.0, Apple silicon),
//! 2026-11-17. Hashes are FNV-1a 64: plane 0 over each row's visible bytes (padding is the buffer's,
//! not the picture), a grab over the whole RGBA buffer. Decoder output can change with an OS update;
//! a golden that fails after one needs the Swift script re-run, not a code change.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_apple::{AvError, BgraReader, FrameGrabber, GrabAlpha, PlaneFormat, PlaneReader, THUMBNAIL_MAX_SIZE, track_facts};
use dxvkit_plus_core::testpattern::read_frame_id;
use dxvkit_plus_core::timing::MediaTime;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name)
}

fn fnv_init() -> u64 {
    0xcbf2_9ce4_8422_2325
}

fn fnv(mut h: u64, data: &[u8]) -> u64 {
    for &b in data {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A copy of `name` whose (first) `tkhd` matrix is a 90° clockwise turn, like a portrait phone clip.
fn rotated(name: &str) -> PathBuf {
    let mut d = std::fs::read(fixture(name)).unwrap();
    let i = d.windows(4).position(|w| w == b"tkhd").unwrap();
    let v = d[i + 4];
    let m = i + 8 + if v == 1 { 32 } else { 20 } + 16;
    let h = u32::from_be_bytes(d[m + 40..m + 44].try_into().unwrap()) >> 16;
    let matrix: [i32; 9] = [0, 0x10000, 0, -0x10000, 0, 0, (h << 16) as i32, 0, 0x4000_0000];
    for (k, x) in matrix.iter().enumerate() {
        d[m + 4 * k..m + 4 * k + 4].copy_from_slice(&x.to_be_bytes());
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("macrate_av");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("rot_{name}"));
    std::fs::write(&out, d).unwrap();
    out
}

// ---------------------------------------------------------------------------------------------
// track_facts

#[test]
fn facts_read_as_swift_macrates_avfoundation_source() {
    let f = track_facts(&fixture("tiles_h264.mp4")).unwrap();
    assert_eq!(f.codec, Some(*b"avc1"));
    assert_eq!(f.codec_string(), "avc1");
    assert_eq!((f.width, f.height, f.nominal_fps, f.duration_seconds), (128, 72, 30.0, 0.8));
    assert_eq!((f.ycbcr_matrix.as_deref(), f.colr_atom, f.full_range, f.depth, f.alpha), (None, false, false, 24, false));
    assert_eq!(f.leading_empty, MediaTime { value: 0, timescale: 1 });

    let f = track_facts(&fixture("tiles_prores4444.mov")).unwrap();
    assert_eq!((f.codec_string().as_str(), f.depth, f.alpha), ("ap4h", 32, true));
    let f = track_facts(&fixture("tiles_prores422.mov")).unwrap();
    assert_eq!((f.codec_string().as_str(), f.depth, f.alpha), ("apcn", 24, false));
    let f = track_facts(&fixture("tiles_mjpeg.mov")).unwrap();
    assert_eq!(f.codec_string(), "jpeg");
    // A matrix tag (from the H.264 stream: no colr atom).
    let f = track_facts(&fixture("edit_h264_bframes.mov")).unwrap();
    assert_eq!((f.ycbcr_matrix.as_deref(), f.colr_atom, f.duration_seconds), (Some("ITU_R_601_4"), false, 0.6));
    // A leading empty edit (AVFoundation describes tracks it cannot decode, too).
    let f = track_facts(&fixture("edit_dxt1_empty_trim.mov")).unwrap();
    assert_eq!((f.codec_string().as_str(), f.duration_seconds), ("DXD3", 0.307));
    assert_eq!(f.leading_empty, MediaTime { value: 7, timescale: 1000 });
    let f = track_facts(&fixture("tiles_hap1.mov")).unwrap();
    assert_eq!((f.codec_string().as_str(), f.width, f.height), ("Hap1", 128, 72));
}

#[test]
fn facts_failures_carry_the_nserror() {
    // Not a movie: AVFoundation's "Cannot Open" (-11828), as Swift's loadTracks throws it.
    let e = track_facts(&fixture("tiles_h264.mp4.attribution")).unwrap_err();
    let AvError::Failed { description, domain, code } = &e else { panic!("{e:?}") };
    assert_eq!((domain.as_str(), *code), ("AVFoundationErrorDomain", -11828));
    assert!(description.starts_with("Error Domain=AVFoundationErrorDomain Code=-11828 \"Cannot Open\" UserInfo={"), "{description}");
    assert_eq!(e.to_string(), *description);
    // A missing file (-11800, underlying -17913).
    let e = track_facts(&fixture("no_such_file.mov")).unwrap_err();
    assert!(matches!(&e, AvError::Failed { domain, code: -11800, .. } if domain == "AVFoundationErrorDomain"), "{e:?}");
}

// ---------------------------------------------------------------------------------------------
// PlaneReader

/// Every frame: (pts, width, height, row_bytes, FNV of the visible bytes so far).
fn read_planes(path: &Path, format: PlaneFormat) -> (Vec<(MediaTime, usize, usize, usize)>, u64, Vec<Vec<u8>>) {
    let mut r = PlaneReader::open(path, format).unwrap();
    let bpp = if format == PlaneFormat::Bgra { 4 } else { 1 };
    let (mut infos, mut h, mut tight) = (Vec::new(), fnv_init(), Vec::new());
    let mut data = Vec::new();
    while let Some(i) = r.next_frame_into(&mut data).unwrap() {
        assert_eq!(data.len(), i.row_bytes * i.height);
        assert!(i.row_bytes >= i.width * bpp);
        let mut t = Vec::new();
        for row in data.chunks(i.row_bytes) {
            h = fnv(h, &row[..i.width * bpp]);
            t.extend_from_slice(&row[..i.width * bpp]);
        }
        tight.push(t);
        infos.push((i.pts, i.width, i.height, i.row_bytes));
    }
    // The end is sticky.
    assert!(r.next_frame().unwrap().is_none());
    r.finish().unwrap();
    (infos, h, tight)
}

#[test]
fn planes_are_what_swift_macrates_reader_serves() {
    // (fixture, frames, FNV for 32BGRA, 420v, 420f as scripts/macrate-av-ref.swift read them)
    for (name, count, golden) in [
        ("tiles_h264.mp4", 24, [0x8d60_52fb_0320_0ea5u64, 0x677b_7a91_2f76_2471, 0xf496_0621_762d_32e1]),
        ("tiles_prores4444.mov", 24, [0xf8d7_9341_c873_daa5, 0xfbb6_4fed_7790_1ca5, 0x4690_af85_c86c_3825]),
        ("tiles_prores422.mov", 24, [0xa758_3afc_275a_6e85, 0x2694_a3f2_7bd1_5025, 0xa3a4_eb68_2ca7_7525]),
        ("tiles_mjpeg.mov", 24, [0xdf2e_d949_82b0_5688, 0x5d43_ac4f_627b_5725, 0x753c_1ea4_508b_92a5]),
        ("edit_h264_bframes.mov", 18, [0x9540_4f2a_c226_62cc, 0x249a_7c1f_f18f_1225, 0x129b_9b80_de9c_aba5]),
    ] {
        for (format, golden) in [PlaneFormat::Bgra, PlaneFormat::Yuv420VideoRange, PlaneFormat::Yuv420FullRange].into_iter().zip(golden) {
            let (infos, h, _) = read_planes(&fixture(name), format);
            assert_eq!(infos.len(), count, "{name} {format:?}");
            assert_eq!(h, golden, "{name} {format:?}: plane 0 differs from Swift's reader");
            for (k, (pts, w, hh, _)) in infos.iter().enumerate() {
                assert_eq!((*w, *hh), (128, 72), "{name} {format:?}");
                // Presentation order, every 1/30 s on a 600 time scale.
                assert_eq!(*pts, MediaTime { value: 20 * k as i64, timescale: 600 }, "{name} {format:?} frame {k}");
            }
        }
    }
}

#[test]
fn bgra_planes_equal_glencs_reader_and_carry_the_pattern() {
    // Same output settings as BgraReader: the same pixels once the stride is removed.
    let path = fixture("jitter_prores4444_30.mov");
    let (infos, _, tight) = read_planes(&path, PlaneFormat::Bgra);
    let mut g = BgraReader::open(&path).unwrap();
    let mut n = 0;
    while let Some(f) = g.next_frame().unwrap() {
        assert_eq!(tight[n], f.data, "frame {n}");
        assert_eq!((infos[n].0.value, i64::from(infos[n].0.timescale)), (f.pts_value, i64::from(f.pts_timescale)));
        n += 1;
    }
    assert_eq!(n, infos.len());
    // The jittered fixture's own times.
    assert_eq!((infos[3].0.value, infos[10].0.value), (61, 201));
    // The pattern's stamped frame index, consecutive.
    let rgba: Vec<Vec<u8>> = tight.iter().map(|b| b.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()).collect();
    let first = read_frame_id(&rgba[0], 128, 72).unwrap();
    for (i, f) in rgba.iter().enumerate() {
        assert_eq!(read_frame_id(f, 128, 72), Some(first + i as u32), "frame {i}");
    }
}

#[test]
fn plane_reader_stops_early_and_fails_as_swift() {
    let mut r = PlaneReader::open(&fixture("tiles_h264.mp4"), PlaneFormat::Yuv420VideoRange).unwrap();
    let f = r.next_frame().unwrap().unwrap();
    assert_eq!(f.data.len(), f.info.row_bytes * f.info.height);
    r.finish().unwrap();
    // No decoder for DXV in AVFoundation: startReading fails with reader.error's description.
    let e = PlaneReader::open(&fixture("edit_dxt1_empty_trim.mov"), PlaneFormat::Bgra).err().unwrap();
    let AvError::StartFailed(Some(d)) = &e else { panic!("{e:?}") };
    assert!(d.starts_with("Error Domain=AVFoundationErrorDomain Code=-11833 \"Cannot Decode\""), "{d}");
    let e = PlaneReader::open(&fixture("tiles_h264.mp4.attribution"), PlaneFormat::Bgra).err().unwrap();
    assert!(matches!(e, AvError::Failed { code: -11828, .. }), "{e:?}");
}

// ---------------------------------------------------------------------------------------------
// FrameGrabber

#[test]
fn grabs_are_what_swift_macrates_thumbnail_generator_draws() {
    // (fixture or rotated copy, max size, seconds, size, FNV of the premultipliedLast RGBA, actual
    // time, image alpha, track alpha) — scripts/macrate-av-ref.swift.
    let h264 = fixture("tiles_h264.mp4");
    let p4444 = fixture("tiles_prores4444.mov");
    let (rot_h264, rot_4444) = (rotated("tiles_h264.mp4"), rotated("tiles_prores4444.mov"));
    for (path, max, s, size, golden, actual, image_alpha, track_alpha) in [
        (&h264, THUMBNAIL_MAX_SIZE, 0.0, (128, 72), 0x4dd3_5886_7189_8d89u64, 0, false, false),
        (&h264, (64.0, 64.0), 0.5, (64, 36), 0x66ee_e491_9270_686a, 300, false, false),
        (&p4444, THUMBNAIL_MAX_SIZE, 0.0, (128, 72), 0xdb8b_c91c_71e9_da25, 0, true, true),
        (&p4444, (64.0, 64.0), 0.5, (64, 36), 0xe170_18f0_0160_0215, 300, true, true),
        (&fixture("tiles_prores422.mov"), THUMBNAIL_MAX_SIZE, 0.0, (128, 72), 0x2e90_a27c_627f_99b5, 0, true, false),
        (&fixture("tiles_mjpeg.mov"), THUMBNAIL_MAX_SIZE, 0.0, (128, 72), 0xb3c0_4bf1_b325_04c8, 0, false, false),
        // The preferred transform is applied: 128x72 turned 90° is 72x128, and the cap applies after.
        (&rot_h264, THUMBNAIL_MAX_SIZE, 0.0, (72, 128), 0xb9b9_8c31_667a_24e5, 0, false, false),
        (&rot_h264, (64.0, 64.0), 0.5, (36, 64), 0x2b0d_99e3_974a_5d1a, 300, false, false),
        (&rot_h264, (32.0, 32.0), 0.3, (18, 32), 0xa5a4_af52_a0dd_b066, 180, false, false),
        (&rot_4444, THUMBNAIL_MAX_SIZE, 0.0, (72, 128), 0xd71a_f88a_6ee1_3625, 0, true, true),
        (&rot_4444, (64.0, 64.0), 0.5, (36, 64), 0x072e_2684_3ba5_9541, 300, true, true),
    ] {
        let mut g = FrameGrabber::open(path, max.0, max.1).unwrap();
        assert_eq!(g.track_alpha(), track_alpha, "{path:?}");
        let img = g.grab_seconds(s, 600, GrabAlpha::Premultiplied).unwrap();
        assert_eq!((img.width, img.height), size, "{path:?} {max:?}");
        assert_eq!(img.rgba.len(), size.0 as usize * size.1 as usize * 4);
        assert_eq!(fnv(fnv_init(), &img.rgba), golden, "{path:?} {max:?} {s}: pixels differ from Swift's");
        assert_eq!(img.actual_time, MediaTime { value: actual, timescale: 600 }, "{path:?}");
        assert_eq!((img.image_has_alpha, img.alpha_mode), (image_alpha, GrabAlpha::Premultiplied), "{path:?}");
        // The same frame by MediaTime.
        let t = MediaTime { value: (s * 600.0).round() as i64, timescale: 600 };
        assert_eq!(g.grab(t, GrabAlpha::Premultiplied).unwrap().rgba, img.rgba);
    }
}

#[test]
fn rotated_grab_is_the_upright_grab_turned() {
    let mut up = FrameGrabber::open(&fixture("tiles_prores422.mov"), 0.0, 0.0).unwrap();
    let mut turned = FrameGrabber::open(&rotated("tiles_prores422.mov"), 0.0, 0.0).unwrap();
    let a = up.grab(MediaTime { value: 0, timescale: 600 }, GrabAlpha::Straight).unwrap();
    let b = turned.grab(MediaTime { value: 0, timescale: 600 }, GrabAlpha::Straight).unwrap();
    assert_eq!((a.width, a.height, b.width, b.height), (128, 72, 72, 128));
    // Clockwise: turned (x', y') shows upright (x = y', y = 71 - x').
    let px = |img: &dxvkit_plus_apple::GrabbedImage, x: u32, y: u32| {
        let o = ((y * img.width + x) * 4) as usize;
        img.rgba[o..o + 4].to_vec()
    };
    let mut worst = 0;
    for y in 0..128 {
        for x in 0..72 {
            let (p, q) = (px(&b, x, y), px(&a, y, 71 - x));
            worst = worst.max(p.iter().zip(&q).map(|(m, n)| m.abs_diff(*n)).max().unwrap());
        }
    }
    assert!(worst <= 2, "rotated pixels differ from the upright ones by {worst}");
}

#[test]
fn straight_grabs_divide_by_alpha() {
    let mut g = FrameGrabber::open(&fixture("tiles_prores4444.mov"), 0.0, 0.0).unwrap();
    let t = MediaTime { value: 0, timescale: 600 };
    let pm = g.grab(t, GrabAlpha::Premultiplied).unwrap();
    let st = g.grab(t, GrabAlpha::Straight).unwrap();
    assert_eq!(st.alpha_mode, GrabAlpha::Straight);
    let mut translucent = 0;
    for (p, s) in pm.rgba.chunks_exact(4).zip(st.rgba.chunks_exact(4)) {
        assert_eq!(p[3], s[3]);
        let a = u32::from(p[3]);
        if a == 255 {
            assert_eq!(p, s);
        } else if a == 0 {
            assert_eq!(&s[..3], &[0, 0, 0]);
        } else {
            translucent += 1;
            for c in 0..3 {
                // Premultiplying the straight value again gives the drawn value back (within rounding).
                let again = (u32::from(s[c]) * a + 127) / 255;
                assert!(again.abs_diff(u32::from(p[c])) <= 1, "{p:?} {s:?}");
            }
        }
    }
    assert!(translucent > 1000, "the 4444 fixture has translucent pixels ({translucent})");
    // Opaque clips are the same either way.
    let mut g = FrameGrabber::open(&fixture("tiles_h264.mp4"), 0.0, 0.0).unwrap();
    assert_eq!(g.grab(t, GrabAlpha::Straight).unwrap().rgba, g.grab(t, GrabAlpha::Premultiplied).unwrap().rgba);
}

#[test]
fn grab_failures_carry_the_nserror() {
    // Opening never decodes; the grab fails as Swift's copyCGImage throws.
    let mut g = FrameGrabber::open(&fixture("edit_dxt1_empty_trim.mov"), 64.0, 64.0).unwrap();
    assert!(!g.track_alpha());
    let e = g.grab(MediaTime { value: 0, timescale: 600 }, GrabAlpha::Straight).unwrap_err();
    assert!(matches!(&e, AvError::Failed { domain, code: -11869, .. } if domain == "AVFoundationErrorDomain"), "{e:?}");
    let mut g = FrameGrabber::open(&fixture("tiles_h264.mp4.attribution"), 64.0, 64.0).unwrap();
    assert!(matches!(g.grab(MediaTime { value: 0, timescale: 600 }, GrabAlpha::Straight).unwrap_err(), AvError::Failed { code: -11828, .. }));
}

// ---------------------------------------------------------------------------------------------
// C ABI values

#[test]
fn abi_values_are_pinned() {
    assert_eq!([PlaneFormat::Bgra, PlaneFormat::Yuv420VideoRange, PlaneFormat::Yuv420FullRange].map(|v| v as u32), [0, 1, 2]);
    assert_eq!([GrabAlpha::Straight, GrabAlpha::Premultiplied].map(|v| v as u32), [0, 1]);
    let all = [
        AvError::PathNotUtf8,
        AvError::Unavailable,
        AvError::NoVideoTrack,
        AvError::Failed { description: String::new(), domain: String::new(), code: 0 },
        AvError::StartFailed(None),
        AvError::ReadFailed(None),
        AvError::PixelBuffer(String::new()),
    ];
    assert_eq!(all.map(|e| e.code()), [1, 2, 3, 4, 5, 6, 7]);
}
