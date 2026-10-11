# Architecture

```
      file.mov / file.mp4 / file.lmbd / PNG folder
                         │
              dxvkit-plus-core::source::probe ──► MediaInfo (codec, size, frames, exact rate, tmcd start)
                         │
     ┌───────────────────┼────────────────────────────┐
     │ HAP, LMBD         │ native codecs              │ PNG stills / sequences
     │ container demux / │ Backend trait              │ png crate
     │ lmbd::LmbdFile    │  macOS: dxvkit-plus-apple  │
     │ → hap::decode,    │                            │
     │   lmbd chunks     │                            │
     │ → BC block planes │  Windows: planned stub     │ → RGBA8 plane
     │   (Decoded::Planes)│ → Decoded::Native         │   (Decoded::Planes)
     └─────────┬─────────┴──────────────┬─────────────┘
               │ cache::Player (read-ahead workers, exact frame on a miss)
               │ transport::Transport (which frame, at exact integer time)
               ▼                         ▼
   dxvkit-plus-wgpu::FrameRenderer   dxvkit-plus-apple::surface::import (zero-copy)
   write_bc → BC texture             IOSurface plane(s) → MTLTexture → wgpu::Texture
               └──────────┬──────────────┘
                 WGSL CONVERT pass → Rgba8Unorm at clip size
```

## Crates

| Crate | `unsafe` | GPU | What |
|---|---|---|---|
| `dxvkit-plus-core` | forbidden | none | demux, HAP, CPU block decoders, sources, cache, transport, time, the `Backend` trait |
| `dxvkit-plus-lmbd-enc` | forbidden | none | the LMBD encoder (4-mode BC7 + RDO, Zstd); links libzstd through the `zstd` crate |
| `dxvkit-plus-wgpu` | forbidden | wgpu 30 | BC textures, the WGSL conversion, read-back, `LmbdTexture` |
| `dxvkit-plus-apple` | **allowed** (every block has a `// SAFETY:` comment) | wgpu-hal Metal | AVFoundation / VideoToolbox decode, IOSurface → wgpu import |
| `dxvkit-plus-cli` | forbidden | wgpu | the `dxvkit-plus` command |

The core never depends on a GPU API: its output is a description of GPU-ready data
(`source::PixelFormat`, `source::Shading`) that any renderer can upload. The platform crate is
the only place that knows objc2, CoreVideo or Metal types; callers see `NativeFrame` with an
opaque handle.

## Decode paths

**HAP.** `container::Movie` resolves sample offsets from `stsz` + `stsc` + `stco`/`co64` (with hard
caps: 64 MB `moov`, 2^24 samples, 300 MB per sample). `hap::decode` walks the section tree (4- or
8-byte headers), decompresses Snappy or the chunk table (chunks decompress in parallel when asked),
and returns each image's raw block stream, exactly `ceil(w/4) * ceil(h/4) * block_bytes` long. The
blocks upload unchanged (`write_bc`); the GPU decodes them. No CPU pixel work on the playback path.

**LMBD.** `lmbd::LmbdFile` reads and validates the 40-byte header and the tail index (every entry
inside the file), then reads a frame's payload with one positioned read. The chunk table is checked
(profile echo, chunks inside the payload, whole 16-byte blocks adding up to exactly
`ceil(w/4) * ceil(h/4) * 16`) before the frame buffer is allocated; each chunk decompresses into its
own slice (Zstd through `ruzstd`, which must produce exactly the declared size), on worker threads
when asked. The block stream uploads unchanged into a BC7 texture; the GPU decodes it on sampling
(`LmbdTexture` adds an sRGB view). The format is in `docs/LMBD.md`.

**Native.** `NativeReader` (macOS) opens the first video track with `AVURLAsset`
(`AVURLAssetPreferPreciseDurationAndTimingKey`) and an `AVAssetReaderTrackOutput` asking for
IOSurface-backed, Metal-compatible pixel buffers:

- `'420v'` (two planes: R8 luma, RG8 chroma; converted in WGSL with the frame's BT.601 / 709 / 2020
  matrix) for 4:2:0 codecs without alpha — H.264, HEVC;
- `'BGRA'` (one plane, converted inside VideoToolbox) for everything else — ProRes 422 and 4444
  (alpha), MJPEG, unknown files — so no 4:2:2 / 4:4:4 chroma is thrown away (measured: ProRes 422
  forced through 4:2:0 lands 13.4 mean RGB error from the source vs 10.8 as BGRA).

Frame `i` is the sample presented at `start + i * minFrameDuration` (exact rationals). Sequential
reads reuse the reader; any other request restarts it one frame early and skips to the target's
presentation time, so seeks are exact for intra and long-GOP codecs alike (verified: shuffled-order
decodes are identical to sequential ones, every frame, every fixture).

**Zero-copy import.** The pixel buffer's IOSurface planes become `MTLTexture`s
(through a `CVMetalTextureCache`, or directly with `newTextureWithDescriptor:iosurface:plane:`), adopted by `wgpu_hal::metal::Device::texture_from_raw`
and registered with `wgpu::Device::create_texture_from_hal`. No pixel is copied or blitted between
decoder and conversion pass: **0 copies, 0 blits, 0 bytes uploaded** per native frame (the test
suite checks that the wgpu texture aliases the frame's IOSurface id and that nothing was written
with `write_texture`). The only GPU work is the conversion pass itself. The wgpu texture's drop
callback holds the `CVPixelBuffer` so the decoder's pool cannot recycle the surface while the GPU
may still read it.

### Import route: IOSurface vs `CVMetalTextureCache`

Cutlass imports through `CVMetalTextureCacheCreateTextureFromImage`. We implemented that route too
(`surface::TextureCacheImporter`) and measured both on the same decoded frames
(`dxvkit-plus bench --import`, import + convert + GPU wait per frame, release build, Mac Studio):

| Clip | `iosurface` p50 / p99 | `cv_texture_cache` p50 / p99 |
|---|---|---|
| H.264 1920×1080 | 1.36 / 7.87 | 1.37 / 9.06 |
| H.264 3840×2160 | 1.65 / 9.34 | 1.27 / 10.51 |
| HEVC 1920×1080 | 1.36 / 10.25 | 1.05 / 6.87 |
| HEVC 3840×2160 | 1.33 / 6.60 | 1.16 / 6.73 |
| **mean of all** | **1.43 / 8.52** | **1.21 / 8.29** |

Milliseconds, mean of three runs (each 30 frames × 5 passes; raw lines in `docs/data/`). The cache route had the lower p50 on three of the four clips (tie on 1080p H.264), and equal p99, so it is the default (`surface::TextureCacheImporter`); the IOSurface route (`surface::import`) needs no per-device object and remains available. The difference is small (~0.2 ms p50): both are zero-copy, and most of the time is the conversion pass and the GPU round-trip.

Both routes produce identical pixels (tested on every native fixture frame).

## Accuracy contracts

| Path | Contract | Test |
|---|---|---|
| HAP Hap1 / Hap5 / HapA (CPU) | exact to the source pattern (0/255 measured; 1/255 allowed) | `core/tests/fixtures.rs` |
| HAP Q HapY / HapM (CPU) | ≤ 2/255 to the source (chroma is lossy by design) | same |
| GPU vs CPU, every HAP fixture frame | ≤ 1/255 (BC decoders may round differently by 1) | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| BC7 CPU decoder vs the GPU | bit-exact, 4096 random blocks per mode, all 8 modes | `wgpu/tests/gpu.rs` |
| LMBD files written by the original encoder (CPU) | bit-exact with the original decoder (per-frame RGBA SHA-256) | `core/tests/lmbd_vectors.rs` |
| LMBD GPU vs CPU | bit-exact, every vector frame, frame renderer and `LmbdTexture`; sRGB view within 1/255 of linear light | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| LMBD encoder vs the original encoder | byte-identical files (five vectors; 1080p frames checked by hand) | `lmbd-enc/tests/encoder_identity.rs` |
| Native BGRA (ProRes 4444) zero-copy vs Apple's own CPU read-back | bit-exact | `apple/tests/native.rs` |
| Native 4:2:0 WGSL conversion vs Apple's conversion | tile interiors mean ≤ 1/255; whole frame no further from the source than Apple's | same |
| Frame identity + seek | every frame carries its index; shuffled seeks equal sequential decodes | `apple/tests/native.rs`, `dxvkit-plus verify` |

## Transport and cache

`transport::Transport` holds the playhead as an exact integer `Tick` (254 016 000 000 per second,
which divides every broadcast frame rate and audio rate), integrated from the host clock times a
rational speed, so frame `k` starts exactly at `rate.tick_of(k)` — no float drift (a 29.97 clip on
a 59.94 clock is still exact after 10 minutes). `cache::Player` keeps an LRU of decoded frames and
decodes ahead on worker threads in playback order; a miss decodes the exact frame on the caller's
thread, never "the nearest one ready".

## C ABI readiness (the C wrapper is planned)

The public API is kept wrappable by a thin `extern "C"` crate without redesign:

| Rust API | C mapping |
|---|---|
| `Box<dyn FrameSource>`, `cache::Player`, `transport::Transport`, `FrameRenderer` | opaque handles (`*mut T` created and freed by the wrapper) |
| `MediaInfo`, `NativeInfo`, `transport::Settings`, `transport::Frame`, `TimecodeStart` | plain-data `#[repr(C)]` mirrors (numbers, bools, one path string) |
| `PixelFormat`, `SourceKind`, `YuvMatrix`, `Codec`, `HapTexture`, `PlayMode`, `Direction`, `SyncMode`, `lmbd::{Profile, TexFormat, Compressor, Colorspace}` | `#[repr(u32)]` with fixed values (pinned by `core/tests/abi.rs`; the LMBD ones equal the bytes in the file) |
| `lmbd::LmbdFile`, `lmbd::Writer`, `LmbdTexture` | opaque handles |
| `lmbd::Header`, `IndexEntry`, `ChunkEntry` | plain-data `#[repr(C)]` mirrors |
| `lmbd_enc::EncodeParams` (`Bc7Params`, `RdoParams`) | a plain-data `#[repr(C)]` mirror (numbers and bools); encoding maps to `encode_frame` (RGBA8 buffer in, payload out) + `Writer::push_payload` / `finish` — no callbacks (`encode_frames` is a Rust convenience over those) |
| `PixelLayout`, `Shading` (enums with a field) | a `u32` tag + the field |
| `Decoded::Planes` | count + per-plane (format, width, height, pointer, length) borrowed from the handle |
| `Decoded::Native` | the platform handle (`CVPixelBufferRef` on macOS), retained for the caller |
| `MediaError` | `MediaError::code()` (stable `u32`, pinned; `Lmbd` = 11) + the message through a separate call |
| `Arc<dyn Backend>` | chosen by the wrapper per platform (no callbacks across the boundary) |

Rules that keep it that way: no generics or lifetimes in the public decode / transport API, no
callbacks, errors as values, every fieldless enum `#[repr(u32)]` with explicit values, and new
variants only appended.

## Windows (planned)

`backend::windows::MediaFoundationBackend` documents the design — `IMFSourceReader` with a D3D
device manager into NV12 / BGRA textures, the same seek rule, and DX12 shared handles
(`CreateSharedHandle` → `OpenSharedHandle` → `wgpu_hal::dx12::Device::texture_from_raw`) for the
zero-copy import — and returns `Unsupported` until it is built.
