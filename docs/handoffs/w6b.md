project:    dxvkit-plus
pass:       w6b-lmbx-colour-shift
STATUS:     COMPLETE
parent:     20cba67
result:     2cceb98 (v0.6.1)
session:    d22520db-0877-4ed7-989c-786152e69bc6
written_at: 2026-10-12T05:28:28Z

# W6b — LMBX colour shift on the graphics eye clips: proven, fixed (DXVKit Plus 0.6.1)

## What shipped
- **0.6.1, encoder-only fix.** The bitstream stays v2. The CPU and GPU decoders are unchanged, and
  every v1 and v2 vector decodes bit-exact as before.
- **Exact forward YCoCg-R.** The encoder now uses the decoder's own quarter-unit definition.
- **Colour guard** (`LmbxParams::colour_tol`, CLI `encode-lmbd --colour-tol T|off`). Default 1 code
  value, which is LMB7's own worst flat-region error on the card (1.00–1.02). It works on two levels:
  - *DCT tiles:* a smooth block takes a finer step only where its DC would decode off its colour.
    This is an exact integer check, made in stream order along every plane's DC chain, on shown RGB
    and alpha.
  - *Flat and palette tiles:* a flat or lossy-palette tile may not move a *region* (a colour filling
    at least 16 uniform 2×2 cells) by more than the tolerance. Lossy palettes seed with the regions,
    a centroid holding regions moves to their mean only, and tiles with at most 16 colours also get
    lossy palettes, so a gradient's bands merge within the tolerance instead of going flat.
- **Docs and data:**
  - `docs/LMBD.md` §11.11 (encoder text, informative) and §11.12a;
  - CHANGELOG 0.6.1, README, ARCHITECTURE;
  - `docs/data/w6b-lmbx-colour.txt` (every table below in full).
- **Mint script:** `scripts/mint-lmbx-vectors.sh` never rebuilt the CLI it runs (`--example` makes
  cargo build only the example). Fixed.

## Diagnosis (completed before any code change; full tables in the data file)
**Step 1: codec, not writer.**
- For the 0.6.0 arms on eye frame 30, the per-colour error is the same in three independent reads:
  1. our CPU decode;
  2. the eye `.mov` read back through AVFoundation;
  3. Arena's own preview thumbnails of the clips.
- The writer adds the same ≤1.0 to every arm, the source clip included (ProRes rounding).
- LMB7's buffer alone is tagged StraightAlpha (its decode has alpha 254 in places). The source and
  LMBX are AlphaChannelIsOpaque, so the tag is not the difference.

**Step 2: per-colour shift.** Signed mean error, R G B, 8-bit code values, our decode, eye frame 30:

| Flat colour | v1 (0.5.0, at its 88.8 KB floor) | v2 4:4:4 (0.6.0) | v2 4:2:2 (0.6.0) | LMB7 |
|---|---|---|---|---|
| background (18,20,32) | −6.93 / 0.00 / −2.08 | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| panel (28,32,52) | −4.88 / −0.21 / −9.54 | 0 | 0 | 0 / 0 / −1 (all 60 frames) |
| bar (40,44,70) | −5.01 / −9.37 / +2.46 | 0 | 0 | 0 |
| green (60,200,90) | +3.33 / −1.04 / **+11.03** | 0 | 0 | 0 |
| blue (80,140,255) | −4.98 / +3.11 / −5.95 | 0 | 0 | 0 |
| red (230,60,60) | −0.65 / +5.79 / −4.03 | 0 | 0 | 0 |
| yellow (250,200,40) | +2.89 / +5.01 / +1.96 | 0 | 0 | 0 |
| orange (255,120,0) | −1.98 / +8.46 / +3.26 | 0 | 0 | 0 |
| gradient bar, worst row | ±13 | **R ±7, G/B ±2** (32-row stripes) | same | R 3, G/B 2 |

- v1's whole-frame bias is R −4.7, B −1.7 (cooler, greener).
- v2's flat fields are exact on the eye clip. Its visible defect is the gradient: every 32×32
  gradient tile (14 colours) was coded as one flat colour. The mean was exact, so neither PSNR nor a
  field mean caught it.
- On the 960×540 card at LMB7's 5.8 KB, v2 also missed white by −1.7 (lossy-palette centroids).

**Step 3: each candidate in the shared path, with a falsifying measurement.**
1. **YCoCg-R rounding — CAUSE (shared).**
   - Exhaustive over 16,777,216 colours: the encoder lifted in code values and scaled by 4, while
     the decoder inverts in quarter units.
   - 4,194,304 colours (25.0%) decode wrong: exactly those with odd Co and odd Cg, with R and B −1
     and G never affected.
   - The quarter-unit forward gets 0 wrong.
   - The bias is present at quality 1: (80,140,255) decodes −0.98 / 0 / −0.98.
2. **10-bit output and (v+2)>>2 — not a cause.** With the exact forward the whole chain is lossless
   on all 16.7M colours.
3. **Clamping — not a cause.** Unsaturated colours shift just as much (background −6.9), and none of
   the flat colours clamps.
4. **DC quantisation and prediction — CAUSE.** Not the floors (2 / 4), which are inactive at q ≥ 4.
   - The DC step grows with q, and DC prediction anchors each block to the previous reconstructed
     DC, so a flat block lands on a lattice of q/8 code values.
   - v1's starved arm runs at q 96 (Y, 12 code values) and chroma 154.
   - The lattice alone predicts the decoded value of **9 of 9** flat colours exactly, on the card and
     on the eye frame.
5. **Chroma minimum step 2 — not a cause:** inactive at chroma 154.
6. **4:2:2 halo — not a cause:** 4:4:4 and 4:2:2 flat means agree to 0.01.
7. **v2 only — CAUSES:**
   - flat-tile selection plus the rate-distortion choice accepting it on gradients (rows ±7);
   - Lloyd centroids pulled by anti-aliased edges (white −1.7 on the card).

## Validation (after the fix; Mac Studio)
- **Gates:**
  - fmt; clippy -D warnings in all three configurations;
  - nextest 111/111 with the GPU required, on both zstd backends;
  - cargo-deny ok on both backends.
- **Bit-exactness:**
  - LMBX GPU == CPU on 20 vector frames and 68 hostile frames.
  - The v1 and v2 vectors are unchanged and pass, and LMB7 stays byte-identical.
  - The encoder re-encodes the vectors' patterns deterministically at 1, 3 and 8 threads, pinned as
    `reencoded_sha256` in the sidecars.
  - No new vectors (the decoder is unchanged).
- **New tests:**
  - flat colours are exact through the DCT path at quality 1;
  - the colour guard holds every flat region of the card within 1 code value at quality 400 (v2,
    both chroma modes, with and without alpha);
  - DCT-only is checked per flat block, with a control without the guard that is off by more than 3.
- **Per-colour after the fix** (worst flat-region mean error, code values):
  - **Card at LMB7's bytes:** v2 0.00 (4:4:4) and 0.02 (4:2:2); LMB7 1.00; 0.6.0 v2 1.81 / 1.98.
  - **Eye clip, all 60 frames:** v2 0.00 / 0.00; LMB7 1.00; 0.6.0 v2 0.53; v1 11.03.
  - **Gradient rows:** within 1 (4:4:4) and 1 (4:2:2), against LMB7's 3 and 0.6.0's 7.
  - **v3 clips read back via AVFoundation:** every arm within 1.00, the ProRes rounding (identical on
    the source clip).
- **W6 A/B rows re-run at LMB7's bytes (0.6.0 → 0.6.1, 4:4:4):**

  | Row | 0.6.0 | 0.6.1 | LMB7 |
  |---|---|---|---|
  | Graphics card | 38.41 dB | **45.35** | 45.50 |
  | Graphics card with alpha (shown) | 35.92 | **43.77** | 46.66 |
  | Real alpha (RGB / shown / alpha) | 48.63 / 55.34 / 50.38 | 48.66 / 55.35 / 50.39 | — |
  | Glow | 43.00 | 43.06 | — |
  | Synthetic alpha | 52.57 | 53.38 | — |
  | Grain | 52.30 | 53.84 | — |
  | Detail | 21.31 | 21.31 | — |

  - Grain gains +1.5 dB from the transform fix.
  - 4:2:2 is likewise equal or better: glow 40.44 against 40.36.
- **Eye clip, 60 frames at LMB7's bytes:** v2 4:4:4 averages 59.98 dB (58.6–61.5) and 4:2:2 58.72
  dB, against LMB7's 49.7 and 0.6.0's 51.2.

## Eye clips (BUILD+TEST 4): built, NOT loaded
- **The resolume-arena MCP failed to connect this session.** Arena was not driven any other way.
- Clips: `~/Documents/claude/native-builds/DXVKit-Plus/_eye/LMBX eye test v3 - graphics -
  {source, lmb7, lmbx_v2_444, lmbx_v2_422halo}.mov`.
  - ProRes 4444 XQ, 1080p, 60 frames, 30 fps.
  - The LMBX arms are rate-controlled to LMB7's bytes (10,779 B / 10,784 B a frame against LMB7's
    10,856 B).
- Planned placement: layer "LMBX Eye Test", columns 17–20 in that order, nothing triggered.

## Deviations
1. **The fix is broader than "one parameter".** Three causes were proven (transform, DC lattice,
   mode decision), and all three were fixed in the encoder. The bitstream stays v2, so no v3 was
   needed.
2. **Built during the fix, measured and dropped** (numbers in the data file):
   - a static quantiser cap on smooth blocks, which raised the rate floor by 1.3 KB;
   - 1-D (32-pixel) regions, which counted anti-aliased lines and lost 7 dB on text tiles;
   - Zstandard-amortised tile rates, which gave worse rate-distortion;
   - pinned Lloyd seeds, superseded by region-mean centroids;
   - a DC-guard fallback to the finest step, which cost glow at 4:2:2 −0.45 dB and was fixed.
3. **The committed vectors are not re-minted.** They remain decoder conformance. The identity test
   now pins the 0.6.1 encoder's re-encodings instead of byte-matching the files.
4. **DCT-only 4:2:2 (non-default)** can miss by up to 1.06 on flat blocks next to a chroma-block
   edge. An odd luma block shares its even neighbour's chroma, so the test allows 1.25 there. The v2
   default is exact.
5. **The A/B footage frames were re-extracted** (the W6 scratch was deleted). The glow, synthetic,
   grain, detail and card LMB7 budgets match W6 to the byte. The real-alpha crop differs (354 KB
   against 362 KB), so 0.6.0 was re-run on the same frame for the comparison.

## Needs Amrit
- **Reconnect the resolume-arena MCP** so the four v3 clips can go into columns 17–20.
- **Your eye** on graphics source / LMB7 / v2 4:4:4 / v2 4:2:2. Look first at the gradient bar
  (0.6.0 striped it) and at whether the colours now read as the source.
- **Note on C14 (v1):** that arm was the 0.5.0 v1 encoder starved at its 88.8 KB floor. The
  shift you saw there is the DC lattice, now guarded. Files written by v1 encoders before 0.6.1 keep
  their shift, because the decoder is unchanged.

## Commits
- Release: `2cceb98` (tag v0.6.1, pushed).
- Handoff doc: the following commit.

## RECEIPTS
```
$ git log -1 --format="%H %d" (at release)
2cceb98ab6ff886a6efe83d4b65ad310220cc928  (HEAD -> main, tag: v0.6.1, origin/main)
$ git ls-remote origin refs/heads/main refs/tags/v0.6.1 'refs/tags/v0.6.1^{}' (at release)
2cceb98ab6ff886a6efe83d4b65ad310220cc928	refs/heads/main
a65ef6f18ede77963fa16092dd13abd122641e7c	refs/tags/v0.6.1
2cceb98ab6ff886a6efe83d4b65ad310220cc928	refs/tags/v0.6.1^{}
$ cargo fmt --all -- --check
fmt: clean
$ cargo clippy --workspace --all-targets  -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.88s
$ cargo clippy --workspace --all-targets --no-default-features --features zstd-rust -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.49s
$ cargo clippy --workspace --all-targets --all-features -- -D warnings | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.48s
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace --no-fail-fast --no-capture | grep (LMBX GPU line, Summary, FAIL)
LMBX GPU == CPU: 20 vector frames, 68 hostile frames
     Summary [  32.227s] 111 tests run: 111 passed, 0 skipped
$ DXVKIT_PLUS_REQUIRE_GPU=1 cargo nextest run --workspace --no-default-features --features zstd-rust --no-fail-fast | grep (Summary, FAIL)
     Summary [  10.674s] 111 tests run: 111 passed, 0 skipped
$ cargo deny check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ cargo deny --no-default-features --features zstd-rust check licenses bans sources | tail -1
bans ok, licenses ok, sources ok
$ ls -la _eye (v3 graphics eye clips)
21921521 Oct 12 07:22 LMBX eye test v3 - graphics - lmb7.mov
18433817 Oct 12 07:22 LMBX eye test v3 - graphics - lmbx_v2_422halo.mov
18215225 Oct 12 07:22 LMBX eye test v3 - graphics - lmbx_v2_444.mov
18154105 Oct 12 07:22 LMBX eye test v3 - graphics - source.mov
$ ls -la docs/data/w6b-lmbx-colour.txt
22993 Oct 12 07:24 docs/data/w6b-lmbx-colour.txt
$ du -sh target (before) ; cargo clean ; du -sh target (after)
target/ before 4.0G, after 0 (removed) —      Removed 17137 files, 4.3GiB total
$ Arena: resolume-arena MCP failed to connect (session start); Arena not driven; no clips loaded, nothing triggered
```
END OF HANDOFF w6b-lmbx-colour-shift sha256:bbf1849b 11774
