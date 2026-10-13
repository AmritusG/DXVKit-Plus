//! The LMBX bitstream v3 writer (W8): per tile, the rate-distortion choice between the DCT coding
//! and the direct candidates — flat; palettes of up to 64 colours with context-coded indices
//! (exact palettes up to 64 colours, lossy ones up to 16); the same with exact edge alpha; quad
//! tiles (four 16x16 sub-tiles, flat or palette each); and DCT colour with an alpha palette. The
//! decoder is `dxvkit_plus_core::lmbx` (version 3).
//
// Lint note: tile-local arithmetic on pixel indices below 1024 and palette indices below 64, and
// indexing into arrays sized by those bounds; relaxed for this module only.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use crate::lmbx::{LmbxParams, QT, Source, Symbols, direct_pixels, histogram, normalize, normalize_bits, rans_encode, rates, rates_bits, regions, within};
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::lmbx::{ALPHA_SYMBOLS, AlphaMode, FreqTable, IDX_BITS, PAL3_CTX, PAL3_MAX, PAL3_SYMBOLS, RANS_L, TILE, TileMode, VERSION_3, VERSION_4};

type Px = [u8; 4];
/// The edge-alpha table's id in a candidate's `(table, symbol)` list (index tables are `0..8`).
const ETAB: u8 = PAL3_CTX as u8;
/// Lossy palette sizes tried (larger palettes are exact only: lossy ones above 16 colours cost
/// glow 0.88 dB in W7).
const LOSSY_KS: [usize; 7] = [2, 3, 4, 6, 8, 12, 16];

/// One direct candidate of a tile (its stream-0 payload).
struct Cand {
    /// The mode byte: colour mode | alpha mode << 4.
    mode: u8,
    /// Bytes before the rANS stream (flat colours, palette headers).
    head: Vec<u8>,
    /// `(table, symbol)` of the rANS stream, in decode order.
    syms: Vec<(u8, u16)>,
    /// Squared RGBA error.
    dist: f64,
    /// What the decision charges for `head`.
    head_bits: f64,
}

/// How a region of a direct tile is coded.
enum Kind {
    Flat(Px),
    Pal(Vec<Px>),
}

/// A coded candidate before the colour guard: head bytes, symbols, head bits, decoded pixels.
type Built = (Vec<u8>, Vec<(u8, u16)>, f64, Vec<Px>);

/// Squared RGBA distance.
fn d2(a: Px, b: Px) -> f64 {
    (0..4).map(|c| (f64::from(a[c]) - f64::from(b[c])).powi(2)).sum()
}

fn partial(c: Px) -> bool {
    c[3] != 0 && c[3] != 255
}

/// The context-coded symbol of index `i` of a `size x size` region (the decoder's `index3`).
fn index_sym(idx: &[usize], size: usize, i: usize, n: usize) -> (u8, u16) {
    let (x, y) = (i % size, i / size);
    let v = idx[i];
    let l = if x > 0 {
        idx[i - 1]
    } else if y > 0 {
        idx[i - size]
    } else {
        0
    };
    let a = if y > 0 { idx[i - size] } else { l };
    let ar = if y > 0 && x + 1 < size { idx[i - size + 1] } else { a };
    let al = if y > 0 && x > 0 { idx[i - size - 1] } else { a };
    let c = usize::from(l == a) * 4 + usize::from(a == ar) * 2 + usize::from(al == l);
    let s = if v == l {
        0
    } else if v == a {
        1
    } else {
        1 + (v + n - l) % n
    };
    (c as u8, s as u16)
}

fn nearest(q: Px, cols: &[Px]) -> usize {
    (0..cols.len()).min_by(|&a, &b| d2(q, cols[a]).total_cmp(&d2(q, cols[b]))).unwrap_or(0)
}

/// Code regions (`origin`, `size`, kind) of a tile: head bytes, symbols in decode order, the
/// decoded pixels. `map` is what is mapped to a palette (the pixels, or their canonical form);
/// with `edge`, every pixel whose colour has partial alpha takes a residual to the source alpha.
fn build(src: &[Px], map: &[Px], regs: &[(usize, usize, Kind)], quad: bool, edge: bool) -> Built {
    let tl = TILE as usize;
    let (mut head, mut syms, mut head_bits) = (Vec::new(), Vec::new(), 0f64);
    let mut dec = vec![[0u8; 4]; tl * tl];
    for (origin, size, kind) in regs {
        let at = |i: usize| origin + (i / size) * tl + i % size;
        let mut put = |syms: &mut Vec<(u8, u16)>, i: usize, c: Px| {
            let mut c = c;
            if edge && partial(c) {
                syms.push((ETAB, u16::from(src[at(i)][3].wrapping_sub(c[3]))));
                c[3] = src[at(i)][3];
            }
            dec[at(i)] = c;
        };
        match kind {
            Kind::Flat(c) => {
                if quad {
                    head.push(0);
                    head_bits += 2.0;
                }
                head.extend_from_slice(c);
                head_bits += 32.0;
                (0..size * size).for_each(|i| put(&mut syms, i, *c));
            }
            Kind::Pal(cols) => {
                if quad {
                    head.push(1);
                    head_bits += 2.0;
                }
                let idx: Vec<usize> = (0..size * size).map(|i| nearest(map[at(i)], cols)).collect();
                head.push(cols.len() as u8);
                cols.iter().for_each(|c| head.extend_from_slice(c));
                head_bits += 8.0 + 32.0 * cols.len() as f64;
                for i in 0..size * size {
                    syms.push(index_sym(&idx, *size, i, cols.len()));
                    put(&mut syms, i, cols[idx[i]]);
                }
            }
        }
    }
    (head, syms, head_bits, dec)
}

/// A candidate from its coded regions, unless it breaks the colour guard (W6b: every region's
/// decoded mean within the tolerance).
fn finish(src: &[Px], built: Built, mode: TileMode, alpha: AlphaMode, p: &LmbxParams, regs: &[Px]) -> Option<Cand> {
    let (head, syms, head_bits, dec) = built;
    let dist = src.iter().zip(&dec).map(|(&s, &d)| d2(s, d)).sum();
    if let Some(tol) = p.colour_tol {
        for &c in regs {
            let (mut acc, mut n) = ([0f64; 4], 0f64);
            for (s, d) in src.iter().zip(&dec) {
                if *s == c {
                    (0..4).for_each(|k| acc[k] += f64::from(d[k]));
                    n += 1.0;
                }
            }
            let m: Px = acc.map(|v| (v / n.max(1.0)).round() as u8);
            if !within(c, m, tol) {
                return None;
            }
        }
    }
    Some(Cand { mode: mode as u8 | (alpha as u8) << 4, head, syms, dist, head_bits })
}

/// Distinct colours with their counts, most frequent first (ties: first appearance).
fn counts(px: &[Px]) -> Vec<(Px, usize)> {
    let mut count: Vec<(Px, usize)> = Vec::new();
    let mut at: std::collections::HashMap<Px, usize> = std::collections::HashMap::new();
    for q in px {
        let k = *at.entry(*q).or_insert_with(|| {
            count.push((*q, 0));
            count.len() - 1
        });
        count[k].1 += 1;
    }
    count.sort_by_key(|c| std::cmp::Reverse(c.1));
    count
}

/// `k` colours by two Lloyd iterations, seeded by the regions then by frequency; a centroid
/// holding regions moves to their mean only (W6b), so a region's colour is not pulled by its
/// anti-aliased edge.
fn lloyd(count: &[(Px, usize)], regs: &[Px], k: usize) -> Vec<Px> {
    let seeds: Vec<Px> = count.iter().filter(|c| regs.contains(&c.0)).chain(count.iter().filter(|c| !regs.contains(&c.0))).map(|c| c.0).collect();
    let mut cols: Vec<Px> = seeds.iter().take(k).copied().collect();
    for _ in 0..2 {
        let mut acc = vec![([0f64; 4], 0f64, [0f64; 4], 0f64); cols.len()];
        for &(q, n) in count {
            let i = nearest(q, &cols);
            let region = regs.contains(&q);
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
    cols
}

/// Palettes for `map`: the exact one when it has at most 64 colours, and with `palette_lossy`
/// lossy ones of up to 16 colours.
fn palettes(map: &[Px], p: &LmbxParams, regs: &[Px]) -> Vec<Vec<Px>> {
    let count = counts(map);
    let mut out = Vec::new();
    if count.len() <= PAL3_MAX {
        out.push(count.iter().map(|c| c.0).collect());
    }
    if p.palette_lossy {
        for k in LOSSY_KS.into_iter().filter(|&k| k < count.len()) {
            out.push(lloyd(&count, regs, k));
        }
    }
    out
}

fn mean(px: &[Px]) -> Px {
    std::array::from_fn(|c| (px.iter().map(|q| f64::from(q[c])).sum::<f64>() / px.len() as f64).round() as u8)
}

/// Every partial alpha as 128: palettes of this form keep partial-alpha pixels on partial
/// entries, whose edge-alpha residual then makes the alpha exact.
fn canonical(src: &[Px]) -> Vec<Px> {
    src.iter().map(|q| if partial(*q) { [q[0], q[1], q[2], 128] } else { *q }).collect()
}

/// Every direct candidate of tile `t`.
fn candidates(rgba: &[u8], width: u32, height: u32, s: &Source, t: u32, p: &LmbxParams, lam: f64, bc7: Option<&[u8]>) -> Vec<Cand> {
    let h = &s.header;
    let src = direct_pixels(rgba, width, height, h, t);
    let tl = TILE as usize;
    let regs = if p.colour_tol.is_some() { regions(&src) } else { Vec::new() };
    let mut out = Vec::new();
    // W9 prototype: the tile's 64 BC7 blocks (from the frame's BC7, coded size, raster blocks).
    if let Some(bc) = bc7 {
        let (tx, ty) = (t % h.tiles_x(), t / h.tiles_x());
        let bw = (h.tiles_x() * 8) as usize;
        let mut head = Vec::with_capacity(1024);
        let mut dec = vec![[0u8; 4]; tl * tl];
        for b in 0..64usize {
            let (bx, by) = (tx as usize * 8 + b % 8, ty as usize * 8 + b / 8);
            let blk = &bc[(by * bw + bx) * 16..(by * bw + bx) * 16 + 16];
            head.extend_from_slice(blk);
            for (k, c) in dxvkit_plus_core::lmbx_bc7_block(blk).iter().enumerate() {
                dec[((b / 8) * 4 + k / 4) * tl + (b % 8) * 4 + k % 4] = *c;
            }
        }
        if !h.alpha {
            dec.iter_mut().for_each(|q| q[3] = 255);
        }
        let bits = 8.0 * zstd::bulk::compress(&head, 3).map_or(1024, |z| z.len()) as f64 * p.bc7_bits_mult;
        if let Some(c) = finish(&src, (head, Vec::new(), bits, dec), TileMode::Bc7, AlphaMode::Own, p, &regs) {
            out.push(c);
        }
    }
    let whole = |kind: Kind| vec![(0usize, tl, kind)];
    let mut push = |rs: Vec<(usize, usize, Kind)>, map: &[Px], mode: TileMode, alpha: AlphaMode| {
        if let Some(c) = finish(&src, build(&src, map, &rs, mode == TileMode::Quad, alpha == AlphaMode::Edge), mode, alpha, p, &regs) {
            out.push(c);
        }
    };
    let has_partial = h.alpha && src.iter().any(|&q| partial(q));
    if p.flat {
        push(whole(Kind::Flat(mean(&src))), &src, TileMode::Flat, AlphaMode::Own);
    }
    if !p.palette {
        return out;
    }
    for cols in palettes(&src, p, &regs) {
        push(whole(Kind::Pal(cols)), &src, TileMode::Palette, AlphaMode::Own);
    }
    let canon = canonical(&src);
    if has_partial {
        for cols in palettes(&canon, p, &regs) {
            push(whole(Kind::Pal(cols)), &canon, TileMode::Palette, AlphaMode::Edge);
        }
    }
    // Quad: per 16x16 sub-tile the best of flat and its palettes (approximate rates).
    for alpha in [AlphaMode::Own, AlphaMode::Edge] {
        if alpha == AlphaMode::Edge && !has_partial {
            continue;
        }
        let map: &[Px] = if alpha == AlphaMode::Edge { &canon } else { &src };
        let mut rs = Vec::new();
        for q in 0..4usize {
            let origin = (q / 2) * 16 * tl + (q % 2) * 16;
            let sub: Vec<Px> = (0..256).map(|i| map[origin + (i / 16) * tl + i % 16]).collect();
            let sub_src: Vec<Px> = (0..256).map(|i| src[origin + (i / 16) * tl + i % 16]).collect();
            let sregs: Vec<Px> = regs.iter().filter(|c| sub_src.contains(c)).copied().collect();
            let mut opts: Vec<Kind> = vec![Kind::Flat(mean(&sub))];
            opts.extend(palettes(&sub, p, &sregs).into_iter().map(Kind::Pal));
            let best = opts
                .into_iter()
                .map(|k| {
                    let (d, bits) = match &k {
                        Kind::Flat(c) => (sub_src.iter().map(|&s| d2(s, *c)).sum::<f64>(), 32.0),
                        Kind::Pal(cols) => {
                            let (mut changes, mut d, mut prev) = (0usize, 0f64, usize::MAX);
                            for (i, q) in sub.iter().enumerate() {
                                let k = nearest(*q, cols);
                                let mut rec = cols[k];
                                if alpha == AlphaMode::Edge && partial(rec) {
                                    rec[3] = sub_src[i][3];
                                }
                                d += d2(sub_src[i], rec);
                                changes += usize::from(k != prev);
                                prev = k;
                            }
                            (d, 8.0 + 8.0 * cols.len() as f64 + changes as f64 * (4.0 + (cols.len() as f64).log2()) + 0.3 * 256.0)
                        }
                    };
                    (d + lam * bits, k)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|x| x.1);
            if let Some(k) = best {
                rs.push((origin, 16usize, k));
            }
        }
        push(rs, map, TileMode::Quad, alpha);
    }
    out
}

/// One alpha palette of a DCT tile ([`AlphaMode::Palette`]): its A stream's head bytes, symbols,
/// alpha squared error.
type AlphaPal = (Vec<u8>, Vec<(u8, u16)>, f64);

/// DCT colour with an alpha palette: the exact levels (up to 64) and 1..16 Lloyd levels.
fn alpha_palettes(rgba: &[u8], width: u32, height: u32, s: &Source, t: u32) -> Vec<AlphaPal> {
    let src = direct_pixels(rgba, width, height, &s.header, t);
    let mut hist = [0usize; 256];
    src.iter().for_each(|q| hist[q[3] as usize] += 1);
    let mut levels: Vec<(u8, usize)> = (0..256).filter(|&a| hist[a] > 0).map(|a| (a as u8, hist[a])).collect();
    levels.sort_by_key(|x| std::cmp::Reverse(x.1));
    let mut sets: Vec<Vec<u8>> = Vec::new();
    if levels.len() <= PAL3_MAX {
        sets.push(levels.iter().map(|l| l.0).collect());
    }
    for k in [1usize, 2, 3, 4, 8, 16] {
        if k < levels.len() {
            let mut c: Vec<f64> = levels.iter().take(k).map(|l| f64::from(l.0)).collect();
            for _ in 0..4 {
                let mut acc = vec![(0f64, 0f64); k];
                for &(a, n) in &levels {
                    let i = (0..k).min_by(|&x, &y| (f64::from(a) - c[x]).abs().total_cmp(&(f64::from(a) - c[y]).abs())).unwrap_or(0);
                    acc[i].0 += f64::from(a) * n as f64;
                    acc[i].1 += n as f64;
                }
                for (ci, (sum, n)) in c.iter_mut().zip(&acc) {
                    if *n > 0.0 {
                        *ci = (sum / n).round();
                    }
                }
            }
            sets.push(c.iter().map(|&v| v as u8).collect());
        }
    }
    sets.into_iter()
        .map(|lv| {
            let idx: Vec<usize> = src.iter().map(|q| (0..lv.len()).min_by_key(|&i| (i32::from(q[3]) - i32::from(lv[i])).abs()).unwrap_or(0)).collect();
            let dist = idx.iter().zip(&src).map(|(&i, q)| (f64::from(q[3]) - f64::from(lv[i])).powi(2)).sum();
            let syms = (0..idx.len()).map(|i| index_sym(&idx, TILE as usize, i, lv.len())).collect();
            let mut head = vec![lv.len() as u8];
            head.extend_from_slice(&lv);
            (head, syms, dist)
        })
        .collect()
}

/// rANS-encode `(table, symbol)`s (as `rans_encode`, each symbol with its own table).
fn rans_multi(syms: &[(u8, u16)], tabs: &[FreqTable]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(syms.len() + 8);
    let mut x = RANS_L;
    for &(t, s) in syms.iter().rev() {
        let tb = &tabs[t as usize];
        let (f, c) = (u32::from(tb.freq[s as usize]), tb.cum[s as usize]);
        let xmax = ((RANS_L >> tb.bits) << 8) * f;
        while x >= xmax {
            out.push((x & 0xff) as u8);
            x >>= 8;
        }
        x = ((x / f) << tb.bits) + (x % f) + c;
    }
    out.extend_from_slice(&x.to_be_bytes());
    out.reverse();
    out
}

fn hist_add(v: &[(u8, u16)], hi: &mut [Vec<u64>], ha: &mut [u64]) {
    for &(tb, sy) in v {
        if tb == ETAB {
            ha[sy as usize] += 1;
        } else {
            hi[tb as usize][sy as usize] += 1;
        }
    }
}

/// The index tables' and the edge table's rates (bits per symbol) from the given symbols.
fn model<'a>(sets: impl Iterator<Item = &'a Vec<(u8, u16)>>) -> (Vec<Vec<f64>>, Vec<f64>) {
    let mut hi = vec![vec![0u64; PAL3_SYMBOLS]; PAL3_CTX];
    let mut ha = vec![0u64; ALPHA_SYMBOLS];
    sets.for_each(|v| hist_add(v, &mut hi, &mut ha));
    (hi.iter().map(|x| rates_bits(&normalize_bits(x, IDX_BITS), IDX_BITS)).collect(), rates_bits(&normalize_bits(&ha, IDX_BITS), IDX_BITS))
}

/// A tile's choice: DCT, a direct candidate, or DCT colour with an alpha palette.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Dct,
    Direct(usize),
    AlphaPal(usize),
}

/// The v3 frame: the per-tile choice, then the bitstream.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_v3(
    rgba: &[u8],
    width: u32,
    height: u32,
    p: &LmbxParams,
    s: &Source,
    qidx: &[Vec<usize>],
    syms: &[Symbols],
    sse: &[[f64; 4]],
) -> Result<Vec<u8>, MediaError> {
    let mut h = s.header;
    h.version = if p.bc7_tiles.is_some() { VERSION_4 } else { VERSION_3 };
    // W9 prototype: the frame's BC7 blocks at the coded size (edge-replicated), for BC7 tiles.
    let bc7: Option<Vec<u8>> = match p.bc7_tiles {
        Some(lambda) => {
            let (cw, ch) = h.coded_size();
            let mut padded = Vec::with_capacity((cw * ch * 4) as usize);
            for y in 0..ch {
                for x in 0..cw {
                    let i = ((y.min(height - 1) * width + x.min(width - 1)) * 4) as usize;
                    padded.extend_from_slice(&rgba[i..i + 4]);
                }
            }
            // Prototype: cached per (pixels, lambda, uber) — rate control re-encodes the same frame.
            static CACHE: std::sync::Mutex<Vec<(u64, std::sync::Arc<Vec<u8>>)>> = std::sync::Mutex::new(Vec::new());
            use std::hash::{Hash, Hasher};
            let mut hs = std::collections::hash_map::DefaultHasher::new();
            (&padded, cw, ch, lambda.to_bits(), p.bc7_uber).hash(&mut hs);
            let key = hs.finish();
            let hit = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().find(|e| e.0 == key).map(|e| e.1.clone());
            let blocks = match hit {
                Some(b) => b,
                None => {
                    let mut ep = crate::EncodeParams::default();
                    ep.rdo.lambda = lambda;
                    ep.bc7.uber_level = p.bc7_uber;
                    let b = std::sync::Arc::new(crate::encode_bc7(&padded, cw, ch, &ep, p.threads)?);
                    let mut g = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if g.len() > 8 {
                        g.remove(0);
                    }
                    g.push((key, b.clone()));
                    b
                }
            };
            Some(blocks.to_vec())
        }
        None => None,
    };
    let np = h.planes() as usize;
    let nt = h.tile_count() as usize;
    let dct_rate = rates(&normalize(&histogram(syms)));
    let sbits = |st: &Symbols| 32.0 + st.iter().map(|&x| dct_rate[x as usize]).sum::<f64>();
    let lam: Vec<f64> = qidx.iter().map(|qi| p.lambda * qi.iter().map(|&i| f64::from(QT[i]).powi(2)).sum::<f64>() / qi.len() as f64).collect();
    // Per-tile candidates, tile-parallel (each tile's are a pure function of its pixels).
    let threads = p.threads.clamp(1, 64);
    let per = nt.div_ceil(threads).max(1);
    let (cand, apal): (Vec<Vec<Cand>>, Vec<Vec<AlphaPal>>) = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .step_by(per)
            .map(|start| {
                let (lam, bc7) = (&lam, bc7.as_deref());
                sc.spawn(move || {
                    (start..(start + per).min(nt))
                        .map(|t| {
                            let ap = if p.alpha_palette && h.alpha { alpha_palettes(rgba, width, height, s, t as u32) } else { Vec::new() };
                            (candidates(rgba, width, height, s, t as u32, p, lam[t], bc7), ap)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).unzip()
    });
    // The index and edge rates: from every candidate's symbols, then from the first choice's.
    let (mut ri, mut ra) = model((0..nt).flat_map(|t| cand[t].iter().map(|c| &c.syms).chain(apal[t].iter().map(|a| &a.1))));
    let mut choice = vec![Choice::Dct; nt];
    for round in 0..2 {
        let cost = |v: &[(u8, u16)]| v.iter().map(|&(tb, sy)| if tb == ETAB { ra[sy as usize] } else { ri[tb as usize][sy as usize] }).sum::<f64>();
        for t in 0..nt {
            let dct_bits: f64 = syms[t * np..t * np + np].iter().map(sbits).sum();
            let mut best = (sse[t].iter().sum::<f64>() + lam[t] * dct_bits, Choice::Dct);
            for (i, c) in cand[t].iter().enumerate() {
                let j = c.dist + lam[t] * (c.head_bits + cost(&c.syms) + if c.syms.is_empty() { 0.0 } else { 32.0 });
                if j < best.0 {
                    best = (j, Choice::Direct(i));
                }
            }
            for (i, a) in apal[t].iter().enumerate() {
                let d = sse[t][0] + sse[t][1] + sse[t][2] + a.2;
                let bits = syms[t * np..t * np + 3].iter().map(sbits).sum::<f64>() + 8.0 * a.0.len() as f64 + cost(&a.1) + 32.0;
                if d + lam[t] * bits < best.0 {
                    best = (d + lam[t] * bits, Choice::AlphaPal(i));
                }
            }
            choice[t] = best.1;
        }
        if round == 0 {
            (ri, ra) = model((0..nt).filter_map(|t| match choice[t] {
                Choice::Dct => None,
                Choice::Direct(i) => Some(&cand[t][i].syms),
                Choice::AlphaPal(i) => Some(&apal[t][i].1),
            }));
        }
    }
    // Tables from the chosen tiles.
    let mut hi = vec![vec![0u64; PAL3_SYMBOLS]; PAL3_CTX];
    let mut ha = vec![0u64; ALPHA_SYMBOLS];
    let mut dct_streams: Vec<Symbols> = Vec::new();
    for t in 0..nt {
        match choice[t] {
            Choice::Dct => dct_streams.extend(syms[t * np..t * np + np].iter().cloned()),
            Choice::AlphaPal(i) => {
                dct_streams.extend(syms[t * np..t * np + 3].iter().cloned());
                hist_add(&apal[t][i].1, &mut hi, &mut ha);
            }
            Choice::Direct(i) => hist_add(&cand[t][i].syms, &mut hi, &mut ha),
        }
    }
    let coef_f = normalize(&histogram(&dct_streams));
    let coef_t = FreqTable::from_freq(coef_f.clone())?;
    let fi: Vec<Vec<u16>> = hi.iter().map(|x| normalize_bits(x, IDX_BITS)).collect();
    let fa = normalize_bits(&ha, IDX_BITS);
    let mut tabs: Vec<FreqTable> = fi.iter().map(|f| FreqTable::from_freq_bits(f.clone(), IDX_BITS)).collect::<Result<_, _>>()?;
    tabs.push(FreqTable::from_freq_bits(fa.clone(), IDX_BITS)?);
    let mut out = h.to_bytes().to_vec();
    out.extend(FreqTable::sparse_bytes(&coef_f));
    fi.iter().for_each(|f| out.extend(FreqTable::sparse_bytes(f)));
    out.extend(FreqTable::sparse_bytes(&fa));
    let mode_byte = |t: usize| match choice[t] {
        Choice::Dct => TileMode::Dct as u8,
        Choice::AlphaPal(_) => (AlphaMode::Palette as u8) << 4,
        Choice::Direct(i) => cand[t][i].mode,
    };
    out.extend((0..nt).map(mode_byte));
    for (t, q) in qidx.iter().enumerate() {
        if mode_byte(t) & 15 == TileMode::Dct as u8 {
            out.extend(q.chunks_exact(2).map(|q| (q[0] | (q[1] << 4)) as u8));
        } else {
            out.extend([0u8; 8]);
        }
    }
    let mut streams: Vec<Vec<u8>> = vec![Vec::new(); nt * np];
    for t in 0..nt {
        let st = &mut streams[t * np..t * np + np];
        match choice[t] {
            Choice::Dct => (0..np).for_each(|k| st[k] = rans_encode(&syms[t * np + k], &coef_t)),
            Choice::AlphaPal(i) => {
                (0..3).for_each(|k| st[k] = rans_encode(&syms[t * np + k], &coef_t));
                let a = &apal[t][i];
                st[3] = a.0.clone();
                st[3].extend(rans_multi(&a.1, &tabs));
            }
            Choice::Direct(i) => {
                let c = &cand[t][i];
                st[0] = c.head.clone();
                if !c.syms.is_empty() {
                    st[0].extend(rans_multi(&c.syms, &tabs));
                }
            }
        }
    }
    out.extend_from_slice(&h.stream_count().to_le_bytes());
    streams.iter().for_each(|st| out.extend_from_slice(&(st.len() as u32).to_le_bytes()));
    streams.iter().for_each(|st| out.extend_from_slice(st));
    Ok(out)
}
