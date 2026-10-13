# Changelog

## 0.7.0 — 2026-10-13

**LMBX bitstream v3** (W7 explored it, W8 made it fast and froze it; decision LMBX-D4: Amrit chose
"speed recovery" — ship v3 after recovering its GPU decode speed before the freeze). Versions 1 and 2
are frozen and still decode bit-exact (every v1 and v2 vector); LMB7 is unchanged (byte-identical).

- Bitstream v3 (`docs/LMBD.md` §11.6a, normative): context-coded palette indices (copy-left,
  copy-above or a literal, eight contexts, 10-bit tables); exact palettes of up to 64 colours (lossy
  ones up to 16); **edge alpha** — a residual per pixel of partial alpha makes anti-aliased edges
  exact; **quad tiles** — four 16x16 sub-tiles, flat or palette each; DCT colour with an **alpha
  palette**. A direct tile's payload is its stream 0 (as in v2).
- Graphics with alpha: lossless at LMB7's bytes (960x540 card 4.4 KB against LMB7's 5.2 KB; the
  1080p eye clip 6.5 KB/frame against 9.4 KB); LMB7's quality at 3.7 KB (0.6.1 needed 6.4 KB).
  Opaque graphics lossless at 5.2 KB (LMB7 5.8 KB). Real alpha +0.1 dB, synthetic alpha +0.5 dB
  shown, grain equal, glow and detail within 0.06 dB of 0.6.1 / 0.03 dB of the W7 prototype
  (`docs/data/w8-lmbx-v3-quality.txt`).
- GPU decode (`dxvkit-plus-wgpu`): profiled with Metal System Trace before the freeze
  (`docs/data/w8-lmbx-v3-profile.txt`). The prototype was 21-40% slower than 0.6.1; v3 itself adds
  nothing to DCT tiles. The CPU now builds a tile list from the mode map; one entropy dispatch runs
  DCT streams and direct tiles in separate workgroups (one direct tile every 8 lanes: a SIMD group
  of palette decodes serialised on memory), with one-word index-table slots, word-cached rANS reads
  and word reads of direct colours. 16 x 1080p glow 59-63 steps/s (0.6.1 61), 4 x 4K Shroomies
  116 (108), 4 x 4K graphics 140 (146), 16 x 1080p graphics 120-121 (142; the context-coded palette
  decode's own cost). GPU == CPU bit-exact on every vector, 96 encoded and 288 hostile v3 frames.
- Encoder (`dxvkit-plus-lmbd-enc`): writes v3 whenever tile modes are on (`--modes dct` still
  writes v1); `LmbxParams::alpha_palette`; deterministic at any thread count. The v2 writer is gone.
- Dropped from the W7 prototype after measurement: escape colours, direct colour with a DCT alpha
  plane, visibility-weighted alpha, lossy palettes above 16 colours, BC7 tiles (README roadmap).
  Rejected in W8: row-run symbols, a row-start context, index tables in workgroup memory.
- Five CC0 v3 conformance vectors (one hand-built: quad tiles with edge alpha); the re-encode
  hashes of the v2 vectors now pin v3 files. Fuzzing 600 s over the v1-v3 vectors: 0 crashes.
- Tools: `examples/lmbx_passes.rs` (per-pass GPU time), `examples/lmbx_quality.rs` (quality at a
  byte budget), `examples/lmbx_v3_quad_edge.rs` (the hand-built vector).

## 0.6.1 — 2026-10-12

Regression fix (W6b): LMBX shifted colours on the flat-graphics eye clips where LMB7 did not.
Encoder only — the bitstream stays v2, the decoder (CPU and GPU) is unchanged, and every v1 and v2
vector decodes bit-exact as before. Diagnosis, per-colour tables and A/B in
`docs/data/w6b-lmbx-colour.txt`.

- Proven causes (each with a falsifying measurement; the writer, the container and the decoder
  were ruled out — our decode, the eye .mov read back through AVFoundation and Arena's own preview
  agree to the code value):
  - **Forward YCoCg-R scale.** The encoder lifted in code values and scaled by 4; the decoder
    inverts in quarter units, where the floors do not scale. Every colour with odd `R - B` and odd
    `G - t` — 4,194,304 of 16,777,216, a quarter — decoded 1 low in R and B at any quality. The
    encoder now uses the decoder's own quarter-unit forward: 0 of 16,777,216 wrong.
  - **DC lattice at coarse steps.** A block's DC is a residual from the previous reconstructed DC at
    step `dc_step(q)`, so a flat block lands on a lattice of `q / 8` code values. The 0.5.0 (v1) eye
    arm, starved at its rate floor (q 96, chroma 154), moved flat fields by up to 12 code values;
    the lattice predicts every flat colour's decoded value exactly (9 of 9).
  - **v2 mode decision.** The rate-distortion choice coded a gradient's 32x32 tiles as flat tiles:
    32-row stripes, rows off by up to 7 code values in R, at a good PSNR. On the small card the
    lossy palettes also pulled white 1.7 low (Lloyd centroids dragged by anti-aliased edges).
- `dxvkit-plus-lmbd-enc::lmbx`: the exact forward transform; a **colour guard**
  (`LmbxParams::colour_tol`, default 1 code value, LMB7's own worst on the card being 1.02): smooth
  DCT blocks take a finer step only where their DC would decode off their colour (exact integer
  check, in stream order with every plane's DC chain); flat and lossy-palette tiles that move a
  region (a colour filling at least 16 uniform 2x2 cells) beyond the tolerance are not candidates;
  lossy palettes seed with the regions, move a region-holding centroid to the regions' mean only,
  and are offered for tiles of at most 16 colours too. CLI `encode-lmbd --colour-tol T|off`.
- Measured at LMB7's bytes per frame (Mac Studio): graphics card 45.35 dB (0.6.0 38.41, LMB7
  45.50), every flat region within 0.02 code values (0.6.0 1.81, LMB7 1.00); with alpha 43.77 dB
  shown (0.6.0 35.92, LMB7 46.66); the 1080p graphics eye clip 60.0 dB mean over 60 frames (0.6.0
  51.2, LMB7 49.7), flat regions exact. Natural content equal or better: glow 43.06 (43.00), real
  alpha 48.66 / 55.35 / 50.39 (48.63 / 55.34 / 50.38), synthetic alpha +0.8, grain +1.5 (the
  transform fix), detail equal; 4:2:2 likewise.
- Tests: the DCT path reproduces flat colours exactly at quality 1 (both chroma modes); the colour
  guard keeps every flat region of the graphics card within 1 code value at a coarse quality (v2,
  both chroma modes, with and without alpha; DCT-only per flat block, its control without the guard
  off by more than 3). The committed vectors stay (decoder conformance); the identity test pins the
  0.6.1 encoder's re-encodings (`reencoded_sha256` in the sidecars) at 1, 3 and 8 threads.
- `scripts/mint-lmbx-vectors.sh` built only its example, not the CLI it runs (cargo builds just the
  example when `--example` is given): fixed.

## 0.6.0 — 2026-10-12

LMBX bitstream v2 (`docs/LMBD.md` §11, decision LMBX-D3): fixes the two weak spots 0.5.0 left — real
alpha footage and flat hard-edged graphics. v1 is frozen and still decodes bit-exact (its five
vectors unchanged); the encoder writes v2 by default.

- Bitstream v2: a palette-index rANS table and a **tile mode map** after the coefficient table —
  per 32x32 tile `0` DCT (v1's coding), `1` flat (one RGBA8 colour), `2` palette (up to 16 RGBA8
  colours and a rANS-coded, left-predicted index per pixel); and a stream directory of lengths
  instead of offsets (12% of a graphics frame once compressed). Direct tiles write the sample
  planes, so the 4:2:2 halo of a neighbouring DCT tile stays defined.
- `dxvkit-plus-core::lmbx`: decodes v1 and v2 (`TileMode`, `Frame::mode`, `direct_tile`,
  `FrameHeader::version`, `Sections::{palette_table, modes}`).
- `dxvkit-plus-wgpu`: v2 on the GPU, bit-exact with the CPU on every vector (every tile mode, both
  chroma modes) and on hostile frames with random tile modes; a third table workgroup turns the
  stream lengths into offsets; tiles stay independent.
- `dxvkit-plus-lmbd-enc::lmbx`: per-tile rate-distortion choice between DCT, flat, exact palette and
  lossy palettes (2/4/8/16 most frequent colours, Lloyd-refined); `LmbxParams::{flat, palette,
  palette_lossy}` (on by default; all off writes v1); `stored_size`, `frame_payload` (Zstandard
  frames when smaller; rate control counts the stored size).
- CLI: `encode-lmbd --profile lmbx` writes v2 and Zstandard frames by default; `--modes
  dct|flat,palette,lossy`, `--compressor zstd|none`; `decode --float` (LMBX); `gen-pattern
  --graphics [--alpha]` (a CC0 flat-graphics card: UI bars, buttons, text, logo, gradient);
  `compare` reports PSNR of RGB as shown (premultiplied).
- Four new CC0 v2 conformance vectors (`lmbx_v2_*`), with the normative output hashes; the encoder
  re-creates all nine byte for byte at 1, 3 and 8 threads.
- `--example lmbx_diag`: error by alpha class, bytes by section, plane, tile class and tile mode.
- Measured (Mac Studio, `docs/data/w6-lmbx-v2-ab.txt`; at LMB7's bytes per frame): real alpha
  48.21 dB RGB / 55.20 shown / 50.2 alpha against LMB7's 46.85 / 52.51 / 45.0 (v1: 45.49 / 52.37 /
  48.1); glow 43.00 (v1 41.97, LMB7 41.40); a flat graphics card 38.41 dB at 5.7 KB against LMB7's
  45.50 at 5.8 KB, equal quality at +14% bytes (v1: a 25 KB floor at 27.8 dB); with alpha +23%;
  noise-like detail +3.0 dB over v1.
- Built, measured and removed (ADD then prune; numbers in the data file): premultiplied-colour
  coding, an alpha quantiser ratio, alpha-aware colour fill and visibility-weighted RDO,
  alpha-weighted direct-mode distortion, lossless (MED) tiles, copy tiles, palettes of 4 or 8.
  The chroma default stays 4:4:4 (better on both real clips).
- Decode speed (Mac Studio, v2 files, Arena running; `docs/data/w6-lmbx-v2-speed-macstudio.jsonl`):
  16 x 1080p 67-70 steps/s (4:4:4) / 79-82 (4:2:2) on Amrit's glow and alpha clips, 154-157 on the
  graphics card; 4 x 4K 8.3-8.5 ms (4:4:4) / 7.4 ms (4:2:2) per step. Found and fixed before
  release: palette tiles were expanded by one GPU invocation each, which put a whole 32x32 tile on
  the critical path (+42% GPU time on the glow clip from four palette tiles a frame); the
  expansion now runs per 8x8 block in the IDCT pass (v2 within 4% of v1 on the same frames). The
  encoder's tile-mode search runs tile-parallel (5x faster encodes).
- Eye-test arms (`docs/data/w6-lmbx-eye-arms.txt`, 60 frames 1080p at LMB7's size): glow v2 42.0-43.6
  dB vs LMB7 41.4-41.6; real alpha v2 RGB 42.8-44.1 / shown 48.8-50.4 / alpha 44.1-45.4 vs LMB7
  43.2-43.8 / 49.1-50.1 / 42.4-43.1 (v1: 39.4-40.2 / 45.9-47.0); a 1080p graphics card v2 47.3-50.9
  dB vs LMB7 49.4-49.7 (v1: 30.3 at 8x the bytes).

## 0.5.0 — 2026-10-11

LMBX: LMBD profile `0x10`, frozen as bitstream v1 (`docs/LMBD.md` §11). Additions; one
`PixelFormat` variant and one `Shading` variant are new.

- `dxvkit-plus-core::lmbx`: the normative reference decoder. YCoCg-R, 8x8 DCT with an exactly
  specified integer inverse (LMBX-D1), per-block quantisers (a raw 4-bit map), rANS with one
  independent stream per plane of every 32x32 tile, DC predicted within each stream, chroma
  4:2:2-halo or 4:4:4 (LMBX-D2), optional alpha, any frame size. Output: RGBA in quarter code
  values (10 bits); 8-bit `(v + 2) >> 2`. Every input that passes validation has one defined
  result (short streams read zeros; renormalisation is bounded), so decoders agree bit for bit.
- LMBD container: profile `0x10` with texture format `0x11`; the never-frozen prototype (texture
  format `0x10`, bitstream "LMXS") is refused as `Unsupported`. `Header::new_lmbx`.
- `source`: LMBX files open as `SourceKind::Lmbd` with codec `LMBX`; a frame is its bitstream
  (`PixelFormat::LmbxBitstream`, `Shading::Lmbx`), validated on the CPU and decoded on the GPU.
- `dxvkit-plus-wgpu`: `FrameRenderer` decodes LMBX with compute passes (table expansion, one
  invocation per stream for rANS, one per 8x8 block for the IDCT as generated straight-line WGSL)
  and a fragment pass for chroma and colour; only the bitstream is uploaded (`write_buffer` or
  the staging belt). Bit-exact with the CPU on every conformance frame and on hostile frames.
- `dxvkit-plus-lmbd-enc::lmbx`: the encoder, from Amrit's B1d prototype (per-block quantiser from AC
  energy, deadzone, RDOQ with an end-of-block trellis, low-frequency protection), with rate
  control (`encode_frame_to_size`), deterministic at any thread count; `float_reference_rgba8`
  (the prototypes' float decoder, for measuring LMBX-D1).
- CLI: `encode-lmbd --profile lmbx [--chroma 422halo|444] [--quality Q | --frame-bytes N]`;
  `compare --float`; `bench` adds CPU LMBX decode measures.
- Five CC0 conformance vectors (`tests/fixtures/lmbx_*.lmbd`, `scripts/mint-lmbx-vectors.sh`) with
  the normative output hashes; `--example lmbx_hashes`; fuzz target `lmbx`.
- Fixed: a chroma step below 2 could not represent chroma DC (now at least 2; a DC step of at least
  2 / 4 bounds DC residuals) — found by measurement before the freeze.
- Measured (Mac Studio; `docs/data/w5-*`): the integer transform costs nothing measurable (float
  decode of the same bitstreams: equal or lower PSNR in every row, SSIM within 0.001). At LMB7's
  size, LMBX gains 4.8 dB on gradients over black, 6.9 dB on film grain, 5.9 dB on synthetic alpha,
  0.6 dB on Amrit's glow footage, and loses on hard-edged flat graphics and 1.4 dB on his
  real-alpha footage. GPU decode: 16 x 1080p at 93 (4:2:2) / 74 (4:4:4) steps per second, a 4K
  frame in 1.9 / 2.5 ms; 4:4:4 is the encoder default by measurement (better on both real clips),
  pending Amrit's eye. 32x32 tiles: +3.5% bytes for 2.2-2.4x faster decode than 64x64 (measured).
  DC prediction: 6-28% fewer bytes at equal quality. Fuzz: 600 s, 6.09 M execs, 0 crashes.

## 0.4.0 — 2026-10-11

DXV decodes. Additions, plus two enum variants and one `ImportOptions` field (below).

- `dxvkit-plus-core::dxv`: DXV decode, every variant — DXV3 `DXT1` (BC1), `DXT5` (BC3), `YCG6`
  (Y BC4 + CoCg BC5), `YG10` (Y + A BC5 + CoCg BC5), stored payloads, legacy DXV1/2 `DXDI` (LZF) —
  to block streams. A safe-Rust port of DXVKit 1.0.0's C decoder (Amrit's MIT library), carried over
  from Loombda-rs. Bit-exact on DXVKit's 334 test vectors (366 block streams, 287 RGBA, 47 rejects;
  `DXVKIT_DIR`) and against the C core on every frame of the fixtures and committed vectors
  (`DXVTOOL`). The MIT subset of the vectors (74 rows, synthetic content) and four CC0 DXV tile
  fixtures are committed.
- `source`: DXV movies open (`SourceKind::Dxv` now decodes; `FrameSource::layout` from the first
  packet, so `decode_into` and staging work); `AlphaMode` (`Straight` default — Resolume Alley's
  convention — or `Premultiplied`, un-premultiplied on output) as `ImportOptions::alpha_mode`;
  `Shading::DxvYcocg { alpha, premultiplied }` and `Shading::RgbaPremultiplied`;
  `bc::unpremul`; `MediaError::Dxv` (code 12). `planes_to_rgba` now strides by the plane's own
  width (DXV planes are coded to 16).
- `dxvkit-plus-wgpu`: DXV High Quality converts in its own pass (`DXV_HQ_WGSL`): the BC4 / BC5
  blocks upload as integer texels and the shader decodes the exact palettes and DXVKit's YCoCg
  folding in integers — bit-exact with the CPU on every vector frame (the fixed-function BC4
  decode was up to 5/255 off). Premultiplied colour un-premultiplies in integers (flag 4). DXT1 /
  DXT5 (straight) qualify for `direct_view`.
- CLI: `--alpha-mode straight|premultiplied`; `probe` prints `dxv_variant` and `alpha_mode`;
  `verify` tolerances: DXV High Quality exact, DXT1 / DXT5 2/255 (fixed-function BC vs DXVKit's
  truncating convention, DXVKit spec 8.1; premultiplied compared as shown).
- Fuzz: `--example fuzz -- 600 8 dxv` (DXV packets and movies; with `DXVKIT_DIR`, DXVKit's full
  vector streams join the corpus).
- P1-1 carried over (Loombda-rs W3): on a 512x256 straight-alpha card encoded by DXVKit, ours =
  the C core (0) for DXT5 and YG10; premultiplied mode vs ffmpeg's DXT5 decode as shown: 1.9/255
  (CPU), 2.6/255 (GPU). `docs/data/w4-p11-dxv-alpha.json`.
- 16 layers of 1080p DXV on the Mac Studio (glenc-cli clips, Arena running), steps per second,
  pipelined + direct, two rounds: DXT1 832 · 722, DXT5 358 · 279, YCG6 429 · 392, YG10 269 · 270;
  decode-into-staging latency p50 2.7-10 ms. `docs/data/w4-dxv-layers-macstudio.jsonl`.
- Fuzz: 600 s, 8 threads, DXV corpus 374, 27.0 M execs, 0 crashes, 0 hangs
  (`docs/data/fuzz-dxv-600s.txt`).

## 0.3.1 — 2026-10-11

- `dxvkit-plus verify`: the HAP Q (HapY, HapM) CPU-vs-GPU tolerance is 2/255, the project's stated
  HAP Q tolerance (it was 1, so the 1080p HAP Q plasma clip, CPU vs GPU 2/255, failed). Other HAP
  stays 1/255; LMBD and native frames stay exact. Pinned by a unit test.

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
