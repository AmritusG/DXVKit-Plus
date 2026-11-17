//! GlEnc parity tool: `scale_bgra` on a raw tight 4-channel image (Swift GlEnc's `FrameResizer`
//! vImage path), written raw; `--stride-pad N` runs it with N padding bytes per source and
//! destination row instead (the output is written tight either way).
//!   cargo run --release -p dxvkit-plus-apple --example glenc_scale -- <in.raw> <w> <h> <W> <H> bilinear|lanczos <out.raw> [--stride-pad N]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[cfg(target_os = "macos")]
fn main() {
    use dxvkit_plus_apple::ScaleFilter;
    let a: Vec<String> = std::env::args().skip(1).collect();
    let src = std::fs::read(&a[0]).unwrap();
    let n = |i: usize| a[i].parse::<u32>().unwrap();
    let (w, h, dw, dh) = (n(1), n(2), n(3), n(4));
    let filter = if a[5] == "lanczos" { ScaleFilter::Lanczos } else { ScaleFilter::Bilinear };
    let pad: usize = a.get(8).map_or(0, |s| s.parse().unwrap());
    let out = if pad == 0 {
        dxvkit_plus_apple::scale_bgra(&src, w, h, dw, dh, filter).unwrap()
    } else {
        let (row, drow) = (w as usize * 4, dw as usize * 4);
        let padded: Vec<u8> = src.chunks_exact(row).flat_map(|r| r.iter().copied().chain(std::iter::repeat_n(0xAB, pad))).collect();
        let (o, stride) = dxvkit_plus_apple::scale::scale_bgra_strided(&padded, w, h, row + pad, dw, dh, drow + pad, filter).unwrap();
        o.chunks(stride).flat_map(|r| r[..drow].iter().copied()).collect()
    };
    std::fs::write(&a[6], out).unwrap();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
