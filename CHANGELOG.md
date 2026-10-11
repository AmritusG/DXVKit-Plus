# Changelog

## 0.3.0 — 2026-10-11

Getting decoded frames onto the GPU is fast. API additions only; nothing removed or changed.

- Diagnosis (Metal System Trace, wgpu timestamp queries, microbenchmarks): wgpu's
  `Queue::write_texture` allocates a new staging buffer per call — 2.8 of the 3.45 ms it took to
  upload 16 1080p BC7 frames — and the GPU sat mostly idle while the CPU staged, uploaded and waited.
- `dxvkit-plus-wgpu`: `FrameRenderer::upload` + `encode_convert` (the two halves of `render`, so many
  layers share one submission; optional timestamp writes on the conversion pass);
  `FrameRenderer::upload_staged` (persistent `StagingBelt` + `copy_buffer_to_texture`);
  `Uploader` (staging belt + at most `depth` submissions in flight, no device-wide wait;
  `stage`, `submit`, `wait_for`); `StagedFrame` (mapped staging for one frame, `Send`, a
  `PlaneSink`) + `FrameRenderer::upload_from_stage`; `FrameRenderer::direct_view` / `DirectView`
  (sample LMB7, HAP R / RGBA / RGB as stored, skipping the conversion pass; exact);
  `GpuError::code()` (pinned). `Gpu::with_adapter` also requests `TIMESTAMP_QUERY` when available.
- `dxvkit-plus-core`: `FrameSource::layout` (a block source's fixed frame shape: `FrameLayout`,
  `PlaneLayout`) and `FrameSource::decode_into` into a `PlaneSink` (default: decode, then write each
  plane; LMBD streams chunk by chunk through a reused buffer: `lmbd::decode_payload_into`,
  `LmbdFile::decode_frame_into`).
- Every new path produces exactly the pixels of `render`: GPU tests on every HAP fixture and LMBD
  vector frame; `dxvkit-plus verify` now checks the staged, decode-into and direct paths against
  the render on every frame (0 difference on all fixtures and four 1080p clips, 164 frames).
- 16 layers of 1080p on the Mac Studio, steps per second, 0.2.1 → 0.3.0 (pipelined + direct):
  LMBD λ 1 132 → 463-516, HAP 163-202 → 692-939, HAP Q 116-139 → 259-273 (pipelined). Decoding into
  staging: 16-layer LMBD latency 3.5 ms p50 (0.2.1 step: 7.5).
- `dxvkit-plus bench --layers N`: `--layers-only`, `--variants` (`per_layer`, `batched`, `staged`,
  `pipelined2`, `pipelined3`, `pipelined2_direct`, `into2`, `into2_direct`), a per-bucket breakdown
  (decode, upload, encode, submit, GPU wait, GPU conversion time from timestamps), sustained
  throughput and decode-to-texture latency.
- Fuzz: the LMBD target also runs `decode_frame_into` through a contract-checking sink and compares
  it with `decode_frame`.

## 0.2.1 — 2026-10-11

LMBD decodes with libzstd by default; the pure-Rust decoder stays as a feature.

- `dxvkit-plus-core`: features `zstd-c` (default: libzstd 1.5.7, the standard C library, bundled)
  and `zstd-rust` (`ruzstd`, pure Rust) pick the Zstd decode backend for LMBD chunks; `zstd-c` wins
  when both are on, and neither is a compile error. Chunks decompress straight into the frame
  buffer either way. No public API change; errors are still `MediaError::Lmbd`.
- Every crate that uses the core forwards both features; the workspace now depends on its own
  crates with default features off, so `--no-default-features --features zstd-rust` switches the
  whole build.
- Both backends decode every LMBD vector frame (13/13, equal to the original decoder) and 44 1080p
  frames to identical RGBA. 16 layers of 1080p LMB7 (λ 1, 8 chunks), Mac Studio: decode-all p50
  3.30 ms with libzstd vs 11.30 ms with `ruzstd`; λ 0: 2.76 vs 8.79 ms.
- `dxvkit-plus` CLI: `probe` and `bench` print `zstd_backend` (`libzstd` or `ruzstd`) for LMBD.
- Notices: `LICENSES/libzstd-BSD-3-Clause.txt`, `LICENSES/zstd-rs-BSD-3-Clause.txt` (the encoder
  has linked libzstd since 0.2.0; its notice was missing). The `zstd` crates are BSD-3-Clause, not
  MIT as a 0.2.0 comment said.

## 0.2.0 — 2026-10-11

LMBD, our own GPU codec (BC7 + rate-distortion optimisation + Zstd), encoder and decoder.

- `dxvkit-plus-core`: the `lmbd` module — read (validated, hostile-input safe, chunks decompress in
  parallel with the pure-Rust `ruzstd`) and write the LMBD v1 container, profile LMB7;
  `SourceKind::Lmbd` (= 5) through `source::probe` / `open`, decoding to one BC7 plane;
  `MediaError::Lmbd` (code 11). Files written by the original C encoder decode bit-exact (five
  compatibility vectors in `tests/fixtures/lmbd_*.lmbd`).
- `dxvkit-plus-lmbd-enc` (new): the LMBD encoder — a faithful Rust port of bc7enc_rdo's 4-mode BC7
  encoder and RDO pass (MIT), reproducing the original's fused multiply-adds, + libzstd level 9.
  Writes byte-identical files to the original encoder; frames encode in parallel.
- `dxvkit-plus-wgpu`: `LmbdTexture` (a BC7 texture with an sRGB view; the GPU decodes the blocks
  when it samples, bit-exact with the CPU), `FrameRenderer::render_view`,
  `create_bc_texture_with_views`.
- `dxvkit-plus` CLI: `encode-lmbd`, `compare` (PSNR / SSIM against a reference),
  `gen-pattern --alpha`; `verify` requires LMBD GPU == CPU exactly; `probe`, `decode`, `bench`
  accept `.lmbd`.
- Fuzzer: the LMBD reader is a target (`fuzz ... lmbd` runs it alone).
- Docs: `docs/LMBD.md` (the container and bitstream spec), LMBD vs HAP Q measurements.


## 0.1.0 — 2026-10-11

First public release.

- `dxvkit-plus-core`: QuickTime / MP4 demux with the `tmcd` timecode track; HAP decode for every
  variant (Hap1, Hap5, HapY, HapM, HapA, Hap7 / HAP R, HapH), Snappy and chunked frames (parallel);
  CPU block decoders (BC1, BC3, BC4, BC7) for CPU RGBA on request; a frame cache with read-ahead;
  an exact-time transport (loop, bounce, once, random frame; reverse and any speed; frame-exact
  seek, step, scrub, cues; beat and timecode sync); the platform `Backend` trait with a documented
  Windows (Media Foundation + DX12) stub. DXV files are recognised and probed; DXV decoding is on
  the roadmap.
- `dxvkit-plus-wgpu`: BC block uploads into BC textures on wgpu 30; WGSL conversion for HAP Q,
  alpha-only, HDR and 4:2:0 YCbCr.
- `dxvkit-plus-apple`: AVFoundation / VideoToolbox decode (H.264, HEVC, ProRes incl. 4444 alpha,
  MJPEG), frame-accurate, imported zero-copy into wgpu Metal textures (IOSurface and
  `CVMetalTextureCache` routes).
- `dxvkit-plus` CLI: `probe`, `decode`, `bench`, `verify`, `gen-pattern`.
