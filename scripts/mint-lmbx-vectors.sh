#!/bin/sh
# Mint the LMBX conformance vectors (tests/fixtures/lmbx_*.lmbd) from our own CC0 procedural
# patterns with the dxvkit-plus encoder, and print, per vector, the SHA-256 of the file and of every
# frame's normative output (RGBA in quarter code values, u16 little endian) — those go into the
# .attribution sidecars. Any conforming LMBX decoder must reproduce the frame hashes.
#   scripts/mint-lmbx-vectors.sh <scratch dir> [out dir]
set -eu
SCRATCH="${1:?scratch dir}"
OUT="${2:-$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures}"
mkdir -p "$SCRATCH" "$OUT"
cargo build -q --release -p dxvkit-plus-cli -p dxvkit-plus-core --example lmbx_hashes
BIN="$(dirname "$0")/../target/release"
"$BIN/dxvkit-plus" gen-pattern "$SCRATCH/tiles" --frames 3 --width 200 --height 130 >/dev/null
"$BIN/dxvkit-plus" gen-pattern "$SCRATCH/plasma_a" --frames 2 --width 333 --height 141 --plasma --alpha >/dev/null
"$BIN/dxvkit-plus" gen-pattern "$SCRATCH/plasma_odd" --frames 2 --width 97 --height 61 --plasma >/dev/null
"$BIN/dxvkit-plus" gen-pattern "$SCRATCH/graphics" --frames 2 --width 333 --height 185 --graphics >/dev/null
"$BIN/dxvkit-plus" gen-pattern "$SCRATCH/graphics_a" --frames 2 --width 333 --height 185 --graphics --alpha >/dev/null
# mint <name> <pattern> <chroma> <quality> [tile modes, e.g. flat,palette,lossy — v2]
# Frames are stored uncompressed (the vectors test the bitstream, not Zstandard).
mint() {
  name="$1"; src="$2"; chroma="$3"; quality="$4"; modes="${5:-}"
  "$BIN/dxvkit-plus" encode-lmbd "$SCRATCH/$src" "$OUT/$name.lmbd" --profile lmbx --chroma "$chroma" --quality "$quality" --threads 1 --quiet \
    --modes "${modes:-dct}" --compressor none >/dev/null
  echo "== $name.lmbd sha256: $(shasum -a 256 "$OUT/$name.lmbd" | cut -d' ' -f1) params: pattern=$src chroma=$chroma quality=$quality${modes:+ modes=$modes}"
  "$BIN/examples/lmbx_hashes" "$OUT/$name.lmbd"
}
mint lmbx_tiles_422_q4 tiles 422halo 4
mint lmbx_tiles_444_q4 tiles 444 4
mint lmbx_plasma_alpha_422_q16 plasma_a 422halo 16
mint lmbx_plasma_alpha_444_q16 plasma_a 444 16
mint lmbx_plasma_odd_422_q48 plasma_odd 422halo 48
# Bitstream v2 (tile modes).
mint lmbx_v2_graphics_444_q16 graphics 444 16 flat,palette,lossy
mint lmbx_v2_graphics_alpha_422_q32 graphics_a 422halo 32 flat,palette,lossy
mint lmbx_v2_plasma_alpha_444_q48 plasma_a 444 48 flat,palette,lossy
mint lmbx_v2_graphics_mixed_422_q400 graphics 422halo 400 flat,palette,lossy
