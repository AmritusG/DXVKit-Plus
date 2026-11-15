# DXVKit-Plus CHECKPOINT

Wave: w3-container-timing. Brief: ~/claude-handoffs/DXVKit-Plus_2026-11-15_w3-container-timing_brief.md. Machine: Mac17,6 (Apple M5 Max, macOS 27.0.1). Base: 740459b (v0.9.0 + W2 handoff). Target tag: v0.10.0.

## DONE
- Read: brief; glance CLAUDE.md:476 (FrameTiming rule) and glance@a9dc601 Sources/GlanceCore/FrameTiming.swift (210 lines) + AVPlaybackController seek use; loombda-rs@b7e6f68 crates/loombda-media/src/container.rs (read_edits :544, read_ctts :569, sync_indices :587 — parse only, no frame mapping, no tests anywhere in loombda-rs for them); this repo's container.rs, source.rs, transport.rs (grid: rate.tick_of), time.rs, apple decode.rs (grid on minFrameDuration = Swift's "wrong way" 3); W2 native seek data (4 clips: periodic +1 offsets every 20 / 24 frames).

- DIAGNOSE 2 measured (scratchpad/diag5.jsonl): on all 5 clips AVAssetReader serves exactly Swift's frame times (counts 30 / 615 / 434 / 300 / 751). indigofera.mp4: 768 samples, edit media_time 18000/25000 drops 17 -> 751 served; "753" = our NativeReader grid round(30.12 s / minFrameDuration 0.04). Seek offsets: simulating the old minFrameDuration-grid rule reproduces W2's problem lists exactly (1 / 26 / 18 / 15) -> cause = grid on the SHORTEST sample (19/600 or 23/600), Swift's wrong way 3.
- DIAGNOSE 3: loombda-media parses elst/ctts (no mapping through edits, no mvhd scale, rate not applied, 1024 cap silent) and has NO tests for them (grep of loombda-rs) — brief premise "with its tests" does not hold; ported the parsers, wrote our own tests.
- Measured: AVFoundation sample-cursor duration = next presentation time - own; last presented keeps stts delta (171 / 171 movies). Used for the rate inputs.
- IMPLEMENT 1 core: src/timing.rs (FrameTiming::from_tables / grid, rate_of, snap, MediaTime, Edit, RateSource); container reads mvhd / elst / ctts, VideoTrack.timing. Cross-test (scripts/crosstest-frame-timing.py; example frame_times; Swift probe scratchpad/swift/ftprobe): Testfiles 168 equal / 0 diff / 6 not openable by either; Glance frame-timing fixtures 11 / 11; new edit fixtures 3 / 3 (scripts/make-edit-list-fixtures.swift: empty 7 ms edit + lcm scale, multi-edit repeat 17 frames of 14 samples, scaled edit -> grid, H.264 B-frames + trim).

- IMPLEMENT 2: source.rs DXV / HAP decode sample_of_frame(n); MediaInfo frames / rate from timing; FrameSource::timing(), NativeDecoder::timing() (defaulted); apple NativeReader on core timing (grid of Swift rules over AVF values only when core cannot parse: frag MP4); div_round deleted. verify --block-path exact on the 4 W2 clips + indigofera: all pass, 0 offsets, indigofera 751 frames (docs/data/w3-verify-native-clips-exact.jsonl).
- IMPLEMENT 3-4: elst/ctts parsers ported (ATTRIBUTION row); hostile lists: past-atom / > MAX_EDITS not applied, nothing-maps / rate != 1 / negative media time / overflow -> grid, cap MAX_SAMPLES; fuzz hook queries timing. Fuzz 60 s x 8: 12 707 220 execs, 0 crashes, 0 hangs (docs/data/w3-fuzz-60s.txt).
- Fixtures (CC0, ours): edit_dxt1_empty_trim, edit_hap1_empty_trim_repeat, edit_hap1_scaled, edit_h264_bframes, jitter_prores4444_30 (+ sidecars); scripts/make-edit-list-fixtures.swift (output differs only in header dates). Cross-test 5 / 5 equal. v0.9.0 vs now (docs/data/w3-verify-fixtures-v0.9.0-vs-0.10.0.txt): 0.9.0 counts 24 / 14 / 24 wrong, B-frame out of range, jitter offset; 0.10.0 all pass.
- Tests: core tests/timing.rs 18 (Swift's rate cases + controls, tables, edits, hostile, fixtures through HAP / DXV, shuffled seeks); apple native.rs +3 (B-frames/edit, frag grid, jitter with 0.9.0-rule control); cli.rs fixture lists; abi.rs RateSource pinned; fixtures count 43.
- Gate (before docs): clippy -D warnings clean; fmt ok; DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace all pass after the CLI test update; cargo deny ok. glance-core @ a0c2d04 on this tree: same 5 failures / messages as on 0.9.0 (docs/data/w3-glance-core-tests-on-0.10.0.txt).
- Docs: CHANGELOG 0.10.0, README (release note + Frame timing section), ARCHITECTURE (native timing, Frame timing section, ABI row), ATTRIBUTION (2 code rows + fixtures), version 0.10.0. Data: docs/data/w3-{swift-frametiming-*,crosstest-*,avf-sample-durations,native-diagnosis,...}.

- Verifier (cc-kit:verifier): 23 CONFIRMED, 5 UNPROVEN, 2 WRONG (CHANGELOG "two" -> three changed meanings; ABI row claimed repr(C) mirrors -> MediaTime / Edit now #[repr(C)]). Fixed; evidence added for the unproven served-times (docs/data/w3-native-served-times.jsonl) and stts-vs-cursor rate (w3-avf-sample-durations.txt last line); loombda grep wording corrected.
- Full Testfiles verify exact (167 rows, 42 min): 155 pass vs W2 149; fixed the 5 timing clips; broke none; 129 DXV/HAP files 24 036 block frames max 0; native 26 / 26; 12 failing = W2's deliberate damaged files (docs/data/w3-verify-testfiles-exact-m5max.jsonl).
- Final gate: fmt ok; clippy -D warnings exit 0; DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --no-fail-fast: 160 passed, 0 failed, 1 ignored; cargo deny ok; existing fixtures unchanged.

## NEXT
- Commit + tag v0.10.0 + push; fill COMMIT / RECEIPTS in docs/handoffs/w3-container-timing.md; glance-core note; mailbox copy.

## STATE
- Test media (Amrit's, read-only): ~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/.
- Swift refs read-only: ~/Documents/claude/native-builds/rust/glance-core/.ref/glance, loombda-rs.
- Handoff name: docs/handoffs/w3-container-timing.md (docs/handoffs/w3.md is an older, unrelated wave — do not touch).

## OPEN
- [ ] Commit, tag v0.10.0, push; handoff receipts; glance-core note; mailbox copy

## IDEAS
- BC7 on the exact path: a per-block compute pre-pass (decode each block once, not per fragment) to cut its 4-6x pass cost.
- Run the 7 skipped 3840x9626 SCANTEST copies through verify when the GPU is free (20 min each).
- (carried) Convert the 59 `chunks_exact(N)` sites to `as_chunks` and drop the lint allowance.
- (carried) `dxvkit-plus bench --layers 1` reports `gpu_convert_passes_sum` 0.000 on this Mac.
