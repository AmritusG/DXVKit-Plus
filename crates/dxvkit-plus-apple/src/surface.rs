//! A decoded frame's IOSurface planes as wgpu textures, zero-copy.
//!
//! Two routes, both zero-copy:
//!
//! 1. [`import`]: `newTextureWithDescriptor:iosurface:plane:` creates an
//!    `MTLTexture` over the surface's memory, `wgpu_hal::metal::Device::texture_from_raw` adopts it
//!    and `wgpu::Device::create_texture_from_hal` registers it. wgpu marks such textures initialised
//!    (no lazy clear) and the Metal hal emits no barriers, so the decoder's pixels are sampled where
//!    VideoToolbox wrote them. The drop callback holds the `CVPixelBuffer`, so the pool cannot
//!    recycle the surface while the GPU may still read it.
//! 2. [`TextureCacheImporter`] (the default): `CVMetalTextureCacheCreateTextureFromImage` maps each plane through
//!    a long-lived `CVMetalTextureCache` (Apple's documented route, and the one Cutlass's
//!    `metal_import.rs` uses — MIT OR Apache-2.0; we read it for the technique and wrote ours from
//!    Apple's CoreVideo documentation). The drop callback holds the `CVMetalTexture` and the pixel
//!    buffer.
//!
//! `dxvkit-plus bench --import` measures both on the same frames; `docs/ARCHITECTURE.md` records
//! the numbers: the cache route had the lower p50 (by ~0.2 ms) with equal p99, so it is the
//! default. [`import`] needs no per-device object and stays available.

use crate::decode::pixel_buffer;
use dxvkit_plus_core::MediaError;
use dxvkit_plus_core::backend::{NativeFrame, PixelLayout};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBufferGetIOSurface, kCVReturnSuccess};
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
use std::ptr::NonNull;

/// One imported plane.
pub struct Plane {
    pub texture: wgpu::Texture,
    pub format: wgpu::TextureFormat,
    pub width: u32,
    pub height: u32,
    /// The IOSurface the texture aliases (same id as the frame's `surface_id`).
    pub surface_id: u32,
}

/// The frame's planes on `device` (`Bgra`: one BGRA8 plane; `Yuv420`: R8 luma + RG8 chroma).
/// `device` must be a Metal device.
pub fn import(device: &wgpu::Device, frame: &NativeFrame) -> Result<Vec<Plane>, MediaError> {
    let buffer = pixel_buffer(frame)?;
    let surface = CVPixelBufferGetIOSurface(Some(&buffer.0)).ok_or_else(|| MediaError::Platform("pixel buffer has no IOSurface".into()))?;
    let planes = plane_specs(frame.layout);
    let plane_count = surface.plane_count();
    let mut out = Vec::with_capacity(planes.len());
    for (index, format, mtl_format) in planes {
        // A non-planar surface reports 0 planes; its single image is plane 0 at the full size.
        let (w, h) = if plane_count == 0 {
            (surface.width(), surface.height())
        } else {
            if index >= plane_count {
                return Err(MediaError::Platform(format!("surface has {plane_count} planes, plane {index} requested")));
            }
            (surface.width_of_plane(index), surface.height_of_plane(index))
        };
        let width = u32::try_from(w).map_err(|_| MediaError::Invalid("plane width".into()))?;
        let height = u32::try_from(h).map_err(|_| MediaError::Invalid("plane height".into()))?;
        if width == 0 || height == 0 {
            return Err(MediaError::Platform(format!("plane {index} is empty")));
        }
        let raw = metal_texture(device, &surface, index, mtl_format, w, h)?;
        let keep_alive = buffer.clone();
        let texture = wrap(device, raw, format, width, height, Box::new(move || drop(keep_alive)));
        out.push(Plane { texture, format, width, height, surface_id: surface.id() });
    }
    Ok(out)
}

fn metal_texture(
    device: &wgpu::Device,
    surface: &IOSurfaceRef,
    plane: usize,
    format: MTLPixelFormat,
    w: usize,
    h: usize,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, MediaError> {
    // SAFETY: we only read the raw MTLDevice from the hal guard while it is alive.
    let hal = unsafe { device.as_hal::<wgpu_hal::api::Metal>() }.ok_or_else(|| MediaError::Unsupported("the wgpu device is not a Metal device".into()))?;
    let raw_device = hal.raw_device();
    // SAFETY: plain descriptor construction with non-zero dimensions checked by the caller.
    let desc = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(format, w, h, false) };
    desc.setStorageMode(MTLStorageMode::Shared);
    desc.setUsage(MTLTextureUsage::ShaderRead);
    raw_device
        .newTextureWithDescriptor_iosurface_plane(&desc, surface, plane)
        .ok_or_else(|| MediaError::Platform(format!("newTextureWithDescriptor:iosurface:plane:{plane} returned nil")))
}

/// (IOSurface plane, wgpu format, Metal format) per plane of a layout.
fn plane_specs(layout: PixelLayout) -> Vec<(usize, wgpu::TextureFormat, MTLPixelFormat)> {
    match layout {
        PixelLayout::Bgra => vec![(0, wgpu::TextureFormat::Bgra8Unorm, MTLPixelFormat::BGRA8Unorm)],
        PixelLayout::Yuv420 { .. } => {
            vec![(0, wgpu::TextureFormat::R8Unorm, MTLPixelFormat::R8Unorm), (1, wgpu::TextureFormat::Rg8Unorm, MTLPixelFormat::RG8Unorm)]
        }
    }
}

fn wrap(
    device: &wgpu::Device,
    raw: Retained<ProtocolObject<dyn MTLTexture>>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    keep_alive: Box<dyn FnOnce() + Send + Sync>,
) -> wgpu::Texture {
    let desc = wgpu::TextureDescriptor {
        label: Some("native-plane"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // SAFETY: `raw` was created on this device's MTLDevice with exactly `desc`'s format, size, type,
    // mip and layer count; the drop callback keeps the frame's memory (the CVPixelBuffer, and for the
    // cache route the CVMetalTexture) alive as long as wgpu holds the texture.
    let hal = unsafe {
        wgpu_hal::metal::Device::texture_from_raw(raw, format, MTLTextureType::Type2D, 1, 1, wgpu_hal::CopyExtent { width, height, depth: 1 }, Some(keep_alive))
    };
    // SAFETY: `hal` is a Metal texture of this device respecting `desc`; its memory is initialised
    // (the decoder wrote the frame before handing the buffer out). On Metal `initial_state` only
    // seeds wgpu's tracker — the hal emits no barriers.
    unsafe { device.create_texture_from_hal::<wgpu_hal::api::Metal>(hal, &desc, wgpu::TextureUses::RESOURCE) }
}

/// A retained `CVMetalTexture` that may travel with the wgpu texture's drop callback.
struct CvTexture(#[allow(dead_code)] CFRetained<CVMetalTexture>);
// SAFETY: a CVMetalTexture is an immutable CoreFoundation object with atomic retain/release; this
// handle is only held (and released) — never used to read or write pixels.
unsafe impl Send for CvTexture {}
// SAFETY: as above — no access through `&CvTexture`.
unsafe impl Sync for CvTexture {}

/// The `CVMetalTextureCache` import route (see the module docs). Build one per wgpu device and
/// reuse it: the cache recycles its plane textures across frames.
pub struct TextureCacheImporter {
    cache: CFRetained<CVMetalTextureCache>,
}

// SAFETY: CVMetalTextureCache is documented thread-safe (internally synchronised); we only create
// textures through it.
unsafe impl Send for TextureCacheImporter {}
// SAFETY: as above.
unsafe impl Sync for TextureCacheImporter {}

impl TextureCacheImporter {
    /// An importer for `device` (must be a Metal device).
    pub fn new(device: &wgpu::Device) -> Result<TextureCacheImporter, MediaError> {
        // SAFETY: we only read the raw MTLDevice from the hal guard while it is alive.
        let hal = unsafe { device.as_hal::<wgpu_hal::api::Metal>() }.ok_or_else(|| MediaError::Unsupported("the wgpu device is not a Metal device".into()))?;
        let raw_device = hal.raw_device();
        let mut out: *mut CVMetalTextureCache = std::ptr::null_mut();
        // SAFETY: a valid MTLDevice, null (default) attribute dictionaries, and a valid out-pointer.
        let status = unsafe { CVMetalTextureCache::create(None, None, raw_device, None, NonNull::from(&mut out)) };
        let ptr = NonNull::new(out).ok_or_else(|| MediaError::Platform(format!("CVMetalTextureCacheCreate failed ({status})")))?;
        if status != kCVReturnSuccess {
            return Err(MediaError::Platform(format!("CVMetalTextureCacheCreate failed ({status})")));
        }
        // SAFETY: CVMetalTextureCacheCreate returns the cache at +1 (Create rule); we take ownership.
        let cache = unsafe { CFRetained::from_raw(ptr) };
        Ok(TextureCacheImporter { cache })
    }

    /// The frame's planes on `device` through the cache (same layout as [`import`]).
    pub fn import(&self, device: &wgpu::Device, frame: &NativeFrame) -> Result<Vec<Plane>, MediaError> {
        let buffer = pixel_buffer(frame)?;
        let surface = CVPixelBufferGetIOSurface(Some(&buffer.0)).ok_or_else(|| MediaError::Platform("pixel buffer has no IOSurface".into()))?;
        let plane_count = surface.plane_count();
        let mut out = Vec::new();
        for (index, format, mtl_format) in plane_specs(frame.layout) {
            let (w, h) = if plane_count == 0 { (surface.width(), surface.height()) } else { (surface.width_of_plane(index), surface.height_of_plane(index)) };
            let width = u32::try_from(w).map_err(|_| MediaError::Invalid("plane width".into()))?;
            let height = u32::try_from(h).map_err(|_| MediaError::Invalid("plane height".into()))?;
            if width == 0 || height == 0 {
                return Err(MediaError::Platform(format!("plane {index} is empty")));
            }
            let mut tex_out: *mut CVMetalTexture = std::ptr::null_mut();
            // SAFETY: a live cache and pixel buffer (an image buffer), no attributes, and a valid
            // out-pointer; the format and plane size come from the buffer's own IOSurface.
            let status = unsafe {
                CVMetalTextureCache::create_texture_from_image(None, &self.cache, &buffer.0, None, mtl_format, w, h, index, NonNull::from(&mut tex_out))
            };
            let ptr = NonNull::new(tex_out).ok_or_else(|| MediaError::Platform(format!("CVMetalTextureCacheCreateTextureFromImage failed ({status})")))?;
            // SAFETY: the Create rule — the CVMetalTexture is returned at +1; we take ownership.
            let cv = unsafe { CFRetained::from_raw(ptr) };
            if status != kCVReturnSuccess {
                return Err(MediaError::Platform(format!("CVMetalTextureCacheCreateTextureFromImage failed ({status})")));
            }
            let raw = CVMetalTextureGetTexture(&cv).ok_or_else(|| MediaError::Platform("CVMetalTextureGetTexture returned nil".into()))?;
            let (keep_tex, keep_buf) = (CvTexture(cv), buffer.clone());
            let texture = wrap(
                device,
                raw,
                format,
                width,
                height,
                Box::new(move || {
                    drop(keep_tex);
                    drop(keep_buf);
                }),
            );
            out.push(Plane { texture, format, width, height, surface_id: surface.id() });
        }
        Ok(out)
    }

    /// Let the cache release textures nobody holds any more (call once per frame or so).
    pub fn flush(&self) {
        self.cache.flush(0);
    }
}
