#!/bin/sh
# Mint the LMBD compatibility vectors (tests/fixtures/lmbd_*.lmbd) with the ORIGINAL C encoder, from
# our own CC0 procedural patterns. The vectors prove that files written by the original decode
# bit-exact here, and (with the parameters in each sidecar) that our Rust encoder writes the same
# bytes.
#
# Tools: the dxvkit-plus CLI (writes the PNG frames) and a build of the original LMBD tools from
# Amrit's loomda-codec repository @ c6d80e6 (private): lmbd-encode (built with its default native
# 4-mode bc7enc), lmbd-write, lmbd-probe, and bc7enc itself (bc7enc_rdo, cmake Release, no OpenMP —
# exactly as the repository's build.sh does). Nothing from that repository is in this one.
#   LMBD_TOOLS=<dir with lmbd-encode lmbd-write lmbd-probe bc7enc> scripts/mint-lmbd-vectors.sh <scratch dir> [out dir]
# Prints, per vector, the SHA-256 of the file and of every frame as the ORIGINAL decoder (lmbd-probe
# decode, bc7enc_rdo's scalar BC7 reference) sees it; those go into the .attribution sidecars.
set -eu
SCRATCH="${1:?scratch dir}"
T="${LMBD_TOOLS:?set LMBD_TOOLS}"
OUT="${2:-$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures}"
mkdir -p "$SCRATCH" "$OUT"
cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$SCRATCH/tiles" --frames 4 --width 128 --height 72 >/dev/null
cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$SCRATCH/plasma" --frames 3 --width 128 --height 72 --plasma >/dev/null
cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$SCRATCH/plasma_a" --frames 2 --width 130 --height 74 --plasma --alpha >/dev/null
frames() { ls "$SCRATCH/$1"/f*.png | head -n "$2"; }
report() {
  f="$1"; n="$2"
  echo "== $(basename "$f") sha256: $(shasum -a 256 "$f" | cut -d' ' -f1)"
  i=0
  while [ "$i" -lt "$n" ]; do
    "$T/lmbd-probe" decode "$f" "$i" "$SCRATCH/dec.rgba" >/dev/null
    echo "decoded_rgba_sha256 frame $i: $(shasum -a 256 "$SCRATCH/dec.rgba" | cut -d' ' -f1)"
    i=$((i + 1))
  done
}
enc() { name="$1"; w="$2"; h="$3"; shift 3; "$T/lmbd-encode" -o "$SCRATCH/$name" -w "$w" -h "$h" -fps 30/1 "$@" >/dev/null 2>&1; cp -p "$SCRATCH/$name" "$OUT/$name"; }
enc lmbd_tiles_l1_c4.lmbd 128 72 -q 1 -zc 16384 -chunks 4 $(frames tiles 4)
enc lmbd_plasma_l1_c1.lmbd 128 72 -q 1 -zc 16384 -chunks 1 $(frames plasma 3)
enc lmbd_plasma_alpha_odd_l05_c3.lmbd 130 74 -q 0.5 -zc 16384 -chunks 3 $(frames plasma_a 2)
enc lmbd_plasma_l0_c2.lmbd 128 72 -q 0 -zc 16384 -chunks 2 $(frames plasma 2)
# Uncompressed chunks (compressor 0x00), written by the original lmbd-write from raw bc7enc blocks.
for i in 0 1; do
  "$T/bc7enc" -z1 -zc16384 -g -q "$SCRATCH/tiles/f0000$i.png" "$SCRATCH/t$i.dds" >/dev/null
  tail -c 9216 "$SCRATCH/t$i.dds" > "$SCRATCH/t$i.bc7raw"
done
"$T/lmbd-write" -o "$SCRATCH/lmbd_tiles_none_c2.lmbd" -w 128 -h 72 -fps 30/1 -chunks 2 "$SCRATCH/t0.bc7raw" "$SCRATCH/t1.bc7raw" >/dev/null
cp -p "$SCRATCH/lmbd_tiles_none_c2.lmbd" "$OUT/lmbd_tiles_none_c2.lmbd"
report "$OUT/lmbd_tiles_l1_c4.lmbd" 4
report "$OUT/lmbd_plasma_l1_c1.lmbd" 3
report "$OUT/lmbd_plasma_alpha_odd_l05_c3.lmbd" 2
report "$OUT/lmbd_plasma_l0_c2.lmbd" 2
report "$OUT/lmbd_tiles_none_c2.lmbd" 2
echo "minted into $OUT (update the .attribution sidecars if anything changed)"
