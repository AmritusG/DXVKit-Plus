//! # dxvkit-plus-enc — the DXV3 and HAP encoders and their QuickTime writer
//!
//! RGBA frames in, `.mov` files out, byte for byte what Swift GlEnc 1.6.2 writes for the same pixels:
//!
//! - [`dxv`]: DXV3 DXT1 / DXT5 / YCG6 / YG10 — DXVKit 1.0.0's encoder (Amrit Rosell, MIT) in safe Rust, at the
//!   BEST effort GlEnc uses, including the fused multiply-adds Apple clang forms when SwiftPM builds DXVKit's C core
//!   (24 `fmadd` / `fnmadd` in `enc_bc.o`), so the blocks equal Swift GlEnc's;
//! - [`hap`]: Hap1 / Hap5 / HapY / HapA / HapM, single-section and chunked — a port of Swift GlEnc's own HAP code
//!   (its BC1 port of FFmpeg's `texturedspenc.c`, its BC4 / BC3 / scaled-YCoCg / alpha packers, its Snappy
//!   compressor, its section and chunk writers);
//! - [`mov`]: the QuickTime writer for both (Swift GlEnc's `VariantMOVWriter`: one sample per chunk, `co64` and the
//!   largesize `mdat` past 4 GB, the `in32` / `lpcm` v2 audio track, `udta/©swr`);
//! - [`alpha`]: the straight-alpha rule every writer follows (un-premultiply on read, force opaque).
//!
//! Inputs are tightly packed RGBA8, top row first, at the presentation size. No pixel is premultiplied here.
#![forbid(unsafe_code)]

pub mod alpha;
pub mod dxv;
mod error;
pub mod hap;
pub mod mov;

pub use error::EncodeError;
