//! DXV decoding — a safe-Rust port of DXVKit's decoder.
//!
//! Carried over (cp -p, unchanged decoding steps) from Loombda-rs `crates/loombda-media/src/dxv/`
//! (AmritusG/Loombda-rs @ 67951932826b7847589bde6de988fdbc94a67661, Amrit Rosell's own code), which
//! is the port; ported there from DXVKit 1.0.0 (AmritusG/DXVKit @ 36209e0aa85b5049079934ec167b7d0c7c9c57df,
//! `src/packet.c`, `src/blockcopy.c`, `src/legacy.c`, `src/hq.c`, `src/pixels.c`), MIT, Copyright (c)
//! 2026 Amrit Rosell — licence text in `LICENSES/DXVKit-MIT.txt`. DXVKit is itself a clean-room
//! implementation of its own `docs/spec/DXV_FORMAT.md`; section numbers below refer to that spec.
//!
//! Output is the normative DXV output: block streams (BC1 / BC3 / BC4 / BC5 layout) covering the
//! whole coded texture (display size rounded up to 16 on both axes), rows top first. They upload
//! straight to BC textures; the YCoCg variants (YCG6, YG10) are converted to RGB on the GPU
//! (`dxvkit-plus-wgpu`, integer arithmetic, bit-exact with the CPU) with the spec 8.4 convention,
//! and on the CPU by [`rgba::blocks_to_rgba`] (the oracle). Differential check: every DXVKit test
//! vector decodes to byte-identical block streams (`tests/dxvkit_vectors.rs`).
//!
//! Differences from the C core, all at the API surface (the decoding steps are line-for-line):
//! no `dxvk_jobs` runner (the two HQ sections are decoded by `std::thread::scope` when asked), no
//! consumption histogram (only the bytes used), and every read is bounds-checked (a malformed
//! packet is an error value, never a panic or out-of-bounds read).

mod blockcopy;
mod hq;
mod lzf;
pub mod rgba;

use crate::bytes::{byte, le32};
use std::fmt;

/// Largest display width / height accepted (pixels).
pub const MAX_DIMENSION: u32 = 16_384;

/// Why a DXV packet was rejected (DXVKit's `dxvk_status`, minus the I/O codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DxvError {
    Argument,
    BufferTooSmall,
    /// Packet shorter than its header, or header length > packet.
    Truncated,
    /// The payload ends before the texture is complete.
    PayloadEnd,
    /// A copy reaches before the first block.
    BadDistance,
    Lzf,
    LzfLength,
    HqSection,
    HqOpcodeCount,
    FseTable,
    FseStream,
    HqOpcodesUsedUp,
    HqBadOpcode,
    HqEmptySlot,
    HqLiterals,
    /// DXV3 header: version byte below 4, or stored flag other than 0 / 1.
    BadHeader,
}

impl fmt::Display for DxvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DxvError::Argument => "invalid argument",
            DxvError::BufferTooSmall => "output buffer too small",
            DxvError::Truncated => "packet shorter than its header or its declared payload",
            DxvError::PayloadEnd => "payload ends before the texture is complete",
            DxvError::BadDistance => "copy distance reaches before the first block",
            DxvError::Lzf => "malformed LZF stream",
            DxvError::LzfLength => "LZF output length differs from the texture size",
            DxvError::HqSection => "HQ section header out of range",
            DxvError::HqOpcodeCount => "HQ opcode count exceeds the block count",
            DxvError::FseTable => "malformed FSE frequency table",
            DxvError::FseStream => "malformed FSE bitstream",
            DxvError::HqOpcodesUsedUp => "HQ opcodes used up before the texture is complete",
            DxvError::HqBadOpcode => "HQ opcode value out of range",
            DxvError::HqEmptySlot => "HQ dictionary slot is empty",
            DxvError::HqLiterals => "HQ literal read past the literal area",
            DxvError::BadHeader => "DXV3 header: version byte below 4 or stored flag above 1",
        })
    }
}

impl std::error::Error for DxvError {}

/// The texture variant of a packet (the packet decides, never the container FourCC: spec 2.2).
/// `#[repr(u32)]` with fixed values (C ABI; pinned by `tests/abi.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Variant {
    /// DXV3 Normal Quality: BC1.
    Dxt1 = 0,
    /// DXV3 Normal Quality With Alpha: BC3.
    Dxt5 = 1,
    /// DXV3 High Quality: Y BC4 + CoCg BC5.
    Ycg6 = 2,
    /// DXV3 High Quality With Alpha: YA BC5 + CoCg BC5.
    Yg10 = 3,
    /// DXDI (DXV1/2): BC1.
    LegacyDxt1 = 4,
    /// DXDI: BC3.
    LegacyDxt5 = 5,
}

impl Variant {
    pub fn name(self) -> &'static str {
        match self {
            Variant::Dxt1 => "DXT1",
            Variant::Dxt5 => "DXT5",
            Variant::Ycg6 => "YCG6",
            Variant::Yg10 => "YG10",
            Variant::LegacyDxt1 => "legacy DXT1",
            Variant::LegacyDxt5 => "legacy DXT5",
        }
    }
    pub fn has_alpha(self) -> bool {
        matches!(self, Variant::Dxt5 | Variant::Yg10 | Variant::LegacyDxt5)
    }
    /// YCoCg variants (two planes, converted to RGB after BC decode).
    pub fn is_hq(self) -> bool {
        matches!(self, Variant::Ycg6 | Variant::Yg10)
    }
}

/// Block-stream layout per plane (the GPU upload format).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockFormat {
    None,
    /// 8-byte blocks, opaque.
    Bc1,
    /// 16-byte blocks.
    Bc3,
    /// 8-byte blocks, one unsigned channel.
    Bc4,
    /// 16-byte blocks: channel 0 BC4 block then channel 1 BC4 block.
    Bc5,
}

impl BlockFormat {
    pub fn block_bytes(self) -> usize {
        match self {
            BlockFormat::None => 0,
            BlockFormat::Bc1 | BlockFormat::Bc4 => 8,
            BlockFormat::Bc3 | BlockFormat::Bc5 => 16,
        }
    }
}

/// Geometry of a frame (spec 2.3, DXVKit `dxvk_frame_layout`). The coded texture is the display size
/// rounded up to 16 on both axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub variant: Variant,
    pub display_width: u32,
    pub display_height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// 1 or 2.
    pub plane_count: usize,
    pub format: [BlockFormat; 2],
    pub blocks_x: [u32; 2],
    pub blocks_y: [u32; 2],
    pub pixels_x: [u32; 2],
    pub pixels_y: [u32; 2],
    /// Exact block-stream size of each plane.
    pub bytes: [usize; 2],
}

impl Layout {
    /// The layout for a variant and display size (1..=[`MAX_DIMENSION`]).
    pub fn new(variant: Variant, width: u32, height: u32) -> Result<Layout, DxvError> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(DxvError::Argument);
        }
        // width <= 16384, so the rounding cannot overflow.
        let coded_width = width.div_ceil(16).saturating_mul(16);
        let coded_height = height.div_ceil(16).saturating_mul(16);
        let mut l = Layout {
            variant,
            display_width: width,
            display_height: height,
            coded_width,
            coded_height,
            plane_count: 1,
            format: [BlockFormat::None; 2],
            blocks_x: [coded_width / 4, 0],
            blocks_y: [coded_height / 4, 0],
            pixels_x: [coded_width, 0],
            pixels_y: [coded_height, 0],
            bytes: [0, 0],
        };
        l.format[0] = match variant {
            Variant::Dxt1 | Variant::LegacyDxt1 => BlockFormat::Bc1,
            Variant::Dxt5 | Variant::LegacyDxt5 => BlockFormat::Bc3,
            Variant::Ycg6 => BlockFormat::Bc4,
            Variant::Yg10 => BlockFormat::Bc5,
        };
        if variant.is_hq() {
            l.plane_count = 2;
            l.format[1] = BlockFormat::Bc5;
            l.pixels_x[1] = coded_width / 2;
            l.pixels_y[1] = coded_height / 2;
            l.blocks_x[1] = coded_width / 8;
            l.blocks_y[1] = coded_height / 8;
        }
        for i in 0..l.plane_count {
            let (bx, by, f) = (l.blocks_x.get(i).copied(), l.blocks_y.get(i).copied(), l.format.get(i).copied());
            let (Some(bx), Some(by), Some(f)) = (bx, by, f) else { return Err(DxvError::Argument) };
            let n = (bx as usize).checked_mul(by as usize).and_then(|n| n.checked_mul(f.block_bytes())).ok_or(DxvError::Argument)?;
            if let Some(b) = l.bytes.get_mut(i) {
                *b = n;
            }
        }
        Ok(l)
    }

    /// Blocks per plane.
    pub fn blocks(&self, plane: usize) -> usize {
        let bx = self.blocks_x.get(plane).copied().unwrap_or(0) as usize;
        let by = self.blocks_y.get(plane).copied().unwrap_or(0) as usize;
        bx.saturating_mul(by)
    }
}

/// DXV3 texture tags: the four header bytes read as a little-endian u32 (spec 3.1).
const TAG_DXT1: u32 = 0x4458_5431;
const TAG_DXT5: u32 = 0x4458_5435;
const TAG_YCG6: u32 = 0x5943_4736;
const TAG_YG10: u32 = 0x5947_3130;
const DXV3_HEADER: usize = 12;
const LEGACY_HEADER: usize = 4;

/// A parsed packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub variant: Variant,
    pub stored: bool,
    pub header_size: usize,
    pub payload_size: usize,
}

fn dxv3_variant(p: &[u8]) -> Option<Variant> {
    match le32(p, 0)? {
        TAG_DXT1 => Some(Variant::Dxt1),
        TAG_DXT5 => Some(Variant::Dxt5),
        TAG_YCG6 => Some(Variant::Ycg6),
        TAG_YG10 => Some(Variant::Yg10),
        _ => None,
    }
}

/// Parse a packet header (spec 3 / 4). The packet decides the generation (spec 2.2).
pub fn parse_header(p: &[u8]) -> Result<Header, DxvError> {
    let size = p.len();
    let h = if let Some(variant) = dxv3_variant(p) {
        if size < DXV3_HEADER {
            return Err(DxvError::Truncated);
        }
        let version = byte(p, 4).ok_or(DxvError::Truncated)?;
        let stored = byte(p, 6).ok_or(DxvError::Truncated)?;
        // version byte = version + 1: below 4 is invalid; stored flag only 0 or 1 (spec 3.2)
        if version < 4 || stored > 1 {
            return Err(DxvError::BadHeader);
        }
        let payload_size = le32(p, 8).ok_or(DxvError::Truncated)? as usize;
        Header { variant, stored: stored == 1, header_size: DXV3_HEADER, payload_size }
    } else {
        let word = le32(p, 0).ok_or(DxvError::Truncated)?;
        let ty = word >> 24;
        // bit 0x40 selects DXT5, everything else is DXT1; no type byte is rejected (spec 4.1)
        let variant = if ty & 0x40 != 0 { Variant::LegacyDxt5 } else { Variant::LegacyDxt1 };
        Header { variant, stored: ty & 0x80 != 0, header_size: LEGACY_HEADER, payload_size: (word & 0x00FF_FFFF) as usize }
    };
    if h.payload_size > size.saturating_sub(h.header_size) {
        return Err(DxvError::Truncated);
    }
    Ok(h)
}

/// Decoded block streams of one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocks {
    pub layout: Layout,
    /// Plane 0 (always) and plane 1 (HQ variants), each exactly `layout.bytes[i]` long.
    pub planes: [Vec<u8>; 2],
    /// Payload bytes the decoder needed (DXVKit consumption: `payload_size - payload_unused`).
    pub used: usize,
}

/// Decode one packet into block streams, allocating the planes. `width`/`height` = display size
/// from the container. `parallel_hq` decodes the two HQ sections on two threads.
pub fn decode_blocks(packet: &[u8], width: u32, height: u32, parallel_hq: bool) -> Result<Blocks, DxvError> {
    let header = parse_header(packet)?;
    let layout = Layout::new(header.variant, width, height)?;
    let mut planes = [vec![0u8; layout.bytes[0]], vec![0u8; layout.bytes[1]]];
    let [p0, p1] = &mut planes;
    let used = decode_into(packet, &header, &layout, p0, p1, parallel_hq)?;
    Ok(Blocks { layout, planes, used })
}

/// Decode one packet into caller planes (no allocation for DXT1/DXT5/legacy). Returns bytes used.
pub fn decode_into(packet: &[u8], header: &Header, layout: &Layout, plane0: &mut [u8], plane1: &mut [u8], parallel_hq: bool) -> Result<usize, DxvError> {
    if plane0.len() < layout.bytes[0] || (layout.plane_count == 2 && plane1.len() < layout.bytes[1]) {
        return Err(DxvError::BufferTooSmall);
    }
    let start = header.header_size;
    let end = start.checked_add(header.payload_size).ok_or(DxvError::Truncated)?;
    let payload = packet.get(start..end).ok_or(DxvError::Truncated)?;
    let plane0 = plane0.get_mut(..layout.bytes[0]).ok_or(DxvError::BufferTooSmall)?;
    if header.stored {
        // the plane streams back to back: HQ = plane 0 then plane 1 (spec 3.3, 4.2)
        let need = layout.bytes[0].checked_add(layout.bytes[1]).ok_or(DxvError::PayloadEnd)?;
        if payload.len() < need {
            return Err(DxvError::PayloadEnd);
        }
        let (a, b) = payload.split_at(layout.bytes[0]);
        plane0.copy_from_slice(a);
        if layout.plane_count == 2 {
            let b = b.get(..layout.bytes[1]).ok_or(DxvError::PayloadEnd)?;
            plane1.get_mut(..layout.bytes[1]).ok_or(DxvError::BufferTooSmall)?.copy_from_slice(b);
        }
        return Ok(need);
    }
    let blocks = layout.blocks(0);
    match header.variant {
        Variant::LegacyDxt1 | Variant::LegacyDxt5 => {
            lzf::decode(payload, plane0)?;
            Ok(payload.len())
        }
        Variant::Dxt1 => blockcopy::decode_dxt1(payload, plane0, blocks),
        Variant::Dxt5 => blockcopy::decode_dxt5(payload, plane0, blocks),
        Variant::Ycg6 | Variant::Yg10 => {
            let plane1 = plane1.get_mut(..layout.bytes[1]).ok_or(DxvError::BufferTooSmall)?;
            let unused = hq::decode(payload, layout, plane0, plane1, parallel_hq)?;
            Ok(payload.len().saturating_sub(unused))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_pads_both_axes_to_16() {
        let l = Layout::new(Variant::Ycg6, 18, 10).unwrap();
        assert_eq!((l.coded_width, l.coded_height), (32, 16));
        assert_eq!(l.bytes, [8 * 4 * 8, (4 * 2) * 16]);
        assert_eq!(l.plane_count, 2);
        // Negative controls: zero and oversize dimensions are rejected.
        assert_eq!(Layout::new(Variant::Dxt1, 0, 4), Err(DxvError::Argument));
        assert_eq!(Layout::new(Variant::Dxt1, MAX_DIMENSION + 1, 4), Err(DxvError::Argument));
    }

    #[test]
    fn headers_follow_the_packet_not_the_container() {
        let mut p = vec![0u8; 12];
        p[..4].copy_from_slice(&TAG_YCG6.to_le_bytes());
        p[4] = 4;
        let h = parse_header(&p).unwrap();
        assert_eq!((h.variant, h.stored, h.header_size, h.payload_size), (Variant::Ycg6, false, 12, 0));
        p[4] = 3;
        assert_eq!(parse_header(&p), Err(DxvError::BadHeader));
        p[4] = 4;
        p[6] = 2;
        assert_eq!(parse_header(&p), Err(DxvError::BadHeader));
        // Legacy: type byte 0x40 bit = DXT5; length larger than the packet = truncated.
        let legacy = [0x01, 0x00, 0x00, 0x40, 0xAA];
        assert_eq!(parse_header(&legacy).unwrap().variant, Variant::LegacyDxt5);
        assert_eq!(parse_header(&[0x05, 0, 0, 0x23, 1]), Err(DxvError::Truncated));
        assert_eq!(parse_header(&[1, 2]), Err(DxvError::Truncated));
    }
}
