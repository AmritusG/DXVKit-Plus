//! W3 cross-test: one JSON line per movie with the frame timing of its first video track
//! (`crate::timing`): rate, rule, frame count, sample count and every frame start as an exact
//! [value, timescale]. Compared with Swift Glance's FrameTiming by `scripts/crosstest-frame-timing.py`.
//!
//! `cargo run --release -p dxvkit-plus-core --example frame_times -- <root> <relative path>...`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use dxvkit_plus_core::container::Movie;
use serde_json::json;
use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("usage: frame_times <root> <file>...");
    for rel in args {
        let line = match Movie::open(&Path::new(&root).join(&rel)) {
            Ok(m) => {
                let t = &m.first().expect("a movie has a track").timing;
                json!({
                    "file": rel,
                    "rate": [t.rate.num, t.rate.den],
                    "source": t.source,
                    "frame_count": t.frame_count(),
                    "samples": m.first().map(|v| v.frame_count()),
                    "table": t.has_table(),
                    "times": t.starts().map(|s| [s.value, i64::from(s.timescale)]).collect::<Vec<_>>(),
                })
            }
            Err(e) => json!({"file": rel, "error": e.to_string()}),
        };
        println!("{line}");
    }
}
