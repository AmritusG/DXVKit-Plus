//! GlEnc parity tool: every frame of a movie as `BgraReader` (Swift GlEnc's AVAssetReader source
//! reader) serves it — one line per frame, `index pts_value/pts_timescale fnv1a64` of the tight
//! BGRA bytes — after a header line (size, duration, alpha facts). `--native` reads the same
//! frames through `NativeReader` (IOSurface + Metal-compatible BGRA, the playback path) instead,
//! to see whether those buffer properties change pixels. `--raw <file>` also writes the frames.
//!   cargo run --release -p dxvkit-plus-apple --example glenc_frames -- [--native] [--raw out.bgra] <movie>
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[cfg(target_os = "macos")]
fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

#[cfg(target_os = "macos")]
fn main() {
    use std::io::Write;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let native = args.iter().position(|a| a == "--native").map(|i| args.remove(i)).is_some();
    let raw = args.iter().position(|a| a == "--raw").map(|i| {
        args.remove(i);
        args.remove(i)
    });
    let path = std::path::PathBuf::from(&args[0]);
    let mut raw = raw.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    if native {
        use dxvkit_plus_core::backend::{NativeDecoder, PixelLayout};
        let mut r = dxvkit_plus_apple::NativeReader::open(&path, PixelLayout::Bgra).unwrap();
        let n = r.info().frame_count;
        println!("native {}x{} frames={n}", r.info().width, r.info().height);
        for i in 0..n {
            let f = r.frame(i).unwrap();
            let rgba = r.cpu_rgba(&f).unwrap();
            let bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
            let t = r.timing().unwrap().time_of_frame(i);
            println!("{i} {}/{} {:016x}", t.value, t.timescale, fnv(&bgra));
            if let Some(w) = raw.as_mut() {
                w.write_all(&bgra).unwrap();
            }
        }
        return;
    }
    let mut r = dxvkit_plus_apple::BgraReader::open(&path).unwrap();
    let a = r.alpha();
    println!(
        "bgra {}x{} duration={} contains_alpha={:?} premultiplied={} codec={}",
        r.width(),
        r.height(),
        r.duration_seconds(),
        a.contains_alpha_channel,
        a.premultiplied,
        String::from_utf8_lossy(&a.codec)
    );
    let mut i = 0;
    while let Some(f) = r.next_frame().unwrap() {
        println!("{i} {}/{} {:016x}", f.pts_value, f.pts_timescale, fnv(&f.data));
        if let Some(w) = raw.as_mut() {
            w.write_all(&f.data).unwrap();
        }
        i += 1;
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
