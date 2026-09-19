//! P9 — native CLAP host IPC surface.
//!
//! This module is the Rust side of the `PluginHost` capability boundary (`src/platform/host.ts`).
//! P9.1 added an out-of-process scan; P9.2 loads/activates/processes a CLAP plugin on a device-less
//! RT thread; P9.3 routes that plugin's mono PCM cross-process into the WebView2 renderer via a
//! WebView2 SharedBuffer (the hop-1 ring) so the Web Audio graph can hear + record it.
//!
//! Contract notes mirrored from `host.ts`:
//!   - `slot` is 0 | 1 (two native slots); we take it as `u8` and validate.
//!   - `loadPlugin` REQUIRES `id`: one `.clap` bundle can export several descriptors, so
//!     `(slot, path)` alone would silently load `descriptor[0]`.
//!   - state is opaque plugin-defined bytes (CLAP `state` ext); JS sees a Uint8Array.
//!   - every command returns `Result<_, String>` so a stub/error surfaces as a rejected JS promise
//!     rather than a panic across the IPC boundary.
//!
//! Mechanically split out of the original single-file `plugin_host.rs` (6445 lines, 87% of the
//! crate) into this directory.
//! Zero logic change: `state.rs` (shared IPC-mirrored types + `PluginHostState`), `commands.rs`
//! (the `#[tauri::command]` surface), `scan.rs` (out-of-process plugin scan), `rt_alloc.rs` (DEV
//! global-allocator shim), `editor_window.rs` (format-agnostic host editor window), `transport.rs`
//! (format-agnostic RT pipes shared by CLAP + VST3), `clap.rs` (the CLAP owner/producer/control
//! plane, plus the VST3 second format as its `vst3.rs` child module).

#[cfg(windows)]
mod clap;
mod commands;
#[cfg(windows)]
mod editor_window;
#[cfg(windows)]
mod native_io;
#[cfg(debug_assertions)]
mod rt_alloc;
#[cfg(windows)]
mod scan;
mod state;
#[cfg(windows)]
mod transport;

pub use commands::*;
pub use state::PluginHostState;
#[cfg(all(windows, debug_assertions))]
pub(crate) use clap::marker_probe_target;

#[cfg(debug_assertions)]
#[global_allocator]
static GLOBAL_ALLOC: rt_alloc::Shim = rt_alloc::Shim;
