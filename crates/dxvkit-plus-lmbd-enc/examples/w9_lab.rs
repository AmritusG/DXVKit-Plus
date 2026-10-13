//! W9 exploration lab (prototype, not shipped): where LMBX v3 loses to LMB7 per tile, and the
//! ceiling of a per-tile BC7-or-v3 choice at LMB7's bytes.
//!
//! ```text
//! w9_lab diag <name> <ref.png> <budget>      # per-tile v3 vs BC7, tile classes, oracle
//! w9_lab bestcrop <in.png> <out.png> <w> <h> # the crop with the most AC energy
//! w9_lab bc7time <ref.png>                   # BC7 encode time per configuration
//! ```
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::needless_range_loop
)]

use dxvkit_plus_core::lmbx::{Frame, TILE};
use dxvkit_plus_core::source;
use dxvkit_plus_lmbd_enc::lmbx::{LmbxParams, encode_frame, stored_size};
use dxvkit_plus_lmbd_enc::{EncodeParams, encode_bc7};

type Px = [u8; 4];

fn load(p: &str) -> (u32, u32, Vec<u8>) {
    let (w, h, r) = source::load_image(std::path::Path::new(p)).unwrap();
    (w, h, r)
}

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let f = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(rgba).unwrap();
    wr.finish().unwrap();
}

/// Shown (premultiplied RGB) squared error of a pixel pair, summed over RGB.
fn shown_se(p: &[u8], q: &[u8]) -> f64 {
    let (pa, qa) = (f64::from(p[3]) / 255.0, f64::from(q[3]) / 255.0);
    (0..3).map(|k| (f64::from(p[k]) * pa - f64::from(q[k]) * qa).powi(2)).sum()
}

fn psnr(se: f64, n: f64) -> f64 {
    if se == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 * n / se).log10() }
}

/// Per-tile shown SE of `d` against `r`.
fn tile_se(r: &[u8], d: &[u8], w: u32, h: u32) -> Vec<f64> {
    let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
    let mut out = vec![0f64; (tx * ty) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            out[((y / TILE) * tx + x / TILE) as usize] += shown_se(&r[i..i + 4], &d[i..i + 4]);
        }
    }
    out
}

/// The BC7 blocks of tile `t` (8x8 blocks, raster), from a frame's block stream.
fn tile_blocks(bc: &[u8], w: u32, t: u32, tx: u32) -> Vec<u8> {
    let bw = w.div_ceil(4);
    let (x0, y0) = ((t % tx) * 8, (t / tx) * 8);
    let bh = bc.len() as u32 / 16 / bw;
    let mut out = Vec::with_capacity(1024);
    for by in y0..y0 + 8 {
        for bx in x0..x0 + 8 {
            if bx < bw && by < bh {
                let at = ((by * bw + bx) * 16) as usize;
                out.extend_from_slice(&bc[at..at + 16]);
            }
        }
    }
    out
}

fn bc7_decode(bc: &[u8], w: u32, h: u32) -> Vec<u8> {
    let bw = w.div_ceil(4);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for (b, blk) in bc.chunks_exact(16).enumerate() {
        let (bx, by) = (b as u32 % bw, b as u32 / bw);
        let px = dxvkit_plus_core::lmbx_bc7_block(blk);
        for (k, c) in px.iter().enumerate() {
            let (x, y) = (bx * 4 + k as u32 % 4, by * 4 + k as u32 / 4);
            if x < w && y < h {
                let i = ((y * w + x) * 4) as usize;
                out[i..i + 4].copy_from_slice(c);
            }
        }
    }
    out
}

/// A v3 frame at quality `q`: decoded RGBA, per-tile raw bytes (its streams + qmap + mode
/// byte) scaled to the stored size, stored size.
fn v3_at(r: &[u8], w: u32, h: u32, q: f64) -> (Vec<u8>, Vec<f64>, usize) {
    let p = LmbxParams { quality: q, zstd: Some(19), threads: 12, ..LmbxParams::default() };
    let b = encode_frame(r, w, h, &p).unwrap();
    let f = Frame::parse(&b, w, h).unwrap();
    let np = f.header.planes() as usize;
    let nt = f.header.tile_count() as usize;
    let raw: Vec<f64> = (0..nt).map(|t| (0..np).map(|k| f.stream(t * np + k).len() as f64).sum::<f64>() + 9.0).collect();
    let stored = stored_size(&b, Some(19));
    let scale = stored as f64 / raw.iter().sum::<f64>();
    let d = dxvkit_plus_core::lmbx::decode_rgba8(&b, w, h, true).unwrap();
    (d, raw.iter().map(|v| v * scale).collect(), stored)
}

/// Fit v3 to a stored-byte budget (log bisection, as W7's lab).
fn v3_fit(r: &[u8], w: u32, h: u32, budget: usize) -> f64 {
    let size = |q: f64| stored_size(&encode_frame(r, w, h, &LmbxParams { quality: q, zstd: Some(19), threads: 12, ..LmbxParams::default() }).unwrap(), Some(19));
    if size(1.0) <= budget {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f64, 600f64.ln());
    while hi - lo > 1.0 / 32.0 {
        let mid = (lo + hi) / 2.0;
        if size(mid.exp()) <= budget { hi = mid } else { lo = mid }
    }
    hi.exp()
}

fn bc7_with(r: &[u8], w: u32, h: u32, lambda: f32, uber: u32) -> (Vec<u8>, f64) {
    let mut p = EncodeParams::default();
    p.rdo.lambda = lambda;
    p.bc7.uber_level = uber;
    let t = std::time::Instant::now();
    let bc = encode_bc7(r, w, h, &p, 12).unwrap();
    (bc, t.elapsed().as_secs_f64())
}

fn diag(name: &str, rp: &str, budget: usize) {
    let (w, h, r) = load(rp);
    let n = f64::from(w * h);
    let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
    let nt = (tx * ty) as usize;
    // v3 at the budget, and around it (for the oracle).
    let q0 = v3_fit(&r, w, h, budget);
    let mut v3opts: Vec<(f64, Vec<f64>, Vec<f64>, usize)> = Vec::new();
    for m in [0.35, 0.5, 0.7, 1.0, 1.4, 2.0, 2.8] {
        let q = (q0 * m).clamp(1.0, 600.0);
        let (d, bytes, stored) = v3_at(&r, w, h, q);
        v3opts.push((q, tile_se(&r, &d, w, h), bytes, stored));
    }
    let base = &v3opts[3];
    println!("## {name}: {w}x{h}, budget {budget} B, v3 fit q {q0:.2} stored {} B shown {:.3} dB", base.3, psnr(base.1.iter().sum(), 3.0 * n));
    // BC7: non-RDO (lambda 0) fast (uber 0) and full (uber 4), and RDO lambda 1 (LMB7's default).
    let mut bcopts: Vec<(String, Vec<f64>, Vec<f64>)> = Vec::new();
    for (lam, uber) in [(0.0f32, 0u32), (0.0, 4), (1.0, 4)] {
        let (bc, secs) = bc7_with(&r, w, h, lam, uber);
        let d = bc7_decode(&bc, w, h);
        let se = tile_se(&r, &d, w, h);
        let frame_z = zstd::bulk::compress(&bc, 19).unwrap().len();
        // Per-tile cost: each tile's zstd size, scaled so they add up to the frame's.
        let raw: Vec<f64> = (0..nt as u32).map(|t| zstd::bulk::compress(&tile_blocks(&bc, w, t, tx), 19).unwrap().len() as f64).collect();
        let scale = frame_z as f64 / raw.iter().sum::<f64>();
        let label = format!("bc7 l{lam} u{uber}");
        println!("   {label:12} encode {secs:7.2} s  frame zstd {frame_z:8} B  shown {:.3} dB", psnr(se.iter().sum(), 3.0 * n));
        bcopts.push((label, se, raw.iter().map(|v| v * scale).collect()));
    }
    // Per-tile comparison against BC7 lambda 1 (LMB7's own blocks).
    let lmb7 = &bcopts[2];
    let mut classes: std::collections::BTreeMap<&str, (usize, f64, f64, f64, f64)> = std::collections::BTreeMap::new();
    for t in 0..nt {
        let (x0, y0) = ((t as u32 % tx) * TILE, (t as u32 / tx) * TILE);
        // Tile statistics: distinct colours, luma variance, mean abs horizontal gradient.
        let (mut cols, mut sum, mut sum2, mut grad, mut cnt) = (std::collections::HashSet::new(), 0f64, 0f64, 0f64, 0f64);
        for y in y0..(y0 + TILE).min(h) {
            for x in x0..(x0 + TILE).min(w) {
                let i = ((y * w + x) * 4) as usize;
                let p: Px = [r[i], r[i + 1], r[i + 2], r[i + 3]];
                cols.insert(p);
                let l = 0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]);
                sum += l;
                sum2 += l * l;
                cnt += 1.0;
                if x + 1 < w {
                    let j = i + 4;
                    let l2 = 0.299 * f64::from(r[j]) + 0.587 * f64::from(r[j + 1]) + 0.114 * f64::from(r[j + 2]);
                    grad += (l2 - l).abs();
                }
            }
        }
        let var = sum2 / cnt - (sum / cnt).powi(2);
        let cls = if cols.len() <= 64 {
            "few colours (<=64)"
        } else if var < 50.0 {
            "smooth (var < 50)"
        } else if grad / cnt > 20.0 {
            "noisy (grad > 20)"
        } else {
            "textured"
        };
        let e = classes.entry(cls).or_default();
        e.0 += 1;
        e.1 += base.1[t];
        e.2 += lmb7.1[t];
        e.3 += base.2[t];
        e.4 += lmb7.2[t];
    }
    println!("   tile class                 tiles   v3 SE share  LMB7 SE share   v3 bytes  BC7(l1) bytes");
    let (tv, tl) = (base.1.iter().sum::<f64>(), lmb7.1.iter().sum::<f64>());
    for (k, (c, sv, sl, bv, bl)) in &classes {
        println!("   {k:24} {c:7} {:12.1}% {:13.1}% {bv:10.0} {bl:13.0}", 100.0 * sv / tv, 100.0 * sl / tl);
    }
    // Oracle: per tile the option minimising SE + mu * bytes, mu swept so the bytes fit the budget
    // (scaled per-tile bytes; v3-only first, then v3 or BC7 — the gain is their difference).
    let solve = |with_bc7: bool| -> (f64, f64, usize) {
        let opts = |t: usize| -> Vec<(f64, f64, bool)> {
            let mut o: Vec<(f64, f64, bool)> = v3opts.iter().map(|v| (v.1[t], v.2[t], false)).collect();
            if with_bc7 {
                o.extend(bcopts.iter().map(|b| (b.1[t], b.2[t], true)));
            }
            o
        };
        let pick = |mu: f64| -> (f64, f64, usize) {
            let (mut se, mut by, mut nb) = (0f64, 0f64, 0usize);
            for t in 0..nt {
                let best = opts(t).into_iter().min_by(|a, b| (a.0 + mu * a.1).total_cmp(&(b.0 + mu * b.1))).unwrap();
                se += best.0;
                by += best.1;
                nb += usize::from(best.2);
            }
            (se, by, nb)
        };
        let (mut lo, mut hi) = (0.0f64, 1e9f64);
        for _ in 0..200 {
            let mid = (lo + hi) / 2.0;
            if pick(mid).1 <= budget as f64 { hi = mid } else { lo = mid }
        }
        pick(hi)
    };
    let (sa, ba, _) = solve(false);
    let (sb, bb, nb) = solve(true);
    println!(
        "   oracle at the budget: v3-only {:.3} dB ({ba:.0} B) | v3 or BC7 {:.3} dB ({bb:.0} B, {nb} of {nt} tiles BC7) | ceiling gain {:+.2} dB",
        psnr(sa, 3.0 * n),
        psnr(sb, 3.0 * n),
        psnr(sb, 3.0 * n) - psnr(sa, 3.0 * n)
    );
}

fn bestcrop(inp: &str, out: &str, cw: u32, ch: u32) {
    let (w, h, r) = load(inp);
    let l = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        0.299 * f64::from(r[i]) + 0.587 * f64::from(r[i + 1]) + 0.114 * f64::from(r[i + 2])
    };
    // Gradient energy per 64x64 cell, then the best cw x ch window on the cell grid.
    let (gx, gy) = (w / 64, h / 64);
    let mut cell = vec![0f64; (gx * gy) as usize];
    for y in 0..gy * 64 {
        for x in 0..gx * 64 - 1 {
            cell[((y / 64) * gx + x / 64) as usize] += (l(x + 1, y) - l(x, y)).abs();
        }
    }
    let (cx, cy) = (cw / 64, ch / 64);
    let mut best = (0f64, 0u32, 0u32);
    for oy in 0..=gy - cy {
        for ox in 0..=gx - cx {
            let e: f64 = (oy..oy + cy).flat_map(|y| (ox..ox + cx).map(move |x| (x, y))).map(|(x, y)| cell[(y * gx + x) as usize]).sum();
            if e > best.0 {
                best = (e, ox * 64, oy * 64);
            }
        }
    }
    let px: Vec<u8> = (0..ch)
        .flat_map(|y| {
            let s = (((best.2 + y) * w + best.1) * 4) as usize;
            r[s..s + (cw * 4) as usize].to_vec()
        })
        .collect();
    write_png(out, cw, ch, &px);
    println!("{out}: crop at {},{} (energy {:.0})", best.1, best.2, best.0);
}

fn bc7time(rp: &str) {
    let (w, h, r) = load(rp);
    for (lam, uber) in [(0.0f32, 0u32), (0.0, 2), (0.0, 4), (1.0, 4)] {
        let (bc, secs) = bc7_with(&r, w, h, lam, uber);
        println!("{rp}: bc7 lambda {lam} uber {uber}: {secs:.2} s, zstd19 {} B", zstd::bulk::compress(&bc, 19).unwrap().len());
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "diag" => diag(&a[2], &a[3], a[4].parse().unwrap()),
        "bestcrop" => bestcrop(&a[2], &a[3], a[4].parse().unwrap(), a[5].parse().unwrap()),
        "bc7time" => bc7time(&a[2]),
        other => panic!("unknown command {other}"),
    }
}
