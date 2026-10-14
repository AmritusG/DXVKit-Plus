//! PROTOTYPE (W10 exploration, never frozen): adaptive transform sizes — LMBX bitstream v4's
//! transform map lets a DCT tile code one 32x32 block, or per 16x16 quadrant one 16x16 block or
//! four 8x8 (`dxvkit_plus_core::lmbx::{tile_blocks, idct_n}`). This module is the encoder side:
//!
//! - per tile, the rate-distortion choice of the 17 layouts: every candidate block (sixteen 8x8,
//!   four 16x16, one 32x32 per plane) quantised with RDOQ against the frame's current rate model,
//!   each layout's DC chain followed exactly, `J = D + lambda * bits` with v3's mode-decision
//!   weights and lambda;
//! - a 16x16 or 32x32 block's step: the quantiser index nearest `base * clamp(AC / 380, ...)` of its
//!   own AC energy per 8x8 area (the 8x8 rule, scaled), or with `region_q_min` the finest of the
//!   8x8 blocks' it covers;
//! - the colour guard (W6b) on blocks of every size, in stream order with every plane's DC chain;
//! - the tile's streams for its layout.
//!
//! 4:4:4 only (the prototype's transform map must be zero in 4:2:2 frames).
//
// Lint note: tile-local arithmetic on block indices below 1024; relaxed for this module only.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use crate::lmbx::{LmbxParams, Plane, QT, Source, Symbols};
use dxvkit_plus_core::lmbx::{EOB, LEVEL_OFFSET, PLANES, PlaneKind, TILE, TMAP_32, dc_step_n, dequant_max, tile_blocks, zigzag_n};

/// 8x8 blocks per tile edge.
const TB: u32 = TILE / 8;

/// Orthonormal float DCT bases and zigzags of 8, 16 and 32 points.
pub(crate) struct Kit {
    m: [Vec<f64>; 3],
    zz: [Vec<u16>; 3],
}

impl Kit {
    pub(crate) fn new() -> Kit {
        let basis = |n: usize| {
            let mut m = vec![0f64; n * n];
            for k in 0..n {
                let c = if k == 0 { (1.0 / n as f64).sqrt() } else { (2.0 / n as f64).sqrt() };
                for i in 0..n {
                    m[k * n + i] = c * (std::f64::consts::PI * (2 * i + 1) as f64 * k as f64 / (2 * n) as f64).cos();
                }
            }
            m
        };
        Kit { m: [basis(8), basis(16), basis(32)], zz: [zigzag_n(8), zigzag_n(16), zigzag_n(32)] }
    }
}

fn si(n: u32) -> usize {
    match n {
        8 => 0,
        16 => 1,
        _ => 2,
    }
}

/// Forward orthonormal DCT of an `n x n` block (raster), coefficients raster.
pub(crate) fn fdct_n(kit: &Kit, n: usize, x: &[f64]) -> Vec<f64> {
    let m = &kit.m[si(n as u32)];
    let mut t = vec![0f64; n * n];
    for k in 0..n {
        for j in 0..n {
            t[k * n + j] = (0..n).map(|i| m[k * n + i] * x[i * n + j]).sum();
        }
    }
    let mut c = vec![0f64; n * n];
    for k in 0..n {
        for l in 0..n {
            c[k * n + l] = (0..n).map(|j| t[k * n + j] * m[l * n + j]).sum();
        }
    }
    c
}

/// Float inverse of [`fdct_n`] (the float reference decoder's).
pub(crate) fn idct_f_n(kit: &Kit, n: usize, d: &[f64]) -> Vec<f64> {
    let m = &kit.m[si(n as u32)];
    let mut t = vec![0f64; n * n];
    for i in 0..n {
        for l in 0..n {
            t[i * n + l] = (0..n).map(|k| m[k * n + i] * d[k * n + l]).sum();
        }
    }
    let mut x = vec![0f64; n * n];
    for i in 0..n {
        for j in 0..n {
            x[i * n + j] = (0..n).map(|l| t[i * n + l] * m[l * n + j]).sum();
        }
    }
    x
}

/// An `n x n` block of a plane in code values (`(v - shift) / 4`).
fn block_n(pl: &Plane, x0: u32, y0: u32, n: u32, shift: i32) -> Vec<f64> {
    (0..n * n).map(|i| f64::from(pl.v[((y0 + i / n) * pl.w + x0 + i % n) as usize] - shift) / 4.0).collect()
}

/// RDOQ of one block's raster coefficients into zigzag levels: `crate::lmbx`'s 8x8 rule
/// (protected low terms rounded, deadzone, the four-candidate rate-distortion choice, the
/// end-of-block trellis) over `n * n` coefficients.
fn quant_n(c: &[f64], zz: &[u16], q: u32, rate: &[f64], lambda: f64, p: &LmbxParams, rdo: bool) -> Vec<i32> {
    let q = f64::from(q);
    let nn = zz.len();
    let clamp = |l: i64| l.clamp(-i64::from(LEVEL_OFFSET), i64::from(LEVEL_OFFSET)) as i32;
    let cz: Vec<f64> = zz.iter().map(|&z| c[z as usize]).collect();
    let mut lev = vec![0i32; nn];
    for k in 0..nn {
        let (v, a) = (cz[k], cz[k].abs());
        let sgn = if v < 0.0 { -1i64 } else { 1 };
        if k < p.protect {
            lev[k] = clamp(sgn * (a / q).round() as i64);
            continue;
        }
        let aq = a / q;
        let n_dz = ((aq + p.deadzone).floor() as i64).max(0);
        if !rdo {
            lev[k] = clamp(sgn * n_dz);
            continue;
        }
        let c0 = (aq.floor() as i64).max(0);
        let mut best = (f64::INFINITY, 0i64);
        for l in [0, n_dz, c0, c0 + 1] {
            let l = l.min(i64::from(LEVEL_OFFSET));
            let d = (a - l as f64 * q).powi(2);
            let sym = if l == 0 { LEVEL_OFFSET as usize } else { (clamp(sgn * l) + LEVEL_OFFSET) as usize };
            let cost = d + lambda * rate[sym];
            if cost < best.0 {
                best = (cost, l);
            }
        }
        lev[k] = clamp(sgn * best.1);
    }
    if rdo {
        let mut last = (0..nn).rev().find(|&k| lev[k] != 0);
        while let Some(l) = last.filter(|&l| l >= p.protect) {
            let prev = (0..l).rev().find(|&k| lev[k] != 0);
            let a = cz[l].abs();
            let dk = (a - f64::from(lev[l].abs()) * q).powi(2);
            let added = cz[l] * cz[l] - dk;
            let mut saved = rate[(lev[l] + LEVEL_OFFSET) as usize];
            for _ in prev.map_or(0, |p| p + 1)..l {
                saved += rate[LEVEL_OFFSET as usize];
            }
            if lambda * saved > added {
                lev[l] = 0;
                last = prev;
            } else {
                break;
            }
        }
    }
    lev
}

/// A quantised candidate block: its raster DC coefficient, the AC squared error, the AC's rate up
/// to its last non-zero level, and that level's zigzag position (0: no AC).
struct Cand {
    dc: f64,
    ac_se: f64,
    ac_bits: f64,
    last_ac: usize,
}

/// The planes of a tile in stream order: source plane, sample offset, chroma, decision weight
/// (v3's: 3 eY^2 + eCo^2 / 2 + 3 eCg^2 / 4, A 1).
fn planes(s: &Source) -> Vec<(&Plane, i32, bool, f64)> {
    PLANES
        .into_iter()
        .take(s.header.planes() as usize)
        .map(|k| match k {
            PlaneKind::Y => (&s.y, 512, false, 3.0),
            PlaneKind::Co => (&s.co, 0, true, 0.5),
            PlaneKind::Cg => (&s.cg, 0, true, 0.75),
            PlaneKind::A => (s.a.as_ref().unwrap_or(&s.y), 512, false, 1.0),
        })
        .collect()
}

/// Whether tile `t` has at most 64 distinct colours (an exact v3 palette can code it).
pub(crate) fn few_colours(s: &Source, t: u32) -> bool {
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    let mut seen: Vec<[i32; 4]> = Vec::with_capacity(65);
    for y in 0..TILE {
        for x in 0..TILE {
            let i = ((ty * TILE + y) * s.y.w + tx * TILE + x) as usize;
            let c = [s.y.v[i], s.co.v[i], s.cg.v[i], s.a.as_ref().map_or(0, |a| a.v[i])];
            if !seen.contains(&c) {
                if seen.len() == dxvkit_plus_core::lmbx::PAL3_MAX {
                    return false;
                }
                seen.push(c);
            }
        }
    }
    true
}

/// The quantiser index of an `n x n` region of tile `t` at 8x8 position (`bx`, `by`): by the 8x8
/// rule (`pick_qidx`) on its AC energy per 8x8 area, or (`region_q_min`) the finest of its 8x8
/// blocks' indices.
fn region_qidx(s: &Source, kit: &Kit, t: u32, bx: u32, by: u32, n: u32, qi8: &[usize], p: &LmbxParams) -> usize {
    if n == 8 {
        return qi8[(by * TB + bx) as usize];
    }
    if p.region_q_min {
        return (by..by + n / 8).flat_map(|y| (bx..bx + n / 8).map(move |x| qi8[(y * TB + x) as usize])).min().unwrap_or(0);
    }
    if p.flat_q {
        return crate::lmbx::nearest_qidx(p.quality);
    }
    let (tx, ty) = (t % s.header.tiles_x(), t / s.header.tiles_x());
    let c = fdct_n(kit, n as usize, &block_n(&s.y, tx * TILE + bx * 8, ty * TILE + by * 8, n, 512));
    let ac = (c.iter().skip(1).map(|v| v * v).sum::<f64>() / f64::from((n / 8) * (n / 8))).sqrt();
    let qy = (p.quality * (ac / 380.0).clamp(0.16, 2.6)).round().clamp(1.0, 96.0);
    QT.iter().enumerate().min_by(|a, b| (f64::from(*a.1) - qy).abs().total_cmp(&(f64::from(*b.1) - qy).abs())).map_or(0, |(i, _)| i)
}

/// A tile's quantiser indices for a layout: each large block's region index at its top-left nibble
/// and at every nibble it covers (unused by the decoder; repeated so Zstandard has less to code).
pub(crate) fn layout_qidx(s: &Source, kit: &Kit, t: u32, qi8: &[usize], layout: u8, p: &LmbxParams) -> Vec<usize> {
    let mut qi = qi8.to_vec();
    for (bx, by, n) in tile_blocks(layout) {
        if n == 8 {
            continue;
        }
        let r = region_qidx(s, kit, t, bx, by, n, qi8, p);
        for y in by..by + n / 8 {
            for x in bx..bx + n / 8 {
                qi[(y * TB + x) as usize] = r;
            }
        }
    }
    qi
}

/// [`layout_qidx`] with each large block's step chosen by rate-distortion (`rd_q`): `J` of its AC
/// terms (every plane, weighted) at the tile's lambda, among the 8x8 rule's index, one either side
/// of it, and the finest and coarsest of the covered 8x8 blocks'.
#[allow(clippy::too_many_arguments)]
fn rd_qidx(s: &Source, kit: &Kit, t: u32, qi8: &[usize], layout: u8, rate: &[f64], lam: f64, p: &LmbxParams) -> Vec<usize> {
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    let mut qi = qi8.to_vec();
    let pls = planes(s);
    for (bx, by, n) in tile_blocks(layout) {
        if n == 8 && !p.rd_q8 {
            continue;
        }
        let rule = region_qidx(s, kit, t, bx, by, n, qi8, p);
        let cover: Vec<usize> = (by..by + n / 8).flat_map(|y| (bx..bx + n / 8).map(move |x| qi8[(y * TB + x) as usize])).collect();
        let mut opts = vec![rule, rule.saturating_sub(1), (rule + 1).min(QT.len() - 1), cover.iter().copied().min().unwrap_or(rule), cover.iter().copied().max().unwrap_or(rule)];
        opts.sort_unstable();
        opts.dedup();
        let coefs: Vec<(Vec<f64>, bool, f64)> =
            pls.iter().map(|&(pl, shift, chroma, w)| (fdct_n(kit, n as usize, &block_n(pl, tx * TILE + bx * 8, ty * TILE + by * 8, n, shift)), chroma, w)).collect();
        let zz = &kit.zz[si(n)];
        let dm = f64::from(dequant_max(n));
        let best = opts
            .into_iter()
            .map(|qx| {
                let j: f64 = coefs
                    .iter()
                    .map(|(c, chroma, w)| {
                        let ql = u32::from(QT[qx]);
                        let q = if *chroma { h.chroma_q(ql) } else { ql };
                        let lev = quant_n(c, zz, q, rate, p.lambda * f64::from(q) * f64::from(q), p, true);
                        let se: f64 = zz.iter().enumerate().skip(1).map(|(k, &z)| (c[z as usize] - (f64::from(lev[k]) * f64::from(q)).clamp(-dm, dm)).powi(2)).sum();
                        let last = lev.iter().skip(1).rposition(|&v| v != 0).map_or(0, |l| l + 1);
                        let bits: f64 = (1..=last).map(|k| rate[(lev[k] + LEVEL_OFFSET) as usize]).sum::<f64>() + rate[EOB as usize];
                        w * se + lam * bits
                    })
                    .sum();
                (j, qx)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(rule, |b| b.1);
        for y in by..by + n / 8 {
            for x in bx..bx + n / 8 {
                qi[(y * TB + x) as usize] = best;
            }
        }
    }
    qi
}

/// The layout of least `J` for tile `t` (8x8 quantiser indices `qi8`, guarded).
pub(crate) fn choose_layout(s: &Source, kit: &Kit, t: u32, qi8: &[usize], rate: &[f64], p: &LmbxParams) -> (u8, Vec<usize>) {
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    let lam = p.lambda * qi8.iter().map(|&i| f64::from(QT[i]).powi(2)).sum::<f64>() / qi8.len() as f64;
    let qi8 = &(if p.rd_q8 { rd_qidx(s, kit, t, qi8, 0, rate, lam, p) } else { qi8.to_vec() })[..];
    let (qi16, qi32) = if p.rd_q {
        (rd_qidx(s, kit, t, qi8, 15, rate, lam, p), rd_qidx(s, kit, t, qi8, TMAP_32, rate, lam, p))
    } else {
        (layout_qidx(s, kit, t, qi8, 15, p), layout_qidx(s, kit, t, qi8, TMAP_32, p))
    };
    let qidx_of = |n: u32, b: usize| match n {
        8 => qi8[b],
        16 => qi16[b],
        _ => qi32[0],
    };
    let pls = planes(s);
    // cands[plane][size][8x8 position]: the top-left positions of each size's grid.
    let mut cands: Vec<[Vec<Option<Cand>>; 3]> = Vec::new();
    for &(pl, shift, chroma, _) in &pls {
        let mut per: [Vec<Option<Cand>>; 3] = std::array::from_fn(|_| (0..16).map(|_| None).collect());
        for n in [8u32, 16, 32] {
            for by in (0..TB).step_by((n / 8) as usize) {
                for bx in (0..TB).step_by((n / 8) as usize) {
                    let b = (by * TB + bx) as usize;
                    let ql = u32::from(QT[qidx_of(n, b)]);
                    let q = if chroma { h.chroma_q(ql) } else { ql };
                    let c = fdct_n(kit, n as usize, &block_n(pl, tx * TILE + bx * 8, ty * TILE + by * 8, n, shift));
                    let zz = &kit.zz[si(n)];
                    let lev = quant_n(&c, zz, q, rate, p.lambda * f64::from(q) * f64::from(q), p, true);
                    let dm = f64::from(dequant_max(n));
                    let ac_se = zz.iter().enumerate().skip(1).map(|(k, &z)| (c[z as usize] - (f64::from(lev[k]) * f64::from(q)).clamp(-dm, dm)).powi(2)).sum();
                    let last_ac = lev.iter().skip(1).rposition(|&v| v != 0).map_or(0, |l| l + 1);
                    let ac_bits = (1..=last_ac).map(|k| rate[(lev[k] + LEVEL_OFFSET) as usize]).sum();
                    per[si(n)][b] = Some(Cand { dc: c[0], ac_se, ac_bits, last_ac });
                }
            }
        }
        cands.push(per);
    }
    let mut best = (f64::INFINITY, 0u8);
    let layouts: Vec<u8> = match p.max_tsize {
        8 => vec![0],
        16 => (0u8..16).collect(),
        _ => (0u8..16).chain([TMAP_32]).collect(),
    };
    for layout in layouts {
        let mut j = 0f64;
        for (pi, &(_, _, chroma, w)) in pls.iter().enumerate() {
            let mut dc = 0i32;
            let (mut se, mut bits) = (0f64, 0f64);
            for (bx, by, n) in tile_blocks(layout) {
                let b = (by * TB + bx) as usize;
                let Some(c) = cands[pi][si(n)][b].as_ref() else { continue };
                let ql = u32::from(QT[qidx_of(n, b)]);
                let q = if chroma { h.chroma_q(ql) } else { ql };
                let qdc = dc_step_n(q, chroma, n);
                let sh = si(n) as u32;
                let pred = dc << sh;
                let l0 = ((c.dc - f64::from(pred)) / f64::from(qdc)).round().clamp(-f64::from(LEVEL_OFFSET), f64::from(LEVEL_OFFSET)) as i32;
                let lim = dequant_max(n);
                let rec = (pred + l0 * qdc as i32).clamp(-lim, lim);
                dc = rec >> sh;
                se += (c.dc - f64::from(rec)).powi(2) + c.ac_se;
                let nn = (n * n) as usize;
                let dcs = rate[(l0 + LEVEL_OFFSET) as usize];
                bits += if c.last_ac > 0 {
                    dcs + c.ac_bits + if c.last_ac + 1 < nn { rate[EOB as usize] } else { 0.0 }
                } else if l0 != 0 {
                    dcs + rate[EOB as usize]
                } else {
                    rate[EOB as usize]
                };
            }
            j += w * se + lam * bits;
        }
        if j < best.0 {
            best = (j, layout);
        }
    }
    // The chosen layout's indices: each block's from the candidates it was judged with.
    let mut qi = qi8.to_vec();
    for (bx, by, n) in tile_blocks(best.1) {
        for y in by..by + n / 8 {
            for x in bx..bx + n / 8 {
                qi[(y * TB + x) as usize] = qidx_of(n, (by * TB + bx) as usize);
            }
        }
    }
    (best.1, qi)
}

/// The colour guard (W6b, `crate::lmbx::dc_guard`) for any layout (4:4:4): per smooth block (luma
/// AC energy per 8x8 area under 23), in stream order with every plane's DC chain, keep its step if
/// the exact decoded colour of a DC-only block (the normative integer inverse of its size and the
/// inverse YCoCg-R) is within `tol` of the block's mean, shown channels and alpha, else take the
/// coarsest finer step that is.
pub(crate) fn guard(s: &Source, t: u32, mut qi: Vec<usize>, layout: u8, tol: f64) -> Vec<usize> {
    use dxvkit_plus_core::lmbx::{idct_n, to_8bit};
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    let mut chain = [0i32; 4];
    for (bx, by, n) in tile_blocks(layout) {
        let (x0, y0) = (tx * TILE + bx * 8, ty * TILE + by * 8);
        let b = (by * TB + bx) as usize;
        let mean = |pl: &Plane, shift: i32| block_n(pl, x0, y0, n, shift).iter().sum::<f64>() / f64::from(n * n);
        // DC coefficients (n x the mean, code values) of the coded planes.
        let c: [Option<f64>; 4] =
            [Some(mean(&s.y, 512)), Some(mean(&s.co, 0)), Some(mean(&s.cg, 0)), s.a.as_ref().map(|a| mean(a, 512))].map(|m| m.map(|v| v * f64::from(n)));
        let ac = {
            let blk = block_n(&s.y, x0, y0, n, 512);
            let m = blk.iter().sum::<f64>() / blk.len() as f64;
            (blk.iter().map(|v| (v - m).powi(2)).sum::<f64>() / f64::from((n / 8) * (n / 8))).sqrt()
        };
        let sh = si(n) as u32;
        let lim = dequant_max(n);
        // The block's decoded DC per plane (the decoder's `D[0]`) for step `q`; the chain is it
        // `>> sh`.
        let next = |q: u32| -> [i32; 4] {
            std::array::from_fn(|k| match c[k] {
                Some(c0) => {
                    let chroma = k == 1 || k == 2;
                    let qdc = dc_step_n(if chroma { h.chroma_q(q) } else { q }, chroma, n);
                    let pred = chain[k] << sh;
                    let l0 = ((c0 - f64::from(pred)) / f64::from(qdc)).round().clamp(-f64::from(LEVEL_OFFSET), f64::from(LEVEL_OFFSET)) as i32;
                    (pred + l0 * qdc as i32).clamp(-lim, lim)
                }
                None => chain[k] << sh,
            })
        };
        let target: [f64; 4] = {
            let mut acc = [0f64; 4];
            for i in 0..n {
                for j in 0..n {
                    let yi = ((y0 + i) * s.y.w + x0 + j) as usize;
                    let (yy, co, cg) = (s.y.v[yi], s.co.v[yi], s.cg.v[yi]);
                    let tt = yy - (cg >> 1);
                    let (g, bb) = (cg + tt, tt - (co >> 1));
                    let a = s.a.as_ref().map_or(1020, |a| a.v[yi]);
                    for (k, v) in [bb + co, g, bb, a].into_iter().enumerate() {
                        acc[k] += f64::from(v) / 4.0 / f64::from(n * n);
                    }
                }
            }
            acc
        };
        // The decoded sample of a DC-only block of DC `d`.
        let flat = |d: i32, off: i32| {
            let nn = (n * n) as usize;
            let mut blk = vec![0i32; nn];
            blk[0] = d;
            let mut out = vec![0i32; nn];
            idct_n(n, &blk, off, &mut out);
            out[0]
        };
        let fits = |nx: &[i32; 4]| {
            let yy = flat(nx[0], 512);
            let (co, cg) = (flat(nx[1], 0), flat(nx[2], 0));
            let tt = yy - (cg >> 1);
            let (g, bb) = (cg + tt, tt - (co >> 1));
            let a = if s.a.is_some() { flat(nx[3], 512) } else { 1020 };
            let rec = [bb + co, g, bb, a].map(|v| f64::from(to_8bit(v.clamp(0, 1020) as u16)));
            (0..3).all(|k| (rec[k] * rec[3] - target[k] * target[3]).abs() / 255.0 <= tol) && (rec[3] - target[3]).abs() <= tol
        };
        if ac < 23.0 && !fits(&next(u32::from(QT[qi[b]]))) {
            qi[b] = (0..qi[b]).rev().find(|&i| fits(&next(u32::from(QT[i])))).unwrap_or(qi[b]);
            for yy in by..by + n / 8 {
                for xx in bx..bx + n / 8 {
                    qi[(yy * TB + xx) as usize] = qi[b];
                }
            }
        }
        chain = next(u32::from(QT[qi[b]])).map(|d| d >> sh);
    }
    qi
}

/// The tile's streams (Y, Co, Cg, and A) for a layout, and its weighted squared error per plane
/// (`crate::lmbx::tile_symbols` for any layout).
#[allow(clippy::too_many_arguments)]
pub(crate) fn tile_symbols(s: &Source, kit: &Kit, t: u32, qi: &[usize], layout: u8, rate: &[f64], p: &LmbxParams, rdo: bool) -> (Vec<Symbols>, [f64; 4]) {
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    let mut out = Vec::with_capacity(4);
    let mut sse = [0f64; 4];
    for (pk, &(pl, shift, chroma, w)) in planes(s).iter().enumerate() {
        let (mut syms, mut dc) = (Symbols::new(), 0i32);
        for (bx, by, n) in tile_blocks(layout) {
            let ql = u32::from(QT[qi[(by * TB + bx) as usize]]);
            let q = if chroma { h.chroma_q(ql) } else { ql };
            let c = fdct_n(kit, n as usize, &block_n(pl, tx * TILE + bx * 8, ty * TILE + by * 8, n, shift));
            let zz = &kit.zz[si(n)];
            let mut lev = quant_n(&c, zz, q, rate, p.lambda * f64::from(q) * f64::from(q), p, rdo);
            let qdc = dc_step_n(q, chroma, n);
            let sh = si(n) as u32;
            let lim = dequant_max(n);
            let pred = dc << sh;
            let l0 = ((c[0] - f64::from(pred)) / f64::from(qdc)).round().clamp(-f64::from(LEVEL_OFFSET), f64::from(LEVEL_OFFSET)) as i32;
            lev[0] = l0;
            let rec = (pred + l0 * qdc as i32).clamp(-lim, lim);
            dc = rec >> sh;
            let nn = lev.len();
            let last = lev.iter().rposition(|&v| v != 0).map_or(0, |l| l + 1);
            syms.extend(lev[..last].iter().map(|&l| (l + LEVEL_OFFSET) as u16));
            if last < nn {
                syms.push(EOB);
            }
            let mut se = (c[0] - f64::from(rec)).powi(2);
            for (k, &z) in zz.iter().enumerate().skip(1) {
                se += (c[z as usize] - f64::from((lev[k] * q as i32).clamp(-lim, lim))).powi(2);
            }
            sse[pk] += w * se;
        }
        out.push(syms);
    }
    (out, sse)
}
