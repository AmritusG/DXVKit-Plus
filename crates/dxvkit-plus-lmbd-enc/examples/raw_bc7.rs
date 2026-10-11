//! Encode one PNG to a raw BC7 block stream (no container), for comparing this encoder with the
//! original `bc7enc -z<lambda> -zc<lookback>` block for block:
//!
//! ```text
//! cargo run --release -p dxvkit-plus-lmbd-enc --example raw_bc7 -- in.png out.bc7 [lambda] [lookback] [partitions]
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use dxvkit_plus_lmbd_enc::{EncodeParams, encode_bc7};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (input, output) = (&a[0], &a[1]);
    let mut p = EncodeParams::default();
    p.rdo.lambda = a.get(2).map_or(1.0, |v| v.parse().expect("lambda"));
    p.rdo.lookback = a.get(3).map_or(16384, |v| v.parse().expect("lookback"));
    p.rdo.partitions = a.get(4).map_or(1, |v| v.parse().expect("partitions"));
    let (w, h, rgba) = dxvkit_plus_core::source::load_image(std::path::Path::new(input)).expect("png");
    let t = std::time::Instant::now();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let blocks = encode_bc7(&rgba, w, h, &p, threads).expect("encode");
    std::fs::write(output, &blocks).expect("write");
    eprintln!("{input}: {w}x{h} -> {} bytes in {:.2} s", blocks.len(), t.elapsed().as_secs_f64());
}
