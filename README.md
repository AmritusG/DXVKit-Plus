# DXVKit Plus

**Rust video for VJs: DXV, HAP, native OS codecs and the LMBD GPU codec, frame-exact, zero-copy into wgpu.**

DXVKit Plus plays video the way VJ software uses it: GPU-texture codecs (DXV, HAP, and our own LMBD)
decompressed to blocks and uploaded straight into BC textures, platform codecs (H.264, HEVC, ProRes, MJPEG)
decoded by the operating system's hardware decoder and imported into the GPU without a copy, a
QuickTime demuxer that reads the timecode track, and a frame-exact transport (loop, bounce,
reverse, any speed, seek, step, beat and timecode sync). Pure Rust where it can be, MIT-licensed,
no FFmpeg, GStreamer or libmpv.

> DXVKit Plus is an independent, clean-room project. It is not affiliated with or endorsed by
> Resolume or Vidvox; DXV and HAP are named only to describe compatibility.

This is release **0.4.0**: **DXV** decodes — DXV3 Normal and High Quality, with and without alpha,
and legacy DXV1/2 — through a safe-Rust port of DXVKit's decoder that is bit-exact on DXVKit's 334
test vectors and against its C core; High Quality converts on the GPU bit-exactly with the CPU;
alpha reads as straight (Resolume Alley's convention, the default) or premultiplied, per clip
([DXV](#dxv)). 0.3.0 made getting decoded frames onto the GPU fast. Block frames go up
through persistent staging memory instead of a new buffer per upload, a pipelined player keeps the
GPU busy while the next frames decode (or decodes straight into GPU-visible memory), and frames
that are plain BC (LMBD, HAP R / RGBA / RGB) can be sampled as they are, skipping the conversion
pass. 16 layers of 1080p LMBD went from 132 to 463-516 steps per second on the Mac Studio, HAP from
163-202 to 692-939 ([numbers](#getting-frames-onto-the-gpu-030)). 0.2.0 added **LMBD**, our own GPU
codec — BC7 with rate-distortion optimisation, then Zstd, in a documented container
([`docs/LMBD.md`](docs/LMBD.md)) — and 0.2.1 decodes its Zstd chunks with libzstd by default.

## Codec support

| Format | Decode | Output | Notes |
|---|---|---|---|
| HAP (`Hap1`) | ✅ pure Rust | BC1 blocks | Snappy and chunked frames (chunks decompress in parallel) |
| HAP Alpha (`Hap5`) | ✅ pure Rust | BC3 blocks | |
| HAP Q (`HapY`) | ✅ pure Rust | BC3 scaled YCoCg → RGB in WGSL | |
| HAP Q Alpha (`HapM`) | ✅ pure Rust | BC3 YCoCg + BC4 alpha | |
| HAP Alpha-Only (`HapA`) | ✅ pure Rust | BC4 | |
| HAP R (`Hap7`) | ✅ pure Rust | BC7 blocks | CPU BC7 decoder bit-exact with the GPU |
| HAP HDR (`HapH`) | ✅ pure Rust | BC6H blocks (unsigned and signed) | GPU only (no CPU BC6H decoder) |
| H.264, HEVC | ✅ macOS (VideoToolbox) | NV12 planes, zero-copy | WGSL BT.601 / 709 / 2020 |
| ProRes 422 / 4444 (alpha) | ✅ macOS | BGRA, zero-copy | |
| MJPEG | ✅ macOS | BGRA, zero-copy | |
| LMBD (`LMB7`: BC7 + RDO + Zstd) | ✅ Rust + libzstd (pure Rust with `zstd-rust`), **and encode** | BC7 blocks | chunks decompress in parallel; GPU decode bit-exact with the CPU; both Zstd backends decode identically |
| LMBD (`LMBX`: YCoCg + integer DCT + rANS, variable rate) | ✅ pure Rust, **and encode** | the bitstream itself, decoded by WGSL compute | 4:2:2-halo or 4:4:4 chroma, optional alpha; GPU bit-exact with the CPU; frozen spec + conformance vectors |
| DXV3 Normal Quality (`DXT1`) | ✅ pure Rust | BC1 blocks | block-copy layer; bit-exact with DXVKit |
| DXV3 Normal Quality With Alpha (`DXT5`) | ✅ pure Rust | BC3 blocks | alpha straight (default) or premultiplied |
| DXV3 High Quality (`YCG6`) | ✅ pure Rust | Y BC4 + CoCg BC5 → RGB in WGSL, bit-exact with the CPU | FSE-coded opcode streams; the two sections decode in parallel |
| DXV3 High Quality With Alpha (`YG10`) | ✅ pure Rust | Y + A BC5 + CoCg BC5 → RGB in WGSL | |
| DXV1 / 2 (`DXDI`, LZF) | ✅ pure Rust | BC1 / BC3 blocks | |
| Native codecs on Windows | 🔜 `Backend` stub | — | Media Foundation + DX12 import, designed |
| PNG stills / sequences | ✅ | RGBA8 | |

Every decode path is frame-accurate: frame `n` is exactly frame `n`, also after seeks (tested on
every frame of every fixture), and every error is a value — hostile files never panic (fuzzed).

## Crates

| Crate | What |
|---|---|
| [`dxvkit-plus-core`](crates/dxvkit-plus-core) | demux, DXV, HAP, the LMBD container (read and write), CPU block decoders, frame cache, transport, the platform `Backend` trait. `#![forbid(unsafe_code)]`, no GPU dependency. Links libzstd (C) for LMBD by default; pure Rust with the `zstd-rust` feature ([below](#zstd-decode-backend)). |
| [`dxvkit-plus-lmbd-enc`](crates/dxvkit-plus-lmbd-enc) | the LMBD encoder: 4-mode BC7 + rate-distortion optimisation (a faithful port of bc7enc_rdo) + Zstd. Byte-identical to the original LMBD encoder. |
| [`dxvkit-plus-wgpu`](crates/dxvkit-plus-wgpu) | BC texture upload and WGSL conversion on wgpu 30; persistent staging and pipelined submission (`Uploader`, `StagedFrame`), direct sampling of plain-BC frames (`DirectView`); `LmbdTexture` (GPU LMBD decode, with an sRGB view). |
| [`dxvkit-plus-apple`](crates/dxvkit-plus-apple) | macOS: AVFoundation / VideoToolbox → IOSurface → wgpu Metal texture, zero-copy. The only crate with `unsafe`. |
| [`dxvkit-plus-cli`](crates/dxvkit-plus-cli) | the `dxvkit-plus` command. |

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and [`docs/PROVENANCE.md`](docs/PROVENANCE.md).

## Quick start

Not on crates.io yet; depend on the git repository:

```toml
[dependencies]
dxvkit-plus-core = { git = "https://github.com/AmritusG/DXVKit-Plus", tag = "v0.4.0" }
dxvkit-plus-wgpu = { git = "https://github.com/AmritusG/DXVKit-Plus", tag = "v0.4.0" }
dxvkit-plus-apple = { git = "https://github.com/AmritusG/DXVKit-Plus", tag = "v0.4.0" } # macOS
# No C in the decoder: add `default-features = false, features = ["zstd-rust"]` to every
# DXVKit Plus crate you depend on (see "Zstd decode backend" below).
```

```rust
use dxvkit_plus_core::source::{self, Decoded, ImportOptions};
use dxvkit_plus_wgpu::{FrameRenderer, Gpu};
use std::sync::Arc;

let opts = ImportOptions { backend: Some(Arc::new(dxvkit_plus_apple::AppleBackend)), ..Default::default() };
let info = source::probe("clip.mov", &opts)?;           // codec, size, frames, exact rate, timecode
let mut clip = source::open(&info, &opts, true)?;
let gpu = Gpu::headless()?;
let mut renderer = FrameRenderer::new(&gpu.device);
let importer = dxvkit_plus_apple::surface::TextureCacheImporter::new(&gpu.device)?; // macOS, once per device
match clip.decode(42)? {
    d @ Decoded::Planes { .. } => { renderer.render(&gpu.device, &gpu.queue, &d)?; }  // HAP, LMBD: blocks → BC texture
    Decoded::Native(frame) => {                                                       // zero-copy import
        let planes = importer.import(&gpu.device, &frame)?;
        let textures: Vec<_> = planes.into_iter().map(|p| p.texture).collect();
        renderer.render_native(&gpu.device, &gpu.queue, &dxvkit_plus_wgpu::NativePlanes {
            textures: &textures, layout: frame.layout, matrix: frame.matrix, width: frame.width, height: frame.height,
        })?;
    }
}
let rgba_texture = renderer.output();                    // Rgba8Unorm, the clip's size
```

For playback, `cache::Player` decodes ahead on worker threads and `transport::Transport` says which
frame to show at each clock tick (exact integer time).

**Many layers (0.3.0).** `render` is the simple path; a player with many layers records every
layer into one submission through an `Uploader` (persistent staging memory, at most 2 steps in
flight, no device-wide wait) and samples plain-BC frames directly:

```rust
use dxvkit_plus_wgpu::{FrameRenderer, Uploader};

let mut up = Uploader::new(&gpu.device, step_bytes, 2);       // ~ one step's frames; 2 in flight
let mut enc = gpu.device.create_command_encoder(&Default::default());
for (layer, decoded) in layers.iter_mut().zip(&frames) {      // frames from cache::Player
    layer.upload_staged(&gpu.device, &mut enc, up.belt(), decoded)?;
    if layer.direct_view().is_none() {                         // HAP Q, alpha-only, HDR: convert
        layer.encode_convert(&gpu.device, &gpu.queue, &mut enc, None)?;
    }
}
up.submit(&gpu.device, &gpu.queue, enc)?;                      // blocks only if 2 steps are still running
// sample layer.direct_view() (BC texture; alpha = 1 if `opaque`) or layer.output() (RGBA8)
```

For the lowest latency, decode straight into staging instead: `up.stage(&layout)`, with the
clip's `clip.layout()` (fixed for LMBD and HAP), gives a `StagedFrame` that a decoder thread fills with `clip.decode_into(frame, &mut staged)`, then
`layer.upload_from_stage(&gpu.device, &mut enc, staged)`.

## CLI

```sh
cargo install --git https://github.com/AmritusG/DXVKit-Plus --tag v0.4.0 dxvkit-plus-cli

dxvkit-plus probe clip.mov                 # JSON: codec, size, frames, rate, alpha, timecode
dxvkit-plus decode clip.mov 42 frame.png   # frame 42 through the GPU path (--cpu for the CPU path)
dxvkit-plus decode clip.mov 42 frame.png --alpha-mode premultiplied   # a DXV clip whose colour is premultiplied
dxvkit-plus verify --tiles tests/fixtures/*.mov tests/fixtures/*.mp4
dxvkit-plus bench clip.mov --frames 60 --layers 16 --import
dxvkit-plus bench clip.lmbd --layers 16 --layers-only --variants per_layer,staged,pipelined2_direct,into2_direct
dxvkit-plus gen-pattern frames/ --plasma --width 1920 --height 1080 --frames 60
dxvkit-plus encode-lmbd frames/ clip.lmbd --chunks 8   # any decodable source → LMBD (lambda 1, look-back 16384)
dxvkit-plus compare frames/ clip.lmbd --frame 3       # PSNR / SSIM of frame 3 against the source
```

`verify` checks every frame: GPU vs CPU (exact for LMBD, native frames and DXV High Quality; HAP Q
and DXV DXT1 / DXT5 within 2/255; other HAP within 1/255), frame identity (fixtures), shuffled seeks
vs sequential decode, and zero-copy for native frames. `bench` prints p50 / p99 per measurement as JSON lines.

## DXV

DXV (Resolume's codec) decodes through `dxvkit_plus_core::dxv`, a safe-Rust port of DXVKit's C
decoder (DXVKit is Amrit's MIT, clean-room DXV library). Every variant: DXV3 Normal Quality
(`DXT1`, BC1) and Normal Quality With Alpha (`DXT5`, BC3), High Quality (`YCG6`: Y BC4 + CoCg BC5)
and High Quality With Alpha (`YG10`: Y + A BC5 + CoCg BC5), stored payloads, and the legacy DXV1 / 2
`DXDI` packets (LZF). Frames come out as block planes like HAP's, so every 0.3.0 upload path works
(staged, pipelined, decode-into-staging; DXT1 / DXT5 also sample directly).

- **Bit-exact with DXVKit**: the 334 rows of DXVKit's test vectors (Resolume Alley and Media Encoder
  streams included) give byte-identical block streams and RGBA, and every frame of our DXV fixtures
  and the committed vectors equals DXVKit's C core (`dxvtool`).
- **GPU**: High Quality converts YCoCg on the GPU with DXVKit's integer arithmetic (the blocks
  uploaded as integer texels, palettes decoded in the shader), bit-exact with the CPU. DXT1 / DXT5
  sample as BC textures: the GPU's own BC decode, within 2/255 of DXVKit's truncating CPU convention
  — what Resolume Arena's GPU upload does too.
- **Alpha**: a DXV file does not say whether its colour is straight or premultiplied. The default is
  **straight** (Resolume Alley writes straight colour); `ImportOptions { alpha_mode:
  AlphaMode::Premultiplied, .. }` (CLI `--alpha-mode premultiplied`) reads it as premultiplied and
  un-premultiplies on output, per clip — the counterpart of Arena's per-clip "Alpha Type". That
  reading is what ffmpeg assumes for DXT5; on a straight-alpha test card our premultiplied mode
  is within 1.9/255 (CPU) / 2.6/255 (GPU) of ffmpeg's output as shown.

## LMBX

LMBX is LMBD's second profile (`0x10`): instead of BC7's fixed 8 bits per pixel it spends bits
where the picture needs them — YCoCg-R, an 8x8 DCT, a quantiser per 8x8 block chosen by rate-
distortion optimisation, and rANS — and the GPU decodes it with compute shaders (only the
compressed bitstream is uploaded). It grew out of Amrit's LMBX prototypes and is frozen here as
bitstream v1 ([`docs/LMBD.md`](docs/LMBD.md) §11):

- **Exact integer decoding** (LMBX-D1). The inverse transform, the colour conversion and every
  rounding are specified in integers, so every conforming decoder — CPU or GPU, ours or anyone's —
  produces the same samples bit for bit (10 bits of precision; 8-bit output is `(v + 2) >> 2`).
  Five CC0 conformance vectors carry the expected hashes.
- **Two chroma modes** (LMBX-D2): `422halo` (half-width chroma, interpolated across tile edges) and
  `444`, signalled per clip.
- **Parallel by construction.** Every plane of every 32x32 tile is its own stream: a 1080p frame is
  6,000-8,000 independent streams.

```sh
dxvkit-plus encode-lmbd frames/ clip.lmbd --profile lmbx --chroma 422halo --quality 16
dxvkit-plus encode-lmbd frames/ clip.lmbd --profile lmbx --chroma 444 --frame-bytes 400000   # rate control
dxvkit-plus compare frames/ clip.lmbd --frame 3 [--float]
```

LMBX_RESULTS_PLACEHOLDER

## LMBD

LMBD is the codec of Amrit's original LMBD project (its C encoder and Metal player), carried over
with its frozen v1 format. Files the original wrote decode here bit-exact, and
`dxvkit-plus-lmbd-enc` writes **byte-identical files** for the same frames and settings — a
faithful port of bc7enc_rdo's 4-mode BC7 encoder and RDO pass, down to the fused multiply-adds the
original's compiler forms (see [`docs/LMBD.md`](docs/LMBD.md), §9). Checked on five compatibility
vectors written by the original encoder (`tests/fixtures/lmbd_*.lmbd`) and on 1080p frames.

```rust
use dxvkit_plus_core::lmbd::Header;
use dxvkit_plus_lmbd_enc::{EncodeParams, encode_frames};

let frames: Vec<Vec<u8>> = load_frames();  // straight RGBA8, 1920x1080 each
let mut p = EncodeParams::default();      // lambda 1.0, look-back 16384, Zstd 9, 1 chunk
p.chunks = 8;                             // chunks decompress in parallel on playback
let out = std::fs::File::create("clip.lmbd")?;
encode_frames(std::io::BufWriter::new(out), Header::new(1920, 1080, 30, 1), &p, 8,
    |i| (i < frames.len() as u32).then(|| Ok(frames[i as usize].clone())), |_, _| {})?;
```

**Quality against HAP Q at matched size**, measured on our own CC0 clips (512×288, 4 frames each,
PSNR over RGB and SSIM on luma, each codec decoded on the CPU and compared with the source; raw
lines in `docs/data/lmbd-vs-hapq.jsonl`):

| Clip | HAP Q (glenc-cli `hapy`) | LMBD at about the same size | Difference |
|---|---|---|---|
| smooth gradient | 47 580 B/frame, 52.01 dB, SSIM 0.99887 | λ 0: 36 894 B (78 %), 65.00 dB, SSIM 0.99985 | **+13.0 dB** at 78 % of the size; λ 0.25 matches HAP Q's PSNR −1.1 dB at **8 %** of the size |
| flat tiles | 16 046 B, 51.67 dB, SSIM 0.9999 | λ 0: 9 327 B (58 %), 58.04 dB, SSIM 1.0000 | **+6.4 dB** at 58 % of the size |
| plasma with film grain | 134 688 B, 40.88 dB, SSIM 0.9946 | λ 0.5: 128 983 B (96 %), 41.24 dB, SSIM 0.9844 | +0.35 dB PSNR, **−0.010 SSIM** |

LMBD wins clearly on smooth and flat content and ties on noise-like grain, where neither codec can
compress much. Encoding is slow by design at the default settings (RDO with a 16 KB look-back:
about 8.6 CPU-minutes per 1080p frame of detailed footage — 14 frames took 15 minutes on the 14-core
Mac Studio, frames encoding in parallel); `--lambda 0` encodes 30 such frames in 8 seconds, and a
shorter `--lookback` sits in between.

### Zstd decode backend

LMBD's CPU work on playback is Zstd decompression, and it has two interchangeable backends in
`dxvkit-plus-core`, chosen at build time:

| Feature | Library | When |
|---|---|---|
| `zstd-c` (default) | libzstd 1.5.7, the standard C library (bundled, built by `zstd-sys`; BSD-3-Clause) | always, unless you opt out |
| `zstd-rust` | `ruzstd`, pure Rust (MIT) | `--no-default-features --features zstd-rust`, for a C-free core |

Every crate that uses the core forwards both features (`zstd-c` on by default). Both on: libzstd is
used. Neither: the core does not compile, with an error naming the two. Both backends decode every
LMBD frame identically — every compatibility-vector frame and 44 1080p frames hash to the same RGBA
under each, equal to the original decoder's — and `dxvkit-plus probe` / `bench` print which one
the binary was built with (`"zstd_backend": "libzstd"` or `"ruzstd"`). The encoder always uses
libzstd. libzstd is used under its BSD-3-Clause licence (it is dual BSD / GPLv2); the notice is in
[`LICENSES/libzstd-BSD-3-Clause.txt`](LICENSES/libzstd-BSD-3-Clause.txt).

## Benchmarks (Mac Studio)

Measured on the project's Mac Studio (Mac16,9, Apple M4 Max, 14 cores, 36 GB, macOS 26.6.2), release build, with `dxvkit-plus bench` on
our own plasma footage minted by `scripts/mint-bench.sh` (glenc-cli encodes). Times are per frame,
milliseconds, p50 / p99. *These are numbers from one machine, not a guarantee.*

| Codec | Size | Decode (CPU, or platform) | Decode, 1 thread (HAP) | Upload or import + convert (GPU) | End to end |
|---|---|---|---|---|---|
| HAP (Hap1) | 1920×1080 | 0.58 / 0.98 | 0.59 / 0.73 | 1.59 / 10.20 | 2.52 / 9.37 |
| HAP (Hap1) | 3840×2160 | 2.36 / 3.42 | 2.38 / 2.85 | 2.15 / 6.05 | 4.43 / 6.63 |
| HAP (Hap1), 8 chunks | 1920×1080 | 0.25 / 0.72 | 0.58 / 0.73 | 1.37 / 11.34 | 1.71 / 8.51 |
| HAP (Hap1), 8 chunks | 3840×2160 | 0.69 / 1.52 | 2.37 / 2.82 | 1.07 / 2.44 | 1.99 / 2.92 |
| HAP Alpha (Hap5), 8 chunks | 1920×1080 | 0.37 / 0.86 | 1.07 / 1.22 | 1.70 / 9.25 | 2.52 / 11.24 |
| HAP Alpha (Hap5), 8 chunks | 3840×2160 | 0.96 / 1.87 | 4.26 / 4.67 | 1.68 / 3.12 | 2.80 / 3.80 |
| HAP Q (HapY) | 1920×1080 | 0.90 / 1.52 | 0.91 / 1.10 | 1.65 / 12.82 | 2.85 / 12.48 |
| HAP Q (HapY) | 3840×2160 | 3.38 / 4.60 | 3.42 / 3.93 | 1.71 / 2.87 | 5.25 / 7.09 |
| HAP Alpha-Only (HapA) | 1920×1080 | 0.13 / 0.28 | 0.13 / 0.18 | 1.32 / 13.13 | 1.43 / 6.27 |
| HAP Alpha-Only (HapA) | 3840×2160 | 0.48 / 0.74 | 0.48 / 0.59 | 1.01 / 2.95 | 1.62 / 2.94 |
| HAP Q Alpha (HapM) | 1920×1080 | 1.04 / 1.71 | 1.04 / 1.28 | 1.73 / 11.51 | 3.63 / 7.80 |
| HAP Q Alpha (HapM) | 3840×2160 | 3.95 / 5.06 | 3.91 / 4.51 | 2.37 / 3.77 | 6.51 / 7.28 |
| LMBD (λ 1), 8 chunks, libzstd | 1920×1080 | 0.40 / 0.71 | — | 1.61 / 14.42 | 2.80 / 5.49 |
| LMBD (λ 0), 8 chunks, libzstd | 1920×1080 | 0.37 / 0.61 | — | 1.80 / 9.44 | 2.55 / 9.43 |
| LMBD (λ 0), 1 chunk, libzstd | 1920×1080 | 0.97 / 1.60 | — | 1.61 / 11.02 | 2.95 / 5.94 |
| LMBD (λ 1), 8 chunks, `ruzstd` | 1920×1080 | 1.05 / 1.36 | — | 1.95 / 11.85 | 2.98 / 6.93 |
| LMBD (λ 0), 8 chunks, `ruzstd` | 1920×1080 | 0.77 / 1.21 | — | 1.43 / 9.70 | 3.00 / 10.21 |
| LMBD (λ 0), 1 chunk, `ruzstd` | 1920×1080 | 3.81 / 4.36 | — | 1.16 / 6.29 | 5.33 / 11.11 |
| H.264 | 1920×1080 | 0.27 / 12.40 | — | 1.58 / 10.05 | 1.18 / 9.86 |
| H.264 | 3840×2160 | 1.28 / 29.07 | — | 1.34 / 12.46 | 1.68 / 31.01 |
| HEVC | 1920×1080 | 0.24 / 8.73 | — | 1.46 / 10.00 | 2.42 / 10.01 |
| HEVC | 3840×2160 | 1.20 / 23.41 | — | 0.78 / 2.33 | 0.84 / 23.22 |
| ProRes 422 | 1920×1080 | 0.95 / 43.50 | — | 1.23 / 10.75 | 1.92 / 37.21 |
| ProRes 422 | 3840×2160 | 3.14 / 42.33 | — | 0.94 / 2.03 | 3.22 / 43.70 |
| ProRes 4444 | 1920×1080 | 0.98 / 47.55 | — | 1.51 / 12.32 | 1.37 / 35.81 |
| ProRes 4444 | 3840×2160 | 3.24 / 50.88 | — | 0.96 / 2.77 | 3.21 / 49.93 |
| MJPEG | 1920×1080 | 2.23 / 13.63 | — | 1.39 / 9.88 | 1.20 / 14.67 |
| MJPEG | 3840×2160 | 8.29 / 25.09 | — | 0.97 / 2.63 | 8.14 / 20.26 |

| 16 layers of | Size | Decode all 16 (parallel) | Decode + upload + convert all 16 |
|---|---|---|---|
| HAP (Hap1) | 1920×1080 | 1.66 / 1.96 | 6.12 / 22.54 |
| HAP (Hap1) | 3840×2160 | 5.25 / 6.03 | 20.86 / 28.60 |
| HAP (Hap1), 8 chunks | 1920×1080 | 1.63 / 3.33 | 12.22 / 21.36 |
| HAP (Hap1), 8 chunks | 3840×2160 | 5.32 / 6.22 | 13.87 / 15.19 |
| HAP Alpha (Hap5), 8 chunks | 1920×1080 | 2.63 / 3.48 | 15.56 / 22.82 |
| HAP Alpha (Hap5), 8 chunks | 3840×2160 | 9.10 / 10.56 | 24.61 / 27.01 |
| HAP Q (HapY) | 1920×1080 | 2.45 / 2.97 | 13.52 / 23.00 |
| HAP Q (HapY) | 3840×2160 | 7.57 / 8.68 | 22.86 / 25.23 |
| HAP Alpha-Only (HapA) | 1920×1080 | 0.48 / 0.75 | 10.63 / 17.85 |
| HAP Alpha-Only (HapA) | 3840×2160 | 1.41 / 1.71 | 9.03 / 10.72 |
| HAP Q Alpha (HapM) | 1920×1080 | 2.63 / 3.74 | 14.08 / 24.28 |
| HAP Q Alpha (HapM) | 3840×2160 | 8.74 / 9.78 | 30.90 / 34.61 |
| LMBD (λ 1), 8 chunks, libzstd | 1920×1080 | **3.30** / 3.68 | 16.28 / 23.10 |
| LMBD (λ 0), 8 chunks, libzstd | 1920×1080 | **2.76** / 3.83 | 16.18 / 23.43 |
| LMBD (λ 1), 8 chunks, `ruzstd` | 1920×1080 | 11.30 / 12.78 | 20.94 / 32.30 |
| LMBD (λ 0), 8 chunks, `ruzstd` | 1920×1080 | 8.79 / 11.23 | 20.85 / 35.85 |

| Codec | Size | Route | Import + convert + GPU wait |
|---|---|---|---|
| H.264 | 1920×1080 | `cv_texture_cache` | 1.47 / 6.69 |
| H.264 | 1920×1080 | `iosurface` | 1.17 / 6.93 |
| H.264 | 3840×2160 | `cv_texture_cache` | 1.23 / 10.85 |
| H.264 | 3840×2160 | `iosurface` | 1.80 / 8.43 |
| HEVC | 1920×1080 | `cv_texture_cache` | 1.26 / 8.19 |
| HEVC | 1920×1080 | `iosurface` | 1.51 / 10.47 |
| HEVC | 3840×2160 | `cv_texture_cache` | 0.68 / 1.99 |
| HEVC | 3840×2160 | `iosurface` | 0.73 / 1.89 |

How to read these: *Decode* is reading + decompressing one frame on the CPU (HAP; chunked frames
decompress on worker threads) or the platform decoder delivering one frame (native). *Upload or
import + convert* is the GPU side for a frame already decoded — HAP blocks written into BC
textures, native frames imported zero-copy — plus the conversion pass and a full GPU wait. *End to
end* is both, per frame, sequentially. The p99 columns include one-off costs (a native reader
restarting at frame 0 at the start of each pass, which is a seek; GPU clock ramp-up); a real
player overlaps decode and GPU work and keeps frames in the read-ahead cache. The layer table runs
16 independent decoders of the same clip (one thread each) plus 16 uploads + conversions on one
device per step. Raw JSON lines: `docs/data/bench-macstudio.jsonl` (LMBD:
`docs/data/bench-lmbd-zstd-backends-macstudio.jsonl`, both backends, two interleaved rounds — the
tables show round 1; 0.2.0's `ruzstd`-only run is `docs/data/bench-lmbd-macstudio.jsonl`; the LMBD
clips are our CC0 plasma, 1920×1080, encoded by `dxvkit-plus encode-lmbd`).

LMBD's CPU side is Zstd decompression. Per thread, on the same chunks, libzstd runs at 968 MB/s on
the λ 1 clip where `ruzstd` runs at 122 MB/s (λ 0: 2119 vs 446 MB/s). With libzstd, the default
since 0.2.1, decoding 16 layers of 1080p takes 3.30 ms at λ 1 (3.37 ms in the second round) and
2.76 ms at λ 0 (2.93 ms), against 11.30 ms and 8.79 ms with `ruzstd`. That is about 10 GB/s of BC7 across the 14
cores — the libzstd rate on the 10 performance cores, with the 4 efficiency cores adding less — so
λ 1 lands a little above the 2-3 ms the per-thread rates suggested. At 16 layers the step was then
dominated by the 16 uploads and conversions, not by decode — which 0.3.0 addresses (below).


### Getting frames onto the GPU (0.3.0)

16 (or, at 4K, 4 and 8) independent decoders of one clip, all on one device — `dxvkit-plus bench
<clip> --layers N --layers-only --variants ...`, Mac Studio, release, two interleaved rounds (both
shown, `round 1 · round 2`). *Before* is the v0.2.1 binary; the other columns are 0.3.0 paths:

- **staged** — persistent staging + one submission, waiting for the GPU every step (step mode);
- **pipelined** — staged, decoding ahead on per-layer threads, 2 steps in flight (sustained);
- **+ direct** — as pipelined, sampling plain-BC frames directly (no conversion pass; HAP Q and the
  other YCoCg / alpha-only / HDR variants still convert, so their numbers equal pipelined);
- **into + direct** — decoding straight into staging memory, 2 steps in flight (lowest latency).

Steps per second (sustained; for the step modes 1000 / mean step):

| Clip | Layers | Before (0.2.1) | staged | pipelined | pipelined + direct | into + direct |
|---|---|---|---|---|---|---|
| LMBD λ 1, 8 chunks, 1080p | 16 | 132 · 133 | 154 · 159 | 259 · 276 | **463 · 516** | 336 · 330 |
| LMBD λ 0, 8 chunks, 1080p | 16 | 141 · 137 | 160 · 160 | 264 · 266 | **533 · 471** | 402 · 362 |
| HAP (Hap1), 8 chunks, 1080p | 16 | 202 · 163 | 203 · 191 | 293 · 272 | **939 · 692** | 620 · 470 |
| HAP Q (HapY), 1080p | 16 | 139 · 116 | 173 · 141 | **261 · 273** | 274 · 259 | 254 · 259 |
| LMBD λ 0, 8 chunks, 4K | 4 | 111 · 107 | 145 · 140 | **224 · 221** | 221 · 223 | 203 · 209 |
| LMBD λ 0, 8 chunks, 4K | 8 | 77 · 79 | 104 · 101 | 211 · 201 | **212 · 206** | 179 · 180 |
| HAP (Hap1), 8 chunks, 4K | 4 | 187 · 191 | 211 · 205 | 405 · 396 | **419 · 415** | 360 · 353 |
| HAP (Hap1), 8 chunks, 4K | 8 | 133 · 134 | 173 · 165 | 326 · 332 | **388 · 396** | 304 · 306 |
| HAP Q (HapY), 4K | 4 | 127 · 130 | 166 · 165 | **293 · 285** | 289 · 286 | 235 · 229 |
| HAP Q (HapY), 4K | 8 | 85 · 89 | 119 · 116 | **257 · 255** | 254 · 236 | 191 · 192 |

Latency, p50 ms, from the first layer's decode starting to that step's textures being ready on
the GPU (for the step modes: the step time):

| Clip | Layers | Before (0.2.1) | staged | pipelined | pipelined + direct | into + direct |
|---|---|---|---|---|---|---|
| LMBD λ 1, 1080p | 16 | 7.50 · 7.69 | 6.59 · 6.26 | 22.7 · 22.0 | 10.7 · 9.7 | **3.56 · 3.54** |
| HAP (Hap1), 1080p | 16 | 5.00 · 6.07 | 4.77 · 4.97 | 19.8 · 21.5 | 5.3 · 8.1 | **2.11 · 2.77** |
| HAP Q (HapY), 1080p | 16 | 7.20 · 8.58 | **5.80 · 6.82** | 22.4 · 22.1 | 21.9 · 22.9 | 11.4 · 11.0 |
| LMBD λ 0, 4K | 8 | 13.0 · 12.6 | 9.98 · 9.84 | 13.5 · 18.8 | 9.7 · 14.7 | **6.41 · 6.29** |

Where the 16-layer 1080p LMBD step went (p50 ms; CPU wall clocks per phase, GPU from Metal System
Trace): before, decode 2.7-2.8 · `write_texture` 3.3-3.5 (2.8 of it wgpu allocating a new staging
buffer per call) · encode + submit 0.5 · GPU wait 0.6; GPU work 1.68 ms per step (blits 1.16,
conversions ~0.5). After (pipelined + direct): the main thread copies into persistent staging
(0.9-2.4) and submits (0.07), decoders run ahead (wait 0.03), and the GPU does 0.32 ms per step
(copies only). Decoding into staging takes the copy off the main thread altogether (0.008 ms).

*Caveat:* Resolume Arena was running on the Mac Studio throughout and held 66-95% of the GPU
(Metal System Trace, `docs/data/w3-metal-system-trace-summary.jsonl`), so the paths that still
convert are contending for a mostly busy GPU; the numbers compare like with like (interleaved), but
the absolutes are lower than on an idle machine, and the laptops (more GPU cores) will differ. Raw
data: `docs/data/w3-layers-macstudio.jsonl` (final rounds), `w3-candidates-ab-macstudio.jsonl`
(every candidate's A/B), `w3-upload-microbench.txt`.

### DXV, 16 layers of 1080p (0.4.0)

The same bench on the four DXV variants: 60-frame 1920×1080 plasma clips encoded by glenc-cli
(`scripts/mint-bench.sh`; DXT5 and YG10 from the plasma with an alpha ramp), straight alpha, Mac
Studio, release, two interleaved rounds (`round 1 · round 2`). DXT1 and DXT5 sample directly in
the `+ direct` columns; DXV High Quality always runs its integer conversion pass.

Steps per second:

| Clip | per layer | staged | pipelined | pipelined + direct | into + direct |
|---|---|---|---|---|---|
| DXV DXT1 | 235 · 202 | 305 · 296 | 588 · 615 | **832 · 722** | 602 · 534 |
| DXV DXT5 (alpha) | 128 · 107 | 186 · 179 | 331 · 290 | **358 · 279** | 277 · 239 |
| DXV YCG6 (High Quality) | 152 · 144 | 173 · 176 | 382 · 355 | **429 · 392** | 281 · 297 |
| DXV YG10 (High Quality, alpha) | 95 · 100 | 131 · 125 | 246 · 258 | **269 · 270** | 163 · 198 |

Latency, p50 ms (decode start to textures ready; step modes: the step):

| Clip | per layer | staged | pipelined | pipelined + direct | into + direct |
|---|---|---|---|---|---|
| DXV DXT1 | 4.24 · 4.60 | 3.27 · 3.39 | 8.57 · 9.13 | 6.22 · 7.34 | **2.67 · 3.08** |
| DXV DXT5 | 7.75 · 8.51 | 5.37 · 5.59 | 15.5 · 17.9 | 13.3 · 17.1 | **4.27 · 6.78** |
| DXV YCG6 | 6.52 · 6.97 | 5.74 · 5.69 | 13.4 · 14.0 | 13.1 · 13.8 | **5.97 · 5.62** |
| DXV YG10 | 10.4 · 10.0 | 7.66 · 8.00 | 19.9 · 19.7 | 18.5 · 19.2 | **9.98 · 8.09** |

GlEnc's DXV is large (48-146 MB for 60 frames: its High Quality streams use only the stored and fill
opcode codings), so these clips are heavier to read and copy than Resolume Alley's; decode of all
16 layers takes 1.6 ms (DXT1) per step. Resolume Arena was running (load 12-16), as in 0.3.0. Raw
data: `docs/data/w4-dxv-layers-macstudio.jsonl`.

## Testing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo deny check licenses bans sources
# the same gates on the pure-Rust Zstd backend
cargo clippy --workspace --all-targets --no-default-features --features zstd-rust -- -D warnings
cargo nextest run --workspace --no-default-features --features zstd-rust
cargo deny --no-default-features --features zstd-rust check licenses bans sources
cargo run --release -p dxvkit-plus-core --example fuzz -- 600 8
cargo run --release -p dxvkit-plus-core --example fuzz -- 600 8 lmbd   # the LMBD reader alone (decode_frame and decode_frame_into)
cargo run --release -p dxvkit-plus-core --example fuzz -- 600 8 dxv    # the DXV decoder (packets and movies)
# DXV against DXVKit itself: its full 334-row vector set and the live C-core differential
DXVKIT_DIR=<dxvkit checkout> DXVTOOL=<dxvkit checkout>/build/dxvtool cargo nextest run -p dxvkit-plus-core --test dxv
```

All test media are our own procedural CC0 fixtures (`tests/fixtures`, each with an `.attribution`
sidecar), plus the MIT subset of DXVKit's test vectors (`tests/fixtures/dxvkit`, synthetic content
only). Builds and tests run on the Mac Studio; there is no remote CI yet.

## Roadmap

- **LMBD** — a faster GPU (compute) BC7 encoder for LMB7; GPU-side LMBX encoding.
- **Windows** — Media Foundation decode and DX12 shared-handle zero-copy import behind the existing
  `Backend` trait.
- **Encoders** — HAP and DXV encode (LMBD encode ships in 0.2.0). (HAP Q encoding will be written from the YCoCg-DXT paper the
  HAP spec cites, never from `hap-qt-codec`'s LGPL-2.1 YCoCg encoder.)
- **C ABI** — a C wrapper over the public API (already kept C-wrappable; see `docs/ARCHITECTURE.md`).
- **crates.io** — publishing the crates.

## Licence

MIT — see [`LICENSE`](LICENSE). Adapted code and assets are listed in [`ATTRIBUTION.md`](ATTRIBUTION.md).
