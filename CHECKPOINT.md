# DXVKit-Plus CHECKPOINT

Wave: w2-probe-findings-and-api-gaps. Brief: ~/claude-handoffs/DXVKit-Plus_2026-11-14_w2-probe-findings-and-api-gaps_brief.md (with the c1-findings r2 brief and the glance-family-needs brief, FINDINGS 6). Machine: Mac17,6 (Apple M5 Max, macOS 27.0.1). Base: d2ec731 (v0.8.0). Target tag: v0.9.0.

## DONE
- Read: the three briefs; glance-core (read-only) c1-exact-picture.md DIAGNOSE 1, 04-C1-RESULTS §4 / §6, route.rs, tests/routing.rs, tests/swift_parity.rs, glance-gpu open_gpu / check_size; Swift Glance PlayerModel.swift:209-229, HAPDemuxer.swift (demux, parseMoov, parseTrakIfVideo, buildFrameIndex), DXVKit mov.c read_track / open loop / build_samples, DXVDetector.swift names, AlphaType.swift probeAVFoundation, AVPlaybackController.inspect; this repo's source.rs, container.rs, wgpu lib.rs / convert.rs, bc7.rs, apple decode.rs / lib.rs, backend.rs, exact.rs.
- A real fragmented MP4 made with AVFoundation (AVAssetWriter, CMAF segments): ftyp + moov (stsz count 0, mvex) + 2 x moof/mdat, 4 618 bytes (scratchpad; generator to be committed as scripts/make-fragmented-mp4.swift).

- RED on v0.8.0 (docs/data/w2-red-*-v0.8.0.txt): core tests/probe.rs 4 failed (findings 2-5); wgpu tests/limits.rs 2 failed (finding 1; adapter 32768, device 8192); exact.rs bc7_is_decoded_in_the_shader_on_the_exact_path failed at the direct_view assertion (finding 8 — on the M5 Max the hardware already gives reserved = [0,0,0,0], so the observable RED is the route, not the value); api_gaps.rs did not compile (18 errors: finding 7 + gaps).
- GREEN: container per-track damage (DamagedTrack, Movie::open_all, VideoTrack::index), cut-off samples kept and failed per read, first_video_fourcc(_in); probe: content first, Swift track order, ImportOptions::track, backend fallback; MediaInfo track / dxv_variant / alpha (StatedAlpha) / codec_name / container_name; names module (codec_name, codec_label, container_name, unsupported_message); NativeInfo::alpha + Default; Apple island stated alpha (Swift's rule). Core 86 tests pass; apple tests/native.rs 6 pass incl. the real fragmented MP4 (tests/fixtures/frag_h264.mp4, ours, CC0, sidecar; generator scripts/make-fragmented-mp4.swift).
- Names: Vidvox HapVideoDRAFT.md "Names and Identifiers": Hap7 "Hap R", HapH "Hap HDR" -> "HAP R" / "HAP HDR" in the family's case.

- Finding 1 GREEN: Gpu requests adapter limit min MAX_EDGE; check_device_size in create_bc_texture*, ensure_output, prepare_planes / upload_from_stage, raw textures, render_native; Apple imports refuse too. wgpu tests/limits.rs 3 pass; apple a_frame_larger_than_the_device_is_refused_by_both_imports passes.
- Finding 8 GREEN: BC_EXACT_WGSL mode 4 (BC7, tables generated from core::bc7); exact.rs with Hap7 in the sweep (108 rows), 69 fixture rows / 504 frames (HAP R + 5 LMB7), flip control 13, staging 48 frames, reserved-block test: all max 0. Cost (convert_cost, 1080p LMB7 encoded from Metric_01_HapM, Arena running): exact 213-225 us/pass median vs sampled 36-56; 16 layers 3.4 vs 0.85 ms/step (docs/data/w2-bc7-convert-cost-m5max.jsonl, 2 rounds; round 1 medians contended).
- Finding 6: rustup stable 1.100.0 (RUSTC/CARGO explicit; Homebrew 1.99.1 has no cross std). x86_64: check --all-targets + release build pass; binary failed to load (@rpath/libswiftCoreMedia, minos 10.12) -> .cargo/config.toml MACOSX_DEPLOYMENT_TARGET=11.0; then runs under Rosetta, verify exact on HapM / Hap7 / ProRes 4444 pass max 0. iOS: island cfg widened to ios; check passes; example links once IPHONEOS_DEPLOYMENT_TARGET is set (unset: ___chkstk_darwin, cc builds libzstd for SDK 27.2 vs rustc 10.0; explicit 10.0 also links; repo uses 15.0); not run (docs/data/w2-apple-targets.txt).
- glance-core @ a0c2d04 run against this tree from a scratch copy (glance-core untouched): one compile change (MediaInfo literal lib.rs:88) + 5 expected test changes (docs/data/w2-glance-core-c1-tests-on-0.9.0.txt). Note written: ~/claude-handoffs/glance-core_2026-11-14_note-from-DXVKit-Plus-w2.md (commit hash placeholder until the commit).
- Fuzz 60 s x 8: 13 562 807 execs, 0 crashes, 0 hangs (docs/data/w2-fuzz-60s.txt). cargo deny: bans ok, licenses ok, sources ok. Clippy clean on 1.99.1 and 1.100.0.
- Gate: fmt ok; clippy -D warnings exit 0; DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace: 139 passed, 0 failed, 1 ignored; no existing fixture changed (fixture count 38 with frag_h264.mp4).
- Docs: CHANGELOG 0.9.0, README (release note, codec table, Probing section, BC7 exact), ARCHITECTURE (Probe and routing, BC7 contract, ABI rows, Apple targets); version 0.9.0; CLI probe's dxv_variant from MediaInfo (helper deleted); homepage field removed (cargo 1.100 lint).

- Real clips (release, verify --block-path exact, 167 of 174 movie files; 7 more 3840x9626 SCANTEST copies skipped, 20 min each): 129 DXV / HAP files, 24 036 block frames, max 0; scantest4 / scantest5 (3840x9626, 605 frames) pass on the 16384 device; 17 failing rows all pre-existing or deliberate (13 unplayable / damaged; 4 native seek mismatches identical on v0.8.0 = W3) (docs/data/w2-verify-testfiles-exact-m5max.jsonl). Probe v0.8.0 vs 0.9.0 over all 174: only adv_trunc_hapm_midframe.mov differs (now HAP) (docs/data/w2-probe-diff-testfiles-v0.8.0-vs-0.9.0.txt). Probe now keeps our parser's error when the backend refuses too (test in probe.rs).
- Handoff draft: docs/handoffs/w2-probe-findings-and-api-gaps.md.

- Verifier (cc-kit:verifier) on CHECKPOINT, handoff and glance-core note: 33 CONFIRMED, 9 UNPROVEN, 1 WRONG. WRONG fixed (note's location of the ".png / .lmbd" sentence). Re-measured: iOS link fails only with no deployment variable set (any value links) - docs corrected. Logs saved: docs/data/w2-native-seek-v0.8.0-vs-0.9.0.txt, w2-verify-testfiles-stderr.txt, w2-glance-core-compile-on-0.9.0.txt, w2-apple-targets.txt. Remaining unproven stated in the handoff.
- Final gate after the last code change: fmt ok, clippy exit 0, 139 passed / 0 failed / 1 ignored (GPU required), deny ok.
- Released: commit 70b5c74 on main, annotated tag v0.9.0, both pushed (ls-remote: main and v0.9.0^{} = 70b5c74).
- Handoff: docs/handoffs/w2-probe-findings-and-api-gaps.md -> ~/claude-handoffs/DXVKit-Plus_2026-11-14_w2-probe-findings-and-api-gaps-result.md; note for glance-core: ~/claude-handoffs/glance-core_2026-11-14_note-from-DXVKit-Plus-w2.md.
- WAVE CLOSED 2026-11-15.

## NEXT
- Wave closed: nothing queued here. Following waves per the brief: W3 container timing (incl. the native frame count 768/753 and the native seek mismatches seen on 4 real clips, identical on v0.8.0), W4 audio, W5 Windows backend, W6 encoders - each from its own brief.

## STATE
- Test media (Amrit's, read-only): ~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/.
- Toolchain rustc 1.99.1; targets installed: aarch64-apple-darwin, aarch64-apple-ios, x86_64-apple-darwin, x86_64-pc-windows-msvc; iPhoneOS27.2.sdk present.

## OPEN

## IDEAS
- BC7 on the exact path: a per-block compute pre-pass (decode each block once, not per fragment) to cut its 4-6x pass cost.
- Run the 7 skipped 3840x9626 SCANTEST copies through verify when the GPU is free (20 min each).
- (carried) Convert the 59 `chunks_exact(N)` sites to `as_chunks` and drop the lint allowance.
- (carried) `dxvkit-plus bench --layers 1` reports `gpu_convert_passes_sum` 0.000 on this Mac.
