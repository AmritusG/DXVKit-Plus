# DXVKit-Plus CHECKPOINT

Wave: w1-exact-path. Brief: ~/claude-handoffs/DXVKit-Plus_2026-11-14_w1-exact-path_brief.md. Machine: Mac17,6 (Apple M5 Max, macOS 27.0.1).

## DONE
- Clone: AmritusG/DXVKit-Plus @ 09f1dbc (v0.7.1) into this folder; baseline `cargo test --workspace` green (every suite `ok`).
- DIAGNOSE 1-3 (read): renderer / `core::bc` / Glance's shader / naga 30.0.1 lowering read; facts with a source each are the DIAGNOSE section of docs/handoffs/w1-exact-path.md.
- DIAGNOSE finding: the HAP Q CPU oracle was `f64` and rounds exact halves down: 574 662 of 200 000 000 random texels differ from the exact rational rounded half up; 2.14% of the HapY / HapM fixture texels, 7.02% of Metric_01_HapM.mov (30 frames), always by 1 (docs/data/w1-hapq-oracle.txt). No shader can equal it, so the oracle became exact integers.
- RED first: tests/exact.rs on unmodified v0.7.1 -> `test result: FAILED. 0 passed; 2 failed`, 93 of 99 sweep rows and 35 of 63 fixture rows differ (docs/data/w1-exact-path-red-v0.7.1.txt).
- core: `bc::bc3_block_exact` returns whole thirds / 35ths (u16), `hap::ycocg_scaled_exact` (integer, half up) replaces `ycocg_scaled_to_rgb_f`; unit test against i128 on 8 604 128 inputs; core lib tests 49 passed.
- wgpu: `BlockPath { Sampled = 0, Exact = 1 }` (default Sampled), `FrameRenderer::with_path / set_path / path`, `BC_EXACT_WGSL` (pipeline built on first exact frame), values pinned in tests/abi.rs.
- GREEN: tests/exact.rs `7 passed; 0 failed` with `--include-ignored`: 99 sweep rows (9 variants + 2 premultiplied readings x 9 sizes) and 63 fixture rows (488 frames) at max diff 0; bit-flip control for every variant; staged + decode_into; path switching; naga MSL + HLSL translation (docs/data/w1-exact-path-green-v0.8.0.txt).
- CLI: `--block-path sampled|exact` for decode / bench / verify; tests/cli.rs `block_path_exact_verifies_with_zero_tolerance` passes.
- Real clips (read-only), final release binary, `verify --block-path exact`, straight and premultiplied: 14 rows pass, 2 544 frame comparisons (1 572 distinct: only the DXT5 clip differs between alpha modes), max 0 (docs/data/w1-exact-path-verify-real-m5max.jsonl).
- Sampled path unchanged: GPU PNG of frame 17 byte-identical between the v0.7.1 binary and 0.8.0 on Metric_01_HapM, DXT5-DXV3, glenc_hap1_1920x1080_chunks4.
- Finding: v0.7.1's own `verify` fails on Metric_01_HapM / HapY (sampled HAP Q max 3, allowed 2). Sampled HAP Q tolerance in `verify` is now 3; documented as measured.
- Measured (release, Arena running on the same GPU): conversion pass per 1080p layer (wall time, GPU waited for) 30-48 us sampled, 50-73 us exact (HAP Q 84-93); 16 layers 0.5-0.8 ms/step sampled, 0.8-1.5 exact (docs/data/w1-exact-path-convert-cost-m5max.jsonl). Pipelined throughput exact / sampled median 1.04 (0.70-1.25, 18 cases, 5 rounds); 16 layers with direct view 1792 / 1006 / 1279 / 671 steps/s sampled (Hap1 / Hap5 / DXT1 / DXT5, `pipelined2_direct`) vs 925 / 706 / 884 / 611 for the same variant on the exact path (docs/data/w1-exact-path-layers-m5max.jsonl).
- Docs: README (release note, codec table, "Exact path (0.8.0)", CLI), docs/ARCHITECTURE.md ("Block paths (0.8.0)", accuracy contracts, C ABI table), CHANGELOG 0.8.0; workspace version 0.8.0.
- Verifier (cc-kit:verifier) run on this file and the handoff draft: 28 CONFIRMED, 5 UNPROVEN, 7 WRONG; every WRONG fixed (counts and wording in docs: 8 604 128 inputs, 35 files, 59 lint sites, "one or more channels", "every tested size", "wall time" for the pass cost, sampled bounds stated as measured), every UNPROVEN restated or re-measured.
- Gate on the final tree: `cargo fmt --all --check` ok; `cargo clippy --workspace --all-targets -- -D warnings` exit 0 (after allowing clippy 1.99's new `chunks_exact_to_as_chunks`, which fails v0.7.1 too: 59 pre-existing sites; this wave adds none); `DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace` exit 0, 123 tests passed, 0 failed; `cargo deny check licenses bans sources` -> `bans ok, licenses ok, sources ok`; fuzz 60 s x 8 threads: 12 723 491 execs, 0 crashes, 0 hangs (docs/data/w1-fuzz-60s.txt; an earlier run with the benchmarks' load: 5 254 928, 0 crashes). No file under tests/fixtures changed (sidecar hash test passes).
- Released: commit 9bc1977 on main, annotated tag v0.8.0, both pushed (`git ls-remote`: refs/heads/main and refs/tags/v0.8.0^{} = 9bc1977).
- Handoff: docs/handoffs/w1-exact-path.md, copied to ~/claude-handoffs/DXVKit-Plus_2026-11-14_w1-exact-path-result.md; note for glance-core: ~/claude-handoffs/glance-core_2026-11-14_note-from-DXVKit-Plus-w1-exact-path.md.
- WAVE CLOSED 2026-11-14.

## NEXT
- Wave closed: nothing queued in this repo. The next waves (item 3 container timing, item 4 audio, item 6 API gaps, item 2 Windows backend, item 5 encoders) each start from their own brief from Cowork.

## STATE
- Test media (Amrit's, read-only, never committed): ~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/ (Metric_01_HapM.mov, Metric_01_HapY.mov, DXT5-DXV3.mov, DXT1-DXV.mov, glenc_hap1_1920x1080_chunks4.mov, glenc_hap5_1920x1080_chunks4.mov, glenc_hapm_LEAKTEST_4k.mov). Data files carry basenames only.
- No glenc-cli / dxvtool on this Mac: the size sweep uses movies the test writes itself (stored HAP sections, stored DXV3 / DXDI packets, random blocks); nothing new is committed under tests/fixtures.
- Brief's "backends.md §12.2 item 18" is the HAP parser on Vidvox packs (roadmap: C1), not a shader fact; §12.2 has no integer-texture item. Recorded as a mis-reference, not a blocker.
- wgpu / naga in Cargo.lock: 30.0.1. naga is a dev-dependency of dxvkit-plus-wgpu (wgsl-in, msl-out, hlsl-out) for the translation test.
- The exact path has run on Metal only; HLSL is translated by naga, not compiled or run (Windows wave).
- Resolume Arena was running during every measurement (not touched): absolute steps/s are depressed and spread widely; A/B was interleaved.

## OPEN

## IDEAS
- Run the exactness sweep over the Vidvox HAP sample packs once they are on this Mac (the brief's §12.2 item 18 belongs to glance-core C1).
- Convert the 59 `chunks_exact(N)` sites to `as_chunks` and drop the lint allowance (a change of its own; decoders with pinned output).
- `dxvkit-plus bench --layers 1` reports `gpu_convert_passes_sum` 0.000 on this Mac (one pass: begin and end timestamps equal); `examples/convert_cost.rs` measures the pass instead. The CLI's timestamp bracket could batch passes the same way.
- A scaled (bilinear) exact mode is not needed here: the renderer always converts at 1:1 and the consumer filters the RGBA8 result.
