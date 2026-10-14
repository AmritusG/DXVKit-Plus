//! `dxvkit-plus` — probe, decode, bench and verify video files with DXVKit Plus.
//!
//! ```text
//! dxvkit-plus probe <file|folder>
//! dxvkit-plus decode <file> <frame> <out.png> [--cpu]
//! dxvkit-plus bench <file>... [--frames N] [--layers N [--layers-only] [--variants V,..]] [--import]
//! dxvkit-plus verify <file>... [--tiles] [--source-tolerance N]
//! dxvkit-plus gen-pattern <dir> [--frames N] [--width W] [--height H] [--plasma|--graphics [--alpha]]
//! dxvkit-plus encode-lmbd <source> <out.lmbd> [--lambda L] [--lookback N] [--chunks K] [--fps N/D] ...
//! dxvkit-plus encode-lmbd <source> <out.lmbd> --profile lmbx [--chroma 444|422halo] [--quality Q | --frame-bytes N]
//! dxvkit-plus compare <reference> <file> [--frame N] [--ref-frame M]
//! ```
//!
//! `--alpha-mode straight|premultiplied` (any command that opens clips): how the colour of DXV clips
//! with alpha is read (default straight, Resolume Alley's convention).
//!
//! Every command prints JSON (one object per line) to stdout; errors go to stderr with exit status 1.
#![forbid(unsafe_code)]

mod bench;
mod gpu;
mod layers;
mod lmbd;
mod verify;

use dxvkit_plus_core::source::{self, AlphaMode, ImportOptions, MediaInfo};
use dxvkit_plus_core::testpattern;
use std::path::Path;
use std::process::ExitCode;

/// The Zstd decode backend LMBD chunks go through in this build (core's `zstd-c` / `zstd-rust`
/// feature, forwarded by this crate; `zstd-c` wins when both are on). Printed by probe and bench.
pub const ZSTD_BACKEND: &str = if cfg!(feature = "zstd-c") { "libzstd" } else { "ruzstd" };

const USAGE: &str = "usage:
  dxvkit-plus probe <file|folder>
  dxvkit-plus decode <file> <frame> <out.png> [--cpu | --float]
  dxvkit-plus bench <file>... [--frames N] [--layers N [--layers-only] [--variants V,..]] [--import]
  dxvkit-plus verify <file>... [--tiles] [--source-tolerance N]
  dxvkit-plus gen-pattern <dir> [--frames N] [--width W] [--height H] [--plasma|--graphics [--alpha]]
  dxvkit-plus encode-lmbd <source> <out.lmbd> [--lambda L] [--lookback N] [--chunks K] [--fps N/D]
                          [--level Z] [--compressor zstd|none] [--rdo-partitions P] [--threads T]
                          [--start F] [--frames N] [--quiet]
  dxvkit-plus encode-lmbd <source> <out.lmbd> --profile lmbx [--chroma 444|422halo]
                          [--quality Q | --frame-bytes N] [--modes dct|flat,palette,lossy]
                          [--colour-tol T|off] [--tsizes rule|rd|rd8] [--compressor zstd|none]
                          [--start F] [--frames N]
                          [--quiet]
  dxvkit-plus compare <reference> <file> [--frame N] [--ref-frame M]
options for commands that open clips:
  --alpha-mode straight|premultiplied   how DXV colour with alpha is read (default straight)";

/// The `--alpha-mode` choice for this run (set once in `main`).
static ALPHA_MODE: std::sync::OnceLock<AlphaMode> = std::sync::OnceLock::new();

/// The import options with this platform's backend.
pub fn options() -> ImportOptions {
    #[cfg(target_os = "macos")]
    {
        ImportOptions {
            backend: Some(std::sync::Arc::new(dxvkit_plus_apple::AppleBackend)),
            alpha_mode: ALPHA_MODE.get().copied().unwrap_or_default(),
            ..ImportOptions::default()
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        ImportOptions { alpha_mode: ALPHA_MODE.get().copied().unwrap_or_default(), ..ImportOptions::default() }
    }
}

/// Parsed arguments: positional values and `--flag [value]` options.
pub struct Args {
    pub positional: Vec<String>,
    pub flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String], valued: &[&str]) -> Args {
        let (mut positional, mut flags) = (Vec::new(), Vec::new());
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let v = if valued.contains(&name) { it.next().cloned() } else { None };
                flags.push((name.to_string(), v));
            } else {
                positional.push(a.clone());
            }
        }
        Args { positional, flags }
    }
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
    /// The value of `--name value`, if given.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }
    pub fn num(&self, name: &str, default: u32) -> Result<u32, String> {
        match self.flags.iter().find(|(n, _)| n == name) {
            None => Ok(default),
            Some((_, Some(v))) => v.parse().map_err(|_| format!("--{name}: not a number: {v}")),
            Some((_, None)) => Err(format!("--{name} needs a value")),
        }
    }
}

pub fn json_line(v: &serde_json::Value) {
    println!("{v}");
}

fn probe(args: &Args) -> Result<(), String> {
    let [path] = args.positional.as_slice() else { return Err(USAGE.into()) };
    let opts = options();
    let info = source::probe(path, &opts).map_err(|e| e.to_string())?;
    let mut v = serde_json::to_value(&info).map_err(|e| e.to_string())?;
    if let Some(o) = v.as_object_mut() {
        o.insert("fps".into(), serde_json::json!(info.rate.as_f64()));
        o.insert("backend".into(), serde_json::json!(opts.backend.as_ref().map(|b| b.name())));
        if info.kind == source::SourceKind::Lmbd {
            o.insert("zstd_backend".into(), serde_json::json!(ZSTD_BACKEND));
        }
        if info.kind == source::SourceKind::Dxv {
            o.insert("dxv_variant".into(), serde_json::json!(dxv_variant(path)));
            o.insert("alpha_mode".into(), serde_json::json!(opts.alpha_mode));
        }
        if let Some(tc) = &info.timecode {
            o.insert("timecode_label".into(), serde_json::json!(tc.label()));
        }
    }
    json_line(&v);
    Ok(())
}

/// The DXV variant of a file's first packet (`None` when it does not parse).
fn dxv_variant(path: &str) -> Option<&'static str> {
    use dxvkit_plus_core::container::{Codec, Movie};
    let movie = Movie::open(Path::new(path)).ok()?;
    let track = movie.track(Codec::Dxv)?;
    let mut packet = Vec::new();
    movie.read_sample(track, 0, &mut packet).ok()?;
    dxvkit_plus_core::dxv::parse_header(&packet).ok().map(|h| h.variant.name())
}

pub fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(rgba).map_err(|e| e.to_string())?;
    wr.finish().map_err(|e| e.to_string())
}

fn decode(args: &Args) -> Result<(), String> {
    let [path, frame, out] = args.positional.as_slice() else { return Err(USAGE.into()) };
    let frame: u32 = frame.parse().map_err(|_| format!("not a frame number: {frame}"))?;
    let opts = options();
    let info = source::probe(path, &opts).map_err(|e| e.to_string())?;
    let mut src = source::open(&info, &opts, true).map_err(|e| e.to_string())?;
    let d = src.decode(frame).map_err(|e| e.to_string())?;
    let (w, h) = d.size();
    let (rgba, route) = if args.has("float") {
        // LMBX only: the floating-point inverse transform (not conforming), for comparisons.
        if info.codec != "LMBX" {
            return Err("--float: only for LMBX files".into());
        }
        (lmbd::float_rgba(path, frame, w, h)?, "float")
    } else if args.has("cpu") {
        (src.cpu_rgba(&d).map_err(|e| e.to_string())?, "cpu")
    } else {
        let mut g = gpu::Session::new().map_err(|e| e.to_string())?;
        (g.render_rgba(&d).map_err(|e| e.to_string())?, "gpu")
    };
    write_png(Path::new(out), w, h, &rgba)?;
    json_line(&serde_json::json!({"decoded": path, "frame": d.index(), "width": w, "height": h, "route": route, "png": out}));
    Ok(())
}

fn gen_pattern(args: &Args) -> Result<(), String> {
    let [dir] = args.positional.as_slice() else { return Err(USAGE.into()) };
    let (frames, w, h) = (args.num("frames", 24)?, args.num("width", 128)?, args.num("height", 72)?);
    if w == 0 || h == 0 || w > dxvkit_plus_core::MAX_DIMENSION || h > dxvkit_plus_core::MAX_DIMENSION {
        return Err(format!("size {w}x{h} out of range"));
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for f in 0..frames {
        let px = if args.has("graphics") {
            testpattern::graphics(f, w, h, args.has("alpha"))
        } else if args.has("plasma") {
            let mut p = testpattern::plasma(f, w, h, args.has("alpha"));
            testpattern::stamp_frame_id(&mut p, w, h, f);
            p
        } else {
            testpattern::tiles(f, w, h)
        };
        write_png(&Path::new(dir).join(format!("f{f:05}.png")), w, h, &px)?;
    }
    let pattern = if args.has("graphics") {
        if args.has("alpha") { "graphics_alpha" } else { "graphics" }
    } else {
        match (args.has("plasma"), args.has("alpha")) {
            (true, true) => "plasma_alpha",
            (true, false) => "plasma",
            _ => "tiles",
        }
    };
    json_line(&serde_json::json!({"pattern": pattern, "dir": dir, "frames": frames, "width": w, "height": h}));
    Ok(())
}

pub fn probe_open(path: &str) -> Result<(MediaInfo, Box<dyn source::FrameSource>), String> {
    let opts = options();
    let info = source::probe(path, &opts).map_err(|e| format!("{path}: {e}"))?;
    let src = source::open(&info, &opts, true).map_err(|e| format!("{path}: {e}"))?;
    Ok((info, src))
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = raw.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let args = Args::parse(
        rest,
        &[
            "frames",
            "layers",
            "variants",
            "width",
            "height",
            "source-tolerance",
            "lambda",
            "lookback",
            "chunks",
            "fps",
            "level",
            "compressor",
            "rdo-partitions",
            "threads",
            "start",
            "frame",
            "ref-frame",
            "alpha-mode",
            "profile",
            "chroma",
            "quality",
            "frame-bytes",
            "modes",
            "colour-tol",
            "tsizes",
        ],
    );
    if let Some(a) = args.value("alpha-mode") {
        let mode = match a {
            "straight" => AlphaMode::Straight,
            "premultiplied" => AlphaMode::Premultiplied,
            other => {
                eprintln!("dxvkit-plus: --alpha-mode {other}: expected straight or premultiplied");
                return ExitCode::FAILURE;
            }
        };
        let _ = ALPHA_MODE.set(mode);
    }
    let r = match cmd.as_str() {
        "probe" => probe(&args),
        "decode" => decode(&args),
        "bench" => bench::run(&args),
        "verify" => verify::run(&args),
        "gen-pattern" => gen_pattern(&args),
        "encode-lmbd" => lmbd::encode(&args),
        "compare" => lmbd::compare(&args),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dxvkit-plus: {e}");
            ExitCode::FAILURE
        }
    }
}
