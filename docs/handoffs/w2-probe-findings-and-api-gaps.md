Project:    DXVKit-Plus
pass:       w2-probe-findings-and-api-gaps
STATUS:     COMPLETE
parent:     d2ec731 (v0.8.0 + W1 handoff)
result:     70b5c746bc40e583fd7b2a724a7d5a1d56b07e5c (main, pushed; tag v0.9.0); this handoff and the closed CHECKPOINT.md are the commit after it
session:    990967ee-5369-4615-9d6f-a0499885b6bd
written_at: 2026-11-14T23:30Z

# W2 — the eight glance-core C1 findings fixed as decided, the MediaInfo API gaps closed; 0.9.0

## Answer in one line
0.9.0 routes movies as Swift Glance does (DXV first; HAP only as the first video track; else the platform), opens a chosen track, judges content before the name, plays a cut-off file up to the cut, offers unreadable non-DXV / non-HAP movies (fragmented MP4) to the platform, turns frames larger than the GPU into error values (and asks the adapter for its 16384), decodes BC7 in the shader on the exact path (reserved blocks transparent black on every GPU), builds for Intel and iOS, and adds `MediaInfo::{track, dxv_variant, alpha, codec_name, container_name}`, `names::*` and `container::first_video_fourcc`.

## For Amrit (one line each)
- Intel Macs: the x86_64 build only loads when built for macOS 10.14.4 or later (rustc's Intel default, 10.12, links a Swift library through a path that does not exist); this repo now builds both halves for macOS 11.0. Say if Intel Macs older than Big Sur (11.0) must run the family's apps.
- iOS: the Apple island builds and links for iPhone / iPad with no code change beyond a `cfg`; nothing ran on a device. This repo's own iOS builds target iOS 15.0 (some value must be set for the link to work); GlancEhr's real floor is yours to set when it starts.
- BC7 on the exact path costs about 4-6x the sampled pass (about 0.21 ms per 1080p layer, 3.4 ms for 16 layers per step); HAP R / LMB7 clips on the exact path are the most expensive kind. The sampled path is unchanged.
- A cut-off file now plays up to the cut (Swift's behaviour, for DXV too, which DXVKit's C demuxer refuses): the clip's frame count is still the full count from its sample table, and the frames after the cut fail.
- Resolume Arena was running on this Mac during every measurement (not touched); speeds are comparisons, not absolute figures.

## DIAGNOSE (a source per fact; glance-core and Swift Glance read-only; Swift paths relative to glance-core/.ref)

### Finding 1 — the device's texture limit
| Fact | Source |
|---|---|
| `Gpu::with_adapter` requested `..Default::default()` limits: `max_texture_dimension_2d` 8192 | v0.8.0 `crates/dxvkit-plus-wgpu/src/lib.rs:111-119` |
| The M5 Max adapter offers 32768 under wgpu 30.0.1 (glance-core's `glance-proof gpu` prints 16384 because it caps at `MAX_EDGE`) | `tests/limits.rs` output, `docs/data/w2-red-limits-v0.8.0.txt` |
| A texture past the limit: wgpu raises a validation error (here caught by an error scope: InvalidDimension, 9628 given, limit 8192) and no `GpuError` comes back (glance-core's report: `Ok` with an invalid texture) | RED test `a_frame_larger_than_the_device_is_an_error_value`, `docs/data/w2-red-limits-v0.8.0.txt`; c1-findings brief r2 finding 1 |
| glance-core's workaround: its own device with `max_texture_dimension_2d = adapter.min(MAX_EDGE)` and a size check before `show` | glance-core `crates/glance-gpu/src/lib.rs:32-65` |

### Finding 2 — track choice (Swift Glance quoted)
- `let dxv = try? DXVDemuxer.demux(url: url)` / `let hap = dxv == nil ? (try? HAPDemuxer.demux(url: url)) : nil` / `if let idx = dxv { routeToDXV } else if let hap { routeToHAP } else { routeToAV }` — `glance/Sources/Glance/PlayerModel.swift:209-229`.
- DXV = the first `trak` whose first sample entry is DXV: `read_track` returns 0 for any other track (`if (entries < 1 || !is_dxv_fourcc(fourcc, &gen)) return 0;`), the open loop `break`s after the first DXV track, success or error — `dxvkit/src/mov.c:165-207, 351-369`.
- HAP = the first video track: `parseMoov` stops at the first `parseTrakIfVideo` that returns an index (`return false  // stop walking once we have it`), and that function throws `notHAP(fourCC:)` for any codec but `Hap1 / Hap5 / HapY / HapM / HapA` — `glance/Sources/GlanceCore/HAPDemuxer.swift:169-191, 255-288`.
- v0.8.0 took the first HAP track anywhere, else the first DXV track, else the first video track — `crates/dxvkit-plus-core/src/source.rs:237`.

### Finding 3 — a cut-off track (Swift Glance quoted)
- Swift's HAP demuxer builds the frame index from the tables alone: `frames.append(HAPFrameEntry(fileOffset: chunkOffset + offsetInChunk, size: size, ...))`, no check against the file size — `HAPDemuxer.swift:468-529`; the frames past the cut fail when read.
- DXVKit refuses: `if (off > file_size || sz > file_size - off)` → corrupt — `dxvkit/src/mov.c:274`; v0.8.0 did the same for every codec ("sample outside the file", `container.rs:436-438`).
- Decided (brief): keep the whole samples, fail the cut ones per frame, the same rule for DXV and HAP.

### Finding 4 — name before content (Swift Glance quoted)
- Swift routes by content only: the three demuxers are tried on every file whatever its name (`PlayerModel.swift:209-229` above); the extension is used only for the message's container name (`AVPlaybackController.swift:85`: `url.pathExtension.isEmpty ? "this container" : url.pathExtension.uppercased()`) and for the Matroska sniff (`:110-112`).
- v0.8.0 returned `Unsupported` for an image extension before trying the content, and tried LMBD for `.lmbd` before `Movie::open` — `source.rs:202-234`.

### Finding 5 — a container our parser calls corrupt
- v0.8.0: only `NotMovie | NoVideoTrack` fell through to `backend.open`; `Corrupt` was returned — `source.rs:266-288`. One damaged video track failed the whole movie (`read_track(..)?` in the trak loop, `container.rs:515`).
- Verified with a real fragmented MP4 first: AVFoundation's CMAF segment writer (AVAssetWriter, `outputFileTypeProfile = .mpeg4CMAFCompliant`) gives `ftyp + moov (stsz count 0, mvex) + 2 x (moof + mdat)`; `Movie::open` on it: `Corrupt("sample count is 0 or above 2^24")`; AVFoundation plays it (60 frames). A plain `movieFragmentInterval` writer re-wrote a non-fragmented file on finish (stsz count 30; seen in this session, no log kept), so the CMAF route was used. Generator: `scripts/make-fragmented-mp4.swift`; the file is ours (CC0): `tests/fixtures/frag_h264.mp4`.

### Finding 6 — the Apple island on the other targets
- Homebrew's rustc 1.99.1 on PATH has no x86_64 / iOS std; rustup's stable 1.100.0 (the toolchain glance-core's CI uses) has `x86_64-apple-darwin`, `aarch64-apple-ios`.
- `x86_64-apple-darwin`: `cargo check --workspace --all-targets` and the release build pass. The release binary did **not load** under Rosetta: `dyld: Library not loaded: @rpath/libswiftCoreMedia.dylib ... no LC_RPATH's found`. Cause (measured): rustc's default deployment target for x86_64 is 10.12 (`LC_VERSION_MIN_MACOSX version 10.12`); below 10.14.4 the macOS SDK links CoreMedia's Swift overlay through `@rpath` (Swift was not in the OS). Built with `MACOSX_DEPLOYMENT_TARGET=11.0` it links no Swift library and runs.
- `aarch64-apple-ios`: the workspace checked because the island was `cfg(target_os = "macos")` (it built empty). With the `cfg` widened to iOS the island compiles unchanged — objc2-av-foundation / core-media / core-video / io-surface / metal and wgpu-hal's Metal backend all build for iOS (AVFoundation, VideoToolbox, `CVMetalTextureCache` and IOSurface exist there) — and an example binary links once `IPHONEOS_DEPLOYMENT_TARGET` is set (with none set, rustc links for its default 10.0 while the cc crate builds libzstd's C for the SDK's 27.2: `Undefined symbols: ___chkstk_darwin`; any explicit value, 10.0 included, aligns both; this repo uses 15.0). `docs/data/w2-apple-targets.txt`. Not run (no device; the simulator target is not installed). So it was cfg work and small: done.

### Finding 7 — the FourCC reader
- Swift's walk: `moov` → each `trak` → `mdia > hdlr` (handler at payload + 8 must be `vide`) → `minf > stbl > stsd`, FourCC at `stsd` payload + 12; 32-bit, 64-bit and to-end sizes; a malformed atom ends the parent's walk — `glance/Sources/GlanceCore/DXVDetector.swift:60-174`. glance-core's 60-line port (`route.rs:155-237`), cross-tested on 226 files, adds: a FourCC with a non-ASCII byte is unreadable and the next track answers. Ported as `container::first_video_fourcc(_in)` over `ReadAt`.

### Finding 8 — reserved-mode BC7 and the BC7 spec as core::bc7 implements it
- `core::bc7::bc7_block` (Khronos Data Format 1.3 "BPTC", Microsoft "BC7 Format"): mode = trailing zeros of byte 0 (byte 0 = 0: reserved, transparent black); fields LSB-first: partition (0/4/6 bits), rotation (2, modes 4-5), index selection (1, mode 4); endpoints channel-major R, G, B (cb bits), A (ab bits), each subset's two endpoints; p-bits per endpoint (modes 0, 3, 6, 7) or shared per subset (mode 1), appended as the LSB of every channel; expansion to 8 bits by bit replication (alpha 255 when ab = 0); indices with each anchor one bit shorter (texel 0, A2 / A3A / A3B); modes 4-5 a second index set (texel 0 one bit shorter), mode 4's selection swaps which set colour and alpha use; weights W2 / W3 / W4, `((64 - w) * e0 + w * e1 + 32) >> 6`; rotation swaps alpha with R, G or B — `crates/dxvkit-plus-core/src/bc7.rs:18-264`.
- WGSL / naga 30: `u32` arithmetic only (no 64-bit), so the 128-bit block is four words and a field is read from at most two of them (`bc7_bits`, every shift below 32); `countTrailingZeros` gives the mode; module-scope `const` arrays may be indexed at run time (naga validates it and writes MSL and HLSL: `conversion_shaders_translate_to_msl_and_hlsl`); `partition` is a reserved word in WGSL (naga refused it), so the variable is `part`. One texel's decode needs only its subset's endpoints and its own index, whose bit offset is `istart + i * ib - (anchors before i)`; the tables are packed from core's (`P2` 1 bit, `P3` 2 bits per texel, anchors in one word) by a script over `bc7.rs`, and the sweep checks them (every mode forced in turn on 1080p / 2160p frames = every partition many times).
- The finding's measurements are glance-core's (CI run 43638498291: GitHub's "Apple Paravirtual device" gives `[0, 0, 0, 255]` for reserved blocks). On this M5 Max the hardware already gives `[0, 0, 0, 0]`, so the RED here is the route (`direct_view()` was `Some` on the exact path), not the value.

### API gaps (glance-family FINDINGS 6)
| Gap | How Swift Glance gets it | How glance-core works around it today | 0.9.0 |
|---|---|---|---|
| (a) the DXV variant | from the first packet through DXVKit (`DXVKitBridge.swift:220-234`), named in the title as DXV3 / DXV3 alpha / DXV3 HQ ... (`ClipTitle.swift:26-36`) | `dxv_variant_label` from a decoded frame's planes (glance-core `crates/glance-media/src/lib.rs:56-60`) | `MediaInfo::dxv_variant: Option<dxv::Variant>` (first packet) |
| (b) a native clip's stated alpha | `AlphaTypeProbe.probeAVFoundation`: `ContainsAlphaChannel` extension, else `ap4h` / `ap4x`; premultiplied only if `AlphaChannelMode` says so (`AlphaType.swift:155-171`) | parity item left open ("native codecs' stated alpha mode — DXVKit Plus API gap", C1 handoff) | `NativeInfo::alpha` from the format description (Swift's rule) → `MediaInfo::alpha: StatedAlpha`, for every kind |
| (c) codec / container names | `DXVDetector.displayName` (`DXVDetector.swift:28-57`), container = extension upper-cased (`AVPlaybackController.swift:85`), the line (`:86-91`), Matroska `V_` (`:110-123`) | its own `display_name`, `unsupported_message`, `first_video_fourcc`, `matroska_codec` (`route.rs:108-268`) | `names::{codec_name, codec_label, container_name, unsupported_message}`, `MediaInfo::{codec_name, container_name}` |
- Hap7 / HapH names: the HAP specification's "Names and Identifiers" calls them "Hap R" and "Hap HDR" (Vidvox/hap `documentation/HapVideoDRAFT.md`, read 2026-11-14); the family writes "HAP" (Swift Glance: "HAP Q Alpha"), so "HAP R" and "HAP HDR", as the brief proposed.

## The failing tests first (RED on v0.8.0)
| Finding | Test | RED on v0.8.0 | Data |
|---|---|---|---|
| 1 | `wgpu/tests/limits.rs` `the_device_asks_for_the_adapters_largest_texture`, `a_frame_larger_than_the_device_is_an_error_value` | 8192 vs 16384; `Ok` + a wgpu validation error | `docs/data/w2-red-limits-v0.8.0.txt` |
| 2 | `core/tests/probe.rs` `tracks_are_chosen_in_swift_glances_order` | DXV + HAP → `Hap` | `docs/data/w2-red-probe-v0.8.0.txt` |
| 3 | `a_cut_off_track_keeps_its_whole_samples` | `corrupt file: sample outside the file` | same |
| 4 | `the_content_decides_before_the_name` | HAP movie as `clip.png` → `Image("Invalid PNG signature")` | same |
| 5 | `a_movie_our_parser_refuses_goes_to_the_backend_unless_it_is_dxv_or_hap` | `Corrupt("sample count is 0 ...")`, backend never asked | same |
| 7 + gaps | `core/tests/api_gaps.rs` | does not compile (18 errors: no `first_video_fourcc`, `names`, `StatedAlpha`, new fields) | `docs/data/w2-red-api-gaps-v0.8.0.txt` |
| 8 | `wgpu/tests/exact.rs` `bc7_is_decoded_in_the_shader_on_the_exact_path` | exact output already equal on the M5 Max; fails at `direct_view().is_none()` (BC7 still on the hardware route) | `docs/data/w2-red-bc7-v0.8.0.txt` |
| 6 | build commands (below) | x86_64 binary does not load; iOS island empty | receipts |

## What shipped
1. **Device limit** (`dxvkit-plus-wgpu`): `Gpu::with_adapter` requests `max_texture_dimension_2d = adapter.min(MAX_EDGE)`; `check_device_size` (public) runs before every texture the renderer makes and in `render_native`; the Apple island's two imports check too. Error text: "a WxH frame is larger than this GPU's largest texture (N pixels)".
2. **Track choice**: `source::probe` → `pick`: chosen (`ImportOptions::track`) or Swift's order; `MediaInfo::track`; `source::open` opens the probed track (`probed_track`). A damaged DXV track the order reaches does not stop a HAP first track or a native first track (Swift: DXV fails → HAP → AV); if nothing else plays, its error stands.
3. **Cut-off track**: `build_samples` no longer refuses samples past the end (the 300 MB per-sample cap stays; a uniform size may not claim more samples than file bytes); `Movie::read_sample` checks the bounds before allocating and returns `Corrupt("sample N lies past the end of the file (cut off)")`.
4. **Content first**: probe order dir → LMBD magic → movie → PNG signature → image extension → backend.
5. **Backend fallback**: per-track damage (`DamagedTrack`, `Movie::damaged`, `Movie::open_all`, `VideoTrack::index`); a damaged non-DXV/HAP first track, an unreadable `moov` (unless `first_video_fourcc` names DXV / HAP) or a non-movie goes to the backend; without one, or when it refuses too, our parser's error is returned.
6. **Apple targets**: `.cargo/config.toml` (`MACOSX_DEPLOYMENT_TARGET = 11.0`, `IPHONEOS_DEPLOYMENT_TARGET = 15.0`, env wins); `dxvkit-plus-apple` compiled for `any(macos, ios)`.
7. **`container::first_video_fourcc(_in)`**.
8. **BC7 on the exact path**: `BC_EXACT_WGSL` mode 4 (`bc7`, `bc7_bits`, `bc7_mode`, packed `BC7_P2` / `BC7_P3` / `BC7_ANCHORS`, `BC7_W2/3/4`); `program_for` sends BC7 `Rgba` frames to it on Exact; `raw_format(Bc7) = Rgba32Uint`.
9. **API gaps**: `MediaInfo::{track, dxv_variant, alpha, codec_name, container_name}` + `Default`; `StatedAlpha` (`#[repr(u32)]` 0..3, pinned in `core/tests/abi.rs`); `NativeInfo::alpha` + `Default` (Apple island: `stated_alpha`, Swift's rule); `core::names`; `ImportOptions::track`.
10. Tests: `core/tests/probe.rs` (4), `core/tests/api_gaps.rs` (5), `wgpu/tests/limits.rs` (3), `exact.rs` (BC7 in sweep / fixtures / flip / staging + `bc7_is_decoded_in_the_shader_on_the_exact_path`), apple `native.rs` (+3: stated alpha, the fragmented MP4, the import size check), `core/tests/container.rs` (the cut-off rule replaces "rejected"); fixture `frag_h264.mp4` (+ sidecar) and `scripts/make-fragmented-mp4.swift`; the fuzzer covers the new paths. CLI `probe` takes `dxv_variant` from `MediaInfo` (its own helper deleted). `homepage` removed from the manifests (cargo 1.100's `redundant_homepage`). Docs: CHANGELOG, README, ARCHITECTURE. Version 0.9.0.

## Validated (each with its evidence)
| Claim | Evidence |
|---|---|
| Findings 2-5 fixed, with controls | `core/tests/probe.rs` 4 pass (RED first, above): DXV+HAP → DXV either order, avc1-then-HAP → native, HAP-then-avc1 → HAP; cut file: frame 0 decodes, cut frames fail, uncut decodes all (HAP and DXV); HAP movie as `.png` / `.lmbd` / `.jpg` / no extension → HAP, PNG as `.mov` → still, LMBD as `.mov` → LMBD, garbage `.png` → `Image`, `.jpg` → `Unsupported`; empty-tables avc1 → backend (asked once), no backend → `Corrupt`, refusing backend → `Corrupt`, damaged HAP / DXV → `Corrupt` with the backend never asked |
| Finding 5 on a real fragmented MP4 | apple `a_fragmented_mp4_plays_through_the_platform`: `Movie::open` → `Corrupt`; probe → native 64x48, 60 frames, "H.264"; all 60 frames decode, frame 60 is an error |
| Finding 1 | `wgpu/tests/limits.rs` 3 pass: device limit = adapter (32768) min 16384; on an 8192 device 16x9628 RGBA8 / BC1 / BC3, both paths → `Unsupported` naming 9628 and 8192 with no wgpu validation error (error scope), 16x8192 renders clean; 16x9628 renders exactly on the 16384 device. Apple imports: `a_frame_larger_than_the_device_is_refused_by_both_imports`. Real clip: `SCANTEST/scantest4.mp4` and `scantest5.mp4` (3840x9626, 605 frames each) verify: every frame zero-copy, every seek checked, pass; the run's stderr holds no wgpu / validation line (`docs/data/w2-verify-testfiles-stderr.txt`) |
| Finding 7 | `api_gaps.rs` `the_first_video_fourcc_is_read_leniently`: cut file and damaged stsz (where `Movie::open` fails) → `HapY`; first video track in file order; non-ASCII skipped; no moov / garbage / empty / missing → none |
| Finding 8 | `exact.rs`: sweep 108 rows (Hap7 at 9 sizes, every mode forced, reserved blocks) max 0; fixtures 69 rows / 504 frames max 0 (6 BC7 rows / 16 frames); reserved-block HAP R frame equals the CPU (`[0,0,0,0]` present) with no direct view on Exact, direct view kept on Sampled; flip control 13 cases; staging and decode-into 48 frames; naga validates `BC_EXACT_WGSL` and writes MSL + HLSL |
| Finding 6 | `docs/data/w2-apple-targets.txt` (rustc 1.100.0, rebuilt for the record: 10.12 → the dyld error, 11.0 → loads; iOS unset → `___chkstk_darwin`, 10.0 / 15.0 set → links; both cross checks with no error lines; clippy 1.100 clean). x86_64: `cargo check --workspace --all-targets` and `cargo build --workspace --release` pass; `otool -L` no Swift library, `minos 11.0`; under Rosetta `verify --block-path exact` on `tiles_hapm.mov`, `synthetic_hap7.mov` (max 0) and `tiles_prores4444.mov` (24 frames zero-copy) pass. iOS: `cargo check --workspace --all-targets --target aarch64-apple-ios` passes with the island's decoder compiled; `decode_frame` example links: `Mach-O 64-bit executable arm64`, platform 2, `minos 15.0`. Not run |
| API gaps | `api_gaps.rs` 5 pass (variant from the packet incl. legacy in a DXD3 track and none for a bad header; stated alpha per fixture kind and consistent with `has_alpha`; the names table (31 entries incl. non-names), containers, the line incl. Matroska; chosen track decodes the other track's picture, out-of-range track is an error); `abi.rs` pins `StatedAlpha` 0..3; apple `native_clips_state_their_alpha` (ProRes 4444 straight, the other four none, backend and probe) |
| Every real clip on the exact path | `dxvkit-plus verify --block-path exact`, release, every movie file of Amrit's Testfiles but seven (167 of 174; skipped: `SCANTEST/scantest6`, `B4`-`B6`, `C4`-`C6`, more 3840x9626 H.264 copies — 20 min each — of the size scantest4 / 5 already cover): 129 DXV / HAP files, **24 036 block frames compared, max 0**; 25 native files, 6 390 frames zero-copy. 17 rows fail, none from this release: 13 files nothing plays or damaged on purpose (the `adv_*` files, `bad-audio*`, `YCG6-DXV3_Hap5.mov`, and `adv_trunc_hapm_midframe.mov`, which now opens and fails at frame 0 because its very first sample is past the cut, and `std_indigofera_h264_audio.mp4`'s frames 753-767, W3); 4 native clips with seek mismatches ("asked for frame 10, got 11") — **identical on the v0.8.0 binary** (all four re-run with it: the same problem lists, `docs/data/w2-native-seek-v0.8.0-vs-0.9.0.txt`) — the native frame grid, W3. `docs/data/w2-verify-testfiles-exact-m5max.jsonl` |
| Routing changes on Amrit's folder | probe of all 174 files with the v0.8.0 binary and the final one: one file differs, `adv_trunc_hapm_midframe.mov` (`Corrupt` → HAP Q Alpha, Swift's route). `docs/data/w2-probe-diff-testfiles-v0.8.0-vs-0.9.0.txt` |
| glance-core against 0.9.0 | scratch copy, unmodified test code: the whole workspace has one compile error, the `MediaInfo` literal at glance-media `src/lib.rs:88` (`docs/data/w2-glance-core-compile-on-0.9.0.txt`); with that literal given `..MediaInfo::default()`, `GLANCE_REQUIRE_GPU=1 cargo test --workspace --release`: 5 expected test changes, the rest pass (`docs/data/w2-glance-core-c1-tests-on-0.9.0.txt`) |
| Gate | fmt ok; clippy `-D warnings` exit 0 on rustc 1.99.1 and 1.100.0; `DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace`: 139 passed, 0 failed, 1 ignored (W1: 123); cargo deny bans / licenses / sources ok; fuzz 60 s x 8: 13 562 807 execs, 0 crashes, 0 hangs; no committed fixture changed (sidecar hashes pass; one fixture added) |

## Measured cost — BC7 on the exact path (release, Mac17,6 Apple M5 Max, Metal; Arena running)
`examples/convert_cost.rs` (W1's harness: 64 passes per submission, the GPU waited for, the two paths interleaved), on `metric_1080p_lmb7.lmbd` (16 frames encoded with `dxvkit-plus encode-lmbd` from `Metric_01_HapM.mov`, 1920x1080; not committed), and `Metric_01_HapM.mov` for reference; `docs/data/w2-bc7-convert-cost-m5max.jsonl`. Second round (600 rounds; the first round's medians were 10x their fastest — Arena contention — and are kept only in the data file):

| 1080p | layers | sampled µs / pass (median / fastest) | exact µs / pass (median / fastest) | exact ms / step (median) |
|---|---|---|---|---|
| LMB7 (BC7) | 1 / 4 / 16 | 36.3 / 25.7 · 56.1 / 42.3 · 52.9 / 42.4 | 225.0 / 158.7 · 218.3 / 205.5 · 213.6 / 208.2 | 0.23 · 0.87 · 3.42 |
| HapM (W1 reference) | 1 / 4 / 16 | 44.4 / 40.3 · 40.6 / 33.5 · 47.2 / 39.7 | 121.0 / 109.1 · 102.0 / 88.0 · 110.6 / 88.6 | 0.12 · 0.41 · 1.77 |

BC7 on the exact path is about 4-6x its sampled pass and about 2x HapM's exact pass (W1 measured HapM exact at 92 µs). Each fragment re-reads its block's header and endpoints; a compute pre-pass per block could cut that (IDEAS).

## Deviations and findings
- **The real-clip run is 167 of 174 files**: seven more 3840x9626 H.264 SCANTEST copies were not run (about 20 min each; scantest4 and scantest5, the same size, pass). The CLI binary was rebuilt once during the run (the change: our parser's error kept when the backend also refuses); it touches only the error text of files nothing plays, and the three files it affects were re-probed with the final binary.
- **Cut-off rule for DXV departs from DXVKit's C demuxer** (and so from what Swift Glance's DXV path accepts), by the brief's decision "the same rule for DXV and HAP". glance-core's `first_dxv_packet` still enforces Swift's sample-0 rule for DXV routing.
- **Native alpha keeps the container's rule when the backend states none**: a depth-32 alpha-capable entry (e.g. `png `, `rle `) without the format-description extension stays `Straight`; Swift would say none for those. ProRes 4444 / HEVC alpha follow Swift exactly.
- **Finding 8's RED is the route, not the value** on this GPU (the M5 Max's hardware already matches the definition). The value difference was measured by glance-core on GitHub's runner; it was not reproduced here.
- **BC7 exact costs 4-6x its sampled pass**; no optimisation in this wave (IDEAS: per-block compute pre-pass).
- **Toolchain**: the cross targets need rustup's stable (1.100.0), with `RUSTC` / `CARGO` pointed at it; Homebrew's 1.99.1 on PATH has no x86_64 / iOS std. Mixing both in one target dir broke clippy once (incompatible rlibs); 1.100 checks used their own target dir.
- **Removed**: the CLI's own `dxv_variant` helper (replaced by `MediaInfo::dxv_variant`, same printed names); `homepage` in the manifests (equal to `repository`; cargo 1.100's `redundant_homepage`).
- **Verifier corrections** (cc-kit:verifier, 33 confirmed / 9 unproven / 1 wrong): the glance-core note pointed at route.rs for a sentence that is in glance-core's C1 docs — fixed; the iOS link failure was first put down to the 10.0 target itself — re-measured: it fails only with no deployment variable set (any value links) — docs corrected; logs were saved for the native v0.8.0 comparison, the verify stderr, glance-core's compile error and the cross-target builds. Still without a saved log: the `movieFragmentInterval` observation; the 174-file probe diff after the final change was re-checked on the three files that differed, not all 174.
- **Pre-existing, for W3**: the native seek mismatches above and indigofera's 768 / 753 frames.

## For glance-core
Note: `~/claude-handoffs/glance-core_2026-11-14_note-from-DXVKit-Plus-w2.md`.

## Test media used (Amrit's, read-only, never committed)
`~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/` (all 174 movie files, incl. `SCANTEST/scantest4.mp4`); `Metric_01_HapM.mov` also as the source of a 16-frame 1080p LMB7 clip encoded into the session scratchpad for the cost measurement (not committed). Committed data files carry basenames only.

## Answers from Amrit during the wave
None asked.

## RECEIPTS
```
$ git log -1 --oneline (release)
70b5c74 DXVKit Plus 0.9.0: the probe opens files as Swift Glance does; BC7 on the exact path; MediaInfo gaps
$ git ls-remote origin refs/heads/main refs/tags/v0.9.0 'refs/tags/v0.9.0^{}'
70b5c746bc40e583fd7b2a724a7d5a1d56b07e5c	refs/heads/main
b81d792b9521008bd051d74ddf0475f12f00bfc4	refs/tags/v0.9.0
70b5c746bc40e583fd7b2a724a7d5a1d56b07e5c	refs/tags/v0.9.0^{}
$ cargo fmt --all --check && echo ok
ok
$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.17s
exit 0
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --no-fail-fast (test result lines)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.81s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.71s
test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.86s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.61s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.17s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.54s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 7 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.39s
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.78s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
exit 0
$ cargo deny check licenses bans sources
bans ok, licenses ok, sources ok
$ cat docs/data/w2-fuzz-60s.txt
# W2: cargo run --release -p dxvkit-plus-core --example fuzz -- 60 8, DXVKit Plus 0.9.0 tree, Mac17,6, 2026-11-14 (a dxvkit-plus verify run over the test folder was using the machine at the same time)
fuzz: 60 s, 8 threads, corpus 209 (hap frames 35, dxv packets 79, files 62, lmbd 33), execs 13562807 (226021/s), crashes 0, hangs 0, slowest input 59.9 ms
$ ls -la docs/data/w2-* tests/fixtures/frag_h264.mp4*
-rw-r--r--  1 -   1577 15 Nov 00:29 docs/data/w2-apple-targets.txt
-rw-r--r--  1 -   3583 14 Nov 23:22 docs/data/w2-bc7-convert-cost-m5max.jsonl
-rw-r--r--  1 -    356 14 Nov 23:31 docs/data/w2-fuzz-60s.txt
-rw-r--r--  1 -   7213 14 Nov 23:28 docs/data/w2-glance-core-c1-tests-on-0.9.0.txt
-rw-r--r--  1 -    524 15 Nov 00:26 docs/data/w2-glance-core-compile-on-0.9.0.txt
-rw-r--r--  1 -    762 15 Nov 00:25 docs/data/w2-native-seek-v0.8.0-vs-0.9.0.txt
-rw-r--r--  1 -    794 15 Nov 00:15 docs/data/w2-probe-diff-testfiles-v0.8.0-vs-0.9.0.txt
-rw-r--r--  1 -    744 14 Nov 22:49 docs/data/w2-red-api-gaps-v0.8.0.txt
-rw-r--r--  1 -    652 14 Nov 22:48 docs/data/w2-red-bc7-v0.8.0.txt
-rw-r--r--  1 -   1448 14 Nov 22:47 docs/data/w2-red-limits-v0.8.0.txt
-rw-r--r--  1 -   1873 14 Nov 22:47 docs/data/w2-red-probe-v0.8.0.txt
-rw-r--r--  1 -  86545 15 Nov 00:15 docs/data/w2-verify-testfiles-exact-m5max.jsonl
-rw-r--r--  1 -    216 15 Nov 00:24 docs/data/w2-verify-testfiles-stderr.txt
-rw-r--r--  1 -   4618 14 Nov 22:45 tests/fixtures/frag_h264.mp4
-rw-r--r--  1 -    802 14 Nov 22:56 tests/fixtures/frag_h264.mp4.attribution
$ git diff --stat d2ec731 70b5c74 | tail -1
 52 files changed, 2426 insertions(+), 209 deletions(-)
```
