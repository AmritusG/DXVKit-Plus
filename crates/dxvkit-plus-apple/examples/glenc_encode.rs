//! GlEnc parity tool: encode a movie as Swift GlEnc's `glenc-cli` does for the VideoToolbox
//! codecs — frames from `BgraReader`, an optional `scale_bgra` resize, `AvWriter` with
//! `CoreEncoder`'s settings (ProRes: none; H.264: quality 0.6 + High profile; HEVC: quality 0.6;
//! MJPEG: quality 0.85) and the source audio from `read_pcm_s32` at its own rate.
//! `--frames N` stops after N frames (GlEnc's pipeline stops at round(duration * fps)).
//!   cargo run --release -p dxvkit-plus-apple --example glenc_encode -- <in> <out>
//!     --codec prores422|prores422hq|prores422lt|prores422proxy|prores4444|h264|hevc|mjpeg
//!     [--container mov|mp4] [--no-audio] [--bare] [--frames N] [--width W --height H --filter bilinear|lanczos]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects)]

#[cfg(target_os = "macos")]
fn main() {
    use dxvkit_plus_apple::{AvCodec, AvContainer, AvWriter, BgraReader, CompressionProps, H264Profile, ScaleFilter};
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str, value: bool| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        args.remove(i);
        Some(if value { args.remove(i) } else { String::new() })
    };
    let codec = match take("--codec", true).unwrap().as_str() {
        "prores422" => AvCodec::ProRes422,
        "prores422hq" => AvCodec::ProRes422HQ,
        "prores422lt" => AvCodec::ProRes422LT,
        "prores422proxy" => AvCodec::ProRes422Proxy,
        "prores4444" => AvCodec::ProRes4444,
        "h264" => AvCodec::H264,
        "hevc" => AvCodec::Hevc,
        "mjpeg" => AvCodec::Jpeg,
        c => panic!("codec {c}"),
    };
    let container = if take("--container", true).as_deref() == Some("mp4") { AvContainer::Mp4 } else { AvContainer::Mov };
    let no_audio = take("--no-audio", false).is_some();
    let bare = take("--bare", false).is_some(); // never pass the decoded attachments
    let frames: Option<usize> = take("--frames", true).map(|s| s.parse().unwrap());
    let size: Option<(u32, u32)> = take("--width", true).map(|w| (w.parse().unwrap(), take("--height", true).unwrap().parse().unwrap()));
    let filter = if take("--filter", true).as_deref() == Some("lanczos") { ScaleFilter::Lanczos } else { ScaleFilter::Bilinear };
    let (input, output) = (std::path::PathBuf::from(&args[0]), std::path::PathBuf::from(&args[1]));
    let props = match codec {
        AvCodec::H264 => CompressionProps { quality: Some(0.6), h264_profile: Some(H264Profile::High), ..Default::default() },
        AvCodec::Hevc => CompressionProps { quality: Some(0.6), ..Default::default() },
        AvCodec::Jpeg => CompressionProps { quality: Some(dxvkit_plus_apple::MJPEG_DEFAULT_QUALITY), ..Default::default() },
        _ => CompressionProps::default(),
    };
    let audio = if no_audio { None } else { dxvkit_plus_apple::read_pcm_s32(&input, None).unwrap().filter(|a| !a.pcm.is_empty()) };
    let mut reader = BgraReader::open(&input).unwrap();
    let (w, h) = size.unwrap_or((reader.width(), reader.height()));
    let mut writer = AvWriter::create(&output, codec, container, w, h, props, audio).unwrap();
    let mut n = 0;
    while let Some(f) = reader.next_frame().unwrap() {
        if frames.is_some_and(|m| n >= m) {
            break;
        }
        // GlEnc passes the decoder's own buffer (with its attachments) when it does not resize, and
        // a fresh buffer without attachments when it does.
        if (f.width, f.height) == (w, h) && !bare {
            writer.append_with_attachments(&f.data, f.pts_value, f.pts_timescale, f.attachments.as_ref()).unwrap();
        } else {
            let data = if (f.width, f.height) == (w, h) { f.data } else { dxvkit_plus_apple::scale_bgra(&f.data, f.width, f.height, w, h, filter).unwrap() };
            writer.append(&data, f.pts_value, f.pts_timescale).unwrap();
        }
        n += 1;
    }
    let warning = writer.finish().unwrap();
    println!("frames={n} warning={warning:?}");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
