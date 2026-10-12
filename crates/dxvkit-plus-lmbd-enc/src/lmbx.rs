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
    COEF_SYMBOLS, Chroma, EOB, Frame, FrameHeader, FreqTable, LEVEL_OFFSET, PLANES, PROB_BITS, PROB_SCALE, PlaneKind, RANS_L, TILE, TileMode, ZIGZAG,
    visit_stream,
};

/// 8x8 blocks per tile edge.
const TB: u32 = TILE / 8;

/// The quantiser steps every frame carries (B1d's table).
pub const QT: [u16; 16] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 48, 64, 96];

/// Encoder settings. The defaults are B1d's, except the chroma mode: 4:4:4, chosen by measurement
/// (W5: better on both real clips at equal bytes, +1.6 dB and +0.6 dB, for about 26% more GPU
/// decode time) and subject to Amrit's eye test.
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
    /// Store frames as a Zstandard chunk at this level when that is smaller (the LMBD container's
    /// own option; the decoder decompresses before decoding). `None` = always stored. Rate control
    /// counts the stored size.
    pub zstd: Option<i32>,
    /// Bitstream v2 tile modes the encoder may choose, per tile by rate-distortion against the DCT
    /// coding (W6, all on by default — measured; all off writes a v1 frame): flat tiles; palette
    /// tiles (at most 16 colours: measured better than 4 or 8); lossy palettes.
    pub flat: bool,
    pub palette: bool,
    /// With `palette`: also lossy palettes (the most frequent 2..16 colours, refined) for tiles
    /// with more colours.
    pub palette_lossy: bool,
    /// Colour-accuracy guard (W6b), in 8-bit code values; `None` = off (0.6.0's behaviour). A
    /// region of one source colour must decode to a mean within this of its colour, not only score
    /// a good PSNR: a smooth DCT block gets a finer step where its DC would otherwise land off its
    /// colour (`dc_guard`), and a flat or lossy-palette tile is not a candidate if it moves any
    /// region's colour ([`GUARD_CELLS`]) by more than this. Default 1.0: LMB7's own worst flat-region error on the graphics card (1.02).
    pub colour_tol: Option<f64>,
}

impl Default for LmbxParams {
    fn default() -> Self {
        LmbxParams {
            chroma: Chroma::Full444,
            quality: 16.0,
            chroma_ratio: None,
            lambda: 0.25,
            deadzone: 0.45,
            rdo_passes: 2,
            protect: 3,
            threads: 8,
            zstd: None,
            flat: true,
            palette: true,
            palette_lossy: true,
            colour_tol: Some(1.0),
        }
    }
}

/// The colour guard's region size: a colour that fills at least this many uniform 2x2 cells of a
/// tile (an area, not an anti-aliased edge line, which has none) is a region whose colour must
/// stay within [`LmbxParams::colour_tol`].
pub const GUARD_CELLS: usize = 16;

/// The colours of a tile's regions ([`GUARD_CELLS`]).
fn regions(px: &[[u8; 4]]) -> Vec<[u8; 4]> {
    let t = TILE as usize;
    let mut cells: std::collections::HashMap<[u8; 4], usize> = std::collections::HashMap::new();
    for y in 0..t - 1 {
        for x in 0..t - 1 {
            let c = px[y * t + x];
            if px[y * t + x + 1] == c && px[(y + 1) * t + x] == c && px[(y + 1) * t + x + 1] == c {
                *cells.entry(c).or_default() += 1;
            }
        }
    }
    let mut r: Vec<[u8; 4]> = cells.into_iter().filter(|c| c.1 >= GUARD_CELLS).map(|c| c.0).collect();
    r.sort_unstable();
    r
}

/// A block is smooth when its luma AC energy is below this (the RMS deviation from the block mean
/// is then under about 3 code values: no texture masks a shift of its mean).
const SMOOTH_AC: f64 = 23.0;

/// The colour guard on DCT tiles (W6b): a block's DC is a residual from the previous block's
/// reconstructed DC at step `dc_step(q)`, so a flat block can only land on that lattice — at a
/// coarse step a flat region's colour moves by up to `q / 16` code values per plane (12 at the
/// rate floor's q 96 and chroma 154: the measured v1 shift). For each smooth block, in stream
/// order with every plane's DC chain, keep the block's step if its mean (DC) decodes within `tol`
/// of the source in R, G, B and A, else take the coarsest step that does. Only blocks where a
/// region's colour changes need a finer step: after one exact DC the run repeats it for nothing.
fn dc_guard(s: &Source, t: u32, mut qi: Vec<usize>, tol: f64) -> Vec<usize> {
    use dxvkit_plus_core::lmbx::{DEQUANT_MAX, dc_step};
    let h = &s.header;
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    // DC coefficient (8 x the block mean, code values) of a plane's block.
    let dc_of = |pl: &Plane, x0: u32, y0: u32, shift: i32| pl.block(x0, y0, shift).iter().sum::<f64>() / 8.0;
    let ac_of = |pl: &Plane, x0: u32, y0: u32, shift: i32| {
        let b = pl.block(x0, y0, shift);
        let mean = b.iter().sum::<f64>() / 64.0;
        b.iter().map(|v| (v - mean).powi(2)).sum::<f64>().sqrt()
    };
    // Reconstructed DC after one block: the decoder's chain (the encoder rounds the residual).
    let step = |dc: i32, c0: f64, q: u32| -> i32 {
        let l0 = ((c0 - f64::from(dc)) / f64::from(q)).round().clamp(-f64::from(LEVEL_OFFSET), f64::from(LEVEL_OFFSET)) as i32;
        (dc + l0 * q as i32).clamp(-DEQUANT_MAX, DEQUANT_MAX)
    };
    let half = h.chroma == Chroma::Halo422;
    let mut chain = [0i32; 4];
    // The DC-only chroma samples of the current 4:2:2 chroma block (an odd luma block shares them).
    let mut chain_c = [0i32; 2];
    for by in 0..TB {
        for bx in 0..TB {
            let (x0, y0) = (tx * TILE + bx * 8, ty * TILE + by * 8);
            // In 4:2:2 the chroma block of an even luma block (the one that carries its step).
            let chroma = (!half || bx % 2 == 0).then(|| (if half { tx * TILE / 2 + bx / 2 * 8 } else { x0 }, y0));
            let b = (by * TB + bx) as usize;
            let c = [
                Some(dc_of(&s.y, x0, y0, 512)),
                chroma.map(|(cx, cy)| dc_of(&s.co, cx, cy, 0)),
                chroma.map(|(cx, cy)| dc_of(&s.cg, cx, cy, 0)),
                s.a.as_ref().map(|a| dc_of(a, x0, y0, 512)),
            ];
            let next = |q: u32| -> [i32; 4] {
                std::array::from_fn(|k| match c[k] {
                    Some(c0) => step(chain[k], c0, dc_step(if k == 1 || k == 2 { h.chroma_q(q) } else { q }, k == 1 || k == 2)),
                    None => chain[k],
                })
            };
            // The block's mean source RGBA (8-bit) and the exact decoded colour of a DC-only block
            // (the normative integer IDCT and inverse YCoCg-R; 4:2:2's odd-column interpolation
            // aside), within `tol` per shown channel.
            let target: [f64; 4] = {
                let mut acc = [0f64; 4];
                for i in 0..8u32 {
                    for j in 0..8u32 {
                        let yi = ((y0 + i) * s.y.w + x0 + j) as usize;
                        let ci = ((y0 + i) * s.co.w + if half { (x0 + j) / 2 } else { x0 + j }) as usize;
                        let (yy, co, cg) = (s.y.v[yi], s.co.v[ci], s.cg.v[ci]);
                        let t = yy - (cg >> 1);
                        let (g, b) = (cg + t, t - (co >> 1));
                        let a = s.a.as_ref().map_or(1020, |a| a.v[yi]);
                        for (k, v) in [b + co, g, b, a].into_iter().enumerate() {
                            acc[k] += f64::from(v) / 4.0 / 64.0;
                        }
                    }
                }
                acc
            };
            let flat = |d: i32, off: i32| {
                let mut blk = [0i32; 64];
                blk[0] = d;
                let mut out = [0i32; 64];
                dxvkit_plus_core::lmbx::idct(&blk, off, &mut out);
                out[0]
            };
            let fits = |n: &[i32; 4]| {
                let yy = flat(n[0], 512);
                let (co, cg) = if chroma.is_some() { (flat(n[1], 0), flat(n[2], 0)) } else { (chain_c[0], chain_c[1]) };
                let t = yy - (cg >> 1);
                let (g, b) = (cg + t, t - (co >> 1));
                let a = if s.a.is_some() { flat(n[3], 512) } else { 1020 };
                let rec = [b + co, g, b, a].map(|v| f64::from(dxvkit_plus_core::lmbx::to_8bit(v.clamp(0, 1020) as u16)));
                // Shown channels (RGB times alpha) and alpha, as `within`.
                (0..3).all(|k| (rec[k] * rec[3] - target[k] * target[3]).abs() / 255.0 <= tol) && (rec[3] - target[3]).abs() <= tol
            };
            if ac_of(&s.y, x0, y0, 512) < SMOOTH_AC && !fits(&next(u32::from(QT[qi[b]]))) {
                // No step fits (e.g. a 4:2:2 odd block, whose chroma its even neighbour carries): keep it.
                qi[b] = (0..qi[b]).rev().find(|&i| fits(&next(u32::from(QT[i])))).unwrap_or(qi[b]);
            }
            chain = next(u32::from(QT[qi[b]]));
            if chroma.is_some() {
                let flat = |d: i32| {
                    let mut blk = [0i32; 64];
                    blk[0] = d;
                    let mut out = [0i32; 64];
                    dxvkit_plus_core::lmbx::idct(&blk, 0, &mut out);
                    out[0]
                };
                chain_c = [flat(chain[1]), flat(chain[2])];
            }
        }
    }
    qi
}

/// The stored size of a frame bitstream: Zstandard at `zstd` when that is smaller.
pub fn stored_size(blob: &[u8], zstd: Option<i32>) -> usize {
    match zstd.and_then(|l| zstd::bulk::compress(blob, l).ok()) {
        Some(z) if z.len() < blob.len() => z.len(),
        _ => blob.len(),
    }
}

/// The LMBD frame payload of a bitstream: one chunk, Zstandard at `zstd` when smaller, else stored.
pub fn frame_payload(header: &dxvkit_plus_core::lmbd::Header, blob: &[u8], zstd: Option<i32>) -> Result<Vec<u8>, MediaError> {
    use dxvkit_plus_core::lmbd::{ChunkData, Compressor};
    let raw = u32::try_from(blob.len()).map_err(|_| MediaError::Invalid("frame over 4 GiB".into()))?;
    let z = zstd.map(|l| zstd::bulk::compress(blob, l).map_err(|e| MediaError::Io(format!("zstd: {e}")))).transpose()?;
    let chunk = match &z {
        Some(z) if z.len() < blob.len() => ChunkData { compressor: Compressor::Zstd, raw_size: raw, data: z },
        _ => ChunkData { compressor: Compressor::None, raw_size: raw, data: blob },
    };
    dxvkit_plus_core::lmbd::frame_payload(header, &[chunk])
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

/// A plane of integer samples (quarter code values, YCoCg-R) at the coded size.
struct Plane {
    w: u32,
    v: Vec<i32>,
}

impl Plane {
    /// An 8x8 block in code values (`(v - shift) / 4`; `shift` in quarter units).
    fn block(&self, x0: u32, y0: u32, shift: i32) -> [f64; 64] {
        let mut b = [0f64; 64];
        for i in 0..8 {
            for j in 0..8 {
                b[i * 8 + j] = f64::from(self.v[((y0 + i as u32) * self.w + x0 + j as u32) as usize] - shift) / 4.0;
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

/// RGBA8 → YCoCg-R planes in quarter units at the coded size (edge-replicated), chroma
/// subsampled for 4:2:2 (`(a + b + 1) >> 1` of each column pair): the decoder's own forward
/// definition (`Co = 4R - 4B`, `t = 4B + (Co >> 1)`, `Cg = 4G - t`, `Y = t + (Cg >> 1)`, `A = 4A`),
/// which its inverse undoes exactly. (0.6.0 lifted in code values and scaled by 4 afterwards: the
/// floors do not scale, so every colour with odd `R - B` and odd `G - t` — a quarter of all
/// colours — decoded 1 lower in R and B, at any quality. W6b.)
fn prepare(rgba: &[u8], width: u32, height: u32, chroma: Chroma, chroma_ratio: u16) -> Result<Source, MediaError> {
    if width == 0 || height == 0 || width > dxvkit_plus_core::MAX_DIMENSION || height > dxvkit_plus_core::MAX_DIMENSION {
        return Err(MediaError::Invalid(format!("frame size {width}x{height}")));
    }
    if rgba.len() != (width * height * 4) as usize {
        return Err(MediaError::Invalid(format!("{width}x{height} RGBA8 needs {} bytes, got {}", width * height * 4, rgba.len())));
    }
    let alpha = rgba.chunks_exact(4).any(|p| p[3] != 255);
    let header = FrameHeader { version: dxvkit_plus_core::lmbx::VERSION_2, chroma, alpha, width, height, chroma_ratio, qt: QT };
    let (cw, ch) = header.coded_size();
    let n = (cw * ch) as usize;
    let (mut y, mut co, mut cg, mut a) = (vec![0; n], vec![0; n], vec![0; n], vec![0; n]);
    for py in 0..ch {
        for px in 0..cw {
            let s = ((py.min(height - 1) * width + px.min(width - 1)) * 4) as usize;
            let (r, g, b) = (i32::from(rgba[s]) * 4, i32::from(rgba[s + 1]) * 4, i32::from(rgba[s + 2]) * 4);
            let o = r - b;
            let t = b + (o >> 1);
            let gg = g - t;
            let i = (py * cw + px) as usize;
            co[i] = o;
            cg[i] = gg;
            y[i] = t + (gg >> 1);
            a[i] = i32::from(rgba[s + 3]) * 4;
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
    let c = fdct(m, &y.block(x0, y0, 512));
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
fn tile_symbols(m: &[[f64; 8]; 8], s: &Source, qidx: &[Vec<usize>], rate: &[f64], p: &LmbxParams, rdo: bool, ti: u32) -> (Vec<Symbols>, f64) {
    let h = &s.header;
    let (tx, ty) = (ti % h.tiles_x(), ti / h.tiles_x());
    let qi = &qidx[ti as usize];
    // The DC is coded as a residual from the previous block's reconstructed DC in the stream
    // (`dc`, the decoder's value), so flat runs cost only their end-of-block.
    // Returns the block's squared reconstruction error (sample domain).
    let block = |plane: &Plane, x0: u32, y0: u32, shift: i32, q: u32, chroma: bool, dc: &mut i32, syms: &mut Symbols| -> f64 {
        let c = fdct(m, &plane.block(x0, y0, shift));
        let lambda = p.lambda * f64::from(q) * f64::from(q);
        let mut lev = quant_block(&c, q, rate, lambda, p, rdo);
        // The DC: a residual from the decoder's previous DC, at the DC step, round to nearest.
        let qdc = dxvkit_plus_core::lmbx::dc_step(q, chroma);
        let l0 = ((c[0] - f64::from(*dc)) / f64::from(qdc)).round().clamp(-f64::from(LEVEL_OFFSET), f64::from(LEVEL_OFFSET)) as i32;
        lev[0] = l0;
        *dc = (*dc + l0 * qdc as i32).clamp(-dxvkit_plus_core::lmbx::DEQUANT_MAX, dxvkit_plus_core::lmbx::DEQUANT_MAX);
        emit_block(syms, &lev);
        let dm = dxvkit_plus_core::lmbx::DEQUANT_MAX;
        let mut sse = (c[0] - f64::from(*dc)).powi(2);
        for k in 1..64 {
            let r = ZIGZAG[k] as usize;
            sse += (c[r] - f64::from((lev[k] * q as i32).clamp(-dm, dm))).powi(2);
        }
        sse
    };
    let q_of = |by: u32, bx: u32| u32::from(QT[qi[(by * TB + bx) as usize]]);
    let mut out = Vec::with_capacity(4);
    // The tile's error, approximately in RGB terms (R, G, B = Y +- Co / 2 +- Cg / 2: 3 eY^2 +
    // eCo^2 / 2 + 3 eCg^2 / 4; a 4:2:2 chroma sample covers two pixels), for the mode decision.
    let mut sse = 0f64;
    for kind in PLANES.into_iter().take(h.planes() as usize) {
        let (mut syms, mut dc) = (Symbols::new(), 0i32);
        match kind {
            PlaneKind::Y | PlaneKind::A => {
                let plane = if kind == PlaneKind::Y { &s.y } else { s.a.as_ref().unwrap_or(&s.y) };
                for by in 0..TB {
                    for bx in 0..TB {
                        let (x0, y0) = (tx * TILE + bx * 8, ty * TILE + by * 8);
                        let wgt = if kind == PlaneKind::Y { 3.0 } else { 1.0 };
                        sse += wgt * block(plane, x0, y0, 512, q_of(by, bx), false, &mut dc, &mut syms);
                    }
                }
            }
            PlaneKind::Co | PlaneKind::Cg => {
                let plane = if kind == PlaneKind::Co { &s.co } else { &s.cg };
                let (cw, cbw) = (h.chroma.tile_width(), h.chroma.tile_width() / 8);
                for by in 0..TB {
                    for bx in 0..cbw {
                        let lbx = if h.chroma == Chroma::Halo422 { 2 * bx } else { bx };
                        let wgt = if kind == PlaneKind::Co { 0.5 } else { 0.75 } * if h.chroma == Chroma::Halo422 { 2.0 } else { 1.0 };
                        sse += wgt * block(plane, tx * cw + bx * 8, ty * TILE + by * 8, 0, h.chroma_q(q_of(by, lbx)), true, &mut dc, &mut syms);
                    }
                }
            }
        }
        out.push(syms);
    }
    (out, sse)
}

/// Every tile's streams (flattened, tile-major) and squared error.
fn all_symbols(m: &[[f64; 8]; 8], s: &Source, qidx: &[Vec<usize>], rate: &[f64], p: &LmbxParams, rdo: bool) -> (Vec<Symbols>, Vec<f64>) {
    let nt = s.header.tile_count();
    let threads = p.threads.clamp(1, 64) as u32;
    let per = nt.div_ceil(threads).max(1);
    let tiles: Vec<(Vec<Symbols>, f64)> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..nt)
            .step_by(per as usize)
            .map(|start| sc.spawn(move || (start..(start + per).min(nt)).map(|ti| tile_symbols(m, s, qidx, rate, p, rdo, ti)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let sse = tiles.iter().map(|t| t.1).collect();
    (tiles.into_iter().flat_map(|t| t.0).collect(), sse)
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

/// A tile's direct-mode candidates (v2), from the straight RGBA8 source (edge-replicated).
struct Direct {
    /// The squared error of the tile's mean colour, and that colour.
    flat: Option<(f64, [u8; 4])>,
    /// Palette candidates (exact, or lossy ones).
    palette: Vec<Palette>,
    /// The palette chosen (index into `palette`) when the tile is coded as a palette tile.
    palette_pick: std::cell::Cell<usize>,
}

/// A palette candidate: colours, index symbols, squared error.
struct Palette {
    cols: Vec<[u8; 4]>,
    syms: Symbols,
    dist: f64,
}

/// Whether coding source colour `src` as `rec` keeps it within the colour tolerance: each shown
/// channel (RGB times alpha) and alpha within `tol` code values.
fn within(src: [u8; 4], rec: [u8; 4], tol: f64) -> bool {
    let shown = |p: [u8; 4], c: usize| f64::from(p[c]) * f64::from(p[3]) / 255.0;
    (0..3).all(|c| (shown(src, c) - shown(rec, c)).abs() <= tol) && (f64::from(src[3]) - f64::from(rec[3])).abs() <= tol
}

/// Palette candidates of a tile: the exact palette when it has at most 16 colours; with
/// `palette_lossy` also the most frequent 2, 4, 8 and 16 colours refined
/// by two Lloyd iterations, each pixel mapped to the nearest colour (RGBA squared distance).
/// Under the colour guard (`regions` non-empty only then) the tile's regions seed first, a
/// centroid holding regions moves to their mean only, and a lossy palette that moves any region
/// beyond the tolerance is not a candidate.
fn palette_candidates(px: &[[u8; 4]], p: &LmbxParams, regions: &[[u8; 4]]) -> Vec<Palette> {
    let max = dxvkit_plus_core::lmbx::PALETTE_SYMBOLS;
    if !p.palette {
        return Vec::new();
    }
    let mut count: Vec<([u8; 4], usize)> = Vec::new();
    let mut at: std::collections::HashMap<[u8; 4], usize> = std::collections::HashMap::new();
    for q in px {
        let k = *at.entry(*q).or_insert_with(|| {
            count.push((*q, 0));
            count.len() - 1
        });
        count[k].1 += 1;
    }
    // Most frequent first; ties by first appearance (stable sort).
    count.sort_by_key(|c| std::cmp::Reverse(c.1));
    let d2 = |a: [u8; 4], b: [u8; 4]| (0..4).map(|c| (f64::from(a[c]) - f64::from(b[c])).powi(2)).sum::<f64>();
    let build = |cols: Vec<[u8; 4]>| -> Palette {
        let nearest = |q: [u8; 4]| (0..cols.len()).min_by(|&a, &b| d2(q, cols[a]).total_cmp(&d2(q, cols[b]))).unwrap_or(0);
        let idx: Vec<usize> = px.iter().map(|&q| nearest(q)).collect();
        let dist = px.iter().zip(&idx).map(|(&q, &i)| d2(q, cols[i])).sum();
        let n = cols.len();
        let syms = (0..idx.len())
            .map(|i| {
                let (x, y) = (i % TILE as usize, i / TILE as usize);
                let pred = if x > 0 {
                    idx[i - 1]
                } else if y > 0 {
                    idx[i - TILE as usize]
                } else {
                    0
                };
                ((idx[i] + n - pred) % n) as u16
            })
            .collect();
        Palette { cols, syms, dist }
    };
    let mut out = Vec::new();
    if count.len() <= max {
        out.push(build(count.iter().map(|c| c.0).collect()));
        // Without the guard an exact palette is the only one; with it, smaller palettes that keep
        // every region within the tolerance too (a gradient's bands merge in pairs instead of the
        // whole tile going flat).
        if p.colour_tol.is_none() {
            return out;
        }
    }
    if !p.palette_lossy {
        return out;
    }
    // Seeds: the tile's regions first, then by frequency.
    let seeds: Vec<[u8; 4]> = count.iter().filter(|c| regions.contains(&c.0)).chain(count.iter().filter(|c| !regions.contains(&c.0))).map(|c| c.0).collect();
    for k in [2usize, 4, 8, 16].into_iter().filter(|&k| k < count.len()) {
        let mut cols: Vec<[u8; 4]> = seeds.iter().take(k).copied().collect();
        for _ in 0..2 {
            // Per centroid: the sum and count of all its pixels, and of its region pixels; under
            // the colour guard a centroid with regions moves to their mean only, so a region's
            // colour is not pulled by its anti-aliased edge pixels (near-equal regions, such as a
            // gradient's bands, still merge at their mean).
            let mut acc = vec![([0f64; 4], 0f64, [0f64; 4], 0f64); k];
            for &(q, n) in &count {
                let i = (0..k).min_by(|&a, &b| d2(q, cols[a]).total_cmp(&d2(q, cols[b]))).unwrap_or(0);
                let region = regions.contains(&q);
                for (c, &v) in q.iter().enumerate() {
                    acc[i].0[c] += f64::from(v) * n as f64;
                    if region {
                        acc[i].2[c] += f64::from(v) * n as f64;
                    }
                }
                acc[i].1 += n as f64;
                if region {
                    acc[i].3 += n as f64;
                }
            }
            for (col, (sum, n, rsum, rn)) in cols.iter_mut().zip(&acc) {
                if *rn > 0.0 {
                    *col = rsum.map(|v| (v / rn).round() as u8);
                } else if *n > 0.0 {
                    *col = sum.map(|v| (v / n).round() as u8);
                }
            }
        }
        if let Some(tol) = p.colour_tol {
            let nearest = |q: [u8; 4]| cols[(0..k).min_by(|&a, &b| d2(q, cols[a]).total_cmp(&d2(q, cols[b]))).unwrap_or(0)];
            if regions.iter().any(|&c| !within(c, nearest(c), tol)) {
                continue;
            }
        }
        out.push(build(cols));
    }
    out
}

fn rates(f: &[u16]) -> Vec<f64> {
    f.iter().map(|&v| -(f64::from(v.max(1)) / f64::from(PROB_SCALE)).log2()).collect()
}

fn hist_of<'a>(streams: impl Iterator<Item = &'a Symbols>, n: usize) -> Vec<u64> {
    let mut h = vec![0u64; n];
    streams.for_each(|s| s.iter().for_each(|&v| h[v as usize] += 1));
    h
}

/// Tile `t`'s pixels as the direct modes see them: straight RGBA8, edge-replicated; without alpha
/// every alpha is 255.
fn direct_pixels(rgba: &[u8], width: u32, height: u32, h: &FrameHeader, t: u32) -> Vec<[u8; 4]> {
    let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
    (0..TILE * TILE)
        .map(|i| {
            let (x, y) = ((tx * TILE + i % TILE).min(width - 1), (ty * TILE + i / TILE).min(height - 1));
            let s = ((y * width + x) * 4) as usize;
            [rgba[s], rgba[s + 1], rgba[s + 2], if h.alpha { rgba[s + 3] } else { 255 }]
        })
        .collect()
}

fn direct_candidates(rgba: &[u8], width: u32, height: u32, h: &FrameHeader, t: u32, p: &LmbxParams) -> Direct {
    let px = direct_pixels(rgba, width, height, h, t);
    let n = px.len() as f64;
    let flat = p.flat.then(|| {
        let mean: [u8; 4] = std::array::from_fn(|c| (px.iter().map(|q| f64::from(q[c])).sum::<f64>() / n).round() as u8);
        let d = px
            .iter()
            .map(|q| (0..3).map(|c| (f64::from(q[c]) - f64::from(mean[c])).powi(2)).sum::<f64>() + (f64::from(q[3]) - f64::from(mean[3])).powi(2))
            .sum();
        (d, mean)
    });
    // The colour guard: a flat tile must keep every region's colour.
    let regions = if p.colour_tol.is_some() { regions(&px) } else { Vec::new() };
    let flat = flat.filter(|&(_, mean)| p.colour_tol.is_none_or(|tol| regions.iter().all(|&c| within(c, mean, tol))));
    let palette = palette_candidates(&px, p, &regions);
    Direct { flat, palette, palette_pick: std::cell::Cell::new(0) }
}

/// Encode one straight-RGBA8 frame to an LMBX frame bitstream.
pub fn encode_frame(rgba: &[u8], width: u32, height: u32, p: &LmbxParams) -> Result<Vec<u8>, MediaError> {
    if !(p.quality.is_finite() && p.quality > 0.0 && p.lambda.is_finite() && p.lambda >= 0.0 && (0.0..=0.5).contains(&p.deadzone)) {
        return Err(MediaError::Invalid(format!("LMBX parameters out of range: {p:?}")));
    }
    let ratio = p.chroma_ratio.unwrap_or(if p.chroma == Chroma::Halo422 { 1.0 } else { 1.6 });
    let ratio256 = (ratio * 256.0).round().clamp(1.0, 65535.0) as u16;
    let mut s = prepare(rgba, width, height, p.chroma, ratio256)?;
    // Without tile modes the frame is a v1 frame.
    let v2 = p.flat || p.palette;
    if !v2 {
        s.header.version = dxvkit_plus_core::lmbx::VERSION;
    }
    let m = basis();
    let h = s.header;
    let qidx: Vec<Vec<usize>> = (0..h.tile_count())
        .map(|ti| {
            let (tx, ty) = (ti % h.tiles_x(), ti / h.tiles_x());
            (0..TB * TB).map(|b| pick_qidx(&m, &s.y, tx * TILE + (b % TB) * 8, ty * TILE + (b / TB) * 8, p.quality)).collect()
        })
        .collect();
    let qidx: Vec<Vec<usize>> = match p.colour_tol {
        Some(tol) => qidx.into_iter().enumerate().map(|(t, qi)| dc_guard(&s, t as u32, qi, tol)).collect(),
        None => qidx,
    };
    // Bootstrap (deadzone only) -> rate model -> RDOQ passes.
    let mut rate = vec![0f64; COEF_SYMBOLS];
    let (mut syms, mut sse) = all_symbols(&m, &s, &qidx, &rate, p, false);
    for _ in 0..p.rdo_passes {
        let f = normalize(&histogram(&syms));
        for (r, &v) in rate.iter_mut().zip(&f) {
            *r = -(f64::from(v.max(1)) / f64::from(PROB_SCALE)).log2();
        }
        (syms, sse) = all_symbols(&m, &s, &qidx, &rate, p, true);
    }
    let np = h.planes() as usize;
    let mut coef_f = normalize(&histogram(&syms));
    // v2: per-tile mode by rate-distortion (J = D + lambda * bits, lambda = the RDO multiplier
    // times the tile's mean squared step), the direct modes' tables from the tiles that use them.
    let (modes, direct) = if v2 {
        let dct_rate: Vec<f64> = coef_f.iter().map(|&v| -(f64::from(v.max(1)) / f64::from(PROB_SCALE)).log2()).collect();
        let dct_bits: Vec<f64> = (0..h.tile_count() as usize)
            .map(|t| syms[t * np..t * np + np].iter().map(|st| 32.0 + st.iter().map(|&x| dct_rate[x as usize]).sum::<f64>()).sum())
            .collect();
        let lam: Vec<f64> = qidx.iter().map(|qi| p.lambda * qi.iter().map(|&i| f64::from(QT[i]).powi(2)).sum::<f64>() / qi.len() as f64).collect();
        // Per-tile candidates, tile-parallel (each tile's are a pure function of its pixels).
        let (nt, threads) = (h.tile_count(), p.threads.clamp(1, 64) as u32);
        let per = nt.div_ceil(threads).max(1);
        let hh = &h;
        let cand: Vec<Direct> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..nt)
                .step_by(per as usize)
                .map(|start| sc.spawn(move || (start..(start + per).min(nt)).map(|t| direct_candidates(rgba, width, height, hh, t, p)).collect::<Vec<_>>()))
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        let pal_rate = rates(&normalize(&hist_of(cand.iter().flat_map(|c| c.palette.iter().map(|x| &x.syms)), dxvkit_plus_core::lmbx::PALETTE_SYMBOLS)));
        let mut modes = vec![TileMode::Dct; h.tile_count() as usize];
        for (t, c) in cand.iter().enumerate() {
            // (J, mode)
            let mut best = (sse[t] + lam[t] * dct_bits[t], TileMode::Dct);
            let mut try_mode = |d: f64, bits: f64, m: TileMode| {
                let better = d + lam[t] * bits < best.0;
                if better {
                    best = (d + lam[t] * bits, m);
                }
                better
            };
            if let Some((d, _)) = c.flat {
                let _ = try_mode(d, 32.0, TileMode::Flat);
            }
            for (k, pc) in c.palette.iter().enumerate() {
                if try_mode(pc.dist, 8.0 + 32.0 * pc.cols.len() as f64 + 32.0 + pc.syms.iter().map(|&x| pal_rate[x as usize]).sum::<f64>(), TileMode::Palette) {
                    c.palette_pick.set(k);
                }
            }
            modes[t] = best.1;
        }
        // The coefficient table from the DCT tiles only.
        let dct_streams: Vec<Symbols> =
            modes.iter().enumerate().filter(|m| *m.1 == TileMode::Dct).flat_map(|(t, _)| syms[t * np..t * np + np].to_vec()).collect();
        coef_f = normalize(&histogram(&dct_streams));
        (modes, Some(cand))
    } else {
        (vec![TileMode::Dct; h.tile_count() as usize], None)
    };
    let coef_t = FreqTable::from_freq(coef_f.clone())?;
    let mut out = h.to_bytes().to_vec();
    out.extend(FreqTable::sparse_bytes(&coef_f));
    let mut streams: Vec<Vec<u8>> = Vec::with_capacity(syms.len());
    if let Some(cand) = direct {
        let pick = |m: TileMode| modes.iter().zip(&cand).filter(move |x| *x.0 == m);
        let mut pal_h = vec![0u64; dxvkit_plus_core::lmbx::PALETTE_SYMBOLS];
        pick(TileMode::Palette).filter_map(|x| x.1.palette.get(x.1.palette_pick.get())).for_each(|pc| pc.syms.iter().for_each(|&v| pal_h[v as usize] += 1));
        let pal_f = normalize(&pal_h);
        let pal_t = FreqTable::from_freq(pal_f.clone())?;
        out.extend(FreqTable::sparse_bytes(&pal_f));
        out.extend(modes.iter().map(|&m| m as u8));
        for (t, (m, c)) in modes.iter().zip(&cand).enumerate() {
            let first = match m {
                TileMode::Dct => {
                    streams.extend(syms[t * np..t * np + np].iter().map(|st| rans_encode(st, &coef_t)));
                    continue;
                }
                TileMode::Flat => c.flat.map_or_else(Vec::new, |f| f.1.to_vec()),
                TileMode::Palette => c.palette.get(c.palette_pick.get()).map_or_else(Vec::new, |pc| {
                    let mut b = vec![pc.cols.len() as u8];
                    pc.cols.iter().for_each(|c| b.extend_from_slice(c));
                    b.extend(rans_encode(&pc.syms, &pal_t));
                    b
                }),
            };
            streams.push(first);
            streams.extend((1..np).map(|_| Vec::new()));
        }
    } else {
        streams.extend(syms.iter().map(|t| rans_encode(t, &coef_t)));
    }
    for (q, m) in qidx.iter().zip(&modes) {
        if *m == TileMode::Dct {
            out.extend(q.chunks_exact(2).map(|p| (p[0] | (p[1] << 4)) as u8));
        } else {
            out.extend([0u8; 8]);
        }
    }
    out.extend_from_slice(&h.stream_count().to_le_bytes());
    // The directory: v1 stream offsets, v2 stream lengths.
    let mut off = 0u32;
    for st in &streams {
        out.extend_from_slice(&(if v2 { st.len() as u32 } else { off }).to_le_bytes());
        off += st.len() as u32;
    }
    for st in &streams {
        out.extend_from_slice(st);
    }
    Ok(out)
}

/// Rate control: the best quality (smallest base quantiser in `1..=max_quality`, searched in
/// halves to 0.25) whose frame's stored size ([`stored_size`]) fits `budget` bytes. Returns the
/// bitstream and the quality used; if even `max_quality` does not fit, its frame.
pub fn encode_frame_to_size(rgba: &[u8], width: u32, height: u32, p: &LmbxParams, budget: usize, max_quality: f64) -> Result<(Vec<u8>, f64), MediaError> {
    let at = |q: f64| encode_frame(rgba, width, height, &LmbxParams { quality: q, ..*p });
    let size = |b: &[u8]| stored_size(b, p.zstd);
    let (mut lo, mut hi) = (1.0f64, max_quality.max(1.0));
    let first = at(lo)?;
    if size(&first) <= budget {
        return Ok((first, lo));
    }
    let mut best = (at(hi)?, hi);
    if size(&best.0) > budget {
        return Ok(best);
    }
    while hi - lo > 0.25 {
        let mid = (lo + hi) / 2.0;
        let f = at(mid)?;
        if size(&f) <= budget {
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
    let mut direct = std::collections::HashMap::new();
    for ti in 0..h.tile_count() {
        let ty = ti / h.tiles_x();
        if f.mode(ti) != TileMode::Dct {
            direct.insert(ti, dxvkit_plus_core::lmbx::direct_tile(&f, ti));
            continue;
        }
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
            if let Some(d) = direct.get(&((py / TILE) * h.tiles_x() + px / TILE)) {
                out.extend_from_slice(&d[((py % TILE) * TILE + px % TILE) as usize]);
                continue;
            }
            out.extend([rr, gg, bb, at(&a, cw, px)].map(|v| v.round().clamp(0.0, 255.0) as u8));
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

    /// v2 tile modes: a graphics card with alpha codes flat and palette tiles; exact palette tiles
    /// decode exactly; every mode off writes a v1 frame; the float reference agrees on direct
    /// tiles.
    #[test]
    fn tile_modes() {
        let (w, h) = (200u32, 120u32);
        let src = dxvkit_plus_core::testpattern::graphics(1, w, h, true);
        for chroma in [Chroma::Halo422, Chroma::Full444] {
            let b = encode_frame(&src, w, h, &LmbxParams { chroma, ..Default::default() }).unwrap();
            let f = Frame::parse(&b, w, h).unwrap();
            assert_eq!(f.header.version, dxvkit_plus_core::lmbx::VERSION_2);
            let modes: std::collections::BTreeSet<u32> = (0..f.header.tile_count()).map(|t| f.mode(t) as u32).collect();
            assert!(modes.contains(&1) && modes.contains(&2), "{modes:?}");
            let out = decode_rgba8(&b, w, h, true).unwrap();
            let float = float_reference_rgba8(&b, w, h).unwrap();
            for t in (0..f.header.tile_count()).filter(|&t| matches!(f.mode(t), TileMode::Palette | TileMode::Flat)) {
                let (tx, ty) = (t % f.header.tiles_x(), t / f.header.tiles_x());
                for y in ty * TILE..((ty + 1) * TILE).min(h) {
                    for x in tx * TILE..((tx + 1) * TILE).min(w) {
                        let i = ((y * w + x) * 4) as usize;
                        assert_eq!(out[i..i + 4], float[i..i + 4], "tile {t} at {x},{y}: integer == float on direct tiles");
                    }
                }
            }
            assert!(psnr(&src, &out) > 40.0, "{chroma:?}: {:.1} dB", psnr(&src, &out));
            let v1 = encode_frame(&src, w, h, &LmbxParams { chroma, flat: false, palette: false, ..Default::default() }).unwrap();
            assert_eq!(Frame::parse(&v1, w, h).unwrap().header.version, dxvkit_plus_core::lmbx::VERSION);
        }
    }
    /// The worst region-mean colour error (W6b): over every source colour with at least 64 pixels
    /// whose 5x5 neighbourhood is that colour (`block`: whose whole aligned block of that width x 8
    /// is — a chroma block's footprint), |mean
    /// decoded - source| of each shown channel (RGB times alpha) and of alpha, in code values; with
    /// the colour.
    fn worst_region_error(src: &[u8], out: &[u8], w: u32, h: u32, block: Option<i32>) -> (f64, [u8; 4]) {
        let at = |x: i32, y: i32| {
            let i = ((y.clamp(0, h as i32 - 1) as u32 * w + x.clamp(0, w as i32 - 1) as u32) * 4) as usize;
            [src[i], src[i + 1], src[i + 2], src[i + 3]]
        };
        let mut acc: std::collections::HashMap<[u8; 4], ([f64; 4], usize)> = std::collections::HashMap::new();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let c = at(x, y);
                let inside = match block {
                    // (4:2:2: plus the next chroma sample, which its odd last column interpolates.)
                    Some(bw) => (0..8).all(|j| (0..bw + if bw == 16 { 2 } else { 0 }).all(|i| at(x - x % bw + i, y - y % 8 + j) == c)),
                    None => (-2..=2).all(|dy| (-2..=2).all(|dx| at(x + dx, y + dy) == c)),
                };
                if inside {
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    let e = acc.entry(c).or_default();
                    (0..4).for_each(|k| e.0[k] += f64::from(out[i + k]));
                    e.1 += 1;
                }
            }
        }
        let shown = |p: [f64; 4], c: usize| p[c] * p[3] / 255.0;
        acc.into_iter()
            .filter(|(_, (_, n))| *n >= 64)
            .map(|(c, (sum, n))| {
                let m = sum.map(|v| v / n as f64);
                let s = c.map(f64::from);
                let e = (0..3).map(|k| (shown(m, k) - shown(s, k)).abs()).fold((m[3] - s[3]).abs(), f64::max);
                (e, c)
            })
            .fold((0.0, [0; 4]), |a, b| if b.0 > a.0 { b } else { a })
    }

    /// The forward YCoCg-R is the decoder's own, in quarter units, so it inverts exactly: flat
    /// fields of colours with odd `R - B` and odd `G - t` (0.6.0 decoded those 1 low in R and B at
    /// any quality) come back exactly through the DCT path at quality 1.
    #[test]
    fn dct_path_reproduces_flat_colours_exactly() {
        let mut cols = vec![[80u8, 140, 255, 255], [255, 120, 0, 255], [18, 20, 32, 255], [1, 2, 4, 255], [254, 3, 129, 255]];
        let mut r = 12345u32;
        while cols.len() < 40 {
            r = r.wrapping_mul(1_103_515_245).wrapping_add(12345);
            cols.push([(r >> 8) as u8, (r >> 16) as u8, (r >> 24) as u8, 255]);
        }
        let (w, h) = (32 * 8, 32 * 5);
        let src: Vec<u8> = (0..w * h).flat_map(|i| cols[((i / w / 32) * 8 + (i % w) / 32) as usize]).collect();
        for chroma in [Chroma::Halo422, Chroma::Full444] {
            let p = LmbxParams { chroma, quality: 1.0, flat: false, palette: false, colour_tol: None, ..Default::default() };
            let out = decode_rgba8(&encode_frame(&src, w, h, &p).unwrap(), w, h, false).unwrap();
            // 4:2:2 shares chroma across a field edge's column pair; compare away from edges.
            let bad = (0..(w * h) as usize)
                .filter(|i| chroma == Chroma::Full444 || (2..30).contains(&(i % w as usize % 32)))
                .find(|&i| out[i * 4..i * 4 + 4] != src[i * 4..i * 4 + 4]);
            assert!(bad.is_none(), "{chroma:?}: pixel {bad:?} {:?} != {:?}", bad.map(|i| &out[i * 4..i * 4 + 4]), bad.map(|i| &src[i * 4..i * 4 + 4]));
        }
    }

    /// The colour guard: on the graphics card at a coarse quality, every flat region decodes to a
    /// mean within the tolerance (1 code value; LMB7's own worst on this card is 1.02) with tile
    /// modes (v2, the default), both chroma modes, with and without alpha; DCT-only (v1) coding
    /// keeps it on every flat block (8x8; 16x8 in 4:2:2, a chroma block) — its edge blocks are the
    /// transform's. Without the guard the
    /// DCT-only frame misses by several code values (its DC lattice at the coarse step).
    #[test]
    fn colour_guard_keeps_flat_regions() {
        let (w, h) = (333u32, 185u32);
        for alpha in [false, true] {
            let src = dxvkit_plus_core::testpattern::graphics(1, w, h, alpha);
            for chroma in [Chroma::Halo422, Chroma::Full444] {
                for v2 in [false, true] {
                    let p = LmbxParams { chroma, quality: 400.0, flat: v2, palette: v2, palette_lossy: v2, ..Default::default() };
                    let out = decode_rgba8(&encode_frame(&src, w, h, &p).unwrap(), w, h, true).unwrap();
                    let block = (!v2).then_some(if chroma == Chroma::Halo422 { 16 } else { 8 });
                    let (e, c) = worst_region_error(&src, &out, w, h, block);
                    // DCT-only 4:2:2: an odd luma block's chroma is its even neighbour's, and an odd
                    // column interpolates the next chroma block's sample — the per-block guard
                    // cannot steer either (measured 1.06).
                    let tol = if !v2 && chroma == Chroma::Halo422 { 1.25 } else { 1.0 };
                    assert!(e <= tol, "alpha {alpha} {chroma:?} v2 {v2}: region {c:?} off by {e:.2}");
                    if !v2 && !alpha {
                        let off = decode_rgba8(&encode_frame(&src, w, h, &LmbxParams { colour_tol: None, ..p }).unwrap(), w, h, true).unwrap();
                        let (e, _) = worst_region_error(&src, &off, w, h, block);
                        assert!(e > 3.0, "{chroma:?}: without the guard the DC lattice shows ({e:.2})");
                    }
                }
            }
        }
    }
}
