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

DXV decoding (a safe-Rust port of DXVKit's decoder, verified against DXVKit's 334 test vectors in
Loombda-rs) is **not in this release**: it waits for an open accuracy question on semi-transparent
edge pixels to be settled first (see the README roadmap).

## Encoders (not in this release) — one warning

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
its SHA-256 (checked by `cargo test`). No third-party media and none of Amrit's footage is
committed. Benchmark clips are minted locally by `scripts/mint-bench.sh` and never committed.

## Licence inventory (this release)

See `ATTRIBUTION.md` for the table of every adapted file and asset. Third-party code: EffectCraft
(MIT, notice in `LICENSES/effectcraft-MIT.txt`). Amrit's own code: DXVKit (MIT, notice in
`LICENSES/DXVKit-MIT.txt`) and Loombda-rs (released here under MIT). No GPL, LGPL, AGPL, EPL or MPL
code or dependency.
