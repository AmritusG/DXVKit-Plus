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
    let variants: Vec<crate::layers::Variant> =
        args.value("variants").unwrap_or("per_layer").split(',').map(crate::layers::Variant::parse).collect::<Result<_, _>>()?;
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
        if info.codec == "LMBX" {
            lmbx_cpu(&info, &mut *src, &base, n, layers)?;
        }
        if args.has("layers-only") && layers > 0 {
            let gpu = Session::new().map_err(|e| e.to_string())?;
            for &v in &variants {
                crate::layers::run(path, &info, layers, n, &gpu.gpu, v)?;
            }
            continue;
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
            for &v in &variants {
                crate::layers::run(path, &info, layers, n, &gpu.gpu, v)?;
            }
        }
    }
    Ok(())
}

/// LMBX on the CPU (the reference decoder: rANS, integer IDCT, YCoCg, to 8-bit RGBA): per frame
/// with the tiles on worker threads and on one thread, and with `layers > 0` that many frames
/// decoded at once (one thread each) per step.
fn lmbx_cpu(
    info: &dxvkit_plus_core::source::MediaInfo,
    src: &mut dyn dxvkit_plus_core::source::FrameSource,
    base: &serde_json::Value,
    n: u32,
    layers: u32,
) -> Result<(), String> {
    use dxvkit_plus_core::lmbx::decode_rgba8;
    let blobs: Vec<Vec<u8>> = (0..n.min(30))
        .map(|f| match src.decode(f) {
            Ok(Decoded::Planes { planes, .. }) => planes.into_iter().next().map(|p| p.data).ok_or_else(|| "LMBX frame without a bitstream".to_string()),
            Ok(_) => Err("not an LMBX frame".to_string()),
            Err(e) => Err(e.to_string()),
        })
        .collect::<Result<_, _>>()?;
    let (w, h) = (info.width, info.height);
    let mut rows = Vec::new();
    for (measure, parallel) in [("cpu_lmbx_rgba_parallel_tiles", true), ("cpu_lmbx_rgba_single_thread", false)] {
        let mut v = Vec::new();
        for b in &blobs {
            let t = Instant::now();
            decode_rgba8(b, w, h, parallel).map_err(|e| e.to_string())?;
            v.push(ms(t));
        }
        rows.push((measure, v));
    }
    if layers > 0 {
        let mut v = Vec::new();
        for step in 0..blobs.len() {
            let t = Instant::now();
            std::thread::scope(|s| {
                let hs: Vec<_> = (0..layers as usize)
                    .map(|l| {
                        let b = &blobs[(step + l) % blobs.len()];
                        s.spawn(move || decode_rgba8(b, w, h, false).map(|_| ()))
                    })
                    .collect();
                hs.into_iter().try_for_each(|h| h.join().map_err(|_| "decoder panicked".to_string())?.map_err(|e| e.to_string()))
            })?;
            v.push(ms(t));
        }
        rows.push(("cpu_lmbx_layers_concurrent", v));
    }
    for (measure, v) in rows {
        let mut row = base.clone();
        if let Some(o) = row.as_object_mut() {
            o.insert("measure".into(), json!(measure));
            if measure == "cpu_lmbx_layers_concurrent" {
                o.insert("layers".into(), json!(layers));
            }
            o.insert("stats".into(), stats(v));
        }
        json_line(&row);
    }
    Ok(())
}
