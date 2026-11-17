Licence texts of code this repository adapts (see ../ATTRIBUTION.md):

- `effectcraft-MIT.txt` — storytold/effectcraft (exact media time, `crates/dxvkit-plus-core/src/time.rs`).
- `bc7enc_rdo.txt` — bc7enc_rdo (Richard Geldreich, Jr.; MIT / Unlicense, used under MIT): the BC7 encoder and RDO pass ported into `crates/dxvkit-plus-lmbd-enc`.
- `DXVKit-MIT.txt` — DXVKit 1.0.0 (Amrit Rosell): the QuickTime atom walk and BC integer conventions.
- `libzstd-BSD-3-Clause.txt` — Zstandard / libzstd 1.5.7 (Meta Platforms, Inc. and affiliates; dual BSD-3-Clause / GPLv2, used under BSD-3-Clause), compiled into the LMBD encoder and, by default, the LMBD decoder.
- `zstd-rs-BSD-3-Clause.txt` — the `zstd`, `zstd-safe` and `zstd-sys` crates (Alexandre Bury; BSD-3-Clause; the three ship the same text), the Rust bindings that build and call libzstd.
- `texturedspenc-MIT.txt` — FFmpeg `libavcodec/texturedspenc.c` (Vittorio Giovara; MIT, as its header states): the BC1 encoder of the HAP path (`crates/dxvkit-plus-enc/src/hap/bc1.rs`, via Swift GlEnc's port). Only this MIT-licensed file's algorithm; nothing LGPL from FFmpeg.
- `GlEnc-Snappy-BSD-3-Clause.txt` — Swift GlEnc's own Snappy compressor (Amrit Rosell; BSD-3-Clause), ported in `crates/dxvkit-plus-enc/src/hap/snappy.rs`.
