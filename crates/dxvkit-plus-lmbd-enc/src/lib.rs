//! # dxvkit-plus-lmbd-enc — the LMBD (LMB7) encoder
//!
//! RGBA frames → 4-mode BC7 with rate-distortion optimisation → block-aligned chunks → Zstd → an
//! LMBD file (`docs/LMBD.md`). This is the encoder of the original LMBD codec (`lmbd-encode` in
//! Amrit's `loomda-codec`), ported to Rust:
//!
//! - [`bc7enc`] and [`ert`]/[`rdo`] are faithful ports of bc7enc_rdo's `bc7enc.cpp`, `ert.cpp` and the
//!   BC7 path of `rdo_bc_encoder.cpp` (Richard Geldreich, Jr.; MIT OR Unlicense), down to the fused
//!   multiply-adds Apple clang forms — so for the same pixels and settings the BC7 blocks are
//!   byte-identical to the original's;
//! - chunks are compressed with libzstd 1.5.7 at level 9 (the original's version and level), so the
//!   whole file is byte-identical too.
//!
//! The defaults ([`EncodeParams::default`]) are the original's: lambda 1.0, look-back 16384 bytes,
//! one chunk per frame, Zstd level 9, colorspace sRGB.
//!
//! RDO at look-back 16384 is slow by design (the original takes about 10 minutes for one 1080p
//! frame of detailed footage on one core); frames are independent, so [`encode_frames`] encodes
//! several at once. A lower `lookback`, `lambda = 0` or more RDO partitions trade size or exactness
//! for speed.
#![forbid(unsafe_code)]

mod bc7dec;
pub mod bc7enc;
pub mod ert;
pub mod lmbx;
pub mod rdo;
mod tables;
mod v3;

pub use bc7enc::Bc7Params;
pub use rdo::RdoParams;

use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::lmbd::{ChunkData, Compressor, Header, Writer, chunk_blocks, frame_payload};
use std::io::{Seek, Write};

/// Everything the encoder needs besides the pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodeParams {
    pub bc7: Bc7Params,
    pub rdo: RdoParams,
    /// Chunks per frame (1..=blocks); chunks decompress in parallel on playback.
    pub chunks: u16,
    pub compressor: Compressor,
    /// Zstd level (the original uses 9).
    pub zstd_level: i32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        EncodeParams { bc7: Bc7Params::default(), rdo: RdoParams::default(), chunks: 1, compressor: Compressor::Zstd, zstd_level: 9 }
    }
}

impl EncodeParams {
    /// Clamp the RDO settings to the original command line's ranges (`-z` 0..=500, `-zc`
    /// 8..=131072) and reject what cannot be encoded.
    pub fn validated(mut self) -> Result<EncodeParams, MediaError> {
        if !self.rdo.lambda.is_finite() {
            return Err(MediaError::Invalid(format!("lambda {} is not a number", self.rdo.lambda)));
        }
        self.rdo.lambda = self.rdo.lambda.clamp(0.0, 500.0);
        self.rdo.lookback = self.rdo.lookback.clamp(8, 131_072);
        self.rdo.partitions = self.rdo.partitions.clamp(1, 128);
        if self.chunks == 0 {
            return Err(MediaError::Invalid("chunks must be at least 1".into()));
        }
        if !(1..=22).contains(&self.zstd_level) {
            return Err(MediaError::Invalid(format!("zstd level {} (1..=22)", self.zstd_level)));
        }
        Ok(self)
    }
}

fn check_frame(rgba: &[u8], width: u32, height: u32) -> Result<(), MediaError> {
    if width == 0 || height == 0 || width > dxvkit_plus_core::MAX_DIMENSION || height > dxvkit_plus_core::MAX_DIMENSION {
        return Err(MediaError::Invalid(format!("frame size {width}x{height} out of range")));
    }
    let want = (width as usize).saturating_mul(height as usize).saturating_mul(4);
    if rgba.len() != want {
        return Err(MediaError::Invalid(format!("{width}x{height} RGBA8 frame needs {want} bytes, got {}", rgba.len())));
    }
    Ok(())
}

/// Encode one straight-RGBA8 frame (`width * 4` stride) to its BC7 block stream
/// (`ceil(W/4) * ceil(H/4) * 16` bytes). `threads` speeds up the per-block stage only.
pub fn encode_bc7(rgba: &[u8], width: u32, height: u32, params: &EncodeParams, threads: usize) -> Result<Vec<u8>, MediaError> {
    check_frame(rgba, width, height)?;
    let params = params.validated()?;
    let blocks = rdo::encode_image(rgba, width, height, &params.bc7, &params.rdo, threads);
    Ok(blocks.concat())
}

/// Split a frame's BC7 blocks into `params.chunks` block-aligned chunks, compress each, and
/// assemble the frame payload.
pub fn pack_frame(header: &Header, blocks: &[u8], params: &EncodeParams) -> Result<Vec<u8>, MediaError> {
    if blocks.len() as u64 != header.frame_bytes() {
        return Err(MediaError::Invalid(format!("{} BC7 bytes for a frame of {}", blocks.len(), header.frame_bytes())));
    }
    let runs = chunk_blocks(header.frame_bytes() / 16, params.chunks)?;
    let mut packed: Vec<(u32, Vec<u8>)> = Vec::with_capacity(runs.len());
    let mut at = 0usize;
    for n in runs {
        let len = usize::try_from(n.saturating_mul(16)).map_err(|_| MediaError::Invalid("chunk too large".into()))?;
        let end = at.saturating_add(len);
        let src = blocks.get(at..end).ok_or_else(|| MediaError::Invalid("chunk outside the frame".into()))?;
        let data = match params.compressor {
            Compressor::None => src.to_vec(),
            Compressor::Zstd => zstd::bulk::compress(src, params.zstd_level).map_err(|e| MediaError::Io(format!("zstd: {e}")))?,
        };
        packed.push((u32::try_from(len).map_err(|_| MediaError::Invalid("chunk over 4 GiB".into()))?, data));
        at = end;
    }
    let chunks: Vec<ChunkData<'_>> = packed.iter().map(|(raw, data)| ChunkData { compressor: params.compressor, raw_size: *raw, data }).collect();
    frame_payload(header, &chunks)
}

/// Encode one frame (straight RGBA8 at `header`'s size) all the way to its payload.
pub fn encode_frame(header: &Header, rgba: &[u8], params: &EncodeParams, threads: usize) -> Result<Vec<u8>, MediaError> {
    let blocks = encode_bc7(rgba, header.width, header.height, params, threads)?;
    pack_frame(header, &blocks, params)
}

/// Encode frames into an LMBD file written to `out`. `next_frame(i)` returns frame `i` as straight
/// RGBA8 at `header`'s size, or `None` after the last. Up to `threads` frames encode at once (each
/// frame's output does not depend on the others, so the file is the same for any thread count);
/// frames are written in order and `progress(i, payload_bytes)` is called as frame `i` is
/// written. Returns the final header.
pub fn encode_frames<W, F, P>(out: W, header: Header, params: &EncodeParams, threads: usize, mut next_frame: F, mut progress: P) -> Result<Header, MediaError>
where
    W: Write + Seek,
    F: FnMut(u32) -> Option<Result<Vec<u8>, MediaError>>,
    P: FnMut(u32, usize),
{
    let params = params.validated()?;
    let mut writer = Writer::new(out, header)?;
    let header = writer.header().clone();
    let threads = threads.clamp(1, 64);
    let mut index = 0u32;
    loop {
        // Gather up to `threads` frames, encode them concurrently, write them in order.
        let mut batch = Vec::with_capacity(threads);
        while batch.len() < threads {
            match next_frame(index.saturating_add(batch.len() as u32)) {
                Some(f) => batch.push(f?),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        for f in &batch {
            check_frame(f, header.width, header.height)?;
        }
        let per_frame_threads = if batch.len() == 1 { threads } else { 1 };
        let payloads: Vec<Result<Vec<u8>, MediaError>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch.iter().map(|f| s.spawn(|| encode_frame(&header, f, &params, per_frame_threads))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(MediaError::Invalid("encoder worker panicked".into())))).collect()
        });
        for p in payloads {
            let p = p?;
            writer.push_payload(&p)?;
            progress(index, p.len());
            index = index.saturating_add(1);
        }
    }
    let (_, header) = writer.finish()?;
    Ok(header)
}
