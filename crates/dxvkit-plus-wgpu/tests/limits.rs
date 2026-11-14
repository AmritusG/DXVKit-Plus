//! The device's largest texture (glance-core C1 finding 1): `Gpu` asks for the adapter's own limit
//! (at most `MAX_EDGE`), and a frame larger than the device holds is a `GpuError::Unsupported` with
//! the numbers — never a wgpu validation error with an `Ok` and an invalid texture. Skips without a
//! GPU unless `DXVKIT_PLUS_REQUIRE_GPU=1`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::source::{Decoded, PixelFormat, Plane, Shading};
use dxvkit_plus_wgpu::{BlockPath, FrameRenderer, Gpu, GpuError, MAX_EDGE, read_texture_rgba};

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

/// A second device on the same adapter with wgpu's default limits (largest texture 8192): a GPU
/// smaller than the frame.
fn small_device(g: &Gpu) -> (wgpu::Device, wgpu::Queue) {
    let required_features = g.adapter.features() & wgpu::Features::TEXTURE_COMPRESSION_BC;
    pollster::block_on(g.adapter.request_device(&wgpu::DeviceDescriptor { label: Some("small"), required_features, ..Default::default() })).unwrap()
}

/// A `w` x `h` frame of `format` (RGBA8 or block planes) filled with a pattern.
fn frame(format: PixelFormat, w: u32, h: u32) -> Decoded {
    let bytes = dxvkit_plus_wgpu::frame_bytes(format, w, h).unwrap();
    let data: Vec<u8> = (0..bytes).map(|i| (i * 7 % 251) as u8).collect();
    let shading = Shading::Rgba { opaque: false };
    Decoded::Planes { index: 0, width: w, height: h, shading, planes: vec![Plane { format, width: w, height: h, data }] }
}

/// Run `f` and return its value with the validation errors wgpu raised meanwhile.
fn scoped<T>(device: &wgpu::Device, f: impl FnOnce() -> T) -> (T, Option<wgpu::Error>) {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let out = f();
    (out, pollster::block_on(scope.pop()))
}

#[test]
fn the_device_asks_for_the_adapters_largest_texture() {
    let Some(g) = gpu() else { return };
    let want = g.adapter.limits().max_texture_dimension_2d.min(MAX_EDGE);
    eprintln!("adapter limit {}, device limit {}", g.adapter.limits().max_texture_dimension_2d, g.device.limits().max_texture_dimension_2d);
    assert_eq!(g.device.limits().max_texture_dimension_2d, want);
}

/// SCANTEST's case: 3840 x 9626 (here 16 x 9628, the same height rounded to whole blocks).
#[test]
fn a_frame_larger_than_the_device_is_an_error_value() {
    let Some(g) = gpu() else { return };
    let (device, queue) = small_device(&g);
    let limit = device.limits().max_texture_dimension_2d;
    assert!(limit < 9628, "the default limit is {limit}");
    for path in [BlockPath::Sampled, BlockPath::Exact] {
        for format in [PixelFormat::Rgba8, PixelFormat::Bc1, PixelFormat::Bc3] {
            if format != PixelFormat::Rgba8 && !device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC) {
                continue;
            }
            let mut r = FrameRenderer::with_path(&device, path);
            let (result, error) = scoped(&device, || r.render(&device, &queue, &frame(format, 16, 9628)).map(|_| ()));
            assert!(error.is_none(), "{path:?} {format:?}: wgpu raised {error:?}");
            match result {
                Err(GpuError::Unsupported(m)) => assert!(m.contains("9628") && m.contains(&limit.to_string()), "{path:?} {format:?}: {m}"),
                other => panic!("{path:?} {format:?}: {other:?}"),
            }
            // Negative control: a frame at the limit renders, with no validation error.
            let (ok, error) = scoped(&device, || r.render(&device, &queue, &frame(format, 16, limit)).map(|_| ()));
            assert!(ok.is_ok() && error.is_none(), "{path:?} {format:?}: {ok:?} {error:?}");
        }
    }
}

/// On a device that has the room (this Mac: 16384), the tall frame renders and reads back as stored.
#[test]
fn a_tall_frame_renders_where_the_device_holds_it() {
    let Some(g) = gpu() else { return };
    if g.device.limits().max_texture_dimension_2d < 9628 {
        eprintln!("SKIP: this device's largest texture is {}", g.device.limits().max_texture_dimension_2d);
        return;
    }
    let d = frame(PixelFormat::Rgba8, 16, 9628);
    let mut r = FrameRenderer::new(&g.device);
    let (out, error) = scoped(&g.device, || r.render(&g.device, &g.queue, &d).cloned());
    assert!(error.is_none(), "{error:?}");
    assert_eq!(read_texture_rgba(&g.device, &g.queue, &out.unwrap()).unwrap(), d.to_rgba().unwrap());
}
