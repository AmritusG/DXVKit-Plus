//! One GPU session: device, frame renderer and (macOS) the native import route.

use dxvkit_plus_core::source::Decoded;
use dxvkit_plus_wgpu::{FrameRenderer, Gpu, GpuError, NativePlanes, read_texture_rgba, wait_idle};

/// How native frames are imported (both zero-copy; see `dxvkit-plus-apple::surface`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportRoute {
    /// `newTextureWithDescriptor:iosurface:plane:`.
    IoSurface,
    /// `CVMetalTextureCacheCreateTextureFromImage` (the default: measured faster).
    TextureCache,
}

impl ImportRoute {
    pub fn name(self) -> &'static str {
        match self {
            ImportRoute::IoSurface => "iosurface",
            ImportRoute::TextureCache => "cv_texture_cache",
        }
    }
}

pub struct Session {
    pub gpu: Gpu,
    pub renderer: FrameRenderer,
    pub route: ImportRoute,
    #[cfg(target_os = "macos")]
    cache: Option<dxvkit_plus_apple::surface::TextureCacheImporter>,
    /// IOSurface id of the last native frame's planes (zero-copy evidence).
    pub last_surface: Option<u32>,
}

impl Session {
    pub fn new() -> Result<Session, GpuError> {
        let gpu = Gpu::headless()?;
        let renderer = FrameRenderer::new(&gpu.device);
        let mut s = Session {
            gpu,
            renderer,
            route: ImportRoute::IoSurface,
            #[cfg(target_os = "macos")]
            cache: None,
            last_surface: None,
        };
        // The default route is the texture cache (measured faster); without it, IOSurface.
        if cfg!(target_os = "macos") && s.set_route(ImportRoute::TextureCache).is_err() {
            s.route = ImportRoute::IoSurface;
        }
        Ok(s)
    }

    pub fn set_route(&mut self, route: ImportRoute) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        if route == ImportRoute::TextureCache && self.cache.is_none() {
            self.cache = Some(dxvkit_plus_apple::surface::TextureCacheImporter::new(&self.gpu.device).map_err(|e| e.to_string())?);
        }
        self.route = route;
        Ok(())
    }

    /// Upload / import and convert `d`; returns after submitting (no wait).
    pub fn render(&mut self, d: &Decoded) -> Result<(), String> {
        match d {
            Decoded::Planes { .. } => self.renderer.render(&self.gpu.device, &self.gpu.queue, d).map(|_| ()).map_err(|e| e.to_string()),
            Decoded::Native(f) => self.render_native(f),
        }
    }

    #[cfg(target_os = "macos")]
    fn render_native(&mut self, f: &dxvkit_plus_core::backend::NativeFrame) -> Result<(), String> {
        let planes = match (self.route, &self.cache) {
            (ImportRoute::TextureCache, Some(c)) => c.import(&self.gpu.device, f),
            _ => dxvkit_plus_apple::surface::import(&self.gpu.device, f),
        }
        .map_err(|e| e.to_string())?;
        self.last_surface = planes.first().map(|p| p.surface_id);
        let textures: Vec<wgpu::Texture> = planes.into_iter().map(|p| p.texture).collect();
        let native = NativePlanes { textures: &textures, layout: f.layout, matrix: f.matrix, width: f.width, height: f.height };
        self.renderer.render_native(&self.gpu.device, &self.gpu.queue, &native).map(|_| ()).map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    fn render_native(&mut self, _f: &dxvkit_plus_core::backend::NativeFrame) -> Result<(), String> {
        let _ = NativePlanes {
            textures: &[],
            layout: dxvkit_plus_core::backend::PixelLayout::Bgra,
            matrix: dxvkit_plus_core::backend::YuvMatrix::Bt709,
            width: 0,
            height: 0,
        };
        Err("native frames need a platform backend (macOS today)".into())
    }

    /// Render and wait for the GPU.
    pub fn render_wait(&mut self, d: &Decoded) -> Result<(), String> {
        self.render(d)?;
        #[cfg(target_os = "macos")]
        if let Some(c) = &self.cache {
            c.flush();
        }
        wait_idle(&self.gpu.device).map_err(|e| e.to_string())
    }

    /// Render and read the result back as RGBA8.
    pub fn render_rgba(&mut self, d: &Decoded) -> Result<Vec<u8>, String> {
        self.render(d)?;
        let out = self.renderer.output().ok_or("no output")?;
        read_texture_rgba(&self.gpu.device, &self.gpu.queue, out).map_err(|e| e.to_string())
    }
}

/// The other upload paths for CPU-decoded frames (`verify` checks each against
/// [`FrameRenderer::render`]): staged through persistent memory, decoded straight into staging,
/// and sampled directly without the conversion pass.
pub struct AltPaths {
    staged: FrameRenderer,
    into: FrameRenderer,
    view: FrameRenderer,
    uploader: dxvkit_plus_wgpu::Uploader,
}

impl AltPaths {
    pub fn new(gpu: &Gpu) -> AltPaths {
        AltPaths {
            staged: FrameRenderer::new(&gpu.device),
            into: FrameRenderer::new(&gpu.device),
            view: FrameRenderer::new(&gpu.device),
            uploader: dxvkit_plus_wgpu::Uploader::new(&gpu.device, 1 << 20, 2),
        }
    }

    fn finish(gpu: &Gpu, uploader: &mut dxvkit_plus_wgpu::Uploader, r: &mut FrameRenderer, mut enc: wgpu::CommandEncoder) -> Result<Vec<u8>, String> {
        let out = r.encode_convert(&gpu.device, &gpu.queue, &mut enc, None).map_err(|e| e.to_string())?.clone();
        uploader.submit(&gpu.device, &gpu.queue, enc).map_err(|e| e.to_string())?;
        read_texture_rgba(&gpu.device, &gpu.queue, &out).map_err(|e| e.to_string())
    }

    /// `upload_staged` + conversion, and (when the frame samples as stored) its `direct_view`
    /// read through a plain texel copy, alpha taken as 1 for opaque frames.
    pub fn staged(&mut self, gpu: &Gpu, d: &Decoded) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        self.staged.upload_staged(&gpu.device, &mut enc, self.uploader.belt(), d).map_err(|e| e.to_string())?;
        let direct = self.staged.direct_view().map(|v| (v.view.clone(), v.width, v.height, v.opaque));
        let staged = AltPaths::finish(gpu, &mut self.uploader, &mut self.staged, enc)?;
        let direct = match direct {
            Some((view, w, h, opaque)) => {
                let out = self.view.render_view(&gpu.device, &gpu.queue, &view, w, h).map_err(|e| e.to_string())?.clone();
                let mut px = read_texture_rgba(&gpu.device, &gpu.queue, &out).map_err(|e| e.to_string())?;
                if opaque {
                    px.chunks_exact_mut(4).filter_map(|p| p.get_mut(3)).for_each(|a| *a = 255);
                }
                Some(px)
            }
            None => None,
        };
        Ok((staged, direct))
    }

    /// `decode_into` a staged frame + conversion (`None` when the source has no fixed layout).
    pub fn decode_into(&mut self, gpu: &Gpu, src: &mut dyn dxvkit_plus_core::source::FrameSource, frame: u32) -> Result<Option<Vec<u8>>, String> {
        let Some(layout) = src.layout() else { return Ok(None) };
        let mut staged = self.uploader.stage(&layout).map_err(|e| e.to_string())?;
        src.decode_into(frame, &mut staged).map_err(|e| e.to_string())?;
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        self.into.upload_from_stage(&gpu.device, &mut enc, staged).map_err(|e| e.to_string())?;
        AltPaths::finish(gpu, &mut self.uploader, &mut self.into, enc).map(Some)
    }
}
