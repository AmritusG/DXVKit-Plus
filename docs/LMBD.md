# LMBD — container and bitstreams (profiles LMB7 and LMBX)

Sections 1-10 specify the container and profile LMB7; section 11 specifies profile LMBX (`0x10`),
which uses the same container.

LMBD is a video format for GPU playback: every frame is a BC7 (BPTC) texture, stored as a few
independently compressed chunks. A player decompresses the chunks (in parallel) and uploads the
blocks unchanged into a BC7 texture; the GPU's texture hardware decodes them when it samples. This
document specifies **LMBD version 1, profile LMB7** completely enough to write a reader or a writer
without looking at any code. It is the frozen v1 contract of the original LMBD codec; DXVKit Plus
reads files written by the original encoder bit for bit and writes byte-identical files from the
same input (see *Compatibility vectors*).

All multi-byte integers are **little-endian**. Structures are **packed**: fields sit at the exact
offsets given, with no padding, and are generally not naturally aligned — read them byte-wise.

## 1. File layout

```
offset 0      header (40 bytes)
offset 40     frame 0 payload
              frame 1 payload
              ...
              frame N-1 payload
index_offset  tail index (13 bytes per frame)
```

Frames are normally stored in order directly after the header and the index directly after the
last frame, but a reader must locate every frame through the index only.

## 2. Header (40 bytes at offset 0)

| Offset | Size | Field | Value |
|---|---|---|---|
| 0x00 | 4 | magic | the ASCII bytes `L` `M` `B` `D` (`4C 4D 42 44`) |
| 0x04 | 2 | version | `1` |
| 0x06 | 1 | profile | `0x01` = LMB7 (see §6) |
| 0x07 | 1 | texture format | `0x01` = BC7 RGBA, 16 bytes per 4x4 block |
| 0x08 | 4 | width | picture width in pixels, 1..16384 |
| 0x0C | 4 | height | picture height in pixels, 1..16384 |
| 0x10 | 4 | fps numerator | > 0 |
| 0x14 | 4 | fps denominator | > 0; the frame rate is numerator / denominator frames per second |
| 0x18 | 4 | frame count | N, the number of index entries |
| 0x1C | 1 | colorspace | `0` = sRGB-encoded colour, `1` = linear colour |
| 0x1D | 1 | transfer | reserved: write 0, ignore on read |
| 0x1E | 2 | reserved | write 0, ignore on read |
| 0x20 | 8 | index offset | absolute file offset of the tail index |

**Colorspace.** With `0` (the default and what every current encoder writes) the BC7 colour values
are sRGB-encoded: sample the texture through an sRGB view (`BC7_RGBA_UNORM_SRGB`) to obtain linear
light, or through the plain view (`BC7_RGBA_UNORM`) to obtain the stored values. Alpha is always
linear and straight (not premultiplied).

## 3. The block grid

A frame is `bx = ceil(width / 4)` by `by = ceil(height / 4)` BC7 blocks, in raster order (left to
right, then top to bottom). One frame of blocks is exactly

```
frame_bytes = bx * by * 16
```

bytes. When the width or height is not a multiple of 4, the texture is `bx*4 x by*4` texels and
the picture is its top-left `width x height`; the extra texels are unspecified (the reference
encoder fills them by repeating the last column and row). Each block is a standard 128-bit BC7
block as defined by the Khronos Data Format Specification ("BPTC Compressed Texture Image
Formats") and Microsoft's "BC7 Format": any of the eight modes may appear; a block whose first
byte is 0 (the reserved mode) decodes to transparent black.

## 4. Frame payload

A frame payload starts at the offset its index entry gives. All chunk offsets inside it are
relative to the payload's first byte.

```
+0     u8   profile echo     same byte as header offset 0x06
+1     u16  chunk count      K >= 1
+3     K chunk entries, 13 bytes each:
         +0  u8   compressor     (see §6)
         +1  u32  raw size       decompressed bytes of this chunk
         +5  u32  packed size    stored bytes of this chunk
         +9  u32  offset         where this chunk's stored bytes begin (relative to +0)
+3+13K the chunks' stored bytes
```

Rules:

- The chunks, decompressed and concatenated **in table order**, are the frame's block stream:
  the raw sizes add up to exactly `frame_bytes`, and each raw size is a multiple of 16 (a chunk
  never splits a block).
- Each chunk's stored bytes lie inside the payload, after the chunk table. Writers store the
  chunks back to back in table order directly after the table, so the payload size is
  `3 + 13*K + sum(packed sizes)`; readers use the offsets.
- Compressor `0x00` (none): the stored bytes are the raw blocks; packed size equals raw size.
- Compressor `0x01` (Zstd): the stored bytes are one or more Zstandard frames (RFC 8878) that
  decompress to exactly the raw size. Chunks are independent: decompress them in any order, in
  parallel.

**Chunking** is the writer's choice and does not change the picture. The reference encoder splits
a frame of `B` blocks into `K` runs, the first `B mod K` runs getting `floor(B/K) + 1` blocks and
the others `floor(B/K)`. A reader must not assume this.

## 5. Tail index (at the header's index offset)

N entries of 13 bytes, frame order:

```
+0   u64  offset     absolute file offset of the frame payload
+8   u32  size       payload bytes (= 3 + 13*K + sum(packed sizes))
+12  u8   keyframe   1 (every LMB7 frame is an intra frame; carried for later use)
```

Seeking to frame `n` is reading index entry `n`.

## 6. Enumerations

| Field | Value | Meaning |
|---|---|---|
| profile | `0x01` | **LMB7**: BC7 RGBA blocks as above |
| profile | `0x02` | reserved (a YCoCg variant of LMB7, not defined) |
| profile | `0x10` | LMBX (§11); its texture format must be `0x11` |
| texture format | `0x11` | LMBX frame bitstream version 1 (only with profile `0x10`) |
| texture format | `0x10` | the never-frozen LMBX prototype: refused (`Unsupported`), not decoded |
| texture format | `0x01` | BC7 RGBA (16 bytes per 4x4 block) |
| compressor | `0x00` | none |
| compressor | `0x01` | Zstd |
| compressor | `0x02` | reserved (LZ4, not defined) |
| colorspace | `0x00` / `0x01` | sRGB / linear |

A reader must reject reserved and unknown values (and say which) rather than guess.

## 7. Reading, step by step

1. Read 40 bytes; check the magic, version 1, profile `0x01`, texture format `0x01`, size and
   frame rate in range, colorspace 0 or 1.
2. Read `13 * N` bytes at the index offset; every entry must lie inside the file and be at least
   3 bytes long.
3. For frame `n`: read the payload; check the profile echo; parse the chunk table; check every chunk
   lies inside the payload and that the raw sizes are whole blocks adding up to `frame_bytes`.
4. Decompress each chunk into its slice of a `frame_bytes` buffer (in parallel if you like); a
   Zstd chunk must produce exactly its raw size.
5. Upload the buffer into a `bx*4 x by*4` BC7 texture (`bytes per row = bx * 16`) and sample the
   top-left `width x height`.

Everything in the file should be treated as untrusted: check each size before allocating and each
offset before reading.

## 8. Worked example

A 16x8 frame (4x2 blocks = 128 bytes), one uncompressed chunk, 30 fps, one frame (197 bytes):

```
00000000: 4c4d 4244 0100 0101 1000 0000 0800 0000   "LMBD", v1, LMB7, BC7, 16 x 8
00000010: 1e00 0000 0100 0000 0100 0000 0000 0000   30/1 fps, 1 frame, sRGB, reserved 0
00000020: b800 0000 0000 0000                       index at 0xB8 = 184
00000028: 0101 0000 8000 0000 8000 0000 1000 0000   echo 01, 1 chunk; [none, raw 128, packed 128, at 16]
00000038: (128 bytes of BC7 blocks)
000000b8: 2800 0000 0000 0000 9000 0000 01          frame 0 at 40, 144 bytes, keyframe
```

## 9. The reference encoder (informative)

The format does not prescribe an encoder. The reference encoder — the original LMBD tool, and
`dxvkit-plus-lmbd-enc`, which reproduces it byte for byte — does:

1. **BC7, 4 modes.** Each 4x4 block (edge-padded) is encoded by bc7enc_rdo's `bc7enc` (Richard
   Geldreich, Jr.; MIT or public domain): modes 1 and 6 for opaque blocks, 5, 6 and 7 for blocks
   with alpha, linear RGBA error, uber level 4, all 64 two-subset partitions with its partition
   filter bank.
2. **Rate-distortion optimisation** (when lambda > 0): bc7enc_rdo's entropy-reduction pass walks
   the blocks in raster order and, where `MSE * w + bits * lambda` improves, copies a run of 3-16
   bytes from an earlier block in a 16384-byte window (so the LZ stage finds a match), then tries a
   second match. Flat, mid-luma regions of at least 64 blocks are protected with a higher error
   weight. Default lambda 1.0; 0 disables it.
3. **Zstd level 9** on each block-aligned chunk (libzstd 1.5.7; one chunk per frame by default).

Byte-for-byte reproduction also needs the original's floating-point behaviour: it is compiled with
fused multiply-adds where the C source writes `a * b + c` in one expression (Apple clang's default
`-ffp-contract=on`), which `dxvkit-plus-lmbd-enc` reproduces operation for operation.

## 10. Compatibility vectors

`tests/fixtures/lmbd_*.lmbd` are files written by the original encoder from our own CC0 test
patterns (`scripts/mint-lmbd-vectors.sh`). Each `.attribution` sidecar records the parameters and
the SHA-256 of every frame as the original decoder returns it (straight RGBA8 at the picture size).
They cover: lambda 0, 0.5 and 1; 1, 2, 3 and 4 chunks; Zstd and uncompressed chunks; opaque and
alpha content (modes 1, 5, 6, 7); and a size that is not a multiple of 4 (130 x 74).

## 11. Profile LMBX (`0x10`)

LMBX trades BC7's fixed 8 bits per pixel for variable-rate coding: YCoCg-R colour, an 8x8 DCT with
an **exactly specified integer inverse**, a quantiser chosen per 8x8 block, and rANS entropy
coding. Every plane of every 32x32 tile is an independent stream, so a GPU decodes thousands of
streams at once. Every step below is integer arithmetic with one defined result for every input
that passes validation, so every conforming decoder — CPU, GPU, anyone's — produces identical
samples. Decisions: LMBX-D1 (integer inverse transform, so outputs are bit-identical, not "within 1
code value"), LMBX-D2 (two signalled chroma modes, 4:2:2 with halo and 4:4:4) and LMBX-D3 (bitstream
version 2, 0.6.0: per-tile **modes** — DCT, flat, palette — signalled in a tile mode map, and a
directory of stream lengths; version 1, 0.5.0, is frozen and still decoded) and LMBX-D4 (bitstream
version 3, 0.7.0: context-coded palette indices, palettes of up to 64 colours, exact edge alpha,
quad tiles of four 16x16 sub-tiles, and DCT colour with an alpha palette — §11.6a; versions 1 and 2
are frozen and still decoded). LMBX-D4 in Amrit's words: W7 was asked to "See if alpha and graphics
can be done, and wgat it would give" (2026-10-12); of shipping the new version with a speed-recovery
step before it is frozen or keeping v2 with an encoder-only gain, he chose "speed recovery"
(2026-10-13). W8 recovered the GPU decode speed (§11.12b) before freezing v3.

### 11.1 In the container

Header: profile `0x10`, texture format `0x11`; width, height, frame rate, frame count, index as in
§2; colorspace `0` (the samples are sRGB-encoded code values). A frame payload is as in §4 except
that chunk raw sizes are **not** block-aligned: the chunks' raw data, concatenated in table order,
is the frame's **LMBX bitstream** (§11.2), whose total size must be 1 ..= `16 * cw * ch + 2^20`
bytes (`cw`, `ch` the coded size, §11.2). Writers store one chunk per frame — since 0.6.0
Zstandard-compressed when that is smaller (it is on most real frames: 7-28% on real footage, 75-80%
on graphics, mostly the quantiser map, the directory and repeated tiles), uncompressed otherwise;
readers accept any chunking and compressor. Every LMBX frame is intra (keyframe 1).

### 11.2 Frame bitstream

All integers little-endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 4 | magic | `L` `M` `B` `X` |
| 4 | 2 | version | `1`, `2` or `3` (a bitstream starting `LMXS` is the prototype: refuse) |
| 6 | 1 | chroma | `0` = 4:4:4, `1` = 4:2:2 halo |
| 7 | 1 | flags | bit 0: an alpha plane is coded; other bits 0 |
| 8 | 4 | width | equals the container's width |
| 12 | 4 | height | equals the container's height |
| 16 | 2 | chroma ratio | `cr`, 1 ..= 65535, in 1/256 |
| 18 | 2 | reserved | 0 |
| 20 | 32 | quantiser steps | `QT[0..16]`, u16 each, 1 ..= 4095 |
| 52 | … | frequency table | §11.3 (coefficients: 2048 symbols) |
| … | … | palette table | **v2 only**: §11.3 with 16 symbols (palette indices, §11.6) |
| … | … | index tables | **v3 only**: eight tables, §11.3 with 65 symbols and precision 10 (palette indices, contexts 0..7, §11.6a) |
| … | … | edge-alpha table | **v3 only**: §11.3 with 256 symbols and precision 10 (§11.6a) |
| … | `T` | tile mode map | **v2, v3**: one byte per tile, raster. v2: `0` DCT, `1` flat, `2` palette (§11.6). v3: colour mode (low nibble: `0` DCT, `1` flat, `2` palette, `3` quad) plus 16 times the alpha mode (high nibble: `0` own, `1` edge, `2` alpha palette) (§11.6a) |
| … | `8 * T` | quantiser map | §11.4 |
| … | 4 | stream count | `S = T * P` |
| … | `4 * S` | stream directory | **v1**: offsets `off[0..S]`, relative to the data start; `off[0] = 0`, non-decreasing, each <= the data length. **v2, v3**: lengths `len[0..S]`; `off[i]` = the sum of `len[0..i]`, and the sum of all lengths must not exceed the data length |
| … | … | data | the streams; stream `i` is bytes `off[i] .. off[i+1]` (the last ends at the bitstream's end) |

Geometry: tiles are 32x32. `tx = ceil(width / 32)`, `ty = ceil(height / 32)`, `T = tx * ty`
tiles in raster order. Coded size `cw = 32 * tx`, `ch = 32 * ty`. Planes: Y at `cw x ch`; Co and
Cg at `cwc x ch` with `cwc = cw` (4:4:4) or `cw / 2` (4:2:2); A at `cw x ch` when flag bit 0 is
set. `P = 3` (Y, Co, Cg) or 4 (with A). Stream `t * P + k` is plane `k` (0 Y, 1 Co, 2 Cg, 3 A) of
tile `t`.

### 11.3 Frequency table (rANS)

`u16 n` (1 ..= the alphabet size), then `n` entries of (`u16 symbol`, `u16 freq`): symbols strictly
increasing, each < the alphabet size (2048 for coefficients, 16 for v2 palette indices, 65 for v3
palette indices, 256 for v3 edge alpha), every freq >= 1, the freqs adding up to exactly `2^b` — the
table's **precision** `b` is 14 (16384), except v3's index and edge-alpha tables, whose precision is
10 (1024; W8: as compact as 12 bits within 0.5%, and faster to decode). Absent symbols have
frequency 0. `cum[s]` = the sum of the frequencies of all symbols below `s`. Coefficient symbols: `s < 2047` is a coefficient level
`s - 1023`; `2047` ends a block (EOB). One table of each kind serves every stream of the frame.

### 11.4 Quantiser map

`8` bytes per tile, tile order: the quantiser index of each of the tile's 4x4 luma blocks (raster),
two per byte, the even block in the low nibble. Luma and alpha blocks use step `q = QT[index]`;
chroma block (column `bx`, row `by` within the tile) uses
`qc = clamp((QT[index of luma block (lbx, by)] * cr + 128) >> 8, 2, 4095)` with `lbx = 2 * bx`
(4:2:2) or `bx` (4:4:4). (Chroma steps are at least 2: Co and Cg span ±255, so their DC reaches
±2040 and every level must fit ±1023.)

### 11.5 Decoding a stream

rANS with 32-bit state `x`, byte-wise renormalisation, lower bound `L = 2^23`; each symbol is read
with its table, of precision `b` (§11.3). Bytes of the stream are read in order; **reading past the
stream's end gives 0** (so a short or empty stream is still defined).

```
x = byte0 | byte1 << 8 | byte2 << 16 | byte3 << 24          (the stream's first four bytes)
next symbol (table of precision b):
    slot = x & (2^b - 1)
    s    = the symbol with cum[s] <= slot < cum[s] + freq[s]
    x    = freq[s] * (x >> b) + slot - cum[s]                (modulo 2^32)
    repeat at most 4 times: if x >= 2^23 stop; else x = (x << 8 | next byte) (modulo 2^32)
```

A stream holds its plane's 8x8 blocks of the tile in raster order (Y and A: 4x4 blocks; 4:4:4
chroma 4x4; 4:2:2 chroma 2 wide x 4 high). Per block: read symbols into the 64 coefficients in
**zigzag order** (below) until EOB or until all 64 are read (no EOB follows the 64th); the rest are
0. Coefficient `k` goes to raster position `ZIGZAG[k]` (row = vertical frequency):

```
 0  1  8 16  9  2  3 10 17 24 32 25 18 11  4  5 12 19 26 33 40 48 41 34 27 20 13  6  7 14 21 28
35 42 49 56 57 50 43 36 29 22 15 23 30 37 44 51 58 59 52 45 38 31 39 46 53 60 61 54 47 55 62 63
```

Dequantise: `D = clamp(level * step, -4095, 4095)` — except the DC (coefficient 0), which is
predicted: each stream keeps `dc`, the previous block's dequantised DC (0 before its first block),
and `D[0] = clamp(dc + level * dcstep, -4095, 4095)` with `dcstep = max(step, 2)` for Y and A and
`max(step, 4)` for Co and Cg (a DC residual reaches ±2040 or ±4080 and must fit a level of ±1023;
the DC is 8x the block mean, so this costs at most a quarter / a half code value); when the block
ends before its first
coefficient (an immediate EOB), `D[0] = dc`. Either way `dc` becomes `D[0]`. (Measured: 6-28%
fewer bytes at equal quality — flat runs cost only their end-of-block.)

### 11.6 Tile modes (v2)

A v1 frame's tiles are all DCT tiles. In v2 the mode map gives each tile's mode. A **DCT** tile is
decoded as in §11.5 and §11.7. A **direct** tile (flat or palette) is 32x32 RGBA8 pixels read from
its stream 0 alone (its other streams and its quantiser map are ignored; writers leave them empty
and zero). Bytes past the stream's end read as 0:

- **Flat** (`1`): bytes 0..4 are R, G, B, A; every pixel is that colour.
- **Palette** (`2`): byte 0 is `n` (read 0 as 1, above 16 as 16), then `n` colours of 4 bytes (R, G,
  B, A), then a rANS stream (§11.5, with the palette table) of 1024 symbols for the pixels in raster
  order: index = `(pred + s) mod n`, `pred` = the left pixel's index (at column 0 the index of the
  pixel above, at the tile's first pixel 0); the pixel is colour `index`.

Without an alpha plane every direct pixel's alpha is 255. A direct tile writes into the sample
planes (so the 4:2:2 halo of a DCT neighbour reads defined values): Y = the pixel's RGBA8 packed as
`R | G << 8 | B << 16 | A << 24` (as a signed 32-bit integer), A = `4 * A`, and Co and Cg the
forward YCoCg-R of `4R, 4G, 4B` (`Co = 4R - 4B`, `t = 4B + (Co >> 1)`, `Cg = 4G - t`), for 4:2:2
the rounded mean `(c[2x] + c[2x+1] + 1) >> 1` of each column pair.

### 11.6a Tile modes (v3)

In v3 the mode byte gives each tile a **colour mode** (low nibble) and an **alpha mode** (high
nibble). Valid bytes: colour mode `0` ..= `3`; alpha mode `0` (own); `1` (**edge**) only with an
alpha plane and colour mode `2` or `3`; `2` (**alpha palette**) only with an alpha plane and colour
mode `0`. Any other byte is invalid (§11.9).

A **DCT** tile (`0`) is decoded as in §11.5 and §11.7 — except, with alpha mode `2`, its A stream
(stream 3), which holds an alpha palette instead of DCT blocks: byte 0 is `n` (read 0 as 1, above
64 as 64), then `n` alpha levels of one byte, then one rANS stream of the 32x32 tile's palette
indices (context-coded, below); the A sample of each pixel is `4 *` its index's level (its A blocks
are not transformed).

A **direct** tile (colour mode `1` ..= `3`) is 32x32 RGBA8 pixels read from its stream 0 alone (as
in v2: its other streams and its quantiser map are ignored; writers leave them empty and zero).
Bytes past the stream's end read as 0.

- **Flat** (`1`): bytes 0..4 are R, G, B, A.
- **Palette** (`2`): a palette header — byte `n` (read 0 as 1, above 64 as 64), then `n` colours
  of 4 bytes (R, G, B, A) — then one rANS stream of the 32x32 tile's pixels: per pixel, raster
  order, its context-coded index (below) and, with the edge alpha mode, its edge-alpha residual.
- **Quad** (`3`): four 16x16 sub-tiles — top-left, top-right, bottom-left, bottom-right — each a
  kind byte, then either (kind `0`, flat) 4 colour bytes or (any other kind, palette) a palette
  header; then one rANS stream (starting right after the last header): the sub-tiles in order,
  each pixel of a sub-tile in raster order within it: a palette sub-tile's pixel takes its
  context-coded index within the 16x16 region, then its edge-alpha residual; a flat sub-tile's
  pixel takes only its edge-alpha residual.

**Context-coded indices** of a region (`s x s`, `s` = 32 or 16, `n` colours or levels), pixel `i =
y * s + x` in raster order: `l` = the index to the left (column 0: the index above; the region's
first pixel: 0); `a` = the index above (row 0: `l`); `ar` = above-right and `al` = above-left (row 0,
or outside the region: `a`). The symbol is read with index table `c = 4 (l == a) + 2 (a == ar) +
(al == l)`: symbol `0` is the index `l`, `1` is `a`, `s >= 2` is `(l + s - 1) mod n`. (Every index is
below `n`.)

**Edge alpha** (alpha mode `1`): after a pixel's index (a flat sub-tile's pixel: at its turn), if
the alpha `A` of its colour is neither 0 nor 255, one edge-alpha symbol `e` follows, and the pixel's
alpha becomes `(A + e) mod 256`. Pixels whose colour has alpha 0 or 255 take none.

Without an alpha plane every direct pixel's alpha is 255. A v3 direct tile writes into the sample
planes as in §11.6.

### 11.7 Integer inverse DCT

With the integer basis `M[k][n]` (frequency `k`, sample `n`) — `1448` for `k = 0`, otherwise
`round(2048 cos((2n + 1) k pi / 16))` — that is:

```
k=0  1448  1448  1448  1448  1448  1448  1448  1448
k=1  2009  1703  1138   400  -400 -1138 -1703 -2009
k=2  1892   784  -784 -1892 -1892  -784   784  1892
k=3  1703  -400 -2009 -1138  1138  2009   400 -1703
k=4  1448 -1448 -1448  1448  1448 -1448 -1448  1448
k=5  1138 -2009   400  1703 -1703  -400  2009 -1138
k=6   784 -1892  1892  -784  -784  1892 -1892   784
k=7   400 -1138  1703 -2009  2009 -1703  1138  -400
```

each block of dequantised coefficients `D[u][v]` becomes samples, in **quarter code values**:

```
t[i][j]   = (sum over k of M[k][i] * D[k][j] + 512) >> 10
out[i][j] = ((sum over l of t[i][l] * M[l][j] + 2048) >> 12) + offset
```

`>>` is an arithmetic shift (rounds towards minus infinity); `offset` is 512 for Y and A, 0 for Co
and Cg. Every value fits a signed 32-bit integer: `|D| <= 4095` and `|M| <= 2009` give
`|first sum| <= 8 * 2009 * 4095 < 2^26`, `|t| <= 65520`, `|second sum| <= 8 * 65520 * 2009 < 2^31`.
The block's samples go to its 8x8 area of the plane (the tile's position plus the block's). Only
DCT tiles' blocks are transformed.

### 11.8 Output

For each picture pixel `(x, y)` (the top-left `width x height` of the coded planes), in quarter
code values: in a direct tile (v2, v3), `R, G, B, A = 4 *` the pixel's RGBA8 (§11.6, §11.6a);
otherwise

```
Y  = Y plane at (x, y)
Co, Cg: 4:4:4: the chroma planes at (x, y)
        4:2:2: c = x >> 1; even x: plane at (c, y);
               odd x: (plane(c, y) + plane(min(c + 1, cwc - 1), y) + 1) >> 1     (the halo: c + 1 may
               be in the next tile)
t = Y - (Cg >> 1);  G = Cg + t;  B = t - (Co >> 1);  R = B + Co
A  = A plane at (x, y) with alpha, else 1020
R, G, B, A each clamped to 0 ..= 1020
```

This RGBA (straight alpha, sRGB-encoded) in quarter code values is the **normative output**.
8-bit: `(v + 2) >> 2`. 10-bit: `(v * 1023 + 510) / 1020`.

### 11.9 Validation

Reject a frame whose magic, version, chroma, flags, size, reserved bytes, quantiser steps, frequency
tables, tile modes (v2: 0 ..= 2; v3: §11.6a), stream count or directory break the rules above. Nothing else is an error: stream contents are
defined for every byte pattern (§11.5), so a decoder needs no per-stream checks.

### 11.10 Conformance vectors

`tests/fixtures/lmbx_*.lmbd` (CC0, `scripts/mint-lmbx-vectors.sh`). Version 1: both chroma modes,
with and without alpha, quality 4 to 48, sizes 200x130, 333x141 and 97x61 (none a multiple of 32).
Version 2 (`lmbx_v2_*`): graphics cards (with and without alpha, 333x185) whose frames mix DCT, flat
and palette tiles (exact and lossy palettes; 4:2:2 at quality 400, so DCT tiles border direct ones
through the halo), and a plasma with alpha (DCT tiles, the length directory). Version 3
(`lmbx_v3_*`, 0.7.0): graphics cards with and without alpha (palette tiles of up to 64 colours with
context-coded indices, edge alpha, quad tiles with flat and palette sub-tiles), a coarse 4:2:2
mixed card (direct tiles bordering DCT ones through the halo), and a plasma with alpha (DCT colour
with an alpha palette). Each
sidecar holds every frame's SHA-256 of the normative output (`width * height * 4` u16 little-endian,
rows top first). Any conforming decoder must reproduce them; DXVKit Plus's CPU decoder
(`dxvkit_plus_core::lmbx`) and its GPU compute decoder (`dxvkit-plus-wgpu`) both do.

### 11.11 The reference encoder (informative)

`dxvkit_plus_lmbd_enc::lmbx` (from Amrit's B1d prototype): RGBA to YCoCg-R in quarter code values,
the forward the decoder's inverse undoes exactly (`Co = 4R - 4B`, `t = 4B + (Co >> 1)`,
`Cg = 4G - t`, `Y = t + (Cg >> 1)`; the definition §11.6 uses), edge-replicated to the coded size;
4:2:2 chroma = the rounded mean of each column pair. (Before 0.6.1 the encoder lifted in code values
and scaled by 4: the floors do not scale, so a quarter of all colours — odd `R - B` and odd `G - t`
— decoded 1 low in R and B at any quality.) A float 8x8 DCT. Per block, the
quantiser index is the step nearest `base * clamp(AC energy / 380, 0.16, 2.6)` (`base` = the quality
setting, default 16). Levels by deadzone quantisation (bias 0.45) refined by RDOQ against the
actual rANS cost (lambda = 0.25 q^2, two passes over a bootstrapped rate model) with an
end-of-block trellis; DC and the two lowest AC terms are never deadzoned or dropped (banding).
Chroma ratio 1.0 (4:2:2) or 1.6 (4:4:4); the default mode is 4:4:4, chosen by measurement on real
footage (W5). The DC residual is quantised against the decoder's
reconstructed DC. Rate control: the best quality whose frame fits a byte budget (bisection on
`base`; the budget counts the stored, possibly Zstandard-compressed size).

Version 2 (the default since 0.6.0; `--modes dct` writes v1): per tile, a rate-distortion choice
between the DCT coding and the direct modes, `J = D + lambda * bits` with `lambda = 0.25 * mean(q^2)`
over the tile's blocks, `D` the squared error approximately in RGB terms (DCT: `3 eY^2 + eCo^2 / 2 +
3 eCg^2 / 4`, from the quantised coefficients), `bits` from the frame's own tables (a 32-bit state
per stream). Candidates: flat (the tile's mean colour), the exact palette when the tile has at most
16 colours, and lossy palettes — the 2, 4, 8 and 16 most frequent colours refined by two Lloyd
iterations, each pixel mapped to the nearest (RGBA squared distance). The coefficient and palette
tables are built from the tiles that use them.

**Colour guard** (0.6.1, default tolerance 1 code value; `--colour-tol T|off`). PSNR alone let the
encoder move a flat region's colour: the guard holds every region's mean within the tolerance.
(a) DCT tiles: a block's DC is a residual from the previous block's reconstructed DC at step
`dc_step(q)`, so a flat block can only land on that lattice — at the rate floor's `q` 96 (chroma 154)
a flat colour moved up to 12 code values. For each smooth block (luma AC energy under 23), in
stream order with every plane's DC chain, the encoder keeps the block's step if the exact decoded
colour of its DC (the integer IDCT and inverse YCoCg-R) is within the tolerance of the block's mean,
shown channels (RGB times alpha) and alpha, else takes the coarsest step that is. Only the block
where a colour changes needs a finer step: the run after it repeats an exact DC for nothing.
(b) Direct tiles: a *region* is a colour that fills at least 16 uniform 2x2 cells of the tile (an
area, not an anti-aliased edge line). A flat or lossy-palette candidate that moves any region by
more than the tolerance is not a candidate; lossy palettes seed with the regions, a centroid holding
regions moves to their mean only (not pulled by edge pixels), and tiles with at most 16 colours get
the lossy palettes too, so near-equal regions such as a gradient's bands can merge within the
tolerance instead of the tile going flat.

Version 3 (the default since 0.7.0; `--modes dct` still writes v1): the same decision over the v3
candidates: flat; palettes of the tile (the exact palette when it has at most 64 colours; lossy
palettes of 2, 3, 4, 6, 8, 12 and 16 colours, Lloyd-refined as in v2 — lossy palettes above 16
colours cost glow 0.88 dB in W7); with partial alpha, the same palettes of the tile's canonical form
(every partial alpha read as 128) with edge alpha, so the residual makes every partial alpha exact;
quad (per 16x16 sub-tile, the best of its mean colour and its palettes by an approximate rate), with
and without edge alpha; and, with alpha, DCT colour with an alpha palette (the exact levels, up to
64, or 1 to 16 Lloyd levels). Index and edge-alpha rates come from every candidate's symbols, then
from the first choice's; the tables are built from the chosen tiles. Writing is deterministic at
any thread count (each tile's candidates are a pure function of its pixels).

**Rate-distortion steps** (0.7.1, default on; `--rd-steps off` is 0.7.0's encoder, byte for byte).
The step rule above picks a block's step from its luma AC energy alone. In 4:4:4 frames with tile
modes the encoder now refines it, on every RDO pass against that pass's rate model: each 8x8 block
of a DCT tile takes, of the rule's index and the one either side, the index of least
`J = sum over planes of w * (AC squared error) + lambda * (AC bits)`, with RDOQ levels at each
candidate step, v3's decision weights (`w` = 3, 1/2, 3/4 for Y, Co, Cg; 1 for A) and the tile's
`lambda = 0.25 * mean(q^2)` of the rule's steps. The colour guard then runs on the chosen steps as
before. Tiles with at most 64 colours (graphics, which an exact palette can code) keep the rule's
steps, so graphics frames are 0.7.0's byte for byte; 4:2:2 frames are unchanged too. The q-map
already carries a step per block, so the frames are plain v3 and every v3 decoder reads them.
Rate control continues its bisection below 0.25, to 1/64, while its best frame is more than 3%
under the budget: the rule's step walls (every smooth block's step going from 3 to 4 at quality
21.875, for example) can otherwise leave it far under. Measured in W10 (as its 8x8 "control" arm)
and W11: `docs/data/w11-lmbx-quality.txt`. W10 also measured adaptive transform sizes (8/16/32,
a v4 prototype): +0.0 to +0.07 dB on real footage over these steps, for +17-24% GPU decode time;
not adopted (branch `w10-explore`).

### 11.12 What version 2 measured (informative)

W6 (`docs/data/w6-lmbx-v2-ab.txt`; every arm at LMB7's bytes per frame, Zstandard where the arm uses
it). Kept: Zstandard frames (every content, e.g. +1.8 dB on real alpha, graphics floor 25 KB to 8 KB);
flat and palette tiles (graphics 27.8 dB to 38.4 dB at 5.8 KB; real alpha +0.8 dB shown); lossy
palettes (graphics +2 dB, noise-like detail +2.8 dB); 16 palette colours (better than 8 or 4); the
length directory (12% of a graphics frame). Built, measured and removed: premultiplied-colour coding
(−2.4 dB shown on real alpha: the 8-bit straight output undoes its gain), an alpha quantiser ratio
(0.5, 0.75, 1.25, 2 — none beat 1 on real alpha), alpha-aware colour fill and visibility-weighted RDO
(−0.4 to −0.8 dB shown on real alpha), alpha-weighted direct-mode distortion (+4 dB on an alpha
graphics card, −3.7 dB shown on real alpha), lossless (MED-predicted) tiles (at most ±0.1 dB once
palettes exist), copy tiles (7 bytes: Zstandard already removes repeated tiles), 4:2:2 as the
default (4:4:4 better on both real clips; 4:2:2 wins only on synthetic grain, +0.2 dB).

### 11.12a What the colour guard measured (informative)

W6b (`docs/data/w6b-lmbx-colour.txt`). The shift Amrit saw on the graphics eye clips had three
proven causes, none in the decoder: the forward YCoCg-R scale (above; 25% of colours 1 low in R and
B), the DC lattice at coarse steps (0.5.0's v1 arm, starved at its rate floor: flat fields off by up
to 12 code values, predicted exactly from the lattice for every flat colour), and v2's mode decision
coding a gradient's tiles flat (32-row stripes, rows off by up to 7). With the guard, at LMB7's
bytes per frame: the graphics card 45.35 dB (0.6.0 38.41, LMB7 45.50), every flat region within
0.02 (LMB7 1.00); with alpha 43.77 dB shown (0.6.0 35.92); the 1080p graphics eye clip 60.0 dB mean
(0.6.0 51.2, LMB7 49.7), flat regions exact; natural footage equal or better (glow 43.06 against
43.00, real alpha 48.66 / 55.35 against 48.63 / 55.34, grain +1.5 dB).

### 11.12b What version 3 measured (informative)

W7 (`docs/data/w7-lmbx-v3-ab.txt`, exploration): at LMB7's bytes the graphics card with alpha lost
2.9 dB shown to LMB7 (43.77 against 46.66), 86% of it in the 0.24% of pixels with partial alpha;
0.6.1 sat at its rate floor there, 75% of which was palette index streams. Context-coded indices cut
the index bits by 62% and alone gave 59.9 dB shown; with 17..64-colour exact palettes, edge alpha
and quad tiles the card codes losslessly in fewer bytes than LMB7. Built, measured and dropped:
escape colours (noise), direct colour with a DCT alpha plane (no gain), visibility-weighted alpha
(no gain), lossy palettes above 16 colours (glow −0.88 dB), BC7 tiles (lose on graphics; +4.6 dB on
noise-like detail at a 2-4 minute encode per 1080p frame — a separate question).

W8 (`docs/data/w8-lmbx-v3-profile.txt`, `w8-lmbx-v3-speed-macstudio.jsonl`,
`w8-lmbx-v3-quality.txt`): the prototype decoded 21% (glow) to 33% (graphics) slower than 0.6.1.
Profiled with Metal System Trace: v3 adds nothing to DCT tiles (a DCT-only v3 frame decodes within
1% of v1); the cost was the serial palette decode — each palette tile's 1024-symbol stream ran in
the same SIMD groups as DCT streams, and a SIMD group full of palette tiles touches a different cache
line per lane at every step. The decoder now runs DCT streams and direct tiles in separate
workgroups of one dispatch, one direct tile every 8 lanes, with one-word table slots; v3's format
changes for speed: the direct payload in stream 0 (no extra stream per tile), edge alpha coded per
pixel (no second sweep), escapes removed, and 10-bit index tables. Rejected after measurement: a
row-copy / row-run symbol (+10% bytes, slower), a row-start context (−1.5% bytes, +4% time),
index tables in workgroup memory (occupancy), splitting the tables pass.

### 11.13 Why 32x32 tiles with a stream per plane (informative)

Measured on the Mac Studio (`docs/data/w5-lmbx-tile-ab.txt`): at the same picture, 32x32 tiles
cost 3.5% more bytes than 64x64 tiles but decode 2.2-2.4x faster on the GPU (16 layers of 1080p:
11.5-13.1 ms vs 24.9-28.1 ms per step), because the serial part of decoding — one rANS stream — is
7x shorter and there are 8x more of them to run in parallel.

