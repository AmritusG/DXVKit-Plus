# W9 — MaCrate's AVFoundation pieces (v0.13.0), 2026-11-17

Briefed by macrate-rs (MaCrate's Rust rebuild): its measurement scanner's gap G4 (the AVFoundation track facts and a
CPU frame reader, which a crate that forbids `unsafe` cannot write) and its grid's thumbnail grab. The scanner's
cross-test had proven a scratch prototype of G4 equal to Swift MaCrate 0.16.4 on every native clip it scanned; this
wave puts that reader where `unsafe` is allowed, with the same settings, and adds the thumbnail generator.

## What shipped (`dxvkit-plus-apple`, macOS; additions only)
- `track_facts(path) -> TrackFacts`: Swift `AVFoundationSource.open()`'s facts of the first video track — size,
  nominal rate, duration, end of the leading empty edits, and the first format description's subtype, matrix tag,
  `colr` atom, full range, alpha (contains alpha or depth 32), depth. Tracks load through the asynchronous
  `loadTracksWithMediaType:completionHandler:`, so a failure is the `NSError` Swift sees (description, domain, code).
- `PlaneReader` (`open(path, PlaneFormat)` → `next_frame()` / `next_frame_into(&mut Vec<u8>)` / `finish()`): plane 0
  (BGRA, or Y of `420v` / `420f`) of every decoded sample with its PTS and the buffer's own stride; the output asks
  only `kCVPixelBufferPixelFormatTypeKey`, `alwaysCopiesSampleData = false`. A reader object rather than a callback
  (CONTRIBUTING: C-wrappable); `finish()` gives a caller that stops early the same `.failed` check as the end.
- `FrameGrabber` (`open(path, max_w, max_h)` → `grab(MediaTime, GrabAlpha)` / `grab_seconds(…)`, `track_alpha()`):
  Swift `ThumbnailExtractor.extractAVAsset`'s generator (preferred transform applied, maximum size, zero tolerance,
  `copyCGImage(at:actualTime:)`); the CGImage drawn into a device-RGB `premultipliedLast` RGBA8 context — the context
  Swift's luminance check and checkerboard composite draw into — and handed over straight or premultiplied. The
  walk-forward, luminance test and checkerboard stay the app's.
- `AvError` (codes 1–7), `PlaneFormat`, `GrabAlpha` (`#[repr(u32)]`), pinned in `tests/macrate_av.rs`.

## How it was proven
| What | Against | Result |
|---|---|---|
| Facts (codec, size, rate, duration, matrix, colr, alpha, depth, leading empty edit) | `scripts/macrate-av-ref.swift` (Swift MaCrate's own calls) on 7 fixtures | equal |
| Plane 0, 32BGRA / 420v / 420f | same script: FNV of every frame's visible bytes, PTS list, sizes, 5 fixtures | 15 / 15 equal |
| Grab (sizes, pixels, actual time, alpha info) | same script, incl. two 90°-rotated copies and 64² / 32² caps | 11 / 11 equal |
| Facts + every frame (PTS, size, stride, full bytes incl. padding) in all three formats | the macrate-rs scanner's cross-tested G4 prototype, on 47 of the app owner's native clips (not in this repo) | 47 / 47 equal (below) |
| Errors | Swift's NSErrors: not a movie −11828, missing file −11800, no decoder −11833 at start, generator −11869 | equal domain / code / text prefix |

Result of that comparison (scratch, in the briefing session): facts equal on 47 / 47 files (6 that AVFoundation cannot
open fail with the same NSError on both sides, heap addresses aside); 35 files decoded, 36,201 frames over the three
formats identical in PTS, size, stride and every byte including row padding; 6 DXV files fail identically at
`startReading` (−11833, no decoder).

Committed tests (no media, no private paths): `tests/macrate_av.rs` (10). The rotated clips are made by the test
from the CC0 `tiles` fixtures (the `tkhd` matrix set to a 90° turn) in cargo's temp dir.

## Notes
- Opening a `FrameGrabber` never decodes: an unreadable file fails at the first grab, as Swift's generator does.
- Straight output divides by alpha with rounding (`round(c · 255 / a)`); the generator's own CGImage for ProRes 4444 is
  straight (alpha info `last`), but the picture handed over is the drawn one so the colour is CoreGraphics' match to
  device RGB, as in Swift. Opaque images are identical in both modes.
- `PlaneReader` deliberately differs from `BgraReader` in two places, both Swift MaCrate's behaviour: a sample
  without an image buffer is skipped (GlEnc's reader ends there), and rows keep their padding.

## Gates
fmt ok · clippy --workspace --all-targets (debug and --release) -D warnings ok · `DXVKIT_PLUS_REQUIRE_GPU=1 cargo test
--workspace --release`: 243 passed, 0 failed, 3 ignored · cargo deny ok · fuzz 60 s × 8: 12.4 M execs, 0 crashes, 0 hangs.
