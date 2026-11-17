//! GlEnc parity tool: a movie's first audio track as `read_pcm_s32` (Swift GlEnc's
//! `readInterleavedPCM`) reads it: `rate channels bytes fnv1a64`, or "none"; `--raw <file>` writes
//! the PCM. The rate argument is `orig` or Hz.
//!   cargo run --release -p dxvkit-plus-apple --example glenc_pcm -- <movie> <orig|rate> [--raw out.s32]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rate = if args[1] == "orig" { None } else { Some(args[1].parse::<u32>().unwrap()) };
    match dxvkit_plus_apple::read_pcm_s32(std::path::Path::new(&args[0]), rate) {
        Ok(Some(a)) => {
            let h = a.pcm.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
            println!("{} {} {} {h:016x}", a.rate, a.channels, a.pcm.len());
            if args.get(2).map(String::as_str) == Some("--raw") {
                std::fs::write(&args[3], &a.pcm).unwrap();
            }
        }
        Ok(None) => println!("none"),
        Err(e) => println!("error: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
