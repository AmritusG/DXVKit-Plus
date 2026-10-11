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
mint() {
  name="$1"; src="$2"; chroma="$3"; quality="$4"
  "$BIN/dxvkit-plus" encode-lmbd "$SCRATCH/$src" "$OUT/$name.lmbd" --profile lmbx --chroma "$chroma" --quality "$quality" --threads 1 --quiet >/dev/null
  echo "== $name.lmbd sha256: $(shasum -a 256 "$OUT/$name.lmbd" | cut -d' ' -f1) params: pattern=$src chroma=$chroma quality=$quality"
  "$BIN/examples/lmbx_hashes" "$OUT/$name.lmbd"
}
mint lmbx_tiles_422_q4 tiles 422halo 4
mint lmbx_tiles_444_q4 tiles 444 4
mint lmbx_plasma_alpha_422_q16 plasma_a 422halo 16
mint lmbx_plasma_alpha_444_q16 plasma_a 444 16
mint lmbx_plasma_odd_422_q48 plasma_odd 422halo 48
