#!/bin/sh
# Build the Linux test plugins (docs/VERIFY.md § Linux plugin hosting) and lay them out as installed
# plugins under $1: $1/clap/LfTestGain.clap and $1/vst3/LfTestGain.vst3/Contents/x86_64-linux/LfTestGain.so.
set -eu
out=${1:?usage: build.sh <output folder>}
here=$(cd "$(dirname "$0")" && pwd)
target=${CARGO_TARGET_DIR:-/tmp/bleeploop-linux-plugins-target}
for crate in test-gain-clap test-gain-vst3; do
  mise exec rust@1.96.0 -- cargo build --release --locked --manifest-path "$here/$crate/Cargo.toml" --target-dir "$target"
done
mkdir -p "$out/clap" "$out/vst3/LfTestGain.vst3/Contents/x86_64-linux"
cp "$target/release/liblf_test_gain_clap.so" "$out/clap/LfTestGain.clap"
cp "$target/release/liblf_test_gain_vst3.so" "$out/vst3/LfTestGain.vst3/Contents/x86_64-linux/LfTestGain.so"
echo "LF_TEST_CLAP=$out/clap/LfTestGain.clap"
echo "LF_TEST_VST3=$out/vst3/LfTestGain.vst3"
