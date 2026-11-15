# DXVKit-Plus CHECKPOINT

Wave: w4-audio. Brief: ~/claude-handoffs/DXVKit-Plus_2026-11-15_w4-audio_brief.md. Machine: Mac17,6 (Apple M5 Max). Base: a826e31 (v0.10.0 + W3 handoff). Target tag: v0.11.0.

## DONE
- Read: brief; family-needs brief FINDINGS 4 / IMPLEMENT 4; glance@a9dc601 AudioClock.swift (497 lines), FrameClock.swift (audioMasterTarget = round(seconds x fps) clamped; 0.25 s never-prime watchdog -> wall clock), DXVPlayer / HAPPlayer audio glue (first audio track; .none / .failed(reason), video plays); glance-core docs/research/glenc.md §2.12 / §4 audio trak (in32 v1 + wave/enda, lpcm v2 above 65535 Hz); loombda-rs@b7e6f68 container.rs read_audio :617 (description only, no tables, no tests); this repo's container.rs, timing.rs, transport.rs, backend.rs, source.rs MediaInfo / probe.
- transport::Clip callers: none in this repo besides abi.rs enums; glance-core glance-playback/src/lib.rs:46 builds `Clip { frames, rate }` by literal.
- DIAGNOSE 2: scratchpad soundinv.py over Testfiles: 37 movies with audio (one sound track each): sowt v0 (3), in32 v1 + wave/enda=1 (24 incl. one with the 16.16 rate overflowed: field 30464, mdhd 96000), lpcm v2 96 kHz s32le (1), AAC mov v1 + wave/esds (7), AAC mp4 v0 esds (3), fLaC mp4 (1).
- AVF probe scripts/swift-audio-probe.swift (scratchpad audioprobe, avf.jsonl, avfdump/*.f32): edit lists trimmed per PCM frame (start in [mt, mt+dur) kept: 1440256 -> 1440240, 220160 -> 220148); AAC served from packet(edit start) - 1 with TrimDurationAtStart (1024 / 1536 / 1856 / 1216); overflowed-rate in32 served at 30464 Hz, 921344 frames; truncated adv_trunc fails (-11880); fLaC "Cannot Decode".

- IMPLEMENT 1-5 code: container AudioTrack (v0/v1/v2, wave/frma/enda, esds/ASC, roll sgpd, chunk table, tkhd duration), timing.rs AudioTiming (edits per PCM frame, silence round(end x rate) half-up for non-trailing empty edits, tkhd implicit edit, +2112 priming without roll) + FrameTiming end_time / nearest_frame / timescale, audio.rs (PcmFormat, AudioInfo, AudioReader; AAC pre-roll 1 packet; float -0 -> +0), backend AudioDecoder + Backend::audio_decoder (default Unsupported), apple audio.rs AacDecoder (AudioConverter, ES descriptor cookie), MediaInfo.audio, transport Clip { timing } + Clip::new / from_timing / frame_at / nearest_frame, Range on real starts. clippy -D warnings clean; core tests pass.
- Cross-test (scratch, final data pending commit): Testfiles 36 / 37 tracks bit-exact full read (AAC incl.); overflowed-rate in32 deliberate difference (ours 96 kHz); adv_trunc both fail; FLAC both fail. Start-time 3/2: AAC mp4 + test-hap1 equal; 5 AAC mov differ ~1e-5..0.1 because AVFoundation's own started read differs from its own full read (measured). Fixtures: all PCM layouts bit-exact (20-in-24: AVF refuses), edits incl. silence bit-exact, AAC roll / noroll / mp4 variants bit-exact.
- AudioToolbox variance: two windows where AAC hashed differently (<= 5.7e-7): ~10:34 (5 runs) and the variant cross-test run; one AAC decoder component only (adec/aac /appl). Sign-of-zero differs on 2 / 21 AAC sweep files (values equal).
- Fixtures (tests/fixtures, CC0, byte-reproducible here): audio_pcm_formats.mov, audio_pcm_edits.mov, audio_aac_priming.mov, audio_aac_trim.mp4, audio_aac_noroll.mov; scripts/make-audio-fixtures.py + make-aac-source.swift; probe scripts/swift-audio-probe.swift; example apple audio_dump.

- Tests: core audio.rs 9, transport.rs 5, abi AudioCodec pin, fixtures count 51, cli audio rows; apple audio.rs 4. Data: docs/data/w4-* (crosstest testfiles 34/37 bit-exact + 2 both refuse + wrapped rate; start-time 9/9; fixtures; transport 6390 times 0 diff; fuzz 60 s 5.7M execs 0 crashes; aac variance; glance-core 27 pass / same 5 fail).
- Gate: fmt ok; clippy -D warnings ok; DXVKIT_PLUS_REQUIRE_GPU=1 cargo test --workspace: 178 passed 0 failed 1 ignored; cargo deny ok after ruzstd / zstd* lockfile security updates; existing fixtures unchanged.
- Docs: CHANGELOG 0.11.0, README (release note + Audio section), ARCHITECTURE (Audio, transport, ABI rows), ATTRIBUTION (2 code rows + assets), version 0.11.0. Handoff draft docs/handoffs/w4-audio.md.

- Verifier: 49 confirmed / 9 unproven / 4 wrong -> fixed (counts, README/CHANGELOG wording, controls + tie test, evidence files w4-avf-v0-entry-in-mov.jsonl, w4-aac-sweep-decode-crosstest.jsonl, second start-vs-full run).

## NEXT
- (was) Verifier on docs/handoffs/w4-audio.md; fix; commit + tag v0.11.0 + push; receipts; copy handoff to ~/claude-handoffs/DXVKit-Plus_2026-11-15_w4-audio-result.md; glance-core note ~/claude-handoffs/glance-core_2026-11-15_note-from-DXVKit-Plus-w4.md.

## STATE
- Test media (Amrit's, read-only): ~/Desktop/_M5-to-M3_2026-10-08/Movies/Testfiles/.
- Swift refs read-only: ~/Documents/claude/native-builds/rust/glance-core/.ref/{glance,loombda-rs}.
- Scratch: <tmp>/w4
- Handoff name: docs/handoffs/w4-audio.md (docs/handoffs/w4.md is an older, unrelated wave — do not touch).

## OPEN

## IDEAS
- (carried) BC7 exact-path pre-pass; 7 skipped SCANTEST copies through verify; `as_chunks` conversion; bench gpu_convert_passes_sum 0.000.
