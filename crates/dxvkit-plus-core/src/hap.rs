//! HAP — our own decoder, written from the public Vidvox HAP specification only
//! (`documentation/HapVideoDRAFT.md`, github.com/Vidvox/hap @ d847f6bbd3be88575dd4ef33a877243780e3be76,
//! BSD-2-Clause; the reference C source in that repository was not read). Spec edition recorded in
//! the crate README.
//!
//! A HAP frame is a tree of sections (4- or 8-byte headers). The top-level section type names the
//! texture format (BC1 RGB, BC3 RGBA, BC3 scaled YCoCg, BC7, BC4 alpha, BC6H unsigned / signed) and
//! the second-stage compressor: none, Snappy (the permissive `snap` crate), or "decode instructions"
//! — a chunk table whose chunks decompress independently (we decompress them in parallel). Type
//! `0x0D` holds several images; the permitted pair is scaled-YCoCg DXT5 + BC4 alpha (HAP Q Alpha).
//!
//! Output is the raw block stream of each image, uploaded straight to a BC texture
//! (`dxvkit-plus-wgpu`); scaled YCoCg is converted on the GPU (van Waveren & Castaño, "Real-Time
//! YCoCg-DXT Compression", 2007, as the spec references) and on the CPU by [`ycocg_scaled_to_rgb`].

use crate::bytes::{byte, le24, le32};
use crate::error::MediaError;
use crate::source::{PixelFormat, Plane, Shading, planes_to_rgba};
use std::fmt;

/// The texture format of one HAP image (the low nibble of its section type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum HapTexture {
    /// `0x?B` RGB DXT1 / BC1.
    Bc1Rgb = 0,
    /// `0x?E` RGBA DXT5 / BC3.
    Bc3Rgba = 1,
    /// `0x?F` scaled YCoCg in DXT5 / BC3 (HAP Q).
    Bc3YCoCg = 2,
    /// `0x?C` RGBA BPTC / BC7 (HAP R).
    Bc7Rgba = 3,
    /// `0x?1` alpha RGTC1 / BC4 (HAP Alpha-Only, and the alpha of HAP Q Alpha).
    Bc4Alpha = 4,
    /// `0x?2` RGB unsigned float BC6U (HAP HDR).
    Bc6hUfloat = 5,
    /// `0x?3` RGB signed float BC6S (HAP HDR).
    Bc6hSfloat = 6,
}

impl HapTexture {
    fn of(low_nibble: u8) -> Option<HapTexture> {
        Some(match low_nibble {
            0xB => HapTexture::Bc1Rgb,
            0xE => HapTexture::Bc3Rgba,
            0xF => HapTexture::Bc3YCoCg,
            0xC => HapTexture::Bc7Rgba,
            0x1 => HapTexture::Bc4Alpha,
            0x2 => HapTexture::Bc6hUfloat,
            0x3 => HapTexture::Bc6hSfloat,
            _ => return None,
        })
    }
    /// The GPU block format this image uploads as.
    pub fn format(self) -> PixelFormat {
        match self {
            HapTexture::Bc1Rgb => PixelFormat::Bc1,
            HapTexture::Bc3Rgba | HapTexture::Bc3YCoCg => PixelFormat::Bc3,
            HapTexture::Bc7Rgba => PixelFormat::Bc7,
            HapTexture::Bc4Alpha => PixelFormat::Bc4,
            HapTexture::Bc6hUfloat => PixelFormat::Bc6hUfloat,
            HapTexture::Bc6hSfloat => PixelFormat::Bc6hSfloat,
        }
    }
    /// Bytes per 4x4 block.
    pub fn block_bytes(self) -> usize {
        match self {
            HapTexture::Bc1Rgb | HapTexture::Bc4Alpha => 8,
            _ => 16,
        }
    }
    /// Exact block-stream size of a `width` x `height` image (rounded up to whole blocks).
    pub fn bytes(self, width: u32, height: u32) -> Option<usize> {
        let bw = width.div_ceil(4) as usize;
        let bh = height.div_ceil(4) as usize;
        bw.checked_mul(bh)?.checked_mul(self.block_bytes())
    }
}

impl fmt::Display for HapTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HapTexture::Bc1Rgb => "RGB DXT1",
            HapTexture::Bc3Rgba => "RGBA DXT5",
            HapTexture::Bc3YCoCg => "scaled YCoCg DXT5",
            HapTexture::Bc7Rgba => "RGBA BC7",
            HapTexture::Bc4Alpha => "alpha BC4",
            HapTexture::Bc6hUfloat => "RGB BC6U",
            HapTexture::Bc6hSfloat => "RGB BC6S",
        })
    }
}

/// One decoded image of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HapImage {
    pub texture: HapTexture,
    /// The block stream, exactly `texture.bytes(width, height)` long.
    pub blocks: Vec<u8>,
}

/// A decoded frame: one image, or the HAP Q Alpha pair (YCoCg first, alpha second).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HapFrame {
    pub images: Vec<HapImage>,
}

fn err(m: impl Into<String>) -> MediaError {
    MediaError::Hap(m.into())
}

/// Section header: (type, body offset, body length) at `at` within `buf` (spec "Section Header").
fn section(buf: &[u8], at: usize) -> Result<(u8, usize, usize), MediaError> {
    let b0 = byte(buf, at).ok_or_else(|| err("section header past the end"))?;
    let b1 = byte(buf, at.saturating_add(1)).ok_or_else(|| err("section header past the end"))?;
    let b2 = byte(buf, at.saturating_add(2)).ok_or_else(|| err("section header past the end"))?;
    let ty = byte(buf, at.saturating_add(3)).ok_or_else(|| err("section header past the end"))?;
    let (len, header) = if b0 == 0 && b1 == 0 && b2 == 0 {
        (le32(buf, at.saturating_add(4)).ok_or_else(|| err("8-byte section header past the end"))? as usize, 8usize)
    } else {
        (le24(buf, at).ok_or_else(|| err("section header past the end"))? as usize, 4usize)
    };
    let body = at.checked_add(header).ok_or_else(|| err("section offset overflows"))?;
    let end = body.checked_add(len).ok_or_else(|| err("section length overflows"))?;
    if end > buf.len() {
        return Err(err(format!("section of {len} bytes runs past the frame ({} bytes)", buf.len())));
    }
    Ok((ty, body, len))
}

/// Decode a HAP frame of `width` x `height`. `parallel` decompresses chunks on worker threads.
pub fn decode(frame: &[u8], width: u32, height: u32, parallel: bool) -> Result<HapFrame, MediaError> {
    if width == 0 || height == 0 || width > crate::MAX_DIMENSION || height > crate::MAX_DIMENSION {
        return Err(err(format!("frame size {width}x{height} out of range")));
    }
    let (ty, body, len) = section(frame, 0)?;
    let data = frame.get(body..body.saturating_add(len)).unwrap_or_default();
    if ty == 0x0D {
        // Multiple images: one or two top-level sections inside.
        let mut images = Vec::new();
        let mut at = 0usize;
        while at < data.len() {
            let (ity, ibody, ilen) = section(data, at)?;
            if images.len() == 2 {
                return Err(err("more than two images in a multiple-image section"));
            }
            let inner = data.get(ibody..ibody.saturating_add(ilen)).unwrap_or_default();
            images.push(image(ity, inner, width, height, parallel)?);
            at = ibody.saturating_add(ilen);
        }
        match images.as_slice() {
            [_] => {}
            [a, b] if a.texture == HapTexture::Bc3YCoCg && b.texture == HapTexture::Bc4Alpha => {}
            [a, b] if a.texture == HapTexture::Bc4Alpha && b.texture == HapTexture::Bc3YCoCg => images.swap(0, 1),
            [a, b] => return Err(err(format!("unsupported image combination {} + {}", a.texture, b.texture))),
            _ => return Err(err("empty multiple-image section")),
        }
        return Ok(HapFrame { images });
    }
    Ok(HapFrame { images: vec![image(ty, data, width, height, parallel)?] })
}

/// One top-level image section (type `ty`, section data `data`).
fn image(ty: u8, data: &[u8], width: u32, height: u32, parallel: bool) -> Result<HapImage, MediaError> {
    let texture = HapTexture::of(ty & 0x0F).ok_or_else(|| err(format!("unknown top-level section type 0x{ty:02X}")))?;
    let want = texture.bytes(width, height).ok_or_else(|| err("texture size overflows"))?;
    let mut blocks = vec![0u8; want];
    match ty >> 4 {
        0xA => copy_exact(data, &mut blocks)?,
        0xB => snappy(data, &mut blocks)?,
        0xC => chunked(data, &mut blocks, parallel)?,
        _ => return Err(err(format!("unknown second-stage compressor in section type 0x{ty:02X}"))),
    }
    Ok(HapImage { texture, blocks })
}

fn copy_exact(src: &[u8], out: &mut [u8]) -> Result<(), MediaError> {
    let s = src.get(..out.len()).ok_or_else(|| err(format!("image data is {} bytes, the texture needs {}", src.len(), out.len())))?;
    out.copy_from_slice(s);
    Ok(())
}

fn snappy(src: &[u8], out: &mut [u8]) -> Result<(), MediaError> {
    let n = snap::raw::decompress_len(src).map_err(|e| err(format!("snappy: {e}")))?;
    if n < out.len() {
        return Err(err(format!("snappy data expands to {n} bytes, the texture needs {}", out.len())));
    }
    if n == out.len() {
        snap::raw::Decoder::new().decompress(src, out).map_err(|e| err(format!("snappy: {e}")))?;
    } else {
        // Larger than the texture: decode bounded by the claimed size (capped) and keep the texture.
        if n > out.len().saturating_mul(2).saturating_add(1 << 20) {
            return Err(err(format!("snappy data claims {n} bytes for a {}-byte texture", out.len())));
        }
        let v = snap::raw::Decoder::new().decompress_vec(src).map_err(|e| err(format!("snappy: {e}")))?;
        copy_exact(&v, out)?;
    }
    Ok(())
}

/// Most chunks a frame may declare (hostile-input bound; encoders use up to 64).
pub const MAX_CHUNKS: usize = 4096;

/// "Decode instructions": a 0x01 container section followed by the frame data.
fn chunked(data: &[u8], out: &mut [u8], parallel: bool) -> Result<(), MediaError> {
    let (ty, body, len) = section(data, 0)?;
    if ty != 0x01 {
        return Err(err(format!("expected a decode-instructions container (0x01), found 0x{ty:02X}")));
    }
    let instr = data.get(body..body.saturating_add(len)).unwrap_or_default();
    let frame_data = data.get(body.saturating_add(len)..).unwrap_or_default();
    let (mut compressors, mut sizes, mut offsets) = (None, None, None);
    let mut at = 0usize;
    while at < instr.len() {
        let (sty, sbody, slen) = section(instr, at)?;
        let s = instr.get(sbody..sbody.saturating_add(slen)).unwrap_or_default();
        match sty {
            0x02 => compressors = Some(s.to_vec()),
            0x03 => sizes = Some(s.chunks_exact(4).filter_map(|c| le32(c, 0)).map(|v| v as usize).collect::<Vec<_>>()),
            0x04 => offsets = Some(s.chunks_exact(4).filter_map(|c| le32(c, 0)).map(|v| v as usize).collect::<Vec<_>>()),
            _ => {} // unknown sections: continue (spec)
        }
        at = sbody.saturating_add(slen);
    }
    let compressors = compressors.ok_or_else(|| err("decode instructions without a compressor table"))?;
    let sizes = sizes.ok_or_else(|| err("decode instructions without a chunk size table"))?;
    let n = compressors.len();
    if n == 0 || n > MAX_CHUNKS || sizes.len() != n || offsets.as_ref().is_some_and(|o| o.len() != n) {
        return Err(err(format!("chunk tables disagree or are out of range ({n} compressors, {} sizes)", sizes.len())));
    }
    // Locate each chunk's input and decompressed length, then its output range.
    let mut jobs = Vec::with_capacity(n);
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    for i in 0..n {
        let size = sizes.get(i).copied().unwrap_or(0);
        let start = match &offsets {
            Some(o) => o.get(i).copied().unwrap_or(0),
            None => in_pos,
        };
        in_pos = start.checked_add(size).ok_or_else(|| err("chunk offset overflows"))?;
        let input = frame_data.get(start..in_pos).ok_or_else(|| err(format!("chunk {i} runs past the frame data")))?;
        let comp = compressors.get(i).copied().unwrap_or(0);
        let out_len = match comp {
            0x0A => input.len(),
            0x0B => snap::raw::decompress_len(input).map_err(|e| err(format!("chunk {i} snappy: {e}")))?,
            other => return Err(err(format!("chunk {i}: unknown compressor 0x{other:02X}"))),
        };
        let out_end = out_pos.checked_add(out_len).ok_or_else(|| err("chunk output overflows"))?;
        if out_end > out.len() {
            return Err(err(format!("chunks expand past the {}-byte texture", out.len())));
        }
        jobs.push((comp, input, out_len));
        out_pos = out_end;
    }
    if out_pos != out.len() {
        return Err(err(format!("chunks expand to {out_pos} bytes, the texture needs {}", out.len())));
    }
    // Disjoint output slices, one per chunk.
    let mut slices = Vec::with_capacity(n);
    let mut rest: &mut [u8] = out;
    for &(_, _, len) in &jobs {
        let (head, tail) = rest.split_at_mut(len.min(rest.len()));
        slices.push(head);
        rest = tail;
    }
    let run = |(comp, input, _): (u8, &[u8], usize), dst: &mut [u8]| -> Result<(), MediaError> {
        if comp == 0x0A {
            copy_exact(input, dst)
        } else {
            snap::raw::Decoder::new().decompress(input, dst).map(|_| ()).map_err(|e| err(format!("snappy chunk: {e}")))
        }
    };
    if parallel && n > 1 {
        let threads = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).clamp(1, 16);
        let per = n.div_ceil(threads);
        let mut work: Vec<_> = jobs.into_iter().zip(slices).collect();
        let results: Vec<Result<(), MediaError>> = std::thread::scope(|s| {
            let handles: Vec<_> = work
                .chunks_mut(per.max(1))
                .map(|group| {
                    s.spawn(move || {
                        for (job, dst) in group.iter_mut() {
                            run(*job, dst)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(err("chunk worker panicked")))).collect()
        });
        results.into_iter().collect::<Result<Vec<()>, _>>()?;
    } else {
        for (job, dst) in jobs.into_iter().zip(slices) {
            run(job, dst)?;
        }
    }
    Ok(())
}

/// Scaled YCoCg (BC3-decoded RGBA, 0..255 per channel) to RGB, the CPU oracle of the GPU shader:
/// `scale = B * 255/8 + 1`, `Co = (R - 128/255) / scale`, `Cg = (G - 128/255) / scale`, `Y = A`,
/// `R = Y + Co - Cg`, `G = Y + Cg`, `B = Y - Co - Cg` (normalised units), rounded and clamped.
pub fn ycocg_scaled_to_rgb(px: [u8; 4]) -> [u8; 3] {
    let n = |v: u8| f64::from(v) / 255.0;
    ycocg_scaled_to_rgb_f([n(px[0]), n(px[1]), n(px[2]), n(px[3])])
}

/// [`ycocg_scaled_to_rgb`] on normalised (0..1) channel values, e.g. from
/// [`crate::bc::bc3_block_exact`].
pub fn ycocg_scaled_to_rgb_f(px: [f64; 4]) -> [u8; 3] {
    let scale = px[2] * (255.0 / 8.0) + 1.0;
    let offset = 128.0 / 255.0;
    let co = (px[0] - offset) / scale;
    let cg = (px[1] - offset) / scale;
    let y = px[3];
    let to8 = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [to8(y + co - cg), to8(y + cg), to8(y - co - cg)]
}

/// The frame's GPU-ready planes and how they become RGBA. Block planes are padded to whole 4x4
/// blocks (`width`/`height` rounded up to multiples of 4).
pub fn planes(frame: HapFrame, width: u32, height: u32) -> Result<(Shading, Vec<Plane>), MediaError> {
    let first = frame.images.first().map(|i| i.texture).ok_or_else(|| err("frame without images"))?;
    let shading = match (first, frame.images.len()) {
        (HapTexture::Bc1Rgb, _) => Shading::Rgba { opaque: true },
        (HapTexture::Bc3Rgba | HapTexture::Bc7Rgba, _) => Shading::Rgba { opaque: false },
        (HapTexture::Bc3YCoCg, n) => Shading::HapYcocg { alpha: n == 2 },
        (HapTexture::Bc4Alpha, _) => Shading::AlphaOnly,
        (HapTexture::Bc6hUfloat | HapTexture::Bc6hSfloat, _) => Shading::Hdr,
    };
    let (tw, th) = (width.div_ceil(4).saturating_mul(4), height.div_ceil(4).saturating_mul(4));
    let planes = frame.images.into_iter().map(|i| Plane { format: i.texture.format(), width: tw, height: th, data: i.blocks }).collect();
    Ok((shading, planes))
}

/// CPU decode of a whole frame to straight RGBA8 (`width * 4` stride) — the oracle for the GPU path
/// and for the fixture accuracy checks. BC1 is opaque (HAP's "RGB"); scaled YCoCg goes through
/// [`ycocg_scaled_to_rgb`]; alpha-only shows the alpha as grey with alpha 255; BC7 (HAP R) through
/// [`crate::bc7`]; HDR (BC6H) has no CPU decoder and is `Unsupported`.
pub fn to_rgba(frame: &HapFrame, width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    let (shading, planes) = planes(frame.clone(), width, height)?;
    planes_to_rgba(shading, &planes, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ty: u8, len: usize) -> Vec<u8> {
        if len < 0x00FF_FFFF && len > 0 {
            let b = (len as u32).to_le_bytes();
            vec![b[0], b[1], b[2], ty]
        } else {
            let mut v = vec![0, 0, 0, ty];
            v.extend_from_slice(&(len as u32).to_le_bytes());
            v
        }
    }

    fn sec(ty: u8, body: &[u8]) -> Vec<u8> {
        let mut v = header(ty, body.len());
        v.extend_from_slice(body);
        v
    }

    fn blocks(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn plain_snappy_and_chunked_frames_decode_to_the_same_blocks() {
        let (w, h) = (16u32, 8u32);
        let tex = blocks(HapTexture::Bc1Rgb.bytes(w, h).unwrap(), 7);
        let plain = sec(0xAB, &tex);
        let snappy = sec(0xBB, &snap::raw::Encoder::new().compress_vec(&tex).unwrap());
        // Two chunks: one stored, one snappy; no offset table.
        let (a, b) = tex.split_at(24);
        let cb = snap::raw::Encoder::new().compress_vec(b).unwrap();
        let mut sizes = Vec::new();
        sizes.extend_from_slice(&(a.len() as u32).to_le_bytes());
        sizes.extend_from_slice(&(cb.len() as u32).to_le_bytes());
        let instr = [sec(0x02, &[0x0A, 0x0B]), sec(0x03, &sizes)].concat();
        let mut body = sec(0x01, &instr);
        body.extend_from_slice(a);
        body.extend_from_slice(&cb);
        let chunked = sec(0xCB, &body);
        for (name, f) in [("plain", &plain), ("snappy", &snappy), ("chunked", &chunked)] {
            for parallel in [false, true] {
                let d = decode(f, w, h, parallel).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(d.images.len(), 1);
                assert_eq!(d.images[0].texture, HapTexture::Bc1Rgb);
                assert_eq!(d.images[0].blocks, tex, "{name}");
            }
        }
        // Negative controls: a short texture, a lying chunk size, an unknown compressor.
        assert!(decode(&sec(0xAB, &tex[..tex.len() - 1]), w, h, false).is_err());
        let mut bad = chunked.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        let _ = decode(&bad, w, h, true); // must not panic (may or may not decode)
        let instr_bad = [sec(0x02, &[0x0A, 0x0C]), sec(0x03, &sizes)].concat();
        let mut body_bad = sec(0x01, &instr_bad);
        body_bad.extend_from_slice(a);
        body_bad.extend_from_slice(&cb);
        assert!(decode(&sec(0xCB, &body_bad), w, h, false).is_err());
    }

    #[test]
    fn hap_q_alpha_is_a_ycocg_plus_alpha_pair() {
        let (w, h) = (8u32, 4u32);
        let y = blocks(HapTexture::Bc3YCoCg.bytes(w, h).unwrap(), 1);
        let a = blocks(HapTexture::Bc4Alpha.bytes(w, h).unwrap(), 2);
        let f = sec(0x0D, &[sec(0xAF, &y), sec(0xA1, &a)].concat());
        let d = decode(&f, w, h, false).unwrap();
        assert_eq!(d.images.iter().map(|i| i.texture).collect::<Vec<_>>(), vec![HapTexture::Bc3YCoCg, HapTexture::Bc4Alpha]);
        // Negative control: a combination the spec does not permit.
        let bad = sec(0x0D, &[sec(0xAB, &blocks(HapTexture::Bc1Rgb.bytes(w, h).unwrap(), 3)), sec(0xA1, &a)].concat());
        assert!(decode(&bad, w, h, false).is_err());
    }

    #[test]
    fn eight_byte_headers_and_unknown_types() {
        let tex = blocks(HapTexture::Bc7Rgba.bytes(4, 4).unwrap(), 9);
        let mut f = vec![0, 0, 0, 0xAC];
        f.extend_from_slice(&(tex.len() as u32).to_le_bytes());
        f.extend_from_slice(&tex);
        assert_eq!(decode(&f, 4, 4, false).unwrap().images[0].texture, HapTexture::Bc7Rgba);
        assert!(decode(&sec(0xA7, &tex), 4, 4, false).is_err());
        assert!(decode(&[1, 2], 4, 4, false).is_err());
    }

    #[test]
    fn scaled_ycocg_grey_is_grey() {
        // Co = Cg = 128/255 (no chroma), scale byte 0: grey Y.
        assert_eq!(ycocg_scaled_to_rgb([128, 128, 0, 77]), [77, 77, 77]);
    }
}
