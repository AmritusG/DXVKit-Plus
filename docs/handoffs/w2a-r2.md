project:    DXVKit-Plus
pass:       dxvkit-plus-w2a
STATUS:     COMPLETE
parent:     eef092ffa46a20fea530944e0b262438492d1229
result:     325ad256b19cc8e2d23773aad08a1ec90b7fcda1 (tag v0.2.0); 62dda7f
session:    bd078af2-9605-4230-a801-c309ee5e531c
written_at: 2026-10-11T13:20:55Z

# DXVKit Plus W2a — result: LMBD (LMB7) ported, byte-identical to the original; v0.2.0 tagged

## Amrit's words (verbatim)
- On the repo description naming DXV and LMBD before they ship: "Trim"
- On the LMBD source: "Codec is on M3 and M5, you can get CC do fetch them from the repo, right?"
- In this session Amrit's answer to the BLOCKED handoff was to push the repo: he pasted the gh output "✓ Created repository AmritusG/loomda-codec on github.com ... ✓ Pushed commits to https://github.com/AmritusG/loomda-codec.git". No other words.

## What shipped (https://github.com/AmritusG/DXVKit-Plus, tag v0.2.0)
- STEP 1: description trimmed, then (this wave ships LMBD) set to "Rust video for VJs: HAP, native OS codecs and the LMBD GPU codec, frame-exact, zero-copy into wgpu." Topics unchanged. README tagline matches. No DXV claim.
- core: lmbd module (validated LMBD v1 reader and writer, chunks decompress in parallel with pure-Rust ruzstd; reserved values refused with Unsupported, unknown with Lmbd); SourceKind::Lmbd = 5; MediaError::Lmbd = 11 (pinned in abi.rs, ARCHITECTURE C mapping updated).
- New crate dxvkit-plus-lmbd-enc: a faithful Rust port of bc7enc_rdo's 4-mode BC7 encoder, ERT (RDO) pass and the BC7 path of rdo_bc_encoder (MIT/Unlicense, LICENSES/bc7enc_rdo.txt) + libzstd 1.5.7 level 9. Not cc-wrapped: the port is complete and byte-identical. Not ported (LMBD never uses them): bc7e.ispc all-mode (Apache-2.0, x86/Rosetta only), perceptual, reduce-entropy, relative-movement.
- wgpu: LmbdTexture (BC7 texture + sRGB view), FrameRenderer::render_view, create_bc_texture_with_views.
- CLI: encode-lmbd, compare (PSNR/SSIM, the original's metric), gen-pattern --alpha; verify requires LMBD GPU == CPU exactly; probe/decode/bench take .lmbd.
- docs/LMBD.md (LMB7 0x01 spec in our own words, 0x10 only "reserved for a future profile"), README codec table, LMBD section, benchmarks, roadmap, CHANGELOG, PROVENANCE, ATTRIBUTION, ARCHITECTURE.
- Five compatibility vectors tests/fixtures/lmbd_*.lmbd (72 KB, CC0 patterns, written by the ORIGINAL C encoder via scripts/mint-lmbd-vectors.sh, sidecars carry params + per-frame RGBA SHA-256 from the original decoder).

## Validated (numbers)
- Original-written files decode bit-exact: 13/13 vector frames equal the original decoder's RGBA SHA-256 (sequential and parallel chunks).
- Encoder vs original: byte-identical. 5/5 vectors re-encode to the same file (test); whole 256x144 4-frame λ1 4-chunk file identical (112,566 B); raw BC7 identical at λ0 and λ1 (zc16384) on 1920x1080 plasma, 1920x1080 tiles and 510x286 plasma (2,073,600 / 2,073,600 / 147,456 B each). Key finding: Apple clang fuses a*b+c (-ffp-contract=on); without reproducing its 100 fmuladd sites 202 bytes differ at λ0 on 1080p and 686,615 at λ1. The sites were taken from clang's own IR.
- Our BC7 decoder == bc7decomp (what the original RDO uses) on 1,000,000 random blocks, all modes: 0 mismatches. Specialised RDO decoder == spec decoder on 1.8 M random blocks (test).
- GPU vs CPU: delta 0 on all 13 vector frames (frame renderer and LmbdTexture) and the 14-frame 1080p clip; sRGB view within 1/255 of exact linear light.
- vs HAP Q at matched size (CC0, 512x288, 4 frames, glenc-cli hapy): smooth gradient +13.0 dB at 78% of HAP Q's bytes (65.00 vs 52.01 dB; SSIM 0.99985 vs 0.99887), and HAP Q's PSNR -1.1 dB at 8% of its bytes (λ0.25); flat tiles +6.4 dB at 58%; film-grain plasma +0.35 dB at 96% but SSIM 0.984 vs 0.995 (worse). The 13-25 dB claim reproduces only on smooth content (its source: all-mode bc7e + a subtle gradient and a third-party landscape frame).
- 16 layers 1080p LMB7, release, M4 Max: λ1 8 chunks decode-all p50 11.76 ms (0.73 ms/layer), decode+upload+convert 28.46 ms (1.78 ms/layer); λ0 7.49 / 18.63 ms. Single layer end to end 2.64 ms. The "<1 ms at 16 layers" claim was GPU time on 16 x 512x288 (0.39-0.51 ms); not comparable, and our CPU side is decode-bound: ruzstd 122 MB/s vs libzstd 968 MB/s per thread on the same λ1 chunks (446 vs 2119 MB/s at λ0).
- SSIM delta 0.005: that bar was 4-mode vs all-mode (0.0002-0.0024 in the original reports); we ship the 4-mode encoder byte-identical, so it carries over unchanged; all-mode not ported.
- Encode speed: λ1 zc16384, 14 1080p frames in 893 s on 14 cores (8.6 CPU-min/frame); the port is ~1.4x faster than the C original single-threaded (510x286: 38 s vs 52 s) after a specialised RDO decoder; λ0 30 x 1080p in 8 s.
- Fuzz: LMBD reader alone 600 s, 8 threads, 95,999,080 execs, 0 crashes, 0 hangs.
- Clean-room: 0 files in the repo share content (SHA-256) with any file of loomda-codec; no Shroomies/third-party media; _src deleted; no GPL/LGPL; cargo-deny green.

## LMBX inventory (read only; not ported, spec not published)
In loomda-codec @ c6d80e6: src/lmbx_pack.cpp, macos-player LmbxMetal.swift + lmbx-gate, phase0-bench/lmbx-b0/* (B0a-B1d prototypes, Swift GPU decode harnesses), reports PHASE-LMBX-B0a..B2a, alpha-corpus, banding-diagnostic; loombda-tierB-handoff.md. State: B2a plays a 300-frame 4K clip (YCoCg/DCT, per-frame rANS tables, tile directory, texFormat 0x10 added "additively"); chroma mode and inverse-transform exactness still open, as the brief says. Its test clips are Amrit's footage.

## Deviations
1. Compatibility vectors minted by the original tools on our CC0 patterns, not taken from the codec repo: its .lmbd files come from third-party film stills (dusk landscape), Amrit's footage, or an unlicensed test card; only odd510 is synthetic.
2. A hand-specialised BC7 decoder for the RDO pass (modes 1/5/6/7) made encoding 2x faster; proven identical to the spec decoder; output unchanged.
3. Decode stays on ruzstd as the brief mandates, which makes 16 x 1080p decode-bound (numbers above).
4. HAP Q numbers measured at 512x288 (the original's resolution) on three CC0 clips, not 1080p (RDO sweeps take minutes per frame).
5. The λ1 bench clip is 14 frames (one parallel encode batch); λ0 clips are 30.
6. DEFINITION_OF_DONE.md does not exist in loomda-codec (brief asked to read it). Its v0.1.0-mvp tag was not pushed (only main).
7. The original lmbd-encode writes temp DDS files to /tmp internally; used as-is to mint the vectors.
8. In-repo handoff written as docs/handoffs/w2a-r2.md (w2a.md holds the earlier BLOCKED record; never overwrite).

## Needs Amrit
- Zstd decode backend: keep pure-Rust ruzstd (8x slower on λ1 chunks) or allow libzstd for decode too (estimated 16 x 1080p decode ~2-3 ms instead of ~12 ms).
- Optional: push loomda-codec's tags (git push --tags) if the v0.1.0-mvp tag matters.
- Unchanged from W1: crates.io publishing; DXV still gated on Loombda-rs P1-1.

## Commits
- 62dda7f docs: trim tagline (STEP 1)
- eef092f docs: W2a handoff (BLOCKED)
- 325ad256b19cc8e2d23773aad08a1ec90b7fcda1 DXVKit Plus 0.2.0: LMBD (LMB7) — tag v0.2.0, pushed
- docs/handoffs/w2a-r2.md commit follows (hash in git log)
- URL: https://github.com/AmritusG/DXVKit-Plus

## RECEIPTS
```
$ git log -1 --format="%H %d %s"
325ad256b19cc8e2d23773aad08a1ec90b7fcda1  (HEAD -> main, tag: v0.2.0, origin/main) DXVKit Plus 0.2.0: LMBD (LMB7) — BC7 + RDO + Zstd, encoder and decoder
$ git status -sb | head -1
## main...origin/main
$ git ls-remote --tags origin | grep v0.2.0
37f357c42f1e496a490a24f79e9a2b999f70ad2a	refs/tags/v0.2.0
325ad256b19cc8e2d23773aad08a1ec90b7fcda1	refs/tags/v0.2.0^{}
$ gh repo view AmritusG/DXVKit-Plus --json description
Rust video for VJs: HAP, native OS codecs and the LMBD GPU codec, frame-exact, zero-copy into wgpu.

$ cargo fmt --all --check
fmt: clean
$ cargo clippy --workspace --all-targets -- -D warnings | tail -2
    Checking dxvkit-plus-cli v0.2.0 (/Users/amritrosell/Documents/claude/native-builds/DXVKit-Plus/crates/dxvkit-plus-cli)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.47s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace | tail -4
        PASS [   4.023s] (74/75) dxvkit-plus-lmbd-enc::encoder_identity encoding_the_vectors_patterns_reproduces_the_original_files
        PASS [   7.019s] (75/75) dxvkit-plus-cli::cli every_command_works_on_every_fixture
────────────
     Summary [   7.022s] 75 tests run: 75 passed, 0 skipped
$ cargo deny check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ cat docs/data/fuzz-lmbd-600s.txt
fuzz: 600 s, 8 threads, corpus 5 (hap frames 0, files 0, lmbd 5), execs 95999080 (159998/s), crashes 0, hangs 0, slowest input 263.3 ms

$ raw BC7, ours vs original bc7enc (zc16384)
plasma_1080_f00000 λ0: IDENTICAL (2073600 bytes)   tiles_1080_f00001 λ0: IDENTICAL (2073600 bytes)   plasma_odd_f00002 λ0: IDENTICAL (147456 bytes)
plasma_odd_f00002 λ1: IDENTICAL (147456 bytes)     tiles_1080_f00001 λ1: IDENTICAL (2073600 bytes)   plasma_1080_f00000 λ1: IDENTICAL (2073600 bytes)
$ cmp orig.lmbd ours.lmbd  (256x144, 4 frames, λ1, 4 chunks, 112566 B)
CONTAINER BYTE-IDENTICAL
$ bc7 decoder vs bc7decomp
blocks 1000000; validity mismatches 0; pixel mismatches by mode [0, 0, 0, 0, 0, 0, 0, 0, 0]

$ ls -la tests/fixtures/lmbd_*.lmbd
-rw-r--r--  15541  lmbd_plasma_alpha_odd_l05_c3.lmbd
-rw-r--r--  17483  lmbd_plasma_l0_c2.lmbd
-rw-r--r--  14928  lmbd_plasma_l1_c1.lmbd
-rw-r--r--   6232  lmbd_tiles_l1_c4.lmbd
-rw-r--r--  18556  lmbd_tiles_none_c2.lmbd
$ ls -la docs/data/ (new files)
-rw-r--r--   2997  bench-lmbd-macstudio.jsonl
-rw-r--r--    136  fuzz-lmbd-600s.txt
-rw-r--r--  19942  lmbd-vs-hapq.jsonl
$ clean-room hash overlap with loomda-codec files
overlap count: 0
```
Wire/Arena: not touched.

END OF HANDOFF dxvkit-plus-w2a sha256:da15e4af 9542
