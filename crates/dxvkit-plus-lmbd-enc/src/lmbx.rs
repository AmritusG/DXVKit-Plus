//! The LMBX encoder: RGBA → YCoCg-R → (4:2:2 or 4:4:4) → 8x8 DCT → per-block adaptive
//! quantisation with RDO → rANS, one stream per plane of every 32x32 tile → one LMBX frame
//! bitstream
//! (`dxvkit_plus_core::lmbx`, `docs/LMBD.md`).
//!
//! Ported from Amrit's B1d prototype encoder (`loomda-codec` @ c6d80e6,
//! `phase0-bench/lmbx-b0/lmbx_b1d.cpp`): the per-block quantiser choice from the luma block's AC
//! energy, deadzone quantisation, RDOQ against the actual rANS cost with an end-of-block trellis,
//! and low-frequency protection (DC and the two lowest AC terms are never deadzoned or dropped,
//! which keeps gradients from banding). New here: the v1 bitstream (frames of any size, edge
//! replicated to whole tiles; optional alpha; compact tables), tile-parallel encoding, and rate
//! control ([`encode_frame_to_size`]: the best quality whose frame fits a byte budget).
//!
//! The encoder works in floating point; only the decoder is normative. [`float_reference_rgba8`]
//! decodes a bitstream with a floating-point inverse transform (the prototypes' decoder) — the
//! yardstick for what the integer transform costs (LMBX-D1).
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::lmbx::{
    COEF_SYMBOLS, Chroma, EOB, Frame, FrameHeader, FreqTable, LEVEL_OFFSET, PLANES, PROB_BITS, PROB_SCALE, PlaneKind, RANS_L, TILE, ZIGZAG, visit_stream,
};

/// 8x8 blocks per tile edge.
const TB: u32 = TILE / 8;

/// The quantiser steps every frame carries (B1d's table).
pub const QT: [u16; 16] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 48, 64, 96];

/// Encoder settings. The defaults are B1d's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LmbxParams {
    pub chroma: Chroma,
    /// Base quantiser (lower = better quality, larger files). B1d default 16.
    pub quality: f64,
    /// Chroma quantiser ratio; `None` = 1.0 for 4:2:2, 1.6 for 4:4:4 (B1d).
    pub chroma_ratio: Option<f64>,
    /// RDO multiplier: lambda = this * q^2 per block (B1d 0.25).
    pub lambda: f64,
    /// Deadzone rounding bias, < 0.5 widens the zero bin (B1d 0.45).
    pub deadzone: f64,
    /// RDOQ passes after the deadzone bootstrap (B1d 2; 0 = deadzone only).
    pub rdo_passes: u32,
    /// Zigzag coefficients protected from the deadzone and the trellis (B1d 3).
    pub protect: usize,
    /// Worker threads for the tiles.
    pub threads: usize,
}

impl Default for LmbxParams {
    fn default() -> Self {
        LmbxParams { chroma: Chroma::Halo422, quality: 16.0, chroma_ratio: None, lambda: 0.25, deadzone: 0.45, rdo_passes: 2, protect: 3, threads: 8 }
    }
}

/// The orthonormal 8x8 DCT-II basis.
fn basis() -> [[f64; 8]; 8] {
    let mut m = [[0f64; 8]; 8];
    for (k, row) in m.iter_mut().enumerate() {
        let ck = if k == 0 { (1.0f64 / 8.0).sqrt() } else { (2.0f64 / 8.0).sqrt() };
        for (n, v) in row.iter_mut().enumerate() {
            *v = ck * (std::f64::consts::PI * (2 * n + 1) as f64 * k as f64 / 16.0).cos();
        }
    }
    m
}

fn fdct(m: &[[f64; 8]; 8], x: &[f64; 64]) -> [f64; 64] {
    let mut t = [0f64; 64];
    for i in 0..8 {
        for j in 0..8 {
            t[i * 8 + j] = (0..8).map(|n| m[i][n] * x[n * 8 + j]).sum();
        }
    }
    let mut out = [0f64; 64];
    for i in 0..8 {
        for j in 0..8 {
            out[i * 8 + j] = (0..8).map(|n| t[i * 8 + n] * m[j][n]).sum();
        }
    }
    out
}

fn idct_f(m: &[[f64; 8]; 8], d: &[f64; 64]) -> [f64; 64] {
    let mut t = [0f64; 64];
    for i in 0..8 {
        for j in 0..8 {
            t[i * 8 + j] = (0..8).map(|k| m[k][i] * d[k * 8 + j]).sum();
        }
    }
    let mut out = [0f64; 64];
    for i in 0..8 {
        for j in 0..8 {
            out[i * 8 + j] = (0..8).map(|l| t[i * 8 + l] * m[l][j]).sum();
        }
    }
    out
}

/// A plane of integer samples (code values, YCoCg-R) at the coded size.
struct Plane {
    w: u32,
    v: Vec<i32>,
}

impl Plane {
    fn block(&self, x0: u32, y0: u32, shift: i32) -> [f64; 64] {
        let mut b = [0f64; 64];
        for i in 0..8 {
            for j in 0..8 {
                b[i * 8 + j] = f64::from(self.v[((y0 + i as u32) * self.w + x0 + j as u32) as usize] - shift);
            }
        }
        b
    }
}

struct Source {
    header: FrameHeader,
    y: Plane,
    co: Plane,
    cg: Plane,
    a: Option<Plane>,
}

/// RGBA8 → YCoCg-R planes at the coded size (edge-replicated), chroma subsampled for 4:2:2
/// (`(a + b + 1) >> 1` of each column pair).
fn prepare(rgba: &[u8], width: u32, height: u32, chroma: Chroma, chroma_ratio: u16) -> Result<Source, MediaError> {
    if width == 0 || height == 0 || width > dxvkit_plus_core::MAX_DIMENSION || height > dxvkit_plus_core::MAX_DIMENSION {
        return Err(MediaError::Invalid(format!("frame size {width}x{height}")));
    }
    if rgba.len() != (width * height * 4) as usize {
        return Err(MediaError::Invalid(format!("{width}x{height} RGBA8 needs {} bytes, got {}", width * height * 4, rgba.len())));
    }
    let alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
    let header = FrameHeader { chroma, alpha, width, height, chroma_ratio, qt: QT };
    let (cw, ch) = header.coded_size();
    let n = (cw * ch) as usize;
    let (mut y, mut co, mut cg, mut a) = (vec![0; n], vec![0; n], vec![0; n], vec![0; n]);
    for py in 0..ch {
        for px in 0..cw {
            let s = ((py.min(height - 1) * width + px.min(width - 1)) * 4) as usize;
            let (r, g, b) = (i32::from(rgba[s]), i32::from(rgba[s + 1]), i32::from(rgba[s + 2]));
            let o = r - b;
            let t = b + (o >> 1);
            let gg = g - t;
            let i = (py * cw + px) as usize;
            co[i] = o;
            cg[i] = gg;
            y[i] = t + (gg >> 1);
            a[i] = i32::from(rgba[s + 3]);
        }
    }
    let (co, cg) = match chroma {
        Chroma::Full444 => (Plane { w: cw, v: co }, Plane { w: cw, v: cg }),
        Chroma::Halo422 => {
            let hw = cw / 2;
            let half = |v: &[i32]| -> Vec<i32> {
                (0..ch)
                    .flat_map(|py| (0..hw).map(move |px| (py, px)))
                    .map(|(py, px)| (v[(py * cw + 2 * px) as usize] + v[(py * cw + 2 * px + 1) as usize] + 1) >> 1)
                    .collect()
            };
            (Plane { w: hw, v: half(&co) }, Plane { w: hw, v: half(&cg) })
        }
    };
    Ok(Source { header, y: Plane { w: cw, v: y }, co, cg, a: alpha.then_some(Plane { w: cw, v: a }) })
}

const AC_REF: f64 = 380.0;

/// B1d's per-block quantiser: the step nearest `base * clamp(AC energy / 380, 0.16, 2.6)`.
fn pick_qidx(m: &[[f64; 8]; 8], y: &Plane, x0: u32, y0: u32, base: f64) -> usize {
    let c = fdct(m, &y.block(x0, y0, 128));
    let ac = c.iter().skip(1).map(|v| v * v).sum::<f64>().sqrt();
    let qy = (base * (ac / AC_REF).clamp(0.16, 2.6)).round().clamp(1.0, 96.0);
    QT.iter().enumerate().min_by(|a, b| (f64::from(*a.1) - qy).abs().total_cmp(&(f64::from(*b.1) - qy).abs())).map_or(0, |(i, _)| i)
}

/// Quantise one block's raster coefficients to zigzag levels (B1d `quant_block`).
fn quant_block(c: &[f64; 64], q: u32, rate: &[f64], lambda: f64, p: &LmbxParams, rdo: bool) -> [i32; 64] {
    let q = f64::from(q);
    let clamp = |l: i64| l.clamp(-i64::from(LEVEL_OFFSET), i64::from(LEVEL_OFFSET)) as i32;
    let cz: [f64; 64] = std::array::from_fn(|k| c[ZIGZAG[k] as usize]);
    let mut lev = [0i32; 64];
    for k in 0..64 {
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
        // End-of-block trellis: drop a trailing nonzero (and the zeros it strands) when it pays.
        let mut last = (0..64).rev().find(|&k| lev[k] != 0);
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

/// One stream's coefficient symbols.
type Symbols = Vec<u16>;

fn emit_block(syms: &mut Symbols, lev: &[i32; 64]) {
    let last = (0..64).rev().find(|&k| lev[k] != 0);
    let n = last.map_or(0, |l| l + 1);
    for &l in &lev[..n] {
        syms.push((l + LEVEL_OFFSET) as u16);
    }
    if n < 64 {
        syms.push(EOB);
    }
}

/// One tile's streams (Y, Co, Cg, and A with alpha) for the given rate model.
fn tile_symbols(m: &[[f64; 8]; 8], s: &Source, qidx: &[Vec<usize>], rate: &[f64], p: &LmbxParams, rdo: bool, ti: u32) -> Vec<Symbols> {
    let h = &s.header;
    let (tx, ty) = (ti % h.tiles_x(), ti / h.tiles_x());
    let qi = &qidx[ti as usize];
    let block = |plane: &Plane, x0: u32, y0: u32, shift: i32, q: u32, syms: &mut Symbols| {
        let c = fdct(m, &plane.block(x0, y0, shift));
        let lambda = p.lambda * f64::from(q) * f64::from(q);
        emit_block(syms, &quant_block(&c, q, rate, lambda, p, rdo));
    };
    let q_of = |by: u32, bx: u32| u32::from(QT[qi[(by * TB + bx) as usize]]);
    let mut out = Vec::with_capacity(4);
    for kind in PLANES.into_iter().take(h.planes() as usize) {
        let mut syms = Symbols::new();
        match kind {
            PlaneKind::Y | PlaneKind::A => {
                let plane = if kind == PlaneKind::Y { &s.y } else { s.a.as_ref().unwrap_or(&s.y) };
                for by in 0..TB {
                    for bx in 0..TB {
                        block(plane, tx * TILE + bx * 8, ty * TILE + by * 8, 128, q_of(by, bx), &mut syms);
                    }
                }
            }
            PlaneKind::Co | PlaneKind::Cg => {
                let plane = if kind == PlaneKind::Co { &s.co } else { &s.cg };
                let (cw, cbw) = (h.chroma.tile_width(), h.chroma.tile_width() / 8);
                for by in 0..TB {
                    for bx in 0..cbw {
                        let lbx = if h.chroma == Chroma::Halo422 { 2 * bx } else { bx };
                        block(plane, tx * cw + bx * 8, ty * TILE + by * 8, 0, h.chroma_q(q_of(by, lbx)), &mut syms);
                    }
                }
            }
        }
        out.push(syms);
    }
    out
}

fn all_symbols(m: &[[f64; 8]; 8], s: &Source, qidx: &[Vec<usize>], rate: &[f64], p: &LmbxParams, rdo: bool) -> Vec<Symbols> {
    let nt = s.header.tile_count();
    let threads = p.threads.clamp(1, 64) as u32;
    let per = nt.div_ceil(threads).max(1);
    std::thread::scope(|sc| {
        let handles: Vec<_> = (0..nt)
            .step_by(per as usize)
            .map(|start| sc.spawn(move || (start..(start + per).min(nt)).flat_map(|ti| tile_symbols(m, s, qidx, rate, p, rdo, ti)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// Normalise a histogram to frequencies adding up to [`PROB_SCALE`], every present symbol >= 1
/// (B1d `normalize`).
fn normalize(hist: &[u64]) -> Vec<u16> {
    let total: u64 = hist.iter().sum();
    let mut f = vec![0u16; hist.len()];
    if total == 0 {
        f[0] = PROB_SCALE as u16;
        return f;
    }
    let mut sum = 0u32;
    for (d, &h) in f.iter_mut().zip(hist) {
        if h > 0 {
            let v = (h * u64::from(PROB_SCALE) / total).clamp(1, u64::from(PROB_SCALE) - 1);
            *d = v as u16;
            sum += v as u32;
        }
    }
    let largest = |f: &[u16]| f.iter().enumerate().max_by_key(|&(i, &v)| (v, std::cmp::Reverse(i))).map_or(0, |(i, _)| i);
    while sum > PROB_SCALE {
        let i = largest(&f);
        if f[i] <= 1 {
            break;
        }
        f[i] -= 1;
        sum -= 1;
    }
    while sum < PROB_SCALE {
        let i = largest(&f);
        f[i] += 1;
        sum += 1;
    }
    f
}

fn histogram(streams: &[Symbols]) -> Vec<u64> {
    let mut h = vec![0u64; COEF_SYMBOLS];
    for t in streams {
        for &s in t {
            h[s as usize] += 1;
        }
    }
    h
}

/// rANS-encode one stream: 4 state bytes (little endian), then the stream.
fn rans_encode(syms: &Symbols, t: &FreqTable) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(syms.len() + 8);
    let mut x = RANS_L;
    for &s in syms.iter().rev() {
        let (f, c) = (u32::from(t.freq[s as usize]), t.cum[s as usize]);
        let xmax = ((RANS_L >> PROB_BITS) << 8) * f;
        while x >= xmax {
            out.push((x & 0xff) as u8);
            x >>= 8;
        }
        x = ((x / f) << PROB_BITS) + (x % f) + c;
    }
    out.extend_from_slice(&x.to_be_bytes());
    out.reverse();
    out
}

/// Encode one straight-RGBA8 frame to an LMBX frame bitstream.
pub fn encode_frame(rgba: &[u8], width: u32, height: u32, p: &LmbxParams) -> Result<Vec<u8>, MediaError> {
    if !(p.quality.is_finite() && p.quality > 0.0 && p.lambda.is_finite() && p.lambda >= 0.0 && (0.0..=0.5).contains(&p.deadzone)) {
        return Err(MediaError::Invalid(format!("LMBX parameters out of range: {p:?}")));
    }
    let ratio = p.chroma_ratio.unwrap_or(if p.chroma == Chroma::Halo422 { 1.0 } else { 1.6 });
    let ratio256 = (ratio * 256.0).round().clamp(1.0, 65535.0) as u16;
    let s = prepare(rgba, width, height, p.chroma, ratio256)?;
    let m = basis();
    let h = s.header;
    let qidx: Vec<Vec<usize>> = (0..h.tile_count())
        .map(|ti| {
            let (tx, ty) = (ti % h.tiles_x(), ti / h.tiles_x());
            (0..TB * TB).map(|b| pick_qidx(&m, &s.y, tx * TILE + (b % TB) * 8, ty * TILE + (b / TB) * 8, p.quality)).collect()
        })
        .collect();
    // Bootstrap (deadzone only) -> rate model -> RDOQ passes.
    let mut rate = vec![0f64; COEF_SYMBOLS];
    let mut syms = all_symbols(&m, &s, &qidx, &rate, p, false);
    for _ in 0..p.rdo_passes {
        let f = normalize(&histogram(&syms));
        for (r, &v) in rate.iter_mut().zip(&f) {
            *r = -(f64::from(v.max(1)) / f64::from(PROB_SCALE)).log2();
        }
        syms = all_symbols(&m, &s, &qidx, &rate, p, true);
    }
    let coef_f = normalize(&histogram(&syms));
    let coef_t = FreqTable::from_freq(coef_f.clone())?;
    let streams: Vec<Vec<u8>> = syms.iter().map(|t| rans_encode(t, &coef_t)).collect();
    let mut out = h.to_bytes().to_vec();
    out.extend(FreqTable::sparse_bytes(&coef_f));
    for q in &qidx {
        out.extend(q.chunks_exact(2).map(|p| (p[0] | (p[1] << 4)) as u8));
    }
    out.extend_from_slice(&h.stream_count().to_le_bytes());
    let mut off = 0u32;
    for st in &streams {
        out.extend_from_slice(&off.to_le_bytes());
        off += st.len() as u32;
    }
    for st in &streams {
        out.extend_from_slice(st);
    }
    Ok(out)
}

/// Rate control: the best quality (smallest base quantiser in `1..=max_quality`, searched in
/// halves to 0.25) whose frame fits `budget` bytes. Returns the bitstream and the quality used; if
/// even `max_quality` does not fit, its frame.
pub fn encode_frame_to_size(rgba: &[u8], width: u32, height: u32, p: &LmbxParams, budget: usize, max_quality: f64) -> Result<(Vec<u8>, f64), MediaError> {
    let at = |q: f64| encode_frame(rgba, width, height, &LmbxParams { quality: q, ..*p });
    let (mut lo, mut hi) = (1.0f64, max_quality.max(1.0));
    let first = at(lo)?;
    if first.len() <= budget {
        return Ok((first, lo));
    }
    let mut best = (at(hi)?, hi);
    if best.0.len() > budget {
        return Ok(best);
    }
    while hi - lo > 0.25 {
        let mid = (lo + hi) / 2.0;
        let f = at(mid)?;
        if f.len() <= budget {
            best = (f, mid);
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(best)
}

/// Decode a frame bitstream with a floating-point inverse DCT and inverse YCoCg (the prototypes'
/// decoder): not conforming — the yardstick for the integer transform. 8-bit RGBA, rounded.
pub fn float_reference_rgba8(b: &[u8], width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    let f = Frame::parse(b, width, height)?;
    let h = f.header;
    let m = basis();
    let (cw, ch) = h.coded_size();
    let chw = h.chroma.plane_width(cw);
    let (mut y, mut co, mut cg, mut a) =
        (vec![0f64; (cw * ch) as usize], vec![0f64; (chw * ch) as usize], vec![0f64; (chw * ch) as usize], vec![255f64; (cw * ch) as usize]);
    for ti in 0..h.tile_count() {
        let ty = ti / h.tiles_x();
        for kind in PLANES.into_iter().take(h.planes() as usize) {
            let (plane, stride, shift) = match kind {
                PlaneKind::Y => (&mut y, cw, 128.0),
                PlaneKind::Co => (&mut co, chw, 0.0),
                PlaneKind::Cg => (&mut cg, chw, 0.0),
                PlaneKind::A => (&mut a, cw, 128.0),
            };
            visit_stream(&f, ti, kind, |x0, y0, d| {
                let s = idct_f(&m, &d.map(f64::from));
                for i in 0..8 {
                    for j in 0..8 {
                        plane[((ty * TILE + y0 + i) * stride + x0 + j) as usize] = s[(i * 8 + j) as usize] + shift;
                    }
                }
            });
        }
    }
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for py in 0..height {
        for px in 0..width {
            let at = |v: &[f64], st: u32, x: u32| v[(py * st + x) as usize];
            let (o, g) = match h.chroma {
                Chroma::Full444 => (at(&co, chw, px), at(&cg, chw, px)),
                Chroma::Halo422 => {
                    let c = px / 2;
                    let n = (c + 1).min(chw - 1);
                    if px % 2 == 0 {
                        (at(&co, chw, c), at(&cg, chw, c))
                    } else {
                        ((at(&co, chw, c) + at(&co, chw, n)) / 2.0, (at(&cg, chw, c) + at(&cg, chw, n)) / 2.0)
                    }
                }
            };
            let t = at(&y, cw, px) - g / 2.0;
            let gg = g + t;
            let bb = t - o / 2.0;
            let rr = bb + o;
            for v in [rr, gg, bb, at(&a, cw, px)] {
                out.push(v.round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dxvkit_plus_core::lmbx::decode_rgba8;

    fn ramp(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        (0..h)
            .flat_map(|y| {
                (0..w).flat_map(move |x| [(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) % 256) as u8, if alpha { (x * 7 % 256) as u8 } else { 255 }])
            })
            .collect()
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse = a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
    }

    #[test]
    fn round_trips_both_chroma_modes_odd_sizes_and_alpha() {
        for (w, h) in [(70u32, 33u32), (128, 64), (5, 3)] {
            for chroma in [Chroma::Halo422, Chroma::Full444] {
                for alpha in [false, true] {
                    let src = ramp(w, h, alpha);
                    let b = encode_frame(&src, w, h, &LmbxParams { chroma, quality: 2.0, ..Default::default() }).unwrap();
                    let f = Frame::parse(&b, w, h).unwrap();
                    assert_eq!((f.header.alpha, f.header.chroma), (alpha, chroma));
                    let out = decode_rgba8(&b, w, h, true).unwrap();
                    assert_eq!(out, decode_rgba8(&b, w, h, false).unwrap(), "parallel == sequential");
                    let p = psnr(&src, &out);
                    // A 5x3 frame is all edges: 4:2:2 caps it lower.
                    let floor = if w < 8 && chroma == Chroma::Halo422 { 25.0 } else { 30.0 };
                    assert!(p > floor, "{w}x{h} {chroma:?} alpha {alpha}: {p:.1} dB");
                    if !alpha {
                        assert!(out.chunks_exact(4).all(|px| px[3] == 255));
                    }
                }
            }
        }
    }

    #[test]
    fn quality_and_rate_control_are_monotonic() {
        let src = ramp(192, 128, false);
        let (fine, coarse) = (
            encode_frame(&src, 192, 128, &LmbxParams { quality: 2.0, ..Default::default() }).unwrap(),
            encode_frame(&src, 192, 128, &LmbxParams { quality: 64.0, ..Default::default() }).unwrap(),
        );
        assert!(fine.len() > coarse.len());
        let budget = (fine.len() + coarse.len()) / 2;
        let (b, q) = encode_frame_to_size(&src, 192, 128, &LmbxParams::default(), budget, 400.0).unwrap();
        assert!(b.len() <= budget && q > 2.0 && q < 64.0, "{} <= {budget}, quality {q}", b.len());
    }
}
