//! BGRA scaling through Accelerate's `vImageScale_ARGB8888`, exactly as Swift GlEnc 1.6.2's
//! `FrameResizer` calls it (glenc `Sources/GlEncCore/FrameResizer.swift`, `vImageScale`):
//! Bilinear = `kvImageNoFlags`, Lanczos = `kvImageHighQualityResampling`, no temp buffer.
//!
//! The filter is per channel, so the byte order (BGRA here, ARGB in the name) does not matter.
//! GlEnc ran it on IOSurface-backed `CVPixelBuffer`s whose rows may be padded; the output does not
//! depend on the row stride (`tests/glenc_av.rs` scales the same image from tight and from padded
//! rows and compares), so tight buffers in and out give GlEnc's pixels.
//!
//! Lint note: `arithmetic_side_effects` is allowed in this module for the stride / size products,
//! which are computed with checked arithmetic up front.

use std::ffi::c_void;
use std::fmt;

/// Which vImage kernel GlEnc's resize quality maps to (`ResizeQuality.bilinear` / `.lanczos`;
/// `.auto` resolves to one of these, `.nearest` is GlEnc's own loop and not here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleFilter {
    /// `kvImageNoFlags` (vImage's default, bilinear-class kernel).
    Bilinear,
    /// `kvImageHighQualityResampling` (Lanczos).
    Lanczos,
}

/// A scale that could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScaleError {
    /// A zero target size (Swift: `FrameResizerError.invalidTargetDimensions`).
    InvalidTargetDimensions { width: u32, height: u32 },
    /// The source slice does not hold `w * h * 4` bytes (or the source size is zero).
    InvalidSource { width: u32, height: u32, len: usize },
    /// vImage returned an error code (Swift: `FrameResizerError.vImageError`).
    VImage(isize),
}

impl fmt::Display for ScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScaleError::InvalidTargetDimensions { width, height } => {
                write!(f, "FrameResizer: target dimensions must be positive — got ({width}×{height})")
            }
            ScaleError::InvalidSource { width, height, len } => {
                write!(
                    f,
                    "FrameResizer: source of {width}×{height} needs {} bytes, got {len}",
                    u64::from(*width).saturating_mul(u64::from(*height)).saturating_mul(4)
                )
            }
            ScaleError::VImage(e) => write!(f, "FrameResizer: vImage error {e}"),
        }
    }
}

impl std::error::Error for ScaleError {}

/// `vImage_Buffer` (vImage_Types.h): data, height, width, rowBytes.
#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

/// `kvImageNoFlags` (vImage_Types.h: `kvImageNoFlags = 0`).
const KV_IMAGE_NO_FLAGS: u32 = 0;
/// `kvImageHighQualityResampling` (vImage_Types.h: `kvImageHighQualityResampling = 32`).
const KV_IMAGE_HIGH_QUALITY_RESAMPLING: u32 = 32;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    /// `vImage_Error vImageScale_ARGB8888(const vImage_Buffer *src, const vImage_Buffer *dest,
    /// void *tempBuffer, vImage_Flags flags)` (vImage Geometry.h; `vImage_Error` = `ssize_t`,
    /// `vImage_Flags` = `uint32_t`).
    fn vImageScale_ARGB8888(src: *const VImageBuffer, dest: *const VImageBuffer, temp_buffer: *mut c_void, flags: u32) -> isize;
}

/// Scale a tight `src_w * src_h * 4` 8-bit 4-channel image to `dst_w * dst_h` (tight out).
/// Equal sizes return a copy (Swift's equal-dimensions fast path returns the frame unchanged).
pub fn scale_bgra(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32, filter: ScaleFilter) -> Result<Vec<u8>, ScaleError> {
    scale_bgra_strided(src, src_w, src_h, src_w as usize * 4, dst_w, dst_h, dst_w as usize * 4, filter).map(|(out, _)| out)
}

/// The same scale with explicit row strides (bytes per row, at least `w * 4`) on both sides; the
/// output keeps `dst_stride` (its padding bytes are whatever vImage leaves, zero-initialised here).
/// Returns (buffer, stride). Used to show the result does not depend on the stride.
#[allow(clippy::arithmetic_side_effects)]
pub fn scale_bgra_strided(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: usize,
    dst_w: u32,
    dst_h: u32,
    dst_stride: usize,
    filter: ScaleFilter,
) -> Result<(Vec<u8>, usize), ScaleError> {
    if dst_w == 0 || dst_h == 0 {
        return Err(ScaleError::InvalidTargetDimensions { width: dst_w, height: dst_h });
    }
    let (sw, sh, dw, dh) = (src_w as usize, src_h as usize, dst_w as usize, dst_h as usize);
    let bad_src = ScaleError::InvalidSource { width: src_w, height: src_h, len: src.len() };
    let src_row = sw.checked_mul(4).ok_or_else(|| bad_src.clone())?;
    let need = src_stride.checked_mul(sh.saturating_sub(1)).and_then(|n| n.checked_add(src_row)).ok_or_else(|| bad_src.clone())?;
    if sw == 0 || sh == 0 || src_stride < src_row || src.len() < need {
        return Err(bad_src);
    }
    let dst_row = dw.checked_mul(4).ok_or(ScaleError::InvalidTargetDimensions { width: dst_w, height: dst_h })?;
    let dst_stride = dst_stride.max(dst_row);
    let dst_len = dst_stride.checked_mul(dh).ok_or(ScaleError::InvalidTargetDimensions { width: dst_w, height: dst_h })?;
    if src_w == dst_w && src_h == dst_h {
        let mut out = vec![0u8; dst_len];
        for (d, s) in out.chunks_mut(dst_stride).zip(src.chunks(src_stride)) {
            if let (Some(d), Some(s)) = (d.get_mut(..dst_row), s.get(..src_row)) {
                d.copy_from_slice(s);
            }
        }
        return Ok((out, dst_stride));
    }
    let mut out = vec![0u8; dst_len];
    let src_buf = VImageBuffer { data: src.as_ptr().cast_mut().cast(), height: sh, width: sw, row_bytes: src_stride };
    let dst_buf = VImageBuffer { data: out.as_mut_ptr().cast(), height: dh, width: dw, row_bytes: dst_stride };
    let flags = match filter {
        ScaleFilter::Bilinear => KV_IMAGE_NO_FLAGS,
        ScaleFilter::Lanczos => KV_IMAGE_HIGH_QUALITY_RESAMPLING,
    };
    // SAFETY: `src_buf` describes `sh` rows of `src_stride` bytes inside `src` (checked above: the
    // last row ends at `need <= src.len()`), read only (vImage takes `const vImage_Buffer *` and
    // never writes the source); `dst_buf` describes `dh * dst_stride` bytes owned by `out`, alive
    // and exclusively borrowed for the call. A null temp buffer asks vImage to allocate its own.
    let e = unsafe { vImageScale_ARGB8888(&raw const src_buf, &raw const dst_buf, std::ptr::null_mut(), flags) };
    if e != 0 {
        return Err(ScaleError::VImage(e));
    }
    Ok((out, dst_stride))
}
