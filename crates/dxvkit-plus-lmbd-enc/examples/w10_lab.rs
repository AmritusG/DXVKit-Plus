//! W10 lab (exploration, not shipped): adaptive transform sizes in LMBX.
//!
//! ```text
//! w10_lab crop-at <in.png> <out.png> <x> <y> <w> <h>   # crop at an offset
//! w10_lab gfx <dir>                                     # testpattern::graphics frame 1, 960x540, with and without alpha
//! w10_lab ceiling <png> [--per-size]                    # the honest B2 ceiling (below)
//! ```
//!
//! `ceiling` re-runs W9's B2 estimate (`w9_lab bsize`) closer to LMBX: all three YCoCg-R planes
//! (4:4:4, chroma step 1.6x the luma step, as the encoder), LMBX's coding — zigzag levels then an
//! end-of-block, one coefficient table for the whole frame (or one per transform size with
//! `--per-size`), the DC a residual from the previous block's in the tile stream — and the
//! signalling: a transform-map byte per tile, charged at its empirical entropy over the frame
//! (what Zstandard leaves of it). Distortion = LMBX's mode-decision weights (3 eY^2 + eCo^2 / 2 +
//! 3 eCg^2 / 4, about the RGB squared error); per tile the Lagrangian best of the 17 layouts (one
//! 32x32; per 16x16 quadrant one 16x16 or four 8x8). Deadzone quantisation (0.45, the first three
//! zigzag terms rounded). Prints (bytes, approximate RGB PSNR) per step for 8x8 only and adaptive,
//! then the gain at equal bytes (log-linear interpolation).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use dxvkit_plus_core::source;

fn load(p: &str) -> (u32, u32, Vec<u8>) {
    source::load_image(std::path::Path::new(p)).unwrap()
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

fn crop_at(inp: &str, out: &str, x0: u32, y0: u32, cw: u32, ch: u32) {
    let (w, _h, r) = load(inp);
    let px: Vec<u8> = (0..ch)
        .flat_map(|y| {
            let s = (((y0 + y) * w + x0) * 4) as usize;
            r[s..s + (cw * 4) as usize].to_vec()
        })
        .collect();
    write_png(out, cw, ch, &px);
}

/// Orthonormal DCT-II basis of size n, `m[k * n + i]`.
fn basis_n(n: usize) -> Vec<f64> {
    let mut m = vec![0f64; n * n];
    for k in 0..n {
        let c = if k == 0 { (1.0 / n as f64).sqrt() } else { (2.0 / n as f64).sqrt() };
        for i in 0..n {
            m[k * n + i] = c * ((2 * i + 1) as f64 * k as f64 * std::f64::consts::PI / (2 * n) as f64).cos();
        }
    }
    m
}

/// Zigzag order of an n x n block (JPEG's, generalised): raster index per scan position.
fn zigzag(n: usize) -> Vec<usize> {
    let mut z = Vec::with_capacity(n * n);
    for d in 0..2 * n - 1 {
        let cells: Vec<(usize, usize)> = (0..n).filter_map(|r| d.checked_sub(r).filter(|&c| c < n).map(|c| (r, c))).collect();
        // Odd diagonals run with the row increasing, even ones decreasing (8x8: 0 1 8 16 9 2 ...).
        if d % 2 == 1 {
            z.extend(cells.iter().map(|&(r, c)| r * n + c));
        } else {
            z.extend(cells.iter().rev().map(|&(r, c)| r * n + c));
        }
    }
    z
}

fn fdct(m: &[f64], n: usize, x: &[f64]) -> Vec<f64> {
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

/// One quantised block: zigzag levels (DC = 0 placeholder) and the AC squared error; the DC
/// coefficient for the chain.
struct Blk {
    lev: Vec<i32>,
    ac_se: f64,
    dc: f64,
}

fn quant(c: &[f64], zz: &[usize], q: f64) -> Blk {
    let mut lev = vec![0i32; c.len()];
    let mut se = 0f64;
    for (k, &r) in zz.iter().enumerate().skip(1) {
        let a = c[r].abs() / q;
        let l = if k < 3 { a.round() } else { (a + 0.45).floor() }.min(1023.0) as i32;
        let l = if c[r] < 0.0 { -l } else { l };
        se += (c[r] - f64::from(l) * q).powi(2);
        lev[k] = l;
    }
    Blk { lev, ac_se: se, dc: c[0] }
}

/// The 17 layouts of a tile: `0` all 8x8, bit `q` set = quadrant `q` is one 16x16, `16` one 32x32.
fn layouts() -> Vec<u8> {
    (0u8..16).chain([16]).collect()
}

/// Blocks of a layout in stream order (raster of their top-left 8x8 positions): (size, x8, y8).
fn blocks_of(layout: u8) -> Vec<(usize, usize, usize)> {
    let mut v = Vec::new();
    if layout == 16 {
        return vec![(32, 0, 0)];
    }
    for by in 0..4 {
        for bx in 0..4 {
            let q = (by / 2) * 2 + bx / 2;
            if layout & (1 << q) != 0 {
                if bx % 2 == 0 && by % 2 == 0 {
                    v.push((16, bx, by));
                }
            } else {
                v.push((8, bx, by));
            }
        }
    }
    v
}

fn ceiling(rp: &str, per_size: bool) {
    let (w, h, r) = load(rp);
    let (w32, h32) = (w / 32 * 32, h / 32 * 32);
    // YCoCg-R in code values (the encoder's, without the quarter scaling): Y centred on 128.
    let n = (w32 * h32) as usize;
    let mut planes = vec![vec![0f64; n]; 3];
    for y in 0..h32 {
        for x in 0..w32 {
            let i = ((y * w + x) * 4) as usize;
            let (rr, g, b) = (f64::from(r[i]), f64::from(r[i + 1]), f64::from(r[i + 2]));
            let co = rr - b;
            let t = b + co / 2.0;
            let cg = g - t;
            let o = (y * w32 + x) as usize;
            planes[0][o] = t + cg / 2.0 - 128.0;
            planes[1][o] = co;
            planes[2][o] = cg;
        }
    }
    let sizes = [8usize, 16, 32];
    let bases: Vec<Vec<f64>> = sizes.iter().map(|&s| basis_n(s)).collect();
    let zzs: Vec<Vec<usize>> = sizes.iter().map(|&s| zigzag(s)).collect();
    let si = |s: usize| match s {
        8 => 0,
        16 => 1,
        _ => 2,
    };
    let wgt = [3.0, 0.5, 0.75];
    let (tx, ty) = ((w32 / 32) as usize, (h32 / 32) as usize);
    let nt = tx * ty;
    let mut rows8: Vec<(f64, f64)> = Vec::new();
    let mut rows_a: Vec<(f64, f64)> = Vec::new();
    for step in [3.0f64, 6.0, 12.0, 24.0, 48.0] {
        // Per tile, plane, size: blocks (raster of the size's grid in the tile).
        let tiles: Vec<Vec<Vec<Vec<Blk>>>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..12)
                .map(|th| {
                    let (planes, bases, zzs) = (&planes, &bases, &zzs);
                    sc.spawn(move || {
                        (th..nt)
                            .step_by(12)
                            .map(|t| {
                                let (x0, y0) = ((t % tx) * 32, (t / tx) * 32);
                                (0..3)
                                    .map(|p| {
                                        let q = if p == 0 { step } else { (step * 1.6).max(2.0) };
                                        sizes
                                            .iter()
                                            .enumerate()
                                            .map(|(k, &s)| {
                                                let mut v = Vec::new();
                                                for by in (0..32).step_by(s) {
                                                    for bx in (0..32).step_by(s) {
                                                        let blk: Vec<f64> =
                                                            (0..s * s).map(|i| planes[p][(y0 + by + i / s) * w32 as usize + x0 + bx + i % s]).collect();
                                                        v.push(quant(&fdct(&bases[k], s, &blk), &zzs[k], q));
                                                    }
                                                }
                                                v
                                            })
                                            .collect()
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let per: Vec<Vec<_>> = hs.into_iter().map(|h| h.join().unwrap()).collect();
            // Re-interleave (thread th has tiles th, th + 12, ...).
            let mut its: Vec<_> = per.into_iter().map(|v| v.into_iter()).collect();
            (0..nt).map(|t| its[t % 12].next().unwrap()).collect()
        });
        // A layout's symbols and distortion for one tile: symbols as (size index, symbol).
        let code = |t: usize, layout: u8| -> (Vec<(usize, i32)>, f64) {
            let mut syms = Vec::new();
            let mut d = 0f64;
            for p in 0..3 {
                let q = if p == 0 { step } else { (step * 1.6).max(2.0) };
                let mut dc = 0f64; // previous block's reconstructed DC, at 8x8 scale
                for (s, bx, by) in blocks_of(layout) {
                    let k = si(s);
                    let per_row = 32 / s;
                    let b = &tiles[t][p][k][(by * 8 / s) * per_row + bx * 8 / s];
                    let scale = s as f64 / 8.0;
                    let qdc = q.max(2.0 * scale * if p == 0 { 1.0 } else { 2.0 });
                    let l0 = ((b.dc - dc * scale) / qdc).round();
                    let rec = dc * scale + l0 * qdc;
                    dc = rec / scale;
                    d += wgt[p] * (b.ac_se + (b.dc - rec).powi(2));
                    let mut lev = b.lev.clone();
                    lev[0] = l0 as i32;
                    let last = lev.iter().rposition(|&v| v != 0).map_or(0, |l| l + 1);
                    syms.extend(lev[..last].iter().map(|&v| (k, v.clamp(-1023, 1023))));
                    if last < s * s {
                        syms.push((k, 2047));
                    }
                }
            }
            (syms, d)
        };
        // Rate model: start from the 8x8-only symbols, choose, refit on the choice, choose again.
        let tabk = |k: usize| if per_size { k } else { 0 };
        let fit_layouts = |per_tile: &dyn Fn(usize) -> Vec<u8>| -> Vec<std::collections::HashMap<i32, f64>> {
            let mut hist = vec![std::collections::HashMap::<i32, f64>::new(); 3];
            for t in 0..nt {
                for l in per_tile(t) {
                    for (k, s) in code(t, l).0 {
                        *hist[tabk(k)].entry(s).or_default() += 1.0;
                    }
                }
            }
            hist.into_iter()
                .map(|hm| {
                    let tot: f64 = hm.values().sum();
                    hm.into_iter().map(|(s, c)| (s, -(c / tot).log2())).collect()
                })
                .collect()
        };
        let bits = |rate: &[std::collections::HashMap<i32, f64>], syms: &[(usize, i32)]| -> f64 {
            syms.iter().map(|&(k, s)| rate[tabk(k)].get(&s).copied().unwrap_or(20.0)).sum::<f64>()
        };
        let fit = |chosen: &[u8]| fit_layouts(&|t| vec![chosen[t]]);
        let lam = 0.25 * step * step;
        let all8 = vec![0u8; nt];
        let rate8 = fit(&all8);
        let (b8, d8) = (0..nt).fold((0f64, 0f64), |a, t| {
            let (s, d) = code(t, 0);
            (a.0 + bits(&rate8, &s), a.1 + d)
        });
        let mut chosen = Vec::new();
        // Bootstrap: every tile coded all-8x8, all-16x16 and as one 32x32 (so every size's symbols
        // have a rate before the first choice).
        let mut rate = fit_layouts(&|_| vec![0, 15, 16]);
        for _ in 0..3 {
            chosen = (0..nt)
                .map(|t| {
                    layouts()
                        .into_iter()
                        .map(|l| {
                            let (s, d) = code(t, l);
                            (d + lam * bits(&rate, &s), l)
                        })
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .unwrap()
                        .1
                })
                .collect();
            rate = fit(&chosen);
        }
        let (ba, da) = (0..nt).fold((0f64, 0f64), |a, t| {
            let (s, d) = code(t, chosen[t]);
            (a.0 + bits(&rate, &s), a.1 + d)
        });
        // Signalling: the transform map at its empirical entropy.
        let mut lh = std::collections::HashMap::<u8, f64>::new();
        chosen.iter().for_each(|&l| *lh.entry(l).or_default() += 1.0);
        let map_bits: f64 = lh.values().map(|&c| -c * (c / nt as f64).log2()).sum();
        let px = f64::from(w32 * h32);
        let psnr = |d: f64| 10.0 * (255.0f64 * 255.0 * 3.0 * px / d).log10();
        let (c32, c16, c8) = (
            chosen.iter().filter(|&&l| l == 16).count(),
            chosen.iter().map(|&l| if l == 16 { 0 } else { l.count_ones() as usize }).sum::<usize>(),
            chosen.iter().map(|&l| if l == 16 { 0 } else { 4 - l.count_ones() as usize }).sum::<usize>(),
        );
        println!(
            "{rp} step {step:4}: 8x8 only {:9.0} B {:6.2} dB | adaptive {:9.0} B (map {:5.0} B) {:6.2} dB | tiles 32: {c32}, quadrants 16: {c16}, 8x8-quadrants: {c8}",
            b8 / 8.0,
            psnr(d8),
            (ba + map_bits) / 8.0,
            map_bits / 8.0,
            psnr(da)
        );
        rows8.push(((b8 / 8.0).ln(), psnr(d8)));
        rows_a.push((((ba + map_bits) / 8.0).ln(), psnr(da)));
    }
    // Gain at equal bytes: adaptive's PSNR interpolated at each 8x8 point's bytes.
    let interp = |rows: &[(f64, f64)], x: f64| -> Option<f64> {
        let mut s: Vec<(f64, f64)> = rows.to_vec();
        s.sort_by(|a, b| a.0.total_cmp(&b.0));
        s.windows(2).find(|w| w[0].0 <= x && x <= w[1].0).map(|w| w[0].1 + (w[1].1 - w[0].1) * (x - w[0].0) / (w[1].0 - w[0].0))
    };
    let gains: Vec<String> =
        rows8.iter().map(|&(x, y)| interp(&rows_a, x).map_or("  n/a".into(), |v| format!("{:+.2} dB at {:.0} B", v - y, x.exp()))).collect();
    println!("{rp} gain at equal bytes (per-size tables: {per_size}): {}", gains.join(", "));
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "crop-at" => crop_at(&a[2], &a[3], a[4].parse().unwrap(), a[5].parse().unwrap(), a[6].parse().unwrap(), a[7].parse().unwrap()),
        "gfx" => {
            for (name, alpha) in [("gfx1", false), ("gfx_a1", true)] {
                write_png(&format!("{}/{name}.png", a[2]), 960, 540, &dxvkit_plus_core::testpattern::graphics(1, 960, 540, alpha));
            }
        }
        "ceiling" => ceiling(&a[2], a.iter().any(|x| x == "--per-size")),
        other => panic!("unknown command {other}"),
    }
}
