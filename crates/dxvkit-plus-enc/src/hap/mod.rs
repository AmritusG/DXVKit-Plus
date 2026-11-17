//! HAP encoders: RGBA8 frames → one HAP frame (the whole sample payload) per call, for Hap1 / Hap5 / HapY / HapA /
//! HapM, single-section or chunked.
//!
//! A port of Swift GlEnc 1.6.2 (github.com/AmritusG/glenc @ ed63fd6, MIT, Copyright (c) 2026 Amrit Rosell) —
//! `HapFrameEncoder.swift` and what it calls: `DXT1Encoder` / `DXT5Encoder` (block walkers), `BC1BlockEncoder`
//! (GlEnc's Swift port of FFmpeg `libavcodec/texturedspenc.c`, MIT, Copyright (C) 2015 Vittorio Giovara, based
//! on public-domain code by Fabian Giesen, Sean Barrett and Yann Collet), `BC4AlphaBlockEncoder` and
//! `BC4AlphaBlockEncoderRefined` (GlEnc's clean-room BC4 searches, the refined one after the ideas of rgbcx /
//! bc7enc_rdo, MIT or public domain), `BC3BlockEncoder`, `BC4PlaneEncoder`, `HapYBlockPacker`, `HapABlockPacker`,
//! `AlphaNormalization`, `SnappyCompressor` (GlEnc's own, SPDX-License-Identifier: BSD-3-Clause), `HAPSection`
//! and `HAPChunkedSection`. Byte for byte: the same pixels and settings give the same packets as Swift GlEnc's
//! `HapFrameEncoder` (checked packet for packet against GlEnc's own GlEncCore by `examples/hap_vs_oracle.rs`).
//!
//! | Codec | Texture | Section (chunks = 1 / ≥ 2) | Alpha tag |
//! |---|---|---|---|
//! | Hap1 | BC1 | `0xBB` / `0xCB` | ignored (alpha bytes still reach BC1's constant-colour test, as in GlEnc) |
//! | Hap5 | BC3 | `0xBE` / `0xCE` | straight colour: premultiplied is un-premultiplied, opaque forces α = 255 |
//! | HapY | scaled YCoCg in BC3 | `0xBF` / `0xCF` | ignored |
//! | HapA | BC4 (alpha) | `0xB1` / `0xC1` | opaque is refused ([`EncodeError::NoAlpha`]) |
//! | HapM | HapY + HapA inside `0x0D` | inner `0xBF`+`0xB1` / `0xCF`+`0xC1` | colour as given; alpha as HapA, opaque → 255 |
//!
//! Coded size = presentation size rounded up to 4; padding pixels are zero (before any transform). Chunked
//! sections split the block stream into `min(chunks, blocks)` runs of `blocks / n` whole blocks (the last takes
//! the rest), each Snappy-compressed or stored raw (`0x0A`) when Snappy does not shrink it.
//!
//! Blocks are independent, so they are encoded on several threads (bands of block rows) and Snappy chunks in
//! parallel; the output does not depend on the thread count.
//!
//! Lint note (this module): frame edges are checked against [`MAX_EDGE`] (16384) before any arithmetic, so
//! pixel / block coordinates and byte offsets (≤ 16384² × 4) fit `usize` and `u32`; the BC math works on 8-bit
//! values in `i32` (largest: 16 × 255² in a covariance sum); fixed 4×4 tile / palette / table arrays are indexed
//! by loop counters or 8-bit values below their length; slices are cut at offsets computed from their length.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

mod bc1;
mod bc4;
mod snappy;

use crate::EncodeError;
use crate::alpha::{FrameAlpha, normalize};
use bc1::Tile;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Largest frame edge accepted (Swift GlEnc sets none; this bound keeps every offset in range).
pub const MAX_EDGE: u32 = 16_384;

/// Largest chunk count (Swift GlEnc's GUI clamps and its CLI validates 1…64).
pub const MAX_CHUNKS: u32 = 64;

/// The HAP variant to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HapCodec {
    /// RGB DXT1 (`Hap1`).
    Hap1,
    /// RGBA DXT5 (`Hap5`).
    Hap5,
    /// Scaled YCoCg DXT5, HAP Q (`HapY`).
    HapY,
    /// Alpha only, RGTC1 / BC4 (`HapA`).
    HapA,
    /// HAP Q Alpha: HapY colour + HapA alpha (`HapM`).
    HapM,
}

impl HapCodec {
    /// The sample-description FourCC.
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            HapCodec::Hap1 => *b"Hap1",
            HapCodec::Hap5 => *b"Hap5",
            HapCodec::HapY => *b"HapY",
            HapCodec::HapA => *b"HapA",
            HapCodec::HapM => *b"HapM",
        }
    }

    /// Whether the output carries alpha.
    pub fn has_alpha(self) -> bool {
        matches!(self, HapCodec::Hap5 | HapCodec::HapA | HapCodec::HapM)
    }

    /// Single-section type byte (HapM: the outer `0x0D`).
    fn section_type(self) -> u8 {
        match self {
            HapCodec::Hap1 => 0xBB,
            HapCodec::Hap5 => 0xBE,
            HapCodec::HapY => 0xBF,
            HapCodec::HapA => 0xB1,
            HapCodec::HapM => 0x0D,
        }
    }
}

/// One HAP encoder for frames of a fixed size. Stateless between frames (like GlEnc's).
#[derive(Clone, Debug)]
pub struct HapEncoder {
    codec: HapCodec,
    chunks: u32,
    width: u32,
    height: u32,
    refined_bc4: bool,
    threads: usize,
}

impl HapEncoder {
    /// An encoder for `width` × `height` frames. `chunks` = 1 writes the single-section form, 2…64 the chunked
    /// form (clamped down to the block count, as GlEnc does).
    pub fn new(codec: HapCodec, chunks: u32, width: u32, height: u32) -> Result<HapEncoder, EncodeError> {
        if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
            return Err(EncodeError::Invalid(format!("HAP frame size {width}x{height} is outside 1..={MAX_EDGE}")));
        }
        if chunks == 0 || chunks > MAX_CHUNKS {
            return Err(EncodeError::Invalid(format!("HAP chunk count {chunks} is outside 1..={MAX_CHUNKS}")));
        }
        Ok(HapEncoder { codec, chunks, width, height, refined_bc4: false, threads: 0 })
    }

    /// Swift GlEnc's session-wide `BC4Config.useRefinement` (default off, which is what GlEnc ships): the 7×7
    /// endpoint search for every BC4 block (Hap5 alpha, HapY luma, HapA, both halves of HapM). Hap1 has no BC4.
    pub fn set_refined_bc4(&mut self, on: bool) {
        self.refined_bc4 = on;
    }

    /// Whether the refined BC4 search is on.
    pub fn refined_bc4(&self) -> bool {
        self.refined_bc4
    }

    /// Worker threads for one frame (0 = the machine's parallelism, the default). Output is the same for any
    /// count.
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// The codec.
    pub fn codec(&self) -> HapCodec {
        self.codec
    }

    /// The chunk count asked for.
    pub fn chunks(&self) -> u32 {
        self.chunks
    }

    /// Presentation width.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Presentation height.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Encode one frame: tightly packed RGBA8, top row first, at the presentation size. `alpha` is how the
    /// alpha byte reads (GlEnc's `CGImageAlphaInfo` tag on the frame). Returns the whole HAP frame.
    pub fn encode(&mut self, rgba: &[u8], alpha: FrameAlpha) -> Result<Vec<u8>, EncodeError> {
        let (w, h) = (self.width as usize, self.height as usize);
        if rgba.len() != w * h * 4 {
            return Err(EncodeError::Frame(format!("HAP frame is {} bytes, {}x{} RGBA8 is {}", rgba.len(), w, h, w * h * 4)));
        }
        if self.codec == HapCodec::HapA && alpha == FrameAlpha::Opaque {
            return Err(EncodeError::NoAlpha);
        }
        let src = Source { rgba, w, h };
        let threads = if self.threads == 0 { std::thread::available_parallelism().map(usize::from).unwrap_or(1) } else { self.threads };
        let bc4: fn(&[u8; 16]) -> [u8; 8] = if self.refined_bc4 { bc4::encode_refined } else { bc4::encode_simple };
        let chunks = self.chunks as usize;
        match self.codec {
            HapCodec::Hap1 => {
                // DXT1Encoder copies the pixels as they are (alpha included) and ignores the tag.
                let blocks = encode_blocks::<8>(&src, threads, |bx, by| bc1::encode(&src.tile(bx, by, |p| p)));
                texture_section(&blocks, 8, chunks, self.codec.section_type(), 0xCB, threads)
            }
            HapCodec::Hap5 => {
                let blocks = encode_blocks::<16>(&src, threads, |bx, by| bc3(&src.tile(bx, by, |p| normalize(alpha, p)), bc4));
                texture_section(&blocks, 16, chunks, self.codec.section_type(), 0xCE, threads)
            }
            HapCodec::HapY => {
                let blocks = encode_blocks::<16>(&src, threads, |bx, by| bc3(&src.hapy_tile(bx, by), bc4));
                texture_section(&blocks, 16, chunks, self.codec.section_type(), 0xCF, threads)
            }
            HapCodec::HapA => {
                let blocks = encode_blocks::<8>(&src, threads, |bx, by| bc4(&src.alpha_tile(bx, by, alpha)));
                texture_section(&blocks, 8, chunks, self.codec.section_type(), 0xC1, threads)
            }
            HapCodec::HapM => {
                // Colour: HapYBlockPacker (no alpha normalisation). Alpha: HapABlockPacker (opaque → 255).
                let y = encode_blocks::<16>(&src, threads, |bx, by| bc3(&src.hapy_tile(bx, by), bc4));
                let a = encode_blocks::<8>(&src, threads, |bx, by| bc4(&src.alpha_tile(bx, by, alpha)));
                let mut inner = par_map(2, threads, |i| {
                    if i == 0 { texture_section(&y, 16, chunks, 0xBF, 0xCF, threads) } else { texture_section(&a, 8, chunks, 0xB1, 0xC1, threads) }
                })
                .into_iter();
                let (Some(iy), Some(ia)) = (inner.next(), inner.next()) else {
                    return Err(EncodeError::Invalid("HapM: inner sections missing".into()));
                };
                let (iy, ia) = (iy?, ia?);
                let mut out = Vec::with_capacity(iy.len() + ia.len() + 8);
                push_header(&mut out, iy.len() + ia.len(), self.codec.section_type())?;
                out.extend_from_slice(&iy);
                out.extend_from_slice(&ia);
                Ok(out)
            }
        }
    }
}

/// The frame being encoded.
struct Source<'a> {
    rgba: &'a [u8],
    w: usize,
    h: usize,
}

impl Source<'_> {
    #[inline(always)]
    fn px(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.w + x) * 4;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    /// The 4×4 tile at block (bx, by) through `f`; pixels past the presentation edge are zero (GlEnc's zero-filled
    /// coded buffer, never normalised).
    #[inline(always)]
    fn tile(&self, bx: usize, by: usize, f: impl Fn([u8; 4]) -> [u8; 4]) -> Tile {
        let mut t = [[0u8; 4]; 16];
        for ty in 0..4 {
            let y = by * 4 + ty;
            if y >= self.h {
                break;
            }
            for tx in 0..4 {
                let x = bx * 4 + tx;
                if x < self.w {
                    t[ty * 4 + tx] = f(self.px(x, y));
                }
            }
        }
        t
    }

    /// HapYBlockPacker's intermediate tile: (Co·s + 128, Cg·s + 128, scale byte, Y), s ∈ {4, 2, 1} the largest
    /// that keeps the block's |Co|, |Cg| in range (≤ 31 → 4 / byte 24, ≤ 63 → 2 / byte 8, else 1 / byte 0).
    /// Padding pixels count as Y = Co = Cg = 0 but do not take part in the scale choice.
    #[inline(always)]
    fn hapy_tile(&self, bx: usize, by: usize) -> Tile {
        let mut ycc = [[0i32; 3]; 16];
        let mut max_abs = 0i32;
        for ty in 0..4 {
            for tx in 0..4 {
                let (x, y) = (bx * 4 + tx, by * 4 + ty);
                if x < self.w && y < self.h {
                    let p = self.px(x, y);
                    let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                    let yy = (r + 2 * g + b + 2) >> 2;
                    let co = (r - b) >> 1;
                    let cg = (-r + 2 * g - b) >> 2;
                    ycc[ty * 4 + tx] = [yy, co, cg];
                    max_abs = max_abs.max(co.abs().max(cg.abs()));
                }
            }
        }
        let (s, scale_byte) = if max_abs <= 31 {
            (4, 24u8)
        } else if max_abs <= 63 {
            (2, 8u8)
        } else {
            (1, 0u8)
        };
        ycc.map(|[yy, co, cg]| [(co * s + 128).clamp(0, 255) as u8, (cg * s + 128).clamp(0, 255) as u8, scale_byte, yy.clamp(0, 255) as u8])
    }

    /// HapABlockPacker's alpha tile: the normalised alpha (opaque → 255), padding 0.
    #[inline(always)]
    fn alpha_tile(&self, bx: usize, by: usize, alpha: FrameAlpha) -> [u8; 16] {
        let t = self.tile(bx, by, |p| normalize(alpha, p));
        t.map(|p| p[3])
    }
}

/// BC3 = BC4 over the tile's alpha + BC1 over its colour (`encodeBC3Block`).
#[inline(always)]
fn bc3(t: &Tile, bc4: fn(&[u8; 16]) -> [u8; 8]) -> [u8; 16] {
    let a = bc4(&t.map(|p| p[3]));
    let c = bc1::encode(t);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a);
    out[8..].copy_from_slice(&c);
    out
}

/// The block stream (row-major blocks of `B` bytes over the coded size), bands of block rows on `threads`.
fn encode_blocks<const B: usize>(src: &Source<'_>, threads: usize, f: impl Fn(usize, usize) -> [u8; B] + Sync) -> Vec<u8> {
    let (bw, bh) = (src.w.div_ceil(4), src.h.div_ceil(4));
    let row_bytes = bw * B;
    let mut out = vec![0u8; bh * row_bytes];
    let rows = |first: usize, band: &mut [u8]| {
        for (r, row) in band.chunks_exact_mut(row_bytes).enumerate() {
            for (bx, dst) in row.chunks_exact_mut(B).enumerate() {
                dst.copy_from_slice(&f(bx, first + r));
            }
        }
    };
    // Bands of at least a few rows; small frames stay on this thread.
    let bands = threads.min(bh.div_ceil(4)).max(1);
    if bands == 1 {
        rows(0, &mut out);
        return out;
    }
    let per = bh.div_ceil(bands);
    std::thread::scope(|s| {
        for (i, band) in out.chunks_mut(per * row_bytes).enumerate() {
            let rows = &rows;
            s.spawn(move || rows(i * per, band));
        }
    });
    out
}

/// `f(0..n)` on up to `threads` workers, results in index order.
fn par_map<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let workers = threads.min(n);
    if workers <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, T)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        mine.push((i, f(i)));
                    }
                    mine
                })
            })
            .collect();
        // A worker that panicked loses its items; the length check below turns that into a fallback.
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    if done.len() != n {
        return (0..n).map(f).collect();
    }
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, t)| t).collect()
}

/// A HAP section header (`HAPSection.make`): 4 bytes (24-bit length + type) below 16 MiB, else 8 bytes (zero
/// length, type, 32-bit length).
fn push_header(out: &mut Vec<u8>, len: usize, ty: u8) -> Result<(), EncodeError> {
    if len < 1 << 24 {
        out.extend_from_slice(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, ty]);
        return Ok(());
    }
    let len32 = u32::try_from(len).map_err(|_| EncodeError::Invalid(format!("HAP section of {len} bytes exceeds 4 GB")))?;
    out.extend_from_slice(&[0, 0, 0, ty]);
    out.extend_from_slice(&len32.to_le_bytes());
    Ok(())
}

/// One texture's section: single (`chunks` ≤ 1: Snappy + `single_type`) or chunked (`HAPChunkedSection.make`
/// under `chunked_type`).
fn texture_section(blocks: &[u8], block_size: usize, chunks: usize, single_type: u8, chunked_type: u8, threads: usize) -> Result<Vec<u8>, EncodeError> {
    if chunks < 2 {
        let payload = snappy::compress(blocks);
        let mut out = Vec::with_capacity(payload.len() + 8);
        push_header(&mut out, payload.len(), single_type)?;
        out.extend_from_slice(&payload);
        return Ok(out);
    }
    let block_count = blocks.len() / block_size;
    let n = chunks.min(block_count.max(1));
    let base = block_count / n;
    let range = |i: usize| {
        let start = i * base * block_size;
        let end = if i == n - 1 { blocks.len() } else { start + base * block_size };
        &blocks[start..end]
    };
    // Snappy each chunk; keep it raw (0x0A) unless Snappy shrinks it (0x0B).
    let stored: Vec<Option<Vec<u8>>> = par_map(n, threads, |i| {
        let raw = range(i);
        let c = snappy::compress(raw);
        (c.len() < raw.len()).then_some(c)
    });
    let chunk = |i: usize| -> &[u8] {
        match &stored[i] {
            Some(c) => c,
            None => range(i),
        }
    };

    let mut compressors = Vec::with_capacity(n);
    let mut sizes = Vec::with_capacity(n * 4);
    for (i, s) in stored.iter().enumerate() {
        compressors.push(if s.is_some() { 0x0B } else { 0x0A });
        let len = u32::try_from(chunk(i).len()).map_err(|_| EncodeError::Invalid("HAP chunk exceeds 4 GB".into()))?;
        sizes.extend_from_slice(&len.to_le_bytes());
    }
    let mut instructions = Vec::with_capacity(compressors.len() + sizes.len() + 16);
    push_header(&mut instructions, compressors.len(), 0x02)?;
    instructions.extend_from_slice(&compressors);
    push_header(&mut instructions, sizes.len(), 0x03)?;
    instructions.extend_from_slice(&sizes);

    let chunk_bytes: usize = (0..n).map(|i| chunk(i).len()).sum();
    let mut instr_section = Vec::with_capacity(instructions.len() + 8);
    push_header(&mut instr_section, instructions.len(), 0x01)?;
    instr_section.extend_from_slice(&instructions);

    let outer_len = instr_section.len() + chunk_bytes;
    let mut out = Vec::with_capacity(outer_len + 8);
    push_header(&mut out, outer_len, chunked_type)?;
    out.extend_from_slice(&instr_section);
    for i in 0..n {
        out.extend_from_slice(chunk(i));
    }
    Ok(out)
}
