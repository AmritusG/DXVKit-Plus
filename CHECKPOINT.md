# DXVKit-Plus CHECKPOINT

Wave: w6-encoders (v0.12.0). Brief: ~/claude-handoffs/glenc-rs_2026-11-17_goal-build-to-swap_brief.md (goal point 2; made from glenc-rs E1). Machine: Mac17,6 (Apple M5 Max). Base: 4b0afbe (v0.11.0 + W4 handoff).

## DONE
- Oracle: Swift GlEnc 1.6.2 built from AmritusG/glenc @ ed63fd6 (glenc-rs tools/swift-ref.sh, tools/swift-enc-oracle.sh): its own DXVKitFrameEncoder / HapFrameEncoder / VariantMOVWriter driven from raw frames; its CDXVKit enc_bc.o machine code equals GlEnc's build.
- dxvkit-plus-enc (new): dxv (DXVKit 1.0.0 port via Loombda-rs + the 24 clang FMAs of GlEnc's build) — 364/364 generated + 320/320 real 1080p packets equal to Swift's (docs/data/w6-dxv-vs-swift-glenc.txt); hap (Swift GlEnc's own encoders) — 10,269 + 6 (≥16 MiB) + 10,224 refined-BC4 packets equal (docs/data/w6-hap-vs-swift-glenc-*.txt); mov (VariantMOVWriter) — 105 identical + 18 refused by both of 123, 6/6 real >4 GiB layouts (docs/data/w6-mov-*.txt); alpha.
- dxvkit-plus-apple (macOS): BgraReader (+ buffer attachments), read_pcm_s32, AvWriter, scale_bgra — 1,948/1,948 frames equal to Swift's reader; ProRes / MJPEG files equal but creation times; vImage 22/22.
- Gates: fmt ok; clippy --workspace -D warnings ok; DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --release: 233 passed, 0 failed, 3 ignored; cargo deny ok.
- Docs: CHANGELOG 0.12.0, README (crates, roadmap), ATTRIBUTION (4 rows), LICENSES (texturedspenc-MIT, GlEnc-Snappy-BSD-3-Clause), version 0.12.0.

## NEXT
- Wave closed with this commit (tag v0.12.0). Consumer: glance-core v0.5.0 (glance-encode).

## STATE
- Scratch (oracle, agents' runs): the glenc-rs session's scratchpad (swiftref, hap, mov, apple); see glenc-rs CHECKPOINT.
- Consumer: glance-core v0.5.0 glance-encode (glenc-rs goal).

- Verifier: 32 confirmed / 9 unproven / 4 wrong → fixed (CHANGELOG counts, DXV evidence with the real-frame run, >4 GiB evidence re-run with the current tool, private path out of this file); unproven items reworded as session measurements.

## OPEN

## IDEAS
- (carried) BC7 exact-path pre-pass; 7 skipped SCANTEST copies through verify; `as_chunks` conversion; bench gpu_convert_passes_sum 0.000.
- Windows writers (Media Foundation) for the encode side.
