#!/bin/sh
# Run `dxvkit-plus bench` over the clips minted by scripts/mint-bench.sh and collect JSON lines.
#   scripts/bench-all.sh <bench dir> <out.jsonl>
set -eu
DIR="${1:?bench dir}"; OUT="${2:?out.jsonl}"
cargo build -q --release -p dxvkit-plus-cli
BIN="$(dirname "$0")/../target/release/dxvkit-plus"
: > "$OUT"
for f in "$DIR"/bench1080_* "$DIR"/bench2160_*; do
  case "$f" in
    *_hap*) extra="--layers 16";;
    *h264*|*hevc*) extra="--import";;
    *) extra="";;
  esac
  # shellcheck disable=SC2086
  "$BIN" bench "$f" --frames 30 $extra >> "$OUT"
done
wc -l "$OUT"
