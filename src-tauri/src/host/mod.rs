//! P9 — native CLAP/VST3/VST2 host IPC surface.
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
//! plugin scan), `folders.rs` (the player's own scan folders), `rt_alloc.rs` (DEV global-allocator shim), `editor_window.rs` (format-agnostic host
//! editor window), `clap.rs` (the shared CLAP host plumbing, plus the VST3 second format as its
//! `vst3.rs` child module and the engine slots, `vst2_engine.rs` among them), `vst2_abi.rs` +
//! `vst2.rs` (the VST2 binary interface, module loader and host callback) and `pe.rs` (which `.dll` is a VST2 plugin, read
//! from its headers).

// Linux builds the Windows host's CLAP and VST3 half without VST2, chains or the editor window: the
// items only those reach are dead there, and the Windows build keeps linting them.
#![cfg_attr(target_os = "linux", allow(dead_code, unused_imports))]

// The CLAP and VST3 hosts run on Windows and Linux (clack and the `vst3` crate are portable); VST2,
// its PE pre-filter and the Win32 editor window are Windows-only. Linux keeps the editor window's
// names in a stand-in (`editor_window_linux.rs`).
#[cfg(any(windows, target_os = "linux"))]
mod clap;
mod commands;
#[cfg(windows)]
mod editor_window;
#[cfg(target_os = "linux")]
#[path = "editor_window_linux.rs"]
mod editor_window;
#[cfg(any(windows, target_os = "linux"))]
mod folders;
#[cfg(windows)]
mod pe;
#[cfg(debug_assertions)]
pub(crate) mod rt_alloc;
#[cfg(any(windows, target_os = "linux"))]
mod scan;
mod state;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) mod tone;
#[cfg(windows)]
mod vst2;
// The interface as the format defines it, with the opcodes this host does not send yet.
#[cfg(windows)]
#[allow(dead_code)]
mod vst2_abi;

pub use commands::*;
pub use state::PluginHostState;
pub(crate) use state::{ParamDesc, PluginDescriptor, PluginInfo, ToneImport};
#[cfg(all(windows, debug_assertions))]
pub(crate) use clap::engine_spike_run;
#[cfg(all(windows, debug_assertions))]
pub(crate) use scan::scan_one;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) use scan::scan_paths;
#[cfg(any(windows, target_os = "linux"))]
#[allow(unused_imports)] // the native engine's plugin slots (the `plugin_*` commands)
pub(crate) use clap::engine_slot;
#[cfg(windows)]
#[allow(unused_imports)] // chain unit owners (E2 P4)
pub(crate) use clap::unit_owner;

#[cfg(debug_assertions)]
#[global_allocator]
static GLOBAL_ALLOC: rt_alloc::Shim = rt_alloc::Shim;
