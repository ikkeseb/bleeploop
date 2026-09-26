//! lf-engine: BleepLoop's native audio engine, a pure crate (docs/plans/native-engine.md § Stage 2).
//! This doc is the crate's briefing.
//!
//! The app runs it in engine mode, the default (`src-tauri/src/engine_io`); the web audio path stays
//! behind the toggle until the plan's Stage 6 deletes it. No host, device or plugin crate may enter its
//! dependency tree (`scripts/engine-deny.mjs`).
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | [`grid`] | frames per bar, whole-bar clamps, commit and stop plans, a later take's bars (the multiply), the beat [`grid::Grid`], take fill | `quantize.ts`, `looper/grid-math.ts` |
//! | [`clock`] | tempo and its lock, the beat pulse (free-run, count-in, master), the click | `clock.ts` |
//! | [`looper`] | lanes and buffers, the recorder, every transition, the multiply, gates and refusals, block jobs | `looper/{machine,state,capture,playback,mixer}.ts`, `ui/looper/gates.ts` |
//! | [`autorec`] | the AUTO REC onset detector | `looper/auto-record.ts` |
//! | [`engine`] | the callback: rings, the block split, the bus topology, master volume | `engine.ts`, `master.ts` |
//! | [`effects`] | each lane's FX chain, the shared reverb bus, their grid, CLEAR and COPY on a lane's FX | `fx/fx.ts`, `looper/{playback,machine}.ts` |
//! | [`input_fx`] | the input sends: ECHO and REVERB on the wet signal, wet only, into the record tap and the monitor | — (engine only) |
//! | [`instruments`] | the six built-in instruments, the selected one, the wheels, their record path | `synths/index.ts`, `input-router.ts` |
//! | [`overview`] | what the UI draws, for a reader off the audio thread: the grid anchor, each lane's buffer, orientation and frames, each buffer's waveform peaks | `looper/peaks.ts` |
//! | [`session`] | saving and loading a session: a snapshot copied out a budget per frame, a load swapped into an empty looper, the host's port | `looper/session.ts`, `export/*` |
//! | [`slots`] | the two plugin slots: install and removal through their ports, bypass crossfades, notes, live and gain, where each output goes | `plugin-bridge.ts`, `instrument-slots.ts` |
//! | [`api`] | commands, events, the process context, the plugin seam ([`SlotProcessor`]) | — |
//! | [`dsp`] | Stage 3 sound: the Tone/Blink building blocks, the six built-in synths, the per-track FX chain and the reverb bus, the limiter | Tone.js on Blink's Web Audio |
//!
//! # Rules
//!
//! - **Every committed lane is one master long.** A later take longer than the master (FIXED past the
//!   loop) multiplies it: the take becomes the new master, whole old loops long, and the other loops
//!   tile out to it with the grid, its beats and every lane's phase unchanged (`looper::Looper`'s
//!   multiply, F14).
//! - **One clock: the device frame.** Input frame `x` is captured at frame `x`; a lane plays loop
//!   position `(f - anchor) mod master` at frame `f`. A take starts `align_frames` (+ the live effect
//!   slot's latency, from the block after its live flag changes, and the master limiter's pre-delay)
//!   after its downbeat; an instrument's record path lags it by the input side (a plugin instrument's,
//!   less its own latency), so its notes land there too. The input sends are wet only and add nothing
//!   to the alignment. There is no user-facing record trim. The synths, FX, input sends and reverbs run
//!   on the device frame less the frames the device skipped, so their blocks follow each other
//!   ([`effects`]); the limiter stays on the device frame.
//! - **Every state change lands on its exact frame.** `process` splits a block wherever a command, a
//!   scheduled looper event, a beat, an AUTO trigger or a render quantum's end falls, so the same
//!   commands render bit-identical output at any block size (the golden jam, the gesture property tests,
//!   `tests/sound.rs` and `tests/slots.rs` assert it). A note sounds one quantum after its frame ([`instruments::LEAD`]);
//!   a wheel or an FX change (a lane's or an input send's) sounds from the next 128-frame quantum
//!   boundary, as a live Web Audio call with no look-ahead does.
//! - **`process` never allocates, locks or waits.** Buffers are allocated (and their pages touched) in
//!   `Engine::new`; commands and events cross on rtrb rings; a full event ring drops and counts.
//! - **No loop-sized work in one callback.** Tiling, the undo copy, a discarded layer's restore, COPY
//!   and a multiply's extension of the other loops are block jobs of `looper::JOB_RATE` positions per
//!   rendered frame, started where a read or write head touches next so they stay ahead of it (a lane a
//!   multiply extends reads through its old loop instead until the extension is done). A command that
//!   needs a lane's job finished waits for it (on an exact frame), and every command sent after it waits
//!   behind it, except the instruments', the plugin slots' and the input sends' (a note never waits on
//!   the looper). A full command table leaves the rest in the ring for the next block: late, never
//!   dropped.
//! - **A command is judged when it is pressed**: one that would do nothing then is dropped, never held.
//! - **The engine never drops a plugin unit** (a drop frees memory and calls into the plugin's DLL).
//!   Units enter and leave through their slot's [`SlotPort`], at a block start (at once while no device
//!   runs: [`Engine::service_slots_idle`]); a removal releases the slot's notes,
//!   crossfades to bypass, stops the unit once and hands it back, and an eviction hands back every unit,
//!   a waiting install included. The slots render ahead to the next slot command, so a plugin sees one
//!   call per block unless a stamped note, target, live flag or gain splits it there.
//! - **A stopped device is a punch-out** (STATUS E3, [`Engine::punch_out`]): what records ends after the
//!   last rendered frame and commits as usual (a RETAKE roll with a kept pass commits that pass), what
//!   has retained nothing is cancelled, and rendering resumes on the next frame with loop-end stops and
//!   block jobs where they were.
//!
//! # Tests
//!
//! `cargo test -p lf-engine` (Linux and Windows CI). `tests/` ports the Web Audio rig guards, one file per
//! group, each header naming the guard it ports and what it drops; `tests/common` is the rig, and every
//! `process` call there runs under `assert_no_alloc`. The looper's tests read [`Taps::looper`], the lanes
//! before their FX (a bypassed chain is not bit-transparent, as in Tone); `tests/sound.rs` holds the
//! wired sound. `tests/slots.rs` holds the plugin slots, with fake units that record what they saw in
//! preallocated buffers (never allocating in `process`) and are handed back to the test to drop;
//! `tests/punch_out.rs` holds the punch-out, `tests/multiply.rs` the multiply, `tests/input_fx.rs` the
//! input sends. `tests/perf.rs` holds the ignored cost bars (Stage 2 and 3, the input sends, a multiply's
//! burst) and the Stage 3 load's alloc check.
//!
//! # Beside this crate, and not built yet
//!
//! The device side (streams, MIDI, Share output) is `src-tauri/src/engine_io`; the CLAP/VST3 units and
//! their engine-mode owners are `src-tauri/src/host/engine_slot.rs` and its siblings; the feed that
//! carries the events and the [`overview`] to the UI is `src-tauri/src/engine_io/feed.rs`. Not built:
//! nothing of Stage 5.

#![forbid(unsafe_code)]

pub mod api;
pub mod autorec;
pub mod clock;
pub mod dsp;
pub mod effects;
pub mod engine;
pub mod grid;
pub mod input_fx;
pub mod instruments;
pub mod looper;
pub mod overview;
pub mod session;
pub mod slots;

pub use api::*;
pub use engine::{Diag, Engine, EngineConfig, EngineHandle, Taps};
pub use overview::{LaneView, Overview};
pub use session::{Load, LoadTrack, SessionError, SessionJob, SessionPort, Snapshot, SnapshotTrack};
pub use slots::SlotPort;
