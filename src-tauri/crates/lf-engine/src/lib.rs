//! lf-engine: BleepLoop's native audio engine, a pure crate: one owner of every piece of musical state
//! (click, looper, synths, FX, mixer, limiter, the plugin slots), clocked by the audio device. This doc
//! is the crate's briefing.
//!
//! The app always runs it (`src-tauri/src/engine_io`). No host, device or plugin crate may enter its
//! dependency tree: `scripts/engine-deny.mjs` fails the tree on the tauri and clack families, cpal,
//! windows, windows-core, clap-sys and vst3 (`pnpm rust:check` runs it, and `ci.yml`'s `engine` job with
//! `cargo test -p lf-engine` on Linux). Its dependencies are `rtrb` and `rustfft` (the convolution
//! reverb); dev-only `assert_no_alloc`, `proptest` (without its fork feature), and `serde`,
//! `serde_json` and `sha2` for the reference fixtures. It builds as
//! one codegen unit (`src-tauri/Cargo.toml` says why).
//!
//! The "Ported from" column and the modules' headers name the Web Audio TypeScript each port copied by
//! its path under `src/audio` (`engine.ts`, `looper/machine.ts`), deleted in Stage 6; git history keeps
//! it (`git log --diff-filter=D -- 'src/audio/*'`). A path from the repo root (`src/ui/…`) is a file
//! that runs today.
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | [`grid`] | frames per bar, whole-bar clamps, commit and stop plans, a later take's bars (the multiply), the beat [`grid::Grid`], take fill | `quantize.ts`, `looper/grid-math.ts` |
//! | [`clock`] | tempo and its lock, the beat pulse (free-run, count-in, master), the click, the bar line a FADE ends on | `clock.ts` |
//! | [`looper`] | lanes and buffers, the recorder, every transition, the multiply, TRIM, DUB FEEDBACK, FADE, gates and refusals, block jobs | `looper/{machine,state,capture,playback,mixer}.ts`, `ui/looper/gates.ts` |
//! | [`autorec`] | the AUTO REC onset detector | `looper/auto-record.ts` |
//! | [`engine`] | the callback: rings, the block split, the bus topology, master volume | `engine.ts`, `master.ts` |
//! | [`effects`] | each lane's FX chain and its pan, the shared reverb bus, their grid, CLEAR and COPY on a lane's FX and pan | `fx/fx.ts`, `looper/{playback,machine}.ts` (the pan: the engine's own) |
//! | [`input_fx`] | the input sends: ECHO, REVERB and RING MOD on the wet signal, wet only, into the record tap and the monitor | — (engine only) |
//! | [`instruments`] | the six built-in instruments, the selected one (or none), each one's level, the wheels, their record path | `synths/index.ts`, `input-router.ts` |
//! | [`overview`] | what the UI draws, for a reader off the audio thread: the grid anchor, each lane's state, buffer, orientation and frames, each buffer's waveform peaks | `looper/peaks.ts` |
//! | [`session`] | saving and loading a session: a snapshot copied out a budget per frame, each lane with its mix at the pin, a load swapped into an empty looper with each lane's mix, both after the commands due on their block's first frame, the host's port | `looper/session.ts`, `export/*` |
//! | [`render`] | the export's wet master, offline: a fresh engine the session's size, every lane playing at its snapshot's mix, the master's settings, the warm-up passes, the kept pass lined up with the stems | `session/render.ts`, `session/render-plan.ts` (Tone, removed) |
//! | [`slots`] | the two plugin slots: install and removal through their ports, bypass crossfades, notes, live and gain, each slot's own input, where each output goes | `plugin-bridge.ts`, `instrument-slots.ts` |
//! | [`api`] | commands, events, the process context, the plugin seam ([`SlotProcessor`]) | — |
//! | [`dsp`] | Stage 3 sound: the Tone/Blink building blocks, the six built-in synths, the per-track FX chain and the reverb bus, the limiter | Tone.js on Blink's Web Audio |
//!
//! # Rules
//!
//! - **Every committed lane is one master long.** A later take longer than the master (FIXED past the
//!   loop, or a free take stopped past its first loop pass: the nearest whole loop, E10) multiplies it:
//!   the take becomes the new master, whole old loops long, and the other loops tile out to it with the
//!   grid, its beats and every lane's phase unchanged (`looper::Looper`'s multiply, F14); the lanes' FX
//!   follow the beat grid's origin, which a multiply leaves where it was. A TRIM keeps the length too: the
//!   lane's first bars repeat across the loop (F16), heard from the next boundary (a second TRIM before
//!   it waits for it, and is heard there instead).
//! - **An overdub writes `input + feedback * old`** (DUB FEEDBACK, a lane setting beside its volume: 1 is
//!   the plain sum, bit for bit; 0 replaces) past its first and before its last 5 ms, which are linear
//!   ramps stored in the loop (D23, `looper`'s `punch_in` and `fade_tail`): the layer fades in from its
//!   window start, and a clean end fades its last writes back toward what each overwrote (a rejected or
//!   discarded layer is restored as it was). The undo target is still the loop at dub start.
//! - **Undo, PLAY and an immediate STOP are edged in playback only** (D23, `looper`'s `Voice`), over the
//!   same 5 ms: an undo's audible switch (on its boundary, or where a DUB forces it) crossfades the loop
//!   heard into the one it gives back, in heard order; a lane PLAYed into a running loop fades in at its
//!   phase; an immediate STOP (a lane's, STOP ALL's, a second END STOP or FADE press, PLAY/STOP closing a
//!   dub) fades out from the press, from the level heard, a FADE's included. A reversal inside a ramp turns
//!   from the level reached. The state, its events, the grid and the transport change on the press frame
//!   as before: a tail is no playing lane, so the click stops with it. A scheduled stop (END STOP's
//!   boundary, a FADE's end; an undo's crossfade switching on it ends there too), an idle PLAY from the
//!   top with no tail of its own, a fresh take, a load and a REVERSE's or TRIM's swap stay cuts. What
//!   fades out is cached once, N samples a lane allocated with the looper, wherever a later write (a
//!   restore, a TRIM, a reused buffer, a closed dub's capture running on) or a restarted grid could reach
//!   it; a second fade-out inside the first fades both. No stored loop changes.
//! - **FADE is a pending stop with a ramp:** every playing lane stops on a bar line (the click's grid), its
//!   level ramped down to it over the stored volume, which never moves; what a loop-end stop refuses, a
//!   fading lane refuses too (`Refusal::Fading`). The ramp is on the lane before its FX and on its delay's
//!   feedback, so its returns (the delay, the reverb send) fall with it and what rings on past the bar
//!   line is the tail of a loop already faded out; a lane started meanwhile is untouched, and a stop now
//!   (a second press, STOP ALL) leaves the returns ringing as any stop does.
//! - **One clock: the device frame.** Input frame `x` is captured at frame `x`, on each slot's own
//!   input (its capture channel: [`Engine::process_inputs`]); a lane plays loop position
//!   `(f - anchor) mod master` at frame `f`. A take starts `align_frames` (+ the largest live effect
//!   slot's latency, from the block after a live flag changes, and the master limiter's pre-delay)
//!   after its downbeat; every live slot's wet reaches the record tap at that latency (one with less is
//!   delayed by the difference, heard at once), and an instrument's record path lags it by the input
//!   side (a plugin instrument's, less its own latency), so its notes land there too. That live latency
//!   and each slot's delay hold from a capture's arm to its end ([`slots`]): a change applies once
//!   nothing captures. The input sends are wet only and add nothing
//!   to the alignment. There is no user-facing record trim. The synths, FX, input sends and reverbs run
//!   on the device frame less the frames the device skipped, so their blocks follow each other
//!   ([`effects`]); the limiter stays on the device frame.
//! - **Every state change lands on its exact frame.** `process` splits a block wherever a command, a
//!   scheduled looper event, a beat, an AUTO trigger or a render quantum's end falls, so the same
//!   commands render bit-identical output at any block size (the golden jam, the gesture property tests,
//!   `tests/sound.rs` and `tests/slots.rs` assert it). A note sounds one quantum after its frame ([`instruments::LEAD`]);
//!   a wheel or an FX change (a lane's or an input send's) sounds from the next 128-frame quantum
//!   boundary, as a live Web Audio call with no look-ahead does. Every control-rate step (the 128-frame
//!   k-rate quanta, the compressor's 32-frame divisions, LFO and envelope ticks) is anchored to the frame
//!   count, never to a block start. A UI gesture lands at the next block start (jitter: IPC plus one
//!   block, inside the quarter-beat free-stop grace); a Web MIDI pedal lands there too (only the dormant
//!   native MIDI path, D22, stamps its press frame). No audio FIFO.
//! - **Decided while porting** (each test file's header names what it changes): the count-in and an
//!   idle PLAY start on the press frame, with no scheduling lead; a later take armed on an idle transport
//!   counts in as the first did, restarts every loop from the top on the count's downbeat, and refuses
//!   COPY and TRIM meanwhile; undo and reverse on a playing lane
//!   switch on the next loop boundary (an undo's switch crossfades there, D23), and a DUB pressed before it
//!   makes the switch heard at once (D19); STOP on an overdubbing
//!   lane discards the whole layer (its tail plays the layer as heard); an input gap
//!   damages only the capture windows (RETAKE passes) it overlaps, and resets AUTO's listening history;
//!   a jump in the device frame drops the beats it skipped, and count-in beats fire late as one click.
//! - **`process` never allocates, locks or waits.** Buffers are allocated (and their pages touched) in
//!   `Engine::new`; commands and events cross on rtrb rings; a full event ring refuses and counts. A
//!   one-off event is lost there; each lane's state and mix ([`Event::Mix`], on change only) and the
//!   transport are marked delivered only once the ring takes them, so the next publish offers them again.
//!   An engine sends no lane's mix until the commands queued ahead of its first block (a new engine's
//!   settings replay, which can outrun one block's take) are all taken: an idle session job before
//!   then reports the lanes' infos only.
//! - **No loop-sized work in one callback.** Tiling, the undo copy, a discarded layer's restore, COPY,
//!   a multiply's extension of the other loops and a TRIM are block jobs of `looper::JOB_RATE` positions
//!   per rendered frame, started where a read or write head touches next so they stay ahead of it (a lane
//!   a multiply extends reads through its old loop instead until the extension is done; a TRIM writes in
//!   heard order from loop position 0, where the swap at the next boundary reads first). A command that
//!   needs a lane's job finished waits for it (on an exact frame), and every command sent after it waits
//!   behind it, except the instruments', the plugin slots' and the input sends' (a note never waits on
//!   the looper). A full command table leaves the rest in the ring for the next block: late, never
//!   dropped. A jump in the device frame moves every job's schedule with it (`Looper::skip`): work is
//!   owed for the frames rendered, never for the ones the device lost. An overdub an input gap damaged
//!   (a jump's, an xrun's) writes nothing more, so its undo copy reads only the loop before it and the
//!   rejection at its end restores that bit for bit.
//! - **A command is judged when it is pressed**: one that would do nothing then is dropped, never held.
//!   A hands-free press ([`Action`]) acts on the lane the engine has selected when it lands, or a named
//!   one, never on what the UI last saw on the feed: HALVE's bars, HOLD's lane (per pedal: an accepted
//!   press's, for that pedal's release) and a pedal MUTE's state are the engine's. Every looper press
//!   but CLEAR's confirming one disarms a pending pedal CLEAR (a pedal's setting toggle says it is one
//!   with [`Command::Press`]); a setting alone does not.
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
//! `tests/punch_out.rs` holds the punch-out, `tests/render_master.rs` the export's wet master, `tests/multiply.rs` the multiply (a free take's too),
//! `tests/trim.rs` the TRIM, `tests/dub_feedback.rs` DUB FEEDBACK, `tests/punch_ramps.rs` an overdub's punch
//! ramps (its reference: `tests/common/dub.rs`), `tests/playback_edges.rs` the undo, PLAY and STOP edges
//! (its reference: `tests/common/edges.rs`), `tests/fade.rs` FADE, `tests/input_fx.rs`
//! the input sends, `tests/mix_feed.rs` a lane's mix on the feed and the event ring's delivery, `tests/pan.rs`
//! a lane's pan (the centre's bits against the render before pan: `effects`' unit test). `tests/perf.rs`
//! holds the ignored cost bars (Stage 2 and 3, the input sends, the lanes' pan, a multiply's burst, a
//! TRIM's) and the Stage 3 load's alloc check. `tests/golden_jam.rs` runs the golden jam at 44.1 and 48 kHz, bit-identical across block
//! sizes 1, 32, 64, 127, 128, 480 and 1024; `tests/gestures.rs` runs proptest gesture scripts (one
//! recorder; a whole-bar master; every lane one master long; undo twice is identity; undo after an
//! N-cycle dub gives back the pre-dub loop; finite output) at two block sizes, bit-identical, with
//! commands landing mid-block. The Stage 3 ports' references and tolerance classes: [`dsp`].
//!
//! Run cargo-mutants on [`grid`] and [`looper`] whenever either changes (the planted-bug rule of
//! `verify/README.md`; no workflow runs it). Seven survivors are equivalent mutants, named
//! here so a rerun can tell them from new ones: the keep-last `written` and the commit's `raw` minimum
//! (the committed length does not move), an empty fill job at `lo == master`, a restore offset at
//! exactly the span's end, `plan_later_stop`'s bar clamp (the window end bounds it), `pair`'s
//! ordering (guarded by `assert_ne`), and `idle`'s `master > 0` as `>=` (with no master no lane is
//! STOPPED, and `start_recording` tests `master == 0` first).
//!
//! # Open threads
//!
//! - **A gap the live plugin's latency carries into a window is missed.** The tap carries input frame
//!   `x` at `x + live latency`, but `input_gap` gets the device frames, which must stay (a jump in a
//!   window's closing tail loses those tap frames): a silent block just before a layer commits it clean.
//!   Next check: pass the looper both intervals, then un-ignore
//!   `a_silent_block_damages_the_layer_its_frames_reach_through_a_live_plugin` beside
//!   `a_jump_in_a_layers_closing_tail_rejects_it_through_a_live_plugin` (`tests/overdub_undo_reverse.rs`).
//! - **With no device running, a session job is served before the commands queued ahead of it.**
//!   `service_session_idle` runs the job at once, so a setting or a take sent before a load applies after
//!   it on resume. Applying them in idle is wrong: a capture latches alignment 0, and the 64-entry pending
//!   table can fill while earlier commands still wait in the ring. Next check: a fix that orders them
//!   without applying captures in idle, made green by the two ignored `with_no_device_*` tests
//!   (`tests/session.rs`).
//!
//! # Beside this crate
//!
//! The device side (streams, MIDI, Share output) is `src-tauri/src/engine_io`; the CLAP/VST3 units and
//! their owners are `src-tauri/src/host/engine_slot.rs` and its siblings; the feed that carries the
//! events and the [`overview`] to the UI is `src-tauri/src/engine_io/feed.rs`, and the export's snapshot
//! that carries the [`render`]ed wet master is `src-tauri/src/engine_io/session.rs`.

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
pub mod render;
pub mod session;
pub mod slots;

pub use api::*;
pub use engine::{Diag, Engine, EngineConfig, EngineHandle, Taps};
pub use overview::{LaneView, Overview};
pub use render::{wet_master, wet_master_with, RenderOptions, WetMaster};
pub use session::{Load, LoadTrack, SessionError, SessionJob, SessionPort, Snapshot, SnapshotTrack};
pub use slots::SlotPort;

/// Within this of its target a gain glide takes the target itself: -120 dB of full scale, a step far
/// under any converter's noise floor, reached about 14 time constants into a glide from 1 (some 170 ms
/// at 12 ms). A lane's pan glides its position with it: a step of at most 1.2e-6 in either gain. Without it a glide never lands: one to 0 decays into f64's subnormals and stalls there (an
/// x86 slow path on every frame), and one to another target sits a hair off it (keeping a slot's and an
/// instrument's unity fast paths off) until it rounds onto it some 37 time constants in.
const GLIDE_SNAP: f64 = 1e-6;

/// One frame of a one-pole gain glide (a lane's volume and mute, a lane's pan position, a slot's gain,
/// an instrument's level, the master volume): today's `target + (gain - target) * coef` until it is within [`GLIDE_SNAP`] of
/// the target, then the target exactly. Per frame, so it lands on the same frame at any block size.
#[inline]
pub(crate) fn glide(gain: f64, target: f64, coef: f64) -> f64 {
    let g = target + (gain - target) * coef;
    if (g - target).abs() < GLIDE_SNAP {
        target
    } else {
        g
    }
}
