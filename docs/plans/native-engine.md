# Plan: one native audio engine (OPEN)

Decided 2026-09-24 (`docs/ARCHITECTURE.md` § Decided: one native audio engine). Click, looper, synths,
FX, mixer, limiter, MIDI and plugin processing move into ONE Rust engine clocked by the audio device;
the WebView becomes UI only. Delete this file when Stage 6 lands, after folding what still binds into
ARCHITECTURE, the briefings and the engine crate's own briefing.

**Owner musts, in rank order:** the base product works — stack, responsiveness, basic looping; no
user-facing latency calibration (rec align goes); ASIO and WASAPI in one package, switchable at
runtime (already true in the release build); the wet master shareable to OBS, browsers and voice chat
("Share output"). Owner decisions for this plan are rows E1–E7 in `STATUS.md` § Decisions.

**Working rules**

- The live line takes fixes only (`AGENTS.md` § Standing rules). Engine code lands on `main`, additive
  and dormant (a DEV probe, then a hidden toggle) until Stage 6.
- Every stage ends machine-verified on the PC. By-ear items gather into ONE engine lap at Stage 5.
- Numbers marked "estimate" are estimates; a pass bar says what it stands for.
- The Mac gets a Rust toolchain later (owner, 2026-09-24); until then engine work is PC-only.

## Stage 0 — ship today's architecture

*Owner: plays one jam on the installed draft, then clicks publish (STATUS E1).*

Close the two open items in `docs/plans/release-prep.md` first: the artifact job is gated on its exact
SHA (`pnpm check`, the Rust tests, the no-ASIO check), and the first CI-built exe is heard on the rig.
Then tag `v0.1.0`; `build-exe.yml` builds the ASIO+WASAPI installer and stages the draft. Testers stop
building from source (LLVM and the ASIO SDK are build-machine needs only). Known limit, said in the
release notes and `README.md`: takes land late until Audio Settings → rec align is set by ear (+69 ms
on the dev rig; other rigs differ); the engine removes the setting (STATUS E7).

## Stage 1 — premise spike (days; beyond the OWNER zone, it stops the plan)

*Owner: leaves the PC with the loopback cable in (line out R → input 2) and the app closed; gets one
PASS/FAIL table; anything in the OWNER zone is his call.*

Four questions, answered with numbers before any engine code: does one native callback give
calibration-free alignment, a low round trip (RT) with an amp in the callback, an explainable WASAPI
round trip, and a silent share stream?

**Shape.** DEV flags on the debug `app.exe`, beside `--probe-output-latency`
(`src-tauri/src/audio_latency_probe.rs`); they exit before Tauri starts. New files (not yet built):
src-tauri/src/host/engine_spike.rs as a DEV child module of `vst3.rs` (reaches the VST3 host's private
items without widening visibility) and src-tauri/src/share_probe.rs. No production-path edit. A
`pnpm native:spike` mode in `scripts/native-probe.mjs` (not yet built) runs the matrix and prints the
table: per buffer size one 10-minute run (A4) and four 60 s launches.

- `app.exe --probe-engine-spike <asio|wasapi> <64|128|256|default> [--plugin <file.vst3>] [--in N] [--out N] [--minutes N]`
- `app.exe --probe-share <mute|vol0|open|zeros|dual>`

**ASIO, one callback.** Build and play the input stream first, then the output: asio-sys 0.3.0 runs
registered callbacks in registration order inside one bufferSwitch. The input callback copies its
block into a preallocated handoff and increments an atomic cycle count; the output callback passes
`sameCycle` when the input count is one ahead of its own. Latencies come only from deltas WITHIN one
stream: inLat = input callback − capture, outLat = output playback − callback (both carry
ASIOGetLatencies). cpal ASIO instants are never compared across streams: each stream has its own
TimeBase, and the rig's driver trips cpal's timestamp overflow (`src-tauri/Cargo.toml`, the cpal
ASIO section). Gaps and the MIDI press→frame map use a QueryPerformanceCounter read at callback entry.

A 64-frame Hann-windowed chirp plays through the cable (`docs/VERIFY.md` § Native / Tauri
verification, the native:loopback baseline: Scarlett 2i2, `--in 1 --out 1`); input and output are
recorded on one frame counter and cross-correlated offline. A1–A4 run with the plugin monitor muted
(no feedback loop). R1/R2 use the echo method of `src/debug/loopback-sync.ts`: the chirp goes round
input → plugin → output → cable once more at monitor gain ≤ 0.5, RT = the echo spacing, Pro-Q 3 in
Zero Latency mode, plugin latency from its report. C1 uses the rig's amp-sim (Archetype Petrucci,
VST3). The plugin runs in the callback by copying, not extracting, the VST3 load and per-block process
sequence from `vst3.rs`; `setProcessing(1)` on the callback thread; the body inside the `rt_alloc`
guard.

**WASAPI.** cpal gives each WASAPI stream its own thread, so the output callback pops the input
through an rtrb ring (the Stage 4 join). cpal's WASAPI threads get no priority boost without its
`realtime` feature (off here), so every engine, join and share callback calls the existing
`promote_pro_audio` once on first entry. Alignment from cpal's QPC stamps only. Print the RT with its
parts (playback−callback, input age, ring fill, device period) against today's ~280 ms.

**Silent share.** A child `app.exe` renders a 997 Hz tone on the default endpoint: its own session
muted (`mute`), at volume 0 (`vol0`), unmuted (`open`, positive control), silent (`zeros`, negative
control), or audible plus a muted copy 20 ms later (`dual`: process loopback takes a whole process
tree, so this shows whether an app capture would hear both). The parent captures the child through
process loopback (ActivateAudioInterfaceAsync, PROCESS_LOOPBACK, INCLUDE_TARGET_PROCESS_TREE) and,
at the same time, plain endpoint loopback (what "share system audio" sees). The `windows` features it
needs are already on windows 0.61.3 through cpal (features unify; no version change).

**Output:** one JSON line per phase (`[engine-spike]` / `[share-probe]`), then one
`PASS|FAIL <id> <value> <bar>` line per criterion. 48 kHz; A-criteria at 64/128/256.

| id | bar | what it stands for |
|---|---|---|
| A1 | sameCycle on every cycle | click out and input in, in one callback |
| A2 | \|median lag − (inLat+outLat)\| ≤ 1.0 ms, dry input | the driver's report alone puts a take on the grid (today +64 ms at trim 0) |
| A3 | spread ≤ 1 frame per run; median moves ≤ 2 frames across launches | one clock, stable across restarts (today one launch in seven reads −8 ms) |
| A4 | drift ≤ 1 frame over 10 min | nothing drifts inside a take (today 0.7–3.2 ms/min) |
| R1 | \|RT − (lag + plugin latency)\| ≤ 1 frame, Pro-Q | no hidden buffering on the in-callback plugin path |
| R2 | RT ≤ 22.2 ms at 256, Pro-Q | at most half of today's 44.4 ms (expected ~11–16 ms, estimate) |
| C1 | amp-sim at 128 and 256, 120 s, first 5 s excluded: 0 gaps (> 1.5 periods between callback entries), 0 cpal Xrun errors, 0 host RT allocs (Rust allocator only), block time p99.9 ≤ 50 % and max < 90 % of the period | the amp leaves room for loops, synths and FX |
| W1 | \|median residual\| ≤ 2.0 ms, spread ≤ 1.0 ms | WASAPI takes also align from timestamps |
| W2 | RT printed with its parts; an unexplained remainder over 5 ms reads "unknown" | explains the ~280 ms (no bar) |
| S1 | `mute` or `vol0`: process loopback within 3 dB of `open`, tone ≥ 30 dB over the floor; endpoint loopback and `zeros` ≤ −80 dBFS; `dual`: no second xcorr peak within 12 dB | the share is capturable while the room hears nothing, the test can fail, and a mirror would not double |

**OWNER zone (numbers go to the owner, no stop):** A2 residual 1–3 ms while A3/A4 hold; A3 median move
≤ 48 frames (1 ms); any W1 or C1 FAIL (accept a documented WASAPI residual or a built-in per-backend
constant that is not user-facing; amp cost vs buffer size); any S1 FAIL (STATUS E2). **STOP** only
beyond that zone on A1–A4, R1 or R2, reproduced on a second launch after a setup check: no Stage 2
work starts and the numbers go to the owner.

**Shortcut that passes without proving the premise:** scoring A2 on the wet signal at its own output
frame — the cable then lands exactly RT late whatever the driver reports. A2 is scored on the dry input
against inLat+outLat only, never against a lag fitted from an earlier run.

**Rules.** Time box 3 working days (estimate); INVALID (xcorr peak < 0.8) is a setup fault: fix and
rerun. The results replace the RT baseline line in `docs/VERIFY.md`; the probe stays as the engine's
L2 gate.

## Stage 2 — lf-engine, the pure engine crate

*Owner: nothing to hear; progress is CI green on the engine tests.*

**Crate** (not yet built): src-tauri/crates/lf-engine; `src-tauri/Cargo.toml` becomes the workspace
root (`[workspace] members = [".", "crates/lf-engine"]`). Dependencies: `rtrb`, `libm` if needed;
dev-only `assert_no_alloc`, `proptest`, `hound`. Deny check: `cargo tree -p lf-engine -e normal,build
--target all --prefix none --format {p}` fails on tauri, cpal, windows, windows-core, clack or vst3.
Add `[profile.dev.package.lf-engine] opt-level = 3`. `pnpm rust:check` and `rust-test.yml` run
`--workspace`, and `ci.yml` gains an ubuntu job running `cargo test -p lf-engine` in this stage, or the
engine tests never run. Gotcha: `tauri dev` watches all of `src-tauri/`, so engine edits relaunch a
running dev app.

**The state machine moves to Rust** (one owner, no IPC races between an engine event and a fresh
gesture). TypeScript keeps UI and settings.

| Module | Ported from | Notes |
|---|---|---|
| clock, click | `src/audio/clock.ts`, `src/audio/quantize.ts` | device-frame clock; click computed per sample from the grid; tap-tempo averaging stays in the UI and sends SetBpm |
| grid | `src/audio/looper/grid-math.ts` | whole file; `compensatedLoopFrame` dropped |
| looper machine | `src/audio/looper/machine.ts`, `src/audio/looper/transport-actions.ts`, `src/audio/looper/looper.ts` | the action gates (`src/ui/looper/gates.ts`) and the CLEAR double-press window move here; refusals return as events |
| track, capture, playback | `src/audio/looper/state.ts`, `src/audio/looper/peaks.ts`, `src/audio/looper/capture.ts`, `src/audio/looper/auto-record.ts`, `src/audio/looper/playback.ts` | mono tracks; overdub sums in place, so swap buffers and swap timers go |
| mixer, limiter, graph | `src/audio/looper/mixer.ts`, `src/audio/engine.ts` | bus topology as today: instrument → input bus → record tap (pre-limiter) and master → limiter |

**API.** Commands and events cross the RT boundary only over rtrb rings; a full event ring drops and
counts, nothing blocks. `process(ctx, io, inserts)`: ctx carries the frame counter, sample rate, xrun
flag and align_frames (a constant from the driver's report — not a user trim); `inserts` is the plugin
seam (Stage 4; tests pass a fake). A UI gesture lands at the next block start (jitter: IPC + one block,
inside the quarter-beat free-stop grace); MIDI pedals carry a device frame. Every control-rate step
(k-rate params per 128 frames, the compressor's 32-frame divisions, LFO and envelope ticks) is
anchored to the absolute frame counter: process() splits a block at those boundaries and never ticks
at the block start. No internal FIFO.

**Memory.** Everything allocated in Engine::new: 5 × (record + undo) + 1 retake + 1 snapshot, 60 s ×
sample rate mono f32 (~138 MB at 48 k, estimate). No O(loop) work in one callback. Undo keeps today's
one-level UNDO/REDO toggle: a frame is copied live → spare only the first time an overdub epoch writes
it; undo and redo swap the two buffers; at epoch start a budgeted block job re-syncs spare ← live over
the previous epoch's span, and an undo or layer reject before that job finishes waits for it (an
event). Copy and the snapshot are budgeted block jobs; Clear zeroes on write; Reverse is an
index-mapping flag. A sample-rate change builds a new engine off-thread and retires the old one
off-thread.

**Tests** (crates/lf-engine/tests, not yet built). A scenario helper scripts (frame, command) lists
over frame-coded inputs and renders at block sizes {1, 32, 64, 127, 128, 480, 1024}, including runs
that cross 32/128 boundaries with a parameter change in flight.

- The rig-guard scenarios become the spec, one test file per group: click_grid (grid, accent-grid,
  pulse-forced-clamp), count_in (count-in, count-grid, bpm-lock), phase_preserve, first_take
  (free-stop, free-record-cap, fixed-length, short-take), later_arm (later-arm, looper-arm),
  overdub_undo_reverse, retake, auto_record. record-compensation is deleted, not ported; an align test
  asserts a take shifts by exactly align_frames. Each file header names the guard it ports.
- golden_jam.rs ports the golden jam (`verify/probes/golden-jam.mjs`, assertions 1–6, 8, 9; 7 becomes
  an injected xrun; 10 stays a UI probe). Output is in hand and frames are absolute, which closes the
  jam's playback and uniform-slip blind spots.
- Block-size invariance (bit-exact), `assert_no_alloc` around every process call, proptest over legal
  gesture scripts (one recorder; master = bars × frames-per-bar; every lane = master until F14/F16;
  undo∘redo = identity; undo after an N-cycle dub is bit-exact to the pre-dub loop; no NaN or panic).
  cargo-mutants on grid and machine for acceptance, and again when either changes (the planted-bug
  rule of `verify/README.md`, automated; no scheduled workflow).

**Acceptance.** `cargo test -p lf-engine` green on ubuntu and windows CI; deny check green; every rig
guard mapped or its deletion justified; golden jam green at 44.1 k and 48 k across all block sizes;
mutants killed or skipped with a reason; 5 lanes (one overdubbing) + click + limiter at 48 k / 64
frames under 10 % of block time offline (estimate). The live line is untouched.

## Stage 3 — synths and FX in lf-engine

*Owner: nothing to hear yet (STATUS E6 before the limiter port).*

**Capture references first, while Tone still runs.** A new probe (not yet built):
verify/probes/tone-refs.mjs renders Tone OfflineContext scenarios from the production modules (as
`verify/probes/fx-grid.mjs` does) with `Math.random` seeded: note scripts per synth (range, velocity,
full-polyphony chord, voice steal, legato, release, vibrato, bend), a seeded input per FX over a
parameter grid, a limiter sweep plus bursts, and the generated reverb IR and noise tables. Float32 WAV
through the real `encodeWav` (the piano peaks near 2.15). Fixtures sit in the crate's tests/fixtures
with a manifest (scenario, sha256, capture commit, Tone and Chromium versions, rate). Budget ≤ 10 MB,
48 k plus a 44.1 k spot set; regenerate only with a stated reason (git keeps every copy). The same
probe captures short v0.1.0 exports (pcm16 zip, float32 recovery) as Stage 5 import fixtures.

**Tolerance classes:** N = null residual ≤ −60 dB of the reference RMS; S = STFT bands within ±1 dB
and RMS envelope within ±0.5 dB. Each port takes the tightest class it passes, recorded in the
manifest. A failing test writes the Rust render beside the reference for an ear A/B.

**Ports, literal:** lead and piano (port Blink's band-limited PeriodicWave tables), pad (FM), organ
(AM), bass (MonoSynth + 24 dB lowpass + filter envelope + vibrato), drum kit (Membrane, Noise with the
fixture table), filter (two biquads, equal-power bypass), stutter, delay. **Hard:** MetalSynth (six
audio-rate FM squares + resonant highpass); PitchShift (two modulated delay lines, its latency
reproduced as heard, CPU budgeted); the convolution reverb (seeded IR with the same envelope and
normalization, a partitioned FFT convolver — crate unknown); the limiter (Blink's
DynamicsCompressorKernel: lookahead, knee, adaptive release, makeup). Blink ports are BSD-3: notices
through `docs/plans/release-prep.md`.

**Acceptance.** Fixtures within budget; every scenario passes its class; alloc and block-size tests
cover voices and FX (FFT paths ≤ −120 dB instead of bit-exact); six synths at full polyphony + full FX
on five lanes + reverb at 48 k / 64 frames under 50 % of block time offline (estimate).

## Stage 4 — device owner and plugins in the engine callback

*Owner: nothing to hear yet; everything audible waits for the Stage 5 lap.*

**Where.** A module, not a crate (not yet built): src-tauri/src/engine_io, holding the device owner,
streams, MIDI and the join and share pipes (InPipe, OutMonitorPipe and DriftController move there from
`host/transport.rs`). lf-engine defines a SlotProcessor trait; the CLAP/VST3 host stays in
`src-tauri/src/host/` and implements it.

**Module fates.** Reuse: `asio_startup.rs`, `host/scan.rs`, `host/editor_window.rs`,
`host/rt_alloc.rs`, the callback/controller/resize fixtures. Reshape: `host/native_io.rs` becomes the
process-wide device owner (its transition state machine is the kernel; per-slot NativeIo goes);
`audio_output.rs`/`audio_input.rs` keep device lists, picks, the ASIO cache and channel select;
`host/clap.rs`/`host/vst3.rs` keep load, params, state, editors and owner threads, and each producer
loop becomes a SlotProcessor (~250 LOC each, estimate); `host/commands.rs` moves arm/disarm/gain/
latency to engine-host commands; the restart fixtures assert "the engine kept rendering".

**Device owner.** One owner thread serializes device transitions (cpal streams are Send; ownership is
for ordering). The engine sits in a Mutex the callback only try_locks — a miss plays silence and
counts — and the owner takes it only with both streams dropped, so engine and slots outlive device
switches and loss. The callback body runs under catch_unwind inside the lock guard: a caught panic
latches a fault and plays silence, the Mutex is never poisoned, and nothing unwinds into asio-sys's
extern "C" bufferSwitch (which would abort). ASIO virtual duplex: input built first, output second,
always as a pair; the output checks the input's cycle count. A channel change uses the existing channel
select, never a rebuild (a rebuilt input would register after the output and add a block). Pin cpal
`=0.18.1`: the callback order A1 proves is read from asio-sys 0.3.0; 0.18.2 moves to asio-sys 0.4.0
and windows 0.62 and needs its own A1 rerun. WASAPI: the output callback is the clock, input joins
through a ring + InPipe, every callback thread promoted as in Stage 1. Backend switch: fade out, drop
output then input, reconfigure under the lock, reopen input then output, fade in; the frame clock
pauses and loops resume in place.

**Plugins in the callback.** The owner builds and activates a unit and pushes it on a ring; the
callback starts processing and crossfades out of bypass. Remove and restart run the reverse; the
callback returns the unit on a ring and never drops one (a drop is dealloc plus DLL calls). Lifecycle
— load, activate, CLAP request_restart, VST3 restartComponent, teardown — stays on the owner thread
with the slot bypassed: an FX slot passes dry, an instrument slot is silent, loops and click never
wait. The VST3 processor and its event and param lists move as one unit struct with a documented
`unsafe impl Send` (one thread at a time), built on the owner.

**Native MIDI.** midir, one thread per port plus hot-plug polling; a lost port releases its notes.
Live notes enter at the next block start; looper actions carry the press frame (QPC → frame).
MIDI-learn bindings stay in settings (`src/app/midi-actions.ts`), mirrored to native, which does the
matching, learn capture, momentary/latching and consume-first rules; bindings re-key from Web MIDI
ids to port name + occurrence.

**Share output.** On WASAPI with a silent-capable target no mirror opens: app capture already takes
the main output (S1 `dual`). On ASIO, whose output bypasses the Windows engine, the engine pushes the
post-limiter stereo master into a drop-on-full ring and a second WASAPI stream pulls it through
OutMonitorPipe (resample + DriftController, 20 ms setpoint) — muted if S1 passed, else to a
user-picked endpoint (STATUS E2). It never blocks the engine; losing it stops only the mirror.

**Device loss.** Error callbacks latch and wake the owner, which drops the streams and keeps engine and
slots. ASIO reset → rebuild from the cache, else fall back to the WASAPI default with a toast; a lost
WASAPI endpoint → the default endpoint. A take recording when the device drops: STATUS E3.

**Gates.** `cargo test` on the fake-driver seams: the transition state machine, the install/remove/
restart handshake with a fake processor on a fake driver thread, join and share convergence at
±400 ppm, MIDI parse and binding rules ported from TS. The Stage 1 probe gains a scripted mode
(`--script <file>`: frame-coded engine commands) that records, overdubs and loops headlessly, reruns
the A/R/C/W/S bars on the engine, and drives 20 backend switches and a plugin swap matrix while loops
play. A 10-min soak with two plugins at ASIO 128: host RT allocs 0 and every diag counter 0 (callback
gaps, ASIO overloads, lock misses, duplex-order faults, join/share starves, command-ring full). The
existing plugin fixtures and `pnpm native:swap|survey|smoke|recall` rerun.

## Stage 5 — cutover behind a hidden toggle

*Owner: turns the engine toggle on and walks the engine lap.*

**Engine-host seam** in `src/platform/host.ts`: `Platform` gains `engine: EngineHost`. Commands out
are fire-and-forget (`send(cmd)` → invoke → the command ring; errors come back on the feed): transport,
track volume/mute/reverse/FX, BPM/bars, click, notes, synth pick, auto-rec threshold, the MIDI-binding
mirror. Request/response: devices, setDevice, setShare, exportSession, importSession, recovery. The
arm/monitor/gain/buffer/latency calls and noteOn/noteOff leave `PluginHost`; scan, load, editors,
params, state and the ASIO status calls stay.

**State feed in:** a Tauri v2 ipc Channel of raw bytes, ~60 frames/s from a non-RT feed thread: track
states, levels, input peak and clip, the phase anchor (device frame, loop start, master length,
sample rate), and waveform peak buckets only when a track's peaks change. Never PCM; the layout is
fixed and versioned. Invariant 6 holds: the channel writes one plain mutable feed object, the waveform
rAF reads it and extrapolates the playhead from the anchor, Solid signals are written only on change.
Measure the feed's IPC cost and playhead jitter before committing to it. One golden wire fixture
(every command and event as JSON plus one binary frame) is parsed by both a cargo test and a TS guard.

**Web fake** in `src/platform/host.web.ts`: records every send and emits frames a probe scripts; not a
second looper. UI probes assert gesture → command and frame → DOM. After the flip `pnpm dev` makes
no sound (the browser tier stays a verification rig).

**Session, recovery, export, import go native.** Recovery and export read a snapshot: the engine
copies a track into the preallocated snapshot buffer as a budgeted block job and hands it to the IO
thread over a ring, which returns it after writing. Export keeps today's zip layout and session.json
fields; the wet master comes from an offline engine running the same process() (no parallel FX
implementation). Import by path (native open dialog plus window drag-drop, STATUS E4); session
validation ported to Rust with the same rules; the v0.1.0 fixtures must import. Recovery: float32 stems
+ session.json written atomically to the app-data folder on today's timing (STATUS E5 for v0.1.0's
IndexedDB records). Settings, rig recall and MIDI bindings stay in TS storage, mirrored to native at
boot.

**Toggle:** one hidden Audio Settings switch, persisted natively and applied on restart, never live
(ASIO allows one client); probes set it in their own profile. Web stays the default until Stage 6.

**Parity checklist** (each item a cargo test, a UI probe on the fake, or a lap stop): rec/dub/play/
stop/undo/retake/clear/reverse, count-in and accent, BPM lock, fixed/free length, record cap, auto
rec; 5 tracks × volume/mute/5 FX, click; six synths + drum kit from the screen keys, PC keys and a MIDI
controller; MIDI learn and pedal actions; GO LIVE, editors, rig recall, a plugin swap while loops
play; export shape, v0.1.0 import, recovery after a kill, the close guard; a WebView reload while the
engine plays (new: the feed resyncs); device yank, ASIO↔WASAPI switch mid-session, Share output in
OBS/Discord. Measured: `pnpm native:loopback` ported to the engine (residual within ±2 ms with no
trim, spread ≤ 1 ms across launches, drift ≈ 0, RT ≤ the Stage 1 number, 10/10 launches with no
rejected take), callback CPU at 64/128/256, UI layouts per 5 s ≤ today.

**The engine lap** (replaces stops, never appends; `STATUS.md` cap): Stop 1 and Stop 2's C check
become "the take lands on the click, no trim, at 256 and 128"; Stop 6 is rewritten for the engine's
device-loss path; Stop 9 becomes the WASAPI A/B on the engine; the rest carries over. Added by ear:
guitar feel through the in-callback amp, loop↔click tightness, the gap on a backend switch, bypass
crossfades on load, Share output heard in OBS, Chrome and Discord, the real footswitch, synth and FX
timbre against today.

## Stage 6 — flip and delete

*Owner: publishes v0.2.0.*

Native becomes the default. The web path stays one release as a fallback toggle only if the lap found
a regression (owner's call). Then delete:

- **TS:** the Web Audio audio path in `src/audio/` (looper, engine, clock, master, capture, playback,
  plugin bridge, record latency, input router, Web MIDI, device glue, output match, autosave, export,
  FX, synths, worklets: ~8.9k LOC, estimate). Survivors move to src/ui/state (persist,
  audio-settings, rig-recall, plugin-descriptor, instrument-slots, a slimmed instrument, quantize
  display math, FX labels) and `src/audio/` goes. In `src/debug/`: the loopback and render-clock
  probes; the marker probe if it measures only the bridge (unknown). In `src/platform/`: the
  SharedBuffer receive path and getUserMedia/Web MIDI.
- **Rust:** from `host/transport.rs` the WebView bridge (Hop1Pipe, the SharedBuffer ring, PaceTimer,
  the force-48k path; ~625 LOC); the old producer loops and RT spawn/join in clap.rs and vst3.rs;
  `audio_latency_probe.rs` and `marker_probe.rs` if nothing else uses them.
- **verify:** the 17 rig guards (ported in Stage 2), the rig harness, pure guards whose modules died,
  the Web Audio probes including the golden jam and its workflow. UI probes rebase on the fake.
- **Dependencies:** `tone`, `ringbuf.js` (its MPL notice with it); COOP/COEP only if nothing still
  needs SharedArrayBuffer (grep at deletion).

**Invariants, rewritten together** (the docs guard compares the two lists). Numbers whose meaning
survives keep it, so in-code citations stay valid (`rt_alloc.rs`, `clap.rs` and `vst3.rs` cite #5 as
no-allocation): 1 the engine's device-frame clock is the single tempo/quantization authority; 2
commands and events cross the RT boundary only through rtrb rings; 3 the engine is the only owner of
musical state, the UI sends commands and reads the feed, no PCM crosses the platform boundary; 4
plugin lifecycle runs off the RT thread with the slot bypassed; 5 the RT path (engine callback, plugin
process) never allocates, logs, blocks or waits on a lock (try_lock only); 6 and 7 unchanged (6's
pattern pointer → the feed mirror). Grep `invariant #?[1-7]` across `src-tauri/src`, `src/` and
`verify/` and re-check each citation. Then ARCHITECTURE (the Decided section gets the Stage 1 numbers;
§ Audio architecture rewritten), `AGENTS.md` (architecture paragraph, router: the lf-engine briefing
replaces `src/audio/AGENTS.md`, commands, the dev-app rule), `verify/README.md` (cargo tests as the
rig tier), `docs/VERIFY.md`, the STATUS lap, `README.md` (no rec align, the silent browser build).

**Gates and release.** `pnpm check` gains `cargo test -p lf-engine` (it warns and skips where cargo is
absent: the Mac until its toolchain lands). Release v0.2.0 from the same workflow; notes: rec align
gone, the browser build is silent, and E5's recovery answer.

## After the flip: first features

Built on the engine, in this order unless the owner reorders: F14 multiply (a later take longer than
the master grows the loop; shorter tracks keep repeating), F15 input FX (an insert before the record
tap, monitored in the same callback, so the player hears what is recorded), F16 track length after
recording (session formatVersion 2). Also engine-bound from `docs/plans/pedalboard.md`: D12 controller
data to plugins and F8 synth plugins on the device clock (both arrive with Stage 4). Stage 2 keeps
F14/F16 possible (a per-track length field, lane length ≤ its buffer) and builds nothing more.
