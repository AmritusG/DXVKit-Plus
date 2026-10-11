//! LMBX (LMBD profile `0x10`, texture format `0x11`): the variable-rate, per-tile-independent
//! profile — YCoCg-R, an 8x8 DCT with an exactly specified integer inverse, per-block adaptive
//! quantisation and rANS with one independent stream per plane of every 32x32 tile, so a GPU
//! decodes thousands of streams at once (the compute passes in `dxvkit-plus-wgpu`). This module is the normative reference decoder; the format is frozen in
//! `docs/LMBD.md` ("LMBX").
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
// +-4095; the IDCT sums are proven inside i32 in docs/LMBD.md) and indices by the tile geometry;
// the arithmetic lint is relaxed for this module only. Every byte read is bounds-checked.
#![allow(clippy::arithmetic_side_effects)]

use crate::bytes::{arr, byte, le16, le32};
use crate::error::MediaError;

/// Frame bitstream magic.
pub const MAGIC: [u8; 4] = *b"LMBX";
/// Frame bitstream version.
pub const VERSION: u16 = 1;
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
    /// The symbol owning each of the `PROB_SCALE` slots.
    pub slot2sym: Vec<u16>,
}

impl FreqTable {
    /// Build from dense frequencies that add up to exactly [`PROB_SCALE`].
    pub fn from_freq(freq: Vec<u16>) -> Result<FreqTable, MediaError> {
        let mut cum = Vec::with_capacity(freq.len() + 1);
        let mut acc = 0u32;
        cum.push(0);
        for &f in &freq {
            acc += u32::from(f);
            cum.push(acc);
        }
        if acc != PROB_SCALE {
            return Err(lmbx(format!("frequency table adds up to {acc}, not {PROB_SCALE}")));
        }
        let mut slot2sym = vec![0u16; PROB_SCALE as usize];
        for (s, w) in cum.windows(2).enumerate() {
            if let [a, b] = w
                && let Some(dst) = slot2sym.get_mut(*a as usize..*b as usize)
            {
                dst.fill(s as u16);
            }
        }
        Ok(FreqTable { freq, cum, slot2sym })
    }

    /// Parse the sparse form at `at`: `u16 n`, then `n` x (`u16 symbol`, `u16 freq`), symbols
    /// strictly increasing below `symbols`, every freq >= 1, sum [`PROB_SCALE`]. Returns the table
    /// and the offset after it.
    pub fn parse(b: &[u8], at: usize, symbols: usize) -> Result<(FreqTable, usize), MediaError> {
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
        Ok((FreqTable::from_freq(freq)?, at + 2 + n * 4))
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
        put(4, &VERSION.to_le_bytes());
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
        if version != u32::from(VERSION) {
            return Err(MediaError::Unsupported(format!("LMBX frame version {version} (this decoder knows version {VERSION})")));
        }
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
        let header = FrameHeader { chroma, alpha: flags & 1 == 1, width: w, height: h, chroma_ratio, qt };
        let coef_table = HEADER_BYTES;
        let (coef, qmap) = FreqTable::parse(b, coef_table, COEF_SYMBOLS)?;
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
            let o = le32(b, directory + 4 + i * 4).ok_or_else(short)?;
            if (i == 0 && o != 0) || o < prev || o as usize > data_len {
                return Err(lmbx(format!("stream {i} offset {o} (previous {prev}, {data_len} data bytes)")));
            }
            prev = o;
            offsets.push(o);
        }
        Ok(Frame { header, coef, offsets, sections: Sections { coef_table, qmap, directory, data }, bytes: b })
    }

    /// The quantiser index of luma block `b` (raster, `0..16`) of tile `t`.
    pub fn qidx(&self, t: u32, b: u32) -> usize {
        let at = self.sections.qmap + t as usize * QMAP_BYTES + (b / 2) as usize;
        let v = self.bytes.get(at).copied().unwrap_or(0);
        usize::from(if b % 2 == 0 { v & 15 } else { v >> 4 })
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
        let slot = self.x & (PROB_SCALE - 1);
        let s = t.slot2sym.get(slot as usize).copied().unwrap_or(0);
        let f = u32::from(t.freq.get(s as usize).copied().unwrap_or(1));
        let c = t.cum.get(s as usize).copied().unwrap_or(0);
        self.x = f.wrapping_mul(self.x >> PROB_BITS).wrapping_add(slot).wrapping_sub(c);
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
    let mut s = [0i32; 64];
    for kind in PLANES.into_iter().take(f.header.planes() as usize) {
        let (plane, stride, offset) = match kind {
            PlaneKind::Y => (&mut *out.y, out.stride, 512),
            PlaneKind::Co => (&mut *out.co, out.chroma_stride, 0),
            PlaneKind::Cg => (&mut *out.cg, out.chroma_stride, 0),
            PlaneKind::A => (&mut *out.a, out.stride, 512),
        };
        visit_stream(f, t, kind, |x0, y0, d| {
            idct(d, offset, &mut s);
            put_block(plane, stride, x0, y0, &s);
        });
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
