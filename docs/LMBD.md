# LMBD — container and bitstream (profile LMB7)

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
| profile | `0x10` | reserved for a future profile |
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
