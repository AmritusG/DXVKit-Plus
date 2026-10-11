//! The README's quick start, compiled: decode frame N of any clip DXVKit Plus plays (HAP blocks or a
//! zero-copy native frame) into an RGBA texture.
//!
//! ```text
//! cargo run -p dxvkit-plus-apple --example decode_frame -- clip.mov 42
//! ```

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use dxvkit_plus_core::source::{self, Decoded, ImportOptions};
    use dxvkit_plus_wgpu::{FrameRenderer, Gpu, NativePlanes};
    use std::sync::Arc;

    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).ok_or("usage: decode_frame <clip> [frame]")?;
    let n: u32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(0);

    let opts = ImportOptions { backend: Some(Arc::new(dxvkit_plus_apple::AppleBackend)), ..Default::default() };
    let info = source::probe(path, &opts)?; // codec, size, frames, exact rate, timecode
    let mut clip = source::open(&info, &opts, true)?;
    let gpu = Gpu::headless()?;
    let mut renderer = FrameRenderer::new(&gpu.device);
    let importer = dxvkit_plus_apple::surface::TextureCacheImporter::new(&gpu.device)?; // once per device
    match clip.decode(n)? {
        d @ Decoded::Planes { .. } => {
            renderer.render(&gpu.device, &gpu.queue, &d)?; // HAP: blocks -> BC texture
        }
        Decoded::Native(frame) => {
            // Zero-copy: the textures alias the decoder's IOSurface.
            let planes = importer.import(&gpu.device, &frame)?;
            let textures: Vec<_> = planes.into_iter().map(|p| p.texture).collect();
            let native = NativePlanes { textures: &textures, layout: frame.layout, matrix: frame.matrix, width: frame.width, height: frame.height };
            renderer.render_native(&gpu.device, &gpu.queue, &native)?;
        }
    }
    let out = renderer.output().ok_or("no output")?; // Rgba8Unorm, the clip's size
    println!("{path}: frame {n} -> {}x{} {:?} texture", out.width(), out.height(), out.format());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this example needs macOS (native backend)");
}
