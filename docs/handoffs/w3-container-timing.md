# W3 — container timing: every frame at its real start, on every path (v0.10.0)

Brief: `~/claude-handoffs/DXVKit-Plus_2026-11-15_w3-container-timing_brief.md`. Machine: Mac17,6
(Apple M5 Max, macOS 27.0.1). Base 740459b (v0.9.0 + W2 handoff). Tag v0.10.0.

## What shipped

`dxvkit_plus_core::timing::FrameTiming` is Swift Glance's FrameTiming rule rebuilt on the container's
own tables (stts + ctts + elst, mdhd / mvhd time scales). It gives the frame count, the exact rate
and the rule that produced it, every frame's real start as an exact rational, the inverse (floor),
and the sample that shows each frame. Every movie track carries one (`VideoTrack::timing`). The DXV
and HAP paths decode `sample_of_frame(n)`. The probe's `MediaInfo::frames` / `rate` come from it.
The Apple island's `NativeReader` takes its frames, count and exact-seek rule from core's timing of
the same file, so "frame n" is the same picture on every path and the same one Swift Glance names
n. `FrameSource::timing()` and `NativeDecoder::timing()` (both new, with defaults) hand the timing
to hosts. The loombda-media `elst` / `ctts` parsers are ported, hardened against hostile lists and
fuzzed. Five CC0 fixtures were added, made with AVFoundation by a committed script.

## DIAGNOSE

### 1. Swift Glance's FrameTiming (glance @ a9dc601, read-only)
- Rule: glance `CLAUDE.md:476`, "Frame timing of an AVFoundation track has ONE rule". Seek to
  `time(ofFrame: n)` (the frame's real start: sample table through the edit list) with zero
  tolerance; read the frame with `frame(at:)` (floor); take the rate from FrameTiming (an exact
  rational). The four wrong ways it lists:
  1. seconds rounded to 1/600 (lands on the previous frame at 23.976 / 29.97);
  2. the N / fps grid alone (misses frames that start a tick late on jittered timing);
  3. `minFrameDuration` alone (that is the SHORTEST sample: 25 fps reads as 26.09, 30 as 31.58);
  4. sample count / the track's duration on short clips (movie-timescale rounding: 60 reads as 59.94).
- Frame starts (`Sources/GlanceCore/FrameTiming.swift:166-209`): the sample cursor walks in decode
  order and records `presentationTimeStamp` and `currentSampleDuration`. `movieTimes` keeps the
  non-empty segments. If none is left, the result is the media times sorted, with no shift. If any
  segment's `source.duration != target.duration` (the edit changes speed), the result is nil and
  FrameTiming uses the grid. Otherwise every media time `t` that falls inside a segment's
  `source` range (start ≤ t < end) becomes `t − source.start + target.start`, once per segment that
  contains it. The list is sorted. If nothing maps, the result is nil (grid).
- What a frame index means: the n-th of those sorted starts (`time(ofFrame:)`, clamped). Without a
  table (nil), the grid `n × den / num` with `frameCount = round(trackSeconds × rate)` (`:188`).
- Rate (`:132-159`), first rule that applies:
  1. mean = count / summed duration (else / track duration), snapped to a standard rate within 0.1 %;
  2. the dominant duration, snapped;
  3. `minFrameDuration` when it agrees with `nominalFrameRate` within 0.2 %;
  4. the nominal rate, snapped, else in thousandths;
  5. `minFrameDuration` alone; else 30.
- Seek (`Sources/Glance/AVPlaybackController.swift:208-230`): `seek(to: time(ofFrame: n),
  toleranceBefore: .zero, toleranceAfter: .zero)`.

### 2. The four seek-offset clips and indigofera — measured (`docs/data/w3-native-diagnosis.txt`)
A probe built from Swift's own FrameTiming source (`scripts/swift-frametiming-probe.swift`) dumped
four things per clip: the sample cursor, `segments`, FrameTiming's output, and every presentation
time AVAssetReader serves when it decodes the whole clip.
- On all five clips, **AVAssetReader serves exactly Swift's frame starts**: the counts are 30 / 615
  / 434 / 300 / 751, and every served time equals the corresponding start (served lists:
  `docs/data/w3-native-served-times.jsonl`).
- `std_indigofera_h264_audio.mp4`: 768 samples. The edit has media time 18000 / 25000 (0.72 s), so
  the 17 samples presented before it are dropped. **751** frames are served. The brief's "753" is
  DXVKit Plus 0.9.0's own grid, round(30.12 s / minFrameDuration 0.04 s), not what the decoder
  serves. W2 recorded this file as an error row ("frame 753 is out of range").
- The four offset clips (`glenc-hapm-gate_Hap5_HEVC.mp4`, the two indigofera `_H.264` mp4s,
  `ShroomsProRes 4444.mov`) have jittered sample durations at 1/600 (20 / 19 / 21 ticks, or
  24 / 23 / 25). The 0.9.0 island numbered frames on the grid of `minFrameDuration` (19 / 600 or
  23 / 600), which is Swift's wrong way 3. Re-running 0.9.0's read loop on the served times
  reproduces W2's recorded problem lists **exactly** (1 / 26 / 18 / 15 "asked for frame n, got
  n+1"). That proves the cause rather than inferring it. The edit lists of these clips are a
  single rate-1 edit (media time 0 or 49 / 600). The ctts offsets are the B-frame delay, which the
  edit maps back to 0.
- The 0.9.0 NativeReader's own counts were round(duration / min): 32 / 642 / 453 / 316 (W2's
  verify showed 30 / 615 / 434 / 300 because MediaInfo used the sample count). The served counts are
  30 / 615 / 434 / 300.

### 3. loombda-media's edit-list code vs Swift's rule (loombda-rs @ b7e6f68, read-only)
- What it has: `Edit` (:201), `read_edits` (:544; versions 0 / 1, caps silently at 1024 entries),
  `read_ctts` (:569; offsets read as signed), and `sync_indices` (:587; key frames shifted by the
  first non-empty edit's media time, divided by the first stts delta).
- What it lacks against Swift: no mapping of samples through the edits; no `mvhd` time scale, so
  edit durations cannot be read in seconds; empty edits are parsed but do not shift anything; the
  rate is stored but never checked; only the first edit counts and multiple edits are ignored;
  `media_time` past the track is not detected.
- **No tests exist for any of it.** A grep of loombda-rs for `read_edits` / `read_ctts` / `.edits` /
  `elst` hits only this code and loombda-encode's separate `elst` *writer* (`mov.rs:722`, tested in
  `tests/mux.rs:73-154`); nothing tests loombda-media's reader. The brief's "port it (with its
  tests)" therefore could not be done for the tests. The parsers were ported; the mapping is new
  code following Swift's rule; the tests were written here (core `tests/timing.rs`).

### 4. Swift cross-test (a throwaway program built from glance's source; output committed)
- `scripts/swift-frametiming-probe.swift` + glance's `FrameTiming.swift`. Output:
  `docs/data/w3-swift-frametiming-{testfiles,fixtures,glance-fixtures}.jsonl`. Every movie of the
  test folder: 174 files, of which 168 have tables and 6 AVFoundation cannot open (the same 6 our
  parser refuses).
- Comparer `scripts/crosstest-frame-timing.py` against our `--example frame_times`. It compares the
  rate (as a reduced fraction), the rule, the count and every frame start:
  - **test folder: 168 equal, 0 different, 6 opened by neither side**
    (`docs/data/w3-crosstest-testfiles.jsonl`);
  - Glance's 11 frame-timing fixtures: 11 / 11;
  - the 5 new fixtures: 5 / 5.
- A finding made while cross-testing: AVFoundation's `currentSampleDuration` is **not** the stts
  delta. It is the next presentation time minus the sample's own; the last sample presented keeps
  its stts delta. This held on 173 / 173 movies (`docs/data/w3-avf-sample-durations.txt`); it
  differs from stts on B-frame streams. With plain stts deltas, the rate rule differed on one clip
  (meanRate instead of Swift's dominantDelta). With the cursor's rule, everything is equal (that clip is `edit_h264_bframes.mov`; both computations are in
  `docs/data/w3-avf-sample-durations.txt`).

## Validation

| Claim | Evidence |
| --- | --- |
| Our per-frame starts and counts equal Swift FrameTiming on every file of the test folder | 168 / 168 (6 opened by neither side), `docs/data/w3-crosstest-testfiles.jsonl` |
| The 4 clips and indigofera pass `verify --block-path exact` with no offset and nothing out of range | all pass, 30 / 615 / 434 / 300 / 751 frames, `docs/data/w3-verify-native-clips-exact.jsonl` |
| Shuffled seeks equal sequential decode on every path | core `shuffled_seeks_equal_sequential_decode_on_edited_movies` (HAP, DXV, grid); apple `edit_lists_and_composition_offsets_number_native_frames`, `jittered_native_timing_is_frame_exact`; `verify` on every fixture and real clip |
| Frame n is the source frame the edit list says (DXV, HAP, native) | the tile pattern's frame id: HAP 17 frames `5..14, 2..5, 6..8`; DXV `4..12`; H.264 B-frames `3..20`; jitter `0..23` |
| RED on v0.9.0 | `docs/data/w3-verify-fixtures-v0.9.0-vs-0.10.0.txt`: 0.9.0 counts 24 / 14 / 24 (should be 9 / 17 / 36), the B-frame clip errors out of range, the jitter clip is a frame off; 0.10.0 passes all 5 |
| Hostile edit lists never panic or hang | `hostile_tables_never_panic_or_hang`, `unreadable_edit_lists_are_not_applied`, `speed_edits_and_unmappable_lists_take_the_grid`; fuzz 60 s × 8: 12 707 220 execs, 0 crashes, 0 hangs (`docs/data/w3-fuzz-60s.txt`) |
| No regression on the real clips | `verify --block-path exact` on W2's 167 files (`docs/data/w3-verify-testfiles-exact-m5max.jsonl`; 42 min): 155 pass (W2 149); fixed: the 4 seek-offset clips + indigofera; broke: none; 129 DXV / HAP files, 24 036 block frames, max 0; native 26 / 26; the 12 failing rows are W2's deliberate damaged / unplayable files (the 21 "Old Testfiles" rows, which W2 logged by basename, were run from that subfolder) |
| glance-core unaffected beyond W2 | glance-core @ a0c2d04 on this tree: the same 5 failures with the same messages as on 0.9.0 (`docs/data/w3-glance-core-tests-on-0.10.0.txt`) |

Negative controls are in every new test: the 0.9.0 rule's rate or index computed beside the new
one, edit-less timing beside edited timing, a fitting count beside the capped one, and the
container's sample count beside the frame count.

## Deviations and decisions
- **loombda tests**: none exist (DIAGNOSE 3). Ours are new.
- **"768 vs 753"**: 753 was our own grid; AVFoundation serves 751 (DIAGNOSE 2). The brief's
  direction stands, and the count is now the served one.
- **`source.rs:954-961`** at 740459b is LMBD's `layout()`. The native-count code the brief
  presumably meant is `NativeSource::decode` (`:1131-1137`), now driven by the timing.
- **API**:
  - additions: the `timing` module; `FrameSource::timing` / `NativeDecoder::timing` with defaults;
    `VideoTrack::{edits, ctts, movie_timescale, timing}` + `frames()`; `container::MAX_EDITS`.
  - Adding fields breaks struct literals of `VideoTrack`. No consumer builds one (glance-core does
    not). `MediaInfo` and `transport::Clip` are unchanged.
  - Changed meanings:
    - `VideoTrack::frame_rate()` now returns the timing's rate (was timescale / first delta);
    - `NativeInfo::frame_count` is the number of frames served (was duration / min);
    - `NativeInfo::frame_duration` is one period at the exact rate (was minFrameDuration).
- **Fixture bytes**: the script's output differs from the committed files only in the movie header
  creation dates (stated in each sidecar).
- **Grid fallback for decoding**: on a speed edit (Swift's grid), frame n decodes the n-th sample in
  presentation order, and the last sample once n runs past the end. Documented in `timing.rs`.

## Needs Amrit's eyes (one line each)
1. The transport still steps on the rate's grid (`rate.tick_of(k)`). For jittered clips, time→frame
   should go through `FrameTiming::frame_at`. Should `transport::Clip` carry frame starts in a later
   wave, or does the host convert?
2. Swift's rule drops a sample that starts before an edit's media time even when the sample overlaps
   the edit (our `edit_hap1_empty_trim_repeat.mov` third edit; AVFoundation playback would show it).
   Keep Swift's rule in both, or change both?

## Commits
- 72763ed DXVKit Plus 0.10.0 (code, tests, fixtures, data, docs) — on main, annotated tag v0.10.0, both pushed.
- This handoff: the docs commit after it.

## RECEIPTS
```
$ git log -1 --oneline; git ls-remote origin main refs/tags/v0.10.0^{}
72763ed DXVKit Plus 0.10.0: container timing — every frame at its real start, on every path
72763ed1d9171d9141e369179f1ca982e71587c9	refs/heads/main
72763ed1d9171d9141e369179f1ca982e71587c9	refs/tags/v0.10.0^{}

$ cargo fmt --all -- --check  -> exit 0
$ cargo clippy --workspace --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.91s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace --no-fail-fast  (exit 0; summed: 160 passed, 0 failed, 1 ignored)
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.47s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 19.02s
test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.20s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.92s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.32s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.72s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 7 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 4.09s
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.71s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.34s
$ cargo deny check licenses bans sources
bans ok, licenses ok, sources ok

$ fuzz (docs/data/w3-fuzz-60s.txt)
fuzz: 60 s, 8 threads, corpus 223 (hap frames 41, dxv packets 82, files 67, lmbd 33), execs 12707220 (211773/s), crashes 0, hangs 0, slowest input 161.4 ms

$ cross-test summaries
{"summary": true, "equal": 168, "different": 0, "not_compared": 6}
{"summary": true, "equal": 5, "different": 0, "not_compared": 0}
{"summary": true, "equal": 11, "different": 0, "not_compared": 0}

$ ls -la tests/fixtures/edit_* tests/fixtures/jitter_*
-rw-r--r-- 20041 tests/fixtures/edit_dxt1_empty_trim.mov
-rw-r--r-- 1001 tests/fixtures/edit_dxt1_empty_trim.mov.attribution
-rw-r--r-- 8867 tests/fixtures/edit_h264_bframes.mov
-rw-r--r-- 1031 tests/fixtures/edit_h264_bframes.mov.attribution
-rw-r--r-- 17209 tests/fixtures/edit_hap1_empty_trim_repeat.mov
-rw-r--r-- 1080 tests/fixtures/edit_hap1_empty_trim_repeat.mov.attribution
-rw-r--r-- 29349 tests/fixtures/edit_hap1_scaled.mov
-rw-r--r-- 968 tests/fixtures/edit_hap1_scaled.mov.attribution
-rw-r--r-- 101027 tests/fixtures/jitter_prores4444_30.mov
-rw-r--r-- 999 tests/fixtures/jitter_prores4444_30.mov.attribution
```
