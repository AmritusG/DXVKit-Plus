# Architecture

```
      file.mov / file.mp4 / file.lmbd / PNG folder
                         │
              dxvkit-plus-core::source::probe ──► MediaInfo (codec + names, size, frames, exact rate, tmcd start,
                         │                       track, DXV variant, stated alpha)
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
   BlockPath::Sampled: write_bc →    IOSurface plane(s) → MTLTexture → wgpu::Texture
     BC texture (GPU decodes)
   BlockPath::Exact: the same bytes
     as integer texels (shader decodes)
               └──────────┬──────────────┘
                 WGSL conversion pass → Rgba8Unorm at clip size
```

## Crates

| Crate | `unsafe` | GPU | What |
|---|---|---|---|
| `dxvkit-plus-core` | forbidden | none | demux, HAP, DXV, the LMBD container (LMB7 and LMBX), CPU block decoders, sources, cache, transport, time, the `Backend` trait; links libzstd for LMBD decode by default (`zstd-c`), or pure Rust (`zstd-rust`) |
| `dxvkit-plus-lmbd-enc` | forbidden | none | the LMBD encoders: LMB7 (4-mode BC7 + RDO, Zstd; links libzstd through the `zstd` crate) and LMBX (DCT + RDOQ + rANS, rate control) |
| `dxvkit-plus-wgpu` | forbidden | wgpu 30 | BC textures, staged uploads (`Uploader`, `StagedFrame`), the WGSL conversion on two block paths (`BlockPath`: sampled, or exact — decoded in the shader) and direct sampling (`DirectView`), read-back, `LmbdTexture` |
| `dxvkit-plus-apple` | **allowed** (every block has a `// SAFETY:` comment) | wgpu-hal Metal | AVFoundation / VideoToolbox decode, IOSurface → wgpu import |
| `dxvkit-plus-cli` | forbidden | wgpu | the `dxvkit-plus` command |

The core never depends on a GPU API: its output is a description of GPU-ready data
(`source::PixelFormat`, `source::Shading`) that any renderer can upload. The platform crate is
the only place that knows objc2, CoreVideo or Metal types; callers see `NativeFrame` with an
opaque handle.

## Probe and routing (0.9.0)

`source::probe` decides what plays a file by its content, and routes a movie the way Swift Glance
does (glance/Sources/Glance/PlayerModel.swift:209-229), so the Rust family and the Swift apps open
the same files the same way:

1. A folder is an image sequence. A file that starts with the LMBD magic is LMBD (whatever its name).
2. A QuickTime / MP4 movie (`container::Movie::open_all`): the track `ImportOptions::track` names
   (its index among the movie's video tracks in file order, damaged ones counted:
   `VideoTrack::index`); without one, Swift Glance's order —
   1. the first DXV track anywhere (DXVKit's `mov.c`: the first `trak` whose entry is `DXD3` /
      `DXD4` / `DXDI`);
   2. else HAP, only when the movie's **first** video track is HAP (Swift Glance's `HAPDemuxer`
      takes the first video track; `Hap7` / `HapH` included here, which Swift Glance does not play);
   3. else the platform backend on the file (it plays the first video track; a chosen native track
      must be that one).
   `MediaInfo::track` says which track plays; `source::open` opens that one.
3. A file whose first bytes are a PNG signature is a still; then an image extension names an image
   (PNG decodes, the others are `Unsupported`) — the name speaks only where the content cannot.
4. Anything else goes to the platform backend.

**Damage is per track.** A video track whose sample tables do not parse is listed in
`Movie::damaged` (`DamagedTrack`: index, FourCC if readable, the error) and the other tracks still
open. A movie our parser cannot read — a damaged first track, a `moov` that does not parse, a
fragmented MP4 (its `moov` has empty sample tables; fragments are not read) — is offered to the
platform backend, **unless a DXV or HAP track was identified** in it (by the damaged track's FourCC,
or by `container::first_video_fourcc` when the `moov` itself is unreadable): the platform decodes
neither, so our parser's `Corrupt` stands. Without a backend the answer is our parser's error.
Verified on a real fragmented MP4 written by AVFoundation (`tests/fixtures/frag_h264.mp4`).

**A cut-off track keeps its whole samples.** A sample that lies past the end of the file stays in
the sample table; `Movie::read_sample` fails it (`Corrupt`, nothing read or allocated), so a file cut
in the middle of its sample data plays up to the cut and fails frame by frame after it — Swift
Glance's HAP demuxer's behaviour, applied to DXV and HAP alike (DXVKit's `mov.c` refuses such a DXV
track). `MediaInfo::frames` stays the sample table's count. Hostile-file caps that remain: 64 MB
`moov`, 2^24 samples, 300 MB per sample, and a uniform sample size may not claim more samples than
the file has bytes.

**What `MediaInfo` says** (fields appended in 0.9.0): `track`; `dxv_variant` (the first packet's
`dxv::Variant`; `None` for other clips or a first packet that states none); `alpha`
(`StatedAlpha`: `None`, `Straight`, `Premultiplied`, `Unstated`) — HAP Alpha / Q Alpha /
Alpha-only / R and LMBD and PNG state straight, DXV with alpha states nothing (the user's choice,
`AlphaMode`), a native clip states what its format description says (`NativeInfo::alpha`; on macOS
Swift Glance's rule: the `ContainsAlphaChannel` extension, or ProRes 4444 / 4444 XQ without it;
premultiplied only when `AlphaChannelMode` says so); `codec_name` and `container_name`
(`names::codec_label`, `names::container_name`).

**Names** (`core::names`): `codec_name` is Swift Glance's `DXVDetector.displayName` table plus
`Hap7` "HAP R" and `HapH` "HAP HDR" (the HAP specification's "Hap R" / "Hap HDR" in the family's
case); `container_name` is the extension in upper case or `this container`; `unsupported_message`
is Swift Glance's line, `<codec> in <CONTAINER> is not supported` or `<CONTAINER> files are not
supported`, the codec from `container::first_video_fourcc` (atom headers only, Swift's
`DXVDetector.compressorFourCC` walk) or a Matroska `V_...` id.

## Decode paths

**HAP.** `container::Movie` resolves sample offsets from `stsz` + `stsc` + `stco`/`co64` (with hard
caps: 64 MB `moov`, 2^24 samples, 300 MB per sample). `hap::decode` walks the section tree (4- or
8-byte headers), decompresses Snappy or the chunk table (chunks decompress in parallel when asked),
and returns each image's raw block stream, exactly `ceil(w/4) * ceil(h/4) * block_bytes` long. The
blocks upload unchanged — into BC textures the GPU decodes (`write_bc`, the sampled path), or as
integer texels the conversion shader decodes (the exact path, [below](#block-paths-080)). No CPU
pixel work on the playback path.

**DXV.** A safe-Rust port of DXVKit's C decoder (`core::dxv`, carried over from Loombda-rs, where
it was ported line for line from DXVKit 1.0.0; DXVKit is itself written from its own
`DXV_FORMAT.md`). The packet decides the variant, never the FourCC: DXV3 `DXT1` (BC1), `DXT5`
(BC3), `YCG6` (Y BC4 + CoCg BC5 at half size) and `YG10` (Y + A BC5 + CoCg BC5), each through
DXV3's block-copy layers (DXT1 / DXT5: 2-bit flags, back references, DXT5 alpha commands) or the HQ
opcode streams (stored, fill, or FSE entropy-coded opcodes; the two sections decode on two
threads when asked), or a stored payload; and legacy `DXDI` (DXV1 / 2: LZF DXT1 / DXT5, or stored).
Output is the normative block streams of the whole coded texture (display size rounded up to 16).
DXT1 / DXT5 upload as BC textures and sample as stored (`direct_view` qualifies) on the sampled
path, or are decoded in the shader on the exact path ([below](#block-paths-080)). DXV High
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
table — coefficients; v2 palette indices; v3 the eight index tables and the edge-alpha table — plus,
in v2 and v3, one that turns the directory's stream lengths into offsets; tables no tile uses are
skipped), `entropy` (one dispatch in two kinds of workgroup, from a tile list the CPU builds from
the mode map: one invocation per stream of each DCT tile — rANS, dequantise, write coefficients in
place — and, in separate workgroups, one invocation per direct tile, one every 8 lanes, for the
serial part: v2 palette indices, v3 context-coded palette and quad indices with edge alpha, v3 alpha
palettes; W8 measured a SIMD group shared between the two, or full of palette decodes, as the
v3 prototype's slowdown), `idct` (one invocation per 8x8 block of a DCT tile: the integer inverse
DCT as generated straight-line WGSL; a direct tile's Y-plane blocks expand its pixels instead),
then a fragment pass (4:2:2 halo or
4:4:4 chroma, inverse YCoCg-R, direct tiles' RGBA8, 8-bit output). The arithmetic is the reference decoder's, so GPU and
CPU agree bit for bit on every frame, hostile ones included. LMBX frames vary in size, so they have
no fixed `FrameSource::layout` (no decode-into-staging, no direct view). The encoder is
`lmbd_enc::lmbx` (B1d's RDOQ, with rate control; a rate-distortion choice per tile between the DCT
and the direct modes; 0.6.1 adds the exact forward YCoCg-R and a colour guard that holds flat
regions within a tolerance; 0.7.0 writes bitstream v3 from `lmbd_enc::v3` — context-coded palettes
of up to 64 colours, edge alpha, quad tiles, alpha palettes — `docs/LMBD.md` §11.11).

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

## Block paths (0.8.0)

A `FrameRenderer` decodes BC1 / BC3 / BC4 frames — DXV DXT1 / DXT5 / legacy DXT, HAP Hap1 / Hap5 /
HapY / HapM / HapA — on one of two paths, chosen per renderer (`BlockPath`,
`FrameRenderer::with_path` / `set_path`; `#[repr(u32)]`, the default is `Sampled`, so a renderer made
with `FrameRenderer::new` behaves as before 0.8.0 — its output is byte-identical to 0.7.1's).

| | `BlockPath::Sampled` (default) | `BlockPath::Exact` |
|---|---|---|
| Upload | the blocks into a BC texture (`Bc1RgbaUnorm`, `Bc3RgbaUnorm`, `Bc4RUnorm`) | the same bytes, the same rows, into an integer texture: one `Rg32Uint` texel per 8-byte block, one `Rgba32Uint` texel per 16-byte block |
| Decode | the GPU's fixed-function BC decoder, when `CONVERT_WGSL` reads the texel | `BC_EXACT_WGSL`: the CPU decoders' integer arithmetic, in the fragment shader |
| Contract against `Decoded::to_rgba` | encoded footage within 1 / 255 (Hap1, Hap5, HapA), 2 / 255 (DXT1, DXT5), 3 / 255 (HAP Q) — measured, Apple M5 Max; arbitrary blocks up to 3 (HAP Q 6); un-premultiplied colour scales that step by 255 / alpha | **equal: every channel of every texel, at every tested size** |
| `direct_view` (no conversion pass) | BC1 / BC3 / BC7 frames | none: integer texels cannot be sampled |
| Use it for | many layers, the lowest GPU time | pictures that must equal a reference decode |

**BC7 (HAP R, LMB7 frames as planes; 0.9.0)** is on both paths too. Sampled, the GPU's BC7
decoder decodes it (exact integer arithmetic by the format's definition, and so on the Apple M5 Max
for every block tested, reserved modes included) and it keeps its `direct_view`. Exact, it is
integer texels decoded by `BC_EXACT_WGSL` mode 4, `core::bc7::bc7_block` line for line — because a
GPU need not follow the definition for blocks no encoder writes: GitHub's virtual Metal GPU ("Apple
Paravirtual device") decodes reserved-mode blocks (first byte 0) as opaque black where the
definition says transparent black (glance-core C1, CI run 43638498291). So "Exact equals
`Decoded::to_rgba` on every texel" holds for every input on every GPU, hostile files included; the
sampled path's BC7 is exact for valid blocks and, for reserved ones, is whatever the GPU gives.
`LmbdTexture` (LMB7 sampled straight from its BC7 texture) is the hardware route.

Frames that take the same route on both paths: DXV High Quality (always decoded in the shader,
`DXV_HQ_WGSL`), LMBX and RGBA8. HAP HDR (BC6H) has no CPU decoder and is always sampled. The exact pipeline is built on a renderer's first exact frame, so a renderer on the sampled
path compiles nothing new.

**What the exact shader computes, per variant** (the oracle is `core::bc` / `core::hap`, which follow
the sources named):

| Variant | Palette and rounding | Source |
|---|---|---|
| DXV DXT1, legacy DXT1, HAP Hap1 (BC1) | 5:6:5 endpoints widened by bit replication; thirds and halves **truncated**; three-colour mode's fourth entry black; alpha 255 | DXVKit spec 8.1, `src/pixels.c` (`bc::bc1_block`) |
| DXV DXT5, legacy DXT5, HAP Hap5 (BC3) | colour always four-colour, thirds **truncated**; alpha sevenths / fifths **truncated** (six-value mode ends in 0 and 255) | DXVKit spec 8.2 (`bc::bc1_block`, `bc::bc4_block`) |
| DXV DXT5 read premultiplied | as above, then `min(255, (c * 255 + a / 2) / a)`, a = 0 keeps c | `bc::unpremul` |
| HAP HapA (BC4) | sevenths / fifths **truncated**, shown as grey, alpha 255 | `bc::bc4_block` |
| HAP R, LMB7 (BC7) | the specification's decode: mode = lowest set bit of byte 0, partition / anchor tables, p-bits, bit-replicated endpoints, `((64 - w) * e0 + w * e1 + 32) >> 6`, index selection, rotation; reserved mode transparent black. Exact by definition (no rounding choice) | Khronos Data Format 1.3 "BPTC", Microsoft "BC7 Format" (`bc7::bc7_block`) |
| HAP Q HapY, HapM (BC3 scaled YCoCg) | the **exact** palettes — colour in thirds, Y in 35ths of a code value — through the scaled-YCoCg transform 0.7.1 already used (offset 128 / 255, `scale = B * 255 / 8 + 1`) over one common denominator, **rounded to nearest, halves up**; HapM's alpha plane truncated like Hap5's | this library's reading of van Waveren & Castaño 2007, the paper the HAP specification references (`bc::bc3_block_exact`, `hap::ycocg_scaled_exact`) |

HAP's own variants thus share DXVKit's truncating convention for plain BC1 / BC3 / BC4 (this
library has one integer BC decoder), and only HAP Q keeps the untruncated palette, because
truncating three channels before the YCoCg transform compounds. Until 0.8.0 the HAP Q oracle
evaluated that transform in `f64`, which lands on either side of an exact half: it rounded down
574 662 of 200 000 000 random texels that the exact value rounds up (2.1% of the HAP Q fixtures'
texels, 7.0% of a real 1080p HAP Q Alpha clip's — always by 1). No shader can reproduce float noise,
so the oracle is now the integer formula and the shader computes the same expression.

**Rules the exact shader keeps so that every backend computes the same value** (wgpu 30.0.1 /
naga 30.0.1; the MSL and HLSL naga writes for these shaders are checked in `wgpu/tests/exact.rs`):

- *Integer texels, never sampled.* `texture_2d<u32>` + `textureLoad`: `Rg32Uint` / `Rgba32Uint` are
  core WebGPU formats (no feature needed; `TEXTURE_BINDING | COPY_DST`), not filterable, and no
  sampler is bound. naga writes `Texture2D<uint4>.Load` (HLSL) and `texture2d<uint>.read` (MSL).
  The texel's words are the block's bytes as little-endian `u32`s, which is the byte order of
  every wgpu target.
- *Unsigned division only, never by zero.* naga wraps every integer `/` in `naga_div`
  (`lhs / (rhs == 0 ? 1 : rhs)` in the MSL and the HLSL it writes), so `x / 0 = x` there; the shader divides
  only by constants, by a non-zero alpha, and by `2 * 35 * (b3 + 24) >= 1680`. Signed values are
  clamped at zero before they are divided, so no back end's rounding of a negative quotient matters.
- *Shift counts below 32.* naga writes `<<` / `>>` as they are, and WGSL defines the count modulo
  the bit width; the shader never relies on that: every count is a constant from 1 to 16, `2 * i`
  (i < 16), or a BC4 bit position proven inside 0..31 by its branch.
- *No overflow.* Apart from the blocks' own 32-bit words (only shifted and masked), every
  arithmetic result is below 2^24 (the largest: `2 * n + 35 * (b3 + 24)` < 14.2 million).
- *Texel address from the fragment position.* `vec2<u32>(position.xy)`: fragment centres are at
  `.5` on every backend and naga's `f2u32` helper clamps then truncates; `xy / 4` is the block,
  `(y & 3) * 4 + (x & 3)` the texel in it. The plane can be larger than the picture (HAP rounds up
  to 4, DXV codes to 16); the padding is never addressed.
- *Float only at the very end.* The result is a whole number 0..255 written as `n / 255.0` into
  `Rgba8Unorm`. Whether a back end divides or multiplies by a reciprocal, the value is within
  2^-22 of `n / 255`, far inside the half code value a unorm conversion needs.
- *Not yet run on D3D12 or Vulkan.* naga validates the shaders and translates them to MSL and HLSL
  in the test suite; no SPIR-V was generated and nothing was compiled by FXC / DXC. The
  0-difference test itself has run on Metal only and runs on D3D12 at the Windows wave.

**Cost** (release, Apple M5 Max, Resolume Arena running on the same GPU; raw:
`docs/data/w1-exact-path-*`): the conversion pass of one 1080p layer takes 30-48 µs sampled and
50-73 µs exact (HAP Q: 30-34 µs and 84-93 µs) — wall time from submission to the GPU being done,
64 passes per submission, so an upper bound on GPU time. For 16 layers that is 0.5-0.8 ms per step
sampled against 0.8-1.5 ms exact. Decode and upload are the same on both paths (the same bytes go
up), so a pipelined player that converts every layer runs at the same speed within the run-to-run
spread (exact / sampled: median 1.04, range 0.70-1.25 over 18 clip and layer-count cases). What the
exact path gives up is `direct_view`, which lets a compositor skip the pass for plain BC1 / BC3
frames: at 16 layers 1792 / 1006 / 1279 / 671 steps/s sampled directly (Hap1 / Hap5 / DXT1 / DXT5;
`pipelined2_direct`) against 925 / 706 / 884 / 611 for the same variant on the exact path.

## Accuracy contracts

| Path | Contract | Test |
|---|---|---|
| HAP Hap1 / Hap5 / HapA (CPU) | exact to the source pattern (0/255 measured; 1/255 allowed) | `core/tests/fixtures.rs` |
| HAP Q HapY / HapM (CPU) | ≤ 2/255 to the source (chroma is lossy by design) | same |
| HAP Q transform (CPU) | the exact rational value rounded half up, on a grid of 8 604 128 inputs against `i128` arithmetic | `core/src/hap.rs` tests |
| **Exact path** GPU vs CPU: DXV DXT1 / DXT5 / legacy DXT (straight and premultiplied), HAP Hap1 / Hap5 / HapY / HapM / HapA, and (0.9.0) BC7: HAP R, LMB7 | **bit-exact** (max 0, all four channels): every frame of the committed fixtures and vectors (504 frames, 348 of them BC1 / BC3 / BC4, 16 BC7), and movies the test writes from random blocks (BC7: every mode forced in turn, all-zero = reserved-mode blocks) at 1x1, 2x2, 3x3, 4x4, 5x7, 15x9, 17x33, 1920x1080, 3840x2160; a HAP R frame with every eighth block reserved; through `write_texture`, staged uploads and decode-into-staging; one flipped bit fails it | `wgpu/tests/exact.rs`, `dxvkit-plus verify --block-path exact` |
| Conversion shaders | naga validates each and translates it to MSL and HLSL | `wgpu/tests/exact.rs` |
| Sampled path GPU vs CPU, every HAP fixture frame | ≤ 1/255 (BC decoders may round differently by 1; HAP Q measures 1 on the fixtures); `verify` allows HAP Q 3/255 (measured 3 on real 1080p clips, Apple M5 Max, on 0.7.1 and 0.8.0) | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| DXV decode (CPU) vs DXVKit's test vectors | bit-exact: 334 rows (366 block streams, 287 RGBA, 47 rejects) with `DXVKIT_DIR`; the committed 74-row MIT subset always | `core/tests/dxv.rs` |
| DXV decode vs DXVKit's C core (`dxvtool`) | bit-exact block streams, straight and premultiplied RGBA on every frame of the DXV fixtures and committed vectors (with `DXVTOOL`) | same |
| DXV GPU vs CPU, sampled path | High Quality (YCG6 / YG10) bit-exact, both alpha modes, every vector frame; DXT1 / DXT5 ≤ 2/255 (the GPU's fixed-function BC1 / BC3 vs DXVKit's truncating convention, as Arena's GPU upload, DXVKit spec 8.1); premultiplied DXT5 ≤ 2/255 as shown | `wgpu/tests/gpu.rs`, `dxvkit-plus verify` |
| BC7 CPU decoder vs the GPU's own BC7 decode (sampled path) | bit-exact on Apple M5 Max, 4096 random blocks per mode, all 8 modes; reserved-mode blocks differ by GPU (GitHub's virtual Metal GPU: opaque black) — the exact path does not depend on it | `wgpu/tests/gpu.rs` |
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
| `PixelFormat`, `SourceKind`, `YuvMatrix`, `Codec`, `HapTexture`, `AlphaMode`, `StatedAlpha` (0.9.0), `dxv::Variant`, `PlayMode`, `Direction`, `SyncMode`, `lmbd::{Profile, TexFormat, Compressor, Colorspace}`, `lmbx::Chroma` | `#[repr(u32)]` with fixed values (pinned by `core/tests/abi.rs`; the LMBD ones equal the bytes in the file) |
| `wgpu::BlockPath` | `#[repr(u32)]` (`Sampled` = 0, `Exact` = 1; pinned by `wgpu/tests/abi.rs`); `FrameRenderer::with_path` / `set_path` / `path` map to a create call taking the value and a setter / getter on the renderer handle |
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
| `ImportOptions` | a plain-data mirror: the sequence rate, `alpha_mode` (`AlphaMode` as `u32`), `track` (0.9.0: a flag + `u32`); the backend chosen by the wrapper |
| `MediaInfo` 0.9.0 fields | `track` and `dxv_variant` as a flag + `u32`; `alpha` as `StatedAlpha` (`u32`); `codec_name`, `container_name` as strings through a call, like the path |
| `container::DamagedTrack`, `first_video_fourcc` | plain data (index, a flag + FourCC bytes, the error's `code()`); the FourCC as 4 bytes + a found flag |
| `names::*` | functions returning strings (a call that fills a caller buffer) |
| `wgpu::check_device_size` | a call returning the `GpuError` code |
| `dxv::Layout`, `dxv::Header`, `dxv::Blocks` | plain-data `#[repr(C)]` mirrors (`Blocks`' planes as pointer + length borrowed from the handle) |
| `Arc<dyn Backend>` | chosen by the wrapper per platform (no callbacks across the boundary) |

Rules that keep it that way: no generics or lifetimes in the public decode / transport API, no
callbacks, errors as values, every fieldless enum `#[repr(u32)]` with explicit values, and new
variants only appended.

## Apple targets (0.9.0)

| Target | What builds | Run |
|---|---|---|
| `aarch64-apple-darwin` | everything | the test suite, every wave |
| `x86_64-apple-darwin` | everything (`cargo check --all-targets`, release build) | the CLI under Rosetta 2 on Apple silicon: `verify --block-path exact` max 0 on HapM / Hap7, ProRes 4444 zero-copy; not on an Intel Mac |
| `aarch64-apple-ios` | the workspace; `dxvkit-plus-apple` with its decoder (AVFoundation, VideoToolbox, CoreVideo's `CVMetalTextureCache`, IOSurface, wgpu Metal — the code is the macOS code, the `cfg` widened) and an example binary links | not run (no device or simulator here) |

**Deployment target.** rustc's default for `x86_64-apple-darwin` is macOS 10.12; below 10.14.4 the
SDK links CoreMedia's Swift overlay as `@rpath/libswiftCoreMedia.dylib` and the binary does not load
("no LC_RPATH's found"). Built for 11.0 (rustc's `aarch64-apple-darwin` default) it links no Swift
library. For iOS, with no `IPHONEOS_DEPLOYMENT_TARGET` set rustc links for 10.0 while the
cc crate builds libzstd for the SDK's version, and the link fails (`___chkstk_darwin`); any value
set aligns both (10.0 links too); this repo uses 15.0. This repo sets both in `.cargo/config.toml`
(`MACOSX_DEPLOYMENT_TARGET = 11.0`, `IPHONEOS_DEPLOYMENT_TARGET = 15.0`, an environment variable
wins); a crate that ships a binary using `dxvkit-plus-apple` sets its own (a library's build cannot
set it for its dependents).

## Windows (planned)

`backend::windows::MediaFoundationBackend` documents the design — `IMFSourceReader` with a D3D
device manager into NV12 / BGRA textures, the same seek rule, and DX12 shared handles
(`CreateSharedHandle` → `OpenSharedHandle` → `wgpu_hal::dx12::Device::texture_from_raw`) for the
zero-copy import — and returns `Unsupported` until it is built.
