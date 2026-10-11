//! GPU LMBD decode. An LMB7 frame is a BC7 block stream: uploaded unchanged into a BC7 texture, it
//! is decoded by the GPU's texture hardware every time it is sampled — no shader decode and no CPU
//! pixels. The hardware's BC7 decode is bit-exact with [`dxvkit_plus_core::bc7`] (checked by the
//! test suite on random blocks of every mode and on the LMBD compatibility vectors).
//!
//! The texture has the frame's coded size (rounded up to whole 4x4 blocks); sample the top-left
//! `width x height`. LMBD colour is sRGB-encoded unless the header says linear: [`LmbdTexture::view`]
//! with `linear_light = true` returns a `Bc7RgbaUnormSrgb` view, so sampling yields linear light
//! (the original Metal player's sampling); `false` returns the stored values.

use crate::{GpuError, create_bc_texture_with_views, write_bc};
use dxvkit_plus_core::lmbd::{Colorspace, Header};
use dxvkit_plus_core::source::PixelFormat;

/// One BC7 texture for the frames of an LMBD file.
#[derive(Debug)]
pub struct LmbdTexture {
    texture: wgpu::Texture,
    frame_size: (u32, u32),
    coded_size: (u32, u32),
    colorspace: Colorspace,
}

impl LmbdTexture {
    /// A texture for `header`'s frames (`TEXTURE_BINDING | COPY_DST`, with an sRGB view format).
    pub fn new(device: &wgpu::Device, header: &Header) -> Result<LmbdTexture, GpuError> {
        let (cw, ch) = header.coded_size();
        let texture = create_bc_texture_with_views(device, PixelFormat::Bc7, cw, ch, "dxvkit-plus-lmbd", &[wgpu::TextureFormat::Bc7RgbaUnormSrgb])?;
        Ok(LmbdTexture { texture, frame_size: (header.width, header.height), coded_size: (cw, ch), colorspace: header.colorspace })
    }

    /// Upload one frame's BC7 blocks (exactly `header.frame_bytes()`, from
    /// [`dxvkit_plus_core::lmbd::LmbdFile::decode_frame`]).
    pub fn upload(&self, queue: &wgpu::Queue, blocks: &[u8]) -> Result<(), GpuError> {
        write_bc(queue, &self.texture, PixelFormat::Bc7, self.coded_size.0, self.coded_size.1, blocks)
    }

    /// A view to sample: the stored values, or (`linear_light`, for sRGB files) linear light.
    pub fn view(&self, linear_light: bool) -> wgpu::TextureView {
        let format =
            if linear_light && self.colorspace == Colorspace::Srgb { wgpu::TextureFormat::Bc7RgbaUnormSrgb } else { wgpu::TextureFormat::Bc7RgbaUnorm };
        self.texture.create_view(&wgpu::TextureViewDescriptor { format: Some(format), ..Default::default() })
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
    /// The picture size (sample this much of the texture).
    pub fn frame_size(&self) -> (u32, u32) {
        self.frame_size
    }
    /// The texture size (whole 4x4 blocks).
    pub fn coded_size(&self) -> (u32, u32) {
        self.coded_size
    }
    pub fn colorspace(&self) -> Colorspace {
        self.colorspace
    }
}
