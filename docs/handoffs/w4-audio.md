# W4 — audio tracks (PCM + AAC on macOS) and the transport on real frame starts (v0.11.0)

Brief: `~/claude-handoffs/DXVKit-Plus_2026-11-15_w4-audio_brief.md`. Machine: Mac17,6 (Apple M5 Max,
macOS 27.0.1). Base a826e31 (v0.10.0 + W3 handoff). Tag v0.11.0.

## What shipped

- **Sound tracks in the container.** `Movie::audio` holds every sound track. Each one has its
  description: sound sample description v0 / v1 / v2, `wave` / `frma` / `enda`, the `esds` ES
  descriptor and AudioSpecificConfig, and the `roll` sample group. It also has its tables: PCM at
  chunk level, AAC per packet, and the `tkhd` duration. A sound track that does not parse goes to
  `Movie::damaged_audio` and never fails the movie. `MediaInfo::audio` lists every track (codec,
  display name, channels, rate, bits, frames, and an error for a damaged one).
- **`timing::AudioTiming`.** Every output frame's real start through the edit list, as exact
  `MediaTime`s, with `position_of(n)` as the one function. It also gives `frame_at(t)` for random
  access by time, and segments, silent ones included.
- **`audio::AudioReader`.** Blocks of interleaved `f32` at the track's own rate. Each block carries
  the exact position of its first frame and never spans an edit boundary. `seek(MediaTime)` and
  `seek_frame(n)` do random access. PCM covers `sowt` / `twos` (16- or 8-bit), `in24` / `in32` /
  `fl32` / `fl64` (`enda`), and `lpcm` v2 with every flag combination: float or integer, either byte
  order, signed or unsigned, packed or aligned high / low. Non-interleaved `lpcm` is refused by name.
- **AAC on macOS through the Backend trait.** A new `backend::AudioDecoder` trait, and
  `Backend::audio_decoder` (new, defaulting to `Unsupported` with the codec's name).
  `dxvkit-plus-apple::audio::AacDecoder` decodes with AudioToolbox `AudioConverter`, using the ES
  descriptor as the magic cookie. The core keeps the demux, the edit list, priming and the pre-roll,
  so the W5 Windows decoder will trim the same frames.
- **Undecodable audio.** FLAC, MP3, AAC without a backend and non-interleaved `lpcm` each come back
  as an `Unsupported` error naming the codec. Damaged tables come back as `Corrupt`. Neither ever
  panics, and the video still opens and decodes (tested).
- **`FrameTiming`.** New `end_time()`, `nearest_frame(t)` and `timescale()`. `nearest_frame` is the
  audio clock's pick: Swift's `round(seconds × fps)` on the grid, the nearest real start on a table,
  and a tie goes to the later frame. timing.rs documents that a sample starting before an edit's
  media time but overlapping it is dropped on every path (Cowork's answer to W3 Q2).
- **`transport::Clip`.** It carries the timing (`timing: Option<Arc<FrameTiming>>`) and gains
  `Clip::new`, `Clip::from_timing`, `Clip::frame_at` and `Clip::nearest_frame`. With a table, time
  maps to frame through `FrameTiming::frame_at`. Each frame's start is rounded up to a tick, so the
  frame shown at a tick is exactly the timing's frame for that tick's time. The range ends at the
  next frame's start, or at `end_time` for the last frame. Seek, step and cues land on real starts,
  and bounce plays the same times backward. A grid clip is unchanged (tested against `Clip::new`
  over 2000 ticks × 3 settings). Cowork's answer to W3 Q1.
- **Fixtures, data and tools.** Five CC0 fixtures and three AVFoundation reference files, made by
  committed scripts. Cross-test data is in `docs/data/w4-*`. Two examples: `audio_dump` (apple) and
  `transport_timing` (core).

## DIAGNOSE

### 1. Swift Glance (glance @ a9dc601, read-only)
- **Which track.** The first audio track: `asset.loadTracks(withMediaType: .audio)` → `tracks?.first`
  (`Sources/GlancePlayback/AudioClock.swift`, `firstAudioTrack`).
- **How it reads.** `AVAssetReaderTrackOutput` with linear PCM Float32, non-interleaved, at the
  track's own rate and channel count ("no AVSampleRateKey / AVNumberOfChannelsKey -> no resample").
  The whole track is decoded into one `AVAudioPCMBuffer`. Its position is
  `(baseFileFrame + nodeSamples) / sampleRate`, wrapped to the buffer length when looping
  (`wrappedPositionSeconds`). Timestamps are not used: it plays the frames AVFoundation serves back
  to back.
- **An undecodable track.** It throws `AudioClockError` (`noAudioTrack` / `invalidFormat` /
  `unreadable`). `DXVPlayer` and `HAPPlayer` then set `audioState = .none(reason:)` or
  `.failed(reason:)` and the video plays on the wall clock ("NEVER silent-drop (§1): a missing or
  failed audio track does NOT fail playback", `DXVPlayer.swift` ~193-231). A runtime failure goes
  through `fallBackToWallClock(reason:)`, which re-anchors to the current frame.
- **The clock rule** (`FrameClock.swift`):
  - The frame is `audioMasterTarget = Int((audioSeconds * frameRate).rounded())`, clamped to
    `[0, totalFrames-1]`. The comment cites "Round-to-nearest (MaCrate Phase 7.c lesson)".
  - If the audio position stays nil for more than `audioPrimeGrace = 0.25` s since play,
    `onAudioUnavailable` fires and the player falls back to the wall clock.
  - On the wall clock, a loop re-anchors at the wrap point: `startFrame = target % totalFrames`,
    with `startWallTime` set back by `intoLoop / frameRate`.
  - On the audio clock, audio loops at its own length (the buffer) and video clamps to its last
    frame while audio is longer.
  - Seeks reposition the audio through `onSeek` to `frame / frameRate`.

### 2. Every audio track in the Testfiles folder (read-only)
`docs/data/w4-testfiles-audio-tracks.jsonl` (our own Python walker) has one sound track in each of
37 movies:

| Codec | Entry | Files | Channels × rate | Bits / order | Edit list | Priming |
|---|---|---|---|---|---|---|
| `sowt` | v0 | 3 (`_master_hapm_*`) | 1 × 44.1 kHz | 16 LE (stsz 2) | 3000 ms @ 0 | — |
| `in32` + `wave{frma,enda=1}` | v1 (GlEnc) | 22: 14 at 1 × 44.1 kHz, 8 at 2 × 48 kHz (7 indigofera DXV + std_indigofera_h264_audio.mov) | | 32 LE (field says 16) | 3000 / 30005 / 30006 / 4992 ms @ 0 | — |
| `in32` v1 with the 16.16 rate wrapped | v1 | 1 (`… [00.02.60-00.13.64].mov`) | 2 × field 30464, mdhd 96000 | 32 LE | 30005 ms @ 0 | — |
| `lpcm` | v2 | 1 (`… [00.03.04-00.17.64].mov`) | 2 × 96 kHz | 32 int LE (flags 0xC) | 30005 ms @ 0 | — |
| AAC `mp4a` + `wave{frma,mp4a,esds}` | v1 | 6 (std_h264 / hevc / prores_audio.mov, test-hap1-audio, bad- / good-audio) | 1 × 44.1 / 2 × 48 kHz, LC | — | media 1024 (or 0) | in the edit (1024), roll −1 |
| AAC `mp4a` + `esds` | v0 (MP4) | 3 (indigofera mp4s) | 2 × 48 kHz, LC | — | media 35328 / 389952 / 123072 | in the edit, roll −1 |
| `fLaC` + `dfLa` | v0 (MP4) | 1 (`bad-audio2.mp4`) | 1 × 44.1 kHz | — | 5000 ms @ 0 | — |

### 3. AAC priming: what AVFoundation serves (`scripts/swift-audio-probe.swift`)
- AVFoundation trims at the PCM frame. It serves the frames whose start lies in
  `[media_time, media_time + duration)` and puts the first one at the edit's movie time:
  - std_h264_audio.mov: media 1024 for 5 s → 220 500 frames;
  - indigofera.mp4: media 35328 → 1 440 256 (the media ends first);
  - DXV High Quality: 1 440 256 → 1 440 240;
  - test-hap1-audio_DXV: an edit of 4.992 s at 44.1 kHz → 220 148, so the frame starting inside
    the edit's last 0.2 frame is kept.
- In passthrough it starts at the packet before the one holding the edit start, with
  `TrimDurationAtStart` set: 1024 / 1536 / 1856 / 1216 on the test folder. A sweep of 81 edit
  media times (`docs/data/w4-aac-start-packet-sweep.txt`) gives packet p − 1 for p ≥ 3, and packet
  0 when the edit is in packet 1 or 2.
- **Measured on the fixture variants.** Without a `roll` sample group (`sgpd`), AVFoundation trims
  AudioToolbox's 2112-frame encoder delay (Apple TN2258) after the edit's media time as well:
  20 352 frames instead of 22 050. With a `roll` group (Apple's writers store roll −1) it trims
  exactly the edit. An ISO-style v0 `mp4a` + `esds` entry inside a QuickTime movie is refused
  ("Cannot Open", −12848); the same entry in an MP4 reads (`docs/data/w4-avf-v0-entry-in-mov.jsonl`).
- **Also measured.** An empty edit becomes silence, enough frames to bring the output to
  `round(edit end × rate)`: 3 ms → 132 frames, then a 2 ms gap → 89. A trailing empty edit adds
  nothing. With no edit list a track plays its `tkhd` duration (21 ms of 1024 frames at 48 kHz →
  1008).
- Our rule reproduces every one of these (same counts, same first time, same samples). Our pre-roll
  is always one packet; where AVFoundation starts at packet 0 instead (edit in packet 2) the
  decoded samples are still equal: 19 / 21 sweep files bit-exact, 2 equal in value with the sign of
  zero differing (`docs/data/w4-aac-sweep-decode-crosstest.jsonl`).

### 4. `transport::Clip` before this wave
`Clip { frames, rate }` was `Copy`. Every position went through `rate.tick_of(k)` /
`rate.frame_at(pos)`: `range`, `period`, `index`, `mirror_in_frame`, `seek`, `step`, `restart`
and the random-jump paths in `transport.rs`. No caller in this repo outside transport.rs (abi.rs
pins its enums only). glance-core `crates/glance-playback/src/lib.rs:46` builds
`Clip { frames: routed.info.frames.max(1), rate: routed.info.rate }` by literal.

## Validation

| Claim | Evidence |
|---|---|
| Testfiles audio = AVFoundation (frame count, first time, samples) | `docs/data/w4-crosstest-testfiles.jsonl`: 34 / 37 tracks bit-exact, including all 9 AAC and all 25 PCM; 2 refused by both (cut-off `adv_trunc`; FLAC); 1 deliberate difference (the wrapped rate, below) |
| Seek by time = AVFoundation started at that time | `docs/data/w4-crosstest-start-testfiles.jsonl`: all 9 AAC tracks from 3/2 s, bit-exact, same first frame and count |
| Every PCM layout = AVFoundation | core `every_pcm_layout_equals_avfoundation`: 25 tracks against AVFoundation's SHA-256 (`docs/data/w4-avf-audio-fixtures.jsonl`); control: the other byte order / signedness differs on every track; 20-in-24 high BE read here, refused by AVFoundation (`fmt?`) |
| Edit list, silence, partial-frame and past-media edits | core `edits_trim_repeat_and_fill_empty_edits_with_silence_as_avfoundation_does` (segments, positions and AVFoundation's hash); controls: no silence, edits ignored |
| AAC priming / trim / no-roll = AVFoundation | apple `aac_decodes_as_avfoundation_serves_it` and `without_a_roll_group_…`; `docs/data/w4-crosstest-fixtures.jsonl` bit-exact; controls: one frame of offset, the no-roll rule off (22 050 frames) |
| Random access | core `seeking_by_time_lands_on_the_frame_playing_then` (PCM equals a straight read); apple `a_seek_restarts_the_decoder_…` (control: one packet later) |
| Undecodable / damaged audio never blocks the picture | core `an_undecodable_track_is_an_error_naming_its_codec_…` (FLAC by name, AAC without a backend, damaged stsc → `damaged_audio`, the video sample still reads) |
| Transport time → frame = FrameTiming on jittered clips | `docs/data/w4-transport-testfiles.jsonl`: the 4 W3 clips and indigofera, 6390 times (start, mid, last tick of every frame), 0 differences; the 0.10.0 grid transport differed on 2 / 43 / 31 / 5 / 14 of them. Fixtures: core `time_to_frame_follows_frame_timing_…` (4 timed fixtures, 0 differences; grid control > 0) |
| Grid clips unchanged | core `a_grid_clip_is_unchanged_…`; the existing 13 transport unit tests unchanged and passing; `w4-transport-fixtures.jsonl`: tiles, grid differs 0 |
| nearest_frame | core `nearest_frame_is_swifts_round_…`: 1.5 frames → 2, 1.49 → 1, a jittered table's nearest start; `a_tie_on_a_table_goes_to_the_later_frame`; controls: `frame_at` floors, the grid's round differs on the table, just before a midpoint is the earlier frame |
| glance-core on 0.11.0 | `docs/data/w4-glance-core-tests-on-0.11.0.txt`: a0c2d04 builds with W2's literal fix plus the one `Clip::new` line; 27 passed, the same 5 failures as on 0.10.0 |
| Hostile input | core `hostile_audio_tables_never_panic`; fuzz 60 s × 8 threads with the sound-track target: 5 698 477 execs, 0 crashes, 0 hangs (`docs/data/w4-fuzz-60s.txt`) |

## Deviations and decisions

- **AudioToolbox's AAC output is not repeatable between runs.** In two time windows today,
  AVFoundation's process and ours hashed the same AAC tracks differently. The largest difference
  was 5.7e-7 (`docs/data/w4-aac-variance.txt`; the runs are in `w4-crosstest-*-variant-run.jsonl`).
  Every other run was bit-exact, including six consecutive repeat runs 20 s apart. There is one AAC
  decoder component (`adec/aac /appl`), so the variation is inside the decoder or what it calls; its
  trigger was not found (forcing efficiency cores with `taskpolicy -b` did not reproduce it; console
  counts in `w4-aac-variance.txt`). The repo's AAC test therefore compares frame counts and positions
  exactly and samples within 1e-6, against AVFoundation's reference (every 7th frame). PCM is
  compared bit-exact.
- **AVFoundation disagrees with itself on random access.** For five AAC `.mov` tracks (std_h264 /
  hevc / prores_audio.mov, bad- / good-audio.mov), its read started at 3/2 s differs from its own
  full read at the same frames: max 2.7e-5 for the std_* clips, max 0.10 for bad- / good-audio
  (`docs/data/w4-avf-start-vs-full.txt`: two runs about an hour apart, identical numbers). No reader can match both. Ours matches each
  in its own mode: a straight read equals AVFoundation's full read, and a seek equals AVFoundation's
  started read. The likely cause is decoder state that depends on the whole history, as AAC's
  perceptual-noise-substitution generator does. That is a hypothesis: I did not parse the streams
  to confirm it.
- **The wrapped 16.16 rate.** One GlEnc `in32` v1 file states 30 464 Hz (96 000 wrapped at 65 536)
  over a 96 000 media time scale. We read 96 000 Hz and 2 880 480 frames (the edit's 30.005 s).
  AVFoundation, and so Swift Glance, labels the identical samples 30 464 Hz and serves 921 344 of
  them, so it plays the file slow. The rule fires only on that exact wrap signature.
- **Sign of zero.** AVFoundation turns a float PCM −0.0 into +0.0, and ours now does the same. On 2
  of 21 AAC sweep files (edits in packet 1, `w4-aac-sweep-decode-crosstest.jsonl`), AVFoundation's
  first frames are +0.0 where AudioToolbox through our converter gives −0.0. The values are equal;
  the cause is not found.
- **Silence for empty edits** is served as samples, as AVFoundation does. Each block still carries
  its exact movie-time start. Counting served frames at the rate (Swift's clock) and reading
  `position_of` differ by less than a frame at each boundary that falls between frames (from the
  rule: each segment's frame count is a ceiling or rounding of its exact span; not measured
  separately).
- **`transport::Clip`** is no longer `Copy`, and a struct literal needs `timing: None` (or
  `Clip::new`). That is a one-line source change for glance-core (lib.rs:46). It follows W3's
  precedent of appended pub fields; the C mapping is append-only.
- **Fixtures** are written byte by byte by a committed script (Python, plus a Swift AAC encode step)
  rather than by AVFoundation's writer, so every sample entry is exactly the layout under test. A
  second run on this Mac writes identical bytes, AAC included.

- **Dependency security updates.** `cargo deny` failed on five advisories published since 0.10.0,
  against `ruzstd` 0.9.0 and the `zstd` / `zstd-safe` / `zstd-sys` crates. I applied the
  semver-compatible fixes (lockfile only: ruzstd 0.9.1, zstd 0.14.4, zstd-safe 8.3.0, zstd-sys
  2.1.3). All tests pass, including the LMBD vectors on the `zstd-rust` backend.

- **Verifier** (cc-kit:verifier on this handoff): 49 confirmed, 9 unproven, 4 wrong. Fixed: the two
  DIAGNOSE 2 counts; README / CHANGELOG had said every test-folder track matches (34 / 37 do);
  negative controls added where four tests lacked them, plus a table-tie test for `nearest_frame`.
  Evidence committed for the unproven items (v0 entry in a movie, sweep decode, second
  start-vs-full run, `taskpolicy` counts). The glance-core note and the tag are this release's last
  steps.

## Needs Amrit's eyes (one line each)

1. The wrapped-rate GlEnc file: we play it at 96 kHz, Swift Glance at 30 464 Hz. Keep ours, or match
   Swift's behaviour?
2. FLAC in MP4 is reported as undecodable, as AVFoundation does ("Cannot Decode"). AudioToolbox has
   a FLAC decoder; add it later?

## Commits

(filled at release)

## RECEIPTS

(filled at release)
