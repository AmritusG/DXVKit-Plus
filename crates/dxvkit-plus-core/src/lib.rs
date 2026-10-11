//! # dxvkit-plus-core
//!
//! Video the way VJs use it, in Rust — no `unsafe` in this crate, no GPU dependency, no FFmpeg,
//! GStreamer or libmpv. One C library is linked by default: libzstd (BSD-3-Clause), for LMBD's
//! Zstd chunks; the `zstd-rust` feature (with default features off) swaps it for the pure-Rust
//! `ruzstd` and the crate is pure Rust again:
//!
//! - [`container`]: our own QuickTime / MP4 demuxer for video tracks, including the QuickTime
//!   timecode track (`tmcd`).
//! - [`dxv`]: DXV decode (DXV3 DXT1 / DXT5 / YCG6 / YG10, legacy DXV1/2 `DXDI`) to block streams —
//!   a safe-Rust port of DXVKit's C decoder; alpha read as straight (default) or premultiplied
//!   ([`source::AlphaMode`]).
//! - [`hap`]: HAP decode, every variant (Hap1, Hap5, HapY, HapM, HapA, Hap7 / HAP R, HapH), Snappy
//!   and chunked frames (chunks decompress in parallel), from the public Vidvox specification.
//! - [`lmbd`]: our own GPU codec's container (profile LMB7: BC7 blocks in Zstd-compressed,
//!   block-aligned chunks) — read, validate, decode to BC7 blocks, and write.
//! - [`bc`] / [`bc7`]: CPU block decoders (BC1, BC3, BC4, BC7) — the oracle for the GPU path and
//!   the route to CPU RGBA ([`source::Decoded::to_rgba`]).
//! - [`source`]: probe + decode for movies, PNG image sequences and stills. Output is GPU-ready:
//!   compressed block planes plus how to shade them ([`source::Shading`]), or a platform frame.
//! - [`backend`]: the trait platform decoders implement (macOS: `dxvkit-plus-apple`; Windows: a
//!   documented stub for Media Foundation + DX12).
//! - [`cache`]: a frame cache with read-ahead on worker threads; a miss decodes the exact frame.
//! - [`transport`]: an exact-time playhead — loop, bounce, once, random; forward, reverse, any
//!   rational speed; frame-exact seek, step, scrub, cues; beat and timecode sync.
//! - [`time`]: exact integer media time ([`time::Tick`]) and timecode formatting.
#![forbid(unsafe_code)]

pub mod backend;
pub mod bc;
pub mod bc7;
mod bytes;
pub mod cache;
pub mod container;
pub mod dxv;
mod error;
pub mod hap;
pub mod lmbd;
pub mod lmbx;
pub mod source;
pub mod testpattern;
pub mod time;
pub mod transport;

pub use error::MediaError;

/// Largest frame edge accepted from any file (pixels).
pub const MAX_DIMENSION: u32 = 16_384;
