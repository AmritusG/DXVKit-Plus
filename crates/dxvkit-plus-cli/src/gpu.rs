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
