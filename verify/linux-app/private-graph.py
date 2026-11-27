"""Run the JACK device integration test on a fresh PipeWire graph, without a session manager."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def main():
    root = Path(__file__).resolve().parents[2]
    name = "private_jack_quantum_and_routes"
    with tempfile.TemporaryDirectory(prefix="bleeploop-test-graph-") as runtime:
        env = dict(os.environ, PIPEWIRE_RUNTIME_DIR=runtime, PIPEWIRE_REMOTE="pipewire-0",
                   XDG_RUNTIME_DIR=runtime, XDG_CONFIG_HOME=runtime, BLEEPLOOP_TEST_GRAPH=runtime,
                   PIPEWIRE_CONFIG_DIR="/usr/share/pipewire", DISABLE_RTKIT="1")
        for key in ("PIPEWIRE_QUANTUM", "PIPEWIRE_LATENCY", "PIPEWIRE_RATE", "PIPEWIRE_PROPS", "PIPEWIRE_CONFIG_NAME"):
            env.pop(key, None)
        server = subprocess.Popen(["pipewire"], env=env, stdout=subprocess.DEVNULL)
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError("Private PipeWire server exited")
                if (Path(runtime) / "pipewire-0").is_socket():
                    break
                time.sleep(0.025)
            else:
                raise RuntimeError("Private PipeWire server did not open its socket")
            # Two sinks (playback_* ports and their monitor_*) and a source (capture_* ports): only
            # the source is an input endpoint. pipewire-jack names a null node of class
            # Audio/Source/Virtual's outputs capture_*; plain Audio/Source would give it sink ports.
            for fixture, media in (("fixture_in", "Audio/Sink"), ("fixture_out", "Audio/Sink"),
                                   ("fixture_src", "Audio/Source/Virtual")):
                props = (f"{{ factory.name=support.null-audio-sink node.name={fixture} node.description={fixture} "
                         f"media.class={media} object.linger=true audio.position=[ FL FR ] "
                         "adapter.auto-port-config={ mode=dsp monitor=true position=preserve } }")
                subprocess.run(["pw-cli", "create-node", "adapter", props], env=env, check=True, timeout=5)
            # Pass a prebuilt lib test binary for fast repeats. Otherwise use the workspace toolchain.
            cmd = sys.argv[1:] or ["cargo", "test", "--locked", "-p", "app", "--lib", "--"]
            result = subprocess.run(["pw-jack", *cmd, name, "--ignored", "--nocapture"], env=env,
                                    cwd=root / "src-tauri", text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=30 if sys.argv[1:] else 1200)
            print(result.stdout, end="")
            result.check_returncode()
            if "1 passed; 0 failed; 0 ignored;" not in result.stdout:
                raise RuntimeError("The private JACK integration test did not run")
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
