# Provenance

DXVKit Plus is an **independent, clean-room** project. It is not affiliated with, endorsed by or
sponsored by Resolume or Vidvox. "DXV" and "HAP" are named only to describe file compatibility.

## Where each part came from

| Part | How it was written |
|---|---|
| QuickTime / MP4 demux (`container.rs`) | Ported from DXVKit 1.0.0's `src/mov.c` (Amrit's own MIT C library, itself written from Apple's *QuickTime File Format* documentation) through Loombda-rs, generalised to every video track. The timecode track (`tmcd`) reader follows the QTFF "Timecode Sample Description". |
| HAP (`hap.rs`) | Written from the public Vidvox specification `documentation/HapVideoDRAFT.md` (Vidvox/hap @ d847f6b) only. The reference C implementation in that repository was not read. Snappy comes from the `snap` crate (BSD-3-Clause). Scaled YCoCg follows van Waveren & Castaño, "Real-Time YCoCg-DXT Compression" (2007), which the HAP specification references. |
| BC1 / BC3 / BC4 CPU decode (`bc.rs`) | The public S3TC / RGTC definitions with DXVKit's integer conventions (bit-replicated 565 expansion, truncating interpolation). |
| BC7 CPU decode (`bc7.rs`) | Written for this project from the public BPTC definition (Khronos Data Format Specification 1.3, "BPTC Compressed Texture Image Formats"; Microsoft "BC7 Format"). Verified bit-exact against the GPU's own BC7 decoder on 4096 random blocks of every mode. |
| Exact media time (`time.rs`) | Adapted from storytold/effectcraft (MIT) via Loombda-rs; notice kept. |
| Frame cache, transport, sources, test pattern | Loombda-rs (Amrit's), ported. |
| macOS native decode (`dxvkit-plus-apple`) | Loombda-rs's island, written against Apple's AVFoundation / CoreVideo / Metal documentation through the `objc2` bindings. The `CVMetalTextureCache` route was written from Apple's CoreVideo documentation after reading how Cutlass (MIT OR Apache-2.0) does it; no Cutlass code is in this repository. |
| WGSL conversion (`dxvkit-plus-wgpu`) | Loombda-rs; YCbCr matrices from ITU-R BT.601 / BT.709 / BT.2020. |
| DXV decode (`core::dxv`) | Carried over (copied with `cp -p`, decoding steps unchanged) from Loombda-rs `crates/loombda-media/src/dxv/` (AmritusG/Loombda-rs @ 6795193, Amrit's own), a line-for-line safe-Rust port of DXVKit 1.0.0 (`src/packet.c`, `blockcopy.c`, `legacy.c`, `hq.c`, `pixels.c`; AmritusG/DXVKit @ 36209e0, Amrit's own MIT C library, itself a clean-room implementation of its own `docs/spec/DXV_FORMAT.md`). Verified here: bit-exact on DXVKit's 334 test vectors and against the C core built from that commit. ffmpeg was used only as an external comparison oracle for the premultiplied-alpha convention (never read, never linked). |
| DXV High Quality GPU conversion (`DXV_HQ_WGSL`) | Written for DXVKit Plus: BC4 palettes from the public RGTC definition, YCoCg folding from DXVKit spec 8.4 (the CPU oracle's integer arithmetic). |
| LMBD container (`core::lmbd`), `docs/LMBD.md` | Reimplemented in Rust from the frozen v1 contract of Amrit's original LMBD codec (`loomda-codec` @ c6d80e6: `lmbd_format.h`, `lmbd_write.c`, `lmbd_encode.cpp`, `lmbd_probe.cpp`). Zstd decode through libzstd 1.5.7 (BSD-3-Clause, via the `zstd` crate) by default, or `ruzstd` (MIT, pure Rust) with the `zstd-rust` feature; both are third-party libraries used unchanged, and decode identically. |
| LMBD encoder (`dxvkit-plus-lmbd-enc`) | A line-by-line Rust port of the parts of bc7enc_rdo (Richard Geldreich, Jr.; MIT or Unlicense) the original encoder runs: `bc7enc.cpp` (4-mode BC7), `ert.cpp` (entropy reduction), the BC7 path of `rdo_bc_encoder.cpp`. Fused multiply-adds placed where Apple clang's `-ffp-contract=on` fuses them (the list taken from clang's own IR), so the output is byte-identical (checked on 1080p frames and five vectors). Not ported: bc7e.ispc (Apache-2.0, the original's optional x86-only `--allmode` path), the perceptual and reduced-entropy options. |
| LMBX (`core::lmbx`, `lmbd_enc::lmbx`, `wgpu::lmbx`, `docs/LMBD.md` §11) | From Amrit's LMBX prototypes (`loomda-codec` @ c6d80e6: `phase0-bench/lmbx-b0/lmbx_b1d.cpp`, `src/lmbx_pack.cpp`, `macos-player/Sources/LmbdMetalCore/LmbxMetal.swift`), read in a fresh clone and rewritten in Rust/WGSL as a new, frozen v1 bitstream: the B1d encoder's decisions (per-block quantiser from AC energy, deadzone, RDOQ with an end-of-block trellis, low-frequency protection) are ported; the integer inverse transform, the 32x32 tile / stream-per-plane layout, the table format, the quantiser map and the GPU decoder are new. The integer DCT basis is the textbook orthonormal DCT-II rounded to 12 bits (no codec standard's transform was used); rANS follows Duda's published description ("Asymmetric numeral systems", 2013) and the public-domain ryg_rans conventions (32-bit state, byte renormalisation) — ideas only, no code. Nothing in loomda-codec is GPL/LGPL; lodepng (zlib) is not used. |
| LMB7 compatibility vectors | Our own CC0 patterns, encoded by the original C tools built on this machine (`scripts/mint-lmbd-vectors.sh`). No other file from loomda-codec is in this repository: its test media include third-party footage and Amrit's own clips, which stay out. |

LMBD's profile `0x10` is LMBX, frozen and shipped in 0.5.0 (above). The DXV
semi-transparent edge question that held DXV back was closed in Loombda-rs (W3 P1-1, f57d1da) with
no code change: the port equals DXVKit's C core, and ffmpeg's different edges come from it treating
DXT5 colour as premultiplied. Amrit's ruling: straight is the default; premultiplied is a per-clip
choice (`AlphaMode`).

## Encoders — one warning (HAP; LMBD encoding ships in 0.2.0)

A future HAP encoder must **not** derive from the YCoCg encoder inside Vidvox's `hap-qt-codec`: it
is LGPL-2.1 (Id Software). The HAP Q encode path is to be written from the paper the HAP
specification cites — J.M.P. van Waveren & I. Castaño, "Real-Time YCoCg-DXT Compression" (NVIDIA,
2007) — and nothing else.

## Why there is no FFmpeg, GStreamer or libmpv

- **Licences.** FFmpeg and libmpv are LGPL/GPL and GStreamer is LGPL; linking them would put
  copyleft obligations on an MIT library and on every application that uses it. `deny.toml` bans
  their Rust bindings, and `cargo deny` gates every release.
- **Clean room.** FFmpeg's DXV and HAP decoders are not a source for this project: our HAP decoder
  comes from the published specification only. FFmpeg is used *outside* the repository as a tool
  for exactly one thing — wrapping our own PNG frames in an uncompressed `rawvideo` container so
  Apple's encoders can read them when minting fixtures (`scripts/mint-*.sh`); no ffmpeg codec
  touches the pixels, and nothing links it.
- **Fit.** VJ codecs (HAP, DXV) are GPU texture formats: the right decode is "decompress to blocks,
  upload blocks", a few hundred lines of Rust. Everything else is decoded best by the operating
  system's own hardware decoders (VideoToolbox on macOS, Media Foundation on Windows), imported into
  the GPU without a copy. A general-purpose media framework adds neither.

## Fixtures

Every fixture is our own procedural footage (CC0-1.0), each with an `.attribution` sidecar holding
its SHA-256 (checked by `cargo test`). DXVKit's test vectors in `tests/fixtures/dxvkit/` are MIT
(DXVKit's licence): the GlEnc, synthetic and invalid groups only (74 rows, 34 files, all synthetic
content), the same subset Loombda-rs commits. The Resolume-encoder group, anything derived from
it and the Arena forms are never committed; the full 334-row run points `DXVKIT_DIR` at a DXVKit
checkout. No third-party media and none of Amrit's footage is committed. Benchmark clips are minted locally by `scripts/mint-bench.sh` and never committed.

## Licence inventory (this release)

See `ATTRIBUTION.md` for the table of every adapted file and asset. Third-party code: EffectCraft
(MIT, notice in `LICENSES/effectcraft-MIT.txt`) and bc7enc_rdo (MIT / Unlicense, notice in
`LICENSES/bc7enc_rdo.txt`). Amrit's own code: DXVKit (MIT, notice in
`LICENSES/DXVKit-MIT.txt`), Loombda-rs and the LMBD codec `loomda-codec` (both released here under
MIT). No GPL, LGPL, AGPL, EPL or MPL
code or dependency.
