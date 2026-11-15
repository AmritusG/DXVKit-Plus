//! W4 check: the transport's time -> frame on real frame starts equals `FrameTiming::frame_at` at
//! every frame start, mid-frame and last tick of every frame (fresh transport, speed 1, from movie
//! time 0), and how often the rate grid (the 0.10.0 transport) shows another frame. One JSON line
//! per movie (paths relative to root).
//!   cargo run --release -p dxvkit-plus-core --example transport_timing -- <root> <file>...
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
use dxvkit_plus_core::container::Movie;
use dxvkit_plus_core::time::{TICKS_PER_SECOND, Tick};
use dxvkit_plus_core::timing::MediaTime;
use dxvkit_plus_core::transport::{Clip, Inputs, Settings, Transport};
use std::sync::Arc;

fn ceil_ticks(m: MediaTime) -> Tick {
    let n = i128::from(m.value) * i128::from(TICKS_PER_SECOND);
    let ts = i128::from(m.timescale);
    Tick((n / ts + i128::from(n % ts != 0)) as i64)
}

fn shown(clip: &Clip, t: Tick) -> u32 {
    let mut tr = Transport::new(0);
    let s = Settings::default();
    // The clock advances in 5 s steps (the transport re-anchors on a jump past MAX_STEP, 10 s).
    let mut now = Tick::ZERO;
    tr.advance(now, clip, &s, &Inputs::default());
    while t.0 - now.0 > 5 * TICKS_PER_SECOND {
        now = Tick(now.0 + 5 * TICKS_PER_SECOND);
        tr.advance(now, clip, &s, &Inputs::default());
    }
    tr.advance(t, clip, &s, &Inputs::default()).frame
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for rel in &args[1..] {
        let m = match Movie::open(&std::path::Path::new(&args[0]).join(rel)) {
            Ok(m) => m,
            Err(e) => {
                println!("{}", serde_json::json!({"file": rel, "error": e.to_string()}));
                continue;
            }
        };
        let ft = Arc::new(m.tracks[0].timing.clone());
        let (clip, grid) = (Clip::from_timing(ft.clone()), Clip::new(ft.frame_count(), ft.rate));
        let n = ft.frame_count();
        let (mut checked, mut wrong, mut grid_wrong) = (0u64, 0u64, 0u64);
        for k in 0..n {
            let a = ceil_ticks(ft.time_of_frame(k));
            let b = if k + 1 < n { ceil_ticks(ft.time_of_frame(k + 1)) } else { ceil_ticks(ft.end_time()) };
            for t in [a, Tick((a.0 + b.0) / 2), Tick(b.0 - 1)] {
                let want =
                    ft.frame_at(MediaTime::new((i128::from(t.0) * i128::from(ft.timescale())).div_euclid(i128::from(TICKS_PER_SECOND)) as i64, ft.timescale()));
                checked += 1;
                wrong += u64::from(shown(&clip, t) != want);
                grid_wrong += u64::from(shown(&grid, t) != want);
            }
        }
        println!(
            "{}",
            serde_json::json!({"file": rel, "frames": n, "rate": [ft.rate.num, ft.rate.den], "table": ft.has_table(), "times_checked": checked,
                "transport_differs": wrong, "grid_transport_differs": grid_wrong})
        );
    }
}
