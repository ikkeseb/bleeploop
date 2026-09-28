//! P9 — native CLAP/VST3 host IPC surface.
//!
//! This module is the Rust side of the `PluginHost` capability boundary (`src/platform/host.ts`).
//! P9.1 added an out-of-process scan; the plugins themselves run as units inside the native
//! engine's callback, each on its own owner thread (`engine_slot.rs`, routed from
//! `engine_io/plugins.rs`).
//!
//! Contract notes mirrored from `host.ts`:
//!   - `slot` is 0 | 1 (two native slots); we take it as `u8` and validate.
//!   - `loadPlugin` REQUIRES `id`: one `.clap` bundle can export several descriptors, so
//!     `(slot, path)` alone would silently load `descriptor[0]`.
//!   - a tone is a tone file's bytes (`tone.rs`), raw both ways: JS sees an ArrayBuffer and sends a
//!     Uint8Array.
//!   - every command returns `Result<_, String>` so a stub/error surfaces as a rejected JS promise
//!     rather than a panic across the IPC boundary.
//!
//! Split out of the original single-file `plugin_host.rs`: `state.rs` (shared IPC-mirrored types +
//! `PluginHostState`), `commands.rs` (the `#[tauri::command]` surface), `scan.rs` (out-of-process
//! plugin scan), `rt_alloc.rs` (DEV global-allocator shim), `editor_window.rs` (format-agnostic host
//! editor window), `clap.rs` (the shared CLAP host plumbing, plus the VST3 second format as its
//! `vst3.rs` child module and the engine slots).

#[cfg(windows)]
mod clap;
mod commands;
#[cfg(windows)]
mod editor_window;
#[cfg(debug_assertions)]
pub(crate) mod rt_alloc;
#[cfg(windows)]
mod scan;
mod state;
#[cfg(windows)]
pub(crate) mod tone;

pub use commands::*;
pub use state::PluginHostState;
pub(crate) use state::{ParamDesc, PluginDescriptor, PluginInfo, ToneImport};
#[cfg(all(windows, debug_assertions))]
pub(crate) use clap::engine_spike_run;
#[cfg(all(windows, debug_assertions))]
pub(crate) use scan::scan_one;
#[cfg(windows)]
#[allow(unused_imports)] // the native engine's plugin slots (the `plugin_*` commands)
pub(crate) use clap::engine_slot;

#[cfg(debug_assertions)]
#[global_allocator]
static GLOBAL_ALLOC: rt_alloc::Shim = rt_alloc::Shim;
