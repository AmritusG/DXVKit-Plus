# Architecture

```
      file.mov / file.mp4 / file.lmbd / PNG folder
                         │
              dxvkit-plus-core::source::probe ──► MediaInfo (codec, size, frames, exact rate, tmcd start)
                         │
     ┌───────────────────┼────────────────────────────┐
     │ HAP, DXV, LMBD    │ native codecs              │ PNG stills / sequences
     │ container demux / │ Backend trait              │ png crate
     │ lmbd::LmbdFile    │  macOS: dxvkit-plus-apple  │
     │ → hap::decode,    │                            │
     │   dxv::decode,    │                            │
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
| `dxvkit-plus-core` | forbidden | none | demux, HAP, DXV, the LMBD container (LMB7 and LMBX), CPU block decoders, sources, cache, transport, time, the `Backend` trait; links libzstd for LMBD decode by default (`zstd-c`), or pure Rust (`zstd-rust`) |
| `dxvkit-plus-lmbd-enc` | forbidden | none | the LMBD encoders: LMB7 (4-mode BC7 + RDO, Zstd; links libzstd through the `zstd` crate) and LMBX (DCT + RDOQ + rANS, rate control) |
| `dxvkit-plus-wgpu` | forbidden | wgpu 30 | BC textures, staged uploads (`Uploader`, `StagedFrame`), the WGSL conversion and direct sampling (`DirectView`), read-back, `LmbdTexture` |
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

**DXV.** A safe-Rust port of DXVKit's C decoder (`core::dxv`, carried over from Loombda-rs, where
it was ported line for line from DXVKit 1.0.0; DXVKit is itself written from its own
`DXV_FORMAT.md`). The packet decides the variant, never the FourCC: DXV3 `DXT1` (BC1), `DXT5`
(BC3), `YCG6` (Y BC4 + CoCg BC5 at half size) and `YG10` (Y + A BC5 + CoCg BC5), each through
DXV3's block-copy layers (DXT1 / DXT5: 2-bit flags, back references, DXT5 alpha commands) or the HQ
opcode streams (stored, fill, or FSE entropy-coded opcodes; the two sections decode on two
threads when asked), or a stored payload; and legacy `DXDI` (DXV1 / 2: LZF DXT1 / DXT5, or stored).
Output is the normative block streams of the whole coded texture (display size rounded up to 16).
DXT1 / DXT5 upload as BC textures and sample as stored (`direct_view` qualifies). DXV High
Quality uploads the same bytes as integer texels — one `Rg32Uint` / `Rgba32Uint` texel per BC4 /
BC5 block — and its own WGSL pass (`DXV_HQ_WGSL`) decodes the BC4 palettes exactly and converts
YCoCg with DXVKit's spec 8.4 integer arithmetic (nearest chroma, midpoint 127.5, out-of-gamut
folding), because the fixed-function BC4 decode rounds the palette and the folding amplifies it
(up to 5/255 on Apple silicon). Alpha: DXV carries no straight / premultiplied flag, so it is a
per-clip `AlphaMode` (`ImportOptions::alpha_mode`): `Straight` (default, Resolume Alley's
convention) or `Premultiplied`, which un-premultiplies on output in integers
(`bc::unpremul`, the same arithmetic on CPU and GPU).

**LMBD.** `lmbd::LmbdFile` reads and validates the 40-byte header and the tail index (every entry
inside the file), then reads a frame's payload with one positioned read. The chunk table is checked
(profile echo, chunks inside the payload, whole 16-byte blocks adding up to exactly
`ceil(w/4) * ceil(h/4) * 16`) before the frame buffer is allocated; each chunk decompresses into its
own slice (Zstd through the compiled-in backend, below; it must produce exactly the declared
size), on worker threads when asked. The block stream uploads unchanged into a BC7 texture; the GPU decodes it on sampling
(`LmbdTexture` adds an sRGB view). The format is in `docs/LMBD.md`.

**LMBX** (LMBD profile `0x10`, `core::lmbx`, spec in `docs/LMBD.md` §11). The container is LMBD's;
a frame's chunks hold its LMBX bitstream (Zstandard-compressed when smaller, 0.6.0). The source
validates the bitstream's header, tables, tile mode map (v2) and stream directory on the CPU and
hands the bytes over unchanged as one `PixelFormat::LmbxBitstream` plane (`Shading::Lmbx`); nothing
else happens on the CPU. `FrameRenderer` uploads the bytes into a storage buffer (`write_buffer`,
or through the staging belt) and decodes them with compute passes: `tables` (a workgroup per rANS
table — coefficients, and in v2 palette indices — plus, in v2, one that turns the directory's
stream lengths into offsets), `entropy` (one invocation per stream = per plane of each 32x32 tile:
rANS, dequantise, write coefficients in place; a v2 flat or palette tile is decoded whole by its
stream-0 invocation into the same sample planes), `idct` (one invocation per 8x8 block of a DCT
tile: the integer inverse DCT as generated straight-line WGSL), then a fragment pass (4:2:2 halo or
4:4:4 chroma, inverse YCoCg-R, direct tiles' RGBA8, 8-bit output). The arithmetic is the reference decoder's, so GPU and
CPU agree bit for bit on every frame, hostile ones included. LMBX frames vary in size, so they have
no fixed `FrameSource::layout` (no decode-into-staging, no direct view). The encoder is
`lmbd_enc::lmbx` (B1d's RDOQ, with rate control; v2 adds a rate-distortion choice per tile between
the DCT and flat or palette coding, lossy palettes included; 0.6.1 adds the exact forward YCoCg-R
and a colour guard that holds flat regions within a tolerance — encoder only, `docs/LMBD.md`
§11.11).

*Zstd backend.* One private function, `lmbd::zstd_decompress(data, out) -> bytes written`, sits
behind two core features. `zstd-c` (default) calls libzstd's `ZSTD_decompressDCtx` through the
`zstd` crate's `bulk::decompress_to_buffer`, straight into the chunk's slice of the frame buffer:
the raw size is known from the chunk table, so there is no growth and no copy, and output larger
than the slice is an error. `zstd-rust` calls `ruzstd`'s `FrameDecoder::decode_all` into the same
slice — the pure-Rust fallback, for a core with no C in it. Both accept one or more Zstd frames per
chunk, neither reads legacy (pre-v0.8) frames, and both map every failure to `MediaError::Lmbd`.
If both features are on, `zstd-c` wins; if neither is, `compile_error!`. Workspace crates depend on
the core with default features off and forward the two features (`zstd-c` by default), so one
`--no-default-features --features zstd-rust` switches the whole build. The core's
`#![forbid(unsafe_code)]` covers this crate's code, not its dependencies: with `zstd-c` the unsafe
FFI lives in `zstd-safe` / `zstd-sys`, as the encoder's always has. libzstd is about 5x (λ 0) to 8x
(λ 1) faster per thread on LMBD chunks, which is what moves 16 layers of 1080p from decode-bound to
upload-bound (README benchmarks).

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
| GPU vs CPU, every HAP fixture frame | ≤ 1/255 (BC decoders may round differently by 1); HAP Q ≤ 2/255 | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| DXV decode (CPU) vs DXVKit's test vectors | bit-exact: 334 rows (366 block streams, 287 RGBA, 47 rejects) with `DXVKIT_DIR`; the committed 74-row MIT subset always | `core/tests/dxv.rs` |
| DXV decode vs DXVKit's C core (`dxvtool`) | bit-exact block streams, straight and premultiplied RGBA on every frame of the DXV fixtures and committed vectors (with `DXVTOOL`) | same |
| DXV GPU vs CPU | High Quality (YCG6 / YG10) bit-exact, both alpha modes, every vector frame; DXT1 / DXT5 ≤ 2/255 (the GPU's fixed-function BC1 / BC3 vs DXVKit's truncating convention, as Arena's GPU upload, DXVKit spec 8.1); premultiplied DXT5 ≤ 2/255 as shown | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| BC7 CPU decoder vs the GPU | bit-exact, 4096 random blocks per mode, all 8 modes | `wgpu/tests/gpu.rs` |
| LMBD files written by the original encoder (CPU) | bit-exact with the original decoder (per-frame RGBA SHA-256) | `core/tests/lmbd_vectors.rs` |
| LMBD GPU vs CPU | bit-exact, every vector frame, frame renderer and `LmbdTexture`; sRGB view within 1/255 of linear light | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| LMBD encoder vs the original encoder | byte-identical files (five vectors; 1080p frames checked by hand) | `lmbd-enc/tests/encoder_identity.rs` |
| LMBX reference decoder vs the conformance vectors | every frame's normative output hashes to its sidecar (bitstreams v1 and v2, every tile mode, both chroma modes, alpha, odd sizes) | `core/tests/lmbx.rs` |
| LMBX GPU vs CPU | bit-exact: every vector frame (`write_buffer` and staged uploads) and hostile frames (random, empty, swapped streams; v2: random tile modes) | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| LMBX encoder | deterministic: the vectors' patterns re-encode identically at 1, 3 and 8 threads, pinned by `reencoded_sha256` | `lmbd-enc/tests/lmbx_identity.rs` |
| LMBX encoder colour accuracy | flat colours exact through the DCT path at quality 1; every flat region of the graphics card within 1 code value at a coarse quality (v2, both chroma modes, alpha) | `lmbd-enc/src/lmbx.rs` tests |
| Native BGRA (ProRes 4444) zero-copy vs Apple's own CPU read-back | bit-exact | `apple/tests/native.rs` |
| Native 4:2:0 WGSL conversion vs Apple's conversion | tile interiors mean ≤ 1/255; whole frame no further from the source than Apple's | same |
| Frame identity + seek | every frame carries its index; shuffled seeks equal sequential decodes | `apple/tests/native.rs`, `dxvkit-plus verify` |

## Upload paths (0.3.0)

Measured first (W3, `docs/data/w3-*`): on wgpu 30's Metal backend, `Queue::write_texture` creates a
new `MAP_WRITE | COPY_SRC` staging `MTLBuffer` (shared storage, write-combined) for every call
(wgpu-core `StagingBuffer::new`), copies the bytes in, and records a blit into the texture (BC
textures are always `Private`) on an internal command buffer submitted before the next submission.
For 16 1080p BC7 frames that is 3.45 ms of CPU, of which a plain copy into an already-mapped buffer
is 0.62; the allocation is the rest. The GPU side was never the limit (1.2-1.7 ms of GPU work per
16-layer step, the GPU otherwise idle while the CPU staged). So:

- **Persistent staging.** `FrameRenderer::upload_staged` copies a frame's planes into a
  `wgpu::util::StagingBelt` (mapped chunks reused across frames, remapped once the GPU is done) and
  records `copy_buffer_to_texture` into the caller's encoder; rows are padded to wgpu's 256-byte
  copy pitch only when needed (1080p and 4K BC rows never are). `FrameRenderer::upload` +
  `encode_convert` split `render` so many layers share one submission. Batching alone measured worse
  (the GPU work all moves to the end of the step) and only ships with the belt.
- **Pipelining.** `Uploader` owns the belt and the submissions in flight: `submit` finishes the
  belt, submits, recalls it and blocks only while more than `depth` (2) submissions are running —
  a wait on one submission index, never `wait_idle`. Decoding the next step overlaps the GPU's work
  on this one; textures need no double-buffering because wgpu orders a later copy into a texture
  after the earlier passes that sample it on the same queue. Depth 3 measured no better than 2 and
  adds latency.
- **Decode into staging.** `FrameSource::layout` gives a block source's fixed frame shape (LMBD;
  HAP by FourCC — HAP HDR excluded, its signedness is per frame) and `FrameSource::decode_into`
  writes a frame into any `PlaneSink`. `Uploader::stage` reserves a `StagedFrame` (mapped staging
  per plane, `Send`) that is a `PlaneSink`; LMBD streams it chunk by chunk through a reused,
  cache-sized buffer, other sources decode and write whole planes. wgpu exposes mapped memory only
  as `WriteOnly` (it may be write-combining; an LZ decoder must not read back from it), which is why
  the chunks decompress into a small buffer first. `upload_from_stage` records the copies; `submit`
  refuses while a staged frame is still open (unmapping under a live view would panic inside wgpu).
  The main thread no longer copies anything; latency drops by half or more, throughput is lower than
  decode-ahead because staging cannot run ahead of the belt.
- **Direct sampling.** `FrameRenderer::direct_view` returns the uploaded block texture for frames
  that sample as stored (LMB7, HAP R BC7, HAP RGBA BC3, HAP RGB BC1 with `opaque`): sampling it gives
  the conversion pass's output exactly (alpha taken as 1 when `opaque`, since BC1's punch-through
  texels read as alpha 0), so a compositor can skip the pass; it doubled 16-layer throughput.
- **Ruled out, with evidence.** Zero-copy into the texture: Metal's buffer-backed (linear) textures
  do not accept compressed pixel formats. Shared-storage BC7 textures filled with `replaceRegion`
  (accepted on Apple silicon): 1.78 ms of CPU for 16 frames against 0.54 ms for the staging copy —
  the driver lays the blocks out for the GPU on the CPU — to save a 0.3 ms GPU blit, and it cannot
  take `decode_into`. No unsafe was added; the native path is unchanged (0 copies, 0 blits).

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
| `PixelFormat`, `SourceKind`, `YuvMatrix`, `Codec`, `HapTexture`, `AlphaMode`, `dxv::Variant`, `PlayMode`, `Direction`, `SyncMode`, `lmbd::{Profile, TexFormat, Compressor, Colorspace}`, `lmbx::Chroma` | `#[repr(u32)]` with fixed values (pinned by `core/tests/abi.rs`; the LMBD ones equal the bytes in the file) |
| `lmbd::LmbdFile`, `lmbd::Writer`, `LmbdTexture` | opaque handles |
| `Uploader`, `StagedFrame` | opaque handles; `decode_into` takes a source handle + a `StagedFrame` handle (the `PlaneSink` is implemented on the Rust side — no C callback) |
| `source::FrameLayout`, `PlaneLayout`, `DirectView` | plain-data `#[repr(C)]` mirrors (`DirectView`'s view as a borrowed handle, valid until the next upload) |
| `GpuError` | `GpuError::code()` (stable `u32`, pinned by `wgpu/tests/abi.rs`) + the message through a separate call |
| `lmbd::Header`, `IndexEntry`, `ChunkEntry`, `lmbx::FrameHeader`, `lmbx::Sections` | plain-data `#[repr(C)]` mirrors |
| `lmbd_enc::lmbx::LmbxParams` | a plain-data `#[repr(C)]` mirror; `encode_frame` / `encode_frame_to_size` take an RGBA8 buffer and return the frame bitstream |
| `lmbd_enc::EncodeParams` (`Bc7Params`, `RdoParams`) | a plain-data `#[repr(C)]` mirror (numbers and bools); encoding maps to `encode_frame` (RGBA8 buffer in, payload out) + `Writer::push_payload` / `finish` — no callbacks (`encode_frames` is a Rust convenience over those) |
| `PixelLayout`, `Shading` (enums with a field) | a `u32` tag + the field |
| `Decoded::Planes` | count + per-plane (format, width, height, pointer, length) borrowed from the handle |
| `Decoded::Native` | the platform handle (`CVPixelBufferRef` on macOS), retained for the caller |
| `MediaError` | `MediaError::code()` (stable `u32`, pinned; `Lmbd` = 11, `Dxv` = 12) + the message through a separate call |
| `ImportOptions` | a plain-data mirror: the sequence rate, `alpha_mode` (`AlphaMode` as `u32`); the backend chosen by the wrapper |
| `dxv::Layout`, `dxv::Header`, `dxv::Blocks` | plain-data `#[repr(C)]` mirrors (`Blocks`' planes as pointer + length borrowed from the handle) |
| `Arc<dyn Backend>` | chosen by the wrapper per platform (no callbacks across the boundary) |

Rules that keep it that way: no generics or lifetimes in the public decode / transport API, no
callbacks, errors as values, every fieldless enum `#[repr(u32)]` with explicit values, and new
variants only appended.

## Windows (planned)

`backend::windows::MediaFoundationBackend` documents the design — `IMFSourceReader` with a D3D
device manager into NV12 / BGRA textures, the same seek rule, and DX12 shared handles
(`CreateSharedHandle` → `OpenSharedHandle` → `wgpu_hal::dx12::Device::texture_from_raw`) for the
zero-copy import — and returns `Unsupported` until it is built.
