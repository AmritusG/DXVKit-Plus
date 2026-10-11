#!/bin/sh
# Re-mint the committed fixtures in tests/fixtures/ from our own procedural tile pattern (CC0):
# 128x72, 24 frames, 30 fps. Tools (external, never linked): the dxvkit-plus CLI (writes the PNG
# frames), ffmpeg ONLY to wrap the PNGs as an uncompressed ARGB .mov that AVFoundation can read
# (`rawvideo`: no ffmpeg codec touches the pixels), and glenc-cli (Amrit's encoder; AVFoundation /
# VideoToolbox for the native codecs) for every HAP variant and native codec.
#   GLENC=<path to glenc-cli> scripts/mint-fixtures.sh <scratch dir> [out dir, default tests/fixtures]
set -eu
SCRATCH="${1:?scratch dir}"
GLENC="${GLENC:-glenc-cli}"
OUT="${2:-$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures}"
mkdir -p "$OUT"
mkdir -p "$SCRATCH"
cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$SCRATCH/frames" --frames 24 --width 128 --height 72
ffmpeg -y -loglevel error -framerate 30 -i "$SCRATCH/frames/f%05d.png" -c:v rawvideo -pix_fmt argb "$SCRATCH/src_argb.mov"
mint() { name="$1"; shift; "$GLENC" --no-audio "$@" "$SCRATCH/src_argb.mov" "$SCRATCH/$name"; cp -p "$SCRATCH/$name" "$OUT/$name"; }
mint tiles_hap1.mov -c hap1
mint tiles_hap1_chunks4.mov -c hap1 --chunks 4
mint tiles_hap5.mov -c hap5
mint tiles_hapa.mov -c hapa
mint tiles_hapy.mov -c hapy
mint tiles_hapm.mov -c hapm
mint tiles_hapm_chunks4.mov -c hapm --chunks 4
mint tiles_h264.mp4 -c h264 --container mp4
mint tiles_hevc.mp4 -c hevc --container mp4
mint tiles_prores422.mov -c prores422
mint tiles_prores4444.mov -c prores4444
mint tiles_mjpeg.mov -c mjpeg
echo "minted into $OUT (update the .attribution sidecars if anything changed)"
