//! `dxvkit-plus bench`: decode p50 / p99 per file (codec, resolution), CPU and GPU, plus optional
//! multi-layer and native import-route measurements. Release builds only give meaningful numbers.

use crate::gpu::{ImportRoute, Session};
use crate::{Args, json_line, probe_open};
use dxvkit_plus_core::source::{self, Decoded, SourceKind};
use serde_json::{Value, json};
use std::time::Instant;

/// p50 / p99 / max / mean of a list of milliseconds.
pub fn stats(mut v: Vec<f64>) -> Value {
    if v.is_empty() {
        return json!({"n": 0});
    }
    v.sort_by(f64::total_cmp);
    let q = |p: f64| -> f64 {
        let i = ((v.len().saturating_sub(1)) as f64 * p).round() as usize;
        let x = v.get(i).copied().unwrap_or(0.0);
        (x * 1000.0).round() / 1000.0
    };
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    json!({"n": v.len(), "p50_ms": q(0.5), "p99_ms": q(0.99), "max_ms": q(1.0), "mean_ms": (mean * 1000.0).round() / 1000.0})
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

pub fn run(args: &Args) -> Result<(), String> {
    if args.positional.is_empty() {
        return Err(crate::USAGE.into());
    }
    let frames = args.num("frames", 60)?.max(1);
    let layers = args.num("layers", 0)?;
    for path in &args.positional {
        let (info, mut src) = probe_open(path)?;
        let n = frames.min(info.frames.max(1));
        let mut base = json!({"file": path, "codec": info.codec, "kind": info.kind, "width": info.width, "height": info.height, "frames": n});
        if let (SourceKind::Lmbd, Some(o)) = (info.kind, base.as_object_mut()) {
            o.insert("zstd_backend".into(), json!(crate::ZSTD_BACKEND));
        }
        // CPU decode (read + decode: the work a cache worker does), 3 passes.
        let mut cpu = Vec::new();
        for _ in 0..3 {
            for f in 0..n {
                let t = Instant::now();
                src.decode(f).map_err(|e| e.to_string())?;
                cpu.push(ms(t));
            }
        }
        let mut row = base.clone();
        if let Some(o) = row.as_object_mut() {
            o.insert("measure".into(), json!(if info.kind == SourceKind::Native { "platform_decode" } else { "cpu_decode_parallel_chunks" }));
            o.insert("stats".into(), stats(cpu));
        }
        json_line(&row);
        if info.kind == SourceKind::Hap {
            let opts = crate::options();
            let mut single = source::open(&info, &opts, false).map_err(|e| e.to_string())?;
            let mut v = Vec::new();
            for _ in 0..3 {
                for f in 0..n {
                    let t = Instant::now();
                    single.decode(f).map_err(|e| e.to_string())?;
                    v.push(ms(t));
                }
            }
            let mut row = base.clone();
            if let Some(o) = row.as_object_mut() {
                o.insert("measure".into(), json!("cpu_decode_single_thread"));
                o.insert("stats".into(), stats(v));
            }
            json_line(&row);
        }
        // GPU: upload (or zero-copy import) + convert + wait, on frames decoded beforehand; and the
        // whole path (decode + upload + convert + wait).
        let mut gpu = Session::new().map_err(|e| e.to_string())?;
        let decoded: Vec<Decoded> = (0..n.min(30)).map(|f| src.decode(f)).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
        let mut up = Vec::new();
        for _ in 0..3 {
            for d in &decoded {
                let t = Instant::now();
                gpu.render_wait(d)?;
                up.push(ms(t));
            }
        }
        let mut whole = Vec::new();
        for f in 0..n {
            let t = Instant::now();
            let d = src.decode(f).map_err(|e| e.to_string())?;
            gpu.render_wait(&d)?;
            whole.push(ms(t));
        }
        for (measure, v) in [("gpu_upload_convert", up), ("end_to_end", whole)] {
            let mut row = base.clone();
            if let Some(o) = row.as_object_mut() {
                o.insert("measure".into(), json!(measure));
                o.insert("upload_bytes_per_frame".into(), json!(gpu.renderer.last_upload_bytes));
                o.insert("stats".into(), stats(v));
            }
            json_line(&row);
        }
        if args.has("import") && info.kind == SourceKind::Native {
            for route in [ImportRoute::IoSurface, ImportRoute::TextureCache] {
                gpu.set_route(route)?;
                let mut v = Vec::new();
                for _ in 0..5 {
                    for d in &decoded {
                        let t = Instant::now();
                        gpu.render_wait(d)?;
                        v.push(ms(t));
                    }
                }
                let mut row = base.clone();
                if let Some(o) = row.as_object_mut() {
                    o.insert("measure".into(), json!("native_import_convert"));
                    o.insert("route".into(), json!(route.name()));
                    o.insert("stats".into(), stats(v));
                }
                json_line(&row);
            }
            gpu.set_route(ImportRoute::TextureCache)?;
        }
        if layers > 0 {
            layers_bench(path, &info, layers, n, &mut gpu)?;
        }
    }
    Ok(())
}

/// `layers` independent decoders of the same file (as a VJ stacks clips): each step decodes frame
/// `f` on every layer in parallel (one thread per layer), then uploads + converts all layers on one
/// device and waits. Wall time per step.
fn layers_bench(path: &str, info: &source::MediaInfo, layers: u32, n: u32, gpu: &mut Session) -> Result<(), String> {
    let opts = crate::options();
    let mut srcs = Vec::new();
    for _ in 0..layers {
        srcs.push(source::open(info, &opts, false).map_err(|e| e.to_string())?);
    }
    let mut renderers: Vec<dxvkit_plus_wgpu::FrameRenderer> = (0..layers).map(|_| dxvkit_plus_wgpu::FrameRenderer::new(&gpu.gpu.device)).collect();
    let (mut decode_ms, mut step_ms) = (Vec::new(), Vec::new());
    for pass in 0..2 {
        for f in 0..n {
            let t = Instant::now();
            let decoded: Vec<Result<Decoded, String>> = std::thread::scope(|s| {
                let hs: Vec<_> = srcs.iter_mut().map(|src| s.spawn(move || src.decode(f).map_err(|e| e.to_string()))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("decode thread panicked".into()))).collect()
            });
            let td = ms(t);
            for (r, d) in renderers.iter_mut().zip(decoded) {
                let d = d?;
                match &d {
                    Decoded::Planes { .. } => {
                        r.render(&gpu.gpu.device, &gpu.gpu.queue, &d).map_err(|e| e.to_string())?;
                    }
                    Decoded::Native(_) => return Err("the layer benchmark runs block codecs (HAP) only".into()),
                }
            }
            dxvkit_plus_wgpu::wait_idle(&gpu.gpu.device).map_err(|e| e.to_string())?;
            if pass > 0 {
                decode_ms.push(td);
                step_ms.push(ms(t));
            }
        }
    }
    let mut row = json!({"file": path, "codec": info.codec, "width": info.width, "height": info.height, "measure": "layers", "layers": layers,
        "decode_all_layers": stats(decode_ms), "step_decode_upload_convert": stats(step_ms)});
    if let (SourceKind::Lmbd, Some(o)) = (info.kind, row.as_object_mut()) {
        o.insert("zstd_backend".into(), json!(crate::ZSTD_BACKEND));
    }
    json_line(&row);
    Ok(())
}
