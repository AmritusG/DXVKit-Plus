project:    DXVKit-Plus
pass:       w1-exact-path
STATUS:     COMPLETE
parent:     09f1dbcd4df07774c860b7f42267779b2f7b53ca
result:     9bc1977c92137b01257102f2a85dedae99566e17 (main, pushed; tag v0.8.0); this handoff and the closed CHECKPOINT.md are the commit after it
session:    990967ee-5369-4615-9d6f-a0499885b6bd
written_at: 2026-11-14T19:41Z

# W1 — the exact path: DXT1 / DXT5 / HAP decoded in the shader, bit-exact with the CPU; 0.8.0

## Answer in one line
0.8.0 adds `BlockPath::Exact` to `dxvkit-plus-wgpu`: a renderer on it shows DXV DXT1 / DXT5 / legacy DXT and HAP Hap1 / Hap5 / HapY / HapM / HapA frames equal to `Decoded::to_rgba`, max 0 on every channel, at every tested size from 1x1 to 3840x2160, on Metal. The default (sampled) path is unchanged, byte for byte.

## For Amrit (one line each)
- The CPU decode of HAP Q changed by 1 code value on some texels (2% of the fixtures', 7% of a real 1080p clip's): the old float code rounded exact halves down some of the time; the new integer code always rounds them up. Without this no shader could match the CPU.
- This repo's HAP Q (as in 0.7.1) uses a chroma midpoint of 128 and the untruncated palette. Swift Glance's HAP Q uses a midpoint of 127.5 and truncated palettes, so the Rust family's HAP Q picture will not be bit-identical to Swift Glance's. Say if the Rust apps should follow Swift Glance instead.
- The fast path's HAP Q distance is 3/255 on your real clips, not the 2/255 the docs said; 0.7.1 measures the same 3. The docs and `verify` now say 3.
- The exact path has run on Metal only. The shaders translate to HLSL; D3D12 runs at the Windows wave.
- Resolume Arena was running on this Mac during every measurement (not touched); speeds are comparisons, not absolute figures.

## DIAGNOSE (read first; a source per fact)

### 1. This repo at v0.7.1 (09f1dbc)
| Fact | Source |
|---|---|
| BC1 / BC3 / BC4 frames upload into BC textures and are decoded by the GPU when `CONVERT_WGSL` reads them (`textureLoad` on `texture_2d<f32>`); only DXV High Quality uploads its blocks as integer texels (`Rg32Uint` per BC4 block, `Rgba32Uint` per BC5 block) and decodes them in `DXV_HQ_WGSL` | `crates/dxvkit-plus-wgpu/src/convert.rs` (v0.7.1: `raw_blocks`, `raw_format`, `Converter::pass`) |
| The raw upload is the same bytes and rows as the BC upload, into a texture of `width / 4` x `height / 4`; staged copies use the texture's own size, so they need no change | `convert.rs` `create_raw_texture`, `write_raw`; `staging.rs` `StagingLayout::copy` |
| One `Converter` (two pipelines) is built per `FrameRenderer`, eagerly | `convert.rs` `FrameRenderer::new` |
| CPU oracle, BC1: 5:6:5 widened by bit replication, `(2a + b) / 3` and `(a + b) / 2` truncated, three-colour index 3 = black, alpha 255 always | `crates/dxvkit-plus-core/src/bc.rs` `bc1_block`; DXVKit spec 8.1 as cited there |
| CPU oracle, BC3: colour always four-colour (`bc1_block(.., true)`), alpha `bc4_block`: sevenths / fifths truncated, six-value mode ends 0, 255 | `bc.rs` `bc4_block`; `source.rs` `block_rgba` |
| HAP Hap1 / Hap5 / HapA use that same truncating decoder (there is one integer BC decoder in the library); `Rgba { opaque: true }` forces alpha 255; alpha-only is `[a, a, a, 255]` | `source.rs` `planes_to_rgba`, `hap.rs` `planes` |
| DXV DXT5 read premultiplied: the straight decode, then `bc::unpremul` (`(c * 255 + a / 2) / a`, capped, a = 0 keeps c) | `source.rs` `planes_to_rgba` (`Shading::RgbaPremultiplied`), `bc.rs` `unpremul` |
| HAP Q used the *exact* BC3 palette (thirds, 35ths) as `f64` and a float transform, `round()` at the end | v0.7.1 `bc.rs` `bc3_block_exact`, `hap.rs` `ycocg_scaled_to_rgb_f` |
| Documented GPU-vs-CPU contracts at v0.7.1: HAP ≤ 1/255, HAP Q ≤ 2/255, DXV DXT1 / DXT5 ≤ 2/255, DXV HQ exact | `docs/ARCHITECTURE.md` "Accuracy contracts" (v0.7.1) |
| Plane sizes: HAP rounds the picture up to 4, DXV codes to 16; the CPU decode addresses blocks by the plane's stride and writes only the picture | `hap.rs` `planes`, `dxv/mod.rs` `Layout::new`, `source.rs` `planes_to_rgba` |

### 2. Glance's exact path (read-only clone, glance @ a9dc601)
| Fact | Source |
|---|---|
| Blocks go up as integer textures, one texel per 4x4 block: `GL_RG32UI` for 8-byte blocks, `GL_RGBA32UI` for 16-byte blocks, nearest, read with `texelFetch` | `Sources/GlancePlayback/DXVRenderer.swift:681-702` |
| Block decode in the shader is DXVKit's: `expand565` by bit replication, BC1 thirds / halves truncated, `bc4Pal8` sevenths / fifths truncated (DXT5 alpha), `bc4Pal35` exact in 1/35 units (HQ) | `DXVRenderer.swift:760-803` |
| DXT program: at 1:1 each fragment is exactly its own decoded texel; scaled, it blends the four decoded texels bilinearly | `DXVRenderer.swift:805-841` |
| HQ program: DXVKit's 1/70 fixed point, nearest chroma, fold, round half up | `DXVRenderer.swift:886-913` |
| The bar: rendered = CPU RGBA, max 0 per channel, DXT1 / DXT5 / legacy / YCG6 / YG10 at every size 1x1 ... 3840x2160; every HAP variant equal to Glance's CPU HAP decoder at 1:1; scaled within 1 of a software model of the same filter | `glance/CLAUDE.md:472` |
| Why: "Apple GPUs' fixed-function BC decode is not bit-exact" (BC4 palette with 1/64 weights, up to 1.7/255; BC3 alpha up to 2) | `glance/CLAUDE.md:474` |
| Glance's HAP Q is a *different convention* from this repo's: truncated 8-bit BC3 palettes, then `R' = Y + 8 (R - G) / D`, `G' = Y + 4 (2G - 255) / D`, `B' = Y - 8 (R + G - 255) / D`, `D = B + 8` — a chroma midpoint of 127.5 | `DXVRenderer.swift:926-935` |
| The 1:1 rule and the scaled-filter rule in DXVKit Plus terms: `FrameRenderer` always converts at 1:1 into an RGBA8 texture of the clip's size; scaling is the consumer's sampling of that texture, so a bilinear filter over it *is* the software model's "bilinear over CPU RGBA" | `convert.rs` `ensure_output`; `glance/CLAUDE.md:472` |

### 3. wgpu 30.0.1 / naga 30.0.1 (the versions in Cargo.lock), read from their sources
| Fact | Source |
|---|---|
| `Rg32Uint` / `Rgba32Uint`: 8 / 16 bytes per texel, sample type `Uint`, usable as `TEXTURE_BINDING | COPY_DST` with no feature | `wgpu-types-30.0.1/src/texture/format.rs` (block size table :1246-1248; `guaranteed_format_features` :986-992) |
| Integer division: naga wraps every `/` on integers in `naga_div`: MSL `lhs / metal::select(rhs, 1u, rhs == 0u)`, HLSL `lhs / (rhs == 0u ? 1u : rhs)`; signed also guards `MIN / -1`. So `x / 0 = x` on both; otherwise the target's own division | `naga-30.0.1/src/back/msl/writer.rs:5911-5996`, `src/back/hlsl/writer.rs:3833-3846`; the generated text of `BC_EXACT_WGSL` (test `conversion_shaders_translate_to_msl_and_hlsl`, `DXVKIT_PLUS_DUMP_SHADERS`) |
| Shifts are written as plain `<<` / `>>` in both back ends (no wrapper); a constant count >= the bit width is a validation error | `naga-30.0.1/src/back/mod.rs:337-338`; the negative control of the same test (`lo >> 32u` is refused) |
| `textureLoad` on an integer texture: MSL `t.read(uint2(at), 0)`, HLSL `t.Load(int3(at, 0))`; Metal applies wgpu's image-load bounds-check policy | generated text; `wgpu-hal-30.0.1/src/metal/device.rs:216-219` |
| `vec2<u32>(position.xy)` goes through `naga_f2u32` = `uint(clamp(v, 0.0, 4294967000.0))` on both | generated text |
| What could differ between backends, and how the shader avoids it: signed division rounding (only unsigned operands are divided), division by zero (never happens; defined as `x` in the MSL and HLSL naga writes), shift count >= 32 (never happens), overflow (every arithmetic result < 2^24), float (one final `n / 255.0`, within 2^-22 of n / 255). Only MSL and HLSL text was generated; no SPIR-V, nothing compiled by FXC / DXC | `docs/ARCHITECTURE.md` "Block paths (0.8.0)" |

### 4. Brief vs sources
- **`backends.md` §12.2 "item 18 is this wave's".** In glance-core @ 6063231, §12.2 item 18 is "HAP parser against all Vidvox test packs" and the roadmap assigns it to C1 (`docs/00-ROADMAP.md:159-161`). §12.2 has no item about integer textures or naga. Treated as a mis-reference: nothing in the task depends on it, and the backend rules were extracted from the wgpu / naga sources instead. The Vidvox packs are not on this Mac.
- **"exactly as core::bc does … the reference rounding where HAP uses it".** Per variant: Hap1 / Hap5 / HapA use DXVKit's truncating convention (not a reference rounding); only HAP Q uses the untruncated palette (this library's reading of the paper the HAP specification references; the specification text itself was not re-read in this wave). And HAP Q's oracle was `f64`, which is not reproducible in a shader (next section). Not treated as a stop: the brief asks to say which rounding each variant uses, and the fix keeps the oracle's own stated definition ("rounded and clamped").

## The failing test first (RED on v0.7.1)
`crates/dxvkit-plus-wgpu/tests/exact.rs`, written against 0.7.1's API and run on the unmodified tree: `test result: FAILED. 0 passed; 2 failed`. 93 of 99 sweep rows and 35 of 63 fixture rows differ from the CPU decode (`docs/data/w1-exact-path-red-v0.7.1.txt`, 162 rows).

| Variant (random blocks, sampled path, v0.7.1) | max diff | texels differing at 1920x1080 (2 frames, 4 147 200 texels) |
|---|---|---|
| DXT1 / legacy DXT1 / Hap1 | 2 | 1 285 465 / 1 286 827 / 1 284 075 |
| DXT5 / legacy DXT5 / Hap5 | 3 | 2 433 496 / 2 434 511 / 2 434 613 |
| DXT5 / legacy DXT5 read premultiplied (raw) | 251 / 247 | 2 006 296 / 2 008 664 |
| HapY / HapM | 6 | 1 790 498 / 2 467 303 |
| HapA | 3 | 1 251 297 |
| committed fixtures: HAP Q | 1 | 75 392 of 221 184 per file |
| committed fixtures: DXT1 / DXT5 vectors (straight; premultiplied) | 2; 8 | — |

The sweep is 9 variants (+ the premultiplied reading of the two DXV variants with alpha) at 1x1, 2x2, 3x3, 4x4, 5x7, 15x9, 17x33, 1920x1080, 3840x2160, 2 frames each. No HAP or DXV encoder exists in this repo or on this Mac, so the test writes its own movies: stored HAP sections and stored DXV3 / DXDI packets holding random blocks (every palette mode and index; equal and extreme endpoints forced on some blocks), and decodes them through the real path (file, probe, open, decode, render, read back).

## What shipped
- **`BlockPath`** (`#[repr(u32)]`, `Sampled` = 0 default, `Exact` = 1; pinned in `wgpu/tests/abi.rs`): `FrameRenderer::with_path(device, path)`, `set_path`, `path`. `FrameRenderer::new` is unchanged.
- **`BC_EXACT_WGSL`**: blocks as integer texels (`Rg32Uint` per 8-byte block, `Rgba32Uint` per 16-byte block; the same bytes and rows as the BC upload, so `upload`, `upload_staged` and `upload_from_stage` all work), decoded with the oracle's integer arithmetic. Built on a renderer's first exact frame.
- **Which rounding, per variant:** DXT1 / legacy DXT1 / Hap1: bit-replicated 5:6:5, thirds and halves truncated (DXVKit spec 8.1, `bc::bc1_block`). DXT5 / legacy DXT5 / Hap5: four-colour, thirds truncated; alpha sevenths / fifths truncated (spec 8.2, `bc::bc4_block`). DXT5 premultiplied: then `bc::unpremul`. HapA: BC4 truncated, grey. HapY / HapM: exact palettes (thirds, 35ths), 0.7.1's scaled-YCoCg transform (offset 128 / 255) over one denominator, rounded half up (`bc::bc3_block_exact`, `hap::ycocg_scaled_exact`); HapM's alpha truncated.
- **HAP Q oracle**: `hap::ycocg_scaled_exact` (integers) replaces `ycocg_scaled_to_rgb_f` (f64); `bc::bc3_block_exact` returns `u16` thirds / 35ths. Checked against `i128` rational arithmetic on 8 604 128 inputs.
- **Unchanged on both paths**: DXV High Quality, BC7 (HAP R, LMB7), LMBX, RGBA8 (already exact), HAP HDR (always sampled). `direct_view` is `None` for BC1 / BC3 frames on the exact path.
- **CLI**: `--block-path sampled|exact` for `decode`, `bench`, `verify`. `examples/convert_cost.rs`.
- **Docs**: README (release note, codec table, "Exact path (0.8.0)"), `docs/ARCHITECTURE.md` ("Block paths (0.8.0)": which path is which, the contract of each, the backend rules), CHANGELOG 0.8.0.

## Validated (each with its evidence)
| Claim | Evidence |
|---|---|
| Exact path = CPU, max 0, all 9 variants + 2 premultiplied readings, all 9 sizes | `exact_path_equals_the_cpu_at_every_size`: 99 rows, 0 differ (`docs/data/w1-exact-path-green-v0.8.0.txt`) |
| Exact path = CPU on every committed fixture and vector frame | `exact_path_equals_the_cpu_on_every_fixture_frame`: 63 rows, 488 frames, 0 differ (348 frames are BC1 / BC3 / BC4 on the new shader; 140 are DXV High Quality, exact before too) |
| Negative control: one flipped bit goes RED, every variant; padding outside the picture is not shown | `one_flipped_bit_is_caught` (12 plane checks) |
| Staged upload and decode-into-staging give the same pixels | `exact_path_through_staging_and_decode_into` (44 frames, 2 sizes) |
| Default is sampled; path switches per frame; BC7 / HQ / LMBX same route | `the_path_is_per_renderer_and_the_default_is_sampled` |
| naga validates the shaders and writes MSL and HLSL; a bad shader is refused | `conversion_shaders_translate_to_msl_and_hlsl` |
| Real clips, every frame, both alpha modes: exact = CPU | `dxvkit-plus verify --block-path exact`: 14 rows pass, 2 544 frame comparisons, max 0; 1 572 are distinct (the alpha mode changes only the DXT5 clip's 300) (`docs/data/w1-exact-path-verify-real-m5max.jsonl`) |
| Sampled path output unchanged from 0.7.1 | GPU PNG of frame 17 identical (`cmp`) between the v0.7.1 binary and 0.8.0 on three 1080p clips (one frame per clip; `CONVERT_WGSL` is textually unchanged) |
| Fixture hashes unchanged | no file under `tests/fixtures` modified; `every_fixture_has_a_cc0_sidecar_with_a_matching_hash` passes |
| Gate | fmt ok; clippy `-D warnings` exit 0; 123 tests passed, 0 failed (GPU required); `cargo deny`: bans ok, licenses ok, sources ok; fuzz 60 s x 8 threads: 12 723 491 execs, 0 crashes, 0 hangs (`docs/data/w1-fuzz-60s.txt`) |

## Measured cost (release, Mac17,6 Apple M5 Max, Metal; Resolume Arena running on the same GPU)
The conversion pass, as wall time from submission to the GPU being done — an upper bound on GPU time (`examples/convert_cost.rs`: 64 passes per submission, the two paths alternating, median of 300; `docs/data/w1-exact-path-convert-cost-m5max.jsonl`). Per pass: 30-48 µs sampled, 50-73 µs exact, HAP Q 84-93 µs exact.

| 1080p clip | 1 layer: sampled / exact, ms per step | 4 layers | 16 layers |
|---|---|---|---|
| HapM (Metric_01_HapM) | 0.030 / 0.093 | 0.124 / 0.368 | 0.535 / 1.477 |
| HapY (Metric_01_HapY) | 0.030 / 0.090 | 0.122 / 0.337 | 0.538 / 1.410 |
| Hap1 (glenc_hap1_1920x1080_chunks4) | 0.032 / 0.053 | 0.160 / 0.201 | 0.668 / 0.804 |
| Hap5 (glenc_hap5_1920x1080_chunks4) | 0.032 / 0.057 | 0.145 / 0.206 | 0.722 / 0.900 |
| DXT1 (DXT1-DXV) | 0.036 / 0.053 | 0.192 / 0.253 | 0.676 / 0.919 |
| DXT5 (DXT5-DXV3) | 0.038 / 0.058 | 0.186 / 0.258 | 0.759 / 1.165 |

Steps per second, decode + upload + convert, pipelined, 2 in flight (`dxvkit-plus bench --layers N --layers-only --variants pipelined2,pipelined2_direct`, median of 5 interleaved rounds; `docs/data/w1-exact-path-layers-m5max.jsonl`). The first three number columns are `pipelined2` (a conversion pass per layer); the last is `pipelined2_direct` on the sampled path:

| 1080p clip | layers | 0.7.1 sampled | 0.8.0 sampled | 0.8.0 exact | sampled + direct view |
|---|---|---|---|---|---|
| HapM | 1 / 4 / 16 | 850 / 753 / 499 | 822 / 710 / 506 | 832 / 738 / 522 | 860 / 744 / 505 |
| HapY | 1 / 4 / 16 | 939 / 862 / 572 | 874 / 901 / 560 | 976 / 866 / 618 | 979 / 876 / 612 |
| Hap1 | 1 / 4 / 16 | 2200 / 2190 / 954 | 1547 / 2075 / 982 | 1941 / 2204 / 925 | 2038 / 2181 / 1792 |
| Hap5 | 1 / 4 / 16 | 1299 / 1369 / 667 | 1404 / 1371 / 679 | 986 / 1481 / 714 | 1550 / 1494 / 1006 |
| DXT1 | 1 / 4 / 16 | 1954 / 1646 / 683 | 1910 / 1860 / 806 | 1694 / 1786 / 921 | 1722 / 1827 / 1279 |
| DXT5 | 1 / 4 / 16 | 1024 / 970 / 600 | 1001 / 980 / 559 | 1083 / 956 / 539 | 1098 / 995 / 671 |

Reading: with a conversion pass on both paths, exact / sampled is 1.04 at the median (0.70-1.25 over the 18 cases) — inside the spread; single rounds of one case are up to 8.8x apart (113-998 steps/s) because Arena shares the GPU. The cost that shows is the direct view: at 16 layers, `pipelined2_direct` on the exact path (no direct view, so it converts) gives 925 / 706 / 884 / 611 steps/s for Hap1 / Hap5 / DXT1 / DXT5 against 1792 / 1006 / 1279 / 671 sampled: 1.1x-1.9x.

## Deviations and findings
1. **HAP Q oracle changed** (above). Forced by the 0-difference contract; `Decoded::to_rgba` of HapY / HapM is +1 in one or more channels on 2.14% of the fixtures' texels and 7.02% of Metric_01_HapM's (first 30 frames) (`docs/data/w1-hapq-oracle.txt`; the verifier counted 23% of those Metric_01 texels changing in two or more channels). Public API: `bc3_block_exact` now returns `[[u16; 4]; 16]`; `ycocg_scaled_to_rgb_f` is removed.
2. **Brief reference**: `backends.md` §12.2 item 18 is the HAP parser on Vidvox packs, not a shader fact (DIAGNOSE 4). Not treated as a stop.
3. **The sampled path's distances are measurements, not guarantees.** On encoded footage: 1 (Hap1 / Hap5), 2 (DXT1 / DXT5), 3 (HAP Q); on arbitrary (random) blocks up to 3, HAP Q 6, premultiplied DXT5 raw 251. **Sampled HAP Q is 3/255 on real clips**, on 0.7.1 too (its own `verify` fails Metric_01_HapM / HapY: max 3, allowed 2). `verify`'s sampled tolerance for HAP Q is now 3; docs say so.
4. **Clippy**: this Mac's clippy 1.99 has a new lint (`chunks_exact_to_as_chunks`) that fails 0.7.1 at 59 pre-existing sites. Allowed in the lint tables with a comment rather than rewriting decoders in this wave; the wave's own code adds no site.
5. **GPU time instrument**: `bench`'s timestamp bracket reads 0.000 ms for a single pass on this Mac, so the pass cost comes from the new `convert_cost` example (wall time of batched passes, GPU waited for; not a timestamp query). The first, noisier 3-round run with the `staged` variant is kept as `docs/data/w1-exact-path-layers-staged-m5max.jsonl`.
6. **Sizes**: every listed size is covered by generated movies, not by committed fixtures. No new fixture is committed; the committed ones are unchanged (ours CC0, DXVKit's vectors MIT).
7. **Not done here**: D3D12 / Vulkan runs of the 0-difference test (Windows wave); a HapA or legacy-DXT real clip at 1080p (none on this Mac; both are covered by the sweep).

## Test media used (Amrit's, read-only, never committed)
`~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/`: Metric_01_HapM.mov, Metric_01_HapY.mov, DXT5-DXV3.mov, DXT1-DXV.mov, glenc_hap1_1920x1080_chunks4.mov, glenc_hap5_1920x1080_chunks4.mov, glenc_hapm_LEAKTEST_4k.mov, glenc_hapy_fromMaster.mov (oracle count only). Committed data files carry basenames only.

## Answers from Amrit during the wave
None asked, none given.

## Independent check (cc-kit:verifier, before the commit)
28 CONFIRMED, 5 UNPROVEN, 7 WRONG. It re-ran the RED test on an untouched v0.7.1 copy (same 162 rows), the GREEN run (same 324 rows), the gate, the real-clip verify, and four shader mutants (BC1 third + 1, HAP Q truncation, BC4 six-value 0 -> 1, midpoint 384 -> 383): each mutant fails the tests on the expected variants only, so the tests are not vacuous. It read `BC_EXACT_WGSL` against the core decoders line by line: CONFIRMED.
- WRONG, all fixed before the commit: "8.4 million" inputs (8 604 128); "39 files" (35); "63 lint sites" (59, and the wave's 4 new ones removed); "+1 in one channel" (one or more); the sampled path's bounds stated as a contract (now: measured on encoded footage); "5x apart" (8.8x); a table caption.
- UNPROVEN, restated: "GPU time" (now "wall time, GPU waited for"); "every size" (every tested size); `x / 0 = x` "everywhere" (in the MSL and HLSL naga writes); the Vidvox attribution of the 128 midpoint (this library's reading); the fuzz count (re-run, output saved).

## RECEIPTS
```
$ git log -1 --format="%H %d %s" && git status -sb | head -1
9bc1977c92137b01257102f2a85dedae99566e17  (HEAD -> main, tag: v0.8.0, origin/main, origin/HEAD) DXVKit Plus 0.8.0: the exact path — BC1 / BC3 / BC4 decoded in the shader, bit-exact with the CPU
## main...origin/main
$ git ls-remote origin refs/heads/main refs/tags/v0.8.0 "refs/tags/v0.8.0^{}"
9bc1977c92137b01257102f2a85dedae99566e17	refs/heads/main
b5ce99dde74b9301c61ace42f25631ec235c5845	refs/tags/v0.8.0
9bc1977c92137b01257102f2a85dedae99566e17	refs/tags/v0.8.0^{}
$ git diff --stat v0.7.1 v0.8.0 | tail -1
 30 files changed, 2965 insertions(+), 158 deletions(-)
$ git diff --stat v0.7.1 v0.8.0 -- tests/fixtures | wc -l
       0
$ cargo fmt --all --check; echo exit $?
exit 0
$ cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.65s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --locked   (suites with tests)
tests/native.rs -> ok. 4 passed; 0 failed; 0 ignored; finished in 2.37s
unittests src/main.rs -> ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
tests/cli.rs -> ok. 5 passed; 0 failed; 0 ignored; finished in 10.40s
unittests src/lib.rs -> ok. 49 passed; 0 failed; 0 ignored; finished in 0.06s
tests/abi.rs -> ok. 2 passed; 0 failed; 0 ignored; finished in 0.00s
tests/container.rs -> ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
tests/decode_into.rs -> ok. 2 passed; 0 failed; 0 ignored; finished in 0.03s
tests/dxv.rs -> ok. 7 passed; 0 failed; 0 ignored; finished in 0.04s
tests/fixtures.rs -> ok. 4 passed; 0 failed; 0 ignored; finished in 0.05s
tests/lmbd_vectors.rs -> ok. 3 passed; 0 failed; 0 ignored; finished in 0.03s
tests/lmbx.rs -> ok. 5 passed; 0 failed; 0 ignored; finished in 0.86s
unittests src/lib.rs -> ok. 9 passed; 0 failed; 0 ignored; finished in 0.60s
tests/encoder_identity.rs -> ok. 3 passed; 0 failed; 0 ignored; finished in 4.21s
tests/lmbx_identity.rs -> ok. 1 passed; 0 failed; 0 ignored; finished in 2.54s
unittests src/lib.rs -> ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
tests/abi.rs -> ok. 2 passed; 0 failed; 0 ignored; finished in 0.00s
tests/exact.rs -> ok. 6 passed; 0 failed; 1 ignored; finished in 2.12s
tests/gpu.rs -> ok. 12 passed; 0 failed; 0 ignored; finished in 3.71s
sum: 123 passed, 0 failed, 1 ignored (sampled_path_numbers, a record not a check)
$ cargo deny check licenses bans sources 2>&1 | tail -1
bans ok, licenses ok, sources ok
$ cat docs/data/w1-fuzz-60s.txt | tail -1
fuzz: 60 s, 8 threads, corpus 208 (hap frames 35, dxv packets 79, files 61, lmbd 33), execs 12723491 (212044/s), crashes 0, hangs 0, slowest input 19.8 ms
$ grep -E "^## |^test result" docs/data/w1-exact-path-green-v0.8.0.txt
## exact_path_equals_the_cpu_at_every_size — BlockPath::Exact: 99 rows, 0 differ from the CPU decode
## exact_path_equals_the_cpu_on_every_fixture_frame — BlockPath::Exact: 63 rows, 0 differ from the CPU decode
## sampled_path_numbers — BlockPath::Sampled (for the record; not a pass / fail check): 162 rows, 128 differ from the CPU decode
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 46.76s
$ ls -la docs/data/w1-*
-rw-r--r--  1 -    7718 14 Nov 20:05 docs/data/w1-exact-path-convert-cost-m5max.jsonl
-rw-r--r--  1 -   26028 14 Nov 20:17 docs/data/w1-exact-path-green-v0.8.0.txt
-rw-r--r--  1 -  481062 14 Nov 20:05 docs/data/w1-exact-path-layers-m5max.jsonl
-rw-r--r--  1 -  247209 14 Nov 20:05 docs/data/w1-exact-path-layers-staged-m5max.jsonl
-rw-r--r--  1 -   13264 14 Nov 20:37 docs/data/w1-exact-path-red-v0.7.1.txt
-rw-r--r--  1 -   15436 14 Nov 20:16 docs/data/w1-exact-path-verify-real-m5max.jsonl
-rw-r--r--  1 -     269 14 Nov 20:39 docs/data/w1-fuzz-60s.txt
-rw-r--r--  1 -    2577 14 Nov 20:05 docs/data/w1-hapq-oracle.txt
```
Real clips, final release binary (`docs/data/w1-exact-path-verify-real-m5max.jsonl`, 28 rows):
```
exact rows 14 all pass True frames 2544 max 0
sampled rows 14 all pass True max 3
```
END OF HANDOFF w1-exact-path sha256:c7bc73b6 23490
