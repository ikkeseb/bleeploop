# BleepLoop

[![CI](https://github.com/ikkeseb/bleeploop/actions/workflows/ci.yml/badge.svg)](https://github.com/ikkeseb/bleeploop/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D6.svg)](#getting-started)

BleepLoop is a Windows desktop instrument. It hosts two native CLAP/VST3 plugins and six
built-in Web Audio synths, and records them into a 5-track looper modelled on the RC-505 MK II.
You play a MIDI controller or a guitar through a plugin or synth, then loop and overdub at low
latency. The computer keyboard works as a fallback.

> Pre-release and Windows-only. There are no binary releases yet, so you build it from source
> (see below). The low-latency ASIO® tier is an opt-in build feature that needs Steinberg's SDK,
> see [Third-party notices](#license-and-third-party-notices).

![BleepLoop](docs/media/bleeploop.png)

## Features

- 5-track looper with overdub and one-level undo, per-track reverse, mute and volume, a one-bar
  record count-in and fixed-length record. The metronome is phase-locked to the loop grid, so the
  click and the loops cannot drift apart.
- Per-track FX: filter, pitch shift, tempo-synced stutter, feedback delay and a shared reverb send.
  Each one bypasses without a click.
- Six built-in Web Audio synths, one of them a 16-voice GM drum kit.
- Two native CLAP/VST3 plugin slots with floating plugin editors.
- Guitar or line input with native low-latency monitoring over WASAPI or ASIO. Record-latency
  compensation puts the take on the grid.
- MIDI controllers work through WebView2's native Web MIDI. Without one, the computer keyboard
  plays notes and runs the transport (1-5 to arm or select tracks, Space or Enter to play and stop).
- Session export and import as one `.zip`: a WAV stem per track, a wet stereo master render and a
  `session.json`.

## Getting started

The frontend runs on its own in a browser with no native dependencies. Plugin hosting and native
audio I/O come from the Tauri shell around it.

### Web frontend only

- Node >= 22.18, pnpm 10

```bash
git clone https://github.com/ikkeseb/bleeploop.git
cd bleeploop
pnpm install
pnpm dev          # http://localhost:1420
```

### Full native app

- Everything above, plus:
- Rust (pinned via `rust-toolchain.toml`), MSVC Build Tools 2022, WebView2

```bash
pnpm dev:wasapi                      # full app, WASAPI, no extra SDK needed
pnpm exec tauri build --no-bundle    # standalone WASAPI exe in src-tauri/target/release/app.exe
```

### ASIO low-latency tier (optional)

Set `LIBCLANG_PATH` (cpal's `asio-sys` runs bindgen) and point `CPAL_ASIO_DIR` at your own copy of
the Steinberg ASIO SDK. The SDK is not in this repo. A binary built with it is GPLv3, see
[Third-party notices](#license-and-third-party-notices).

```bash
pnpm dev:asio     # full app, ASIO + native sample rate
```

## Commands

| Command | What it does |
|---|---|
| `pnpm dev` | Vite dev server, frontend only |
| `pnpm dev:asio` | Full app, ASIO low-latency + native sample rate |
| `pnpm dev:wasapi` | Full app, WASAPI (no ASIO SDK needed) |
| `pnpm build` | `tsc --noEmit && vite build`, which the Tauri bundle depends on |
| `pnpm build:app` | Standalone release exe with ASIO (`tauri build --no-bundle --features asio`) |
| `pnpm check` | Typecheck, oxlint, the capability-boundary check and the `verify/` guards. Also the pre-push hook |
| `pnpm verify` | Deterministic guards for the audio core's pure logic, no browser or hardware |
| `pnpm verify:jam` | The golden jam. Drives the real app in a headless browser and checks the recorded grid frame by frame (~40 s, not part of `pnpm check`) |

The `build-exe` workflow builds the ASIO installer and exe on a clean Windows runner and keeps them
as a run artifact for 30 days. Run it from the Actions tab. A `v*` tag also stages a draft GitHub
Release with the installer, its sha256 and the licence texts. That binary is GPLv3, see the
third-party notices.

## Architecture

Hosting native plugins is the only thing that needs Tauri and Rust. Synths, looper, FX, MIDI,
keyboard and waveforms are Web Audio and TypeScript, and run in a plain browser. `src/platform/` is
the only directory allowed to import `@tauri-apps/*`, and `pnpm check:boundary` enforces that.
Plugin audio enters the Web Audio graph as an `AudioNode`, never as PCM passed across that
boundary. One shared `AudioContext` times everything, so tempo and quantization have a single clock.

Full design decisions and invariants: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
Live project state and open threads: [`STATUS.md`](STATUS.md).

## Verification

There is no unit-test runner. Two layers cover the audio core:

- `pnpm verify` (~1 s, part of `pnpm check`) checks the pure logic: looper frame math, latency
  compensation and clock math, with no browser, `AudioContext` or hardware. Some guards import the
  real source. Most are hand-ported copies, and `fs-mirror-drift-verify.mjs` re-hashes the source
  range each one copies, so a copy that falls behind fails the run.
- `pnpm verify:jam` (~40 s, outside `pnpm check`) is the golden jam. It drives the real app in a
  headless browser, records impulses on the beat grid and checks the committed loop frame by frame.
  Focused browser probes also cover input ownership, session round trips, recovery failures and
  plugin lifecycle transitions; see [`verify/README.md`](verify/README.md). The pure-logic guards
  never reach the running audio graph.

Feel, the native half and real rig latency are verified by running the app and measuring it, not by
reading code or trusting a typecheck. [`docs/VERIFY.md`](docs/VERIFY.md) explains how: the browser
harness, the `window.__lf` debug hook, audio measurement and the `tauri dev` routine on the PC.

## License and third-party notices

The source is MIT, see [`LICENSE`](LICENSE). A binary built with `--features asio` links the
Steinberg ASIO SDK and is GPLv3 as a whole. [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)
lists the dependency licences, read from the installed packages and crates, and `licenses/` holds
the GPLv3 and MPL-2.0 texts that ship with the installer.

<img src="src/assets/third-party/ASIO-compatible-logo-Steinberg-TM-BW.jpg" alt="ASIO Compatible" width="72" />

ASIO is a registered trademark of Steinberg Media Technologies GmbH.
