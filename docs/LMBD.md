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
code value") and LMBX-D2 (two signalled chroma modes, 4:2:2 with halo and 4:4:4).

### 11.1 In the container

Header: profile `0x10`, texture format `0x11`; width, height, frame rate, frame count, index as in
§2; colorspace `0` (the samples are sRGB-encoded code values). A frame payload is as in §4 except
that chunk raw sizes are **not** block-aligned: the chunks' raw data, concatenated in table order,
is the frame's **LMBX bitstream** (§11.2), whose total size must be 1 ..= `16 * cw * ch + 2^20`
bytes (`cw`, `ch` the coded size, §11.2). Writers store one uncompressed chunk per frame (the
bitstream is entropy-coded already); readers accept any chunking and compressor. Every LMBX frame
is intra (keyframe 1).

### 11.2 Frame bitstream

All integers little-endian.

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 4 | magic | `L` `M` `B` `X` |
| 4 | 2 | version | `1` (a bitstream starting `LMXS` is the prototype: refuse) |
| 6 | 1 | chroma | `0` = 4:4:4, `1` = 4:2:2 halo |
| 7 | 1 | flags | bit 0: an alpha plane is coded; other bits 0 |
| 8 | 4 | width | equals the container's width |
| 12 | 4 | height | equals the container's height |
| 16 | 2 | chroma ratio | `cr`, 1 ..= 65535, in 1/256 |
| 18 | 2 | reserved | 0 |
| 20 | 32 | quantiser steps | `QT[0..16]`, u16 each, 1 ..= 4095 |
| 52 | … | frequency table | §11.3 |
| … | `8 * T` | quantiser map | §11.4 |
| … | 4 | stream count | `S = T * P` |
| … | `4 * S` | stream offsets | `off[0..S]`, relative to the data start; `off[0] = 0`, non-decreasing, each <= the data length |
| … | … | data | the streams; stream `i` is bytes `off[i] .. off[i+1]` (the last ends at the bitstream's end) |

Geometry: tiles are 32x32. `tx = ceil(width / 32)`, `ty = ceil(height / 32)`, `T = tx * ty`
tiles in raster order. Coded size `cw = 32 * tx`, `ch = 32 * ty`. Planes: Y at `cw x ch`; Co and
Cg at `cwc x ch` with `cwc = cw` (4:4:4) or `cw / 2` (4:2:2); A at `cw x ch` when flag bit 0 is
set. `P = 3` (Y, Co, Cg) or 4 (with A). Stream `t * P + k` is plane `k` (0 Y, 1 Co, 2 Cg, 3 A) of
tile `t`.

### 11.3 Frequency table (rANS)

`u16 n` (1 ..= 2048), then `n` entries of (`u16 symbol`, `u16 freq`): symbols strictly increasing,
each < 2048, every freq >= 1, the freqs adding up to exactly 16384. Absent symbols have frequency 0.
`cum[s]` = the sum of the frequencies of all symbols below `s`. Symbol meaning: `s < 2047` is a
coefficient level `s - 1023`; `2047` ends a block (EOB). One table serves every stream of the frame.

### 11.4 Quantiser map

`8` bytes per tile, tile order: the quantiser index of each of the tile's 4x4 luma blocks (raster),
two per byte, the even block in the low nibble. Luma and alpha blocks use step `q = QT[index]`;
chroma block (column `bx`, row `by` within the tile) uses
`qc = clamp((QT[index of luma block (lbx, by)] * cr + 128) >> 8, 2, 4095)` with `lbx = 2 * bx`
(4:2:2) or `bx` (4:4:4). (Chroma steps are at least 2: Co and Cg span ±255, so their DC reaches
±2040 and every level must fit ±1023.)

### 11.5 Decoding a stream

rANS with 32-bit state `x`, 14-bit probabilities, byte-wise renormalisation, lower bound `L = 2^23`.
Bytes of the stream are read in order; **reading past the stream's end gives 0** (so a short or
empty stream is still defined).

```
x = byte0 | byte1 << 8 | byte2 << 16 | byte3 << 24          (the stream's first four bytes)
next symbol:
    slot = x & 16383
    s    = the symbol with cum[s] <= slot < cum[s] + freq[s]
    x    = freq[s] * (x >> 14) + slot - cum[s]               (modulo 2^32)
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

### 11.6 Integer inverse DCT

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
The block's samples go to its 8x8 area of the plane (the tile's position plus the block's).

### 11.7 Output

For each picture pixel `(x, y)` (the top-left `width x height` of the coded planes), in quarter
code values:

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

### 11.8 Validation

Reject a frame whose magic, version, chroma, flags, size, reserved bytes, quantiser steps, frequency
table, stream count or offsets break the rules above. Nothing else is an error: stream contents are
defined for every byte pattern (§11.5), so a decoder needs no per-stream checks.

### 11.9 Conformance vectors

`tests/fixtures/lmbx_*.lmbd` (CC0, `scripts/mint-lmbx-vectors.sh`): both chroma modes, with and
without alpha, quality 4 to 48, sizes 200x130, 333x141 and 97x61 (none a multiple of 32). Each
sidecar holds every frame's SHA-256 of the normative output (`width * height * 4` u16 little-endian,
rows top first). Any conforming decoder must reproduce them; DXVKit Plus's CPU decoder
(`dxvkit_plus_core::lmbx`) and its GPU compute decoder (`dxvkit-plus-wgpu`) both do.

### 11.10 The reference encoder (informative)

`dxvkit_plus_lmbd_enc::lmbx` (from Amrit's B1d prototype): RGBA to YCoCg-R in integers
(`Co = R - B`, `t = B + (Co >> 1)`, `Cg = G - t`, `Y = t + (Cg >> 1)`), edge-replicated to the coded
size; 4:2:2 chroma = the rounded mean of each column pair. A float 8x8 DCT. Per block, the
quantiser index is the step nearest `base * clamp(AC energy / 380, 0.16, 2.6)` (`base` = the quality
setting, default 16). Levels by deadzone quantisation (bias 0.45) refined by RDOQ against the
actual rANS cost (lambda = 0.25 q^2, two passes over a bootstrapped rate model) with an
end-of-block trellis; DC and the two lowest AC terms are never deadzoned or dropped (banding).
Chroma ratio 1.0 (4:2:2) or 1.6 (4:4:4); the default mode is 4:4:4, chosen by measurement on real
footage (W5). The DC residual is quantised against the decoder's
reconstructed DC. Rate control: the best quality whose frame fits a byte budget (bisection on
`base`).

### 11.11 Why 32x32 tiles with a stream per plane (informative)

Measured on the Mac Studio (`docs/data/w5-lmbx-tile-ab.txt`): at the same picture, 32x32 tiles
cost 3.5% more bytes than 64x64 tiles but decode 2.2-2.4x faster on the GPU (16 layers of 1080p:
11.5-13.1 ms vs 24.9-28.1 ms per step), because the serial part of decoding — one rANS stream — is
7x shorter and there are 8x more of them to run in parallel.

