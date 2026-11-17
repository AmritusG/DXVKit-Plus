# W6 — encoders and the QuickTime writer (v0.12.0), 2026-11-17

Made from glenc-rs's goal run (brief `~/claude-handoffs/glenc-rs_2026-11-17_goal-build-to-swap_brief.md`, point 2:
"Encoders live in DXVKit Plus (MIT): DXV, HAP, and the QuickTime writer, with tests there"). Amrit's approval of the
move under MIT: 2026-11-14 ("Yes"; ROADMAP §5, W3 brief).

## What shipped
- **`dxvkit-plus-enc`** (new, safe Rust): `dxv` (DXT1 / DXT5 / YCG6 / YG10 at DXVKit's BEST effort), `hap` (Hap1 /
  Hap5 / HapY / HapA / HapM, single and chunked 2…64, refined BC4 switch), `mov` (`MovWriter`, `derive_timescale`;
  `in32` / `lpcm` v2 audio, largesize `mdat` + `co64` past 4 GiB, `udta/©swr`), `alpha` (straight-alpha rule).
- **`dxvkit-plus-apple`** (macOS): `BgraReader` (GlEnc's AVAssetReader 32BGRA settings; the buffer's attachments),
  `read_pcm_s32`, `AvWriter` (ProRes 422 / HQ / LT / Proxy / 4444, H.264, HEVC, Motion JPEG; LPCM / AAC 256 kbit/s),
  `scale_bgra` (vImage Bilinear / Lanczos).

## How identity was proven
An oracle built from Swift GlEnc 1.6.2's source runs GlEnc's own encoder and writer classes on raw frames
(glenc-rs `tools/swift-enc-oracle`). Swift's DXVKit is DXVKit's C core compiled by SwiftPM with Apple clang's
default `-ffp-contract=on`: `enc_bc.o` has 24 `fmadd` / `fnmadd` (19 in `dxvk_bc1_encode`'s principal-axis search,
5 in `dxvk_bc4_encode_fine`'s refit); the port reproduces them with `mul_add` in clang's `fmuladd` order. Fused and
unfused give the same blocks on 20,000,000 random BC1 and BC4-fine blocks, so the choice is invisible in practice;
the port follows the machine code. (That 20-million-block comparison was a scratch tool built against Loombda-rs's unfused
port, not kept; the oracle's `enc_bc.o` was compared instruction for instruction with the `enc_bc.o` of Swift GlEnc
1.6.2 built from its source on this Mac — no shipped 1.6.2 binary is installed to compare.)

| Part | Compared against Swift GlEnc | Result | Evidence |
|---|---|---|---|
| DXV | 4 variants × (88 generated frames, 1x1 … 1920x1080, + 3 testsrc2 PNG frames), and × 80 real 1080p frames | 364 / 364 and 320 / 320 packets identical | `docs/data/w6-dxv-vs-swift-glenc.txt` |
| HAP | 5 codecs × chunks 1,2,3,4,7,64 × 3 alpha tags × 9 sizes × 8 patterns + PNGs; 4096² past 16 MiB; refined BC4 | 10,269 + 6 + 10,224 packets identical | `docs/data/w6-hap-vs-swift-glenc-*.txt` |
| QuickTime writer | 123 generated cases (every FourCC, sizes to 65535, 15 rates, audio 1–8 ch at 44.1–192 kHz, writer strings, 0-byte / 3 MiB packets) | 105 identical, 18 refused by both, 0 different | `docs/data/w6-mov-vs-swift-glenc.txt` |
| > 4 GiB | 6 real ~4 GiB files around every `u32::MAX` boundary | 6 / 6 identical | `docs/data/w6-mov-over-4gib.txt` |
| BGRA reader | 18 movies, 1,948 frames (ProRes 422 / 4444, H.264, HEVC, MJPEG) | every frame hash and time equal | agent run (glenc-rs E1) |
| AvWriter | ProRes ×5, MJPEG, H.264 / HEVC in mov / mp4 (through glenc-cli on 8 sources) | ProRes whole files byte-identical when written in the same second, MJPEG but 3 time bytes; container atoms (times zeroed) and decoded pictures equal in 76 / 80; the 4 H.264 cases differ as Swift differs from itself (5 different pictures in 6 Swift runs) | glenc-rs `docs/data/e3/platform-deep-compare.jsonl`, `h264-swift-run-to-run.txt` |
| vImage | 11 size pairs × 2 filters | 22 / 22 equal | agent run |

Committed tests (no media, no private paths): `dxv_encoder.rs` (decode = packed planes; 48 golden), `hap_structure.rs`
(12), `hap_golden.rs` (56), `mov_writer.rs` (20), `mov_golden.rs` (2), `glenc_av.rs` (13), `alpha` units (2).

## Deviations (on purpose)
- Swift writes each packet with one `FileHandle.write` (macOS refuses ≥ 2 GiB in one write, as the agent's run found); Rust
  writes packets up to the format's `u32::MAX`. No real frame comes near (not part of the oracle comparison).
- Where Swift crashes (fps 0 or ≥ 8388608, sizes 0 / > 65535, > 65535 audio channels at ≤ 65535 Hz: SIGTRAP in the
  oracle run), Rust returns an error; a directory at the output path is refused (Swift's `removeItem` would delete it).
- `HapEncoder::new` takes chunks 1…64 only (Swift's encoder does not check; its GUI clamps and its CLI rejects).
- Swift's `AVAssetWriterInput` throws an Objective-C exception with more than 2 audio channels into ProRes / H.264
  ("Missing required key AVChannelLayoutKey"); `AvWriter` returns "rejected the audio input" instead.

## Gates
fmt ok · clippy --workspace --all-targets --release -D warnings ok · `DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace
--release`: 233 passed, 0 failed, 3 ignored · cargo deny ok.
