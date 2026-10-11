//! `dxvkit-plus bench --layers N`: N independent decoders of one file (as a VJ stacks clips), all
//! on one device. Each step decodes frame `f` on every layer (one thread per layer), uploads and
//! converts every layer, and waits for the GPU. Reports wall time per step and a per-bucket
//! breakdown (CPU wall clocks per phase; GPU time of the conversion passes from timestamp
//! queries where the device has them).

use crate::bench::stats;
use crate::json_line;
use dxvkit_plus_core::source::{self, Decoded, SourceKind};
use dxvkit_plus_wgpu::{FrameRenderer, Gpu, wait_idle};
use serde_json::json;
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Timestamp queries bracketing each layer's conversion pass.
struct Stamps {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    count: u32,
    period_ns: f64,
}

impl Stamps {
    fn new(gpu: &Gpu, passes: u32) -> Option<Stamps> {
        if !gpu.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let count = passes.checked_mul(2)?;
        let size = u64::from(count) * 8;
        let set = gpu.device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("bench-ts"), ty: wgpu::QueryType::Timestamp, count });
        let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bench-ts-resolve"),
            size,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bench-ts-read"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Some(Stamps { set, resolve, readback, count, period_ns: f64::from(gpu.queue.get_timestamp_period()) })
    }

    fn writes(&self, pass: u32) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(pass.saturating_mul(2)),
            end_of_pass_write_index: Some(pass.saturating_mul(2).saturating_add(1)),
        }
    }

    fn resolve(&self, enc: &mut wgpu::CommandEncoder) {
        enc.resolve_query_set(&self.set, 0..self.count, &self.resolve, 0);
        enc.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, u64::from(self.count) * 8);
    }

    /// (sum of pass durations, first begin to last end) in ms, after the GPU is idle.
    fn read(&self, device: &wgpu::Device) -> Result<(f64, f64), String> {
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        wait_idle(device).map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let ticks: Vec<u64> = {
            let m = slice.get_mapped_range().map_err(|e| e.to_string())?;
            m.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect()
        };
        self.readback.unmap();
        let ns = |t: u64| t as f64 * self.period_ns / 1e6;
        let pairs: Vec<(u64, u64)> = ticks.chunks_exact(2).filter_map(|p| if let [a, b] = *p { Some((a, b)) } else { None }).collect();
        let busy: f64 = pairs.iter().map(|&(a, b)| ns(b.saturating_sub(a))).sum();
        let first = pairs.iter().map(|p| p.0).min().unwrap_or(0);
        let last = pairs.iter().map(|p| p.1).max().unwrap_or(0);
        Ok((busy, ns(last.saturating_sub(first))))
    }
}

/// Per-step bucket samples (ms).
#[derive(Default)]
struct Buckets {
    step: Vec<f64>,
    decode: Vec<f64>,
    upload_cpu: Vec<f64>,
    encode: Vec<f64>,
    submit: Vec<f64>,
    wait: Vec<f64>,
    gpu_convert: Vec<f64>,
    gpu_convert_span: Vec<f64>,
}

/// One way of getting a step's frames onto the GPU (the bench's A/B candidates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// `FrameRenderer::render` per layer: `write_texture` + a submit per layer (0.2.x).
    PerLayer,
    /// `write_texture` per layer, one submission for all layers.
    Batched,
    /// Persistent staging belt + `copy_buffer_to_texture`, one submission.
    Staged,
}

impl Variant {
    pub fn parse(s: &str) -> Result<Variant, String> {
        Ok(match s {
            "per_layer" => Variant::PerLayer,
            "batched" => Variant::Batched,
            "staged" => Variant::Staged,
            other => return Err(format!("unknown variant {other} (per_layer, batched, staged)")),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Variant::PerLayer => "per_layer",
            Variant::Batched => "batched",
            Variant::Staged => "staged",
        }
    }
}

pub fn run(path: &str, info: &source::MediaInfo, layers: u32, n: u32, gpu: &Gpu, variant: Variant) -> Result<(), String> {
    let opts = crate::options();
    let mut srcs = Vec::new();
    for _ in 0..layers {
        srcs.push(source::open(info, &opts, false).map_err(|e| e.to_string())?);
    }
    let mut renderers: Vec<FrameRenderer> = (0..layers).map(|_| FrameRenderer::new(&gpu.device)).collect();
    let stamps = Stamps::new(gpu, layers);
    let mut belt: Option<wgpu::util::StagingBelt> = None;
    let mut b = Buckets::default();
    for pass in 0..2 {
        for f in 0..n {
            let t = Instant::now();
            let decoded: Vec<Result<Decoded, String>> = std::thread::scope(|s| {
                let hs: Vec<_> = srcs.iter_mut().map(|src| s.spawn(move || src.decode(f).map_err(|e| e.to_string()))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("decode thread panicked".into()))).collect()
            });
            let td = ms(t);
            let decoded: Vec<Decoded> = decoded.into_iter().collect::<Result<_, _>>()?;
            if decoded.iter().any(|d| matches!(d, Decoded::Native(_))) {
                return Err("the layer benchmark runs block codecs (HAP, LMBD) only".into());
            }
            let (mut up, mut en, mut sb) = (0.0, 0.0, 0.0);
            let new_encoder = || gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render") });
            match variant {
                Variant::PerLayer => {
                    for (i, (r, d)) in renderers.iter_mut().zip(&decoded).enumerate() {
                        let t1 = Instant::now();
                        r.upload(&gpu.device, &gpu.queue, d).map_err(|e| e.to_string())?;
                        up += ms(t1);
                        let t1 = Instant::now();
                        let mut enc = new_encoder();
                        r.encode_convert(&gpu.device, &gpu.queue, &mut enc, stamps.as_ref().map(|s| s.writes(i as u32))).map_err(|e| e.to_string())?;
                        let cb = enc.finish();
                        en += ms(t1);
                        let t1 = Instant::now();
                        gpu.queue.submit([cb]);
                        sb += ms(t1);
                    }
                }
                Variant::Batched | Variant::Staged => {
                    let mut enc = new_encoder();
                    let t1 = Instant::now();
                    if variant == Variant::Staged {
                        let belt = belt.get_or_insert_with(|| {
                            let bytes: usize = decoded.iter().map(Decoded::cpu_bytes).sum();
                            wgpu::util::StagingBelt::new(gpu.device.clone(), (bytes as u64).max(1 << 20))
                        });
                        for (r, d) in renderers.iter_mut().zip(&decoded) {
                            r.upload_staged(&gpu.device, &mut enc, belt, d).map_err(|e| e.to_string())?;
                        }
                        belt.finish();
                    } else {
                        for (r, d) in renderers.iter_mut().zip(&decoded) {
                            r.upload(&gpu.device, &gpu.queue, d).map_err(|e| e.to_string())?;
                        }
                    }
                    up += ms(t1);
                    let t1 = Instant::now();
                    for (i, r) in renderers.iter_mut().enumerate() {
                        r.encode_convert(&gpu.device, &gpu.queue, &mut enc, stamps.as_ref().map(|s| s.writes(i as u32))).map_err(|e| e.to_string())?;
                    }
                    let cb = enc.finish();
                    en += ms(t1);
                    let t1 = Instant::now();
                    gpu.queue.submit([cb]);
                    if let Some(belt) = belt.as_mut() {
                        belt.recall();
                    }
                    sb += ms(t1);
                }
            }
            if let Some(s) = &stamps {
                let t1 = Instant::now();
                let mut enc = gpu.device.create_command_encoder(&Default::default());
                s.resolve(&mut enc);
                gpu.queue.submit([enc.finish()]);
                sb += ms(t1);
            }
            let t1 = Instant::now();
            wait_idle(&gpu.device).map_err(|e| e.to_string())?;
            let tw = ms(t1);
            let step = ms(t);
            let gpu_times = stamps.as_ref().map(|s| s.read(&gpu.device)).transpose()?;
            if pass > 0 {
                b.step.push(step);
                b.decode.push(td);
                b.upload_cpu.push(up);
                b.encode.push(en);
                b.submit.push(sb);
                b.wait.push(tw);
                if let Some((busy, span)) = gpu_times {
                    b.gpu_convert.push(busy);
                    b.gpu_convert_span.push(span);
                }
            }
        }
    }
    let mut row = json!({"file": path, "codec": info.codec, "width": info.width, "height": info.height, "measure": "layers", "layers": layers,
    "variant": variant.name(),
    "decode_all_layers": stats(b.decode.clone()), "step_decode_upload_convert": stats(b.step),
    "breakdown_p50_ms": {
        "cpu_decode": p50(&b.decode), "cpu_upload": p50(&b.upload_cpu), "cpu_encode": p50(&b.encode),
        "cpu_submit": p50(&b.submit), "cpu_wait_gpu": p50(&b.wait), "gpu_convert_passes_sum": p50(&b.gpu_convert),
        "gpu_convert_first_to_last": p50(&b.gpu_convert_span)
    }});
    if let (SourceKind::Lmbd, Some(o)) = (info.kind, row.as_object_mut()) {
        o.insert("zstd_backend".into(), json!(crate::ZSTD_BACKEND));
    }
    json_line(&row);
    Ok(())
}

fn p50(v: &[f64]) -> f64 {
    stats(v.to_vec()).get("p50_ms").and_then(|x| x.as_f64()).unwrap_or(0.0)
}
