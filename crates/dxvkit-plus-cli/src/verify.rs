//! `dxvkit-plus verify`: per file, every frame through the CPU and the GPU paths —
//!
//! - CPU vs GPU: block codecs within 1/255 (the BC conventions allow a GPU to differ by 1), BGRA
//!   native frames exactly (Apple's own conversion vs our zero-copy import);
//! - `--tiles`: frame identity (the frame index the fixture pattern carries) on every frame, and,
//!   with `--source-tolerance N`, every frame against the source pattern within N/255;
//! - seek: the frames decoded in a shuffled order are identical to the sequential pass (frame-exact
//!   random access);
//! - native: zero-copy evidence (no bytes uploaded; the texture aliases the frame's IOSurface).
//!
//! One JSON line per file; exit status 1 if any file fails.

use crate::gpu::Session;
use crate::{Args, json_line, probe_open};
use dxvkit_plus_core::source::{Decoded, Shading, SourceKind};
use dxvkit_plus_core::testpattern::{read_frame_id, tiles};
use serde_json::json;

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3))
}

/// Max channel difference (RGB and A separately), ignoring colour where the reference alpha is 0.
fn max_diff(a: &[u8], b: &[u8]) -> (u8, u8) {
    let (mut c, mut al) = (0u8, 0u8);
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        if let ([pr, pg, pb, pa], [qr, qg, qb, qa]) = (p, q) {
            al = al.max(pa.abs_diff(*qa));
            if *qa != 0 {
                c = c.max(pr.abs_diff(*qr)).max(pg.abs_diff(*qg)).max(pb.abs_diff(*qb));
            }
        }
    }
    (c, al)
}

pub fn run(args: &Args) -> Result<(), String> {
    if args.positional.is_empty() {
        return Err(crate::USAGE.into());
    }
    let tiles_check = args.has("tiles");
    let src_tol = if args.has("source-tolerance") { Some(args.num("source-tolerance", 1)?) } else { None };
    let mut failed = 0usize;
    for path in &args.positional {
        let r = verify_one(path, tiles_check, src_tol);
        match r {
            Ok(v) => {
                if v.get("pass") != Some(&json!(true)) {
                    failed = failed.saturating_add(1);
                }
                json_line(&v);
            }
            Err(e) => {
                failed = failed.saturating_add(1);
                json_line(&json!({"file": path, "pass": false, "error": e}));
            }
        }
    }
    if failed > 0 { Err(format!("{failed} file(s) failed verification")) } else { Ok(()) }
}

fn verify_one(path: &str, tiles_check: bool, src_tol: Option<u32>) -> Result<serde_json::Value, String> {
    let (info, mut src) = probe_open(path)?;
    let mut gpu = Session::new().map_err(|e| e.to_string())?;
    let n = info.frames;
    let (mut worst_c, mut worst_a, mut cpu_checked) = (0u8, 0u8, 0u32);
    let (mut id_fail, mut src_worst) = (Vec::new(), 0u8);
    let mut hashes = Vec::with_capacity(n as usize);
    let (mut zero_copy_frames, mut uploaded) = (0u32, 0usize);
    let mut problems: Vec<String> = Vec::new();
    for f in 0..n {
        let d = src.decode(f).map_err(|e| format!("frame {f}: {e}"))?;
        if d.index() != f {
            problems.push(format!("asked for frame {f}, got {}", d.index()));
        }
        let (w, h) = d.size();
        let g = gpu.render_rgba(&d)?;
        hashes.push(fnv(&g));
        match &d {
            Decoded::Native(nf) => {
                if gpu.renderer.last_upload_bytes == 0 && gpu.last_surface == Some(nf.surface_id) {
                    zero_copy_frames = zero_copy_frames.saturating_add(1);
                }
            }
            Decoded::Planes { .. } => uploaded = uploaded.saturating_add(gpu.renderer.last_upload_bytes),
        }
        let hdr = matches!(&d, Decoded::Planes { shading: Shading::Hdr, .. });
        if !hdr && let Ok(c) = src.cpu_rgba(&d) {
            let (dc, da) = max_diff(&g, &c);
            worst_c = worst_c.max(dc);
            worst_a = worst_a.max(da);
            cpu_checked = cpu_checked.saturating_add(1);
        }
        if tiles_check {
            let alpha_only = matches!(&d, Decoded::Planes { shading: Shading::AlphaOnly, .. });
            if !alpha_only && read_frame_id(&g, w, h) != Some(f % 256) {
                id_fail.push(f);
            }
            if let Some(tol) = src_tol {
                let mut want = tiles(f, w, h);
                if alpha_only {
                    for p in want.chunks_exact_mut(4) {
                        if let [r, g, b, a] = p {
                            (*r, *g, *b, *a) = (*a, *a, *a, 255);
                        }
                    }
                } else if !info.has_alpha {
                    want.chunks_exact_mut(4).filter_map(|p| p.get_mut(3)).for_each(|a| *a = 255);
                }
                let (c, a) = max_diff(&g, &want);
                src_worst = src_worst.max(c).max(a);
                if u32::from(c.max(a)) > tol {
                    problems.push(format!("frame {f} differs from the source by {} (> {tol})", c.max(a)));
                }
            }
        }
    }
    // Seek: a fresh decoder, frames in a deterministic shuffled order.
    let (_, mut seeker) = probe_open(path)?;
    let mut order: Vec<u32> = (0..n).collect();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, state.checked_rem(i as u64).unwrap_or(0).min(i as u64) as usize);
    }
    let mut seek_mismatch = Vec::new();
    for &f in &order {
        let d = seeker.decode(f).map_err(|e| format!("seek to {f}: {e}"))?;
        let g = gpu.render_rgba(&d)?;
        if hashes.get(f as usize) != Some(&fnv(&g)) || d.index() != f {
            seek_mismatch.push(f);
        }
    }
    let cpu_tol = if info.kind == SourceKind::Native { 0 } else { 1 };
    if worst_c > cpu_tol || worst_a > cpu_tol {
        problems.push(format!("CPU vs GPU differ by {worst_c} (rgb) / {worst_a} (alpha), allowed {cpu_tol}"));
    }
    if !id_fail.is_empty() {
        problems.push(format!("frame identity failed on frames {id_fail:?}"));
    }
    if !seek_mismatch.is_empty() {
        problems.push(format!("seek mismatch on frames {seek_mismatch:?}"));
    }
    if info.kind == SourceKind::Native && zero_copy_frames != n {
        problems.push(format!("zero-copy on {zero_copy_frames} of {n} frames"));
    }
    Ok(json!({
        "file": path, "codec": info.codec, "kind": info.kind, "frames": n, "width": info.width, "height": info.height,
        "pass": problems.is_empty(), "problems": problems,
        "cpu_vs_gpu": {"frames": cpu_checked, "max_rgb": worst_c, "max_alpha": worst_a},
        "seek_frames_checked": order.len(), "frame_identity": tiles_check,
        "source_max_diff": if src_tol.is_some() { json!(src_worst) } else { json!(null) },
        "native_zero_copy_frames": if info.kind == SourceKind::Native { json!(zero_copy_frames) } else { json!(null) },
        "copies_per_native_frame": if info.kind == SourceKind::Native { json!(0) } else { json!(null) },
        "uploaded_bytes": uploaded,
    }))
}
