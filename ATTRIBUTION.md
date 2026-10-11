# Attribution

DXVKit Plus is MIT-licensed (`LICENSE`, Copyright (c) 2026 Amrit Rosell). This file lists every
piece of code and every asset that came from somewhere else, with its licence. `docs/PROVENANCE.md`
explains how each part was written.

## Code

| Code in this repo | From | Licence |
| --- | --- | --- |
| `crates/dxvkit-plus-core/src/time.rs` | storytold/effectcraft @ 813c7c4 `crates/time/src/lib.rs` (Copyright (c) 2026 ArtCraft Team and the EffectCraft contributors), via Loombda-rs `crates/loombda-time` | MIT (`LICENSES/effectcraft-MIT.txt`) |
| `crates/dxvkit-plus-core/src/container.rs` (atom walk, sample tables), `src/bc.rs` (BC1/BC4 integer conventions) | DXVKit 1.0.0 (Amrit Rosell; AmritusG/DXVKit @ 36209e0) `src/mov.c`, `src/pixels.c`, via Loombda-rs `crates/loombda-media` | MIT (`LICENSES/DXVKit-MIT.txt`) |
| `crates/dxvkit-plus-core/src/{hap,cache,transport,source,testpattern,bytes,error}.rs`, `crates/dxvkit-plus-apple/src/{decode,objc,surface}.rs` (IOSurface route), `crates/dxvkit-plus-wgpu` (BC texture helpers, WGSL conversion), `crates/dxvkit-plus-core/examples/fuzz.rs`, `tests/support/qt_movie.rs` | Loombda-rs (Amrit Rosell; AmritusG/Loombda-rs @ d2c0c43), ported and adapted | Amrit's own; released here under MIT |
| `crates/dxvkit-plus-core/src/dxv/{mod,blockcopy,hq,lzf,rgba}.rs` | Loombda-rs (Amrit Rosell; AmritusG/Loombda-rs @ 6795193) `crates/loombda-media/src/dxv/`, copied with `cp -p`; itself a port of DXVKit 1.0.0 (Amrit Rosell; AmritusG/DXVKit @ 36209e0) `src/packet.c`, `blockcopy.c`, `legacy.c`, `hq.c`, `pixels.c` | MIT (`LICENSES/DXVKit-MIT.txt`); Loombda-rs's port: Amrit's own, released here under MIT |
| `clippy.toml`, `rustfmt.toml` | storytold/effectcraft, via Loombda-rs | MIT (`LICENSES/effectcraft-MIT.txt`) |
| `crates/dxvkit-plus-core/src/hap.rs` | written from the public HAP specification `documentation/HapVideoDRAFT.md` (Vidvox/hap @ d847f6b); no code read or copied | spec only (repository BSD-2-Clause) |
| `crates/dxvkit-plus-core/src/bc7.rs` | written for DXVKit Plus from the public BPTC definition (Khronos Data Format Specification 1.3; Microsoft "BC7 Format") | own code, MIT |
| `crates/dxvkit-plus-lmbd-enc/src/{bc7enc,ert,rdo,tables}.rs` | bc7enc_rdo (Richard Geldreich, Jr.; github.com/richgel999/bc7enc_rdo) `bc7enc.cpp`, `ert.cpp`, the BC7 path of `rdo_bc_encoder.cpp`, ported to Rust — read from the copy vendored in Amrit's loomda-codec @ c6d80e6 | MIT (dual MIT / Unlicense upstream; `LICENSES/bc7enc_rdo.txt`) |
| `crates/dxvkit-plus-core/src/lmbd.rs`, `crates/dxvkit-plus-lmbd-enc/src/lib.rs` (container, chunking, encoder driver), `docs/LMBD.md` | the LMBD v1 format and `lmbd-encode` / `lmbd-write` / `lmbd-probe` of Amrit's loomda-codec @ c6d80e6 (AmritusG/loomda-codec), reimplemented in Rust; the spec is written in our own words | Amrit's own; released here under MIT |
| `crates/dxvkit-plus-apple/src/surface.rs` (`TextureCacheImporter`) | written from Apple's CoreVideo documentation; the technique (a `CVMetalTextureCache` adopted through `wgpu_hal`) was learned from 1mrnewton/cutlass @ 22437e2 `crates/cutlass-compositor/src/metal_import.rs` — read, not copied | own code, MIT (Cutlass: MIT OR Apache-2.0) |

## Assets

| Asset | What | Licence |
| --- | --- | --- |
| `tests/fixtures/tiles_*.{mov,mp4}` (16 files, sidecars `*.attribution`) | our own procedural tile pattern (`testpattern::tiles`), 128x72, 24 frames, 30 fps; encoded with glenc-cli (Amrit's encoder), the four `tiles_{dxt1,dxt5,ycg6,yg10}.mov` with DXVKit's `dxvtool encode` (copied from Loombda-rs) | CC0-1.0 |
| `tests/fixtures/dxvkit/` (`vectors.tsv` + 34 streams, sidecars `*.attribution`) | DXVKit 1.0.0's test vectors, the GlEnc / synthetic / invalid groups (synthetic content: DXVKit's own pattern, encoded by GlEnc or built by hand), with their reference SHA-256 rows | MIT (`LICENSES/DXVKit-MIT.txt`) |
| `tests/fixtures/synthetic_hap7.mov`, `tests/fixtures/synthetic_haph.mov` (sidecars `*.attribution`) | HAP R (random BC7 blocks, all 8 modes) and HAP HDR (solid BC6H blocks), written byte by byte by `crates/dxvkit-plus-core/examples/mint_synthetic.rs` (no encoder writes these variants) | CC0-1.0 |

No third-party media, and none of Amrit's footage, is in this repository.

| `tests/fixtures/lmbd_*.lmbd` (5 files, sidecars `*.attribution`) | LMBD compatibility vectors: our own procedural patterns (`testpattern::tiles` / `plasma`), written by the ORIGINAL LMBD encoder (`scripts/mint-lmbd-vectors.sh`) | CC0-1.0 |

## Dependencies

Every dependency is permissive (MIT, Apache-2.0, BSD-2/3, Zlib, ISC, MIT-0), enforced by
`cargo deny check licenses bans sources` (`deny.toml`). The one crate-scoped exception is
`unicode-ident` (Unicode-3.0, permissive; Unicode's data tables), a build-time dependency of
serde's derive macros. FFmpeg, libmpv and GStreamer bindings are banned. LMBD's Zstd goes through
the `zstd` / `zstd-safe` / `zstd-sys` crates (BSD-3-Clause, Alexandre Bury), which build the
bundled libzstd 1.5.7 (Meta Platforms, Inc. and affiliates; dual BSD-3-Clause / GPLv2, used under
BSD-3-Clause; only `lib/common`, `lib/compress` and `lib/decompress` are compiled — no legacy
formats, no dictionary builder): the encoder always, the decoder by default (core feature
`zstd-c`). With `zstd-rust` the decoder uses `ruzstd` (MIT, pure Rust) instead. Notices:
`LICENSES/libzstd-BSD-3-Clause.txt`, `LICENSES/zstd-rs-BSD-3-Clause.txt`.
