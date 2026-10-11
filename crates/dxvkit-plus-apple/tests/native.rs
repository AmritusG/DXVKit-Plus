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
