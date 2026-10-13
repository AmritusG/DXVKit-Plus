# LMBX bitstream v3 — W7 PROTOTYPE (branch `w7-explore`, not frozen, not shipped)

> **Superseded.** The frozen v3 (0.7.0) is specified in [`LMBD.md`](LMBD.md) §11.6a; it differs
> from this prototype (no escapes, no extra stream per tile, edge alpha per pixel, 10-bit index
> tables, alpha modes renumbered, no BC7 tiles). Kept as the record of what W7 measured.

This is the delta from version 2 (`docs/LMBD.md` §11) that the W7 prototypes decode. It exists
only on the `w7-explore` branch so every W7 candidate could be measured alone and in combination.
Versions 1 and 2 are unchanged and still decode bit-exact. Nothing here is a release; if a part of
it ships, it is re-specified and frozen then.

## Header and tables

`version = 3`. After the coefficient table:

| Field | Value |
|---|---|
| palette flags | `u8`: bit 0 = context-coded indices; other bits 0 |
| index tables | 8 tables (context coding) or 1, each §11.3 with **66** symbols |
| edge-alpha table | §11.3 with 256 symbols |
| tile mode map | one byte per tile: low nibble colour mode, high nibble alpha mode |

Colour modes: `0` DCT, `1` flat, `2` palette, `3` quad, `4` BC7. Alpha modes: `0` the colour mode's
own; `1` DCT (direct colour modes only, needs the alpha flag): the tile's A stream is a DCT alpha
plane with the quantiser map; `2` edge (palette or quad, needs alpha); `3` alpha palette (DCT
colour only, needs alpha). Anything else is invalid.

The quantiser map is as v2. **Streams per tile are `P + 1`**: the planes as v2, then the tile's
*direct stream* (`S = T * (P + 1)`, directory of lengths as v2). A direct tile's payload is its
direct stream (its plane streams are empty, except A under alpha mode 1).

## Direct stream payloads

Bytes past the stream's end read as 0.

- **Flat**: R, G, B, A.
- **Palette header** (used by palette and quad): byte `nb`; `n = clamp(nb & 127, 1, 64)`; if
  `nb & 128`, a `u16 e` (escape count) follows; then `n` RGBA colours; then `e` RGBA escape colours.
- **Palette**: a palette header, then one rANS stream (§11.5) holding the 1024 indices
  (raster), then with alpha mode 2 the edge-alpha residuals.
- **Quad**: four 16x16 sub-tiles (top-left, top-right, bottom-left, bottom-right), each a kind
  byte (`0`: flat, 4 colour bytes; otherwise: a palette header); then one rANS stream: each
  palette sub-tile's 256 indices in order, then the edge-alpha residuals.
- **BC7**: 64 BC7 blocks of 16 bytes, raster in the tile's 8x8 grid of 4x4 blocks (CPU only in the
  prototype; the GPU decoder refuses frames with BC7 tiles).
- **Alpha palette** (DCT colour, alpha mode 3): `n = clamp(byte 0, 1, 64)`, `n` alpha levels, then
  one rANS stream of 1024 indices; A = `4 *` the pixel's level.

## Palette indices

A region (`size` 32 or 16) has `n` index values: the colours, plus one (the escape) when `e > 0`.
Per pixel, raster: `l` = the left index (column 0: the index above; the region's first pixel: 0).

- **Context coding** (flag bit 0): `a` = the index above (row 0: `l`), `ar` above-right and `al`
  above-left (outside the region: `a`); table `4 (l == a) + 2 (a == ar) + (al == l)`; symbol `0` →
  `l`, `1` → `a`, `s >= 2` → `(l + s - 1) mod n`.
- **Otherwise** (v2's rule): one table, `(l + s) mod n`.

Index `i < colours` is that colour; index `colours` is the next escape colour, in order
(transparent black past `e`).

## Edge alpha (alpha mode 2)

After all indices, for every pixel of the tile in raster order whose colour came from a palette
entry or a flat sub-tile (not an escape) and has `0 < A < 255`: one edge-alpha symbol `s`,
`A = (A + s) mod 256`. (The encoder codes partial-alpha pixels against a partial entry, so the
residual makes their alpha exact.)

## Output

As v2: a direct tile's pixels are their RGBA8; with alpha mode 1 the alpha is the A plane's
sample (clamped `0..=1020`). A DCT tile with alpha mode 3 takes its A plane from the alpha palette.
