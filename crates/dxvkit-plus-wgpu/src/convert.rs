//! Upload + WGSL conversion of decoded frames into straight-alpha RGBA8 at the clip size.

use crate::staging::{STAGING_ALIGNMENT, StagedFrame, StagingLayout};
use crate::{GpuError, create_bc_texture, write_bc};
use dxvkit_plus_core::backend::{PixelLayout, YuvMatrix};
use dxvkit_plus_core::source::{Decoded, PixelFormat, Shading};

/// The converter's output format (straight alpha, not sRGB: values are stored as decoded).
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// How a [`FrameRenderer`] turns BC1 / BC3 / BC4 / BC7 block planes (DXV DXT1 / DXT5 / legacy DXT,
/// HAP Hap1 / Hap5 / HapY / HapM / HapA / R, LMB7 frames as planes) into pixels. Chosen per renderer ([`FrameRenderer::with_path`],
/// [`FrameRenderer::set_path`]); the default is [`BlockPath::Sampled`]. `#[repr(u32)]` with fixed
/// values (C ABI; pinned by `tests/abi.rs`; new variants only appended).
///
/// BC7 (0.9.0): sampled, the GPU's BC7 decode, exact by the format's definition for valid blocks
/// (and on Apple M5 Max for reserved-mode blocks too); exact, [`BC_EXACT_WGSL`] mode 4, so that
/// reserved-mode blocks — which GitHub's virtual Metal GPU decodes as opaque black — are
/// transparent black on every GPU. Frames that take the same route on both paths: DXV High Quality
/// (always decoded in the shader, [`DXV_HQ_WGSL`]), LMBX and RGBA8. HAP HDR (BC6H) has no CPU oracle
/// and is always sampled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum BlockPath {
    /// The blocks upload into BC textures and the GPU's fixed-function decoder decodes them when
    /// [`CONVERT_WGSL`] samples them: the fast many-layer path, and the only one with
    /// [`FrameRenderer::direct_view`] for BC1 / BC3 frames. No exactness contract: GPUs round BC
    /// palettes differently. Measured on Apple M5 Max against the CPU decode: encoded footage within
    /// 1 / 255 (Hap1, Hap5, HapA), 2 / 255 (DXT1, DXT5) and 3 / 255 (HAP Q); arbitrary blocks up to
    /// 3 (HAP Q 6); un-premultiplied colour scales that step by 255 / alpha.
    #[default]
    Sampled = 0,
    /// The same bytes upload as integer texels — one `Rg32Uint` (8-byte block) or `Rgba32Uint`
    /// (16-byte block) texel per block — and [`BC_EXACT_WGSL`] decodes them with the CPU decoders'
    /// integer arithmetic. Contract: the output equals `Decoded::to_rgba`, every channel of every
    /// texel. Every frame of these variants needs the conversion pass (no `direct_view`).
    Exact = 1,
}

/// Decoded planes -> straight RGBA8 at the clip size, one texel per pixel.
///
/// Modes: 0 RGBA as stored (flag 1 = force opaque); 2 HAP Q scaled YCoCg (van Waveren & Castaño,
/// "Real-Time YCoCg-DXT Compression", 2007, as the HAP specification references; flag 2 = BC4
/// alpha in `t1`); 3 HAP alpha-only (grey); 4 HDR BC6H clamped to 0..1; 5 4:2:0 YCbCr from an
/// `R8` luma and an `RG8` chroma plane (BT.601 / 709 / 2020 matrix, video or full range). Flag 4
/// (mode 0): the colour is stored premultiplied; it is un-premultiplied in integers
/// (`min(255, round(c * 255 / a))`, a = 0 keeps c), as `dxvkit_plus_core::bc::unpremul`.
/// DXV High Quality has its own pipeline ([`DXV_HQ_WGSL`]); so do BC1 / BC3 / BC4 frames on the
/// exact path ([`BC_EXACT_WGSL`], [`BlockPath::Exact`]).
pub const CONVERT_WGSL: &str = r#"
struct U { mode: u32, flags: u32, matrix: u32, full_range: u32, size: vec2<u32>, pad: vec2<u32> };
@group(0) @binding(0) var t0: texture_2d<f32>;
@group(0) @binding(1) var t1: texture_2d<f32>;
@group(0) @binding(2) var<uniform> u: U;
@group(0) @binding(3) var lin: sampler;

@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

fn ycbcr(y8: f32, cb8: f32, cr8: f32) -> vec3<f32> {
    var y = y8;
    var cb = cb8 - 128.0 / 255.0;
    var cr = cr8 - 128.0 / 255.0;
    if (u.full_range == 0u) {
        y = (y8 * 255.0 - 16.0) / 219.0;
        cb = (cb8 * 255.0 - 128.0) / 224.0;
        cr = (cr8 * 255.0 - 128.0) / 224.0;
    }
    var k = vec4<f32>(1.5748, 0.187324, 0.468124, 1.8556); // BT.709
    if (u.matrix == 0u) { k = vec4<f32>(1.402, 0.344136, 0.714136, 1.772); }   // BT.601
    if (u.matrix == 2u) { k = vec4<f32>(1.4746, 0.164553, 0.571353, 1.8814); } // BT.2020
    return clamp(vec3<f32>(y + k.x * cr, y - k.y * cb - k.z * cr, y + k.w * cb), vec3<f32>(0.0), vec3<f32>(1.0));
}

// Premultiplied 0..255 -> straight, `min(255, round(c * 255 / a))`; a = 0 keeps c.
fn unpremul(c: vec4<f32>) -> vec4<f32> {
    let a = u32(round(c.a * 255.0));
    if (a == 0u) { return c; }
    let ci = vec3<u32>(round(c.rgb * 255.0));
    let s = min((ci * 255u + vec3<u32>(a / 2u)) / a, vec3<u32>(255u));
    return vec4<f32>(vec3<f32>(s) / 255.0, c.a);
}

@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let xy = vec2<i32>(pos.xy);
    var c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    switch u.mode {
        case 0u: { // RGBA
            c = textureLoad(t0, xy, 0);
            if ((u.flags & 1u) != 0u) { c.a = 1.0; }
        }
        case 2u: { // HAP Q: scaled YCoCg in BC3 (+ BC4 alpha)
            let s = textureLoad(t0, xy, 0);
            let scale = s.b * (255.0 / 8.0) + 1.0;
            let co = (s.r - 128.0 / 255.0) / scale;
            let cg = (s.g - 128.0 / 255.0) / scale;
            let y = s.a;
            c = vec4<f32>(clamp(vec3<f32>(y + co - cg, y + cg, y - co - cg), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
            if ((u.flags & 2u) != 0u) { c.a = textureLoad(t1, xy, 0).r; }
        }
        case 3u: { // alpha-only, shown as grey
            let a = textureLoad(t0, xy, 0).r;
            c = vec4<f32>(a, a, a, 1.0);
        }
        case 4u: { // HDR BC6H, clamped
            c = vec4<f32>(clamp(textureLoad(t0, xy, 0).rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
        }
        default: { // 4:2:0 YCbCr: R8 luma + RG8 chroma (bilinear, centred)
            let y = textureLoad(t0, xy, 0).r;
            let uv = (vec2<f32>(xy) + 0.5) / vec2<f32>(u.size);
            let ch = textureSampleLevel(t1, lin, uv, 0.0).rg;
            c = vec4<f32>(ycbcr(y, ch.r, ch.g), 1.0);
        }
    }
    if ((u.flags & 4u) != 0u) { c = unpremul(c); }
    return c;
}
"#;

/// DXV High Quality (YCG6 / YG10) -> straight RGBA8, bit-exact with the CPU oracle
/// (`dxvkit_plus_core::dxv::rgba`, DXVKit spec 8.4). The BC4 / BC5 blocks are uploaded as they are
/// but as integer texels — one `Rg32Uint` (BC4) or `Rgba32Uint` (BC5) texel per block — and decoded
/// here with the exact palette in 35ths of a code value, because a GPU's fixed-function BC4 decode
/// rounds the palette (on Apple silicon the folded result then differs by up to 5/255). `t0`: Y
/// (BC4) or Y + A (BC5) blocks; `t1`: Co + Cg (BC5) blocks at half size; nearest chroma, midpoint
/// 127.5, out-of-gamut folding, round half up. Flag 2: alpha (YG10); flag 4: colour stored
/// premultiplied (un-premultiplied in integers, as mode 0).
pub const DXV_HQ_WGSL: &str = r#"
struct U { mode: u32, flags: u32, matrix: u32, full_range: u32, size: vec2<u32>, pad: vec2<u32> };
@group(0) @binding(0) var t0: texture_2d<u32>;
@group(0) @binding(1) var t1: texture_2d<u32>;
@group(0) @binding(2) var<uniform> u: U;

@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

// Texel i (0..15) of the BC4 block in words (lo, hi), exact palette value in 35ths (0..8925).
fn bc4(lo: u32, hi: u32, i: u32) -> i32 {
    let a0 = lo & 255u;
    let a1 = (lo >> 8u) & 255u;
    let bit = 16u + 3u * i;
    var k = 0u;
    if (bit + 3u <= 32u) {
        k = (lo >> bit) & 7u;
    } else if (bit >= 32u) {
        k = (hi >> (bit - 32u)) & 7u;
    } else {
        k = ((lo >> bit) | (hi << (32u - bit))) & 7u;
    }
    if (k == 0u) { return i32(a0 * 35u); }
    if (k == 1u) { return i32(a1 * 35u); }
    let j = k - 1u;
    if (a0 > a1) { return i32(((7u - j) * a0 + j * a1) * 5u); }
    if (k == 6u) { return 0; }
    if (k == 7u) { return 8925; }
    return i32(((5u - j) * a0 + j * a1) * 7u);
}

// 70ths of a code value -> 0..255, round half up, clamped (DXVKit `hq_round`).
fn hq8(v: i32) -> i32 {
    if (v <= 0) { return 0; }
    if (v >= 17850) { return 255; }
    return (v + 35) / 70;
}

@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let xy = vec2<u32>(pos.xy);
    let i = (xy.y & 3u) * 4u + (xy.x & 3u);
    let yb = textureLoad(t0, vec2<i32>(xy / 4u), 0);
    let c2 = xy / 2u;
    let cb = textureLoad(t1, vec2<i32>(c2 / 4u), 0);
    let ci = (c2.y & 3u) * 4u + (c2.x & 3u);
    let y = 2 * bc4(yb.x, yb.y, i);
    let co = 2 * bc4(cb.x, cb.y, ci) - 8925;
    let cg = 2 * bc4(cb.z, cb.w, ci) - 8925;
    let t = y - cg;
    var r = t + co;
    let g = y + cg;
    var b = t - co;
    if (r < 0) { b += r; r = 0; }
    if (b < 0) { r += b; b = 0; }
    if (r > 17850) { b += r - 17850; r = 17850; }
    if (b > 17850) { r += b - 17850; b = 17850; }
    var rgb = vec3<u32>(vec3<i32>(hq8(r), hq8(g), hq8(b)));
    var a = 255u;
    if ((u.flags & 2u) != 0u) { a = u32(hq8(2 * bc4(yb.z, yb.w, i))); }
    if ((u.flags & 4u) != 0u && a != 0u) { rgb = min((rgb * 255u + vec3<u32>(a / 2u)) / a, vec3<u32>(255u)); }
    return vec4<f32>(vec3<f32>(rgb), f32(a)) / 255.0;
}
"#;

/// The exact path ([`BlockPath::Exact`]): BC1 / BC3 / BC4 blocks -> straight RGBA8, bit-exact with
/// the CPU decoders (`dxvkit_plus_core::bc`, `dxvkit_plus_core::hap::ycocg_scaled_exact`). The
/// blocks are integer texels (`t0`, and `t1` for HAP Q Alpha's alpha plane): `.xy` are the two
/// little-endian words of an 8-byte block, `.xyzw` the four of a 16-byte block (BC3: alpha block in
/// `.xy`, colour block in `.zw`). Everything is `u32` / `i32` arithmetic on values below 2^24;
/// every division has non-negative operands and a non-zero divisor, every shift count is below 32.
///
/// Modes (`U.mode`):
/// - 0, BC1 (HAP Hap1, DXV DXT1, legacy DXT1): DXVKit's convention (spec 8.1) — 5:6:5 endpoints
///   widened by bit replication, thirds and halves truncated, the three-colour palette's fourth
///   entry black; alpha 255.
/// - 1, BC3 (HAP Hap5, DXV DXT5, legacy DXT5): colour always four-colour, thirds truncated; alpha
///   the BC4 palette in sevenths / fifths, truncated (spec 8.2). Flag 1: alpha forced to 255. Flag
///   4: colour stored premultiplied, un-premultiplied in integers (`bc::unpremul`).
/// - 2, HAP Q (HapY; HapM with flag 2): the BC3 palettes exact (colour in thirds, Y in 35ths), the
///   HAP specification's scaled-YCoCg transform over the common denominator, rounded half up
///   (`ycocg_scaled_exact`); HapM's alpha plane as mode 1's alpha.
/// - 3, alpha only (HAP HapA): the BC4 palette truncated, shown as grey with alpha 255.
/// - 4, BC7 (HAP R, LMB7 frames uploaded as planes; 0.9.0): `dxvkit_plus_core::bc7::bc7_block`
///   line for line — every mode, the partition and anchor tables, p-bits, bit-replicated
///   endpoints, the 6-bit weights `((64 - w) * e0 + w * e1 + 32) >> 6`, index selection and channel
///   rotation; a reserved mode (first byte 0) is transparent black. Flag 1: alpha forced to 255.
pub const BC_EXACT_WGSL: &str = r#"
struct U { mode: u32, flags: u32, matrix: u32, full_range: u32, size: vec2<u32>, pad: vec2<u32> };
@group(0) @binding(0) var t0: texture_2d<u32>;
@group(0) @binding(1) var t1: texture_2d<u32>;
@group(0) @binding(2) var<uniform> u: U;

@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

// A 5:6:5 endpoint widened to 8 bits per channel by bit replication.
fn expand565(c: u32) -> vec3<u32> {
    let r = c >> 11u;
    let g = (c >> 5u) & 63u;
    let b = c & 31u;
    return vec3<u32>((r << 3u) | (r >> 2u), (g << 2u) | (g >> 4u), (b << 3u) | (b >> 2u));
}

// Texel i (0..15) of the BC1 colour block in words (lo = the endpoints, hi = the indices),
// truncated to 8 bits. `four`: always the four-colour palette (the colour half of BC3).
fn bc1(lo: u32, hi: u32, i: u32, four: bool) -> vec3<u32> {
    let c0 = lo & 0xFFFFu;
    let c1 = lo >> 16u;
    let p0 = expand565(c0);
    let p1 = expand565(c1);
    let k = (hi >> (2u * i)) & 3u;
    if (k == 0u) { return p0; }
    if (k == 1u) { return p1; }
    if (four || c0 > c1) {
        if (k == 2u) { return (2u * p0 + p1) / 3u; }
        return (p0 + 2u * p1) / 3u;
    }
    if (k == 2u) { return (p0 + p1) / 2u; }
    return vec3<u32>(0u);
}

// Texel i of the same block with the exact four-colour palette, in thirds of a code value (0..765).
fn bc1_thirds(lo: u32, hi: u32, i: u32) -> vec3<u32> {
    let p0 = expand565(lo & 0xFFFFu);
    let p1 = expand565(lo >> 16u);
    let k = (hi >> (2u * i)) & 3u;
    if (k == 0u) { return 3u * p0; }
    if (k == 1u) { return 3u * p1; }
    if (k == 2u) { return 2u * p0 + p1; }
    return p0 + 2u * p1;
}

// The 3-bit palette index of texel i of the BC4 block in words (lo, hi).
fn bc4_index(lo: u32, hi: u32, i: u32) -> u32 {
    let bit = 16u + 3u * i;
    if (bit + 3u <= 32u) { return (lo >> bit) & 7u; }
    if (bit >= 32u) { return (hi >> (bit - 32u)) & 7u; }
    return ((lo >> bit) | (hi << (32u - bit))) & 7u;
}

// Texel i of the BC4 block, palette truncated to 8 bits.
fn bc4(lo: u32, hi: u32, i: u32) -> u32 {
    let a0 = lo & 255u;
    let a1 = (lo >> 8u) & 255u;
    let k = bc4_index(lo, hi, i);
    if (k == 0u) { return a0; }
    if (k == 1u) { return a1; }
    let j = k - 1u;
    if (a0 > a1) { return ((7u - j) * a0 + j * a1) / 7u; }
    if (k == 6u) { return 0u; }
    if (k == 7u) { return 255u; }
    return ((5u - j) * a0 + j * a1) / 5u;
}

// Texel i of the BC4 block, exact palette value in 35ths of a code value (0..8925).
fn bc4_35ths(lo: u32, hi: u32, i: u32) -> u32 {
    let a0 = lo & 255u;
    let a1 = (lo >> 8u) & 255u;
    let k = bc4_index(lo, hi, i);
    if (k == 0u) { return a0 * 35u; }
    if (k == 1u) { return a1 * 35u; }
    let j = k - 1u;
    if (a0 > a1) { return ((7u - j) * a0 + j * a1) * 5u; }
    if (k == 6u) { return 0u; }
    if (k == 7u) { return 8925u; }
    return ((5u - j) * a0 + j * a1) * 7u;
}

// n / d rounded to nearest (halves up), clamped to 0..255; d > 0.
fn round8(n: i32, d: i32) -> u32 {
    if (n <= 0) { return 0u; }
    if (n >= 255 * d) { return 255u; }
    return (2u * u32(n) + u32(d)) / (2u * u32(d));
}

// HAP scaled YCoCg: c = (Co, Cg, scale) in thirds, y in 35ths (`ycocg_scaled_exact`).
fn hap_ycocg(c: vec3<u32>, y35: u32) -> vec3<u32> {
    let k = i32(c.z) + 24;
    let d = 35 * k;
    let y = i32(y35) * k;
    let r3 = i32(c.x);
    let g3 = i32(c.y);
    return vec3<u32>(round8(y + 280 * (r3 - g3), d), round8(y + 280 * (g3 - 384), d), round8(y + 280 * (768 - r3 - g3), d));
}

// ---- BC7 (mode 4 of U.mode), `dxvkit_plus_core::bc7::bc7_block` line for line --------------------
// Tables from core::bc7: the subset of each texel of the 64 two-subset partitions (bit px) and of
// the 64 three-subset partitions (bits 2px..2px+1); the anchor texel of subset 1 (two subsets, bits
// 0-7), of subset 1 and 2 (three subsets, bits 8-15 and 16-23).
const BC7_P2 = array<u32, 64>(
    0xCCCCu, 0x8888u, 0xEEEEu, 0xECC8u, 0xC880u, 0xFEECu, 0xFEC8u, 0xEC80u,
    0xC800u, 0xFFECu, 0xFE80u, 0xE800u, 0xFFE8u, 0xFF00u, 0xFFF0u, 0xF000u,
    0xF710u, 0x008Eu, 0x7100u, 0x08CEu, 0x008Cu, 0x7310u, 0x3100u, 0x8CCEu,
    0x088Cu, 0x3110u, 0x6666u, 0x366Cu, 0x17E8u, 0x0FF0u, 0x718Eu, 0x399Cu,
    0xAAAAu, 0xF0F0u, 0x5A5Au, 0x33CCu, 0x3C3Cu, 0x55AAu, 0x9696u, 0xA55Au,
    0x73CEu, 0x13C8u, 0x324Cu, 0x3BDCu, 0x6996u, 0xC33Cu, 0x9966u, 0x0660u,
    0x0272u, 0x04E4u, 0x4E40u, 0x2720u, 0xC936u, 0x936Cu, 0x39C6u, 0x639Cu,
    0x9336u, 0x9CC6u, 0x817Eu, 0xE718u, 0xCCF0u, 0x0FCCu, 0x7744u, 0xEE22u,
);
const BC7_P3 = array<u32, 64>(
    0xAA685050u, 0x6A5A5040u, 0x5A5A4200u, 0x5450A0A8u, 0xA5A50000u, 0xA0A05050u, 0x5555A0A0u, 0x5A5A5050u,
    0xAA550000u, 0xAA555500u, 0xAAAA5500u, 0x90909090u, 0x94949494u, 0xA4A4A4A4u, 0xA9A59450u, 0x2A0A4250u,
    0xA5945040u, 0x0A425054u, 0xA5A5A500u, 0x55A0A0A0u, 0xA8A85454u, 0x6A6A4040u, 0xA4A45000u, 0x1A1A0500u,
    0x0050A4A4u, 0xAAA59090u, 0x14696914u, 0x69691400u, 0xA08585A0u, 0xAA821414u, 0x50A4A450u, 0x6A5A0200u,
    0xA9A58000u, 0x5090A0A8u, 0xA8A09050u, 0x24242424u, 0x00AA5500u, 0x24924924u, 0x24499224u, 0x50A50A50u,
    0x500AA550u, 0xAAAA4444u, 0x66660000u, 0xA5A0A5A0u, 0x50A050A0u, 0x69286928u, 0x44AAAA44u, 0x66666600u,
    0xAA444444u, 0x54A854A8u, 0x95809580u, 0x96969600u, 0xA85454A8u, 0x80959580u, 0xAA141414u, 0x96960000u,
    0xAAAA1414u, 0xA05050A0u, 0xA0A5A5A0u, 0x96000000u, 0x40804080u, 0xA9A8A9A8u, 0xAAAAAA44u, 0x2A4A5254u,
);
const BC7_ANCHORS = array<u32, 64>(
    0x0F030Fu, 0x08030Fu, 0x080F0Fu, 0x030F0Fu, 0x0F080Fu, 0x0F030Fu, 0x030F0Fu, 0x080F0Fu,
    0x0F080Fu, 0x0F080Fu, 0x0F060Fu, 0x0F060Fu, 0x0F060Fu, 0x0F050Fu, 0x0F030Fu, 0x08030Fu,
    0x0F030Fu, 0x080302u, 0x0F0808u, 0x030F02u, 0x0F0302u, 0x080308u, 0x0F0608u, 0x080A0Fu,
    0x030502u, 0x0F0808u, 0x060802u, 0x0A0602u, 0x0F0808u, 0x0F0508u, 0x0A0F02u, 0x080F02u,
    0x0F080Fu, 0x030F0Fu, 0x0F0306u, 0x0A0508u, 0x0A0602u, 0x080A08u, 0x09080Fu, 0x0A0F0Fu,
    0x060F02u, 0x0F0308u, 0x080F02u, 0x0F0502u, 0x030F02u, 0x060F0Fu, 0x060F0Fu, 0x080F06u,
    0x0F0306u, 0x030F02u, 0x0F0506u, 0x0F0508u, 0x0F050Fu, 0x0F080Fu, 0x0F0502u, 0x0F0A02u,
    0x0F050Fu, 0x0F0A0Fu, 0x0F080Fu, 0x0F0D0Fu, 0x030F0Fu, 0x0F0C02u, 0x0F0302u, 0x08030Fu,
);

// Word i (0..3) of the block; 0 past the end (bits beyond 128 read as 0, as `Bits::take`).
fn bc7_word(b: vec4<u32>, i: u32) -> u32 {
    switch i {
        case 0u: { return b.x; }
        case 1u: { return b.y; }
        case 2u: { return b.z; }
        case 3u: { return b.w; }
        default: { return 0u; }
    }
}

// `n` bits (0..8) of the block, LSB first, from bit `at`.
fn bc7_bits(b: vec4<u32>, at: u32, n: u32) -> u32 {
    if (n == 0u) { return 0u; }
    let w = at >> 5u;
    let o = at & 31u;
    var v = bc7_word(b, w) >> o;
    if (o + n > 32u) { v = v | (bc7_word(b, w + 1u) << (32u - o)); }
    return v & ((1u << n) - 1u);
}

// An n-bit value (5..8 bits here) widened to 8 bits by bit replication (`bc7::expand`).
fn bc7_expand(v: u32, n: u32) -> u32 {
    if (n >= 8u) { return v & 255u; }
    let s = v << (8u - n);
    return s | (s >> n);
}

// The 2-, 3- and 4-bit index weights of the specification (`bc7::W2`, `W3`, `W4`).
const BC7_W2 = array<u32, 4>(0u, 21u, 43u, 64u);
const BC7_W3 = array<u32, 8>(0u, 9u, 18u, 27u, 37u, 46u, 55u, 64u);
const BC7_W4 = array<u32, 16>(0u, 4u, 9u, 13u, 17u, 21u, 26u, 30u, 34u, 38u, 43u, 47u, 51u, 55u, 60u, 64u);

fn bc7_weight(bits: u32, i: u32) -> u32 {
    if (bits == 2u) { return BC7_W2[i & 3u]; }
    if (bits == 3u) { return BC7_W3[i & 7u]; }
    return BC7_W4[i & 15u];
}

fn bc7_interp(e0: u32, e1: u32, w: u32) -> u32 {
    return ((64u - w) * e0 + w * e1 + 32u) >> 6u;
}

// Per mode (`bc7::MODES`): subsets, partition / rotation / index-selection bits, colour and alpha
// bits, endpoint p-bits (1 = per endpoint, 2 = shared per subset), index and second index bits.
struct Bc7Mode { ns: u32, pb: u32, rb: u32, isb: u32, cb: u32, ab: u32, pbits: u32, ib: u32, ib2: u32 };

fn bc7_mode(m: u32) -> Bc7Mode {
    switch m {
        case 0u: { return Bc7Mode(3u, 4u, 0u, 0u, 4u, 0u, 1u, 3u, 0u); }
        case 1u: { return Bc7Mode(2u, 6u, 0u, 0u, 6u, 0u, 2u, 3u, 0u); }
        case 2u: { return Bc7Mode(3u, 6u, 0u, 0u, 5u, 0u, 0u, 2u, 0u); }
        case 3u: { return Bc7Mode(2u, 6u, 0u, 0u, 7u, 0u, 1u, 2u, 0u); }
        case 4u: { return Bc7Mode(1u, 0u, 2u, 1u, 5u, 6u, 0u, 2u, 3u); }
        case 5u: { return Bc7Mode(1u, 0u, 2u, 0u, 7u, 8u, 0u, 2u, 2u); }
        case 6u: { return Bc7Mode(1u, 0u, 0u, 0u, 7u, 7u, 1u, 4u, 0u); }
        default: { return Bc7Mode(2u, 6u, 0u, 0u, 5u, 5u, 1u, 2u, 0u); }
    }
}

// Texel i (0..15) of the BC7 block in words b, straight RGBA8. A reserved mode (first byte 0) is
// transparent black, as the specification and `bc7_block` define it.
fn bc7(b: vec4<u32>, i: u32) -> vec4<u32> {
    let first = b.x & 255u;
    if (first == 0u) { return vec4<u32>(0u); }
    let mode = countTrailingZeros(first);
    let m = bc7_mode(mode);
    var at = mode + 1u;
    let part = bc7_bits(b, at, m.pb);
    at = at + m.pb;
    let rotation = bc7_bits(b, at, m.rb);
    at = at + m.rb;
    let isel = bc7_bits(b, at, m.isb);
    at = at + m.isb;
    // This texel's subset and the anchors (texel 0 always; then subset 1's, and subset 2's).
    var subset = 0u;
    var anchor1 = 16u;
    var anchor2 = 16u;
    let anchors = BC7_ANCHORS[part & 63u];
    if (m.ns == 2u) {
        subset = (BC7_P2[part & 63u] >> i) & 1u;
        anchor1 = anchors & 255u;
    } else if (m.ns == 3u) {
        subset = (BC7_P3[part & 63u] >> (2u * i)) & 3u;
        anchor1 = (anchors >> 8u) & 255u;
        anchor2 = (anchors >> 16u) & 255u;
    }
    // Endpoints, channel-major: R, G, B (cb bits each), A (ab bits), each subset's two endpoints.
    let pstart = at + (3u * m.cb + m.ab) * 2u * m.ns;
    var p0 = 0u;
    var p1 = 0u;
    if (m.pbits == 1u) {
        p0 = bc7_bits(b, pstart + 2u * subset, 1u);
        p1 = bc7_bits(b, pstart + 2u * subset + 1u, 1u);
    } else if (m.pbits == 2u) {
        p0 = bc7_bits(b, pstart + subset, 1u);
        p1 = p0;
    }
    let has_p = u32(m.pbits != 0u);
    var e0 = vec4<u32>(0u);
    var e1 = vec4<u32>(0u);
    for (var ch = 0u; ch < 4u; ch = ch + 1u) {
        var n = m.cb;
        var base = at + ch * m.cb * 2u * m.ns;
        if (ch == 3u) { n = m.ab; }
        let x0 = (bc7_bits(b, base + (2u * subset) * n, n) << has_p) | p0;
        let x1 = (bc7_bits(b, base + (2u * subset + 1u) * n, n) << has_p) | p1;
        var v0 = 255u;
        var v1 = 255u;
        if (ch < 3u || m.ab != 0u) {
            v0 = bc7_expand(x0, n + has_p);
            v1 = bc7_expand(x1, n + has_p);
        }
        e0[ch] = v0;
        e1[ch] = v1;
    }
    // Indices: each texel ib bits, an anchor one bit less; the second set (modes 4, 5) after them.
    let istart = pstart + select(0u, select(m.ns, 2u * m.ns, m.pbits == 1u), m.pbits != 0u);
    var before = u32(i > 0u) + u32(anchor1 < i) + u32(anchor2 < i);
    let anchor = i == 0u || i == anchor1 || i == anchor2;
    let i1 = bc7_bits(b, istart + i * m.ib - before, m.ib - u32(anchor));
    var i2 = 0u;
    if (m.ib2 > 0u) {
        let i2start = istart + 16u * m.ib - 1u;
        i2 = bc7_bits(b, i2start + i * m.ib2 - u32(i > 0u), m.ib2 - u32(i == 0u));
    }
    var ci = i1;
    var cbits = m.ib;
    var ai = i1;
    var abits = m.ib;
    if (m.ib2 > 0u) {
        if (isel == 0u) {
            ai = i2;
            abits = m.ib2;
        } else {
            ci = i2;
            cbits = m.ib2;
        }
    }
    let cw = bc7_weight(cbits, ci);
    let aw = bc7_weight(abits, ai);
    var p = vec4<u32>(bc7_interp(e0.x, e1.x, cw), bc7_interp(e0.y, e1.y, cw), bc7_interp(e0.z, e1.z, cw), bc7_interp(e0.w, e1.w, aw));
    if (rotation == 1u) { p = p.wyzx; }
    if (rotation == 2u) { p = p.xwzy; }
    if (rotation == 3u) { p = p.xywz; }
    return p;
}

@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let xy = vec2<u32>(pos.xy);
    let i = (xy.y & 3u) * 4u + (xy.x & 3u);
    let at = vec2<i32>(xy / 4u);
    let b = textureLoad(t0, at, 0);
    var rgb = vec3<u32>(0u);
    var a = 255u;
    switch u.mode {
        case 0u: { // BC1
            rgb = bc1(b.x, b.y, i, false);
        }
        case 1u: { // BC3
            rgb = bc1(b.z, b.w, i, true);
            if ((u.flags & 1u) == 0u) { a = bc4(b.x, b.y, i); }
        }
        case 2u: { // HAP Q (+ BC4 alpha plane)
            rgb = hap_ycocg(bc1_thirds(b.z, b.w, i), bc4_35ths(b.x, b.y, i));
            if ((u.flags & 2u) != 0u) {
                let ab = textureLoad(t1, at, 0);
                a = bc4(ab.x, ab.y, i);
            }
        }
        case 4u: { // BC7
            let p = bc7(b, i);
            rgb = p.xyz;
            if ((u.flags & 1u) == 0u) { a = p.w; }
        }
        default: { // alpha only, shown as grey
            rgb = vec3<u32>(bc4(b.x, b.y, i));
        }
    }
    if ((u.flags & 4u) != 0u && a != 0u) { rgb = min((rgb * 255u + vec3<u32>(a / 2u)) / a, vec3<u32>(255u)); }
    return vec4<f32>(vec3<f32>(rgb), f32(a)) / 255.0;
}
"#;

/// WGSL uniforms (`U`).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniform {
    mode: u32,
    flags: u32,
    matrix: u32,
    full_range: u32,
    size: [u32; 2],
    pad: [u32; 2],
}

/// Native frame planes already imported as textures by a platform crate (zero-copy).
pub struct NativePlanes<'a> {
    /// `Bgra`: one BGRA8 plane; `Yuv420`: R8 luma + RG8 chroma.
    pub textures: &'a [wgpu::Texture],
    pub layout: PixelLayout,
    pub matrix: YuvMatrix,
    pub width: u32,
    pub height: u32,
}

fn tex_entry(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn uint_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Uint, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

/// A full-screen-triangle pipeline from `source` (entry points `vs` / `fs`) writing [`OUTPUT_FORMAT`].
fn convert_pipeline(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, label: &str, source: &str) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(source.into()) });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(layout)], immediate_size: 0 });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[Some(OUTPUT_FORMAT.into())],
            compilation_options: Default::default(),
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// The WGSL program a frame is converted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Program {
    /// [`CONVERT_WGSL`]: float textures (BC textures decoded by the GPU, RGBA8, native planes).
    Sampled,
    /// [`DXV_HQ_WGSL`]: DXV High Quality blocks as integer texels.
    DxvHq,
    /// [`BC_EXACT_WGSL`]: BC1 / BC3 / BC4 / BC7 blocks as integer texels.
    Exact,
}

impl Program {
    /// Whether the program reads blocks uploaded as integer texels instead of BC textures.
    fn raw(self) -> bool {
        self != Program::Sampled
    }
}

/// The program for frames of `shading` whose first plane is `first`, on `path`. DXV High Quality
/// is always decoded in the shader; on the exact path so are the BC1 / BC3 / BC4 frames whose CPU
/// decode is the integer conventions of `dxvkit_plus_core::bc`.
fn program_for(path: BlockPath, shading: Shading, first: Option<PixelFormat>) -> Program {
    let exact = path == BlockPath::Exact
        && match shading {
            Shading::Rgba { .. } => matches!(first, Some(PixelFormat::Bc1 | PixelFormat::Bc3 | PixelFormat::Bc7)),
            Shading::RgbaPremultiplied | Shading::HapYcocg { .. } => first == Some(PixelFormat::Bc3),
            Shading::AlphaOnly => first == Some(PixelFormat::Bc4),
            Shading::Hdr | Shading::Lmbx | Shading::DxvYcocg { .. } => false,
        };
    match shading {
        Shading::DxvYcocg { .. } => Program::DxvHq,
        _ if exact => Program::Exact,
        _ => Program::Sampled,
    }
}

/// The integer texture holding one block per texel: 8-byte blocks (BC1, BC4) -> `Rg32Uint`,
/// 16-byte blocks (BC3, BC5, BC7) -> `Rgba32Uint`.
fn raw_format(format: PixelFormat) -> Result<wgpu::TextureFormat, GpuError> {
    match format {
        PixelFormat::Bc1 | PixelFormat::Bc4 => Ok(wgpu::TextureFormat::Rg32Uint),
        PixelFormat::Bc3 | PixelFormat::Bc5 | PixelFormat::Bc7 => Ok(wgpu::TextureFormat::Rgba32Uint),
        other => Err(GpuError::Invalid(format!("{other:?} blocks are not uploaded as integer texels"))),
    }
}

/// The conversion pipelines (shared by every clip on a device).
pub struct Converter {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    raw_layout: wgpu::BindGroupLayout,
    raw_pipeline: wgpu::RenderPipeline,
    /// [`BC_EXACT_WGSL`] on `raw_layout`, built on the first exact frame (renderers on the sampled
    /// path never compile it).
    exact_pipeline: std::sync::OnceLock<wgpu::RenderPipeline>,
    sampler: wgpu::Sampler,
    dummy: wgpu::TextureView,
}

impl Converter {
    pub fn new(device: &wgpu::Device) -> Converter {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dxvkit-plus-convert"),
            entries: &[
                tex_entry(0, false),
                tex_entry(1, true),
                uniform_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline = convert_pipeline(device, &layout, "dxvkit-plus-convert", CONVERT_WGSL);
        let raw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dxvkit-plus-convert-blocks"),
            entries: &[uint_entry(0), uint_entry(1), uniform_entry(2)],
        });
        let raw_pipeline = convert_pipeline(device, &raw_layout, "dxvkit-plus-convert-dxv-hq", DXV_HQ_WGSL);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dxvkit-plus-linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let dummy = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("dxvkit-plus-dummy"),
                size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OUTPUT_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        Converter { layout, pipeline, raw_layout, raw_pipeline, exact_pipeline: std::sync::OnceLock::new(), sampler, dummy }
    }

    /// Record one conversion pass from `t0` (+ `t1`) into `target` (an [`OUTPUT_FORMAT`] view).
    fn pass(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        t0: &wgpu::TextureView,
        t1: Option<&wgpu::TextureView>,
        uniform: &wgpu::Buffer,
        target: &wgpu::TextureView,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
        program: Program,
    ) {
        let pipeline = match program {
            Program::Sampled => &self.pipeline,
            Program::DxvHq => &self.raw_pipeline,
            Program::Exact => self.exact_pipeline.get_or_init(|| convert_pipeline(device, &self.raw_layout, "dxvkit-plus-convert-bc-exact", BC_EXACT_WGSL)),
        };
        let bind = if program.raw() {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dxvkit-plus-convert-blocks"),
                layout: &self.raw_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(t0) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t1.unwrap_or(t0)) },
                    wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                ],
            })
        } else {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dxvkit-plus-convert"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(t0) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t1.unwrap_or(&self.dummy)) },
                    wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            })
        };
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("dxvkit-plus-convert"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(pipeline);
        rp.set_bind_group(0, &bind, &[]);
        rp.draw(0..3, 0..1);
    }
}

/// What [`FrameRenderer::upload`] left for [`FrameRenderer::encode_convert`].
#[derive(Clone, Copy)]
struct Pending {
    shading: Shading,
    width: u32,
    height: u32,
    planes: usize,
    /// The program the frame was uploaded for (it decides BC textures or integer texels).
    program: Program,
}

/// An uploaded frame a compositor can sample as it is, skipping the conversion pass (see
/// [`FrameRenderer::direct_view`]). The texels are the GPU's own BC decode ([`BlockPath::Sampled`]'s
/// contract; for BC7 exact on valid blocks).
#[derive(Clone, Copy, Debug)]
pub struct DirectView<'a> {
    /// The frame's block texture (BC1 / BC3 / BC7, sampled as stored RGBA).
    pub view: &'a wgpu::TextureView,
    /// The picture size: sample the top-left `width` x `height` texels (the texture can be a little
    /// larger, rounded up to whole 4x4 blocks).
    pub width: u32,
    pub height: u32,
    /// Treat alpha as 1 (HAP "RGB": BC1's punch-through texels would otherwise read as alpha 0).
    pub opaque: bool,
}

struct Upload {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    format: PixelFormat,
    width: u32,
    height: u32,
    /// Blocks as integer texels ([`raw_format`]) rather than a BC texture.
    raw: bool,
}

/// Renders one clip's frames: keeps its upload textures, uniform and output across frames.
pub struct FrameRenderer {
    converter: Converter,
    path: BlockPath,
    uploads: Vec<Upload>,
    uniform: wgpu::Buffer,
    last_uniform: Option<Uniform>,
    output: Option<(wgpu::Texture, wgpu::TextureView)>,
    pending: Option<Pending>,
    /// LMBX's compute decoder (created on the first LMBX frame).
    lmbx: Option<crate::lmbx::LmbxDecoder>,
    /// Bytes written with `write_texture` for the last frame (0 for native frames: zero-copy;
    /// for LMBX the bitstream).
    pub last_upload_bytes: usize,
}

impl FrameRenderer {
    /// A renderer on the default path ([`BlockPath::Sampled`]).
    pub fn new(device: &wgpu::Device) -> FrameRenderer {
        FrameRenderer::with_path(device, BlockPath::default())
    }

    /// A renderer on `path`.
    pub fn with_path(device: &wgpu::Device, path: BlockPath) -> FrameRenderer {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dxvkit-plus-convert-u"),
            size: std::mem::size_of::<Uniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        FrameRenderer {
            converter: Converter::new(device),
            path,
            uploads: Vec::new(),
            uniform,
            last_uniform: None,
            output: None,
            pending: None,
            lmbx: None,
            last_upload_bytes: 0,
        }
    }

    /// The path this renderer decodes BC1 / BC3 / BC4 frames on.
    pub fn path(&self) -> BlockPath {
        self.path
    }

    /// Change the path for the frames uploaded from now on. A frame uploaded before the change must
    /// be uploaded again before it can be converted (its texture was made for the other path).
    pub fn set_path(&mut self, path: BlockPath) {
        if self.path != path {
            self.path = path;
            self.pending = None;
        }
    }

    /// After [`FrameRenderer::upload`] / [`FrameRenderer::upload_staged`]: the uploaded texture
    /// itself when the frame needs no conversion to be sampled — one RGBA block plane (LMBD
    /// LMB7, HAP R BC7, HAP RGBA BC3, HAP RGB BC1 with `opaque`). Sampling it gives the conversion
    /// pass's output values (exactly; for `opaque` frames, with alpha taken as 1), so a compositor
    /// can skip [`FrameRenderer::encode_convert`] for these frames. `None` for frames that need
    /// the pass (HAP Q's and DXV HQ's YCoCg, premultiplied colour, alpha-only, HDR, RGBA8 images)
    /// or before an upload. DXV DXT1 / DXT5 (straight) qualify; their textures are the coded size
    /// (rounded up to 16). On [`BlockPath::Exact`] BC1 / BC3 / BC7 frames are integer texels that
    /// only the conversion pass can read, so there is no direct view there (0.9.0: BC7 included).
    pub fn direct_view(&self) -> Option<DirectView<'_>> {
        let p = self.pending?;
        let Shading::Rgba { opaque } = p.shading else { return None };
        let up = self.uploads.first()?;
        let block = matches!(up.format, PixelFormat::Bc1 | PixelFormat::Bc3 | PixelFormat::Bc7);
        (p.planes == 1 && block && !up.raw).then_some(DirectView { view: &up.view, width: p.width, height: p.height, opaque })
    }

    /// The output texture (`OUTPUT_FORMAT`, the clip's size) after a render.
    pub fn output(&self) -> Option<&wgpu::Texture> {
        self.output.as_ref().map(|(t, _)| t)
    }

    fn ensure_output(&mut self, device: &wgpu::Device, w: u32, h: u32) -> Result<wgpu::TextureView, GpuError> {
        if w == 0 || h == 0 || w > crate::MAX_EDGE || h > crate::MAX_EDGE {
            return Err(GpuError::Invalid(format!("output size {w}x{h}")));
        }
        crate::check_device_size(device, w, h)?;
        let fits = self.output.as_ref().is_some_and(|(t, _)| t.width() == w && t.height() == h);
        if !fits {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("dxvkit-plus-output"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OUTPUT_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let v = t.create_view(&Default::default());
            self.output = Some((t, v));
        }
        self.output.as_ref().map(|(_, v)| v.clone()).ok_or_else(|| GpuError::Invalid("no output".into()))
    }

    fn set_uniform(&mut self, queue: &wgpu::Queue, u: Uniform) {
        if self.last_uniform != Some(u) {
            queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&u));
            self.last_uniform = Some(u);
        }
    }

    /// Upload a CPU-decoded frame's planes and convert it; returns the output texture. Submits.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, decoded: &Decoded) -> Result<&wgpu::Texture, GpuError> {
        self.upload(device, queue, decoded)?;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render") });
        self.encode_convert(device, queue, &mut enc, None)?;
        queue.submit([enc.finish()]);
        self.output().ok_or_else(|| GpuError::Invalid("no output".into()))
    }

    /// The first half of [`FrameRenderer::render`]: upload a CPU-decoded frame's planes into this
    /// renderer's textures (`Queue::write_texture`; wgpu stages the bytes and copies them before
    /// the next submission). Follow with [`FrameRenderer::encode_convert`].
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, decoded: &Decoded) -> Result<(), GpuError> {
        if let Some((blob, width, height)) = lmbx_frame(decoded) {
            let dec = self.prepare_lmbx(device, blob, width, height)?;
            dec.write(queue, blob)?;
            return Ok(());
        }
        let (shading, width, height, program) = self.prepare_planes(device, decoded)?;
        let Decoded::Planes { planes, .. } = decoded else { return Err(GpuError::Invalid("not planes".into())) };
        let mut bytes = 0usize;
        for (up, p) in self.uploads.iter().zip(planes) {
            if up.raw {
                write_raw(queue, &up.texture, p.format, p.width, p.height, &p.data)?;
            } else {
                write_bc(queue, &up.texture, p.format, p.width, p.height, &p.data)?;
            }
            bytes = bytes.saturating_add(p.data.len());
        }
        self.last_upload_bytes = bytes;
        self.pending = Some(Pending { shading, width, height, planes: planes.len(), program });
        Ok(())
    }

    /// [`FrameRenderer::upload`] through persistent staging memory: copies the planes into
    /// `belt` (mapped buffers reused across frames, instead of a new staging buffer per
    /// `write_texture`) and records the buffer-to-texture copies into `encoder`. Then
    /// [`FrameRenderer::encode_convert`], `belt.finish()`,
    /// submit, `belt.recall()`.
    pub fn upload_staged(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        decoded: &Decoded,
    ) -> Result<(), GpuError> {
        if let Some((blob, width, height)) = lmbx_frame(decoded) {
            let dec = self.prepare_lmbx(device, blob, width, height)?;
            let bits = dec.bits.as_ref().ok_or_else(|| GpuError::Invalid("LMBX buffers not prepared".into()))?;
            let size = (blob.len() as u64).div_ceil(4).max(1).saturating_mul(4);
            let slice_size = wgpu::BufferSize::new(size).ok_or_else(|| GpuError::Invalid("empty LMBX frame".into()))?;
            let (buffer, offset) = {
                let slice = belt.allocate(slice_size, STAGING_ALIGNMENT);
                let (buffer, offset) = (slice.buffer().clone(), slice.offset());
                let mut view = slice.get_mapped_range_mut().map_err(|e| GpuError::Device(e.to_string()))?;
                let mut padded = blob.to_vec();
                padded.resize(size as usize, 0);
                view.slice(..).copy_from_slice(&padded);
                (buffer, offset)
            };
            encoder.copy_buffer_to_buffer(&buffer, offset, bits, 0, size);
            return Ok(());
        }
        let (shading, width, height, program) = self.prepare_planes(device, decoded)?;
        let Decoded::Planes { planes, .. } = decoded else { return Err(GpuError::Invalid("not planes".into())) };
        let mut bytes = 0usize;
        for (up, p) in self.uploads.iter().zip(planes) {
            let l = StagingLayout::of(p.format, p.width, p.height)?;
            if p.data.len() != l.tight_bytes() {
                return Err(GpuError::Invalid(format!("{:?} {}x{} needs {} bytes, got {}", p.format, p.width, p.height, l.tight_bytes(), p.data.len())));
            }
            let (buffer, offset) = {
                let slice = belt.allocate(l.size()?, STAGING_ALIGNMENT);
                let (buffer, offset) = (slice.buffer().clone(), slice.offset());
                let mut view = slice.get_mapped_range_mut().map_err(|e| GpuError::Device(e.to_string()))?;
                l.write(view.slice(..), &p.data);
                (buffer, offset)
            };
            l.copy(encoder, &buffer, offset, &up.texture);
            bytes = bytes.saturating_add(p.data.len());
        }
        self.last_upload_bytes = bytes;
        self.pending = Some(Pending { shading, width, height, planes: planes.len(), program });
        Ok(())
    }

    /// Validate an LMBX frame's header (CPU, headers only), size the decoder's buffers and make
    /// the frame pending.
    fn prepare_lmbx(&mut self, device: &wgpu::Device, blob: &[u8], width: u32, height: u32) -> Result<&crate::lmbx::LmbxDecoder, GpuError> {
        let shape = crate::lmbx::shape(blob, width, height)?;
        let dec = self.lmbx.get_or_insert_with(|| crate::lmbx::LmbxDecoder::new(device, OUTPUT_FORMAT));
        dec.prepare(device, shape);
        self.last_upload_bytes = blob.len();
        self.pending = Some(Pending { shading: Shading::Lmbx, width, height, planes: 1, program: Program::Sampled });
        Ok(dec)
    }

    /// Check a frame's planes and make sure this renderer has a texture of the right kind, format
    /// and size for each (reused across frames).
    fn prepare_planes(&mut self, device: &wgpu::Device, decoded: &Decoded) -> Result<(Shading, u32, u32, Program), GpuError> {
        let Decoded::Planes { width, height, shading, planes, .. } = decoded else {
            return Err(GpuError::Invalid("native frames are rendered with render_native (after the platform import)".into()));
        };
        let shapes: Vec<(PixelFormat, u32, u32)> = planes.iter().map(|p| (p.format, p.width, p.height)).collect();
        fits(device, *width, *height, &shapes)?;
        let program = program_for(self.path, *shading, shapes.first().map(|s| s.0));
        self.ensure_uploads(device, &shapes, program.raw())?;
        Ok((*shading, *width, *height, program))
    }

    fn ensure_uploads(&mut self, device: &wgpu::Device, shapes: &[(PixelFormat, u32, u32)], raw: bool) -> Result<(), GpuError> {
        if shapes.is_empty() || shapes.len() > 2 {
            return Err(GpuError::Invalid(format!("{} planes", shapes.len())));
        }
        for (i, &(format, width, height)) in shapes.iter().enumerate() {
            let reuse = self.uploads.get(i).is_some_and(|u| u.format == format && u.width == width && u.height == height && u.raw == raw);
            if !reuse {
                let texture = if raw {
                    create_raw_texture(device, format, width, height)?
                } else {
                    create_bc_texture(device, format, width, height, "dxvkit-plus-plane")?
                };
                let view = texture.create_view(&Default::default());
                let up = Upload { texture, view, format, width, height, raw };
                if i < self.uploads.len() {
                    if let Some(slot) = self.uploads.get_mut(i) {
                        *slot = up;
                    }
                } else {
                    self.uploads.push(up);
                }
            }
        }
        Ok(())
    }

    /// Record the copies of a frame staged with [`crate::Uploader::stage`] and filled (for example
    /// by `FrameSource::decode_into`) into this renderer's textures. The frame must be complete.
    /// Then [`FrameRenderer::encode_convert`] (or sample [`FrameRenderer::direct_view`]) and
    /// [`crate::Uploader::submit`].
    pub fn upload_from_stage(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, staged: StagedFrame) -> Result<(), GpuError> {
        if !staged.is_complete() {
            return Err(GpuError::Invalid("staged frame not completely written".into()));
        }
        let l = &staged.layout;
        let shapes: Vec<(PixelFormat, u32, u32)> = l.planes.iter().map(|p| (p.format, p.width, p.height)).collect();
        fits(device, l.width, l.height, &shapes)?;
        let program = program_for(self.path, l.shading, shapes.first().map(|s| s.0));
        self.ensure_uploads(device, &shapes, program.raw())?;
        let (shading, width, height, n) = (l.shading, l.width, l.height, l.planes.len());
        let mut bytes = 0usize;
        // Dropping each plane's mapped view before the copy is recorded; the belt unmaps on finish.
        for (up, p) in self.uploads.iter().zip(staged.planes) {
            let (buffer, offset, layout) = (p.buffer.clone(), p.offset, p.layout);
            drop(p);
            layout.copy(encoder, &buffer, offset, &up.texture);
            bytes = bytes.saturating_add(layout.tight_bytes());
        }
        self.last_upload_bytes = bytes;
        self.pending = Some(Pending { shading, width, height, planes: n, program });
        Ok(())
    }

    /// The second half of [`FrameRenderer::render`]: record the conversion of the last uploaded
    /// frame into `encoder` (no submit, so many renderers can share one submission). `timestamps`
    /// brackets the conversion pass (needs a `TIMESTAMP_QUERY` device). Returns the output
    /// texture, valid once `encoder` is submitted.
    pub fn encode_convert(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        timestamps: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> Result<&wgpu::Texture, GpuError> {
        let Pending { shading, width, height, planes, program } = self.pending.ok_or_else(|| GpuError::Invalid("encode_convert before upload".into()))?;
        if shading == Shading::Lmbx {
            let target = self.ensure_output(device, width, height)?;
            let dec = self.lmbx.as_ref().ok_or_else(|| GpuError::Invalid("LMBX decode before upload".into()))?;
            dec.encode(device, queue, encoder, &target, timestamps)?;
            return self.output().ok_or_else(|| GpuError::Invalid("no output".into()));
        }
        // RGBA as stored is mode 0 in `CONVERT_WGSL`; `BC_EXACT_WGSL` has one mode per block format.
        let rgba = match (program, self.uploads.first().map(|u| u.format)) {
            (Program::Exact, Some(PixelFormat::Bc3)) => 1,
            (Program::Exact, Some(PixelFormat::Bc7)) => 4,
            _ => 0,
        };
        let (mode, flags) = match shading {
            Shading::Rgba { opaque } => (rgba, u32::from(opaque)),
            Shading::HapYcocg { alpha } => (2, if alpha { 2 } else { 0 }),
            Shading::AlphaOnly => (3, 0),
            Shading::Hdr => (4, 0),
            Shading::RgbaPremultiplied => (rgba, 4),
            Shading::DxvYcocg { alpha, premultiplied } => (1, (u32::from(alpha) << 1) | (u32::from(premultiplied) << 2)),
            Shading::Lmbx => (0, 0), // decoded above
        };
        self.set_uniform(queue, Uniform { mode, flags, matrix: 1, full_range: 1, size: [width, height], pad: [0; 2] });
        let target = self.ensure_output(device, width, height)?;
        let first = self.uploads.first().ok_or_else(|| GpuError::Invalid("no plane".into()))?;
        if first.raw != program.raw() {
            return Err(GpuError::Invalid("the frame was uploaded for the other block path".into()));
        }
        let t0 = &first.view;
        let t1 = if planes == 2 { self.uploads.get(1).map(|u| &u.view) } else { None };
        if program == Program::DxvHq && t1.is_none() {
            return Err(GpuError::Invalid("DXV High Quality frames have two planes".into()));
        }
        if program == Program::Exact && shading == (Shading::HapYcocg { alpha: true }) && t1.is_none() {
            return Err(GpuError::Invalid("HAP Q Alpha frames have two planes".into()));
        }
        self.converter.pass(device, encoder, t0, t1, &self.uniform, &target, timestamps, program);
        self.output().ok_or_else(|| GpuError::Invalid("no output".into()))
    }

    /// Convert a frame that is already on the GPU — such as an [`crate::LmbdTexture`]'s BC7 view,
    /// which the GPU decodes as it samples — as stored RGBA, texel for texel from the top-left, at
    /// `width` x `height`. Returns the output texture. Submits.
    pub fn render_view(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<&wgpu::Texture, GpuError> {
        self.last_upload_bytes = 0;
        self.pending = None;
        self.set_uniform(queue, Uniform { mode: 0, flags: 0, matrix: 1, full_range: 1, size: [width, height], pad: [0; 2] });
        let target = self.ensure_output(device, width, height)?;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render-view") });
        self.converter.pass(device, &mut enc, view, None, &self.uniform, &target, None, Program::Sampled);
        queue.submit([enc.finish()]);
        self.output().ok_or_else(|| GpuError::Invalid("no output".into()))
    }

    /// Convert a native frame whose planes a platform crate imported (zero-copy); returns the
    /// output texture. Submits.
    pub fn render_native(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, native: &NativePlanes<'_>) -> Result<&wgpu::Texture, GpuError> {
        let (mode, full_range, want) = match native.layout {
            PixelLayout::Bgra => (0, 1, 1),
            PixelLayout::Yuv420 { full_range } => (5, u32::from(full_range), 2),
        };
        if native.textures.len() != want {
            return Err(GpuError::Invalid(format!("{:?} needs {want} planes, got {}", native.layout, native.textures.len())));
        }
        crate::check_device_size(device, native.width, native.height)?;
        self.last_upload_bytes = 0;
        self.pending = None;
        self.set_uniform(queue, Uniform { mode, flags: 0, matrix: native.matrix.index(), full_range, size: [native.width, native.height], pad: [0; 2] });
        let target = self.ensure_output(device, native.width, native.height)?;
        let views: Vec<wgpu::TextureView> = native.textures.iter().map(|t| t.create_view(&Default::default())).collect();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render-native") });
        let t0 = views.first().ok_or_else(|| GpuError::Invalid("no plane".into()))?;
        self.converter.pass(device, &mut enc, t0, views.get(1), &self.uniform, &target, None, Program::Sampled);
        queue.submit([enc.finish()]);
        self.output().ok_or_else(|| GpuError::Invalid("no output".into()))
    }
}

/// The device holds a frame of display size `width` x `height` with planes `shapes` (the output is
/// the display size; a plane can be a little larger, rounded up to whole blocks or DXV's 16).
fn fits(device: &wgpu::Device, width: u32, height: u32, shapes: &[(PixelFormat, u32, u32)]) -> Result<(), GpuError> {
    let (w, h) = shapes.iter().fold((width, height), |(w, h), s| (w.max(s.1), h.max(s.2)));
    crate::check_device_size(device, w, h)
}

/// A texture of `format` blocks as integer texels ([`raw_format`]): one texel per 4x4 block of a
/// `width` x `height` plane.
fn create_raw_texture(device: &wgpu::Device, format: PixelFormat, width: u32, height: u32) -> Result<wgpu::Texture, GpuError> {
    crate::check_size(format, width, height)?;
    crate::check_device_size(device, width / 4, height / 4)?;
    Ok(device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dxvkit-plus-plane-blocks"),
        size: wgpu::Extent3d { width: width / 4, height: height / 4, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: raw_format(format)?,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    }))
}

/// [`crate::write_bc`] for a texture made by [`create_raw_texture`]: the same bytes and rows.
fn write_raw(queue: &wgpu::Queue, texture: &wgpu::Texture, format: PixelFormat, width: u32, height: u32, data: &[u8]) -> Result<(), GpuError> {
    let want = crate::check_size(format, width, height)?;
    if texture.format() != raw_format(format)? || texture.width() != width / 4 || texture.height() != height / 4 {
        return Err(GpuError::Invalid(format!("{format:?} {width}x{height} blocks do not match the texture")));
    }
    if data.len() != want {
        return Err(GpuError::Invalid(format!("{format:?} {width}x{height} needs {want} bytes, got {}", data.len())));
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some((width / 4).saturating_mul(format.unit_bytes())), rows_per_image: Some(height / 4) },
        wgpu::Extent3d { width: width / 4, height: height / 4, depth_or_array_layers: 1 },
    );
    Ok(())
}

/// An LMBX frame's bitstream and picture size, if `decoded` is one.
fn lmbx_frame(decoded: &Decoded) -> Option<(&[u8], u32, u32)> {
    match decoded {
        Decoded::Planes { shading: Shading::Lmbx, planes, width, height, .. } => planes.first().map(|p| (p.data.as_slice(), *width, *height)),
        _ => None,
    }
}
