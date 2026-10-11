//! # dxvkit-plus-wgpu — DXVKit Plus frames on wgpu 30
//!
//! - **Block uploads.** CPU-decoded block planes (HAP: BC1 / BC3 / BC4 / BC7 / BC6H) go straight
//!   into BC textures ([`create_bc_texture`] + [`write_bc`]): no CPU pixel decode on the playback
//!   path. RGBA8 images upload as RGBA8.
//! - **Conversion** ([`Converter`], WGSL [`CONVERT_WGSL`]) into a straight-alpha `Rgba8Unorm`
//!   texture at the clip's own size, texel for texel: RGBA as stored (HAP "RGB" forced opaque), HAP
//!   Q scaled YCoCg (+ BC4 alpha), HAP alpha-only, HDR BC6H (clamped), 4:2:0 YCbCr (BT.601 / 709 /
//!   2020, video or full range) for native frames imported by a platform crate.
//! - [`FrameRenderer`] keeps the upload textures and the output across frames (a steady clip
//!   reallocates nothing).
//! - **LMBD** ([`LmbdTexture`]): an LMB7 frame's BC7 blocks upload as they are and the GPU decodes
//!   them in hardware when it samples — bit-exact with the CPU decoder; an sRGB view gives linear
//!   light for files whose colorspace is sRGB.
//!
//! Nothing here panics on bad input or a missing GPU feature; failures are [`GpuError`]s.
#![forbid(unsafe_code)]

mod convert;
mod lmbd;

pub use convert::{CONVERT_WGSL, Converter, FrameRenderer, NativePlanes, OUTPUT_FORMAT};
pub use lmbd::LmbdTexture;

use dxvkit_plus_core::source::PixelFormat;
use std::fmt;

/// Largest texture edge accepted.
pub const MAX_EDGE: u32 = 16_384;

/// A GPU failure. Nothing in this crate panics on bad input or a missing device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuError {
    /// No suitable adapter (e.g. a machine without a GPU).
    NoAdapter(String),
    Device(String),
    Invalid(String),
    /// A feature this device or build does not have.
    Unsupported(String),
    /// A GPU read-back failed or timed out.
    Readback(String),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::NoAdapter(m) => write!(f, "no GPU adapter: {m}"),
            GpuError::Device(m) => write!(f, "device: {m}"),
            GpuError::Invalid(m) => write!(f, "invalid: {m}"),
            GpuError::Unsupported(m) => write!(f, "unsupported: {m}"),
            GpuError::Readback(m) => write!(f, "readback: {m}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// One device + queue.
#[derive(Clone, Debug)]
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// A device with no surface, on the high-performance adapter. Requests
    /// `TEXTURE_COMPRESSION_BC` (always present on macOS and Windows desktop) when the adapter has it.
    pub fn headless() -> Result<Gpu, GpuError> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() }),
        )
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        Self::with_adapter(instance, adapter)
    }

    /// A device on a chosen adapter.
    pub fn with_adapter(instance: wgpu::Instance, adapter: wgpu::Adapter) -> Result<Gpu, GpuError> {
        let required_features = adapter.features() & wgpu::Features::TEXTURE_COMPRESSION_BC;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("dxvkit-plus"), required_features, ..Default::default() }))
                .map_err(|e| GpuError::Device(e.to_string()))?;
        // wgpu's default handler panics on an uncaptured error; log instead (never-crash rule).
        device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| eprintln!("dxvkit-plus-wgpu: uncaptured wgpu error: {e}")));
        Ok(Gpu { instance, adapter, device, queue })
    }

    /// Whether BC upload is available.
    pub fn has_bc(&self) -> bool {
        self.device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    }
}

/// The wgpu texture format of a core pixel format.
pub fn texture_format(f: PixelFormat) -> wgpu::TextureFormat {
    match f {
        PixelFormat::Bc1 => wgpu::TextureFormat::Bc1RgbaUnorm,
        PixelFormat::Bc3 => wgpu::TextureFormat::Bc3RgbaUnorm,
        PixelFormat::Bc4 => wgpu::TextureFormat::Bc4RUnorm,
        PixelFormat::Bc5 => wgpu::TextureFormat::Bc5RgUnorm,
        PixelFormat::Bc6hUfloat => wgpu::TextureFormat::Bc6hRgbUfloat,
        PixelFormat::Bc6hSfloat => wgpu::TextureFormat::Bc6hRgbFloat,
        PixelFormat::Bc7 => wgpu::TextureFormat::Bc7RgbaUnorm,
        PixelFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
    }
}

/// Exact byte size of one frame of `format` (`width`/`height` rounded up to whole 4x4 blocks for BC).
pub fn frame_bytes(format: PixelFormat, width: u32, height: u32) -> Option<usize> {
    let (units_x, units_y) = if format.is_compressed() { (width.checked_add(3)? / 4, height.checked_add(3)? / 4) } else { (width, height) };
    usize::try_from(u64::from(units_x).checked_mul(u64::from(units_y))?.checked_mul(u64::from(format.unit_bytes()))?).ok()
}

fn check_size(format: PixelFormat, width: u32, height: u32) -> Result<usize, GpuError> {
    let aligned = !format.is_compressed() || (width.is_multiple_of(4) && height.is_multiple_of(4));
    if width == 0 || height == 0 || !aligned || width > MAX_EDGE || height > MAX_EDGE {
        return Err(GpuError::Invalid(format!("{format:?} frame {width}x{height} must be non-zero (multiples of 4 for BC) up to {MAX_EDGE}")));
    }
    frame_bytes(format, width, height).ok_or_else(|| GpuError::Invalid("frame size overflows".into()))
}

/// Create a reusable texture (`TEXTURE_BINDING | COPY_DST`) for codec frames of `format` (BC sizes
/// must be multiples of 4). Refill it each frame with [`write_bc`].
pub fn create_bc_texture(device: &wgpu::Device, format: PixelFormat, width: u32, height: u32, label: &str) -> Result<wgpu::Texture, GpuError> {
    create_bc_texture_with_views(device, format, width, height, label, &[])
}

/// [`create_bc_texture`] with extra view formats (for example the sRGB variant of the format, to
/// sample linear light).
pub fn create_bc_texture_with_views(
    device: &wgpu::Device,
    format: PixelFormat,
    width: u32,
    height: u32,
    label: &str,
    view_formats: &[wgpu::TextureFormat],
) -> Result<wgpu::Texture, GpuError> {
    if format.is_compressed() && !device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC) {
        return Err(GpuError::Unsupported("TEXTURE_COMPRESSION_BC is not available on this adapter".into()));
    }
    check_size(format, width, height)?;
    Ok(device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format(format),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats,
    }))
}

/// Upload one frame (exactly [`frame_bytes`]) into a texture made by [`create_bc_texture`] with the
/// same format and size.
pub fn write_bc(queue: &wgpu::Queue, texture: &wgpu::Texture, format: PixelFormat, width: u32, height: u32, data: &[u8]) -> Result<(), GpuError> {
    let want = check_size(format, width, height)?;
    if texture.format() != texture_format(format) || texture.width() != width || texture.height() != height {
        return Err(GpuError::Invalid(format!(
            "{format:?} {width}x{height} does not match the texture ({:?} {}x{})",
            texture.format(),
            texture.width(),
            texture.height()
        )));
    }
    if data.len() != want {
        return Err(GpuError::Invalid(format!("{format:?} {width}x{height} needs {want} bytes, got {}", data.len())));
    }
    let (per_row, rows) =
        if format.is_compressed() { ((width / 4).saturating_mul(format.unit_bytes()), height / 4) } else { (width.saturating_mul(4), height) };
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(per_row), rows_per_image: Some(rows) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    Ok(())
}

/// Read an `Rgba8Unorm` / `Bgra8Unorm` texture back as tight RGBA8 (BGRA is swizzled). Waits for
/// the GPU (bounded).
pub fn read_texture_rgba(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Result<Vec<u8>, GpuError> {
    let bgra = match texture.format() {
        wgpu::TextureFormat::Rgba8Unorm => false,
        wgpu::TextureFormat::Bgra8Unorm => true,
        other => return Err(GpuError::Unsupported(format!("read-back of {other:?}"))),
    };
    let (w, h) = (texture.width(), texture.height());
    let tight = w.saturating_mul(4);
    let padded = tight.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT).saturating_mul(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dxvkit-plus-readback"),
        size: u64::from(padded).saturating_mul(u64::from(h)),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dxvkit-plus-readback") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) } },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(10)) })
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    rx.recv_timeout(std::time::Duration::from_secs(10)).map_err(|e| GpuError::Readback(e.to_string()))?.map_err(|e| GpuError::Readback(e.to_string()))?;
    let mapped = slice.get_mapped_range().map_err(|e| GpuError::Readback(e.to_string()))?;
    let mut out = Vec::with_capacity((tight as usize).saturating_mul(h as usize));
    for row in mapped.chunks(padded as usize).take(h as usize) {
        let row = row.get(..tight as usize).unwrap_or_default();
        if bgra {
            for p in row.chunks_exact(4) {
                if let [b, g, r, a] = *p {
                    out.extend_from_slice(&[r, g, b, a]);
                }
            }
        } else {
            out.extend_from_slice(row);
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}

/// Wait until the GPU has finished all submitted work (benchmarks, tests).
pub fn wait_idle(device: &wgpu::Device) -> Result<(), GpuError> {
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(10)) })
        .map(|_| ())
        .map_err(|e| GpuError::Device(e.to_string()))
}
