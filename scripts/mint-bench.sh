#!/bin/sh
# Mint benchmark clips (NOT committed): our own plasma footage with the frame index stamped in the
# top-left tiles, at 1920x1080 (60 frames) and 3840x2160 (30 frames), encoded with glenc-cli into
# every HAP variant (single section and 8 chunks where glenc offers chunks), the four DXV3 variants
# (DXT5 and YG10 from the plasma with an alpha ramp) and the native codecs.
# ffmpeg only wraps the PNGs as uncompressed ARGB (`rawvideo`).
#   GLENC=<path to glenc-cli> scripts/mint-bench.sh <scratch dir>
set -eu
SCRATCH="${1:?scratch dir}"
GLENC="${GLENC:-glenc-cli}"
mkdir -p "$SCRATCH"
for spec in 1080:1920:1080:60 2160:3840:2160:30; do
  tag="${spec%%:*}"; rest="${spec#*:}"; w="${rest%%:*}"; rest="${rest#*:}"; h="${rest%%:*}"; n="${rest#*:}"
  dir="$SCRATCH/frames_$tag"
  [ -d "$dir" ] || cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$dir" --plasma --frames "$n" --width "$w" --height "$h" >/dev/null
  src="$SCRATCH/src_$tag.mov"
  [ -f "$src" ] || ffmpeg -y -loglevel error -framerate 30 -i "$dir/f%05d.png" -c:v rawvideo -pix_fmt argb "$src"
  for job in hap1:1 hap1:8 hap5:8 hapy:1 hapa:1 hapm:1 h264:0 hevc:0 prores422:0 prores4444:0 mjpeg:0; do
    codec="${job%%:*}"; chunks="${job##*:}"
    case "$codec" in h264|hevc) ext=mp4; extra="--container mp4";; *) ext=mov; extra="";; esac
    out="$SCRATCH/bench${tag}_${codec}_c${chunks}.$ext"
    if [ "$chunks" -gt 1 ]; then extra="$extra --chunks $chunks"; fi
    # shellcheck disable=SC2086
    [ -f "$out" ] || "$GLENC" --no-audio -c "$codec" $extra "$src" "$out"
  done
  rm -f "$src"
  # DXV3: dxt1 / ycg6 from the opaque plasma, dxt5 / yg10 from the plasma with alpha.
  adir="$SCRATCH/frames_${tag}_alpha"
  [ -d "$adir" ] || cargo run -q --release -p dxvkit-plus-cli -- gen-pattern "$adir" --plasma --alpha --frames "$n" --width "$w" --height "$h" >/dev/null
  for job in dxt1:"$dir" ycg6:"$dir" dxt5:"$adir" yg10:"$adir"; do
    codec="${job%%:*}"; frames="${job#*:}"
    out="$SCRATCH/bench${tag}_dxv_${codec}.mov"
    if [ ! -f "$out" ]; then
      asrc="$SCRATCH/src_${tag}_${codec}.mov"
      ffmpeg -y -loglevel error -framerate 30 -i "$frames/f%05d.png" -c:v rawvideo -pix_fmt argb "$asrc"
      "$GLENC" --no-audio -c "$codec" "$asrc" "$out"
      rm -f "$asrc"
    fi
  done
done
ls -la "$SCRATCH"/bench*
