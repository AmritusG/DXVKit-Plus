//! DXV3 encoder (Resolume's DXV): RGBA8 → BC block planes → one DXV3 packet per frame.
//!
//! A safe-Rust port of the DXVKit encoder — `src/enc.c`, `enc_bc.c`, `enc_pack.c`, `enc_hq.c` and
//! `tools/gen_bc_tables.c` of github.com/AmritusG/dxvkit 1.0.0 (2f5256a; `src/` unchanged at fb506da; MIT,
//! Copyright (c) 2026 Amrit Rosell — `LICENSES/DXVKit-MIT.txt`), by way of Loombda-rs's port
//! (`crates/loombda-encode/src/dxv/` @ b7e6f68, Amrit's own, moved here under MIT on 2026-11-14).
//! DXVKit was written from its own spec (`docs/spec/DXV_FORMAT.md`) and the public S3TC / RGTC / FSE
//! descriptions; no other DXV implementation was read for either.
//!
//! | Variant | Planes | Packet tag (on disk) |
//! |---|---|---|
//! | DXT1 (Normal Quality) | BC1 | `1TXD` |
//! | DXT5 (Normal Quality with alpha) | BC3 | `5TXD` |
//! | YCG6 (High Quality) | BC4 Y + BC5 CoCg (half size) | `6GCY` |
//! | YG10 (High Quality with alpha) | BC5 Y A + BC5 CoCg | `01GY` |
//!
//! Coded size = the display size rounded up to 16 on both axes, padded by edge replication. The alpha
//! convention is never converted here: the caller hands straight RGBA (DXT1 / YCG6 ignore alpha).
//!
//! **Byte identity with Swift GlEnc.** Swift GlEnc encodes DXV through DXVKit's C core as SwiftPM builds it
//! (Apple clang, default `-ffp-contract=on`): `enc_bc.o` holds 24 fused multiply-adds (18 in the BC1
//! principal-axis search, 5 in `dxvk_bc4_encode_fine`; measured with `objdump`, glenc-rs `docs/handoffs/`
//! E1). [`bc`] reproduces exactly those fusions with `f32::mul_add` / `f64::mul_add`, so the same pixels give
//! the same blocks as GlEnc's DXVKit — not as DXVKit built with `-ffp-contract=off`.
//!
//! Lint note (this module): frame edges are checked against [`MAX_EDGE`] (16384) before any
//! arithmetic, so pixel / block coordinates and byte offsets (≤ 16384² × 4) fit `u32` / `usize`;
//! fixed 4×4 / 8×8 block arrays are indexed by loop counters below their length.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

mod bc;
mod hq;
mod pack;

pub use bc::{bc1_encode, bc4_encode, bc4_encode_fine};

use crate::EncodeError;
use pack::MatchTable;

/// Largest display edge (DXVKit `DXVK_MAX_DIMENSION`).
pub const MAX_EDGE: u32 = 16_384;
/// The packet header size (spec §3.1).
pub const HEADER: usize = 12;

/// The four DXV3 variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DxvVariant {
    Dxt1,
    Dxt5,
    Ycg6,
    Yg10,
}

impl DxvVariant {
    pub const ALL: [DxvVariant; 4] = [DxvVariant::Dxt1, DxvVariant::Dxt5, DxvVariant::Ycg6, DxvVariant::Yg10];
    /// Whether the variant stores alpha (DXT5, YG10).
    pub fn alpha(self) -> bool {
        matches!(self, DxvVariant::Dxt5 | DxvVariant::Yg10)
    }
    fn hq(self) -> bool {
        matches!(self, DxvVariant::Ycg6 | DxvVariant::Yg10)
    }
    /// The packet tag: the texture FourCC's bytes reversed (`1TXD`, `5TXD`, `6GCY`, `01GY`).
    pub fn tag(self) -> [u8; 4] {
        match self {
            DxvVariant::Dxt1 => *b"1TXD",
            DxvVariant::Dxt5 => *b"5TXD",
            DxvVariant::Ycg6 => *b"6GCY",
            DxvVariant::Yg10 => *b"01GY",
        }
    }
}

/// How hard the block search and the match / opcode search work (DXVKit `dxvk_effort`). GlEnc always uses
/// [`DxvEffort::Best`] (`DXVKitFrameEncoder.swift:59`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DxvEffort {
    Fast,
    Best,
}

fn check_frame(rgba: &[u8], width: u32, height: u32) -> Result<(), EncodeError> {
    let want = (width as usize).saturating_mul(height as usize).saturating_mul(4);
    if rgba.len() != want {
        return Err(EncodeError::Frame(format!("RGBA frame of {} bytes, {width}x{height} needs {want}", rgba.len())));
    }
    Ok(())
}

/// The block-plane geometry of a frame (DXVKit `dxvk_layout`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DxvLayout {
    pub variant: DxvVariant,
    pub display: (u32, u32),
    pub coded: (u32, u32),
    /// Blocks per row / block rows of plane 0 and plane 1 (plane 1 only for HQ).
    pub blocks: [(u32, u32); 2],
    /// Exact block-stream bytes per plane.
    pub bytes: [usize; 2],
}

impl DxvLayout {
    pub fn new(variant: DxvVariant, width: u32, height: u32) -> Result<DxvLayout, EncodeError> {
        if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
            return Err(EncodeError::Invalid(format!("DXV frame size {width}x{height} is outside 1..={MAX_EDGE}")));
        }
        let (cw, ch) = (width.div_ceil(16).saturating_mul(16), height.div_ceil(16).saturating_mul(16));
        let b0 = (cw / 4, ch / 4);
        let b1 = if variant.hq() { (cw / 8, ch / 8) } else { (0, 0) };
        let bb0 = if matches!(variant, DxvVariant::Dxt1 | DxvVariant::Ycg6) { 8 } else { 16 };
        let n = |b: (u32, u32)| (b.0 as usize).saturating_mul(b.1 as usize);
        Ok(DxvLayout { variant, display: (width, height), coded: (cw, ch), blocks: [b0, b1], bytes: [n(b0).saturating_mul(bb0), n(b1).saturating_mul(16)] })
    }
    pub fn count(&self, plane: usize) -> usize {
        self.blocks.get(plane).map(|b| (b.0 as usize).saturating_mul(b.1 as usize)).unwrap_or(0)
    }
}

/// One frame's block planes (plane 1 empty for DXT1 / DXT5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DxvPlanes {
    pub layout: DxvLayout,
    pub planes: [Vec<u8>; 2],
}

/// Pixel (x, y) of the padded texture: the display picture extended by edge replication.
fn pixel(rgba: &[u8], w: u32, h: u32, x: u32, y: u32) -> [u8; 4] {
    let (x, y) = (x.min(w.saturating_sub(1)) as usize, y.min(h.saturating_sub(1)) as usize);
    let i = y.saturating_mul(w as usize).saturating_add(x).saturating_mul(4);
    match rgba.get(i..i.saturating_add(4)) {
        Some(p) => [p[0], p[1], p[2], p[3]],
        None => [0; 4],
    }
}

/// Worker threads for one frame: `asked` (0 = the machine's parallelism), 1 ..= 64.
fn threads(asked: usize) -> usize {
    if asked > 0 { asked.min(64) } else { std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).clamp(1, 64) }
}

/// Run `f(first_unit, units)` over bands on `t` threads (one band runs on the calling thread when
/// `t` is 1); concatenate in order (the output does not depend on the thread count).
fn bands(units: usize, t: usize, f: impl Fn(usize, usize) -> Vec<u8> + Sync) -> Vec<u8> {
    if t <= 1 {
        return f(0, units);
    }
    let t = t.min(units.max(1));
    let per = units.div_ceil(t).max(1);
    let starts: Vec<usize> = (0..units).step_by(per).collect();
    std::thread::scope(|s| {
        let hs: Vec<_> = starts
            .iter()
            .map(|&u0| {
                let f = &f;
                s.spawn(move || f(u0, per.min(units.saturating_sub(u0))))
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// RGBA8 (display size, tightly packed, top row first) → block planes (`dxvk_encoder_compress`), on the
/// machine's parallelism.
pub fn compress(variant: DxvVariant, width: u32, height: u32, rgba: &[u8], effort: DxvEffort) -> Result<DxvPlanes, EncodeError> {
    compress_on(variant, width, height, rgba, effort, threads(0))
}

/// [`compress`] on `t` threads (0.15.0; the same planes for any count).
fn compress_on(variant: DxvVariant, width: u32, height: u32, rgba: &[u8], effort: DxvEffort, t: usize) -> Result<DxvPlanes, EncodeError> {
    let l = DxvLayout::new(variant, width, height)?;
    check_frame(rgba, width, height)?;
    let best = effort == DxvEffort::Best;
    if !variant.hq() {
        let bc3 = variant == DxvVariant::Dxt5;
        let (bx, by) = l.blocks[0];
        let p0 = bands(by as usize, t, |r0, rows| {
            let mut out = Vec::with_capacity(rows.saturating_mul(bx as usize).saturating_mul(16));
            for by in r0..r0.saturating_add(rows) {
                for bxi in 0..bx {
                    let mut px = [[0u8; 4]; 16];
                    for (i, p) in px.iter_mut().enumerate() {
                        let (x, y) = ((i % 4) as u32, (i / 4) as u32);
                        *p = pixel(rgba, width, height, bxi.saturating_mul(4).saturating_add(x), (by as u32).saturating_mul(4).saturating_add(y));
                    }
                    if bc3 {
                        let mut a = [0u8; 16];
                        for (d, p) in a.iter_mut().zip(px.iter()) {
                            *d = p[3];
                        }
                        out.extend_from_slice(&bc4_encode(&a, best));
                        out.extend_from_slice(&bc1_encode(&px, best, true));
                    } else {
                        out.extend_from_slice(&bc1_encode(&px, best, false));
                    }
                }
            }
            out
        });
        return Ok(DxvPlanes { layout: l, planes: [p0, Vec::new()] });
    }
    // HQ: per chroma block (8 × 8 pixels): 4 Y blocks (+ alpha) of plane 0 and one CoCg block.
    let ya = variant == DxvVariant::Yg10;
    let (cbx, cby) = l.blocks[1];
    let (b0x, _) = l.blocks[0];
    let ybytes = if ya { 16usize } else { 8 };
    // Each band returns its chroma rows: [plane-0 rows (2 block rows per chroma row) | plane-1 rows].
    let rows: Vec<(Vec<u8>, Vec<u8>)> = {
        let t = t.min((cby as usize).max(1));
        let per = (cby as usize).div_ceil(t).max(1);
        let starts: Vec<usize> = (0..cby as usize).step_by(per).collect();
        std::thread::scope(|s| {
            let hs: Vec<_> = starts
                .iter()
                .map(|&r0| {
                    s.spawn(move || {
                        let n = per.min((cby as usize).saturating_sub(r0));
                        let mut p0 = vec![0u8; n.saturating_mul(2).saturating_mul(b0x as usize).saturating_mul(ybytes)];
                        let mut p1 = Vec::with_capacity(n.saturating_mul(cbx as usize).saturating_mul(16));
                        for cy in r0..r0.saturating_add(n) {
                            for cx in 0..cbx {
                                let (mut yv, mut av) = ([[0u16; 8]; 8], [[0u8; 8]; 8]);
                                let (mut co, mut cg) = ([0u16; 16], [0u16; 16]);
                                for sy in 0..4u32 {
                                    for sx in 0..4u32 {
                                        let (mut sum_co, mut sum_cg) = (0i32, 0i32);
                                        for k in 0..4u32 {
                                            let (x, y) = (sx * 2 + (k & 1), sy * 2 + (k >> 1));
                                            let p = pixel(
                                                rgba,
                                                width,
                                                height,
                                                cx.saturating_mul(8).saturating_add(x),
                                                (cy as u32).saturating_mul(8).saturating_add(y),
                                            );
                                            let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                                            if let (Some(row), Some(arow)) = (yv.get_mut(y as usize), av.get_mut(y as usize)) {
                                                if let Some(v) = row.get_mut(x as usize) {
                                                    *v = (4 * (r + 2 * g + b)) as u16;
                                                }
                                                if let Some(v) = arow.get_mut(x as usize) {
                                                    *v = p[3];
                                                }
                                            }
                                            sum_co += r - b;
                                            sum_cg += 2 * g - r - b;
                                        }
                                        let i = (sy * 4 + sx) as usize;
                                        if let (Some(a), Some(b)) = (co.get_mut(i), cg.get_mut(i)) {
                                            *a = (2 * sum_co + 2040) as u16;
                                            *b = (sum_cg + 2040) as u16;
                                        }
                                    }
                                }
                                for sub in 0..4u32 {
                                    let (ox, oy) = ((sub & 1) * 4, (sub >> 1) * 4);
                                    let bx = (cx * 2 + (sub & 1)) as usize;
                                    let by_local = (cy - r0) * 2 + (sub >> 1) as usize;
                                    let mut blk16 = [0u16; 16];
                                    let mut blk = [0u8; 16];
                                    for y in 0..4u32 {
                                        for x in 0..4u32 {
                                            let i = (y * 4 + x) as usize;
                                            let (yy, xx) = ((oy + y) as usize, (ox + x) as usize);
                                            blk16[i] = yv.get(yy).and_then(|r| r.get(xx)).copied().unwrap_or(0);
                                            blk[i] = av.get(yy).and_then(|r| r.get(xx)).copied().unwrap_or(0);
                                        }
                                    }
                                    let at = by_local.saturating_mul(b0x as usize).saturating_add(bx).saturating_mul(ybytes);
                                    if let Some(d) = p0.get_mut(at..at.saturating_add(8)) {
                                        d.copy_from_slice(&bc4_encode_fine(&blk16, best));
                                    }
                                    if ya && let Some(d) = p0.get_mut(at.saturating_add(8)..at.saturating_add(16)) {
                                        d.copy_from_slice(&bc4_encode(&blk, best));
                                    }
                                }
                                p1.extend_from_slice(&bc4_encode_fine(&co, best));
                                p1.extend_from_slice(&bc4_encode_fine(&cg, best));
                            }
                        }
                        (p0, p1)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        })
    };
    let (mut p0, mut p1) = (Vec::with_capacity(l.bytes[0]), Vec::with_capacity(l.bytes[1]));
    for (a, b) in rows {
        p0.extend_from_slice(&a);
        p1.extend_from_slice(&b);
    }
    if p0.len() != l.bytes[0] || p1.len() != l.bytes[1] {
        return Err(EncodeError::Invalid(format!("DXV planes {}+{} bytes, layout wants {}+{}", p0.len(), p1.len(), l.bytes[0], l.bytes[1])));
    }
    Ok(DxvPlanes { layout: l, planes: [p0, p1] })
}

/// The packer's reusable state (match tables, HQ sections), one per encoder.
pub struct DxvPacker {
    layout: DxvLayout,
    best: bool,
    tables: [MatchTable; 5],
    sections: Option<[hq::Section; 2]>,
}

impl DxvPacker {
    pub fn new(layout: DxvLayout, effort: DxvEffort) -> DxvPacker {
        let best = effort == DxvEffort::Best;
        let bits = pack::table_bits(layout.count(0), best);
        let tables = [MatchTable::new(bits), MatchTable::new(bits), MatchTable::new(bits), MatchTable::new(bits), MatchTable::new(1)];
        let sections = layout.variant.hq().then(|| {
            let alpha = layout.variant == DxvVariant::Yg10;
            [hq::Section::new(if alpha { 2 } else { 1 }, layout.count(0), best, false), hq::Section::new(2, layout.count(1), best, false)]
        });
        DxvPacker { layout, best, tables, sections }
    }

    /// Block planes → one DXV3 packet (`dxvk_encoder_pack`, never a stored payload). Lossless:
    /// decoding the packet returns exactly these planes.
    pub fn pack(&mut self, planes: &DxvPlanes) -> Result<Vec<u8>, EncodeError> {
        if planes.layout != self.layout {
            return Err(EncodeError::Frame("DXV planes of another layout".into()));
        }
        let [p0, p1] = &planes.planes;
        if p0.len() != self.layout.bytes[0] || p1.len() != self.layout.bytes[1] {
            return Err(EncodeError::Frame("DXV plane sizes do not match the layout".into()));
        }
        let payload = match self.layout.variant {
            DxvVariant::Dxt1 => pack::pack_dxt1(p0, self.layout.count(0), &mut self.tables),
            DxvVariant::Dxt5 => pack::pack_dxt5(p0, self.layout.count(0), self.best, &mut self.tables),
            _ => {
                let [s0, s1] = self.sections.as_mut().ok_or_else(|| EncodeError::Invalid("HQ sections missing".into()))?;
                let mut a = s0.encode(p0);
                a.extend_from_slice(&s1.encode(p1));
                a
            }
        };
        let len = u32::try_from(payload.len()).map_err(|_| EncodeError::Invalid("DXV payload over 4 GiB".into()))?;
        let mut out = Vec::with_capacity(HEADER.saturating_add(payload.len()));
        out.extend_from_slice(&self.layout.variant.tag());
        out.extend_from_slice(&[4, 0, 0, 0]);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }
}

/// RGBA8 → one DXV3 packet (compress + pack).
pub fn encode_frame(variant: DxvVariant, width: u32, height: u32, rgba: &[u8], effort: DxvEffort) -> Result<(Vec<u8>, DxvPlanes), EncodeError> {
    let planes = compress(variant, width, height, rgba, effort)?;
    let mut p = DxvPacker::new(planes.layout, effort);
    Ok((p.pack(&planes)?, planes))
}

/// One DXV3 encoder per output file (Swift GlEnc: one `DXVEncoder` per file, `DXVKitFrameEncoder.swift:69-79`).
pub struct DxvEncoder {
    variant: DxvVariant,
    effort: DxvEffort,
    width: u32,
    height: u32,
    packer: DxvPacker,
    threads: usize,
}

impl DxvEncoder {
    /// An encoder for frames of `width` × `height` (1 ..= 16384 each).
    pub fn new(variant: DxvVariant, width: u32, height: u32, effort: DxvEffort) -> Result<DxvEncoder, EncodeError> {
        let layout = DxvLayout::new(variant, width, height)?;
        Ok(DxvEncoder { variant, effort, width, height, packer: DxvPacker::new(layout, effort), threads: 0 })
    }
    /// Worker threads for one frame (0 = the machine's parallelism, the default; 0.15.0). Output is the same
    /// for any count.
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }
    pub fn variant(&self) -> DxvVariant {
        self.variant
    }
    /// Straight RGBA8 at the presentation size → one DXV3 packet.
    pub fn encode(&mut self, rgba: &[u8]) -> Result<Vec<u8>, EncodeError> {
        let planes = compress_on(self.variant, self.width, self.height, rgba, self.effort, threads(self.threads))?;
        self.packer.pack(&planes)
    }
}
