//! Diagnose where error and bytes go (W6). For a reference PNG and a decoded frame of any
//! supported file: squared error split by the reference's alpha class (0, partial, 255) — straight
//! RGB, premultiplied ("as shown") RGB and alpha — and, for LMBX, every stream's bytes summed per
//! plane and per tile class (transparent, partial, opaque; flat / few colours / other).
//!
//! ```text
//! cargo run --release -p dxvkit-plus-core --example lmbx_diag -- <reference.png> <file> <frame>
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
use dxvkit_plus_core::source::{self, Decoded, ImportOptions};
use std::collections::BTreeMap;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (rp, fp, n): (&str, &str, u32) = (&a[1], &a[2], a[3].parse().unwrap());
    let (w, h, r) = source::load_image(std::path::Path::new(rp)).unwrap();
    let opts = ImportOptions::default();
    let info = source::probe(fp, &opts).unwrap();
    let mut src = source::open(&info, &opts, true).unwrap();
    let d = src.decode(n).unwrap();
    let dec = src.cpu_rgba(&d).unwrap();
    assert_eq!((info.width, info.height), (w, h));
    // Error by alpha class.
    let mut cls: BTreeMap<&str, (u64, f64, f64, f64)> = BTreeMap::new();
    for (p, q) in r.chunks_exact(4).zip(dec.chunks_exact(4)) {
        let c = match p[3] {
            0 => "alpha 0",
            255 => "alpha 255",
            _ => "alpha partial",
        };
        let e = cls.entry(c).or_default();
        e.0 += 1;
        let (pa, qa) = (f64::from(p[3]) / 255.0, f64::from(q[3]) / 255.0);
        for k in 0..3 {
            let s = f64::from(p[k]) - f64::from(q[k]);
            e.1 += s * s;
            let m = f64::from(p[k]) * pa - f64::from(q[k]) * qa;
            e.2 += m * m;
        }
        let s = f64::from(p[3]) - f64::from(q[3]);
        e.3 += s * s;
    }
    let total = (w * h) as f64;
    println!("{fp} frame {n}: {}x{}, codec {}", w, h, info.codec);
    for (k, (cnt, se, sp, sa)) in &cls {
        println!(
            "  {k:14} pixels {:6.2}%  RGB MSE {:8.3}  shown-RGB MSE {:8.3}  alpha MSE {:8.3}  | share of total RGB SE {:5.1}%  shown {:5.1}%",
            100.0 * *cnt as f64 / total,
            se / (3.0 * *cnt as f64),
            sp / (3.0 * *cnt as f64),
            sa / *cnt as f64,
            100.0 * se / cls.values().map(|v| v.1).sum::<f64>().max(1e-9),
            100.0 * sp / cls.values().map(|v| v.2).sum::<f64>().max(1e-9)
        );
    }
    // LMBX bytes.
    let Decoded::Planes { planes, .. } = &d else { return };
    if info.codec != "LMBX" {
        return;
    }
    let blob = &planes[0].data;
    let f = Frame::parse(blob, w, h).unwrap();
    let hd = f.header;
    let np = hd.planes() as usize;
    let fixed = f.sections.data;
    println!(
        "  bitstream {} B: header+table {} B, quantiser map {} B, directory {} B, streams {} B",
        blob.len(),
        f.sections.qmap,
        f.sections.directory - f.sections.qmap,
        fixed - f.sections.directory,
        blob.len() - fixed
    );
    let mut by_plane = vec![0usize; np];
    let mut by_class: BTreeMap<String, (usize, usize, Vec<usize>, f64)> = BTreeMap::new();
    for t in 0..hd.tile_count() as usize {
        let (tx, ty) = (t as u32 % hd.tiles_x(), t as u32 / hd.tiles_x());
        let (mut amin, mut amax, mut colours, mut se) = (255u8, 0u8, std::collections::HashSet::new(), 0f64);
        for y in ty * TILE..(ty * TILE + TILE).min(h) {
            for x in tx * TILE..(tx * TILE + TILE).min(w) {
                let i = ((y * w + x) * 4) as usize;
                amin = amin.min(r[i + 3]);
                amax = amax.max(r[i + 3]);
                se += (0..3).map(|k| (f64::from(r[i + k]) - f64::from(dec[i + k])).powi(2)).sum::<f64>();
                if colours.len() <= 16 {
                    colours.insert([r[i], r[i + 1], r[i + 2], r[i + 3]]);
                }
            }
        }
        let alpha = if amax == 0 {
            "transparent"
        } else if amin == 255 {
            "opaque"
        } else {
            "partial"
        };
        let kind = match colours.len() {
            1 => "flat",
            2..=16 => "<=16 colours",
            _ => "other",
        };
        let e = by_class.entry(format!("{alpha:11} {kind:12} {:?}", f.mode(t as u32))).or_insert((0, 0, vec![0; np], 0.0));
        e.0 += 1;
        e.3 += se;
        for k in 0..np {
            let s = f.stream(t * np + k).len();
            by_plane[k] += s;
            e.1 += s;
            e.2[k] += s;
        }
    }
    println!("  stream bytes by plane (Y, Co, Cg, A): {by_plane:?}");
    // Exact repeats of an earlier non-flat tile (edge-replicated), and their stream bytes.
    let mut seen = std::collections::HashSet::new();
    let (mut rep, mut rep_bytes) = (0, 0);
    for t in 0..hd.tile_count() {
        let (tx, ty) = (t % hd.tiles_x(), t / hd.tiles_x());
        let px: Vec<u8> = (0..TILE * TILE)
            .flat_map(|i| {
                let (x, y) = ((tx * TILE + i % TILE).min(w - 1), (ty * TILE + i / TILE).min(h - 1));
                let o = ((y * w + x) * 4) as usize;
                r[o..o + 4].to_vec()
            })
            .collect();
        if px.chunks_exact(4).all(|c| c == &px[..4]) {
            continue;
        }
        if !seen.insert(px) {
            rep += 1;
            rep_bytes += (0..np).map(|k| f.stream(t as usize * np + k).len()).sum::<usize>();
        }
    }
    println!("  non-flat tiles repeating an earlier tile: {rep}, their stream bytes {rep_bytes}");
    let se_all: f64 = by_class.values().map(|v| v.3).sum::<f64>().max(1e-9);
    for (k, (tiles, bytes, per, se)) in &by_class {
        println!(
            "  tiles {k:40} {tiles:5}  bytes {bytes:8} ({:5.1}%)  per tile {:7.1}  RGB SE {:5.1}%  by plane {per:?}",
            100.0 * *bytes as f64 / (blob.len() - fixed) as f64,
            *bytes as f64 / *tiles as f64,
            100.0 * se / se_all
        );
    }
}
