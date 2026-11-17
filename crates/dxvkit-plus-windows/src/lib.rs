//! # dxvkit-plus-windows — Media Foundation decode for DXVKit Plus (0.13.0)
//!
//! [`MediaFoundationBackend`] opens a movie's first video track with an `IMFSourceReader`. Decode is hardware
//! accelerated: the reader gets a DXGI device manager over a D3D11 device made for video
//! (`MF_SOURCE_READER_D3D_MANAGER`, `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`), and the video processor converts to
//! 32-bit RGB (`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`). Frames are handed over as straight RGBA8
//! ([`dxvkit_plus_core::backend::CpuFrame`]): this first reader reads each frame back once; the zero-copy import
//! (shared NT handle → D3D12 `OpenSharedHandle` → wgpu `create_texture_from_hal`, `backend/windows.rs`'s design) is the
//! next step and changes nothing for callers.
//!
//! Codecs are what Windows itself decodes: H.264 and MJPEG always; HEVC when Microsoft's HEVC Video Extensions are
//! installed; VP9 / AV1 when their (free) extensions are; **never ProRes** (Windows has no ProRes decoder, and the
//! programme adds none until Apple answers). A file Windows cannot decode is refused at open with a sentence that says
//! why, never a black picture.
//!
//! Frame-exact seek, as the macOS reader: `SetCurrentPosition` to the target minus half a frame, then read and drop
//! samples until the target's presentation time; reading on from the last frame needs no seek.

#[cfg(windows)]
mod mf;

#[cfg(windows)]
pub use mf::MediaFoundationBackend;

/// The sentence for a codec Windows cannot decode (also used by the tests on every OS).
pub fn missing_decoder_message(codec: &str) -> String {
    match codec {
        "HEVC" => "HEVC needs Microsoft's HEVC Video Extensions (Microsoft Store) on this PC".into(),
        "ProRes" => "ProRes cannot play on Windows (Windows has no ProRes decoder)".into(),
        "VP9" => "VP9 needs Microsoft's VP9 Video Extensions (Microsoft Store) on this PC".into(),
        "AV1" => "AV1 needs Microsoft's AV1 Video Extension (Microsoft Store) on this PC".into(),
        other => format!("{other}: Windows has no decoder for it on this PC"),
    }
}

/// A codec's name from a Media Foundation subtype's FourCC (the first four bytes of the GUID).
pub fn codec_name(fourcc: u32) -> String {
    let b = fourcc.to_le_bytes();
    match &b {
        b"H264" | b"h264" | b"avc1" => "H.264".into(),
        b"HEVC" | b"hev1" | b"hvc1" | b"H265" => "HEVC".into(),
        b"apcn" | b"apch" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "ProRes".into(),
        b"VP90" | b"vp09" => "VP9".into(),
        b"AV01" | b"av01" => "AV1".into(),
        b"MJPG" => "MJPEG".into(),
        _ => {
            let s: String = b.iter().map(|c| if c.is_ascii_graphic() { *c as char } else { '?' }).collect();
            format!("'{s}'")
        }
    }
}

/// BGRX / BGRA rows (any pitch, top-down or bottom-up) to tight straight RGBA8, alpha forced to 255.
pub fn bgrx_to_rgba(src: &[u8], width: usize, height: usize, pitch: isize, first_row: usize) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    let mut out = Vec::with_capacity(row_bytes.checked_mul(height)?);
    for y in 0..height {
        let start = (first_row as isize).checked_add(pitch.checked_mul(y as isize)?)?;
        let start = usize::try_from(start).ok()?;
        let row = src.get(start..start.checked_add(row_bytes)?)?;
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&[*px.get(2)?, *px.get(1)?, *px.first()?, 255]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_convert_in_either_direction_and_short_input_is_refused() {
        // 2 x 2, pitch 12 (4 bytes of padding per row), BGRX.
        let src = [1, 2, 3, 9, 4, 5, 6, 9, 0, 0, 0, 0, 7, 8, 9, 9, 10, 11, 12, 9, 0, 0, 0, 0];
        let down = bgrx_to_rgba(&src, 2, 2, 12, 0);
        assert_eq!(down, Some(vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]));
        // Bottom-up: the first row is the last in memory, the pitch negative.
        let up = bgrx_to_rgba(&src, 2, 2, -12, 12);
        assert_eq!(up, Some(vec![9, 8, 7, 255, 12, 11, 10, 255, 3, 2, 1, 255, 6, 5, 4, 255]));
        // The last row's padding is not needed; one byte short of its pixels is refused.
        assert!(bgrx_to_rgba(&src[..20], 2, 2, 12, 0).is_some());
        assert_eq!(bgrx_to_rgba(&src[..19], 2, 2, 12, 0), None);
    }

    #[test]
    fn codecs_are_named_and_missing_ones_explained() {
        assert_eq!(codec_name(u32::from_le_bytes(*b"HEVC")), "HEVC");
        assert_eq!(codec_name(u32::from_le_bytes(*b"apch")), "ProRes");
        assert_eq!(codec_name(u32::from_le_bytes(*b"H264")), "H.264");
        assert!(missing_decoder_message("HEVC").contains("HEVC Video Extensions"));
        assert!(missing_decoder_message("ProRes").contains("cannot play on Windows"));
    }
}
