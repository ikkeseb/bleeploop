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
//! | [`effects`] | each lane's FX chain, the shared reverb bus, their grid, CLEAR and COPY on a lane's FX | `fx/fx.ts`, `looper/{playback,machine}.ts` |
//! | [`instruments`] | the six built-in instruments, the selected one, the wheels, their record path | `synths/index.ts`, `input-router.ts` |
//! | [`api`] | commands, events, the process context, the plugin seam | — |
//! | [`dsp`] | Stage 3 sound: the Tone/Blink building blocks, the six built-in synths, the per-track FX chain and the reverb bus, the limiter | Tone.js on Blink's Web Audio |
//!
//! # Rules
//!
//! - **One clock: the device frame.** Input frame `x` is captured at frame `x`; a lane plays loop
//!   position `(f - anchor) mod master` at frame `f`. A take starts `align_frames` (+ the plugin's
//!   latency and the master limiter's pre-delay) after its downbeat; a built-in instrument's record path
//!   lags it by the input side, so its notes land there too. There is no user-facing record trim. The
//!   synths, FX and reverb run on the device frame less the frames the device skipped, so their blocks
//!   follow each other ([`effects`]); the limiter stays on the device frame.
//! - **Every state change lands on its exact frame.** `process` splits a block wherever a command, a
//!   scheduled looper event, a beat, an AUTO trigger or a render quantum's end falls, so the same
//!   commands render bit-identical output at any block size (the golden jam, the gesture property tests
//!   and `tests/sound.rs` assert it). A note sounds one quantum after its frame ([`instruments::LEAD`]);
//!   a wheel or an FX change sounds from the next 128-frame quantum boundary, as a live Web Audio call
//!   with no look-ahead does.
//! - **`process` never allocates, locks or waits.** Buffers are allocated (and their pages touched) in
//!   `Engine::new`; commands and events cross on rtrb rings; a full event ring drops and counts.
//! - **No loop-sized work in one callback.** Tiling, the undo copy, a discarded layer's restore and
//!   COPY are block jobs of `looper::JOB_RATE` positions per rendered frame, started where a read or
//!   write head touches next so they stay ahead of it. A command that needs a lane's job finished waits
//!   for it (on an exact frame), and every command sent after it waits behind it, except the
//!   instruments' (a note never waits on the looper). A full command table leaves the rest in the ring
//!   for the next block: late, never dropped.
//! - **A command is judged when it is pressed**: one that would do nothing then is dropped, never held.
//!
//! # Tests
//!
//! `cargo test -p lf-engine` (Linux and Windows CI). `tests/` ports the Web Audio rig guards, one file per
//! group, each header naming the guard it ports and what it drops; `tests/common` is the rig, and every
//! `process` call there runs under `assert_no_alloc`. The looper's tests read [`Taps::looper`], the lanes
//! before their FX (a bypassed chain is not bit-transparent, as in Tone); `tests/sound.rs` holds the
//! wired sound. `tests/perf.rs` holds the ignored cost bars (Stage 2 and 3) and the Stage 3 load's alloc
//! check.
//!
//! # Not built yet
//!
//! The device owner and plugin slots (Stage 4; a plugin slot taking the notes is `SelectInstrument(None)`
//! until then); waveform peaks and the export snapshot (with the feed, Stage 5).

#![forbid(unsafe_code)]

pub mod api;
pub mod autorec;
pub mod clock;
pub mod dsp;
pub mod effects;
pub mod engine;
pub mod grid;
pub mod instruments;
pub mod looper;

pub use api::*;
pub use engine::{Diag, Engine, EngineConfig, EngineHandle, Taps};
