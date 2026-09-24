//! lf-engine: BleepLoop's native audio engine, a pure crate (docs/plans/native-engine.md § Stage 2).
//! This doc is the crate's briefing.
//!
//! Dormant until the plan's Stage 6 flip: nothing in the app calls it yet, and the live line keeps the
//! Web Audio looper. No host, device or plugin crate may enter its dependency tree
//! (`scripts/engine-deny.mjs`).
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | [`grid`] | frames per bar, whole-bar clamps, commit and stop plans, the beat [`grid::Grid`], take fill | `quantize.ts`, `looper/grid-math.ts` |
//! | [`clock`] | tempo and its lock, the beat pulse (free-run, count-in, master), the click | `clock.ts` |
//! | [`looper`] | lanes and buffers, the recorder, every transition, gates and refusals, block jobs | `looper/{machine,state,capture,playback,mixer}.ts`, `ui/looper/gates.ts` |
//! | [`autorec`] | the AUTO REC onset detector | `looper/auto-record.ts` |
//! | [`engine`] | the callback: rings, the block split, the bus topology, master volume | `engine.ts`, `master.ts` |
//! | [`api`] | commands, events, the process context, the plugin seam | — |
//!
//! # Rules
//!
//! - **One clock: the device frame.** Input frame `x` is captured at frame `x`; a lane plays loop
//!   position `(f - anchor) mod master` at frame `f`. A take starts `align_frames` (+ the plugin's
//!   latency) after its downbeat. There is no user-facing record trim.
//! - **Every state change lands on its exact frame.** `process` splits a block wherever a command, a
//!   scheduled looper event, a beat or an AUTO trigger falls, so the same commands render bit-identical
//!   output at any block size (the golden jam and the gesture property tests assert it).
//! - **`process` never allocates, locks or waits.** Buffers are allocated (and their pages touched) in
//!   `Engine::new`; commands and events cross on rtrb rings; a full event ring drops and counts.
//! - **No loop-sized work in one callback.** Tiling, the undo copy, a discarded layer's restore and
//!   COPY are block jobs of `looper::JOB_RATE` positions per rendered frame, started where a read or
//!   write head touches next so they stay ahead of it. A command that needs a lane's job finished waits
//!   for it (on an exact frame), and every command sent after it waits behind it.
//! - **A command is judged when it is pressed**: one that would do nothing then is dropped, never held.
//!
//! # Tests
//!
//! `cargo test -p lf-engine` (Linux and Windows CI). `tests/` ports the Web Audio rig guards, one file per
//! group, each header naming the guard it ports and what it drops; `tests/common` is the rig, and every
//! `process` call there runs under `assert_no_alloc`. `tests/perf.rs` is the ignored cost measurement.
//!
//! # Not built yet
//!
//! The limiter, synths and FX (plan Stage 3), the device owner and plugin slots (Stage 4), waveform
//! peaks and the export snapshot (with the feed, Stage 5).

#![forbid(unsafe_code)]

pub mod api;
pub mod autorec;
pub mod clock;
pub mod dsp;
pub mod engine;
pub mod grid;
pub mod looper;

pub use api::*;
pub use engine::{Diag, Engine, EngineConfig, EngineHandle};
