//! Display names: what a viewer calls a codec and a container, and the line it shows for a file
//! nothing plays — Swift Glance's, so the Rust family reads the same.
//!
//! - [`codec_name`]: `DXVDetector.displayName` (glance/Sources/GlanceCore/DXVDetector.swift:28-57),
//!   plus `Hap7` "HAP R" and `HapH` "HAP HDR", which only the Rust family plays (the HAP
//!   specification's own names are "Hap R" and "Hap HDR", Vidvox/hap documentation/HapVideoDRAFT.md
//!   "Names and Identifiers"; the family writes "HAP" as Swift Glance does).
//! - [`container_name`]: the file name's extension in upper case, `this container` without one
//!   (`AVPlaybackController.inspect`, glance/Sources/Glance/AVPlaybackController.swift:84-91).
//! - [`unsupported_message`]: `<codec> in <CONTAINER> is not supported`, else `<CONTAINER> files
//!   are not supported`; the codec from [`crate::container::first_video_fourcc`] or a Matroska
//!   `V_...` codec id (`matroskaCodec`, AVPlaybackController.swift:110-123).

use crate::container::first_video_fourcc;
use std::io::Read;
use std::path::Path;

/// The name for a sample-entry FourCC; `None` for a FourCC without one (show the FourCC itself:
/// [`codec_label`]).
pub fn codec_name(fourcc: &[u8]) -> Option<&'static str> {
    Some(match fourcc {
        b"DXD3" | b"DXD4" => "DXV3",
        b"DXDI" => "DXV1",
        b"rle " => "QuickTime Animation",
        b"Hap1" => "HAP",
        b"Hap5" => "HAP Alpha",
        b"HapY" => "HAP Q",
        b"HapM" => "HAP Q Alpha",
        b"HapA" => "HAP Alpha-only",
        b"Hap7" => "HAP R",
        b"HapH" => "HAP HDR",
        b"jpeg" | b"mjpa" | b"mjpb" | b"mjpg" => "Motion JPEG",
        b"apch" => "ProRes 422 HQ",
        b"apcn" => "ProRes 422",
        b"apcs" => "ProRes 422 LT",
        b"apco" => "ProRes 422 Proxy",
        b"ap4h" => "ProRes 4444",
        b"ap4x" => "ProRes 4444 XQ",
        b"avc1" | b"avc3" => "H.264",
        b"hvc1" | b"hev1" => "HEVC",
        b"av01" => "AV1",
        b"vp09" => "VP9",
        b"mp4v" => "MPEG-4",
        _ => return None,
    })
}

/// [`codec_name`], or the FourCC itself with spaces and tabs trimmed (Swift Glance's fallback).
pub fn codec_label(fourcc: &[u8]) -> String {
    codec_name(fourcc).map_or_else(|| String::from_utf8_lossy(fourcc).trim_matches([' ', '\t']).to_string(), str::to_string)
}

/// The container's name: the extension in upper case, `this container` without one.
pub fn container_name(path: &Path) -> String {
    match path.extension().map(|e| e.to_string_lossy()) {
        Some(e) if !e.is_empty() => e.to_uppercase(),
        _ => "this container".into(),
    }
}

/// The line a viewer shows for a file nothing plays (Swift Glance's words).
pub fn unsupported_message(path: &Path) -> String {
    let container = container_name(path);
    match first_video_fourcc(path).map(|cc| codec_label(&cc)).or_else(|| matroska_codec(path)) {
        Some(codec) => format!("{codec} in {container} is not supported"),
        None => format!("{container} files are not supported"),
    }
}

/// The video codec of a Matroska / WebM file (by extension, as Swift Glance decides), from the first
/// `V_...` codec id in its first MiB.
fn matroska_codec(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if !["mkv", "webm", "mk3d"].contains(&ext.as_str()) {
        return None;
    }
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(1 << 20).read_to_end(&mut head).ok()?;
    let at = head.windows(2).position(|w| w == b"V_")?;
    let id: String = head
        .get(at..)?
        .iter()
        .take(32)
        .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || **b == b'_' || **b == b'/')
        .map(|&b| char::from(b))
        .collect();
    let name = match id.as_str() {
        "V_VP8" => "VP8",
        "V_VP9" => "VP9",
        "V_AV1" => "AV1",
        "V_MPEGH/ISO/HEVC" => "HEVC",
        "V_MPEG4/ISO/AVC" => "H.264",
        "V_PRORES" => "ProRes",
        "V_THEORA" => "Theora",
        "V_MPEG2" => "MPEG-2",
        "V_MS/VFW/FOURCC" => "VfW",
        other => return other.get(2..).filter(|rest| !rest.is_empty()).map(str::to_string),
    };
    Some(name.to_string())
}
