//! LMBX quality at a byte budget (W8; the method of W7's lab): CC0 synthetic contents, centre
//! crops, each arm rate-controlled to a stored byte budget (LMB7's bytes), PSNR (straight RGB,
//! shown = premultiplied, alpha) and the worst flat-region colour error.
//!
//! ```text
//! lmbx_quality synth <dir>                                  # alpha / film_grain / detail PNGs
//! lmbx_quality crop <in.png> <out.png> <w> <h>              # centre crop
//! lmbx_quality ab <name> <ref.png> <budget> <arm>...        # arms: base, 422, noapal, exact
//! lmbx_quality match <name> <ref.png> rgb|shown <dB> <arm>... # bytes to reach a PSNR
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use dxvkit_plus_core::source;
use dxvkit_plus_lmbd_enc::lmbx::{LmbxParams, frame_payload};

/// Shown (premultiplied) squared error summed over RGB, and alpha squared error, of one pixel.
fn px_err(p: &[u8], q: &[u8]) -> (f64, f64) {
    let (pa, qa) = (f64::from(p[3]) / 255.0, f64::from(q[3]) / 255.0);
    let s = (0..3).map(|k| (f64::from(p[k]) * pa - f64::from(q[k]) * qa).powi(2)).sum::<f64>();
    (s, (f64::from(p[3]) - f64::from(q[3])).powi(2))
}

pub fn psnr(se: f64, n: f64) -> f64 {
    if se == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 * n / se).log10() }
}

pub fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let f = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(rgba).unwrap();
    wr.finish().unwrap();
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// CC0 synthetic contents (re-created after W5's descriptions; W5's scratch generator is gone):
/// alpha = testpattern plasma with alpha; film_grain = gradient + gaussian grain sigma 8; detail =
/// fine rings, a 3-px checker and random blue. 960x540, one PNG each.
fn synth(dir: &str) {
    let (w, h) = (960u32, 540u32);
    let mut rng = 0x5EED_u64;
    let mut gauss = || {
        let u1 = (splitmix(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
        let u2 = (splitmix(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
        (-2.0 * u1.max(1e-300).ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    };
    let alpha = dxvkit_plus_core::testpattern::plasma(1, w, h, true);
    let mut grain = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let base = [40.0 + 180.0 * f64::from(x) / f64::from(w), 60.0 + 120.0 * f64::from(y) / f64::from(h), 160.0 - 80.0 * f64::from(x) / f64::from(w)];
            let n = gauss() * 8.0;
            grain.extend(base.map(|b| (b + n).round().clamp(0.0, 255.0) as u8));
            grain.push(255);
        }
    }
    let mut detail = Vec::with_capacity((w * h * 4) as usize);
    let mut r2 = 0xD37A_u64;
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (f64::from(x) - 300.0, f64::from(y) - 270.0);
            let ring = (((dx * dx + dy * dy).sqrt() * 1.3).sin() * 0.5 + 0.5) * 255.0;
            let chk = if ((x / 3) + (y / 3)) % 2 == 0 { 230.0 } else { 25.0 };
            let blue = (splitmix(&mut r2) & 255) as f64;
            let px = if x < 600 { [ring, ring * 0.8, 255.0 - ring] } else { [chk, chk, blue] };
            detail.extend(px.map(|v| v.round().clamp(0.0, 255.0) as u8));
            detail.push(255);
        }
    }
    for (name, px) in [("alpha", alpha), ("film_grain", grain), ("detail", detail)] {
        std::fs::create_dir_all(format!("{dir}/{name}")).unwrap();
        write_png(&format!("{dir}/{name}/f00000.png"), w, h, &px);
    }
}

/// Centre crop of a PNG to `w x h`.
fn crop(inp: &str, out: &str, cw: u32, ch: u32) {
    let (w, h, r) = source::load_image(std::path::Path::new(inp)).unwrap();
    let (x0, y0) = ((w - cw) / 2, (h - ch) / 2);
    let px: Vec<u8> = (0..ch)
        .flat_map(|y| {
            let s = (((y0 + y) * w + x0) * 4) as usize;
            r[s..s + (cw * 4) as usize].to_vec()
        })
        .collect();
    write_png(out, cw, ch, &px);
}

/// The worst flat-region error (W6b): over source colours with at least 2000 pixels whose 7x7
/// neighbourhood is that colour, the largest |mean decoded - source| of a shown channel or alpha.
pub fn colour_err(src: &[u8], out: &[u8], w: u32, h: u32) -> f64 {
    let at = |x: i32, y: i32| {
        let i = ((y.clamp(0, h as i32 - 1) as u32 * w + x.clamp(0, w as i32 - 1) as u32) * 4) as usize;
        [src[i], src[i + 1], src[i + 2], src[i + 3]]
    };
    let mut acc: std::collections::HashMap<[u8; 4], ([f64; 4], usize)> = std::collections::HashMap::new();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let c = at(x, y);
            if (-3..=3).all(|dy| (-3..=3).all(|dx| at(x + dx, y + dy) == c)) {
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                let e = acc.entry(c).or_default();
                (0..4).for_each(|k| e.0[k] += f64::from(out[i + k]));
                e.1 += 1;
            }
        }
    }
    let shown = |p: [f64; 4], c: usize| p[c] * p[3] / 255.0;
    acc.into_iter()
        .filter(|(_, (_, n))| *n >= 2000)
        .map(|(c, (sum, n))| {
            let m = sum.map(|v| v / n as f64);
            let s = c.map(f64::from);
            (0..3).map(|k| (shown(m, k) - shown(s, k)).abs()).fold((m[3] - s[3]).abs(), f64::max)
        })
        .fold(0.0, f64::max)
}

pub struct Score {
    pub rgb: f64,
    pub shown: f64,
    pub a: f64,
}

pub fn score(r: &[u8], d: &[u8]) -> Score {
    let n = (r.len() / 4) as f64;
    let (mut se, mut ss, mut sa) = (0f64, 0f64, 0f64);
    for (p, q) in r.chunks_exact(4).zip(d.chunks_exact(4)) {
        se += (0..3).map(|k| (f64::from(p[k]) - f64::from(q[k])).powi(2)).sum::<f64>();
        let (s, a) = px_err(p, q);
        ss += s;
        sa += a;
    }
    Score { rgb: psnr(se, 3.0 * n), shown: psnr(ss, 3.0 * n), a: psnr(sa, n) }
}

/// Rate control for the A/B: the best quality (finest base quantiser in 1..=600, bisected to
/// 1/64 on a log scale) whose stored frame fits `budget`; every probe is kept, and the best fitting
/// probe (the largest that fits — the size is not monotonic in the quality) wins.
pub fn fit(r: &[u8], w: u32, h: u32, p: &LmbxParams, budget: usize) -> (Vec<u8>, f64) {
    let at = |q: f64| dxvkit_plus_lmbd_enc::lmbx::encode_frame(r, w, h, &LmbxParams { quality: q, ..*p }).unwrap();
    let size = |b: &[u8]| dxvkit_plus_lmbd_enc::lmbx::stored_size(b, p.zstd);
    let mut best: Option<(Vec<u8>, f64, usize)> = None;
    let mut consider = |b: Vec<u8>, q: f64| {
        let n = size(&b);
        if n <= budget && best.as_ref().is_none_or(|x| n > x.2) {
            best = Some((b, q, n));
        }
        n <= budget
    };
    if consider(at(1.0), 1.0) {
        let b = best.unwrap();
        return (b.0, b.1);
    }
    let (mut lo, mut hi) = (0.0f64, 600f64.ln());
    if !consider(at(600.0), 600.0) {
        return (at(600.0), 600.0);
    }
    while hi - lo > 1.0 / 64.0 {
        let mid = (lo + hi) / 2.0;
        if consider(at(mid.exp()), mid.exp()) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let b = best.unwrap();
    (b.0, b.1)
}

/// `ab <content-name> <ref.png> <budget> <arm>...`: each arm rate-controlled to the budget (stored
/// bytes, as the CLI's `--frame-bytes`); prints payload bytes and PSNRs.
fn ab(name: &str, rp: &str, budget: usize, arms: &[String]) {
    let (w, h, r) = source::load_image(std::path::Path::new(rp)).unwrap();
    let header = dxvkit_plus_core::lmbd::Header::new_lmbx(w, h, 30, 1);
    for a in arms {
        let p = arm(a);
        let t = std::time::Instant::now();
        let (blob, q) = if budget == 0 { (dxvkit_plus_lmbd_enc::lmbx::encode_frame(&r, w, h, &p).unwrap(), p.quality) } else { fit(&r, w, h, &p, budget) };
        let secs = t.elapsed().as_secs_f64();
        let payload = frame_payload(&header, &blob, p.zstd).unwrap().len();
        let d = dxvkit_plus_core::lmbx::decode_rgba8(&blob, w, h, true).unwrap();
        let sc = score(&r, &d);
        let col = colour_err(&r, &d, w, h);
        println!(
            "{name:13} {a:24} budget {budget:7} bytes {payload:7} q {q:7.2} RGB {:7.3} shown {:7.3} A {:7.3} colour {col:5.2} enc {secs:6.2}s",
            sc.rgb, sc.shown, sc.a
        );
    }
}

/// `match <name> <ref.png> <metric rgb|shown> <target dB> <arm>...`: the fewest stored bytes at which
/// the arm reaches the target (budget bisection, each probe rate-controlled with `fit`).
fn match_(name: &str, rp: &str, metric: &str, target: f64, arms: &[String]) {
    let (w, h, r) = source::load_image(std::path::Path::new(rp)).unwrap();
    let header = dxvkit_plus_core::lmbd::Header::new_lmbx(w, h, 30, 1);
    for a in arms {
        let p = arm(a);
        let probe = |budget: usize| {
            let (blob, _) = fit(&r, w, h, &p, budget);
            let d = dxvkit_plus_core::lmbx::decode_rgba8(&blob, w, h, true).unwrap();
            let sc = score(&r, &d);
            (if metric == "rgb" { sc.rgb } else { sc.shown }, frame_payload(&header, &blob, p.zstd).unwrap().len())
        };
        let (mut lo, mut hi) = (200usize, 4 * (w * h) as usize);
        if probe(hi).0 < target {
            println!("{name:13} {a:40} never reaches {target:.2} dB {metric}");
            continue;
        }
        while hi - lo > 8 {
            let mid = (lo + hi) / 2;
            if probe(mid).0 >= target {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        let (v, bytes) = probe(hi);
        println!("{name:13} {a:40} reaches {target:.2} dB {metric} at {bytes:7} B ({v:.2} dB)");
    }
}

/// An arm: `base` (the defaults, Zstandard 19) and modifiers joined by `+`.
fn arm(spec: &str) -> LmbxParams {
    let mut p = LmbxParams { zstd: Some(19), threads: 12, ..LmbxParams::default() };
    for k in spec.split('+') {
        match k {
            "base" => {}
            "422" => p.chroma = dxvkit_plus_core::lmbx::Chroma::Halo422,
            "noapal" => p.alpha_palette = false,
            "exact" => p.palette_lossy = false,
            // W9 prototype: BC7 tiles (v4) with the BC7 encoder's RDO lambda, uber level, rate multiplier.
            l if l.starts_with("bc7l") => p.bc7_tiles = Some(l[4..].parse().unwrap()),
            l if l.starts_with("bc7u") => p.bc7_uber = l[4..].parse().unwrap(),
            l if l.starts_with("bc7m") => p.bc7_bits_mult = l[4..].parse().unwrap(),
            l if l.starts_with("bc7p") => p.bc7_partitions = l[4..].parse().unwrap(),
            l if l.starts_with("q=") => p.quality = l[2..].parse().unwrap(),
            other => panic!("unknown arm modifier {other}"),
        }
    }
    p
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "synth" => synth(&a[2]),
        "crop" => crop(&a[2], &a[3], a[4].parse().unwrap(), a[5].parse().unwrap()),
        "ab" => ab(&a[2], &a[3], a[4].parse().unwrap(), &a[5..]),
        "match" => match_(&a[2], &a[3], &a[4], a[5].parse().unwrap(), &a[6..]),
        other => panic!("unknown command {other}"),
    }
}
