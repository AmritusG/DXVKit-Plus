//! LMBX (LMBD profile `0x10`, texture format `0x11`): the variable-rate, per-tile-independent
//! profile — YCoCg-R, an 8x8 DCT with an exactly specified integer inverse, per-block adaptive
//! quantisation and rANS with one independent stream per plane of every 32x32 tile, so a GPU
//! decodes thousands of streams at once (the compute passes in `dxvkit-plus-wgpu`). This module is the
//! normative reference decoder of the three frozen bitstream versions — v1 (DCT tiles), v2 (flat and
//! palette tiles), v3 (context-coded palettes of up to 64 colours, edge alpha, quad tiles, alpha
//! palettes) — specified in `docs/LMBD.md` ("LMBX").
//!
//! Every step of the decode is integer arithmetic with a fixed definition for every input that
//! passes [`Frame::parse`] (a tile stream that ends early reads zero bytes; the rANS
//! renormalisation is bounded), so every conforming decoder produces bit-identical samples. The
//! output is RGBA in quarter code values (`0..=1020`, 10 bits of precision); 8-bit RGBA is
//! `(v + 2) >> 2`.
//!
//! Lineage: the design of Amrit's LMBX prototypes (`loomda-codec` @ c6d80e6, `lmbx_b1d.cpp` /
//! `LmbxMetal.swift`, LMXS v4), rewritten here as a frozen v1 bitstream: an integer inverse
//! transform (LMBX-D1), signalled 4:2:2-halo and 4:4:4 chroma (LMBX-D2), arbitrary frame sizes,
//! optional alpha, 32x32 tiles with a stream per plane and the quantiser map stored raw (for
//! parallel decode; measured), a compact rANS table. Prototype bitstreams are refused
//! (`Unsupported`).
//
// Lint note: the sample arithmetic is bounded by the clamps below (dequantised coefficients to
// +-4095; the IDCT sums are proven inside i32 in docs/LMBD.md); array indexing is only into the
// fixed 64-entry block arrays with loop indices below 64 (and a 4-bit index into the 16 quantiser
// steps). Both lints are relaxed for this module only. Every read of file bytes and every plane
// access is bounds-checked (`get`).
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use crate::bytes::{arr, byte, le16, le32};
use crate::error::MediaError;

/// Frame bitstream magic.
pub const MAGIC: [u8; 4] = *b"LMBX";
/// Frame bitstream version 1 (0.5.0): DCT tiles only. Still decoded.
pub const VERSION: u16 = 1;
/// Frame bitstream version 2 (0.6.0): adds per-tile modes (flat, palette) and a stream-length
/// directory.
pub const VERSION_2: u16 = 2;
/// Palette-index alphabet (v2): a palette has at most 16 colours.
pub const PALETTE_SYMBOLS: usize = 16;
/// Frame bitstream version 3 (0.7.0): context-coded palette indices, palettes of up to 64
/// colours, quad tiles (four 16x16 direct sub-tiles), exact edge alpha on palette colours with
/// partial alpha, and DCT colour with an alpha palette.
pub const VERSION_3: u16 = 3;
/// v3: the most colours a palette holds.
pub const PAL3_MAX: usize = 64;
/// v3 palette-index alphabet: 0 copy-left, 1 copy-above, `1 + d` the index `d` steps after the
/// left one (`d` in `1..=63`).
pub const PAL3_SYMBOLS: usize = PAL3_MAX + 1;
/// v3 palette-index contexts (one table each): `4 (left == above) + 2 (above == above-right) +
/// (above-left == left)`.
pub const PAL3_CTX: usize = 8;
/// v3 edge-alpha residual alphabet.
pub const ALPHA_SYMBOLS: usize = 256;
/// v3 precision of the index and edge-alpha tables: their frequencies add up to `1 << IDX_BITS`.
pub const IDX_BITS: u32 = 10;

/// How a tile is coded (v2, v3; every v1 tile is [`TileMode::Dct`]). `#[repr(u32)]` = the byte in
/// the v2 tile mode map, the low nibble of the v3 one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum TileMode {
    /// YCoCg planes, DCT, per-plane rANS streams (v1's coding).
    Dct = 0,
    /// One RGBA8 colour (stream 0 holds the four bytes).
    Flat = 1,
    /// Up to 16 RGBA8 colours (v3: 64) and a rANS-coded index per pixel (stream 0).
    Palette = 2,
    /// v3: four 16x16 direct sub-tiles, each flat or palette (stream 0).
    Quad = 3,
}

/// v3: where a tile's alpha comes from (the high nibble of its mode byte). `#[repr(u32)]` = the
/// nibble.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum AlphaMode {
    /// The colour mode's own: a direct tile's RGBA, a DCT tile's A plane (every v1 / v2 tile).
    Own = 0,
    /// A palette or quad tile's colours, made exact on pixels of partial alpha by a residual each.
    Edge = 1,
    /// A DCT tile's alpha as a palette of up to 64 levels (its A stream).
    Palette = 2,
}

impl TileMode {
    fn from_byte(b: u8) -> Option<TileMode> {
        match b {
            0 => Some(TileMode::Dct),
            1 => Some(TileMode::Flat),
            2 => Some(TileMode::Palette),
            _ => None,
        }
    }
    /// A v3 mode byte (colour mode | alpha mode << 4); `alpha`: the frame codes alpha.
    fn from_byte3(b: u8, alpha: bool) -> Option<(TileMode, AlphaMode)> {
        let m = match b & 15 {
            0 => TileMode::Dct,
            1 => TileMode::Flat,
            2 => TileMode::Palette,
            3 => TileMode::Quad,
            _ => return None,
        };
        let a = match b >> 4 {
            0 => AlphaMode::Own,
            1 if alpha && matches!(m, TileMode::Palette | TileMode::Quad) => AlphaMode::Edge,
            2 if alpha && m == TileMode::Dct => AlphaMode::Palette,
            _ => return None,
        };
        Some((m, a))
    }
}
/// Tile edge in pixels (each plane of each tile is an independent stream).
pub const TILE: u32 = 32;
/// 8x8 blocks per tile edge.
const TB: u32 = TILE / 8;
/// Bytes of a tile's quantiser map: one four-bit index per 8x8 luma block.
pub const QMAP_BYTES: usize = (TB * TB / 2) as usize;
/// rANS probability precision: frequencies add up to `1 << PROB_BITS`.
pub const PROB_BITS: u32 = 14;
/// `1 << PROB_BITS`.
pub const PROB_SCALE: u32 = 1 << PROB_BITS;
/// rANS lower bound of the normalised state.
pub const RANS_L: u32 = 1 << 23;
/// Coefficient alphabet: symbol `s < 2047` is level `s - 1023`; 2047 ends the block.
pub const COEF_SYMBOLS: usize = 2048;
/// End-of-block symbol.
pub const EOB: u16 = 2047;
/// Level offset of a coefficient symbol.
pub const LEVEL_OFFSET: i32 = 1023;
/// Largest quantiser step.
pub const MAX_Q: u32 = 4095;
/// Dequantised coefficients are clamped to `-DEQUANT_MAX..=DEQUANT_MAX`.
pub const DEQUANT_MAX: i32 = 4095;
/// Output samples are quarter code values, `0..=OUT_MAX`.
pub const OUT_MAX: i32 = 1020;
/// Fixed byte size of the frame header (before the frequency table).
pub const HEADER_BYTES: usize = 52;

/// Zigzag order: coefficient `k` of a block's symbol stream is raster position `ZIGZAG[k]`
/// (`row * 8 + column`, row = vertical frequency).
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29,
    22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The integer DCT-II basis, `M[k][n]`, frequency `k`, sample `n`: `1448` for `k = 0`, else
/// `round(2048 cos((2n + 1) k pi / 16))` — the orthonormal basis scaled by 4096. Normative.
pub const IDCT_MATRIX: [[i32; 8]; 8] = [
    [1448, 1448, 1448, 1448, 1448, 1448, 1448, 1448],
    [2009, 1703, 1138, 400, -400, -1138, -1703, -2009],
    [1892, 784, -784, -1892, -1892, -784, 784, 1892],
    [1703, -400, -2009, -1138, 1138, 2009, 400, -1703],
    [1448, -1448, -1448, 1448, 1448, -1448, -1448, 1448],
    [1138, -2009, 400, 1703, -1703, -400, 2009, -1138],
    [784, -1892, 1892, -784, -784, 1892, -1892, 784],
    [400, -1138, 1703, -2009, 2009, -1703, 1138, -400],
];

/// Chroma layout (frame header byte 6). `#[repr(u32)]` = the byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Chroma {
    /// Co and Cg at full resolution.
    Full444 = 0,
    /// Co and Cg at half horizontal resolution; odd columns interpolate with the next chroma
    /// column, across tile edges (the "halo", read in a second pass).
    Halo422 = 1,
}

/// The planes of a tile, in stream order.
pub const PLANES: [PlaneKind; 4] = [PlaneKind::Y, PlaneKind::Co, PlaneKind::Cg, PlaneKind::A];

impl Chroma {
    /// Chroma plane width for a coded luma width.
    pub fn plane_width(self, coded_width: u32) -> u32 {
        match self {
            Chroma::Full444 => coded_width,
            Chroma::Halo422 => coded_width / 2,
        }
    }
    /// Chroma tile width in pixels.
    pub fn tile_width(self) -> u32 {
        match self {
            Chroma::Full444 => TILE,
            Chroma::Halo422 => TILE / 2,
        }
    }
}

/// A validated rANS frequency table, expanded for decoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreqTable {
    /// Frequency per symbol (0 = absent).
    pub freq: Vec<u16>,
    /// Cumulative frequency, `cum[s] = sum(freq[..s])`; `freq.len() + 1` entries.
    pub cum: Vec<u32>,
    /// The symbol owning each of the `1 << bits` slots.
    pub slot2sym: Vec<u16>,
    /// Precision: the frequencies add up to `1 << bits` ([`PROB_BITS`]; v3 index and edge-alpha
    /// tables [`IDX_BITS`]).
    pub bits: u32,
}

impl FreqTable {
    /// Build from dense frequencies that add up to exactly [`PROB_SCALE`].
    pub fn from_freq(freq: Vec<u16>) -> Result<FreqTable, MediaError> {
        FreqTable::from_freq_bits(freq, PROB_BITS)
    }

    /// Build from dense frequencies that add up to exactly `1 << bits`.
    pub fn from_freq_bits(freq: Vec<u16>, bits: u32) -> Result<FreqTable, MediaError> {
        let scale = 1u32 << bits;
        let mut cum = Vec::with_capacity(freq.len() + 1);
        let mut acc = 0u32;
        cum.push(0);
        for &f in &freq {
            acc += u32::from(f);
            cum.push(acc);
        }
        if acc != scale {
            return Err(lmbx(format!("frequency table adds up to {acc}, not {scale}")));
        }
        let mut slot2sym = vec![0u16; scale as usize];
        for (s, w) in cum.windows(2).enumerate() {
            if let [a, b] = w
                && let Some(dst) = slot2sym.get_mut(*a as usize..*b as usize)
            {
                dst.fill(s as u16);
            }
        }
        Ok(FreqTable { freq, cum, slot2sym, bits })
    }

    /// Parse the sparse form at `at`: `u16 n`, then `n` x (`u16 symbol`, `u16 freq`), symbols
    /// strictly increasing below `symbols`, every freq >= 1, sum [`PROB_SCALE`]. Returns the table
    /// and the offset after it.
    pub fn parse(b: &[u8], at: usize, symbols: usize) -> Result<(FreqTable, usize), MediaError> {
        FreqTable::parse_bits(b, at, symbols, PROB_BITS)
    }

    /// [`FreqTable::parse`] of a table whose frequencies add up to `1 << bits`.
    pub fn parse_bits(b: &[u8], at: usize, symbols: usize, bits: u32) -> Result<(FreqTable, usize), MediaError> {
        let short = || lmbx("frequency table truncated".into());
        let n = le16(b, at).ok_or_else(short)? as usize;
        if n == 0 || n > symbols {
            return Err(lmbx(format!("frequency table with {n} entries (1..={symbols})")));
        }
        let mut freq = vec![0u16; symbols];
        let mut prev: Option<usize> = None;
        for i in 0..n {
            let o = at + 2 + i * 4;
            let (s, f) = (le16(b, o).ok_or_else(short)? as usize, le16(b, o + 2).ok_or_else(short)?);
            if s >= symbols || prev.is_some_and(|p| s <= p) || f == 0 {
                return Err(lmbx(format!("frequency table entry {i}: symbol {s}, freq {f}")));
            }
            prev = Some(s);
            if let Some(d) = freq.get_mut(s) {
                *d = f as u16;
            }
        }
        Ok((FreqTable::from_freq_bits(freq, bits)?, at + 2 + n * 4))
    }

    /// The sparse form ([`FreqTable::parse`]) of dense frequencies.
    pub fn sparse_bytes(freq: &[u16]) -> Vec<u8> {
        let present: Vec<(usize, u16)> = freq.iter().copied().enumerate().filter(|&(_, f)| f > 0).collect();
        let mut b = Vec::with_capacity(2 + present.len() * 4);
        b.extend_from_slice(&(present.len() as u16).to_le_bytes());
        for (s, f) in present {
            b.extend_from_slice(&(s as u16).to_le_bytes());
            b.extend_from_slice(&f.to_le_bytes());
        }
        b
    }
}

/// The fixed frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FrameHeader {
    /// [`VERSION`] (1), [`VERSION_2`] or [`VERSION_3`].
    pub version: u16,
    pub chroma: Chroma,
    /// An alpha plane is coded (otherwise alpha is 1).
    pub alpha: bool,
    /// Picture size (the coded size is this rounded up to whole 64x64 tiles).
    pub width: u32,
    pub height: u32,
    /// Chroma quantiser ratio in 1/256 (>= 1).
    pub chroma_ratio: u16,
    /// Quantiser steps, each `1..=MAX_Q`, indexed by the per-block quantiser index.
    pub qt: [u16; 16],
}

impl FrameHeader {
    pub fn tiles_x(&self) -> u32 {
        self.width.div_ceil(TILE)
    }
    pub fn tiles_y(&self) -> u32 {
        self.height.div_ceil(TILE)
    }
    pub fn tile_count(&self) -> u32 {
        self.tiles_x() * self.tiles_y()
    }
    /// Streams per tile: Y, Co, Cg, and A when alpha is coded.
    pub fn planes(&self) -> u32 {
        3 + u32::from(self.alpha)
    }
    /// Every stream of the frame (`tile_count * planes`), tile-major.
    pub fn stream_count(&self) -> u32 {
        self.tile_count() * self.planes()
    }
    /// The coded size, whole 32x32 tiles.
    pub fn coded_size(&self) -> (u32, u32) {
        (self.tiles_x() * TILE, self.tiles_y() * TILE)
    }
    /// Quantiser step of a chroma block whose co-located luma block has step `q`. At least 2:
    /// Co and Cg span -255..=255, so their DC reaches +-2040 and every level must fit +-1023.
    pub fn chroma_q(&self, q: u32) -> u32 {
        ((q * u32::from(self.chroma_ratio) + 128) >> 8).clamp(2, MAX_Q)
    }

    /// The 52 header bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_BYTES] {
        let mut b = [0u8; HEADER_BYTES];
        let mut put = |at: usize, v: &[u8]| {
            if let Some(d) = b.get_mut(at..at + v.len()) {
                d.copy_from_slice(v);
            }
        };
        put(0, &MAGIC);
        put(4, &self.version.to_le_bytes());
        put(6, &[self.chroma as u8, u8::from(self.alpha)]);
        put(8, &self.width.to_le_bytes());
        put(12, &self.height.to_le_bytes());
        put(16, &self.chroma_ratio.to_le_bytes());
        for (i, q) in self.qt.iter().enumerate() {
            put(20 + i * 2, &q.to_le_bytes());
        }
        b
    }
}

/// Byte offsets of a frame's sections (for decoders that read the bitstream in place, such as
/// the GPU decoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Sections {
    pub coef_table: usize,
    /// v2: the palette-index table and the tile mode map (one byte per tile); 0 for v1. v3: the
    /// first of the eight index tables.
    pub palette_table: usize,
    /// v3: the eight index tables and the edge-alpha table (0 before v3).
    pub tables3: [usize; 9],
    pub modes: usize,
    /// The quantiser map: [`QMAP_BYTES`] per tile, tile-major; block `b` (raster in the tile's 4x4
    /// luma blocks) is the low nibble of byte `b / 2` for even `b`, the high nibble for odd.
    pub qmap: usize,
    /// The `u32` stream count, followed by the stream offsets.
    pub directory: usize,
    /// First byte of stream data (stream offsets are relative to it).
    pub data: usize,
}

/// A parsed, validated LMBX frame bitstream (borrowing its bytes).
#[derive(Clone, Debug)]
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub coef: FreqTable,
    /// The v2 palette-index table (v1, v3: an unused single-symbol table).
    pub palette: FreqTable,
    /// v3: the palette-index tables, one per context (empty before v3).
    pub idx: Vec<FreqTable>,
    /// v3: the edge-alpha table (before v3: an unused single-symbol table).
    pub edge: FreqTable,
    /// Start of every stream (tile `t`, plane `k` is stream `t * planes + k`), relative to
    /// [`Sections::data`]; a stream ends where the next starts (the last at the end of the
    /// bitstream).
    pub offsets: Vec<u32>,
    pub sections: Sections,
    pub bytes: &'a [u8],
}

fn lmbx(m: String) -> MediaError {
    MediaError::Lmbd(format!("lmbx: {m}"))
}

/// Whether `b` is an LMBX prototype bitstream (`LMXS`, never frozen; refused).
pub fn is_prototype(b: &[u8]) -> bool {
    b.get(..4) == Some(b"LMXS")
}

impl<'a> Frame<'a> {
    /// Parse and validate a frame bitstream: magic, version, chroma, flags, size (against the
    /// container's), quantiser steps, the frequency table, the quantiser map's extent, the stream
    /// directory.
    pub fn parse(b: &'a [u8], width: u32, height: u32) -> Result<Frame<'a>, MediaError> {
        let short = || lmbx("frame header truncated".into());
        if is_prototype(b) {
            return Err(MediaError::Unsupported(
                "LMBX prototype bitstream (LMXS, never frozen): re-encode with `dxvkit-plus encode-lmbd --profile lmbx`".into(),
            ));
        }
        let magic: [u8; 4] = arr(b, 0).ok_or_else(short)?;
        if magic != MAGIC {
            return Err(lmbx(format!("bad frame magic {magic:02X?}")));
        }
        let version = le16(b, 4).ok_or_else(short)?;
        if version != u32::from(VERSION) && version != u32::from(VERSION_2) && version != u32::from(VERSION_3) {
            return Err(MediaError::Unsupported(format!("LMBX frame version {version} (this decoder knows versions {VERSION} to {VERSION_3})")));
        }
        let v3 = version == u32::from(VERSION_3);
        let v2 = version == u32::from(VERSION_2) || v3;
        let chroma = match byte(b, 6).ok_or_else(short)? {
            0 => Chroma::Full444,
            1 => Chroma::Halo422,
            other => return Err(lmbx(format!("unknown chroma mode {other}"))),
        };
        let flags = byte(b, 7).ok_or_else(short)?;
        if flags & !1 != 0 {
            return Err(lmbx(format!("unknown flags 0x{flags:02X}")));
        }
        let (w, h) = (le32(b, 8).ok_or_else(short)?, le32(b, 12).ok_or_else(short)?);
        if (w, h) != (width, height) {
            return Err(lmbx(format!("frame is {w}x{h}, the file {width}x{height}")));
        }
        if w == 0 || h == 0 || w > crate::MAX_DIMENSION || h > crate::MAX_DIMENSION {
            return Err(lmbx(format!("frame size {w}x{h}")));
        }
        let chroma_ratio = le16(b, 16).ok_or_else(short)? as u16;
        if chroma_ratio == 0 || le16(b, 18).ok_or_else(short)? != 0 {
            return Err(lmbx("chroma ratio 0 or reserved bytes set".into()));
        }
        let mut qt = [0u16; 16];
        for (i, q) in qt.iter_mut().enumerate() {
            let v = le16(b, 20 + i * 2).ok_or_else(short)?;
            if v == 0 || v > MAX_Q {
                return Err(lmbx(format!("quantiser step {i} = {v} (1..={MAX_Q})")));
            }
            *q = v as u16;
        }
        let header = FrameHeader { version: version as u16, chroma, alpha: flags & 1 == 1, width: w, height: h, chroma_ratio, qt };
        let coef_table = HEADER_BYTES;
        let (coef, after) = FreqTable::parse(b, coef_table, COEF_SYMBOLS)?;
        let single = || FreqTable::from_freq(vec![PROB_SCALE as u16]);
        let (mut idx, mut edge, mut tables3) = (Vec::new(), single()?, [0usize; 9]);
        let (palette, palette_table, modes, qmap) = if v3 {
            let mut at = after;
            for (k, slot) in tables3.iter_mut().enumerate() {
                *slot = at;
                let (t, n) = FreqTable::parse_bits(b, at, if k < PAL3_CTX { PAL3_SYMBOLS } else { ALPHA_SYMBOLS }, IDX_BITS)?;
                if k < PAL3_CTX {
                    idx.push(t);
                } else {
                    edge = t;
                }
                at = n;
            }
            let map = b.get(at..at + header.tile_count() as usize).ok_or_else(short)?;
            if let Some(i) = map.iter().position(|&x| TileMode::from_byte3(x, header.alpha).is_none()) {
                return Err(lmbx(format!("tile {i} mode byte {}", map.get(i).copied().unwrap_or(0))));
            }
            (single()?, after, at, at + header.tile_count() as usize)
        } else if v2 {
            let (palette, m) = FreqTable::parse(b, after, PALETTE_SYMBOLS)?;
            let map = b.get(m..m + header.tile_count() as usize).ok_or_else(short)?;
            if let Some(i) = map.iter().position(|&x| TileMode::from_byte(x).is_none()) {
                return Err(lmbx(format!("tile {i} mode {}", map.get(i).copied().unwrap_or(0))));
            }
            (palette, after, m, m + header.tile_count() as usize)
        } else {
            (single()?, 0, 0, after)
        };
        let directory = qmap + header.tile_count() as usize * QMAP_BYTES;
        let nt = le32(b, directory).ok_or_else(short)?;
        if nt != header.stream_count() {
            return Err(lmbx(format!("{nt} streams, a {w}x{h} frame has {}", header.stream_count())));
        }
        let data = directory + 4 + nt as usize * 4;
        if data > b.len() {
            return Err(lmbx("tile directory truncated".into()));
        }
        let data_len = b.len() - data;
        let mut offsets = Vec::with_capacity(nt as usize);
        let mut prev = 0u32;
        for i in 0..nt as usize {
            let v = le32(b, directory + 4 + i * 4).ok_or_else(short)?;
            if v2 {
                // v2: the directory holds stream lengths; offsets are their running sum.
                offsets.push(prev);
                prev = prev
                    .checked_add(v)
                    .filter(|&e| e as usize <= data_len)
                    .ok_or_else(|| lmbx(format!("stream {i} length {v} (at {prev}, {data_len} data bytes)")))?;
                continue;
            }
            let o = v;
            if (i == 0 && o != 0) || o < prev || o as usize > data_len {
                return Err(lmbx(format!("stream {i} offset {o} (previous {prev}, {data_len} data bytes)")));
            }
            prev = o;
            offsets.push(o);
        }
        Ok(Frame {
            header,
            coef,
            palette,
            idx,
            edge,
            offsets,
            sections: Sections { coef_table, palette_table, tables3, modes, qmap, directory, data },
            bytes: b,
        })
    }

    /// Tile `t`'s mode (v1: always DCT).
    pub fn mode(&self, t: u32) -> TileMode {
        if self.header.version < VERSION_2 {
            return TileMode::Dct;
        }
        let m = self.bytes.get(self.sections.modes + t as usize).copied();
        if self.header.version >= VERSION_3 {
            return m.and_then(|m| TileMode::from_byte3(m, self.header.alpha)).map_or(TileMode::Dct, |m| m.0);
        }
        m.and_then(TileMode::from_byte).unwrap_or(TileMode::Dct)
    }

    /// Tile `t`'s alpha mode (before v3: always the colour mode's own).
    pub fn alpha_mode(&self, t: u32) -> AlphaMode {
        if self.header.version < VERSION_3 {
            return AlphaMode::Own;
        }
        self.bytes.get(self.sections.modes + t as usize).and_then(|&m| TileMode::from_byte3(m, self.header.alpha)).map_or(AlphaMode::Own, |m| m.1)
    }

    /// The quantiser index of luma block `b` (raster, `0..16`) of tile `t`.
    pub fn qidx(&self, t: u32, b: u32) -> usize {
        let at = self.sections.qmap + t as usize * QMAP_BYTES + (b / 2) as usize;
        let v = self.bytes.get(at).copied().unwrap_or(0);
        usize::from(if b.is_multiple_of(2) { v & 15 } else { v >> 4 })
    }

    /// Stream `i`.
    pub fn stream(&self, i: usize) -> &'a [u8] {
        let data = self.bytes.get(self.sections.data..).unwrap_or_default();
        let start = self.offsets.get(i).copied().unwrap_or(0) as usize;
        let end = self.offsets.get(i + 1).map_or(data.len(), |&e| e as usize);
        data.get(start..end).unwrap_or_default()
    }
}

/// rANS decoder over one tile stream. Bytes past the end read as 0; renormalisation reads at
/// most four bytes per symbol. State arithmetic wraps (u32).
struct Rans<'a> {
    s: &'a [u8],
    pos: usize,
    x: u32,
}

impl<'a> Rans<'a> {
    fn new(s: &'a [u8]) -> Rans<'a> {
        let b = |i: usize| u32::from(s.get(i).copied().unwrap_or(0));
        Rans { s, pos: 4, x: b(0) | (b(1) << 8) | (b(2) << 16) | (b(3) << 24) }
    }
    #[inline]
    fn sym(&mut self, t: &FreqTable) -> u16 {
        let slot = self.x & ((1 << t.bits) - 1);
        let s = t.slot2sym.get(slot as usize).copied().unwrap_or(0);
        let f = u32::from(t.freq.get(s as usize).copied().unwrap_or(1));
        let c = t.cum.get(s as usize).copied().unwrap_or(0);
        self.x = f.wrapping_mul(self.x >> t.bits).wrapping_add(slot).wrapping_sub(c);
        for _ in 0..4 {
            if self.x >= RANS_L {
                break;
            }
            let b = u32::from(self.s.get(self.pos).copied().unwrap_or(0));
            self.pos += 1;
            self.x = (self.x << 8) | b;
        }
        s
    }
}

/// Integer inverse DCT of dequantised raster coefficients `d` into quarter-unit samples
/// (`out[i * 8 + j]`, row `i`, column `j`), plus `offset`. Normative.
pub fn idct(d: &[i32; 64], offset: i32, out: &mut [i32; 64]) {
    let m = &IDCT_MATRIX;
    let mut t = [0i32; 64];
    for i in 0..8 {
        for j in 0..8 {
            let mut s = 0i32;
            for (k, row) in m.iter().enumerate() {
                s += row[i] * d[k * 8 + j];
            }
            t[i * 8 + j] = (s + 512) >> 10;
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            let mut s = 0i32;
            for (l, row) in m.iter().enumerate() {
                s += t[i * 8 + l] * row[j];
            }
            out[i * 8 + j] = ((s + 2048) >> 12) + offset;
        }
    }
}

/// Decoded sample planes of one frame (quarter units), at the coded size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planes {
    pub coded_width: u32,
    pub coded_height: u32,
    pub chroma_width: u32,
    pub y: Vec<i32>,
    pub co: Vec<i32>,
    pub cg: Vec<i32>,
    /// Empty without an alpha plane.
    pub a: Vec<i32>,
    /// Tile modes, tile-major (`tiles_x` per row); a non-DCT tile's Y samples hold its RGBA8
    /// packed (`R | G << 8 | B << 16 | A << 24`).
    pub modes: Vec<TileMode>,
    pub tiles_x: u32,
}

/// Read one block's coefficients and dequantise them with step `q` into raster order.
/// The DC step for a block of step `q`: at least 2 (Y, A) or 4 (Co, Cg), so a DC residual —
/// up to +-2040 (Y, A) or +-4080 (Co, Cg) — always fits a level of +-1023.
pub fn dc_step(q: u32, chroma: bool) -> u32 {
    q.max(if chroma { 4 } else { 2 })
}

/// Read one block's coefficients and dequantise them with step `q` into raster order. The DC
/// level is a residual from `dc`, the previous block's dequantised DC in this stream (0 before
/// the first), with step `qdc` ([`dc_step`]): `D[0] = clamp(dc + level * qdc)`, and `dc` becomes
/// it — so a block that repeats its predecessor's DC costs nothing but its end-of-block (an
/// immediate EOB keeps `D[0] = dc`).
fn read_block(r: &mut Rans<'_>, coef: &FreqTable, q: i32, qdc: i32, dc: &mut i32, d: &mut [i32; 64]) {
    *d = [0; 64];
    d[0] = *dc;
    for (k, &z) in ZIGZAG.iter().enumerate() {
        let s = r.sym(coef);
        if s == EOB {
            break;
        }
        let v = if k == 0 { *dc + (i32::from(s) - LEVEL_OFFSET) * qdc } else { (i32::from(s) - LEVEL_OFFSET) * q };
        d[z as usize] = v.clamp(-DEQUANT_MAX, DEQUANT_MAX);
    }
    *dc = d[0];
}

fn put_block(plane: &mut [i32], stride: u32, x0: u32, y0: u32, s: &[i32; 64]) {
    for i in 0..8u32 {
        let at = ((y0 + i) * stride + x0) as usize;
        if let (Some(dst), Some(src)) = (plane.get_mut(at..at + 8), s.get(i as usize * 8..i as usize * 8 + 8)) {
            dst.copy_from_slice(src);
        }
    }
}

/// One tile row of every plane (disjoint slices of the frame's planes).
struct RowOut<'p> {
    stride: u32,
    chroma_stride: u32,
    y: &'p mut [i32],
    co: &'p mut [i32],
    cg: &'p mut [i32],
    a: &'p mut [i32],
}

/// Which plane a block belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    Y,
    Co,
    Cg,
    A,
}

/// Entropy-decode plane `kind` of tile `t` (one stream), calling `visit(x0, y0, coefficients)`
/// for every 8x8 block in bitstream order (raster within the tile) with its dequantised raster
/// coefficients; `x0` is the block's column in its plane, `y0` its row within the tile. A pure
/// function of the stream's bytes, the quantiser map and the frame header and table. The plane must
/// be coded (`A` only with alpha).
pub fn visit_stream(f: &Frame<'_>, t: u32, kind: PlaneKind, mut visit: impl FnMut(u32, u32, &[i32; 64])) {
    let h = &f.header;
    let tx = t % h.tiles_x().max(1);
    let k = match kind {
        PlaneKind::Y => 0,
        PlaneKind::Co => 1,
        PlaneKind::Cg => 2,
        PlaneKind::A => 3,
    };
    let mut r = Rans::new(f.stream((t * h.planes() + k) as usize));
    let q_of = |by: u32, bx: u32| u32::from(h.qt[f.qidx(t, by * TB + bx)]);
    let (mut d, mut dc) = ([0i32; 64], 0i32);
    match kind {
        PlaneKind::Y | PlaneKind::A => {
            for by in 0..TB {
                for bx in 0..TB {
                    let q = q_of(by, bx);
                    read_block(&mut r, &f.coef, q as i32, dc_step(q, false) as i32, &mut dc, &mut d);
                    visit(tx * TILE + bx * 8, by * 8, &d);
                }
            }
        }
        PlaneKind::Co | PlaneKind::Cg => {
            let (cw, cbw) = (h.chroma.tile_width(), h.chroma.tile_width() / 8);
            for by in 0..TB {
                for bx in 0..cbw {
                    let lbx = if h.chroma == Chroma::Halo422 { 2 * bx } else { bx };
                    let q = h.chroma_q(q_of(by, lbx));
                    read_block(&mut r, &f.coef, q as i32, dc_step(q, true) as i32, &mut dc, &mut d);
                    visit(tx * cw + bx * 8, by * 8, &d);
                }
            }
        }
    }
}

/// Decode tile `t` into its tile row `out`: [`visit_stream`] of every coded plane + [`idct`] (Y
/// and A offset by 512, i.e. 128 in code values).
fn decode_tile(f: &Frame<'_>, t: u32, out: &mut RowOut<'_>) {
    if f.mode(t) != TileMode::Dct {
        let px = direct_tile(f, t);
        write_direct(f, t, &px, out);
        return;
    }
    let mut s = [0i32; 64];
    for kind in PLANES.into_iter().take(f.header.planes() as usize) {
        let (plane, stride, offset) = match kind {
            PlaneKind::Y => (&mut *out.y, out.stride, 512),
            PlaneKind::Co => (&mut *out.co, out.chroma_stride, 0),
            PlaneKind::Cg => (&mut *out.cg, out.chroma_stride, 0),
            PlaneKind::A => (&mut *out.a, out.stride, 512),
        };
        if kind == PlaneKind::A && f.alpha_mode(t) == AlphaMode::Palette {
            alpha_palette(f, t, plane, stride);
            continue;
        }
        visit_stream(f, t, kind, |x0, y0, d| {
            idct(d, offset, &mut s);
            put_block(plane, stride, x0, y0, &s);
        });
    }
}

/// The 32x32 RGBA8 pixels (raster) of a non-DCT tile, from its stream 0. Normative:
///
/// - Flat: bytes 0..4 are R, G, B, A.
/// - Palette: byte 0 is `n` (taken as 1 when 0, as 16 when above 16), then `n` RGBA8 colours,
///   then a rANS stream (§ the palette table) of 1024 symbols in raster order; pixel index =
///   `(pred + s) mod n` with `pred` the left pixel's index (the pixel above at column 0, 0 at the
///   tile's first pixel).
///
/// Missing bytes read as 0. Without an alpha plane the alpha of every pixel is 255.
pub fn direct_tile(f: &Frame<'_>, t: u32) -> Vec<[u8; 4]> {
    let h = &f.header;
    if h.version >= VERSION_3 {
        return direct_tile3(f, t);
    }
    let s = f.stream((t * h.planes()) as usize);
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let n_px = (TILE * TILE) as usize;
    let mut px = vec![[0u8; 4]; n_px];
    match f.mode(t) {
        TileMode::Dct | TileMode::Quad => {}
        TileMode::Flat => px.fill([at(0), at(1), at(2), at(3)]),
        TileMode::Palette => {
            let n = usize::from(at(0)).clamp(1, PALETTE_SYMBOLS);
            let pal: Vec<[u8; 4]> = (0..n).map(|i| [at(1 + i * 4), at(2 + i * 4), at(3 + i * 4), at(4 + i * 4)]).collect();
            let mut r = Rans::new(s.get(1 + n * 4..).unwrap_or_default());
            let mut idx = vec![0usize; n_px];
            for i in 0..n_px {
                let (x, y) = (i % TILE as usize, i / TILE as usize);
                let pred = if x > 0 {
                    idx[i - 1]
                } else if y > 0 {
                    idx[i - TILE as usize]
                } else {
                    0
                };
                idx[i] = (pred + usize::from(r.sym(&f.palette))) % n;
                px[i] = pal[idx[i]];
            }
        }
    }
    if !h.alpha {
        px.iter_mut().for_each(|p| p[3] = 255);
    }
    px
}

/// v3: one palette index, context-coded. `l`: the left index (column 0: the index above; the
/// region's first pixel: 0); `a`: above (row 0: `l`); `ar`, `al`: above-right and above-left
/// (outside the region: `a`). The table is `4 (l == a) + 2 (a == ar) + (al == l)`; symbol 0 is
/// `l`, 1 is `a`, `s >= 2` is `(l + s - 1) mod n`.
fn index3(r: &mut Rans<'_>, f: &Frame<'_>, n: usize, l: usize, a: usize, ar: usize, al: usize) -> usize {
    let c = usize::from(l == a) * 4 + usize::from(a == ar) * 2 + usize::from(al == l);
    match f.idx.get(c).map_or(0, |t| r.sym(t)) {
        0 => l,
        1 => a,
        s => (l + usize::from(s) - 1) % n,
    }
}

/// v3: the indices of a `size x size` region in raster order ([`index3`]), each through
/// `pixel(i, index)`.
fn indices3(r: &mut Rans<'_>, f: &Frame<'_>, n: usize, size: usize, mut pixel: impl FnMut(&mut Rans<'_>, usize, usize)) {
    let mut idx = vec![0usize; size * size];
    for i in 0..size * size {
        let (x, y) = (i % size, i / size);
        let l = if x > 0 {
            idx[i - 1]
        } else if y > 0 {
            idx[i - size]
        } else {
            0
        };
        let a = if y > 0 { idx[i - size] } else { l };
        let ar = if y > 0 && x + 1 < size { idx[i - size + 1] } else { a };
        let al = if y > 0 && x > 0 { idx[i - size - 1] } else { a };
        // l, a, ar, al < n, so the index is too.
        idx[i] = index3(r, f, n, l, a, ar, al);
        pixel(r, i, idx[i]);
    }
}

/// v3: a palette header at `s[at..]`: `n` (1..=64; 0 reads as 1, above 64 as 64), then `n` RGBA8
/// colours. Returns the colours and the offset after them.
fn palette3_header(s: &[u8], at: usize) -> (Vec<[u8; 4]>, usize) {
    let b = |i: usize| s.get(i).copied().unwrap_or(0);
    let n = usize::from(b(at)).clamp(1, PAL3_MAX);
    ((0..n).map(|i| [b(at + 1 + 4 * i), b(at + 2 + 4 * i), b(at + 3 + 4 * i), b(at + 4 + 4 * i)]).collect(), at + 1 + 4 * n)
}

/// v3: whether a colour takes an edge-alpha residual (`0 < A < 255`).
fn partial(c: [u8; 4]) -> bool {
    c[3] != 0 && c[3] != 255
}

/// v3 direct tiles, from stream 0 (normative):
///
/// - Flat: bytes 0..4 are R, G, B, A.
/// - Palette: a palette header ([`palette3_header`]), then one rANS stream: per pixel in raster
///   order its index ([`indices3`]), and with [`AlphaMode::Edge`], when its colour has
///   `0 < A < 255`, one edge-alpha symbol `s`: `A = (A + s) mod 256`.
/// - Quad: four 16x16 sub-tiles (top-left, top-right, bottom-left, bottom-right), each a kind byte
///   (0: flat, then 4 colour bytes; otherwise palette, then a palette header); then one rANS
///   stream: the sub-tiles in order, each pixel (raster in the sub-tile) as in a palette tile —
///   a flat sub-tile's pixels have no index symbol, only their edge-alpha symbol.
///
/// Missing bytes read as 0. Without an alpha plane every alpha is 255.
fn direct_tile3(f: &Frame<'_>, t: u32) -> Vec<[u8; 4]> {
    let h = &f.header;
    let s = f.stream((t * h.planes()) as usize);
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let tl = TILE as usize;
    let edge = f.alpha_mode(t) == AlphaMode::Edge;
    let mut px = vec![[0u8; 4]; tl * tl];
    // (origin, size, colours; one colour and no index symbols for a flat region) in stream order.
    let mut regions: Vec<(usize, usize, Vec<[u8; 4]>, bool)> = Vec::new();
    let mut o = 0usize;
    match f.mode(t) {
        TileMode::Dct => {}
        TileMode::Flat => px.fill([at(0), at(1), at(2), at(3)]),
        TileMode::Palette => {
            let (pal, n) = palette3_header(s, 0);
            regions.push((0, tl, pal, false));
            o = n;
        }
        TileMode::Quad => {
            for q in 0..4usize {
                let origin = (q / 2) * 16 * tl + (q % 2) * 16;
                if at(o) == 0 {
                    regions.push((origin, 16, vec![[at(o + 1), at(o + 2), at(o + 3), at(o + 4)]], true));
                    o += 5;
                } else {
                    let (pal, n) = palette3_header(s, o + 1);
                    regions.push((origin, 16, pal, false));
                    o = n;
                }
            }
        }
    }
    if !regions.is_empty() {
        let mut r = Rans::new(s.get(o..).unwrap_or_default());
        for (origin, size, pal, flat) in &regions {
            let mut put = |r: &mut Rans<'_>, i: usize, v: usize| {
                let mut c = pal.get(v).copied().unwrap_or([0; 4]);
                if edge && partial(c) {
                    c[3] = c[3].wrapping_add(r.sym(&f.edge) as u8);
                }
                if let Some(p) = px.get_mut(origin + (i / size) * tl + i % size) {
                    *p = c;
                }
            };
            if *flat {
                (0..size * size).for_each(|i| put(&mut r, i, 0));
            } else {
                indices3(&mut r, f, pal.len(), *size, put);
            }
        }
    }
    if !h.alpha {
        px.iter_mut().for_each(|p| p[3] = 255);
    }
    px
}

/// v3 [`AlphaMode::Palette`] (a DCT tile's alpha, its A stream): byte 0 is `n` (1..=64; 0 reads
/// as 1, above 64 as 64), then `n` alpha levels, then one rANS stream of the 1024 indices
/// ([`indices3`]); A = `4 *` the pixel's level.
fn alpha_palette(f: &Frame<'_>, t: u32, plane: &mut [i32], stride: u32) {
    let s = f.stream((t * f.header.planes() + 3) as usize);
    let n = usize::from(s.first().copied().unwrap_or(0)).clamp(1, PAL3_MAX);
    let mut r = Rans::new(s.get(1 + n..).unwrap_or_default());
    let tx = t % f.header.tiles_x().max(1);
    indices3(&mut r, f, n, TILE as usize, |_, i, v| {
        let (x, y) = (i as u32 % TILE, i as u32 / TILE);
        if let Some(d) = plane.get_mut((y * stride + tx * TILE + x) as usize) {
            *d = i32::from(s.get(1 + v).copied().unwrap_or(0)) * 4;
        }
    });
}

/// Write a direct tile into its tile row: Y samples hold the packed RGBA8; Co and Cg the exact
/// forward YCoCg-R in quarter units (`Co = 4R - 4B`, `t = 4B + (Co >> 1)`, `Cg = 4G - t`; 4:2:2:
/// `(c[2x] + c[2x+1] + 1) >> 1`), for the 4:2:2 halo of DCT neighbours; A samples `4 A`.
fn write_direct(f: &Frame<'_>, t: u32, px: &[[u8; 4]], out: &mut RowOut<'_>) {
    let h = &f.header;
    let tx = t % h.tiles_x().max(1);
    let fwd = |p: [u8; 4]| {
        let (r, g, b) = (i32::from(p[0]) * 4, i32::from(p[1]) * 4, i32::from(p[2]) * 4);
        let co = r - b;
        let tt = b + (co >> 1);
        (co, g - tt)
    };
    for y in 0..TILE {
        for x in 0..TILE {
            let p = px.get((y * TILE + x) as usize).copied().unwrap_or([0; 4]);
            let i = (y * out.stride + tx * TILE + x) as usize;
            if let Some(v) = out.y.get_mut(i) {
                *v = u32::from_le_bytes(p) as i32;
            }
            if let Some(v) = out.a.get_mut(i) {
                *v = i32::from(p[3]) * 4;
            }
        }
        match h.chroma {
            Chroma::Full444 => {
                for x in 0..TILE {
                    let (co, cg) = fwd(px.get((y * TILE + x) as usize).copied().unwrap_or([0; 4]));
                    let i = (y * out.chroma_stride + tx * TILE + x) as usize;
                    if let Some(v) = out.co.get_mut(i) {
                        *v = co;
                    }
                    if let Some(v) = out.cg.get_mut(i) {
                        *v = cg;
                    }
                }
            }
            Chroma::Halo422 => {
                for x in 0..TILE / 2 {
                    let (c0, g0) = fwd(px.get((y * TILE + 2 * x) as usize).copied().unwrap_or([0; 4]));
                    let (c1, g1) = fwd(px.get((y * TILE + 2 * x + 1) as usize).copied().unwrap_or([0; 4]));
                    let i = (y * out.chroma_stride + tx * (TILE / 2) + x) as usize;
                    if let Some(v) = out.co.get_mut(i) {
                        *v = (c0 + c1 + 1) >> 1;
                    }
                    if let Some(v) = out.cg.get_mut(i) {
                        *v = (g0 + g1 + 1) >> 1;
                    }
                }
            }
        }
    }
}

/// Pass 1: decode every tile into sample planes. `parallel` decodes tile rows on worker threads.
pub fn decode_planes(f: &Frame<'_>, parallel: bool) -> Planes {
    let h = &f.header;
    let (cw, ch) = h.coded_size();
    let chroma_width = h.chroma.plane_width(cw);
    let n = (cw * ch) as usize;
    let nc = (chroma_width * ch) as usize;
    let mut p = Planes {
        coded_width: cw,
        coded_height: ch,
        chroma_width,
        y: vec![0; n],
        co: vec![0; nc],
        cg: vec![0; nc],
        a: if h.alpha { vec![0; n] } else { Vec::new() },
        modes: (0..h.tile_count()).map(|t| f.mode(t)).collect(),
        tiles_x: h.tiles_x(),
    };
    let rows = h.tiles_y() as usize;
    let (row_n, row_c) = ((cw * TILE) as usize, (chroma_width * TILE) as usize);
    let mut a_rows: Vec<&mut [i32]> = if h.alpha { p.a.chunks_mut(row_n).collect() } else { (0..rows).map(|_| <&mut [i32]>::default()).collect() };
    let mut work: Vec<(u32, RowOut<'_>)> =
        p.y.chunks_mut(row_n)
            .zip(p.co.chunks_mut(row_c))
            .zip(p.cg.chunks_mut(row_c))
            .zip(a_rows.drain(..))
            .enumerate()
            .map(|(i, (((y, co), cg), a))| (i as u32, RowOut { stride: cw, chroma_stride: chroma_width, y, co, cg, a }))
            .collect();
    let run_row = |(ty, out): &mut (u32, RowOut<'_>)| {
        for tx in 0..h.tiles_x() {
            decode_tile(f, *ty * h.tiles_x() + tx, out);
        }
    };
    if parallel && rows > 1 {
        let threads = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).clamp(1, 16);
        let per = rows.div_ceil(threads).max(1);
        std::thread::scope(|s| {
            for group in work.chunks_mut(per) {
                s.spawn(move || group.iter_mut().for_each(run_row));
            }
        });
    } else {
        work.iter_mut().for_each(run_row);
    }
    drop(work);
    p
}

/// Pass 2, one pixel: RGBA in quarter units. Chroma 4:2:2: even columns take chroma column
/// `x / 2`, odd columns the rounded mean of `x / 2` and the next column (clamped to the plane).
/// Inverse YCoCg-R: `t = Y - (Cg >> 1)`, `G = Cg + t`, `B = t - (Co >> 1)`, `R = B + Co`;
/// every channel clamped to `0..=1020`.
pub fn pixel(p: &Planes, alpha: bool, chroma: Chroma, x: u32, y: u32) -> [i32; 4] {
    let at = |v: &[i32], stride: u32, cx: u32| v.get((y * stride + cx) as usize).copied().unwrap_or(0);
    let yv = at(&p.y, p.coded_width, x);
    let mode = p.modes.get(((y / TILE) * p.tiles_x + x / TILE) as usize).copied().unwrap_or(TileMode::Dct);
    if mode != TileMode::Dct {
        return (yv as u32).to_le_bytes().map(|v| i32::from(v) * 4);
    }
    let (co, cg) = match chroma {
        Chroma::Full444 => (at(&p.co, p.chroma_width, x), at(&p.cg, p.chroma_width, x)),
        Chroma::Halo422 => {
            let c = x >> 1;
            if x & 1 == 0 {
                (at(&p.co, p.chroma_width, c), at(&p.cg, p.chroma_width, c))
            } else {
                let n = (c + 1).min(p.chroma_width.saturating_sub(1));
                ((at(&p.co, p.chroma_width, c) + at(&p.co, p.chroma_width, n) + 1) >> 1, (at(&p.cg, p.chroma_width, c) + at(&p.cg, p.chroma_width, n) + 1) >> 1)
            }
        }
    };
    let t = yv - (cg >> 1);
    let g = cg + t;
    let b = t - (co >> 1);
    let r = b + co;
    let a = if alpha { at(&p.a, p.coded_width, x) } else { OUT_MAX };
    [r, g, b, a].map(|v| v.clamp(0, OUT_MAX))
}

/// Decode a frame bitstream to RGBA in quarter units (`width * height * 4` values, rows top
/// first): the normative output.
pub fn decode_rgba_q(b: &[u8], width: u32, height: u32, parallel: bool) -> Result<Vec<u16>, MediaError> {
    let f = Frame::parse(b, width, height)?;
    let p = decode_planes(&f, parallel);
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            out.extend(pixel(&p, f.header.alpha, f.header.chroma, x, y).map(|v| v as u16));
        }
    }
    Ok(out)
}

/// 8-bit from a quarter-unit sample: `(v + 2) >> 2`.
pub fn to_8bit(v: u16) -> u8 {
    ((u32::from(v) + 2) >> 2) as u8
}

/// 10-bit from a quarter-unit sample: `(v * 1023 + 510) / 1020`.
pub fn to_10bit(v: u16) -> u16 {
    ((u32::from(v) * 1023 + 510) / 1020) as u16
}

/// 8-bit RGBA of a frame bitstream.
pub fn decode_rgba8(b: &[u8], width: u32, height: u32, parallel: bool) -> Result<Vec<u8>, MediaError> {
    Ok(decode_rgba_q(b, width, height, parallel)?.into_iter().map(to_8bit).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idct_of_a_dc_block_is_flat_and_exactly_defined() {
        // DC 8 * 100 (orthonormal: a flat block of 100) -> 400 quarter units everywhere.
        let mut d = [0i32; 64];
        d[0] = 800;
        let mut s = [0i32; 64];
        idct(&d, 0, &mut s);
        assert!(s.iter().all(|&v| v == 400), "{s:?}");
        // Worst-case magnitudes stay inside i32 (the documented bound).
        let d = [DEQUANT_MAX; 64];
        idct(&d, 0, &mut s);
        let d = core::array::from_fn(|i| if i % 2 == 0 { DEQUANT_MAX } else { -DEQUANT_MAX });
        idct(&d, 512, &mut s);
    }

    #[test]
    fn freq_tables_validate() {
        let mut f = vec![0u16; 16];
        f[3] = 16384;
        let t = FreqTable::from_freq(f.clone()).unwrap();
        assert!(t.slot2sym.iter().all(|&s| s == 3));
        let b = FreqTable::sparse_bytes(&f);
        assert_eq!(FreqTable::parse(&b, 0, 16).unwrap().0, t);
        // Negative controls: a sum other than 16384, a symbol past the alphabet, zero entries.
        f[3] = 16383;
        assert!(FreqTable::from_freq(f).is_err());
        assert!(FreqTable::parse(&[1, 0, 16, 0, 0, 64], 0, 16).is_err());
        assert!(FreqTable::parse(&[0, 0], 0, 16).is_err());
    }

    #[test]
    fn output_conversions() {
        assert_eq!([0u16, 1, 2, 1018, 1020].map(to_8bit), [0, 0, 1, 255, 255]);
        assert_eq!([0u16, 1020, 510].map(to_10bit), [0, 1023, 512]);
    }
}
