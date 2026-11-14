//! Cost of the conversion pass on each block path, as wall time of a submission that the GPU is
//! waited for (an upper bound on GPU time): `convert_cost <file> [layers] [rounds]`.
//!
//! `layers` renderers per path each hold their own frame of the clip (uploaded once, not timed).
//! One measured submission records every layer's conversion pass, repeated until it holds 64 passes
//! (so one layer is not lost in the latency of a submission), and waits for the GPU; the two paths
//! alternate submission by submission, so both see the same background load. Prints one JSON line:
//! per path the median and the fastest submission, as milliseconds per step (every layer converted
//! once) and microseconds per pass. Release builds only give meaningful numbers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::source::{self, Decoded, ImportOptions};
use dxvkit_plus_wgpu::{BlockPath, FrameRenderer, Gpu, read_texture_rgba, wait_idle};
use std::time::Instant;

/// Conversion passes per measured submission.
const PASSES: u32 = 64;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: convert_cost <file> [layers] [rounds]");
    let layers: u32 = args.get(2).map_or(16, |s| s.parse().unwrap());
    let rounds: usize = args.get(3).map_or(200, |s| s.parse().unwrap());
    assert!((1..=PASSES).contains(&layers), "layers 1..={PASSES}");
    let opts = ImportOptions::default();
    let info = source::probe(path, &opts).unwrap();
    let mut src = source::open(&info, &opts, true).unwrap();
    let frames: Vec<Decoded> = (0..layers).map(|l| src.decode(l % info.frames.max(1)).unwrap()).collect();
    let gpu = Gpu::headless().unwrap();
    let errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = errors.clone();
    gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
        eprintln!("wgpu error: {e}");
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let paths = [BlockPath::Sampled, BlockPath::Exact];
    let mut renderers: Vec<Vec<FrameRenderer>> = paths.iter().map(|&p| (0..layers).map(|_| FrameRenderer::with_path(&gpu.device, p)).collect()).collect();
    // Upload once; guard: the exact path's first layer equals the CPU decode (a measurement of a
    // wrong decode is no measurement), and report how far the sampled path is from it.
    let cpu = frames[0].to_rgba().unwrap();
    let mut sampled_max = 0u8;
    for (p, rs) in paths.iter().zip(renderers.iter_mut()) {
        for (r, d) in rs.iter_mut().zip(&frames) {
            r.render(&gpu.device, &gpu.queue, d).unwrap();
        }
        let got = read_texture_rgba(&gpu.device, &gpu.queue, rs[0].output().unwrap()).unwrap();
        let worst = cpu.iter().zip(&got).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        match p {
            BlockPath::Exact => assert_eq!(worst, 0, "{path}: the exact path differs from the CPU decode"),
            BlockPath::Sampled => sampled_max = worst,
        }
    }
    let repeats = PASSES / layers;
    let passes = repeats * layers;
    let submission = |rs: &mut Vec<FrameRenderer>| -> f64 {
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        for _ in 0..repeats {
            for r in rs.iter_mut() {
                r.encode_convert(&gpu.device, &gpu.queue, &mut enc, None).unwrap();
            }
        }
        let t = Instant::now();
        gpu.queue.submit([enc.finish()]);
        wait_idle(&gpu.device).unwrap();
        t.elapsed().as_secs_f64() * 1000.0
    };
    let mut ms: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); paths.len()];
    for round in 0..rounds + 20 {
        for (i, rs) in renderers.iter_mut().enumerate() {
            let t = submission(rs);
            if round >= 20 {
                ms[i].push(t); // the first 20 rounds warm up
            }
        }
    }
    assert_eq!(errors.load(std::sync::atomic::Ordering::SeqCst), 0, "wgpu reported errors");
    let stat = |v: &mut Vec<f64>| -> (f64, f64) {
        v.sort_by(f64::total_cmp);
        (v[v.len() / 2], v[0])
    };
    let mut out = format!(
        "{{\"file\":\"{}\",\"codec\":\"{}\",\"width\":{},\"height\":{},\"layers\":{layers},\"passes_per_submission\":{passes},\"rounds\":{rounds},\"sampled_max_diff_vs_cpu\":{sampled_max}",
        path.rsplit('/').next().unwrap_or(path),
        info.codec,
        info.width,
        info.height
    );
    for (p, v) in paths.iter().zip(ms.iter_mut()) {
        let (median, fastest) = stat(v);
        let name = if *p == BlockPath::Exact { "exact" } else { "sampled" };
        let (per_step, per_pass) = (f64::from(layers) / f64::from(passes), 1000.0 / f64::from(passes));
        out.push_str(&format!(
            ",\"{name}\":{{\"ms_per_step_median\":{:.4},\"ms_per_step_fastest\":{:.4},\"us_per_pass_median\":{:.1},\"us_per_pass_fastest\":{:.1}}}",
            median * per_step,
            fastest * per_step,
            median * per_pass,
            fastest * per_pass
        ));
    }
    println!("{out}}}");
}
