//! Native decode on macOS: H.264, HEVC, ProRes 422, ProRes 4444 (alpha) and MJPEG fixtures decode
//! frame-accurately and import zero-copy (the GPU texture aliases the decoder's IOSurface; no byte
//! is uploaded), through both import routes.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_apple::surface::{TextureCacheImporter, import};
use dxvkit_plus_apple::{AppleBackend, NativeReader};
use dxvkit_plus_core::backend::{Backend, NativeDecoder, NativeFrame, PixelLayout};
use dxvkit_plus_core::testpattern::{read_frame_id, tiles};
use dxvkit_plus_wgpu::{FrameRenderer, Gpu, NativePlanes, read_texture_rgba};
use std::path::{Path, PathBuf};

/// (fixture, sample-entry FourCC, has alpha).
const NATIVE: [(&str, &[u8; 4], bool); 5] = [
    ("tiles_h264.mp4", b"avc1", false),
    ("tiles_hevc.mp4", b"hvc1", false),
    ("tiles_prores422.mov", b"apcn", false),
    ("tiles_prores4444.mov", b"ap4h", true),
    ("tiles_mjpeg.mov", b"jpeg", false),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name)
}

fn layout(fourcc: &[u8; 4], alpha: bool) -> PixelLayout {
    dxvkit_plus_core::source::layout_for(fourcc, alpha)
}

fn to_rgba(g: &Gpu, r: &mut FrameRenderer, f: &NativeFrame, cache: Option<&TextureCacheImporter>) -> Vec<u8> {
    let planes = match cache {
        Some(c) => c.import(&g.device, f).unwrap(),
        None => import(&g.device, f).unwrap(),
    };
    assert!(planes.iter().all(|p| p.surface_id == f.surface_id), "the texture aliases the frame's IOSurface");
    let textures: Vec<wgpu::Texture> = planes.into_iter().map(|p| p.texture).collect();
    let n = NativePlanes { textures: &textures, layout: f.layout, matrix: f.matrix, width: f.width, height: f.height };
    let out = r.render_native(&g.device, &g.queue, &n).unwrap().clone();
    assert_eq!(r.last_upload_bytes, 0, "zero-copy: nothing uploaded");
    read_texture_rgba(&g.device, &g.queue, &out).unwrap()
}

#[test]
fn every_native_fixture_decodes_zero_copy_and_frame_accurately() {
    let g = Gpu::headless().unwrap();
    let cache = TextureCacheImporter::new(&g.device).unwrap();
    let mut r = FrameRenderer::new(&g.device);
    for (name, fourcc, alpha) in NATIVE {
        let mut rd = AppleBackend.open(&fixture(name), layout(fourcc, alpha)).unwrap();
        let info = rd.info().clone();
        assert_eq!((info.width, info.height, info.frame_count), (128, 72, 24), "{name}");
        let mut seq = Vec::new();
        for f in 0..info.frame_count {
            let fr = rd.frame(f).unwrap();
            assert_eq!(fr.index, f, "{name}");
            let a = to_rgba(&g, &mut r, &fr, None);
            let b = to_rgba(&g, &mut r, &fr, Some(&cache));
            assert_eq!(a, b, "{name} frame {f}: the two import routes agree");
            assert_eq!(read_frame_id(&a, 128, 72), Some(f), "{name} frame {f}");
            seq.push(a);
        }
        // Seek: backward, forward jumps and repeats on a fresh reader give the same pixels.
        let mut s = AppleBackend.open(&fixture(name), layout(fourcc, alpha)).unwrap();
        for f in [17u32, 3, 3, 23, 0, 12, 11, 22, 5] {
            let fr = s.frame(f).unwrap();
            assert_eq!(fr.index, f);
            assert_eq!(to_rgba(&g, &mut r, &fr, None), seq[f as usize], "{name}: seek to {f}");
        }
        // Accuracy against the source pattern: lossy 4:2:0 codecs at 128x72 smear chroma across
        // the sharp 8-pixel tile edges, so this is a loose bound on the mean (the tight checks are
        // frame identity above, `yuv_conversion_matches_apples_own` and ProRes 4444's exact
        // read-back below).
        let src = tiles(7, 128, 72);
        let (mut sum, mut n, mut max) = (0u64, 0u64, 0u8);
        for (p, q) in seq[7].chunks_exact(4).zip(src.chunks_exact(4)) {
            if alpha && q[3] != 255 {
                continue;
            }
            for c in 0..3 {
                let d = p[c].abs_diff(q[c]);
                sum += u64::from(d);
                n += 1;
                max = max.max(d);
            }
        }
        let mean = sum as f64 / n as f64;
        eprintln!("{name}: frame 7 vs source mean RGB diff {mean:.2}, max {max}");
        assert!(mean <= 20.0, "{name}: mean {mean}");
        // Negative control: out of range is an error.
        assert!(s.frame(24).is_err());
    }
}

/// ProRes 4444's alpha survives, and our zero-copy BGRA import equals Apple's own CPU read-back.
#[test]
fn prores_4444_alpha_matches_apples_own_read_back() {
    let g = Gpu::headless().unwrap();
    let mut r = FrameRenderer::new(&g.device);
    let mut rd = NativeReader::open(&fixture("tiles_prores4444.mov"), PixelLayout::Bgra).unwrap();
    for f in [0u32, 9, 23] {
        let fr = rd.frame(f).unwrap();
        let cpu = rd.cpu_rgba(&fr).unwrap();
        let gpu = to_rgba(&g, &mut r, &fr, None);
        assert_eq!(cpu, gpu, "frame {f}");
        let src = tiles(f, 128, 72);
        let worst_a = gpu.chunks_exact(4).zip(src.chunks_exact(4)).map(|(p, q)| p[3].abs_diff(q[3])).max().unwrap();
        assert!(worst_a <= 2, "frame {f}: alpha diff {worst_a}");
    }
}

#[test]
fn missing_and_garbage_files_are_errors() {
    assert!(AppleBackend.open(Path::new("/nonexistent/x.mov"), PixelLayout::Bgra).is_err());
    let p = std::env::temp_dir().join(format!("dxvkit-plus-garbage-{}.mov", std::process::id()));
    std::fs::write(&p, vec![0x5Au8; 4096]).unwrap();
    assert!(AppleBackend.open(&p, PixelLayout::Bgra).is_err());
    let _ = std::fs::remove_file(&p);
}

/// Our 4:2:0 YCbCr conversion (WGSL, from the zero-copy planes) against Apple's own conversion of
/// the same frame (the decoder asked for BGRA): inside the flat 8x8 tiles (away from chroma edges)
/// they agree — the matrix and range are right — and over whole frames ours is no further from the
/// source pattern than Apple's (chroma siting / upsampling filters differ at sharp edges).
#[test]
fn yuv_conversion_matches_apples_own() {
    let g = Gpu::headless().unwrap();
    let mut r = FrameRenderer::new(&g.device);
    // Run the 4:2:0 conversion on every 4:2:2 / 4:2:0 fixture (forced to 4:2:0 here), including
    // the ones `layout_for` sends as BGRA, to check the matrix on all of them.
    for (name, _, alpha) in NATIVE {
        if alpha {
            continue;
        }
        let mut yuv = NativeReader::open(&fixture(name), PixelLayout::Yuv420 { full_range: false }).unwrap();
        let mut bgra = NativeReader::open(&fixture(name), PixelLayout::Bgra).unwrap();
        let (mut isum, mut ind, mut ours_src, mut apple_src) = (0u64, 0u64, 0f64, 0f64);
        for f in [0u32, 7, 15, 23] {
            let ours = to_rgba(&g, &mut r, &yuv.frame(f).unwrap(), None);
            let fb = bgra.frame(f).unwrap();
            let apple = bgra.cpu_rgba(&fb).unwrap();
            let src = tiles(f, 128, 72);
            let m = |a: &[u8]| {
                a.chunks_exact(4).zip(src.chunks_exact(4)).map(|(p, q)| (0..3).map(|c| u64::from(p[c].abs_diff(q[c]))).sum::<u64>()).sum::<u64>() as f64
                    / (128.0 * 72.0 * 3.0)
            };
            ours_src += m(&ours) / 4.0;
            apple_src += m(&apple) / 4.0;
            for (i, (p, q)) in ours.chunks_exact(4).zip(apple.chunks_exact(4)).enumerate() {
                let (x, y) = (i % 128, i / 128);
                if (2..6).contains(&(x % 8)) && (2..6).contains(&(y % 8)) {
                    for c in 0..3 {
                        isum += u64::from(p[c].abs_diff(q[c]));
                        ind += 1;
                    }
                }
            }
        }
        let interior = isum as f64 / ind as f64;
        eprintln!("{name}: tile interiors ours vs Apple mean {interior:.3}; whole frame vs source: ours {ours_src:.2}, Apple {apple_src:.2}");
        assert!(interior <= 1.0, "{name}: interior mean {interior}");
        // Only for true 4:2:0 codecs is 4:2:0 the right request (`layout_for`); a 4:2:2 source
        // forced through 4:2:0 loses vertical chroma, which is why those decode as BGRA.
        if matches!(name, "tiles_h264.mp4" | "tiles_hevc.mp4") {
            assert!(ours_src <= apple_src + 0.5, "{name}: ours {ours_src} vs Apple {apple_src}");
        }
    }
}

/// The alpha each native fixture states (0.9.0), read from AVFoundation's format description by
/// Swift Glance's rule, and carried into `MediaInfo`: ProRes 4444 straight, the others none.
#[test]
fn native_clips_state_their_alpha() {
    use dxvkit_plus_core::source::{self, ImportOptions, StatedAlpha};
    let opts = ImportOptions { backend: Some(std::sync::Arc::new(AppleBackend)), ..ImportOptions::default() };
    for (name, fourcc, alpha) in NATIVE {
        let want = if alpha { StatedAlpha::Straight } else { StatedAlpha::None };
        let rd = AppleBackend.open(&fixture(name), layout(fourcc, alpha)).unwrap();
        assert_eq!(rd.info().alpha, want, "{name}: backend");
        let info = source::probe(fixture(name).to_str().unwrap(), &opts).unwrap();
        assert_eq!((info.alpha, info.has_alpha), (want, alpha), "{name}: probe");
    }
}

/// glance-core C1 finding 5, on a real fragmented MP4 written by AVFoundation
/// (`tests/fixtures/frag_h264.mp4`, scripts/make-fragmented-mp4.swift): its moov has empty sample
/// tables, so our parser refuses the track; the probe offers the file to AVFoundation, which plays
/// it — every frame decodes.
#[test]
fn a_fragmented_mp4_plays_through_the_platform() {
    use dxvkit_plus_core::source::{self, ImportOptions, SourceKind};
    let path = fixture("frag_h264.mp4");
    let m = dxvkit_plus_core::container::Movie::open(&path);
    assert!(matches!(m, Err(dxvkit_plus_core::MediaError::Corrupt(_))), "our parser: {m:?}");
    let opts = ImportOptions { backend: Some(std::sync::Arc::new(AppleBackend)), ..ImportOptions::default() };
    let info = source::probe(path.to_str().unwrap(), &opts).unwrap();
    assert_eq!((info.kind, info.width, info.height, info.frames, info.codec_name.as_str()), (SourceKind::Native, 64, 48, 60, "H.264"));
    let mut src = source::open(&info, &opts, false).unwrap();
    for f in 0..info.frames {
        assert_eq!(src.decode(f).unwrap().size(), (64, 48), "frame {f}");
    }
    assert!(src.decode(info.frames).is_err());
    // Control: without a backend it is our parser's error.
    assert!(matches!(source::probe(path.to_str().unwrap(), &ImportOptions::default()), Err(dxvkit_plus_core::MediaError::Corrupt(_))));
}

/// glance-core C1 finding 1 in the island: a frame larger than the device's largest texture is an
/// error value from both import routes, before any texture exists (a frame handle that is not a
/// pixel buffer at all shows the size check comes first); a frame within the limit gets past it.
#[test]
fn a_frame_larger_than_the_device_is_refused_by_both_imports() {
    use dxvkit_plus_core::backend::YuvMatrix;
    let g = Gpu::headless().unwrap();
    let cache = TextureCacheImporter::new(&g.device).unwrap();
    let limit = g.device.limits().max_texture_dimension_2d;
    let frame = |height: u32| NativeFrame {
        index: 0,
        width: 16,
        height,
        layout: PixelLayout::Bgra,
        matrix: YuvMatrix::Bt709,
        surface_id: 0,
        handle: std::sync::Arc::new(()),
    };
    for r in [import(&g.device, &frame(limit + 4)).err(), cache.import(&g.device, &frame(limit + 4)).err()] {
        match r {
            Some(dxvkit_plus_core::MediaError::Unsupported(m)) => assert!(m.contains(&(limit + 4).to_string()) && m.contains(&limit.to_string()), "{m}"),
            other => panic!("{other:?}"),
        }
    }
    // Control: at the limit the size passes and the (fake) handle is what fails.
    assert!(!matches!(import(&g.device, &frame(limit)), Err(dxvkit_plus_core::MediaError::Unsupported(_)) | Ok(_)));
}

/// W3 (0.10.0): a native clip with B-frames (ctts) and a trimming edit list numbers its frames by
/// core's timing — the frames AVAssetReader serves, at their real starts — so frame n shows source
/// frame 3 + n on sequential and shuffled reads, and the count is the 18 frames served, not the 22
/// samples in the table.
#[test]
fn edit_lists_and_composition_offsets_number_native_frames() {
    let path = fixture("edit_h264_bframes.mov");
    let g = Gpu::headless().unwrap();
    let mut r = FrameRenderer::new(&g.device);
    let core = dxvkit_plus_core::container::Movie::open(&path).unwrap();
    let track = core.first().unwrap();
    // Control: the table holds 22 samples and composition offsets.
    assert_eq!((track.frame_count(), track.ctts.iter().any(|&o| o != 0)), (22, true));
    let mut rd = AppleBackend.open(&path, PixelLayout::Bgra).unwrap();
    assert_eq!(rd.info().frame_count, 18);
    assert_eq!(rd.timing(), Some(&track.timing), "the decoder numbers frames by core's timing");
    assert_eq!(rd.info().frame_duration, (1, 30));
    let mut seq = Vec::new();
    for f in 0..18 {
        let fr = rd.frame(f).unwrap();
        assert_eq!(fr.index, f);
        let px = to_rgba(&g, &mut r, &fr, None);
        assert_eq!(read_frame_id(&px, 128, 72), Some(3 + f), "frame {f}");
        seq.push(px);
    }
    assert!(rd.frame(18).is_err(), "past the served frames is an error");
    let mut s = AppleBackend.open(&path, PixelLayout::Bgra).unwrap();
    for f in [11u32, 2, 17, 0, 9, 9, 16, 1, 13, 4] {
        let fr = s.frame(f).unwrap();
        assert_eq!(fr.index, f);
        assert_eq!(to_rgba(&g, &mut r, &fr, None), seq[f as usize], "seek to {f}");
    }
    // The probe agrees: 18 frames at 30 fps.
    let opts = dxvkit_plus_core::source::ImportOptions { backend: Some(std::sync::Arc::new(AppleBackend)), ..Default::default() };
    let info = dxvkit_plus_core::source::probe(path.to_str().unwrap(), &opts).unwrap();
    assert_eq!((info.frames, info.rate), (18, dxvkit_plus_core::time::FrameRate::FPS_30));
}

/// W3: a file core cannot parse (the fragmented MP4) gets the grid from Swift's rate rules over
/// AVFoundation's own timing: 60 frames at 30 fps.
#[test]
fn a_fragmented_mp4_gets_the_grid_timing() {
    let rd = AppleBackend.open(&fixture("frag_h264.mp4"), PixelLayout::Bgra).unwrap();
    let t = rd.timing().unwrap();
    assert!(!t.has_table());
    assert_eq!((t.frame_count(), t.rate), (60, dxvkit_plus_core::time::FrameRate::FPS_30));
    // Control: core's parser does refuse the file.
    assert!(dxvkit_plus_core::container::Movie::open(&fixture("frag_h264.mp4")).is_err());
}

/// W3: jittered native timing (frames 3, 10, 17 one tick late at 1/600, shortest sample 19 ticks).
/// Every frame decodes as itself at 30 fps, sequentially and on seeks, and the count is 24.
#[test]
fn jittered_native_timing_is_frame_exact() {
    use dxvkit_plus_core::timing::MediaTime;
    let path = fixture("jitter_prores4444_30.mov");
    let g = Gpu::headless().unwrap();
    let mut r = FrameRenderer::new(&g.device);
    let mut rd = AppleBackend.open(&path, PixelLayout::Bgra).unwrap();
    let t = rd.timing().unwrap().clone();
    assert_eq!((rd.info().frame_count, t.rate), (24, dxvkit_plus_core::time::FrameRate::FPS_30));
    assert_eq!(t.time_of_frame(3), MediaTime::new(61, 600), "the real start, one tick late");
    // Control: 0.9.0's rule — the grid of the shortest sample (19 / 600), rounded to the nearest
    // point — names frame 3's start (61 ticks) frame 3 but frame 10's (201) frame 11, and counts
    // round(480 / 19) = 25 frames.
    let old = |ticks: i64| (ticks * 2 + 19) / (2 * 19);
    assert_eq!((old(61), old(201), old(480)), (3, 11, 25));
    let mut seq = Vec::new();
    for f in 0..24 {
        let fr = rd.frame(f).unwrap();
        assert_eq!(fr.index, f);
        let px = to_rgba(&g, &mut r, &fr, None);
        assert_eq!(read_frame_id(&px, 128, 72), Some(f), "frame {f}");
        seq.push(px);
    }
    let mut s = AppleBackend.open(&path, PixelLayout::Bgra).unwrap();
    for f in [10u32, 3, 17, 11, 4, 18, 0, 23, 9, 10] {
        let fr = s.frame(f).unwrap();
        assert_eq!(fr.index, f);
        assert_eq!(to_rgba(&g, &mut r, &fr, None), seq[f as usize], "seek to {f}");
    }
}
