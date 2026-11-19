# W10 — fragmented MP4 and a faster DXV encode / CPU decode (v0.15.0), 2026-11-19

Briefed by glenc-rs (`~/claude-handoffs/glenc-rs_2026-11-19_speed-build-and-fragmented-mp4_brief.md`): ship the
measured byte-identical speed-ups of the 2026-11-18 speed diagnosis, and read fragmented MP4 so a fragmented file
gets its exact sample table in every app. Every encoder output is byte-identical to 0.14.0.

## What shipped
- **Fragmented MP4** (`container.rs`, from ISO/IEC 14496-12 §8.8: `mvex` / `trex`, `moof` / `traf` / `tfhd` / `tfdt` /
  `trun`; `sidx` read in the standard — it indexes the same `moof`s, which the top-level walk reaches without it).
  When the `moov` has `mvex`, every top-level `moof` is read (≤ 64 MB each, `MAX_FRAGMENT_BYTES` = 256 MB in all) and
  its track fragments add their samples after the `moov`'s own: sizes / durations from the `trun`, else the `tfhd`
  defaults, else the `trex` ones; data offsets from the base-data-offset, else the `moof` (`default-base-is-moof` or
  the first `traf`), else the end of the previous `traf`'s data, and a `trun` without an offset after the previous
  run; composition offsets into `ctts`; a `tfdt` later than the samples before it lengthens the previous sample (the
  `moov`'s last one before the first fragment). The media duration is all the samples'. `Movie::fragments` counts the
  `moof`s; `Movie::from_source_all` is `open_all` over any byte source. Because the Apple backend takes core's timing
  whenever core parses a file, the platform path now counts the samples too.
- **DXV CPU decode** (`dxv::rgba::blocks_to_rgba`): no allocation per block row (it made a `Vec` for every row of
  every block); a 4K DXT5 frame 39 → 15 ms on one thread. The GPU-ready path (`decode_blocks`) is unchanged.
- **DXV encoder searches**: `Bc1::try_pair` (59 % of DXT1 encode time in the diagnosis profile) and `bc4_eval` (DXT5
  alpha) compute every pixel's error to every palette entry and keep the first least per pixel; vectorisable, the
  same choices and totals as the C loops. `try_pair` no longer returns the error (no caller used it).
- **`DxvEncoder::set_threads`**: an encoder's threads per frame (1 = the calling thread), as HAP's.
- **0.15.1, the decoder's own buffer** (`dxvkit-plus-apple`): `BgraReader::next_buffer` / `DecodedBuffer` and
  `AvWriter::append_buffer` — Swift GlEnc's pass-through without the two copies (decoder buffer → bytes → a new
  writer buffer). Test `the_decoders_own_buffer_writes_what_its_copy_writes`: the same ProRes file apart from its
  times; another size refused. 0.15.1 gates: 254 passed / 0 failed (GPU required), clippy release + debug, deny.

## How it was proven
| What | Against | Result |
|---|---|---|
| Fragment reader | `tiles_h264.mp4` and four remuxes of it (ffmpeg 9.0.2 as a black-box tool: default-base-is-moof + trex; explicit base-data-offset; samples in the `moov` and in fragments; a `sidx`) | sample bytes, durations, composition offsets, media duration, rate, every frame's start and sample equal (`tests/fragments.rs`) |
| AVFoundation's CMAF writer | `frag_h264.mp4` | 60 frames at 30 fps, samples tile the media data; the platform decodes every frame (`dxvkit-plus-apple` tests) |
| A user's capture (694 fragments, 110,964 samples; AVFoundation: 7397.6 s) | its `mdhd` / edit (3698.8 s) | 110,964 frames, 30 fps, ends at 3698.8 s, 694 fragments (`DXVKIT_PLUS_FRAGMENTED_CLIP` on a `cp -p` copy; never committed) |
| Hostile input | a `trun` count past its box; `mvex` without fragments; `mvex` renamed | the track is damaged (Corrupt) in each; fuzz 60 s × 8 (corpus includes the fragmented fixtures): 0 crashes, 0 hangs |
| `try_pair`, `bc4_eval` | the C loops, kept test-only, on 20,000 / 50,000 random blocks and limits | the same best block, error, endpoints and indices (`try_pair_chooses_what_the_c_loop_chooses`, `bc4_search_keeps_what_the_c_loop_keeps`) |
| Every packet | Swift GlEnc 1.6.2's goldens (`packets_equal_swift_glencs`, `packets_match_swift_glenc`, `golden_files_equal_swift_glenc`) | pass unchanged; DXV packets equal on 1 / 3 / 64 threads (`packets_do_not_depend_on_the_thread_count`) |

## Measured and not kept (one thread, the same frame, interleaved; packets equal in every variant)
- HAP `bc4::encode_simple` in the same form: Hap5 / HapY 15–20 % slower (the original already auto-vectorised: 48 %
  NEON). HAP `match_colors` / projection scan: within noise or slower. `bc4f_eval` (YCG6 / YG10, 64-bit): 2–19 % slower.

## Gates
fmt ok; clippy `--workspace --all-targets` release and debug `-D warnings` ok; `DXVKIT_PLUS_REQUIRE_GPU=1 cargo test
--workspace --release`: 253 passed, 0 failed, 3 ignored; `cargo deny check licenses bans sources` ok; fuzz 60 s × 8:
7,680,211 execs, 0 crashes, 0 hangs.

## Consumers
glance-core v0.8.0 (glance-encode: frames in flight, the fragmented count), then GlEnc 1.8.0, Glance 0.19.2, CueGlance
1.5.2, MaCrate 0.17.2.
