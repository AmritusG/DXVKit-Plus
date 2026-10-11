//! The GPU path against the CPU oracles, on the Mac Studio's GPU (skips without a GPU unless
//! `DXVKIT_PLUS_REQUIRE_GPU=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::container::{Codec, Movie};
use dxvkit_plus_core::hap;
use dxvkit_plus_core::source::{self, Decoded, ImportOptions, PixelFormat, Plane, Shading, planes_to_rgba};
use dxvkit_plus_core::testpattern::read_frame_id;
use dxvkit_plus_wgpu::{FrameRenderer, Gpu, GpuError, create_bc_texture, read_texture_rgba, write_bc};
use std::path::{Path, PathBuf};

#[path = "../../../tests/support/qt_movie.rs"]
mod qt_movie;
use qt_movie::{bc6h_solid, bc7_solid, hap_frame, video_movie};

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(g) => Some(g),
        Err(e) if std::env::var("DXVKIT_PLUS_REQUIRE_GPU").as_deref() != Ok("1") => {
            eprintln!("SKIP: no GPU ({e})");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

fn render(g: &Gpu, r: &mut FrameRenderer, d: &Decoded) -> Vec<u8> {
    let out = r.render(&g.device, &g.queue, d).unwrap().clone();
    read_texture_rgba(&g.device, &g.queue, &out).unwrap()
}

/// splitmix64.
fn rng(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Our CPU BC7 decoder equals the GPU's own BC7 decode, bit for bit, on 4096 random blocks of each
/// of the eight modes (random partitions, rotations, index selections, endpoints, p-bits, indices).
#[test]
fn bc7_cpu_decoder_matches_the_gpu_bit_for_bit() {
    let Some(g) = gpu() else { return };
    let mut r = FrameRenderer::new(&g.device);
    let (w, h) = (256u32, 256u32);
    let mut state = 7u64;
    let mut total = 0usize;
    for mode in 0..8u32 {
        let mut data = Vec::with_capacity((w * h) as usize);
        for _ in 0..(w / 4) * (h / 4) {
            let mut v = (u128::from(rng(&mut state)) << 64) | u128::from(rng(&mut state));
            v &= !((1u128 << (mode + 1)) - 1);
            v |= 1u128 << mode;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let planes = vec![Plane { format: PixelFormat::Bc7, width: w, height: h, data }];
        let cpu = planes_to_rgba(Shading::Rgba { opaque: false }, &planes, w, h).unwrap();
        let d = Decoded::Planes { index: 0, width: w, height: h, shading: Shading::Rgba { opaque: false }, planes };
        let gpu = render(&g, &mut r, &d);
        let bad = cpu.chunks_exact(4).zip(gpu.chunks_exact(4)).filter(|(a, b)| a != b).count();
        let first = cpu.chunks_exact(4).zip(gpu.chunks_exact(4)).position(|(a, b)| a != b);
        assert_eq!(
            bad,
            0,
            "mode {mode}: {bad} pixels differ, first at {first:?}: cpu {:?} gpu {:?}",
            first.map(|i| &cpu[i * 4..i * 4 + 4]),
            first.map(|i| &gpu[i * 4..i * 4 + 4])
        );
        total += (w * h) as usize;
    }
    eprintln!("BC7: {total} pixels, CPU == GPU");
    // Negative control: a corrupted CPU result would be caught.
    let planes = vec![Plane { format: PixelFormat::Bc7, width: 4, height: 4, data: bc7_solid(100, 50, 25, 127, 1).to_vec() }];
    let mut cpu = planes_to_rgba(Shading::Rgba { opaque: false }, &planes, 4, 4).unwrap();
    cpu[0] ^= 1;
    let gpu = render(&g, &mut r, &Decoded::Planes { index: 0, width: 4, height: 4, shading: Shading::Rgba { opaque: false }, planes });
    assert_ne!(cpu, gpu);
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Every HAP fixture frame on the GPU matches the CPU oracle within 1/255 and carries its index.
#[test]
fn hap_fixtures_gpu_match_the_cpu() {
    let Some(g) = gpu() else { return };
    let mut r = FrameRenderer::new(&g.device);
    for name in ["tiles_hap1.mov", "tiles_hap1_chunks4.mov", "tiles_hap5.mov", "tiles_hapa.mov", "tiles_hapy.mov", "tiles_hapm.mov", "tiles_hapm_chunks4.mov"] {
        let p = fixtures().join(name);
        let opts = ImportOptions::default();
        let info = source::probe(p.to_str().unwrap(), &opts).unwrap();
        let mut src = source::open(&info, &opts, true).unwrap();
        let mut worst = 0u8;
        for f in 0..info.frames {
            let d = src.decode(f).unwrap();
            let cpu = d.to_rgba().unwrap();
            let gpu = render(&g, &mut r, &d);
            worst = worst.max(cpu.iter().zip(&gpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap());
            if !name.contains("hapa") {
                assert_eq!(read_frame_id(&gpu, info.width, info.height), Some(f), "{name} frame {f}");
            }
            assert_eq!(r.last_upload_bytes, d.cpu_bytes(), "blocks upload as-is");
        }
        eprintln!("{name}: GPU vs CPU max diff {worst}");
        assert!(worst <= 1, "{name}: {worst}");
    }
}

/// HAP R (BC7) and HAP HDR (BC6H) movies render their colours (blocks built from the public BPTC
/// definition; no encoder we may use writes these variants).
#[test]
fn hap_r_and_hap_hdr_render_their_colours() {
    let Some(g) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("dxvkit-plus-gpu-hap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bc7: Vec<u8> = (0..4).flat_map(|_| bc7_solid(100, 50, 25, 127, 1)).collect();
    let p7 = dir.join("solid_hap7.mov");
    std::fs::write(&p7, video_movie(b"Hap7", 8, 8, &[hap_frame(0xAC, &bc7)])).unwrap();
    // HAP HDR: 10-bit 462 -> half 0x3801 (0.5005) -> 8-bit 128; 0 -> 0.
    let bc6: Vec<u8> = (0..4).flat_map(|_| bc6h_solid(462, 0, 462)).collect();
    let ph = dir.join("solid_haph.mov");
    std::fs::write(&ph, video_movie(b"HapH", 8, 8, &[hap_frame(0xA2, &bc6)])).unwrap();
    let mut r = FrameRenderer::new(&g.device);
    let mut one = |p: &Path| {
        let m = Movie::open(p).unwrap();
        let t = m.track(Codec::Hap).unwrap().clone();
        let mut pkt = Vec::new();
        m.read_sample(&t, 0, &mut pkt).unwrap();
        let (shading, planes) = hap::planes(hap::decode(&pkt, 8, 8, false).unwrap(), 8, 8).unwrap();
        render(&g, &mut r, &Decoded::Planes { index: 0, width: 8, height: 8, shading, planes })
    };
    let (r7, rh) = (one(&p7), one(&ph));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(r7.chunks_exact(4).all(|p| p == [201, 101, 51, 255]), "HAP R: {:?}", &r7[..4]);
    for p in rh.chunks_exact(4) {
        assert!(p[0].abs_diff(128) <= 1 && p[1] == 0 && p[2].abs_diff(128) <= 1 && p[3] == 255, "HAP HDR: {p:?}");
    }
}

#[test]
fn bc_upload_is_validated() {
    let Some(g) = gpu() else { return };
    let t = create_bc_texture(&g.device, PixelFormat::Bc1, 8, 8, "t").unwrap();
    assert!(write_bc(&g.queue, &t, PixelFormat::Bc1, 8, 8, &[0; 32]).is_ok());
    // Negative controls: wrong byte count, wrong format, unaligned size.
    assert!(matches!(write_bc(&g.queue, &t, PixelFormat::Bc1, 8, 8, &[0; 31]), Err(GpuError::Invalid(_))));
    assert!(matches!(write_bc(&g.queue, &t, PixelFormat::Bc3, 8, 8, &[0; 64]), Err(GpuError::Invalid(_))));
    assert!(matches!(create_bc_texture(&g.device, PixelFormat::Bc7, 6, 8, "t"), Err(GpuError::Invalid(_))));
}
