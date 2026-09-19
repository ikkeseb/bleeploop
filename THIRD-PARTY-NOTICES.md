# Third-Party Notices

BleepLoop is MIT-licensed (see `LICENSE`). It depends on the third-party packages and crates
listed below. Every license string here was read directly from the locally installed package —
`node_modules/<pkg>/package.json` for JS dependencies, and the crate's own `Cargo.toml` under
`~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/` for Rust dependencies — not from
memory or upstream docs. Versions are the ones actually resolved in this repo
(`package.json`/`pnpm-lock.yaml`, `src-tauri/Cargo.lock`) at the time of writing.

## JavaScript / frontend dependencies

| Package | Version | License | Notes |
|---|---|---|---|
| [solid-js](https://github.com/solidjs/solid) | 1.9.13 | MIT | UI reactivity runtime |
| [tone](https://github.com/Tonejs/Tone.js) | 15.1.22 | MIT | Web Audio synth/transport layer |
| [ringbuf.js](https://github.com/padenot/ringbuf.js) | 0.4.0 | **MPL-2.0** | see below |
| [@tauri-apps/api](https://github.com/tauri-apps/tauri) | 2.11.0 | Apache-2.0 OR MIT | Tauri JS bindings |

### ringbuf.js — MPL-2.0 (file-level copyleft)

`ringbuf.js` is licensed under the Mozilla Public License 2.0. MPL-2.0 is a *file-level*
copyleft: it requires that modifications to MPL-covered files be made available under MPL-2.0,
but does not extend that requirement to the rest of the codebase that merely uses the library.
BleepLoop vendors/imports `ringbuf.js` **unmodified** as a dependency; no MPL-covered source in
this repo has been altered. Upstream source is available at
<https://github.com/padenot/ringbuf.js>. The licence text is `licenses/MPL-2.0.txt`; the installer
puts it, and this file, beside the exe.

## Rust / native dependencies (`src-tauri/`)

All versions below are as pinned/resolved in `src-tauri/Cargo.lock`.

| Crate | Version | License (as declared in the crate's own `Cargo.toml`) |
|---|---|---|
| tauri | 2.11.2 | Apache-2.0 OR MIT |
| tauri-plugin-log | 2.8.0 | Apache-2.0 OR MIT |
| serde | 1.0.228 | MIT OR Apache-2.0 |
| serde_json | 1.0.150 | MIT OR Apache-2.0 |
| log | 0.4.32 | MIT OR Apache-2.0 |
| clack-host | 0.1.0 | MIT OR Apache-2.0 |
| clack-extensions | 0.1.0 | MIT OR Apache-2.0 |
| cpal | 0.18.1 | Apache-2.0 (single license, not dual) |
| rtrb | 0.3.4 | MIT OR Apache-2.0 |
| rubato | 3.0.0 | MIT |
| walkdir | 2.5.0 | `Unlicense/MIT` (crate's own non-SPDX-normalized string; effectively dual Unlicense-or-MIT) |
| vst3 | 0.3.0 | MIT OR Apache-2.0 — see the separate VST3-hosting note below |
| webview2-com | 0.38.2 | MIT |
| windows | 0.61.3 | MIT OR Apache-2.0 |
| windows-core | 0.61.2 | MIT OR Apache-2.0 |

### Steinberg ASIO SDK — not included; ASIO-enabled binaries are GPLv3

The `asio` Cargo feature (`--features asio`, gating `cpal/asio`) compiles against Steinberg's ASIO
SDK. **The SDK is not included in this repository and is not redistributed by this project.** A
local build supplies it through `CPAL_ASIO_DIR`; the `build-exe` workflow downloads a pinned
version from Steinberg at build time. A plain `cargo build` needs no SDK and produces the WASAPI
tier under this repo's MIT licence.

The SDK's own `LICENSE.txt` (2.3.4, 2025-10-15) reads: "This Software Development Kit is licensed
under the terms of the Steinberg ASIO License, or alternatively under the terms of the General
Public License (GPL) Version 3." This project takes the GPLv3 option: **any binary built with
`--features asio`, including the exe from `build-exe`, is a GPLv3 work as a whole**; the source in
this repository stays MIT, which is GPL-compatible. The SDK does not itself address an MIT source
tree feeding a GPLv3 binary.

ASIO is a registered trademark of Steinberg Media Technologies GmbH. Steinberg makes the name and
logo optional under GPLv3, but any use must follow the Usage Guidelines shipped in the SDK. The app
says "ASIO", so it shows the unaltered ASIO Compatible Logo in Help ("About this build"), and the
README and the release notes carry it too. The artwork is Steinberg's and is not covered by this repo's MIT licence
(`src/assets/third-party/NOTICE.md`). The GPLv3 text is `licenses/GPL-3.0.txt`.

### VST3 hosting

Steinberg's VST 3 SDK is MIT-licensed since version 3.8
(<https://steinbergmedia.github.io/vst3_dev_portal/pages/VST+3+Licensing/Index.html>); using the
"VST" name or logo is optional and, if used, subject to Steinberg's trademark rules. BleepLoop
hosts VST3 through the `vst3` crate (coupler-rs, MIT OR Apache-2.0), which since 0.3.0 ships
pre-generated bindings and needs no SDK at build time. Which SDK version those bindings were
generated from is not stated by the crate — unknown here. BleepLoop uses no VST logo.

## Explicitly not covered here

`@soundtouchjs/audio-worklet` is not listed: it has no references in `src/` (dead dependency,
removed) and is not shipped.
