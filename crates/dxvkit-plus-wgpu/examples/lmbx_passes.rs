//! Per-pass GPU time of LMBX decode: `lmbx_passes <file.lmbd> [layers] [steps]`. Prints the
//! median over steps of each pass's time summed over the layers, and the step span (ms).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::lmbd::{LmbdFile, decode_payload};
use dxvkit_plus_wgpu::{Gpu, profile_passes};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: lmbx_passes <file.lmbd> [layers] [steps]");
    let layers: u32 = args.get(2).map_or(16, |s| s.parse().unwrap());
    let steps: u32 = args.get(3).map_or(60, |s| s.parse().unwrap());
    let file = LmbdFile::open(std::path::Path::new(path)).unwrap();
    let h = file.header().clone();
    let n = file.frame_count().min(30);
    let mut frames = Vec::new();
    let mut payload = Vec::new();
    for i in 0..n {
        file.read_payload(i, &mut payload).unwrap();
        let mut blob = Vec::new();
        decode_payload(&payload, &h, true, &mut blob).unwrap();
        frames.push(blob);
    }
    // Tile mode census (colour mode, alpha mode) over the frames.
    let mut census = std::collections::BTreeMap::<String, u32>::new();
    for b in &frames {
        let f = dxvkit_plus_core::lmbx::Frame::parse(b, h.width, h.height).unwrap();
        for t in 0..f.header.tile_count() {
            *census.entry(format!("{:?}/{:?}", f.mode(t), f.alpha_mode(t))).or_default() += 1;
        }
    }
    let v = dxvkit_plus_core::lmbx::Frame::parse(&frames[0], h.width, h.height).unwrap().header.version;
    eprintln!("{path}: v{v} tiles {census:?}");
    let gpu = Gpu::headless().unwrap();
    // Guard: the GPU decode of the first frames equals the CPU's (a measurement of a wrong decode
    // is no measurement), and wgpu reported no error.
    let errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let e2 = errors.clone();
    gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
        eprintln!("wgpu error: {e}");
        e2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let mut r = dxvkit_plus_wgpu::FrameRenderer::new(&gpu.device);
    for blob in frames.iter().take(if std::env::var_os("LMBX_NOGUARD").is_some() { 0 } else { 3 }) {
        use dxvkit_plus_core::source::{Decoded, PixelFormat, Plane, Shading};
        let planes = vec![Plane { format: PixelFormat::LmbxBitstream, width: h.width, height: h.height, data: blob.clone() }];
        let d = Decoded::Planes { index: 0, width: h.width, height: h.height, shading: Shading::Lmbx, planes };
        let out = r.render(&gpu.device, &gpu.queue, &d).unwrap().clone();
        let g = dxvkit_plus_wgpu::read_texture_rgba(&gpu.device, &gpu.queue, &out).unwrap();
        assert!(g == d.to_rgba().unwrap(), "{path}: GPU decode differs from the CPU's");
    }
    // Warm-up, then the measured steps.
    profile_passes(&gpu, &frames, h.width, h.height, layers, 5).unwrap();
    let mut rows: Vec<Vec<f64>> = profile_passes(&gpu, &frames, h.width, h.height, layers, steps).unwrap().into_iter().map(|r| r.to_vec()).collect();
    // The last row: the run's sustained GPU time per step (back-to-back submissions).
    let last = rows.pop().unwrap_or_default();
    let (busy, sustained) = (last.get(3).copied().unwrap_or(0.0), last.get(4).copied().unwrap_or(0.0));
    assert_eq!(errors.load(std::sync::atomic::Ordering::SeqCst), 0, "wgpu reported errors");
    let med = |k: usize| {
        let mut v: Vec<f64> = rows.iter().map(|r| r[k]).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let n = rows.first().map_or(0, Vec::len);
    let passes: Vec<String> = (0..n - 1).map(|k| format!("{:.3}", med(k))).collect();
    println!(
        "{{\"file\":\"{path}\",\"layers\":{layers},\"steps\":{steps},\"passes_ms\":[{}],\"span_ms\":{:.3},\"sustained_ms_per_step\":{:.3},\"busy_ms_per_step\":{:.3}}}",
        passes.join(","),
        med(n - 1),
        sustained,
        busy
    );
}
