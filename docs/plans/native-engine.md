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
(`src-tauri/src/audio_latency_probe.rs`); they exit before Tauri starts. Files:
`src-tauri/src/host/engine_spike.rs`, a DEV child module of `vst3.rs` (reaches the VST3 host's private
items without widening visibility) and `src-tauri/src/share_probe.rs`. No production-path edit.
`pnpm native:spike` (`scripts/native-spike.mjs`) runs the matrix and prints the table: per buffer
size one 10-minute run (A4) and four 60 s launches. The probes run at the device's current rate and
never change it (a forced 48 kHz disturbed the owner's listening).

- `app.exe --probe-engine-spike <asio|wasapi> <64|128|256|default> [--plugin <file.vst3>] [--in N] [--out N] [--minutes N | --seconds N] [--echo] [--quiet] [--device <name>]`
- `app.exe --probe-share <mute|vol0|open|zeros|dual|all>`

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
`PASS|FAIL <id> <value> <bar>` line per criterion. The device's rate; A-criteria at 64/128/256.

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

**Measured so far (2026-09-24, dev PC, Scarlett 2i2 at 44.1 kHz, no cable in):** built and pushed;
the cable matrix (`pnpm native:spike`) waits for the owner (cable in, monitors down, ~55 min of
audible chirps). A virtual cable (VB-Cable) cannot stand in: it measures Windows' buffering, not the
interface driver's report against the physical path.

- A1 mechanism: ASIO 256, 3639/3639 callbacks sameCycle, 0 other-thread, 0 gaps, 0 xruns.
- Driver report at ASIO 256: inLat 549 + outLat 637 frames = 26.9 ms before any plugin, so R2
  (≤ 22.2 ms) likely fails on this driver at 256 if A2 confirms the report.
- C1 partial: Archetype Petrucci X in the ASIO callback, 256, 20 s (`--quiet`): 0 gaps, 0 xruns,
  0 allocs, block p99.9 28 % / max 32 % of the period; plugin latency 57 frames. The 120 s runs at
  128 and 256 are open.
- WASAPI mechanics (441-frame packets, `--quiet`): 0 gaps, 0 xruns. The first run showed a 298 ms
  join ring: input ran 300 ms before output opened and the backlog was never drained. Drained to its
  target, the parts sum to ~47 ms (input age 15.5 + ring 20 + output 12) against today's ~280 ms.
  Whether the production WASAPI path carries a similar startup backlog: unknown, not examined.
- S1, one `--probe-share all` run: FAIL. Process loopback captures the child after its session mute
  and volume (digital zero for `mute` and `vol0`, the tone at 0.0 dB gain for `open`), so a muted
  mirror is silent to app capture; `dual` shows no second peak. STATUS E2 decides the fallback.

**First cable runs (2026-09-26, ASIO 256, 44.1 kHz, line out R → input 2 over a guitar cable,
`--only=asio --launches=1 --long-min=1`):** two launches, 240/241 chirps found in each, spread < 0.001
frame inside each run, 0 xruns.

- A2: launch 1 lands at 1189.8 frames against the reported 1186 (+0.086 ms, PASS), so the 26.9 ms
  report holds and R2 fails at 256 on this driver. Launch 2 lands at 1711.8 (+11.9 ms, FAIL): A3
  moves 522 frames between the two launches. Not yet reproduced; the matrix's 4 launches per block
  size decide it. Unproven lead: the spike plays the input stream before it builds the output
  (`run_asio`), the order behind the first-open hang below, so the two streams' offset may differ
  per launch.
- The first open of each run's block size hung with no `started` line and fell to the runner's
  timeout (64 and 128 on 2026-09-25, 256 on 2026-09-26); all 14 later opens at an unchanged size
  started.
- Launch 1 had one sameCycle miss (A1) and one 12 ms callback gap, at the moment a failing USB
  device re-enumerated on another controller of the dev PC (a hardware fault, open).
- Setup: the earlier INVALID runs had the cable in line out L (on the 2i2 the R jack is the left one
  seen from behind); a second cable gave no signal from line out R either, cause unknown.

## Stage 2 — lf-engine, the pure engine crate

*Owner: nothing to hear; progress is CI green on the engine tests.*

**Crate** (built): `src-tauri/crates/lf-engine`, a member of the `src-tauri/Cargo.toml` workspace.
Dependencies: `rtrb`, `libm` if needed; dev-only `assert_no_alloc`, `proptest` (without its fork
feature), `hound` when a test first writes audio. `scripts/engine-deny.mjs` fails the tree on the
tauri and clack families, cpal, windows, windows-core, clap-sys or vst3; `pnpm rust:check` runs it
with the `--workspace` check and test, `ci.yml`'s `engine` job runs it and `cargo test -p lf-engine`
on ubuntu, `rust-test.yml` tests the workspace on Windows. Gotcha: `tauri dev` watches all of
`src-tauri/`, so engine edits relaunch a running dev app.

**The state machine moves to Rust** (one owner, no IPC races between an engine event and a fresh
gesture). TypeScript keeps UI and settings. Built: the module map, the rules and what is not built yet
are in the crate briefing (`src-tauri/crates/lf-engine/src/lib.rs`); tap-tempo averaging stays in the
UI and sends SetBpm.

**API.** Commands and events cross the RT boundary only over rtrb rings; a full event ring drops and
counts, nothing blocks. `process(ctx, input, left, right, inserts)`: ctx carries the frame counter, the
xrun flag and align_frames (a constant from the driver's report — not a user trim); `inserts` is the
plugin seam (Stage 4; tests pass a fake) and adds its reported latency to the alignment. A UI gesture
lands at the next block start (jitter: IPC + one block, inside the quarter-beat free-stop grace); MIDI
pedals carry a device frame. Every control-rate step Stage 3 adds (k-rate params per 128 frames, the
compressor's 32-frame divisions, LFO and envelope ticks) is anchored to the absolute frame counter:
process() splits a block at those boundaries and never ticks at the block start. No audio FIFO.

**Memory.** Everything allocated (and its pages touched) in Engine::new: 5 × (live + spare) + 1 free
buffer, 60 s × sample rate mono f32 (~127 MB at 48 k); the snapshot buffer arrives with export (Stage 5).
Undo keeps the one-level UNDO/REDO toggle: undo and redo swap a lane's live and spare. At an overdub's
start the spare is re-synced to the loop by a block job over the whole master, running ahead of the
layer's write head; the previous undo target waits in the free buffer, so a layer rejected for an
input gap gives back both the pre-layer loop and that target. COPY, a later take's tiling and a first
take's padding are block jobs too; Clear zeroes on write; Reverse is an index-mapping flag. A
sample-rate change builds a new engine off-thread and retires the old one off-thread (Stage 4).

**Decided while porting** (each test file header names what it changes): the count-in and an idle PLAY
start on the press frame, with no Web Audio scheduling lead; undo and reverse on a playing lane still
switch on the next loop boundary; Stop on an overdubbing lane discards the whole layer; an input gap
damages only the RETAKE pass it falls in, and resets AUTO's listening history; a jump in the device
frame counter drops the beats it skipped, count-in beats fire late as one click. A command is judged
when it is pressed, and a command that waits for a block job holds every later one behind it.

**Tests** (built, `src-tauri/crates/lf-engine/tests`): a rig (`tests/common`) drives the engine with
frame-coded input and frame-stamped commands, every `process` under `assert_no_alloc`. The 17 rig guards
map to click_grid (grid, accent-grid, pulse-forced-clamp), count_in (count-in, count-grid, bpm-lock),
phase_preserve, first_take (free-stop, free-record-cap, fixed-length, short-take), later_arm (later-arm,
looper-arm), overdub_undo_reverse (overdub, undo, reverse), retake, auto_record; record-compensation is
deleted, and align.rs asserts a take shifts by exactly align_frames. golden_jam.rs ports the golden jam
(assertions 1–6, 8, 9; 7 as an input gap; 10 stays a UI probe, its engine half, gates and the CLEAR
double press, is here) with absolute frames and the rendered output, at 44.1 k and 48 k, bit-identical
across block sizes 1, 32, 64, 127, 128, 480 and 1024. gestures.rs runs proptest scripts (one
recorder; a whole-bar master; every lane = master until F14/F16; undo twice = identity; undo after an
N-cycle dub gives back the pre-dub loop; finite output) at two block sizes, bit-identical, with
frame-stamped commands landing mid-block. cargo-mutants on grid and looper for acceptance, and again
when either changes (the planted-bug rule of `verify/README.md`, automated; no scheduled workflow).
Status (2026-09-24): 629 mutants, 95 survived the first run; new tests and removed dead conditions
leave 6, all equivalent (named here as the rule asks): the keep-last `written` and the commit's `raw`
minimum (the committed length does not move), an empty fill job at `lo == master`, a restore offset at
exactly the span's end, `plan_later_stop`'s bar clamp (the window end bounds it), `pair`'s ordering
(guarded by `assert_ne`).

**Acceptance.** `cargo test -p lf-engine` green on ubuntu and windows CI; deny check green; every rig
guard mapped or its deletion justified; golden jam green at 44.1 k and 48 k across all block sizes;
mutants killed or skipped with a reason; 5 lanes (one overdubbing) + click at 48 k / 64 frames under
10 % of block time offline (`src-tauri/crates/lf-engine/tests/perf.rs`, ignored by default: 0.17 % mean on the dev PC,
2026-09-24; 0.33 % with the master limiter wired, 3.8 to 4.0 % with the Stage 3 sound wired and idle,
2026-09-25). The live line is untouched.

## Stage 3 — synths and FX in lf-engine

*Owner: nothing to hear yet (STATUS E6 before the limiter port).*

**References (built 2026-09-25, Chromium 153, Tone 15.1.22).** `verify/probes/tone-refs.mjs` renders
Tone OfflineContext scenarios from the production modules with `Math.random` replaced by a seeded
mulberry32: two note scripts per pitched synth (range and velocity; a full-polyphony chord, a voice
steal, legato, bend and vibrato) and the drum kit, the FX over a parameter grid (with a mid-render param
change and a bypass crossfade), the limiter on a ramp plus bursts, and the reverb IR. 48 k plus a
44.1 k spot set, 26 float32 WAVs through the real `encodeWav`, 8.3 MB, in
`src-tauri/crates/lf-engine/tests/fixtures/tone` with `manifest.json` (scenario scripts, frame-stamped
events, every random draw, input hashes, capture commit and versions). The noise tables are not
stored: `dsp::noise` regenerates them from their seed, bit-exact. Without `--write` the probe re-renders
and compares (two renders differ by ≤ 1.8e-7: Blink sums a node's inputs in no fixed order). The
port harness is `src-tauri/crates/lf-engine/tests/common/refs.rs`. Stage 5's import fixtures come from
the sibling `verify/probes/export-refs.mjs` (UI, download and IndexedDB, not OfflineContexts): a 48 k,
240 BPM, one-bar, two-lane session captured as the Export button's download (float32 stems, PCM16 wet
master, session.json) and as autosave's IndexedDB recovery archive (float32 stems, session.json), 0.9 MB
with its manifest in `src-tauri/crates/lf-engine/tests/fixtures/v0.1.0`, whose zip layout
`src-tauri/crates/lf-engine/tests/v0_1_0_exports.rs` reads. Both probes hold the fixtures together to
10 MB; its compare ignores only the timestamps.

**Tolerance classes:** N = null residual ≤ −60 dB of the reference RMS; S = STFT bands within ±1 dB
and RMS envelope within ±0.5 dB. Each port takes the tightest class it passes, recorded in its test
(the manifest is regenerated whole). A failing test writes the Rust render to the fixtures' untracked
`out/` for an ear A/B.

**Ports, literal:** lead and piano (port Blink's band-limited PeriodicWave tables), pad (FM), organ
(AM), bass (MonoSynth + 24 dB lowpass + filter envelope + vibrato), drum kit (Membrane, Noise with the
fixture table), filter (two biquads, equal-power bypass), stutter, delay. **Hard:** MetalSynth (six
audio-rate FM squares + resonant highpass); PitchShift (two modulated delay lines, its latency
reproduced as heard, CPU budgeted); the convolution reverb (seeded IR with the same envelope and
normalization, Blink's partitioned convolver on RustFFT 6.4.1, the FFT Chromium 153 runs); the limiter (Blink's
DynamicsCompressorKernel: lookahead, knee, adaptive release, makeup). Blink ports are BSD-3: the
notice is in `THIRD-PARTY-NOTICES.md`; the installer carries it once the engine links.

**Built (2026-09-25),** in `src-tauri/crates/lf-engine/src/dsp` (module map in its `mod.rs`): every
scenario passes class N. Bit-exact: the limiter (E6's default, a literal port), the reverb IR (from
makeReverbBus's second `generate()`, draws 3 and 4), the filter, stutter and delay FX, the bypass
crossfade and the reverb bus (where RustFFT picks its AVX code, as it did for the reference). Near
exact: the lead, piano, organ and pad (−125 to −141 dB), the bass (−114 dB), the drum kit (−95 dB: the
metals' FM near Nyquist and Blink's biquad tail-stop), the hot delay (−776 dB: Blink's denormal flush)
and PitchShift (−69 to −72 dB, the nearest to the −60 dB bar: a 1-ulp wave-table difference moves its
float delay reads). Costs per 128-frame quantum at 48 k, release, dev PC: pad at 12 voices 85 µs, the
drum kit with all 16 voices ringing 367 µs (13.8 %), a chain with pitch on 18 µs, the reverb bus 47 µs
mean and 106 µs worst.

**Wired (2026-09-25)** in `src-tauri/crates/lf-engine/src/engine.rs`, with `src-tauri/crates/lf-engine/src/effects.rs` (each lane
through its chain, the shared reverb bus, the grid, CLEAR resetting and COPY carrying a lane's FX) and
`src-tauri/crates/lf-engine/src/instruments.rs` (all six built, one selected; a switch releases the
held notes and hands over the wheels, also to the same instrument; a tail rings out where the web
disposed a synth swapped within its slot). The stereo master bus carries the chains, the reverb bus, the instruments and the click under
the master volume, then the limiter; the wet signal joins after it under the same master volume,
unlimited (owner decision, 2026-09-25): the played instrument keeps today's native-monitor latency
instead of gaining the 6 ms pre-delay (288 frames at 48 k, 264 at 44.1 k), and like today's monitor it
is not limited. Everything on the bus is heard that much later and +0.57 dB louder under the threshold
(makeup gain), so a take's alignment is `align_frames` + inserts + limiter. The instruments are heard
and recorded, as on the web's looperInputBus. A note sounds one 128-frame quantum after it is applied
(`LEAD`; the web scheduled 5 ms ahead, and the ported synths need a look-ahead), and never waits
behind a looper command held for a block job; a wheel or an FX change sounds from the next quantum
boundary. The record path lags the instruments by the input latency plus the plugin's, less the lead
(`ProcessContext::input_frames`), so a note played on the heard click lands on the grid where a guitar
note does. A full command table leaves the rest in the ring for the next block instead of dropping
it. The synths, FX and reverb run on the device frame less the frames the device skipped, so their
blocks always follow each other; the limiter stays on the device frame. Two
literal Tone behaviours stay: a bypassed chain is not bit-transparent (about +0.035 dB), and a closed
stutter gate leaks about 0.2 % of the dry signal; the looper's tests therefore read `Taps::looper`, the
lanes before their FX. Tests: `src-tauri/crates/lf-engine/tests/sound.rs` (the instruments on the bus
and on the grid, a note during a held command, a burst of notes, the reverb's stereo tail, CLEAR/COPY,
the stutter on the grid across a restart, the whole wired sound bit-identical at seven block sizes); `src-tauri/crates/lf-engine/tests/align.rs` checks the take's sum against the click as it
leaves the limiter, `src-tauri/crates/lf-engine/tests/mixer.rs` that the output is limiter(bus) + monitor on both sides, bit for bit.

The acceptance run (`src-tauri/crates/lf-engine/tests/perf.rs`, ignored; dev PC, release, 2026-09-25,
five runs) goes through the wired engine: five lanes (one overdubbing) with every effect on (cutoff
ramping, delay feedback 0.95, send 1) into the bus, the click, the drum kit selected with all 16 voices
re-hit, the limiter; and beside it the other five synths with every voice sounding and the mod wheel
full, which a session never asks for (only the selected instrument takes notes). At 48 k / 64 frames:
mean 24.7 to 25.0 % of the 1333 µs block, the engine's share 18.2 to 18.4 %. The synths and FX compute
whole 128-frame quanta, so every other 64-frame block carries their work: those blocks average 47.8 to
48.4 %, the worst of the load's own cycle 51.9 to 52.6 %. p99.9 (79 to 80 %) and the worst block (89
to 127 %) are preemption on a busy desktop at normal priority. Idle, the wired sound lifts the Stage 2
bar from 0.33 % to 3.8 to 4.0 % (a one-off probe splits it about evenly between the bypassed chains
with the idle reverb bus and the six silent instruments): Blink renders a connected node whether or
not it sounds, and the port does the same. Rerun: `cargo test -p lf-engine --release --test perf -- --ignored --nocapture --test-threads=1`.

**Acceptance.** Fixtures within budget; every scenario passes its class; alloc and block-size tests
cover voices and FX (FFT paths ≤ −120 dB instead of bit-exact); six synths at full polyphony + full FX
on five lanes + reverb at 48 k / 64 frames under 50 % of block time offline (estimate).

## Stage 4 — device owner and plugins in the engine callback

*Owner: nothing to hear yet; everything audible waits for the Stage 5 lap.*

**Where** (built): a module, not a crate: `src-tauri/src/engine_io` (its `mod.rs` is the briefing)
holds the device owner, the callbacks, MIDI, and the join and share pipes (`pipes.rs`, reshaped from
`host/transport.rs`'s InPipe, OutMonitorPipe and DriftController, which the live line keeps until
Stage 6). lf-engine defines the SlotProcessor trait and the slots (`slots.rs`); the CLAP/VST3 host
stays in `src-tauri/src/host/` and implements it (`engine_slot.rs`, `clap_engine.rs`,
`vst3_engine.rs`).

**Module fates.** Reuse: `asio_startup.rs`, `host/scan.rs`, `host/editor_window.rs`,
`host/rt_alloc.rs`, the callback/controller/resize fixtures. `engine_io/owner.rs` is the process-wide
device owner, with its own transition kernel (`transition.rs`); the per-slot `host/native_io.rs` stays
for the live line and goes at Stage 6. `audio_output.rs`/`audio_input.rs` keep device lists, picks,
the ASIO cache and channel select; `host/clap.rs`/`host/vst3.rs` keep load, params, state and
editors; the engine-mode owners live beside the live ones until the flip deletes the live ones;
`host/commands.rs` moves arm/disarm/gain/latency to engine-host commands (Stage 5); the restart
fixtures assert "the engine kept rendering".

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
±400 ppm, MIDI parse and binding rules ported from TS. On the rig, `pnpm native:engine` runs the DEV
probe `app.exe --probe-engine` (`src-tauri/src/engine_io/probe.rs`): a 10-min soak with two plugins at
ASIO 128 (the amp-sim and Pro-Q), a loop on lane 0, then 20 backend and buffer switches and plugin
swaps while it plays. The bar: host RT allocs 0 and every diag counter 0 (callback gaps, ASIO
overloads, lock misses, duplex-order faults, join/share starves and trims, command-ring full), no
device event, the loop playing through every switch at an unchanged rate, every plugin back in its
slot, and the soak's output callbacks at p99.9 ≤ 50 %, max < 90 % of the period
(`EngineHost::block_load`). The existing plugin fixtures and `pnpm native:swap|survey|smoke|recall`
rerun. Not built (owner, 2026-09-25: the slim probe first): a scripted mode (`--script`, frame-coded
engine commands) that reruns the Stage 1 A/R/C/W/S bars on the engine; it comes only if the rig run
needs it. Stage 5's ported `native:loopback` measures the take against the click on the engine.

**Built (2026-09-25), dormant**, every part proven without hardware (fakes, in-process fixture
plugins, the real engine on a test thread): the slots and the punch-out in lf-engine
(`tests/slots.rs`, `tests/punch_out.rs`); the device owner, the callbacks, ASIO duplex, the WASAPI
join, Share output and native MIDI in `engine_io` (its `tests.rs` on a fake driver, the pipe matrix in
`pipes.rs`); ClapUnit, Vst3Unit and the engine-mode owners in `host/` (the restart fixtures load the
fixture plugins into a rendering engine). Code only a device or a real plugin can run is compile-checked
(`--features asio` too). Decided while building:

- **Slot routing follows today's web graph:** a live slot holding an effect (or nothing) takes the
  input and its output is the wet signal (after the limiter, recorded at the take's alignment); an
  instrument slot plays the notes while it is the note target (`SelectInstrument(NoteTarget::Slot)`)
  and joins the master bus, recorded where a built-in instrument is, less its own latency. A plugin
  hears a note on its frame (no `LEAD`); the wheels stay with the built-in synths (D12 later). The
  slots render ahead to the next slot command: one plugin call per device block. A unit crossfades
  in and out over 10 ms, counted in frames. With no device running, the thread that holds the engine
  stands in for the audio thread (a removal's NoteOffs ride one silent 1-frame process, then stop):
  unverified with real plugins.
- **Every stop of the device punches out** (a switch and a close as well as a loss, STATUS E3): a
  take or overdub in flight ends after the last rendered frame and commits as usual; a RETAKE roll
  with a kept pass commits that pass, even inside the grace where a stop would let the pass in flight
  finish (cut short, a many-bar take would floor a bar shorter).
- **A device at another sample rate builds a new engine:** the plugin units go back to their owners,
  who re-activate them at the new rate; the loops are lost, also when that device then fails to start
  (the restore builds a fresh engine). Same-rate switches and recoveries keep the loops in place. A
  unit installed with a rate a rebuild has since replaced goes back to its owner for re-activation.
- **A panic under the engine lock replaces the engine** at the same rate and restarts the device: the
  loops are lost, the units go back to their owners (`DeviceEvent::EngineFaulted`), at most once per
  10 s. A unit that panics on its way out is leaked with the old engine, never dropped off its owner.
- **A pedal's press frame** is the render position at its arrival plus one block: always the next
  block or later, applied on that frame, jitter-free; the UI's gestures land at the next block start.
- **The pipes:** a PullPipe's setpoint is at least the largest push plus the largest pull plus ~3 ms
  (at 15 ms a 10 ms ↔ 10 ms WASAPI join starves), so the join holds 25 ms and Share output
  max(20 ms, block + 13 ms). Their PI (ωn 0.1 rad/s, ζ 0.7, a 0.5 s low-pass on the fill) is faster
  than the live line's and not yet heard; a 600 s simulated matrix at ±400 ppm runs without a
  shortfall. cpal's `DeviceChanged` and `RealtimeDenied` are not faults (cpal 0.18.1 documents both as
  non-fatal); every other error is a loss.
- **Native MIDI** (midir 0.11, on windows 0.61): bindings key on port name + occurrence; WinMM input
  ports are exclusive, so Web MIDI and native MIDI cannot hold one controller at once (the Stage 5
  toggle picks one). Open for Stage 5: one note owner across the UI keys and MIDI (the web router
  merged them), and resending the wheels after an engine rebuild.
- **lf-engine builds as one codegen unit** (`src-tauri/Cargo.toml`): split, its DSP lost cross-unit
  inlining whenever a new module moved the partition, and the Stage 3 load read 33 % instead of 25 %
  with the synths' code untouched. Measured on the dev PC, release, 2026-09-25: Stage 2 idle 3.94 %,
  Stage 3 load mean 24.5 % (the engine 18.2 %).

Known limits, not built: a punch-out inside a take's last quarter-beat commits the whole bars before
it, where a stop there rounds up (owner's call); the dry signal steps without a ramp on a live toggle
and on an instrument installed into a live slot (web parity unknown; the lap's bypass stop hears it);
a pedal binding's port occurrence is recounted on every hot-plug, so two same-named controllers can
swap bindings; an ASIO period the driver drops without its overload report is not flagged (input and
output stay in step, the take is spliced there); the no-device removal path (a 1-frame process and `stop` on the plugin owner) has no
test with a real unit, and the CLAP restart fixture's thread check would flag it.

Still open in Stage 4: the fixes from the fan-out review of `25501f5..5ab8b4c` (four Opus readers,
2026-09-25; the owner let it replace the cross-family review). Before the owner plays on the engine:
- **Open deadlock (likely):** `engine_io/cpal_driver.rs` `start` (and the spike's `run_asio`) plays the
  input before building the output; on a first open cpal holds its `asio_streams` mutex through
  `create_buffers` (which calls `ASIOStop`) while the playing input's bufferSwitch takes the same
  mutex. Fix: build input, build output, play input, play output. Check whether the live line's
  `NativeIo` shares the order.
- **Orphaned unit (confirmed):** a unit returning after a `remove` timeout is `mem::forget`-ed while
  `occupied` stays true, and a later swap can hand the old unit to the next owner (`own()` checks the
  type only). Fix: an owner token per install; an orphan leaks.
- **Abandoned open closes the device (confirmed):** `owner.rs` runs `open` before checking `claimed`,
  then `stop(true)`, even for a channel-only request. Fix: check the claim first; restore the previous
  device.
- **Stuck notes with no device:** without a stamp, wheel/CC/NoteOff pass `midi/mod.rs` into the
  undrained 256-slot command ring; once full, a NoteOff drops after the router forgot the note.
Smaller, after the release: a parked unit never retried on a device change; `kNotImplemented` from
`setProcessing` read as refusal; slot/lane/master gain left subnormal after 0 (snap to target);
`tests/slots.rs:382` `<=` for `==`; each ASIO overload counted twice in `xruns`; an output lock miss
reads as a duplex fault; the panic hook allocates on the audio thread; a MIDI port back within one
1 s poll keeps a dead connection. Two live slots sum the dry input (+6 dB): whether more than one may
be live is a Stage 5 question. The plugin probes reran clean the same day (smoke 30 of 30; survey 32
`restartComponent`, all latency, as the baseline; swap 24 of 24 at 55–584 ms; recall 5 phases), and
the plugin fixtures pass in `pnpm rust:check`.

The rig run (`pnpm native:engine`, 2026-09-25, ASIO 128, Archetype Petrucci X and Pro-Q 3): the 600 s
soak is clean (206 717 callbacks, every counter 0, block time p99.9 < 30 %, max < 37 %); all 20
switches start and the loop plays through them and the 4 swaps; every check passes but the counters (10
`gaps`, 14 `duplex_faults`, 7 `engine.xruns`).

- `duplex_faults`: none on the first open. Every ASIO re-open logs a BadMode input build and its retry
  (`retry_on_asio`), and 14 of the 15 took exactly one fault: the input ran alone before the output's
  first callback. Fixed: a run's first output callback takes the input's count (`Render::block`).

- `gaps` and `engine.xruns`: none at ASIO 128/256 or in the soak. At ASIO 64, 4 gaps in 3 of 5 visits,
  each a callback just over 1.5 periods late followed by early ones (98, 78, 8 frames: three callbacks
  in under three periods), so no buffer was skipped, yet each counted a lost period, an xrun and a
  64-frame jump of the frame counter. On WASAPI, 6 gaps in the first 1.6 s after a switch: the three
  that delivered one period (n = 441, the padding at its usual 529) were the audio engine running late
  and catching up two callbacks later (n = 882), yet each counted a lost period, an xrun and a
  441-frame jump. The rule inferred a loss from one late interval. Fixed: the frame counter counts
  what the device took, and a late wake is no loss. ASIO infers nothing from timing (a dropped period
  arrives as the driver's overload, a cpal xrun); WASAPI counts a gap only when a callback finds its
  endpoint buffer empty, jumps by what played past it and drops as much join input, so takes stay
  aligned (`callback::dry_frames`; the old jump left that input in the ring).

- Rerun after both fixes (same day and setup): every check passes, every counter 0 over 224 763
  callbacks, 20 switches and 4 swaps; soak p99.9 < 33 %, max < 52 %; all 15 ASIO re-opens still retry
  their input build. The record phase opened with two callbacks at ≥ 199 % of the period (the next
  wakes 603 and 640 frames apart at 128, then quick ones), with no overload report from the driver.
  Cause unknown; not seen in the first run.

- Swaps: loads 7–78 ms; unloading Archetype while a second instance ran took 4 798 ms, 4 784 of them
  the plugin's own release, deactivate, terminate and module unload, with the slot dry meanwhile.

- One of the day's three probe launches hung on its first ASIO open: 0 callbacks, the open timed out at
  15 s and the owner thread did not stop; the next launch opened fine. Likely the open deadlock above
  (compare the live line's `arm_monitor` timeouts in `src-tauri/AGENTS.md`).

WASAPI on the Scarlett (muted `pnpm native:engine` runs, 2026-09-25, both plugins, 60 s soak): with no
other app on the microphone every check passes, input and output both at 44 100.7 Hz against QPC. With
a browser video call (and the Windows Settings app) holding the microphone, three runs saw 0 gaps but
15 `join_trims` each; the third measured the input pushing 0.87 % more frames than the output pulled
(44 482 vs 44 098 Hz), past what the join's controller (sized for ±400 ppm) holds, so the ring ran over
twice its setpoint every 3–6 s and each trim skipped ~25 ms of input. Unknown: whether those extra
frames are real time (a faster controller fixes it) or an artefact (resampling them shifts the pitch 15
cents); the output rate did not move, on the same device. An earlier run, also with a call on the
microphone, saw the reverse, 42 `gaps` (29 in the soak) and 9 `engine.xruns` but no trims, not
reproduced since. The output buffer holds 970 frames (2.2 periods of 441), so a callback up to 2.2
periods after the previous loses nothing. The probe prints a trace of each late wake, trim and the
join's rates (`trace` in `src-tauri/src/engine_io/callback.rs`).

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
- **Rust:** `host/transport.rs` (the WebView bridge: Hop1Pipe, the SharedBuffer ring, PaceTimer, the
  force-48k path; and the pipes `engine_io/pipes.rs` replaced); `host/native_io.rs`; the live owner
  mains, producer loops and RT spawn/join in clap.rs and vst3.rs; `audio_latency_probe.rs` and
  `marker_probe.rs` if nothing else uses them.
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
