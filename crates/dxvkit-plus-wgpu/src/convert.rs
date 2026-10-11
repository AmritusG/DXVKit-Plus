//! Upload + WGSL conversion of decoded frames into straight-alpha RGBA8 at the clip size.

use crate::staging::{STAGING_ALIGNMENT, StagingLayout};
use crate::{GpuError, create_bc_texture, write_bc};
use dxvkit_plus_core::backend::{PixelLayout, YuvMatrix};
use dxvkit_plus_core::source::{Decoded, PixelFormat, Shading};

/// The converter's output format (straight alpha, not sRGB: values are stored as decoded).
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Decoded planes -> straight RGBA8 at the clip size, one texel per pixel.
///
/// Modes: 0 RGBA as stored (flag 1 = force opaque); 2 HAP Q scaled YCoCg (van Waveren & Castaño,
/// "Real-Time YCoCg-DXT Compression", 2007, as the HAP specification references; flag 2 = BC4
/// alpha in `t1`); 3 HAP alpha-only (grey); 4 HDR BC6H clamped to 0..1; 5 4:2:0 YCbCr from an
/// `R8` luma and an `RG8` chroma plane (BT.601 / 709 / 2020 matrix, video or full range).
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
    return c;
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

/// The conversion pipeline (shared by every clip on a device).
pub struct Converter {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let module = device
            .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("dxvkit-plus-convert"), source: wgpu::ShaderSource::Wgsl(CONVERT_WGSL.into()) });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dxvkit-plus-convert"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dxvkit-plus-convert"),
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
        });
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
        Converter { layout, pipeline, sampler, dummy }
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
    ) {
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dxvkit-plus-convert"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(t0) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(t1.unwrap_or(&self.dummy)) },
                wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
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
        rp.set_pipeline(&self.pipeline);
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
}

struct Upload {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    format: PixelFormat,
    width: u32,
    height: u32,
}

/// Renders one clip's frames: keeps its upload textures, uniform and output across frames.
pub struct FrameRenderer {
    converter: Converter,
    uploads: Vec<Upload>,
    uniform: wgpu::Buffer,
    last_uniform: Option<Uniform>,
    output: Option<(wgpu::Texture, wgpu::TextureView)>,
    pending: Option<Pending>,
    /// Bytes written with `write_texture` for the last frame (0 for native frames: zero-copy).
    pub last_upload_bytes: usize,
}

impl FrameRenderer {
    pub fn new(device: &wgpu::Device) -> FrameRenderer {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dxvkit-plus-convert-u"),
            size: std::mem::size_of::<Uniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        FrameRenderer { converter: Converter::new(device), uploads: Vec::new(), uniform, last_uniform: None, output: None, pending: None, last_upload_bytes: 0 }
    }

    /// The output texture (`OUTPUT_FORMAT`, the clip's size) after a render.
    pub fn output(&self) -> Option<&wgpu::Texture> {
        self.output.as_ref().map(|(t, _)| t)
    }

    fn ensure_output(&mut self, device: &wgpu::Device, w: u32, h: u32) -> Result<wgpu::TextureView, GpuError> {
        if w == 0 || h == 0 || w > crate::MAX_EDGE || h > crate::MAX_EDGE {
            return Err(GpuError::Invalid(format!("output size {w}x{h}")));
        }
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
        let (shading, width, height) = self.prepare_planes(device, decoded)?;
        let Decoded::Planes { planes, .. } = decoded else { return Err(GpuError::Invalid("not planes".into())) };
        let mut bytes = 0usize;
        for (up, p) in self.uploads.iter().zip(planes) {
            write_bc(queue, &up.texture, p.format, p.width, p.height, &p.data)?;
            bytes = bytes.saturating_add(p.data.len());
        }
        self.last_upload_bytes = bytes;
        self.pending = Some(Pending { shading, width, height, planes: planes.len() });
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
        let (shading, width, height) = self.prepare_planes(device, decoded)?;
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
        self.pending = Some(Pending { shading, width, height, planes: planes.len() });
        Ok(())
    }

    /// Check a frame's planes and make sure this renderer has a texture of the right format and
    /// size for each (reused across frames).
    fn prepare_planes(&mut self, device: &wgpu::Device, decoded: &Decoded) -> Result<(Shading, u32, u32), GpuError> {
        let Decoded::Planes { width, height, shading, planes, .. } = decoded else {
            return Err(GpuError::Invalid("native frames are rendered with render_native (after the platform import)".into()));
        };
        if planes.is_empty() || planes.len() > 2 {
            return Err(GpuError::Invalid(format!("{} planes", planes.len())));
        }
        for (i, p) in planes.iter().enumerate() {
            let reuse = self.uploads.get(i).is_some_and(|u| u.format == p.format && u.width == p.width && u.height == p.height);
            if !reuse {
                let texture = create_bc_texture(device, p.format, p.width, p.height, "dxvkit-plus-plane")?;
                let view = texture.create_view(&Default::default());
                let up = Upload { texture, view, format: p.format, width: p.width, height: p.height };
                if i < self.uploads.len() {
                    if let Some(slot) = self.uploads.get_mut(i) {
                        *slot = up;
                    }
                } else {
                    self.uploads.push(up);
                }
            }
        }
        Ok((*shading, *width, *height))
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
        let Pending { shading, width, height, planes } = self.pending.ok_or_else(|| GpuError::Invalid("encode_convert before upload".into()))?;
        let (mode, flags) = match shading {
            Shading::Rgba { opaque } => (0, u32::from(opaque)),
            Shading::HapYcocg { alpha } => (2, if alpha { 2 } else { 0 }),
            Shading::AlphaOnly => (3, 0),
            Shading::Hdr => (4, 0),
        };
        self.set_uniform(queue, Uniform { mode, flags, matrix: 1, full_range: 1, size: [width, height], pad: [0; 2] });
        let target = self.ensure_output(device, width, height)?;
        let t0 = &self.uploads.first().ok_or_else(|| GpuError::Invalid("no plane".into()))?.view;
        let t1 = if planes == 2 { self.uploads.get(1).map(|u| &u.view) } else { None };
        self.converter.pass(device, encoder, t0, t1, &self.uniform, &target, timestamps);
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
        self.set_uniform(queue, Uniform { mode: 0, flags: 0, matrix: 1, full_range: 1, size: [width, height], pad: [0; 2] });
        let target = self.ensure_output(device, width, height)?;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render-view") });
        self.converter.pass(device, &mut enc, view, None, &self.uniform, &target, None);
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
        self.last_upload_bytes = 0;
        self.set_uniform(queue, Uniform { mode, flags: 0, matrix: native.matrix.index(), full_range, size: [native.width, native.height], pad: [0; 2] });
        let target = self.ensure_output(device, native.width, native.height)?;
        let views: Vec<wgpu::TextureView> = native.textures.iter().map(|t| t.create_view(&Default::default())).collect();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-render-native") });
        let t0 = views.first().ok_or_else(|| GpuError::Invalid("no plane".into()))?;
        self.converter.pass(device, &mut enc, t0, views.get(1), &self.uniform, &target, None);
        queue.submit([enc.finish()]);
        self.output().ok_or_else(|| GpuError::Invalid("no output".into()))
    }
}
