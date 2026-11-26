#!/usr/bin/env bash
# All Blender renders explicitly use Cycles CPU.
set -euo pipefail
cd "$(dirname "$0")"
if (( $# > 1 )) || [[ $# == 1 && $1 != --draft ]]; then
  echo "Usage: render.sh [--draft]" >&2
  exit 2
fi
src=.
fingerprint=$(sha256sum "$src"/*.py "$src/composite.cpp" "$src/render.sh" "$src"/inputs/*.ogg \
  "$src/inputs/rack-bass.flac" "$src"/inputs/fonts/*.ttf | sha256sum | cut -c1-16)
export PROMO_WORK="$PWD/output/$fingerprint"
mkdir -p output deliverables
# Concurrent invocations share the same three delivery filenames.
exec 9>output/render.lock
flock 9
if [[ -d "$PROMO_WORK" ]]; then
  echo "Resuming render in $PROMO_WORK"
else
  echo "Starting clean render in $PROMO_WORK"
  mkdir "$PROMO_WORK"
fi
python_bin=${PROMO_PYTHON:-python3}
blender_bin=${PROMO_BLENDER:-blender}
threads=${PROMO_THREADS:-12}
export OPENBLAS_NUM_THREADS=1
render() {
  nice -n 7 "$blender_bin" -b -t "$threads" --python-exit-code 1 -P "$src/scene.py" -- "$@"
}
"$python_bin" "$src/audio.py"
g++ -O3 -std=c++17 -fopenmp "$src/composite.cpp" -lpng -o "$PROMO_WORK/composite"
render --audit
if [[ ${1:-} == --draft ]]; then
  for aspect in wide tall; do
    render --format "$aspect" --frames 302 --dir draft --scale 25 --samples 2
  done
  echo "Draft plates in $PROMO_WORK/draft"
  exit 0
fi
for aspect in wide tall; do
  render --format "$aspect" --project
  render --format "$aspect" --dir plates --frames plates --samples 10
  "$python_bin" "$src/finish.py" "$aspect"
done
render --hero --dir poster --frames 302 --samples 48
"$python_bin" "$src/finish.py" hero
"$python_bin" "$src/verify.py"
sha256sum deliverables/BleepLoop-Ring-of-Takes-{16x9.mp4,9x16.mp4,Hero.png} > "$PROMO_WORK/deliverables.sha256"
echo "Verified delivery hashes in $PROMO_WORK/deliverables.sha256"
