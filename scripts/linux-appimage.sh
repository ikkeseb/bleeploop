#!/bin/sh
# Build the Linux app as one AppImage: src-tauri/target/release/bundle/appimage/BleepLoop-x86_64.AppImage.
# `tauri build` prepares the AppDir (tauri.linux.conf.json: AppImage only, the licences beside the app);
# its own linuxdeploy step is run again here with two changes: a copy of Tauri's GTK plugin that
# tolerates a gdk-pixbuf without a loaders folder (Arch's since 2.44; the stock plugin stops there), and
# libpipewire left out, so the system's JACK library (pipewire-jack) always meets its own PipeWire
# (Arch updates PipeWire under an AppImage that would otherwise carry an older copy).
# Tauri's cache (~/.cache/tauri) is read, never changed. Needs network once, for Tauri's first download.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cache=${XDG_CACHE_HOME:-$HOME/.cache}/tauri
tools=$(mktemp -d /tmp/bleeploop-linuxdeploy-XXXXXX)
trap 'rm -rf "$tools"' EXIT
cd "$root"
# The first run downloads linuxdeploy into Tauri's cache; its bundle step is expected to fail on Arch.
NO_STRIP=true APPIMAGE_EXTRACT_AND_RUN=1 pnpm tauri build --bundles appimage || true
appdir=src-tauri/target/release/bundle/appimage/BleepLoop.AppDir
[ -x "$appdir/usr/bin/app" ] || { echo "tauri build left no AppDir at $appdir" >&2; exit 1; }
cp "$cache/linuxdeploy-x86_64.AppImage" "$cache/linuxdeploy-plugin-appimage.AppImage" "$cache/linuxdeploy-plugin-gtk.sh" "$tools/"
sed -i 's|^copy_lib_tree "$gdk_pixbuf_binarydir" "$APPDIR/"$|if [ -d "$gdk_pixbuf_binarydir" ]; then copy_lib_tree "$gdk_pixbuf_binarydir" "$APPDIR/"; else mkdir -p "$(dirname "$APPDIR/${gdk_pixbuf_cache_file/$LD_GTK_LIBRARY_PATH//usr/lib}")"; fi|' "$tools/linuxdeploy-plugin-gtk.sh"
grep -q 'if \[ -d "$gdk_pixbuf_binarydir" \]' "$tools/linuxdeploy-plugin-gtk.sh" || { echo "the GTK plugin changed: its gdk-pixbuf step was not patched" >&2; exit 1; }
chmod +x "$tools"/*
out=src-tauri/target/release/bundle/appimage
rm -f "$out"/*.AppImage
# Deploy (GTK included), then drop libpipewire (it arrives with the excluded libjack's dependencies),
# then pack: the packer adds no library.
(cd "$out" && PATH="$tools:$PATH" NO_STRIP=true APPIMAGE_EXTRACT_AND_RUN=1 "$tools/linuxdeploy-x86_64.AppImage" --appimage-extract-and-run \
  --appdir BleepLoop.AppDir --plugin gtk)
rm -f "$appdir"/usr/lib/libpipewire-0.3.so*
(cd "$out" && APPIMAGE_EXTRACT_AND_RUN=1 "$tools/linuxdeploy-plugin-appimage.AppImage" --appimage-extract-and-run --appdir BleepLoop.AppDir)
ls -la "$out"/BleepLoop-x86_64.AppImage
"$out"/BleepLoop-x86_64.AppImage --version
