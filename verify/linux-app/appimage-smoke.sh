#!/bin/sh
# Start a BleepLoop AppImage headless, as from one click, and close it: a private PipeWire graph with two
# null sinks (nothing reaches the speakers), a private X display, a temporary HOME. Passes when the app
# logs its start and its plugin scan and still runs; keeps a screenshot of the window.
# Usage: sh verify/linux-app/appimage-smoke.sh BleepLoop-x86_64.AppImage SCREENSHOT.png [PLUGINS_DIR]
set -eu
app=$(realpath "$1"); shot=$2; plugins=${3:-}
dir=$(mktemp -d /tmp/bleeploop-appimage-smoke-XXXXXX)
run="$dir/runtime"; home="$dir/home"; mkdir -m 700 "$run"; mkdir -p "$home"
pids=""
cleanup() { for p in $pids; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM
export PIPEWIRE_RUNTIME_DIR="$run" PIPEWIRE_REMOTE=pipewire-0 XDG_RUNTIME_DIR="$run" DISABLE_RTKIT=1 PIPEWIRE_CONFIG_DIR=/usr/share/pipewire
unset PIPEWIRE_QUANTUM PIPEWIRE_LATENCY PIPEWIRE_RATE PIPEWIRE_PROPS PIPEWIRE_CONFIG_NAME WAYLAND_DISPLAY
XDG_CONFIG_HOME="$run" pipewire >/dev/null 2>&1 & pids="$pids $!"
i=0; until [ -S "$run/pipewire-0" ]; do i=$((i+1)); [ $i -lt 200 ] || { echo "no private PipeWire"; exit 1; }; sleep 0.025; done
for n in fixture_in fixture_out; do
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$n node.description=$n media.class=Audio/Sink object.linger=true audio.position=[ FL FR ] adapter.auto-port-config={ mode=dsp monitor=true position=preserve } }" >/dev/null
done
display=":$((90 + $$ % 9))"
"${XVFB:-Xvfb}" "$display" -screen 0 1600x1000x24 -nolisten tcp >/dev/null 2>&1 & pids="$pids $!"
sleep 0.5
log="$dir/app.log"
env DISPLAY="$display" HOME="$home" GDK_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1 \
  XDG_CONFIG_HOME="$home/.config" XDG_DATA_HOME="$home/.local/share" XDG_CACHE_HOME="$home/.cache" \
  ${plugins:+CLAP_PATH="$plugins/clap" VST3_PATH="$plugins/vst3"} \
  "$app" >"$log" 2>&1 & appid=$!; pids="$appid $pids"
ok=""
i=0; while [ $i -lt 600 ]; do
  if grep -q "BleepLoop v" "$log" 2>/dev/null && grep -q "\[scan\]" "$log"; then ok=1; break; fi
  kill -0 "$appid" 2>/dev/null || break
  i=$((i+1)); sleep 0.1
done
sleep 3
import -display "$display" -window root "$shot" || true
grep -E "BleepLoop v|\[scan\]|plugin|JACK|jack|ERROR|panic" "$log" | head -20
echo "log: $log"
[ -n "$ok" ] && kill -0 "$appid" 2>/dev/null && echo "PASS: the AppImage started, ran and was closed" || { echo "FAIL"; exit 1; }
