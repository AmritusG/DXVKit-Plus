# DXVKit-Plus CHECKPOINT

Wave: w9-macrate-native (v0.13.0). Brief: the macrate-rs session (MaCrate's Rust rebuild; its scanner's gap G4 and
the grid's thumbnail grab). Machine: Mac17,6 (Apple M5 Max). Base: 4343fe9 (v0.12.0).

## DONE
- dxvkit-plus-apple (macOS): `track_facts` / `TrackFacts`, `PlaneReader` (`PlaneFormat`, `PlaneInfo`, `PlaneFrame`),
  `FrameGrabber` (`GrabAlpha`, `GrabbedImage`, `THUMBNAIL_MAX_SIZE`), `AvError` (codes 1–7). Additions only.
- Oracle: `scripts/macrate-av-ref.swift` (Swift MaCrate 0.16.4's AVFoundationSource / ThumbnailExtractor calls):
  facts, 15 / 15 plane-0 hashes + PTS, 11 / 11 grabs (rotated copies, size caps) equal → goldens in
  `tests/macrate_av.rs` (10 tests).
- Against the briefing app's cross-tested G4 prototype, on its 47 native / lab files (scratch, not in the repo):
  facts and every frame (PTS, size, stride, all bytes) in 32BGRA / 420v / 420f — see docs/handoffs/w9-macrate-native.md.
- Gates: fmt ok; clippy --workspace --all-targets (debug and --release) -D warnings ok;
  DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --release: 243 passed, 0 failed, 3 ignored; cargo deny ok;
  fuzz 60 s × 8: 0 crashes, 0 hangs.
- Docs: CHANGELOG 0.13.0, README (apple row), handoff docs/handoffs/w9-macrate-native.md, version 0.13.0.

## NEXT
- Wave closed with this commit (tag v0.13.0). Consumer: glance-core glance-media (re-exports), then macrate-rs
  macrate-measure (`NativeVideo` over them).

## STATE
- Scratch (oracle binary, prototype comparison): the macrate-rs session's scratchpad (w9/).

## OPEN

## IDEAS
- (carried) BC7 exact-path pre-pass; 7 skipped SCANTEST copies through verify; `as_chunks` conversion; bench gpu_convert_passes_sum 0.000.
- (carried) Windows writers (Media Foundation) for the encode side.
- A Media Foundation twin of `track_facts` / `PlaneReader` / `FrameGrabber` when MaCrate goes to Windows.
