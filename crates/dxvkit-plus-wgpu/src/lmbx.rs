//! LMBX on the GPU: the frame bitstream is uploaded as it is (nothing is decoded on the CPU) and
//! decoded by compute passes, bit-exact with `dxvkit_plus_core::lmbx`:
//!
//! 1. `tables` (one workgroup): expand the sparse rANS table in the bitstream into frequency,
//!    cumulative-frequency and slot-to-symbol arrays;
//! 2. `entropy` (one invocation per stream — each plane of each 32x32 tile): rANS-decode it and
//!    write each block's dequantised coefficients into the block's own 8x8 area of its plane;
//! 3. `idct` (one invocation per 8x8 block): the integer inverse DCT, in place (straight-line
//!    code generated from the basis);
//! 4. a fragment pass ([`LMBX_OUT_WGSL`]): chroma (4:2:2 halo or 4:4:4), inverse YCoCg-R, clamp,
//!    8-bit output.
//!
//! The planes are `i32` storage at the coded size (whole tiles): `4 * (2 + chroma planes) *
//! coded pixels` bytes per renderer (1080p 4:2:2 with alpha: 26 MB).

use crate::GpuError;
use dxvkit_plus_core::lmbx::{Chroma, Frame};

/// The uniforms both shaders share.
const PARAMS_WGSL: &str = r#"
struct P {
    w: u32, h: u32, cw: u32, ch: u32,
    chw: u32, tiles_x: u32, nt: u32, chroma: u32,
    alpha: u32, cratio: u32, plen: u32, coef_off: u32,
    qmap_off: u32, dir_off: u32, data_off: u32, nblocks: u32,
    qt: array<vec4<u32>, 4>,
};
"#;

/// WGSL of the compute passes. The arithmetic is `dxvkit_plus_core::lmbx`'s, operation for
/// operation (i32 / u32, arithmetic shifts, wrapping rANS state).
pub const LMBX_WGSL: &str = r#"
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> bits: array<u32>;
@group(0) @binding(2) var<storage, read_write> tab: array<u32>;
@group(0) @binding(3) var<storage, read_write> planes: array<i32>;

// tab layout: dense frequencies (2048), cumulative frequencies (2049), then per slot (16384)
// two words: the owning symbol | frequency << 11, and slot - cum[symbol] — one read per symbol.
const C_FREQ: u32 = 0u;
const C_CUM: u32 = 2048u;
const C_SLOT: u32 = 4097u;

var<private> ZZ: array<u32, 64> = array<u32, 64>(
    0u, 1u, 8u, 16u, 9u, 2u, 3u, 10u, 17u, 24u, 32u, 25u, 18u, 11u, 4u, 5u, 12u, 19u, 26u, 33u, 40u, 48u, 41u, 34u, 27u, 20u, 13u, 6u, 7u, 14u, 21u, 28u,
    35u, 42u, 49u, 56u, 57u, 50u, 43u, 36u, 29u, 22u, 15u, 23u, 30u, 37u, 44u, 51u, 58u, 59u, 52u, 45u, 38u, 31u, 39u, 46u, 53u, 60u, 61u, 54u, 47u, 55u, 62u, 63u);

fn byte_at(i: u32) -> u32 {
    if (i >= p.plen) { return 0u; }
    return (bits[i >> 2u] >> ((i & 3u) * 8u)) & 255u;
}
fn le16(i: u32) -> u32 { return byte_at(i) | (byte_at(i + 1u) << 8u); }
fn le32(i: u32) -> u32 { return le16(i) | (le16(i + 2u) << 16u); }
fn qt(i: u32) -> u32 { return p.qt[i >> 2u][i & 3u]; }

var<workgroup> wcum: array<u32, 2049>;
var<workgroup> wpart: array<u32, 256>;

// Expand the sparse table into freq, cum and slot-to-symbol, all 256 threads: scatter the
// frequencies, prefix-sum them (8 symbols per thread, then the 256 partial sums), and give every
// slot the largest symbol s with cum[s] <= slot (binary search in workgroup memory).
@compute @workgroup_size(256)
fn tables(@builtin(local_invocation_index) li: u32) {
    for (var s = li; s < 2048u; s += 256u) { tab[C_FREQ + s] = 0u; }
    storageBarrier();
    workgroupBarrier();
    let n = le16(p.coef_off);
    for (var i = li; i < n; i += 256u) {
        let at = p.coef_off + 2u + i * 4u;
        tab[C_FREQ + le16(at)] = le16(at + 2u);
    }
    storageBarrier();
    workgroupBarrier();
    var part = 0u;
    for (var j = 0u; j < 8u; j++) { part += tab[C_FREQ + li * 8u + j]; }
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
        tab[C_CUM + s] = acc;
        acc += tab[C_FREQ + s];
    }
    if (li == 0u) { tab[C_CUM + 2048u] = wcum[2048]; }
    workgroupBarrier();
    for (var slot = li; slot < 16384u; slot += 256u) {
        var lo = 0u;
        var hi = 2047u;
        while (lo < hi) {
            let mid = (lo + hi + 1u) >> 1u;
            if (wcum[mid] <= slot) { lo = mid; } else { hi = mid - 1u; }
        }
        tab[C_SLOT + 2u * slot] = lo | (tab[C_FREQ + lo] << 11u);
        tab[C_SLOT + 2u * slot + 1u] = slot - wcum[lo];
    }
}

struct Rans { pos: u32, end: u32, x: u32 };

fn tile_byte(r: ptr<function, Rans>) -> u32 {
    let i = (*r).pos;
    (*r).pos = i + 1u;
    if (i >= (*r).end) { return 0u; }
    return byte_at(i);
}

fn sym(r: ptr<function, Rans>) -> u32 {
    let slot = (*r).x & 16383u;
    let e = tab[C_SLOT + 2u * slot];
    let s = e & 2047u;
    (*r).x = (e >> 11u) * ((*r).x >> 14u) + tab[C_SLOT + 2u * slot + 1u];
    for (var k = 0u; k < 4u; k++) {
        if ((*r).x >= 8388608u) { break; }
        (*r).x = ((*r).x << 8u) | tile_byte(r);
    }
    return s;
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

@compute @workgroup_size(64)
fn entropy(@builtin(global_invocation_id) gid: vec3<u32>) {
    let planes_n = 3u + p.alpha;
    let si = gid.x;
    if (si >= p.nt * planes_n) { return; }
    let t = si / planes_n;
    let k = si % planes_n;
    let tx = t % p.tiles_x;
    let ty = t / p.tiles_x;
    let dir = p.dir_off + 4u + si * 4u;
    let start = p.data_off + le32(dir);
    var end = p.plen;
    if (si + 1u < p.nt * planes_n) { end = p.data_off + le32(dir + 4u); }
    var r: Rans;
    r.pos = start + 4u;
    r.end = end;
    var x = 0u;
    for (var i = 0u; i < 4u; i++) {
        var b = 0u;
        if (start + i < end) { b = byte_at(start + i); }
        x = x | (b << (8u * i));
    }
    r.x = x;
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
    if (b >= ny && b < ny + 2u * nc) {
        b = b - ny;
        stride = p.chw;
        offset = 0;
        base = p.cw * p.ch;
        if (b >= nc) { b = b - nc; base = base + p.chw * p.ch; }
    } else if (b >= ny) {
        b = b - ny - 2u * nc;
        base = p.cw * p.ch + 2u * p.chw * p.ch;
    }
    let bw = stride / 8u;
    let o = base + (b / bw) * 8u * stride + (b % bw) * 8u;
    idct_block(o, stride, offset);
}
"#;

/// WGSL of the output pass: 4:2:2 halo / 4:4:4 chroma, inverse YCoCg-R, clamp to `0..=1020`,
/// 8-bit `(v + 2) >> 2`.
pub const LMBX_OUT_WGSL: &str = r#"
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> samples: array<i32>;

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
    let m = dxvkit_plus_core::lmbx::IDCT_MATRIX;
    let mut w = String::from("fn idct_block(o: u32, stride: u32, offset: i32) {\n");
    for k in 0..8 {
        for j in 0..8 {
            w.push_str(&format!("    let d{k}_{j} = planes[o + {k}u * stride + {j}u];\n"));
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            let terms: Vec<String> = (0..8).map(|k| format!("({}) * d{k}_{j}", m[k][i])).collect();
            w.push_str(&format!("    let t{i}_{j} = ({} + 512) >> 10u;\n", terms.join(" + ")));
        }
    }
    for i in 0..8 {
        for j in 0..8 {
            let terms: Vec<String> = (0..8).map(|l| format!("t{i}_{l} * ({})", m[l][j])).collect();
            w.push_str(&format!("    planes[o + {i}u * stride + {j}u] = (({} + 2048) >> 12u) + offset;\n", terms.join(" + ")));
        }
    }
    w.push_str("}\n");
    w
}

/// Uniforms (`P`).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    v: [u32; 16],
    qt: [u32; 16],
}

impl std::fmt::Debug for Params {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Params").field("v", &self.v).finish()
    }
}

const TABLE_WORDS: u64 = 4097 + 2 * 16384;

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

/// What an upload needs to know about a frame (from its validated header).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LmbxShape {
    params: Params,
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
    ];
    Ok(LmbxShape {
        params: Params { v, qt: h.qt.map(u32::from) },
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
    table: wgpu::Buffer,
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
            entries: &[uniform(0, fs), storage(1, true, fs)],
        });
        let module = |label: &str, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(format!("{PARAMS_WGSL}{src}").into()),
            })
        };
        let geometry = format!(
            "const TILE: u32 = {}u;\nconst TB: u32 = {}u;\nconst QMAP: u32 = {}u;\n",
            dxvkit_plus_core::lmbx::TILE,
            dxvkit_plus_core::lmbx::TILE / 8,
            dxvkit_plus_core::lmbx::QMAP_BYTES
        );
        let (cm, om) = (module("dxvkit-plus-lmbx", &format!("{geometry}{LMBX_WGSL}{}", idct_wgsl())), module("dxvkit-plus-lmbx-out", LMBX_OUT_WGSL));
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
        let table = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dxvkit-plus-lmbx-tables"),
            size: TABLE_WORDS * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        LmbxDecoder { compute_layout, tables, entropy, idct, render_layout, render, params, table, bits: None, planes: None, pending: None }
    }

    /// Make sure the bitstream and plane buffers fit `s` (grown, never shrunk), and make `s` the
    /// pending frame.
    pub(crate) fn prepare(&mut self, device: &wgpu::Device, s: LmbxShape) {
        let small = |b: &Option<wgpu::Buffer>, need: u64| b.as_ref().is_none_or(|b| b.size() < need);
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
        let s = self.pending.ok_or_else(|| GpuError::Invalid("LMBX decode before upload".into()))?;
        let (Some(bits), Some(planes)) = (&self.bits, &self.planes) else {
            return Err(GpuError::Invalid("LMBX buffers not prepared".into()));
        };
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&s.params));
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dxvkit-plus-lmbx"),
            layout: &self.compute_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: bits.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: self.table.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: planes.as_entire_binding() },
            ],
        });
        {
            let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("dxvkit-plus-lmbx"), timestamp_writes: None });
            cp.set_bind_group(0, &bind, &[]);
            cp.set_pipeline(&self.tables);
            cp.dispatch_workgroups(1, 1, 1);
            cp.set_pipeline(&self.entropy);
            cp.dispatch_workgroups(s.streams.div_ceil(64), 1, 1);
            cp.set_pipeline(&self.idct);
            cp.dispatch_workgroups(s.nblocks.div_ceil(64), 1, 1);
        }
        let out_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dxvkit-plus-lmbx-out"),
            layout: &self.render_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: planes.as_entire_binding() },
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
