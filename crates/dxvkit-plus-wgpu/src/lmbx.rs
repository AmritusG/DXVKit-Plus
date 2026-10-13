//! LMBX on the GPU: the frame bitstream is uploaded as it is (nothing is decoded on the CPU but
//! the header and the tile mode map, which give the tile list) and decoded by compute passes,
//! bit-exact with `dxvkit_plus_core::lmbx`:
//!
//! 1. `tables` (one workgroup per table: coefficients; v2 the palette table; v3 the eight index
//!    tables and the edge-alpha table; and one for the directory's stream offsets): expand each
//!    sparse rANS table into slot entries, one read per decoded symbol;
//! 2. `entropy`, one dispatch in two kinds of workgroup (W8): the first ones decode the DCT tiles'
//!    streams (one invocation per stream: rANS, dequantised coefficients into each block's 8x8
//!    area); the ones after them do the serial part of each direct tile (one invocation per tile,
//!    one tile every [`DSPREAD`] lanes): v2 palette indices, v3 palette and quad indices with edge
//!    alpha, v3 alpha palettes. No workgroup mixes the two, so a 1024-symbol palette decode never
//!    holds the lanes of DCT streams;
//! 3. `idct` (one invocation per 8x8 block): the integer inverse DCT, in place (straight-line code
//!    generated from the basis); a direct tile's Y-plane blocks instead expand their 64 pixels as
//!    the CPU decoder stores them (packed RGBA8 in Y, forward chroma, `4 A`);
//! 4. a fragment pass ([`LMBX_OUT_WGSL`]): chroma (4:2:2 halo or 4:4:4), inverse YCoCg-R, clamp,
//!    direct tiles' RGBA8, 8-bit output.
//!
//! The planes are `i32` storage at the coded size (whole tiles): `4 * (2 + chroma planes) *
//! coded pixels` bytes per renderer (1080p 4:2:2 with alpha: 26 MB). The per-pass measurements
//! behind this layout are in `docs/data/w8-lmbx-v3-profile.txt`; `examples/lmbx_passes.rs` times
//! each pass ([`profile_passes`]).
//
// Lint note: sizes here derive from a frame header already validated by
// `dxvkit_plus_core::lmbx::Frame::parse` (dimensions <= 16384), so their products fit u32 / u64;
// the arithmetic lint is relaxed for this module only.
#![allow(clippy::arithmetic_side_effects)]

use crate::GpuError;
use dxvkit_plus_core::lmbx::{Chroma, Frame};

/// The uniforms both shaders share.
const PARAMS_WGSL: &str = r#"
struct P {
    w: u32, h: u32, cw: u32, ch: u32,
    chw: u32, tiles_x: u32, nt: u32, chroma: u32,
    alpha: u32, cratio: u32, plen: u32, coef_off: u32,
    qmap_off: u32, dir_off: u32, data_off: u32, nblocks: u32,
    v2: u32, modes_off: u32, pal_off: u32, v3: u32,
    ibits: u32, n_dct: u32, n_dir: u32, list_off: u32,
    qt: array<vec4<u32>, 4>,
    // v3: the eight index tables' and the edge-alpha table's offsets in the bitstream.
    t3: array<vec4<u32>, 3>,
};
"#;

/// Bitstream access both shaders share (after the bindings).
const PARAMS_FNS_WGSL: &str = r#"
fn byte_at(i: u32) -> u32 {
    if (i >= p.plen) { return 0u; }
    return (bits[i >> 2u] >> ((i & 3u) * 8u)) & 255u;
}
// Tile `t`'s mode byte (v1: 0, DCT; v3: colour mode | alpha mode << 4).
fn tile_mode(t: u32) -> u32 {
    if (p.v2 == 0u) { return 0u; }
    return byte_at(p.modes_off + t);
}
"#;

/// WGSL of the compute passes. The arithmetic is `dxvkit_plus_core::lmbx`'s, operation for
/// operation (i32 / u32, arithmetic shifts, wrapping rANS state).
pub const LMBX_BINDINGS_WGSL: &str = r#"
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> bits: array<u32>;
"#;

/// WGSL of the compute passes (after [`LMBX_BINDINGS_WGSL`]).
pub const LMBX_WGSL: &str = r#"
@group(0) @binding(2) var<storage, read_write> tab: array<u32>;
@group(0) @binding(3) var<storage, read_write> planes: array<i32>;

// tab layout, per table: dense frequencies (2048), cumulative frequencies (2049), then per slot
// (up to 16384) the slot entry — coefficient and v2 palette tables two words (the owning symbol |
// frequency << 11, and slot - cum[symbol]); v3 index and edge tables one word (symbol |
// (frequency - 1) << 8 | (slot - cum[symbol]) << 20). Tables: coefficients (0); the v2 palette
// table (1) or the v3 index tables (1..=8); the v3 edge-alpha table (9). Then the stream offsets
// (data-relative), then the tile list (`list_off`: the DCT tiles, then the direct-work tiles).
const C_FREQ: u32 = 0u;
const C_CUM: u32 = 2048u;
const C_SLOT: u32 = 4097u;
const T_STRIDE: u32 = 36865u;
const T_EDGE: u32 = 9u * T_STRIDE;
const OFFS: u32 = 10u * T_STRIDE;

var<private> ZZ: array<u32, 64> = array<u32, 64>(
    0u, 1u, 8u, 16u, 9u, 2u, 3u, 10u, 17u, 24u, 32u, 25u, 18u, 11u, 4u, 5u, 12u, 19u, 26u, 33u, 40u, 48u, 41u, 34u, 27u, 20u, 13u, 6u, 7u, 14u, 21u, 28u,
    35u, 42u, 49u, 56u, 57u, 50u, 43u, 36u, 29u, 22u, 15u, 23u, 30u, 37u, 44u, 51u, 58u, 59u, 52u, 45u, 38u, 31u, 39u, 46u, 53u, 60u, 61u, 54u, 47u, 55u, 62u, 63u);

fn le16(i: u32) -> u32 { return byte_at(i) | (byte_at(i + 1u) << 8u); }
fn le32(i: u32) -> u32 { return le16(i) | (le16(i + 2u) << 16u); }
fn qt(i: u32) -> u32 { return p.qt[i >> 2u][i & 3u]; }
fn t3(k: u32) -> u32 { return p.t3[k >> 2u][k & 3u]; }

var<workgroup> wcum: array<u32, 2049>;
var<workgroup> wpart: array<u32, 256>;

// Workgroups: 0 the coefficient table, 1 the v2 palette table, 2 the stream offsets (v2, v3),
// 3..=10 the v3 index tables, 11 the v3 edge-alpha table; tables no tile of the frame uses are
// skipped. A table: scatter the frequencies, prefix-sum them (8 symbols per thread, then the 256
// partial sums), and give every slot the largest symbol s with cum[s] <= slot (binary search in
// workgroup memory). (W8: variants that kept the frequencies in workgroup memory, or split a table
// over many workgroups, measured slower — docs/data/w8-lmbx-v3-profile.txt.)
@compute @workgroup_size(256)
fn tables(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    if (wg.x > 0u && p.v2 == 0u) { return; }
    if (wg.x == 1u && p.v3 == 1u) { return; }
    if (wg.x >= 3u && p.v3 == 0u) { return; }
    // Tables no tile uses: the coefficients without DCT tiles, the palette tables without direct
    // work.
    if (wg.x == 0u && p.n_dct == 0u) { return; }
    if ((wg.x == 1u || wg.x >= 3u) && p.n_dir == 0u) { return; }
    if (wg.x == 2u) {
        // The running sum of the directory's stream lengths (a chunk per thread).
        let ns = p.nt * (3u + p.alpha);
        let per = (ns + 255u) / 256u;
        let s0 = min(li * per, ns);
        let s1 = min(s0 + per, ns);
        var acc = 0u;
        for (var i = s0; i < s1; i++) { acc += le32(p.dir_off + 4u + i * 4u); }
        wpart[li] = acc;
        workgroupBarrier();
        if (li == 0u) {
            var run = 0u;
            for (var i = 0u; i < 256u; i++) { let v = wpart[i]; wpart[i] = run; run += v; }
        }
        workgroupBarrier();
        var o = wpart[li];
        for (var i = s0; i < s1; i++) { tab[OFFS + i] = o; o += le32(p.dir_off + 4u + i * 4u); }
        return;
    }
    var off = p.coef_off;
    var T = 0u;
    var bits = 14u;
    var top = 2047u;
    if (wg.x == 1u) { off = p.pal_off; T = T_STRIDE; top = 15u; }
    if (wg.x >= 3u) {
        let k = wg.x - 3u;
        off = t3(k);
        T = (1u + k) * T_STRIDE;
        bits = p.ibits;
        top = 64u;
        if (k == 8u) { top = 255u; }
    }
    for (var s = li; s < 2048u; s += 256u) { tab[T + C_FREQ + s] = 0u; }
    storageBarrier();
    workgroupBarrier();
    let n = le16(off);
    for (var i = li; i < n; i += 256u) {
        let at = off + 2u + i * 4u;
        tab[T + C_FREQ + le16(at)] = le16(at + 2u);
    }
    storageBarrier();
    workgroupBarrier();
    var part = 0u;
    for (var j = 0u; j < 8u; j++) { part += tab[T + C_FREQ + li * 8u + j]; }
    wpart[li] = part;
    workgroupBarrier();
    if (li == 0u) {
        var acc = 0u;
        for (var i = 0u; i < 256u; i++) { let v = wpart[i]; wpart[i] = acc; acc += v; }
        wcum[2048] = acc;
    }
    workgroupBarrier();
    var acc = wpart[li];
    for (var j = 0u; j < 8u; j++) {
        let s = li * 8u + j;
        wcum[s] = acc;
        tab[T + C_CUM + s] = acc;
        acc += tab[T + C_FREQ + s];
    }
    if (li == 0u) { tab[T + C_CUM + 2048u] = wcum[2048]; }
    workgroupBarrier();
    for (var slot = li; slot < (1u << bits); slot += 256u) {
        var lo = 0u;
        var hi = top;
        while (lo < hi) {
            let mid = (lo + hi + 1u) >> 1u;
            if (wcum[mid] <= slot) { lo = mid; } else { hi = mid - 1u; }
        }
        if (wg.x >= 3u) {
            // v3 index / edge tables: one word, symbol | (frequency - 1) << 8 | bias << 20
            // (frequency 1..=4096 and bias below 4096: IDX_BITS <= 12).
            tab[T + C_SLOT + slot] = lo | ((tab[T + C_FREQ + lo] - 1u) << 8u) | ((slot - wcum[lo]) << 20u);
        } else {
            tab[T + C_SLOT + 2u * slot] = lo | (tab[T + C_FREQ + lo] << 11u);
            tab[T + C_SLOT + 2u * slot + 1u] = slot - wcum[lo];
        }
    }
}

// A stream's rANS decoder: read position, end, state, and the bitstream word last read (`wi`
// its index) — renormalisation reads bytes in order, so a word serves four of them.
struct Rans { pos: u32, end: u32, x: u32, w: u32, wi: u32 };

fn tile_byte(r: ptr<function, Rans>) -> u32 {
    let i = (*r).pos;
    (*r).pos = i + 1u;
    if (i >= (*r).end) { return 0u; }
    let wi = i >> 2u;
    if (wi != (*r).wi) {
        (*r).wi = wi;
        (*r).w = bits[wi];
    }
    return ((*r).w >> ((i & 3u) * 8u)) & 255u;
}

// One symbol of table `T` (precision `bits`).
fn sym_in(r: ptr<function, Rans>, T: u32, bits: u32) -> u32 {
    let slot = (*r).x & ((1u << bits) - 1u);
    let e = tab[T + C_SLOT + 2u * slot];
    let s = e & 2047u;
    (*r).x = (e >> 11u) * ((*r).x >> bits) + tab[T + C_SLOT + 2u * slot + 1u];
    for (var k = 0u; k < 4u; k++) {
        if ((*r).x >= 8388608u) { break; }
        (*r).x = ((*r).x << 8u) | tile_byte(r);
    }
    return s;
}
fn sym(r: ptr<function, Rans>) -> u32 { return sym_in(r, 0u, 14u); }
// One symbol of a v3 index or edge table (one-word slots, precision `p.ibits`).
fn sym3(r: ptr<function, Rans>, T: u32) -> u32 {
    let slot = (*r).x & ((1u << p.ibits) - 1u);
    let e = tab[T + C_SLOT + slot];
    (*r).x = (((e >> 8u) & 4095u) + 1u) * ((*r).x >> p.ibits) + (e >> 20u);
    for (var k = 0u; k < 4u; k++) {
        if ((*r).x >= 8388608u) { break; }
        (*r).x = ((*r).x << 8u) | tile_byte(r);
    }
    return e & 255u;
}

fn rans_at(start: u32, end: u32) -> Rans {
    var r: Rans;
    r.pos = start + 4u;
    r.end = end;
    r.wi = 0xffffffffu;
    var x = 0u;
    for (var i = 0u; i < 4u; i++) {
        var b = 0u;
        if (start + i < end) { b = byte_at(start + i); }
        x = x | (b << (8u * i));
    }
    r.x = x;
    return r;
}

// A v2 palette tile's serial part (`dxvkit_plus_core::lmbx::direct_tile`), from its stream 0
// (`start..end`): the palette index of every pixel into the tile's Y samples. The per-pixel
// expansion (colour, alpha, chroma) runs in parallel in the `idct` pass (`expand_block`).
fn palette_indices(t: u32, start: u32, end: u32) {
    let y0 = (t / p.tiles_x) * TILE * p.cw + (t % p.tiles_x) * TILE;
    let n = clamp(sb(start, end), 1u, 16u);
    var r = rans_at(start + 1u + 4u * n, end);
    var left = 0u;
    var col0 = 0u;
    for (var i = 0u; i < TILE * TILE; i++) {
        let x = i % TILE;
        var pred = left;
        if (x == 0u) { pred = col0; }
        let idx = (pred + sym_in(&r, T_STRIDE, 14u)) % n;
        left = idx;
        if (x == 0u) { col0 = idx; }
        planes[y0 + (i / TILE) * p.cw + x] = i32(idx);
    }
}

// A byte of a stream (0 at or past its end).
fn sb(i: u32, end: u32) -> u32 {
    if (i >= end) { return 0u; }
    return byte_at(i);
}

// A direct tile's row of palette indices above the current pixel, in workgroup memory: entry x of
// the workgroup's i-th active invocation at `x * RSTRIDE + i` (`ROW0` = i); updated in place as
// the row is decoded (1 KB: a larger buffer costs occupancy).
var<workgroup> rows: array<u32, 256>;
const RSTRIDE: u32 = 64u / DSPREAD;
var<private> ROW0: u32;

// v3 edge alpha of the pixel at Y index `at` whose colour is at `addr`: with partial alpha, its
// residual; the exact alpha (4 A) or -1 into A.
fn edge_px(r: ptr<function, Rans>, addr: u32, end: u32, at: u32) {
    let a = sb(addr + 3u, end);
    var ov = -1;
    if (a != 0u && a != 255u) { ov = i32((a + sym3(r, T_EDGE)) & 255u) * 4; }
    planes[p.cw * p.ch + 2u * p.chw * p.ch + at] = ov;
}

// v3 indices of a `size x size` region at Y index `y0` (`dxvkit_plus_core::lmbx::indices3`):
// each pixel's colour address (`col + 4 * index`) into Y; with `edge`, its edge alpha.
fn region3(r: ptr<function, Rans>, n: u32, size: u32, y0: u32, col: u32, edge: bool, end: u32) {
    var l = 0u;
    for (var y = 0u; y < size; y++) {
        // Sliding window over the row above: pa = above-left, a = above, nx = above-right; the
        // row is overwritten in place (rows[x] after pixel x is this row's).
        var pa = 0u;
        var a = 0u;
        var nx = 0u;
        if (y > 0u) {
            a = rows[ROW0];
            nx = rows[ROW0 + RSTRIDE];
        }
        for (var x = 0u; x < size; x++) {
            var aa = l;
            var ar = l;
            var al = l;
            if (y > 0u) {
                if (x > 0u) {
                    pa = a;
                    a = nx;
                    nx = a;
                    if (x + 1u < size) { nx = rows[ROW0 + (x + 1u) * RSTRIDE]; }
                }
                if (x == 0u) { l = a; }
                aa = a;
                ar = a;
                if (x + 1u < size) { ar = nx; }
                al = a;
                if (x > 0u) { al = pa; }
            } else if (x == 0u) {
                l = 0u;
                aa = 0u;
                ar = 0u;
                al = 0u;
            }
            let c = select(0u, 4u, l == aa) + select(0u, 2u, aa == ar) + select(0u, 1u, al == l);
            let s = sym3(r, (1u + c) * T_STRIDE);
            var v = (l + s - 1u) % n;
            if (s == 0u) { v = l; } else if (s == 1u) { v = aa; }
            rows[ROW0 + x * RSTRIDE] = v;
            l = v;
            let at = y0 + y * p.cw + x;
            planes[at] = i32(col + 4u * v);
            if (edge) { edge_px(r, col + 4u * v, end, at); }
        }
    }
}

// v3 palette and quad tiles' serial part (`dxvkit_plus_core::lmbx::direct_tile3`): colour
// addresses into Y, and with edge alpha the exact alpha (4 A, or -1) into A.
fn direct3(t: u32, start: u32, end: u32, cm: u32, am: u32) {
    let y0 = (t / p.tiles_x) * TILE * p.cw + (t % p.tiles_x) * TILE;
    let edge = am == 1u;
    if (cm == 2u) {
        let n = clamp(sb(start, end), 1u, 64u);
        var r = rans_at(start + 1u + 4u * n, end);
        region3(&r, n, TILE, y0, start + 1u, edge, end);
        return;
    }
    // Quad: per sub-tile, flat (colour address) or palette (colours' address, count).
    var rk: array<u32, 4>;
    var rc: array<u32, 4>;
    var rn: array<u32, 4>;
    var o = start;
    for (var q = 0u; q < 4u; q++) {
        if (sb(o, end) == 0u) {
            rk[q] = 0u;
            rc[q] = o + 1u;
            rn[q] = 1u;
            o += 5u;
        } else {
            let n = clamp(sb(o + 1u, end), 1u, 64u);
            rk[q] = 1u;
            rc[q] = o + 2u;
            rn[q] = n;
            o += 2u + 4u * n;
        }
    }
    var r = rans_at(o, end);
    for (var q = 0u; q < 4u; q++) {
        let origin = y0 + (q / 2u) * 16u * p.cw + (q % 2u) * 16u;
        if (rk[q] == 0u) {
            for (var i = 0u; i < 256u; i++) {
                let at = origin + (i / 16u) * p.cw + i % 16u;
                planes[at] = i32(rc[q]);
                if (edge) { edge_px(&r, rc[q], end, at); }
            }
        } else {
            region3(&r, rn[q], 16u, origin, rc[q], edge, end);
        }
    }
}

// v3 alpha palette of a DCT tile, from its A stream (`dxvkit_plus_core::lmbx::alpha_palette`):
// A = 4 * level (the indices as in `region3`).
fn alpha_pal3(t: u32, start: u32, end: u32) {
    let y0 = (t / p.tiles_x) * TILE * p.cw + (t % p.tiles_x) * TILE;
    let ab = p.cw * p.ch + 2u * p.chw * p.ch;
    let n = clamp(sb(start, end), 1u, 64u);
    var r = rans_at(start + 1u + n, end);
    var l = 0u;
    for (var y = 0u; y < TILE; y++) {
        var pa = 0u;
        var a = 0u;
        var nx = 0u;
        if (y > 0u) {
            a = rows[ROW0];
            nx = rows[ROW0 + RSTRIDE];
        }
        for (var x = 0u; x < TILE; x++) {
            var aa = l;
            var ar = l;
            var al = l;
            if (y > 0u) {
                if (x > 0u) {
                    pa = a;
                    a = nx;
                    nx = a;
                    if (x + 1u < TILE) { nx = rows[ROW0 + (x + 1u) * RSTRIDE]; }
                }
                if (x == 0u) { l = a; }
                aa = a;
                ar = a;
                if (x + 1u < TILE) { ar = nx; }
                al = a;
                if (x > 0u) { al = pa; }
            } else if (x == 0u) {
                l = 0u;
                aa = 0u;
                ar = 0u;
                al = 0u;
            }
            let c = select(0u, 4u, l == aa) + select(0u, 2u, aa == ar) + select(0u, 1u, al == l);
            let s = sym3(&r, (1u + c) * T_STRIDE);
            var v = (l + s - 1u) % n;
            if (s == 0u) { v = l; } else if (s == 1u) { v = aa; }
            rows[ROW0 + x * RSTRIDE] = v;
            l = v;
            planes[ab + y0 + y * p.cw + x] = i32(sb(start + 1u + v, end)) * 4;
        }
    }
}

// Stream `si`'s byte range in the bitstream.
fn stream_range(si: u32) -> vec2<u32> {
    let ns = p.nt * (3u + p.alpha);
    let dir = p.dir_off + 4u + si * 4u;
    var start = p.data_off + le32(dir);
    var end = p.plen;
    if (si + 1u < ns) { end = p.data_off + le32(dir + 4u); }
    if (p.v2 == 1u) {
        start = p.data_off + tab[OFFS + si];
        if (si + 1u < ns) { end = p.data_off + tab[OFFS + si + 1u]; } else { end = p.plen; }
    }
    return vec2<u32>(start, end);
}

// One 8x8 luma block (Y-plane index `o`, picture position `gx, gy`) of a direct tile: its
// pixels' RGBA8 packed into Y, `4 A` into A, and the forward YCoCg-R chroma (4:2:2: the rounded
// mean of each column pair, both columns in this block).
fn expand_block(t: u32, mode: u32, o: u32, gx: u32, gy: u32) {
    let sr = stream_range(t * (3u + p.alpha));
    let start = sr.x;
    let end = sr.y;
    let ysz = p.cw * p.ch;
    let csz = p.chw * p.ch;
    let cm = mode & 15u;
    let edge = (mode >> 4u) == 1u;
    for (var i = 0u; i < 8u; i++) {
        var pc = vec2<i32>(0, 0);
        for (var j = 0u; j < 8u; j++) {
            let at = o + i * p.cw + j;
            var c = start;
            if (p.v3 == 1u) {
                // v3: the entropy pass left each pixel's colour address (palette, quad).
                if (cm != 1u) { c = u32(planes[at]); }
            } else if (mode == 2u) { c = start + 1u + 4u * u32(planes[at]); }
            var v = 0u;
            if (c + 4u <= end) {
                // The four colour bytes from (at most) two aligned words.
                let sh = (c & 3u) * 8u;
                v = bits[c >> 2u] >> sh;
                if (sh != 0u) { v = v | (bits[(c >> 2u) + 1u] << (32u - sh)); }
            } else {
                for (var k = 0u; k < 4u; k++) { if (c + k < end) { v = v | (byte_at(c + k) << (8u * k)); } }
            }
            if (edge) {
                // Edge alpha: the entropy pass left the exact alpha (4 A) or -1.
                let ov = planes[ysz + 2u * csz + at];
                if (ov >= 0) { v = (v & 0xffffffu) | (u32(ov >> 2u) << 24u); }
            }
            if (p.alpha == 0u) { v = v | 0xff000000u; }
            planes[at] = i32(v);
            if (p.alpha == 1u) { planes[ysz + 2u * csz + at] = i32(v >> 24u) * 4; }
            let r = i32(v & 255u) * 4;
            let g = i32((v >> 8u) & 255u) * 4;
            let bl = i32((v >> 16u) & 255u) * 4;
            let co = r - bl;
            let cg = g - (bl + (co >> 1u));
            if (p.chroma == 0u) {
                let ci = (gy + i) * p.chw + gx + j;
                planes[ysz + ci] = co;
                planes[ysz + csz + ci] = cg;
            } else if ((j & 1u) == 0u) {
                pc = vec2<i32>(co, cg);
            } else {
                let ci = (gy + i) * p.chw + (gx + j) / 2u;
                planes[ysz + ci] = (pc.x + co + 1) >> 1u;
                planes[ysz + csz + ci] = (pc.y + cg + 1) >> 1u;
            }
        }
    }
}

// Decode one block's coefficients into raster positions of its 8x8 area (base index, stride).
// `dc`: the previous block's dequantised DC in this stream; the DC level is a residual from it.
fn block(r: ptr<function, Rans>, q: i32, qdc: i32, dc: ptr<function, i32>, base: u32, stride: u32) {
    for (var k = 1u; k < 64u; k++) { planes[base + (k >> 3u) * stride + (k & 7u)] = 0; }
    var d0 = *dc;
    for (var k = 0u; k < 64u; k++) {
        let s = sym(r);
        if (s == 2047u) { break; }
        let z = ZZ[k];
        if (k == 0u) {
            d0 = clamp(*dc + (i32(s) - 1023) * qdc, -4095, 4095);
        } else {
            planes[base + (z >> 3u) * stride + (z & 7u)] = clamp((i32(s) - 1023) * q, -4095, 4095);
        }
    }
    planes[base] = d0;
    *dc = d0;
}

fn chroma_q(q: u32) -> i32 { return i32(clamp((q * p.cratio + 128u) >> 8u, 2u, 4095u)); }

// Quantiser step of luma block `b` (raster in the 4x4 blocks) of tile `t`, from the raw map.
fn qstep(t: u32, b: u32) -> u32 {
    let v = byte_at(p.qmap_off + t * QMAP + (b >> 1u));
    return qt((v >> ((b & 1u) * 4u)) & 15u);
}

// The entropy pass, one dispatch: workgroups 0..ceil(n_dct * planes / 64) take one DCT stream per
// invocation (the DCT tiles of the tile list, `list_off`); the workgroups after them one direct
// tile each (`direct_tile`). No workgroup mixes the two, so a serial direct decode never holds
// DCT lanes.
@compute @workgroup_size(64)
fn entropy(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let planes_n = 3u + p.alpha;
    let dct_groups = (p.n_dct * planes_n + 63u) / 64u;
    if (wg.x >= dct_groups) {
        ROW0 = li / DSPREAD;
        if ((li & (DSPREAD - 1u)) != 0u) { return; }
        direct_tile((gid.x - dct_groups * 64u) / DSPREAD);
        return;
    }
    let e = gid.x / planes_n;
    if (e >= p.n_dct) { return; }
    let k = gid.x % planes_n;
    let t = tab[p.list_off + e];
    // v3: a DCT tile's alpha palette is the direct pass's.
    if (p.v3 == 1u && k == 3u && (tile_mode(t) >> 4u) == 2u) { return; }
    let tx = t % p.tiles_x;
    let ty = t / p.tiles_x;
    let sr = stream_range(t * planes_n + k);
    let start = sr.x;
    let end = sr.y;
    var r = rans_at(start, end);
    var dc = 0;
    let ysz = p.cw * p.ch;
    let csz = p.chw * p.ch;
    if (k == 0u || k == 3u) {
        var base0 = 0u;
        if (k == 3u) { base0 = ysz + 2u * csz; }
        for (var by = 0u; by < TB; by++) {
            for (var bx = 0u; bx < TB; bx++) {
                let q = i32(qstep(t, by * TB + bx));
                block(&r, q, max(q, 2), &dc, base0 + (ty * TILE + by * 8u) * p.cw + tx * TILE + bx * 8u, p.cw);
            }
        }
    } else {
        var ctw = TILE;
        if (p.chroma == 1u) { ctw = TILE / 2u; }
        let base0 = ysz + (k - 1u) * csz;
        for (var by = 0u; by < TB; by++) {
            for (var bx = 0u; bx < ctw / 8u; bx++) {
                var lbx = bx;
                if (p.chroma == 1u) { lbx = 2u * bx; }
                let q = chroma_q(qstep(t, by * TB + lbx));
                block(&r, q, max(q, 4), &dc, base0 + (ty * TILE + by * 8u) * p.chw + tx * ctw + bx * 8u, p.chw);
            }
        }
    }
}

// The direct tiles' serial part: one invocation per entry `n_dct..n_dct + n_dir` of the tile list
// (v2 palette tiles; v3 palette and quad tiles, and DCT tiles with an alpha palette).
fn direct_tile(i: u32) {
    if (i >= p.n_dir) { return; }
    let t = tab[p.list_off + p.n_dct + i];
    let mode = tile_mode(t);
    let s0 = t * (3u + p.alpha);
    if (p.v3 == 0u) {
        let sr = stream_range(s0);
        palette_indices(t, sr.x, sr.y);
        return;
    }
    let cm = mode & 15u;
    if (cm == 0u) {
        let sr = stream_range(s0 + 3u);
        alpha_pal3(t, sr.x, sr.y);
        return;
    }
    let sr = stream_range(s0);
    direct3(t, sr.x, sr.y, cm, mode >> 4u);
}

@compute @workgroup_size(64)
fn idct(@builtin(global_invocation_id) gid: vec3<u32>) {
    // Blocks: Y, then Co, Cg, then A (when coded), raster within each plane.
    var b = gid.x;
    if (b >= p.nblocks) { return; }
    let ny = (p.cw / 8u) * (p.ch / 8u);
    let nc = (p.chw / 8u) * (p.ch / 8u);
    var base = 0u;
    var stride = p.cw;
    var offset = 512;
    var is_a = false;
    if (b >= ny && b < ny + 2u * nc) {
        b = b - ny;
        stride = p.chw;
        offset = 0;
        base = p.cw * p.ch;
        if (b >= nc) { b = b - nc; base = base + p.chw * p.ch; }
    } else if (b >= ny) {
        b = b - ny - 2u * nc;
        base = p.cw * p.ch + 2u * p.chw * p.ch;
        is_a = true;
    }
    let bw = stride / 8u;
    // The block's tile (a chroma tile is TILE * chw / cw samples wide).
    let ctw = TILE * stride / p.cw;
    let t = ((b / bw) * 8u / TILE) * p.tiles_x + (b % bw) * 8u / ctw;
    let o = base + (b / bw) * 8u * stride + (b % bw) * 8u;
    let mode = tile_mode(t);
    if ((mode & 15u) != 0u) {
        // A direct tile: its Y-plane blocks expand the pixels (every plane); the rest do nothing.
        if (base == 0u) { expand_block(t, mode, o, (b % bw) * 8u, (b / bw) * 8u); }
        return;
    }
    // v3: a DCT tile's alpha palette is already in A.
    if (is_a && (mode >> 4u) == 2u) { return; }
    idct_block(o, stride, offset);
}
"#;

/// WGSL of the output pass: 4:2:2 halo / 4:4:4 chroma, inverse YCoCg-R, clamp to `0..=1020`,
/// 8-bit `(v + 2) >> 2`.
pub const LMBX_OUT_WGSL: &str = r#"
@group(0) @binding(2) var<storage, read> samples: array<i32>;

@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let q = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(q * 2.0 - 1.0, 0.0, 1.0);
}

@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = u32(pos.x);
    let y = u32(pos.y);
    let ysz = p.cw * p.ch;
    let csz = p.chw * p.ch;
    let yv = samples[y * p.cw + x];
    let mode = tile_mode((y / TILE) * p.tiles_x + x / TILE);
    if ((mode & 15u) != 0u) {
        let u = u32(yv);
        return vec4<f32>(f32(u & 255u), f32((u >> 8u) & 255u), f32((u >> 16u) & 255u), f32(u >> 24u)) / 255.0;
    }
    var co = 0;
    var cg = 0;
    if (p.chroma == 0u) {
        co = samples[ysz + y * p.chw + x];
        cg = samples[ysz + csz + y * p.chw + x];
    } else {
        let c = x >> 1u;
        co = samples[ysz + y * p.chw + c];
        cg = samples[ysz + csz + y * p.chw + c];
        if ((x & 1u) == 1u) {
            let n = min(c + 1u, p.chw - 1u);
            co = (co + samples[ysz + y * p.chw + n] + 1) >> 1u;
            cg = (cg + samples[ysz + csz + y * p.chw + n] + 1) >> 1u;
        }
    }
    let t = yv - (cg >> 1u);
    let g = cg + t;
    let bl = t - (co >> 1u);
    let r = bl + co;
    var a = 1020;
    if (p.alpha == 1u) { a = samples[ysz + 2u * csz + y * p.cw + x]; }
    let v = clamp(vec4<i32>(r, g, bl, a), vec4<i32>(0), vec4<i32>(1020));
    return vec4<f32>((v + vec4<i32>(2)) >> vec4<u32>(2u)) / 255.0;
}
"#;

/// The integer inverse DCT of one block, in place, as straight-line WGSL with the basis as
/// literals (`dxvkit_plus_core::lmbx::IDCT_MATRIX`) — no array indexing, so every value stays in
/// a register. The same sums, in the same order, as `dxvkit_plus_core::lmbx::idct`.
fn idct_wgsl() -> String {
    let m = |k: usize, n: usize| dxvkit_plus_core::lmbx::IDCT_MATRIX.get(k).and_then(|r| r.get(n)).copied().unwrap_or(0);
    let mut w = String::from("fn idct_block(o: u32, stride: u32, offset: i32) {\n");
    for k in 0..8 {
        for j in 0..8 {
            w.push_str(&format!("    let d{k}_{j} = planes[o + {k}u * stride + {j}u];\n"));
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            let terms: Vec<String> = (0..8).map(|k| format!("({}) * d{k}_{j}", m(k, i))).collect();
            w.push_str(&format!("    let t{i}_{j} = ({} + 512) >> 10u;\n", terms.join(" + ")));
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            let terms: Vec<String> = (0..8).map(|l| format!("t{i}_{l} * ({})", m(l, j))).collect();
            w.push_str(&format!("    planes[o + {i}u * stride + {j}u] = (({} + 2048) >> 12u) + offset;\n", terms.join(" + ")));
        }
    }
    w.push_str("}\n");
    w
}

fn geometry_wgsl() -> String {
    format!(
        "const TILE: u32 = {}u;\nconst TB: u32 = {}u;\nconst QMAP: u32 = {}u;\nconst DSPREAD: u32 = {DSPREAD}u;\n",
        dxvkit_plus_core::lmbx::TILE,
        dxvkit_plus_core::lmbx::TILE / 8,
        dxvkit_plus_core::lmbx::QMAP_BYTES
    )
}

/// The whole WGSL of the compute passes (the uniforms and every entry point).
fn compute_wgsl() -> String {
    format!("{PARAMS_WGSL}{}{LMBX_BINDINGS_WGSL}{PARAMS_FNS_WGSL}{LMBX_WGSL}{}", geometry_wgsl(), idct_wgsl())
}

/// The entropy pass runs one direct tile every `DSPREAD` lanes (the rest of the workgroup
/// idles): a direct tile's decode touches a different cache line per lane at almost every step, so
/// a SIMD group full of them serialises on memory. Measured (W8, 16 x 1080p): 1 lane in 8 is the
/// fastest of 1, 4, 8, 16, 32.
const DSPREAD: u32 = 8;

/// Uniforms (`P`).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    v: [u32; 24],
    qt: [u32; 16],
    t3: [u32; 12],
}

impl std::fmt::Debug for Params {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Params").field("v", &self.v).finish()
    }
}

/// Ten tables (coefficients; the v2 palette or the v3 index tables; the v3 edge-alpha table) of
/// `T_STRIDE` words; the stream offsets follow (one word per stream).
const TABLE_WORDS: u64 = 10 * (4097 + 2 * 16384);

fn storage(binding: u32, read_only: bool, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn uniform(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

/// What an upload needs to know about a frame (from its validated header and tile mode map).
#[derive(Clone, Debug)]
pub(crate) struct LmbxShape {
    params: Params,
    /// The tile list: the DCT tiles, then the tiles with serial direct work (grouped by mode).
    list: Vec<u32>,
    /// Bytes the bitstream occupies on the GPU (padded to whole words).
    pub(crate) bits_bytes: u64,
    planes_bytes: u64,
    nblocks: u32,
    streams: u32,
}

/// The GPU shape of an LMBX frame bitstream (parsed and validated on the CPU — headers only).
pub(crate) fn shape(blob: &[u8], width: u32, height: u32) -> Result<LmbxShape, GpuError> {
    let f = Frame::parse(blob, width, height).map_err(|e| GpuError::Invalid(e.to_string()))?;
    let h = f.header;
    let (cw, ch) = h.coded_size();
    let chw = h.chroma.plane_width(cw);
    let alpha = u32::from(h.alpha);
    let nblocks = (cw / 8) * (ch / 8) * (1 + alpha) + 2 * (chw / 8) * (ch / 8);
    let planes = u64::from(cw) * u64::from(ch) * u64::from(1 + alpha) + 2 * u64::from(chw) * u64::from(ch);
    let s = f.sections;
    let len = u32::try_from(blob.len()).map_err(|_| GpuError::Invalid("LMBX frame over 4 GiB".into()))?;
    let as32 = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    use dxvkit_plus_core::lmbx::{AlphaMode, TileMode, VERSION_3};
    let v3 = h.version >= VERSION_3;
    let mut list: Vec<u32> = (0..h.tile_count()).filter(|&t| f.mode(t) == TileMode::Dct).collect();
    let n_dct = list.len() as u32;
    let mut direct: Vec<(u8, u32)> = (0..h.tile_count())
        .filter_map(|t| {
            let (m, a) = (f.mode(t), f.alpha_mode(t));
            let work = matches!(m, TileMode::Palette | TileMode::Quad) || (v3 && m == TileMode::Dct && a == AlphaMode::Palette);
            work.then_some(((m as u8) | ((a as u8) << 4), t))
        })
        .collect();
    direct.sort_by_key(|d| d.0);
    let n_dir = direct.len() as u32;
    list.extend(direct.into_iter().map(|d| d.1));
    let v = [
        width,
        height,
        cw,
        ch,
        chw,
        h.tiles_x(),
        h.tile_count(),
        u32::from(h.chroma == Chroma::Halo422),
        alpha,
        u32::from(h.chroma_ratio),
        len,
        as32(s.coef_table),
        as32(s.qmap),
        as32(s.directory),
        as32(s.data),
        nblocks,
        u32::from(h.version >= dxvkit_plus_core::lmbx::VERSION_2),
        as32(s.modes),
        as32(s.palette_table),
        u32::from(h.version >= dxvkit_plus_core::lmbx::VERSION_3),
        dxvkit_plus_core::lmbx::IDX_BITS,
        n_dct,
        n_dir,
        (TABLE_WORDS as u32) + h.stream_count(),
    ];
    let mut t3 = [0u32; 12];
    for (d, &o) in t3.iter_mut().zip(&s.tables3) {
        *d = as32(o);
    }
    Ok(LmbxShape {
        params: Params { v, qt: h.qt.map(u32::from), t3 },
        list,
        bits_bytes: u64::from(len).div_ceil(4).max(1) * 4,
        planes_bytes: planes * 4,
        nblocks,
        streams: h.stream_count(),
    })
}

/// The LMBX decode pipelines and one frame's GPU buffers (grown as needed, reused across frames).
pub(crate) struct LmbxDecoder {
    compute_layout: wgpu::BindGroupLayout,
    tables: wgpu::ComputePipeline,
    entropy: wgpu::ComputePipeline,
    idct: wgpu::ComputePipeline,
    render_layout: wgpu::BindGroupLayout,
    render: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    table: Option<wgpu::Buffer>,
    pub(crate) bits: Option<wgpu::Buffer>,
    planes: Option<wgpu::Buffer>,
    /// The frame staged by the last upload.
    pending: Option<LmbxShape>,
}

impl LmbxDecoder {
    pub(crate) fn new(device: &wgpu::Device, output: wgpu::TextureFormat) -> LmbxDecoder {
        let cs = wgpu::ShaderStages::COMPUTE;
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dxvkit-plus-lmbx"),
            entries: &[uniform(0, cs), storage(1, true, cs), storage(2, false, cs), storage(3, false, cs)],
        });
        let fs = wgpu::ShaderStages::FRAGMENT;
        let render_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
            entries: &[uniform(0, fs), storage(1, true, fs), storage(2, true, fs)],
        });
        let module = |label: &str, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.to_owned().into()) })
        };
        let (cm, om) = (
            module("dxvkit-plus-lmbx", &compute_wgsl()),
            module("dxvkit-plus-lmbx-out", &format!("{PARAMS_WGSL}{}{LMBX_BINDINGS_WGSL}{PARAMS_FNS_WGSL}{LMBX_OUT_WGSL}", geometry_wgsl())),
        );
        let cpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dxvkit-plus-lmbx"),
            bind_group_layouts: &[Some(&compute_layout)],
            immediate_size: 0,
        });
        let compute = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&cpl),
                module: &cm,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (tables, entropy, idct) = (compute("tables"), compute("entropy"), compute("idct"));
        let rpl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
            bind_group_layouts: &[Some(&render_layout)],
            immediate_size: 0,
        });
        let render = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
            layout: Some(&rpl),
            vertex: wgpu::VertexState { module: &om, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &om,
                entry_point: Some("fs"),
                targets: &[Some(output.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dxvkit-plus-lmbx-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        LmbxDecoder { compute_layout, tables, entropy, idct, render_layout, render, params, table: None, bits: None, planes: None, pending: None }
    }

    /// Make sure the bitstream and plane buffers fit `s` (grown, never shrunk), and make `s` the
    /// pending frame.
    pub(crate) fn prepare(&mut self, device: &wgpu::Device, s: LmbxShape) {
        let small = |b: &Option<wgpu::Buffer>, need: u64| b.as_ref().is_none_or(|b| b.size() < need);
        let table_bytes = (TABLE_WORDS + u64::from(s.streams) + s.list.len() as u64) * 4;
        if small(&self.table, table_bytes) {
            self.table = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dxvkit-plus-lmbx-tables"),
                size: table_bytes.next_power_of_two(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if small(&self.bits, s.bits_bytes) {
            self.bits = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dxvkit-plus-lmbx-bits"),
                size: s.bits_bytes.next_power_of_two(),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if small(&self.planes, s.planes_bytes) {
            self.planes = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dxvkit-plus-lmbx-planes"),
                size: s.planes_bytes,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }));
        }
        self.pending = Some(s);
    }

    /// Upload the bitstream with `write_buffer` (the last word zero-padded).
    pub(crate) fn write(&self, queue: &wgpu::Queue, blob: &[u8]) -> Result<(), GpuError> {
        let bits = self.bits.as_ref().ok_or_else(|| GpuError::Invalid("LMBX buffers not prepared".into()))?;
        let whole = blob.len() / 4 * 4;
        if let Some(head) = blob.get(..whole)
            && !head.is_empty()
        {
            queue.write_buffer(bits, 0, head);
        }
        if whole < blob.len() {
            let mut tail = [0u8; 4];
            if let (Some(src), Some(dst)) = (blob.get(whole..), tail.get_mut(..blob.len() - whole)) {
                dst.copy_from_slice(src);
            }
            queue.write_buffer(bits, whole as u64, &tail);
        }
        Ok(())
    }

    /// Record the decode of the pending frame into `encoder`, rendering 8-bit RGBA into `target`.
    pub(crate) fn encode(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> Result<(), GpuError> {
        self.encode_with(device, queue, encoder, target, timestamp_writes, None)
    }

    /// [`LmbxDecoder::encode`] with every pass in its own pass, bracketed by timestamps `base..base + 8`
    /// of `set` (tables, entropy, idct, output) — profiling only.
    pub(crate) fn encode_profiled(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        set: &wgpu::QuerySet,
        base: u32,
    ) -> Result<(), GpuError> {
        let rw = wgpu::RenderPassTimestampWrites { query_set: set, beginning_of_pass_write_index: Some(base + 6), end_of_pass_write_index: Some(base + 7) };
        self.encode_with(device, queue, encoder, target, Some(rw), Some((set, base)))
    }

    fn encode_with(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        timestamp_writes: Option<wgpu::RenderPassTimestampWrites<'_>>,
        prof: Option<(&wgpu::QuerySet, u32)>,
    ) -> Result<(), GpuError> {
        let s = self.pending.as_ref().ok_or_else(|| GpuError::Invalid("LMBX decode before upload".into()))?;
        let (Some(bits), Some(planes), Some(table)) = (&self.bits, &self.planes, &self.table) else {
            return Err(GpuError::Invalid("LMBX buffers not prepared".into()));
        };
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&s.params));
        if !s.list.is_empty() {
            queue.write_buffer(table, (TABLE_WORDS + u64::from(s.streams)) * 4, bytemuck::cast_slice(&s.list));
        }
        let (n_dct, n_dir) = (s.params.v[21], s.params.v[22]);
        let planes_n = 3 + s.params.v[8];
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dxvkit-plus-lmbx"),
            layout: &self.compute_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: bits.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: table.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: planes.as_entire_binding() },
            ],
        });
        let passes: [(&str, &wgpu::ComputePipeline, u32); 3] = [
            ("lmbx-tables", &self.tables, 12),
            ("lmbx-entropy", &self.entropy, (n_dct * planes_n).div_ceil(64) + (n_dir * DSPREAD).div_ceil(64)),
            ("lmbx-idct", &self.idct, s.nblocks.div_ceil(64)),
        ];
        if let Some((set, base)) = prof {
            for (i, (label, pipe, n)) in (0u32..).zip(passes) {
                let tw = wgpu::ComputePassTimestampWrites {
                    query_set: set,
                    beginning_of_pass_write_index: Some(base + 2 * i),
                    end_of_pass_write_index: Some(base + 2 * i + 1),
                };
                let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(label), timestamp_writes: Some(tw) });
                cp.set_bind_group(0, &bind, &[]);
                cp.set_pipeline(pipe);
                if n > 0 {
                    cp.dispatch_workgroups(n, 1, 1);
                }
            }
        } else {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("dxvkit-plus-lmbx"), timestamp_writes: None });
            cp.set_bind_group(0, &bind, &[]);
            for (_, pipe, n) in passes.into_iter().filter(|p| p.2 > 0) {
                cp.set_pipeline(pipe);
                cp.dispatch_workgroups(n, 1, 1);
            }
        }
        let out_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
            layout: &self.render_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: bits.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: planes.as_entire_binding() },
            ],
        });
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
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
        rp.set_pipeline(&self.render);
        rp.set_bind_group(0, &out_bind, &[]);
        rp.draw(0..3, 0..1);
        Ok(())
    }
}

/// Per-pass GPU time of LMBX decode (profiling, not API): `layers` decoders each decode the
/// frames in turn, one submission per step, every step submitted back to back (the GPU never
/// idles, so it stays at its working clock); returns per step the summed GPU milliseconds of each
/// pass over the layers (tables, entropy, idct, output) and the step's first-to-last span, then a
/// final row whose last value is the whole run's first-to-last span divided by the steps.
#[doc(hidden)]
pub fn profile_passes(gpu: &crate::Gpu, frames: &[Vec<u8>], width: u32, height: u32, layers: u32, steps: u32) -> Result<Vec<[f64; 5]>, GpuError> {
    let (device, queue) = (&gpu.device, &gpu.queue);
    if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
        return Err(GpuError::Unsupported("TIMESTAMP_QUERY".into()));
    }
    let count = layers * 8;
    let size = u64::from(count) * 8;
    // One query set (at most 4096 timestamps): `steps` is capped to fit.
    let steps = steps.min(4096 / count.max(1)).max(1);
    let set = device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("lmbx-prof"), ty: wgpu::QueryType::Timestamp, count: count * steps });
    let resolve = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: size * u64::from(steps),
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: size * u64::from(steps),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut decs: Vec<LmbxDecoder> = (0..layers).map(|_| LmbxDecoder::new(device, crate::OUTPUT_FORMAT)).collect();
    let views: Vec<wgpu::TextureView> = (0..layers)
        .map(|_| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: crate::OUTPUT_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        })
        .collect();
    let period = f64::from(queue.get_timestamp_period()) / 1e6;
    for step in 0..steps {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("lmbx-prof") });
        for (l, (dec, view)) in (0u32..).zip(decs.iter_mut().zip(&views)) {
            let blob = frames.get(((step + l) as usize) % frames.len().max(1)).ok_or_else(|| GpuError::Invalid("no frames".into()))?;
            dec.prepare(device, shape(blob, width, height)?);
            dec.write(queue, blob)?;
            dec.encode_profiled(device, queue, &mut enc, view, &set, step * count + l * 8)?;
        }
        enc.resolve_query_set(&set, step * count..(step + 1) * count, &resolve, u64::from(step) * size);
        queue.submit([enc.finish()]);
    }
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("lmbx-prof-read") });
    enc.copy_buffer_to_buffer(&resolve, 0, &readback, 0, size * u64::from(steps));
    queue.submit([enc.finish()]);
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    crate::wait_idle(device)?;
    rx.recv_timeout(std::time::Duration::from_secs(30)).map_err(|e| GpuError::Readback(e.to_string()))?.map_err(|e| GpuError::Readback(e.to_string()))?;
    let ticks: Vec<u64> = {
        let m = slice.get_mapped_range().map_err(|e| GpuError::Readback(e.to_string()))?;
        m.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect()
    };
    readback.unmap();
    let mut out = Vec::new();
    for st in ticks.chunks_exact(count as usize) {
        let mut row = [0f64; 5];
        for l in st.chunks_exact(8) {
            for (k, slot) in row.iter_mut().take(4).enumerate() {
                let (a, b) = (l.get(2 * k).copied().unwrap_or(0), l.get(2 * k + 1).copied().unwrap_or(0));
                *slot += b.saturating_sub(a) as f64 * period;
            }
        }
        let first = st.iter().copied().filter(|&t| t > 0).min().unwrap_or(0);
        let last = st.iter().copied().max().unwrap_or(0);
        row[4] = last.saturating_sub(first) as f64 * period;
        out.push(row);
    }
    let first = ticks.iter().copied().filter(|&t| t > 0).min().unwrap_or(0);
    let last = ticks.iter().copied().max().unwrap_or(0);
    // GPU busy: the union of every pass's interval (idle gaps, e.g. waiting for the CPU, excluded).
    let mut iv: Vec<(u64, u64)> = ticks.chunks_exact(2).filter_map(|p| if let [a, b] = *p { (a > 0 && b >= a).then_some((a, b)) } else { None }).collect();
    iv.sort_unstable();
    let (mut busy, mut cur): (u64, Option<(u64, u64)>) = (0, None);
    for (a, b) in iv {
        cur = match cur {
            Some((c0, c1)) if a <= c1 => Some((c0, c1.max(b))),
            Some((c0, c1)) => {
                busy += c1 - c0;
                Some((a, b))
            }
            None => Some((a, b)),
        };
    }
    if let Some((c0, c1)) = cur {
        busy += c1 - c0;
    }
    let n = f64::from(steps.max(1));
    out.push([0.0, 0.0, 0.0, busy as f64 * period / n, last.saturating_sub(first) as f64 * period / n]);
    Ok(out)
}
