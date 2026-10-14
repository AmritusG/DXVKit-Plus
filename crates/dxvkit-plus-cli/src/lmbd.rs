//! `dxvkit-plus encode-lmbd` (any CPU-decodable source to an LMBD file) and `dxvkit-plus compare`
//! (PSNR / SSIM of one decoded frame against a reference).

use crate::{Args, json_line, probe_open};
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::lmbd::{Compressor, Header, LmbdFile};
use dxvkit_plus_core::source::{MediaInfo, SourceKind};
use dxvkit_plus_lmbd_enc::{EncodeParams, encode_frames};
use serde_json::json;
use std::path::Path;
use std::time::Instant;

/// `N` or `N/D`.
fn parse_fps(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--fps: expected N or N/D, got {s}");
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d): (u32, u32) = (n.trim().parse().map_err(|_| bad())?, d.trim().parse().map_err(|_| bad())?);
    if n == 0 || d == 0 {
        return Err(bad());
    }
    Ok((n, d))
}

pub fn encode(args: &Args) -> Result<(), String> {
    match args.value("profile").unwrap_or("lmb7") {
        "lmb7" => {}
        "lmbx" => return encode_lmbx(args),
        other => return Err(format!("--profile: lmb7 or lmbx, got {other}")),
    }
    let [input, out] = args.positional.as_slice() else { return Err(crate::USAGE.into()) };
    let (info, mut src) = probe_open(input)?;
    let start = args.num("start", 0)?;
    let available = info.frames.saturating_sub(start);
    let count = args.num("frames", available)?.min(available);
    if count == 0 {
        return Err(format!("{input}: no frames to encode from frame {start}"));
    }
    let (fps_num, fps_den) = match args.value("fps") {
        Some(s) => parse_fps(s)?,
        None => (u32::try_from(info.rate.num).unwrap_or(30), u32::try_from(info.rate.den).unwrap_or(1)),
    };
    let mut p = EncodeParams::default();
    if let Some(v) = args.value("lambda") {
        p.rdo.lambda = v.parse().map_err(|_| format!("--lambda: not a number: {v}"))?;
    }
    p.rdo.lookback = args.num("lookback", p.rdo.lookback)?;
    p.rdo.partitions = args.num("rdo-partitions", 1)?;
    p.chunks = u16::try_from(args.num("chunks", 1)?).map_err(|_| "--chunks: at most 65535".to_string())?;
    p.zstd_level = i32::try_from(args.num("level", 9)?).map_err(|_| "--level out of range".to_string())?;
    p.compressor = match args.value("compressor").unwrap_or("zstd") {
        "zstd" => Compressor::Zstd,
        "none" => Compressor::None,
        other => return Err(format!("--compressor: zstd or none, got {other} (lz4 is reserved)")),
    };
    let threads = args.num("threads", std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))?.max(1);
    let header = Header::new(info.width, info.height, fps_num, fps_den);
    let file = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
    let t = Instant::now();
    let mut total_bytes = 0usize;
    let quiet = args.has("quiet");
    let h = encode_frames(
        std::io::BufWriter::new(file),
        header,
        &p,
        threads as usize,
        |i| {
            if i >= count {
                return None;
            }
            let f = start.saturating_add(i);
            Some(src.decode(f).and_then(|d| src.cpu_rgba(&d)))
        },
        |i, bytes| {
            total_bytes = total_bytes.saturating_add(bytes);
            if !quiet {
                eprintln!("encode-lmbd: frame {}/{count} {bytes} B ({:.1} s)", i.saturating_add(1), t.elapsed().as_secs_f64());
            }
        },
    )
    .map_err(|e| e.to_string())?;
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    json_line(&json!({
        "encoded": input, "lmbd": out, "frames": h.frame_count, "width": h.width, "height": h.height,
        "fps": format!("{}/{}", h.fps_num, h.fps_den), "lambda": p.rdo.lambda, "lookback": p.rdo.lookback,
        "rdo_partitions": p.rdo.partitions, "chunks": p.chunks, "compressor": p.compressor, "zstd_level": p.zstd_level,
        "payload_bytes": total_bytes, "file_bytes": size,
        "bytes_per_frame": total_bytes.checked_div(h.frame_count as usize).unwrap_or(0), "seconds": (t.elapsed().as_secs_f64() * 100.0).round() / 100.0,
    }));
    Ok(())
}

/// `encode-lmbd --profile lmbx`: every frame through the LMBX encoder, one stored chunk per frame
/// (the bitstream is entropy-coded already). `--chroma 444|422halo` (default 444, by measurement),
/// `--quality Q` (base quantiser, default 16), or `--frame-bytes N` (rate control: the best
/// quality that fits N bytes per frame).
fn encode_lmbx(args: &Args) -> Result<(), String> {
    use dxvkit_plus_core::lmbd::Writer;
    use dxvkit_plus_core::lmbx::Chroma;
    use dxvkit_plus_lmbd_enc::lmbx::{LmbxParams, encode_frame, encode_frame_to_size};
    let [input, out] = args.positional.as_slice() else { return Err(crate::USAGE.into()) };
    let (info, mut src) = probe_open(input)?;
    let start = args.num("start", 0)?;
    let available = info.frames.saturating_sub(start);
    let count = args.num("frames", available)?.min(available);
    if count == 0 {
        return Err(format!("{input}: no frames to encode from frame {start}"));
    }
    let (fps_num, fps_den) = match args.value("fps") {
        Some(s) => parse_fps(s)?,
        None => (u32::try_from(info.rate.num).unwrap_or(30), u32::try_from(info.rate.den).unwrap_or(1)),
    };
    let chroma = match args.value("chroma").unwrap_or("444") {
        "422halo" | "422" => Chroma::Halo422,
        "444" => Chroma::Full444,
        other => return Err(format!("--chroma: 422halo or 444, got {other}")),
    };
    let quality: f64 = args.value("quality").map(|v| v.parse().map_err(|_| format!("--quality: not a number: {v}"))).transpose()?.unwrap_or(16.0);
    let budget = args.value("frame-bytes").map(|v| v.parse::<usize>().map_err(|_| format!("--frame-bytes: not a number: {v}"))).transpose()?;
    let threads = args.num("threads", std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4))?.max(1) as usize;
    // Tile modes: `dct` alone writes bitstream v1.
    let modes: Vec<&str> = args.value("modes").unwrap_or("flat,palette,lossy").split(',').collect();
    if let Some(bad) = modes.iter().find(|m| !["dct", "flat", "palette", "lossy"].contains(m)) {
        return Err(format!("--modes: dct, flat, palette, lossy; got {bad}"));
    }
    let zstd = match args.value("compressor").unwrap_or("zstd") {
        "none" => None,
        "zstd" => Some(i32::try_from(args.num("level", 19)?).map_err(|_| "--level out of range".to_string())?),
        other => return Err(format!("--compressor: none or zstd, got {other}")),
    };
    // The colour-accuracy guard, in 8-bit code values (default 1.0; `off` = no guard).
    let colour_tol = match args.value("colour-tol") {
        None => LmbxParams::default().colour_tol,
        Some("off") => None,
        Some(v) => Some(v.parse::<f64>().ok().filter(|t| t.is_finite() && *t > 0.0).ok_or_else(|| format!("--colour-tol: a positive number or off, got {v}"))?),
    };
    // W10 PROTOTYPE (writes LMBX bitstream v4, never frozen): adaptive transform sizes — `rule` with
    // 0.7.0's step rule, `rd` with rate-distortion steps (W10's arm), `rd8` the steps alone (8x8, v3).
    let (tsizes, rd_q, rd_q8, max_tsize) = match args.value("tsizes") {
        None | Some("off") => (false, false, false, 32),
        Some("rule") => (true, false, false, 32),
        Some("rd") => (true, true, true, 32),
        Some("rd8") => (true, false, true, 8),
        Some(other) => return Err(format!("--tsizes: rule, rd, rd8 or off, got {other}")),
    };
    let p = LmbxParams {
        chroma,
        quality,
        colour_tol,
        tsizes,
        rd_q,
        rd_q8,
        max_tsize,
        threads,
        zstd,
        flat: modes.contains(&"flat"),
        palette: modes.contains(&"palette"),
        palette_lossy: modes.contains(&"lossy"),
        ..LmbxParams::default()
    };
    let header = Header::new_lmbx(info.width, info.height, fps_num, fps_den);
    let file = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
    let mut w = Writer::new(std::io::BufWriter::new(file), header.clone()).map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (mut total, mut qualities) = (0usize, Vec::new());
    for i in 0..count {
        let f = start.saturating_add(i);
        let rgba = src.decode(f).and_then(|d| src.cpu_rgba(&d)).map_err(|e| format!("{input} frame {f}: {e}"))?;
        let (blob, q) = match budget {
            Some(b) => encode_frame_to_size(&rgba, info.width, info.height, &p, b, 600.0).map_err(|e| e.to_string())?,
            None => (encode_frame(&rgba, info.width, info.height, &p).map_err(|e| e.to_string())?, quality),
        };
        let payload = dxvkit_plus_lmbd_enc::lmbx::frame_payload(&header, &blob, p.zstd).map_err(|e| e.to_string())?;
        w.push_payload(&payload).map_err(|e| e.to_string())?;
        total = total.saturating_add(payload.len());
        qualities.push(q);
        if !args.has("quiet") {
            eprintln!("encode-lmbd lmbx: frame {}/{count} {} B quality {q} ({:.1} s)", i.saturating_add(1), payload.len(), t.elapsed().as_secs_f64());
        }
    }
    let (_, h) = w.finish().map_err(|e| e.to_string())?;
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    json_line(&json!({
        "encoded": input, "lmbd": out, "profile": "lmbx", "chroma": chroma, "frames": h.frame_count, "width": h.width, "height": h.height,
        "fps": format!("{}/{}", h.fps_num, h.fps_den), "quality": if budget.is_some() { json!(qualities) } else { json!(quality) }, "frame_bytes_budget": budget,
        "payload_bytes": total, "file_bytes": size, "bytes_per_frame": total.checked_div(count as usize).unwrap_or(0),
        "seconds": (t.elapsed().as_secs_f64() * 100.0).round() / 100.0,
    }));
    Ok(())
}

/// Stored bytes of frame `n` (LMBD payload, or the movie sample), when the container says.
fn frame_bytes(info: &MediaInfo, n: u32) -> Option<u64> {
    match info.kind {
        SourceKind::Lmbd => LmbdFile::open(Path::new(&info.path)).ok()?.index().get(n as usize).map(|e| u64::from(e.size)),
        SourceKind::Hap | SourceKind::Native | SourceKind::Dxv => {
            let movie = dxvkit_plus_core::container::Movie::open(Path::new(&info.path)).ok()?;
            let track = movie.track(dxvkit_plus_core::container::Codec::Hap).or_else(|| movie.first())?;
            track.samples.get(n as usize).map(|s| u64::from(s.1))
        }
        SourceKind::ImageSequence | SourceKind::Still => None,
    }
}

/// PSNR per channel and of the RGB mean squared error, and SSIM on Rec. 601 luma over every 8x8
/// window (stride 1, sample variance) — the measure the original LMBD reports used, so numbers
/// compare directly.
pub struct Metrics {
    pub psnr: [f64; 4],
    pub psnr_rgb: f64,
    /// PSNR of the RGB as shown: each image's colour premultiplied by its own alpha
    /// (`c * a / 255`), so colour under transparent pixels does not count.
    pub psnr_rgb_premultiplied: f64,
    pub ssim: f64,
    pub windows: u64,
}

pub fn metrics(a: &[u8], b: &[u8], w: usize, h: usize) -> Metrics {
    let mut se = [0f64; 4];
    let mut se_pm = 0f64;
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for (c, s) in se.iter_mut().enumerate() {
            let d = f64::from(p.get(c).copied().unwrap_or(0)) - f64::from(q.get(c).copied().unwrap_or(0));
            *s += d * d;
        }
        let (pa, qa) = (f64::from(p.get(3).copied().unwrap_or(255)) / 255.0, f64::from(q.get(3).copied().unwrap_or(255)) / 255.0);
        for c in 0..3 {
            let d = f64::from(p.get(c).copied().unwrap_or(0)) * pa - f64::from(q.get(c).copied().unwrap_or(0)) * qa;
            se_pm += d * d;
        }
    }
    let n = (w.saturating_mul(h)).max(1) as f64;
    let psnr_of = |mse: f64| if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() };
    let mse = se.map(|s| s / n);
    let psnr = mse.map(psnr_of);
    let psnr_rgb = psnr_of((mse[0] + mse[1] + mse[2]) / 3.0);
    let psnr_rgb_premultiplied = psnr_of(se_pm / (3.0 * n));
    let luma = |px: &[u8]| -> Vec<f64> {
        px.chunks_exact(4)
            .map(|p| {
                0.299 * f64::from(p.first().copied().unwrap_or(0))
                    + 0.587 * f64::from(p.get(1).copied().unwrap_or(0))
                    + 0.114 * f64::from(p.get(2).copied().unwrap_or(0))
            })
            .collect()
    };
    let (y1, y2) = (luma(a), luma(b));
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut sum, mut windows) = (0f64, 0u64);
    if w >= 8 && h >= 8 {
        let at = |v: &[f64], x: usize, y: usize| v.get(y.saturating_mul(w).saturating_add(x)).copied().unwrap_or(0.0);
        for y in 0..=h.saturating_sub(8) {
            for x in 0..=w.saturating_sub(8) {
                let (mut m1, mut m2) = (0f64, 0f64);
                for j in y..y.saturating_add(8) {
                    for i in x..x.saturating_add(8) {
                        m1 += at(&y1, i, j);
                        m2 += at(&y2, i, j);
                    }
                }
                m1 /= 64.0;
                m2 /= 64.0;
                let (mut v1, mut v2, mut cov) = (0f64, 0f64, 0f64);
                for j in y..y.saturating_add(8) {
                    for i in x..x.saturating_add(8) {
                        let (p, q) = (at(&y1, i, j) - m1, at(&y2, i, j) - m2);
                        v1 += p * p;
                        v2 += q * q;
                        cov += p * q;
                    }
                }
                v1 /= 63.0;
                v2 /= 63.0;
                cov /= 63.0;
                sum += ((2.0 * m1 * m2 + c1) * (2.0 * cov + c2)) / ((m1 * m1 + m2 * m2 + c1) * (v1 + v2 + c2));
                windows = windows.saturating_add(1);
            }
        }
    }
    Metrics { psnr, psnr_rgb, psnr_rgb_premultiplied, ssim: if windows > 0 { sum / windows as f64 } else { 1.0 }, windows }
}

fn num(v: f64) -> serde_json::Value {
    if v.is_finite() { json!((v * 10000.0).round() / 10000.0) } else { json!("inf") }
}

pub fn compare(args: &Args) -> Result<(), String> {
    let [reference, file] = args.positional.as_slice() else { return Err(crate::USAGE.into()) };
    let frame = args.num("frame", 0)?;
    let ref_frame = args.num("ref-frame", frame)?;
    let decode = |path: &str, n: u32| -> Result<(MediaInfo, Vec<u8>, (u32, u32)), String> {
        let (info, mut src) = probe_open(path)?;
        let d = src.decode(n).map_err(|e| format!("{path} frame {n}: {e}"))?;
        let size = d.size();
        let rgba = src.cpu_rgba(&d).map_err(|e: MediaError| format!("{path} frame {n}: {e}"))?;
        Ok((info, rgba, size))
    };
    let (_, a, (rw, rh)) = decode(reference, ref_frame)?;
    let (info, mut b, (w, h)) = decode(file, frame)?;
    // `--float`: an LMBX frame decoded with the floating-point inverse transform instead (the
    // prototypes' decoder; not conforming) — what the integer transform costs (LMBX-D1).
    if args.has("float") {
        if info.codec != "LMBX" {
            return Err("--float: only for LMBX files".into());
        }
        b = float_rgba(file, frame, w, h)?;
    }
    if (rw, rh) != (w, h) {
        return Err(format!("size mismatch: reference {rw}x{rh}, {file} {w}x{h}"));
    }
    let m = metrics(&a, &b, w as usize, h as usize);
    json_line(&json!({
        "reference": reference, "ref_frame": ref_frame, "file": file, "frame": frame, "codec": info.codec, "float_reference": args.has("float"),
        "width": w, "height": h, "frame_bytes": frame_bytes(&info, frame),
        "psnr_rgb": num(m.psnr_rgb), "psnr_rgb_premultiplied": num(m.psnr_rgb_premultiplied), "psnr": {"r": num(m.psnr[0]), "g": num(m.psnr[1]), "b": num(m.psnr[2]), "a": num(m.psnr[3])},
        "ssim": (m.ssim * 1_000_000.0).round() / 1_000_000.0, "ssim_windows": m.windows,
    }));
    Ok(())
}

/// An LMBX frame decoded with the floating-point inverse transform (not conforming; see
/// `float_reference_rgba8`), 8-bit RGBA.
pub(crate) fn float_rgba(file: &str, frame: u32, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let f = LmbdFile::open(Path::new(file)).map_err(|e| e.to_string())?;
    let (mut scratch, mut blob) = (Vec::new(), Vec::new());
    f.decode_frame(frame, &mut scratch, &mut blob, true).map_err(|e| e.to_string())?;
    dxvkit_plus_lmbd_enc::lmbx::float_reference_rgba8(&blob, w, h).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
    use super::*;

    #[test]
    fn metrics_identical_and_known_error() {
        let a: Vec<u8> = (0..16 * 16 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let m = metrics(&a, &a, 16, 16);
        assert!(m.psnr_rgb.is_infinite() && (m.ssim - 1.0).abs() < 1e-12);
        // Negative control: +1 on every R value gives MSE_R = 1, so PSNR_R = 10 log10(65025).
        let b: Vec<u8> = a.iter().enumerate().map(|(i, &v)| if i % 4 == 0 { v.saturating_add(1) } else { v }).collect();
        let m = metrics(&a, &b, 16, 16);
        let want = 10.0 * 65025f64.log10();
        assert!((m.psnr[0] - want).abs() < 0.05, "{}", m.psnr[0]);
        assert!(m.ssim < 1.0);
    }

    #[test]
    fn fps_parsing() {
        assert_eq!(parse_fps("30").unwrap(), (30, 1));
        assert_eq!(parse_fps("30000/1001").unwrap(), (30000, 1001));
        assert!(parse_fps("0").is_err() && parse_fps("x/2").is_err());
    }
}
