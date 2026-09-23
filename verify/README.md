# `verify/` — deterministic regression guards

`pnpm verify` runs the deterministic `*-verify.mjs` guards in this directory. They check audio math,
file formats and repository contracts without a browser or audio hardware. They do not establish
that the running looper dispatches correctly or that the native app sounds right.

Browser probes run outside this command. `golden-jam.mjs` covers capture and dispatch;
`capture-clock.mjs` measures absolute capture frames under producer/consumer interleaving;
`overdub-window.mjs` checks compensated punch windows; `capture-loss.mjs` checks rollback after missing
packets; `master-latency.mjs` checks measured limiter delay;
`record-stop-window.mjs` checks first/later/AUTO/FIXED capture windows, padding, cancellation,
recorder release, playback-failure retry and the 60-second cap;
`overdub-timers.mjs` counts actual callback registration/cancellation through rapid stop/reuse,
compensated STOP, CLEAR, normal boundary rearming and a boundary swap whose source start fails (lane
lands STOPPED, logged once, no successor timer, PLAY restarts the committed loop);
`loop-end-stop.mjs` measures playback deadlines and UI;
`playback-restart.mjs` measures rendered lane PCM for single/ALL idle restarts from frame zero,
simultaneous lane starts, live-phase joins beside a muted lane pending END STOP, cold-graph starts
(first PLAY and COPY into a lane that never played, under an injected FX-build cost) and PLAY ALL
with one lane failing at source start or at buffer prep (the others still start, one toast);
`fx-grid.mjs` measures rhythmic effects;
`fx-pitch-cost.mjs` checks unused pitch allocation, offline DSP cost, and live enable/reset continuity;
`recovery-capacity.mjs`, `recovery-failure.mjs`, `recovery-worker.mjs` and `recovery-playback.mjs`
cover recovery fidelity, failure paths and main-thread load.
`recovery-transactions.mjs` injects actual IndexedDB transaction failures; `recovery-close.mjs` checks
close approval after a failed recovery deletion and a silent close during the first take (RECORDING,
nothing committed), with native close capabilities substituted.
`recovery-import-failure.mjs` checks archive preservation after failed restore reads, buffer allocation
and playback startup, rollback, retry, explicit clear and a live jam winning the restore race.
`monitor-generation.mjs` controls delayed host replies through the actual frontend monitor lifecycle,
including survivor promotion and a first take before its refreshed latency reply arrives.
`plugin-load-buffer-generation.mjs` checks late/failed plugin buffers and worklet-module retry through
the production frontend with an instrumented host, including capture warmup before the first note;
it does not exercise native COM cleanup.
`plugin-slot-pending.mjs` drives deferred plugin operations through the rendered picker and checks
pending controls, queue completion, failure recovery, independence of the other slot, and selection
of separate plugin files that share a class id.
`instrument-controls.mjs` checks keyboard-octave remounts, plugin gain/live fallbacks, empty scans and MIDI request failures.
`instrument-routing.mjs` checks that a failed unload aborts a plugin swap (old plugin kept, no load,
toast, retry completes), a failed clear keeps the plugin and a following synth pick does not activate
the slot, the swap is silent while unload runs, that picking a synth or instrument plugin moves the
MIDI slot while an effect does not, and host-side sustain on a plugin sink (deferred note-off,
re-strike order and release on a slot switch).
`input-controls.mjs` checks BPM cancellation, pointer release across octave changes, independent MIDI
ownership and the playable upper note range. `ui-state-carriers.mjs` checks transport, meter, lamp,
looper-announcement and toast state carriers, plus the looper refusal gates (a refused lane core's
title/label reason, and a refused Space announcing that reason with no state change).
`transport-focus.mjs` checks that Space/Enter/1–5 drive the looper behind an open popover and after
an Escape close, yet still yield to the BPM field and Tab-focused buttons. `session-state-roundtrip.mjs`
checks STOPPED recovery, state-only autosave and subsequent PLAY ALL. `mic-arm-race.mjs` substitutes the platform input open
with a deferred promise and checks that a disarm cancels a pending open (stream closed, tracks
stopped), that a burst of toggles ends in the state of the last gesture, and that a real browser-tier
open whose splitter wiring throws stops its tracks and disconnects its source. `transport-start.mjs`
injects one rejected `engine.start` and checks that the clock falls back to not running, logs once,
and the next gesture retries with the pulse and Tone transport started exactly once. These probes,
`recovery-import-failure.mjs`,
`monitor-generation.mjs`, `plugin-load-buffer-generation.mjs` and `asio-startup.mjs` run in
`.github/workflows/browser-lifecycle.yml` on frontend and verification changes. They do not gate the
physical rig or replace the separately dispatched golden jam.
`marker-probe.mjs` checks DEV marker correlation and clock arithmetic; it does not run native audio.
`render-clock.mjs` checks the DEV worklet observer preserves PCM; `render-cursor.mjs` exercises the
production compensation sampler with paired queue/timestamp observations, invalid clocks and freeze.
`export-context.mjs` covers live/export isolation, lossless editable downloads and bounded ZIP work;
`synth-note-ownership.mjs` and `midi-note-ownership.mjs` measure note release, voice reuse and input owners;
`audio-settings-startup.mjs` exercises actual frontend orchestration with an instrumented host;
`asio-startup.mjs` drives the ASIO startup coordinator's frontend half (saved-off never probes, saved-on
probes before the scan, blocked/failed offer RETRY, timed-out does not, not-compiled/flag hide the
toggle) — the native state machine itself is `cargo test` in `src-tauri/src/asio_startup.rs`;
`layout-reachability.mjs` measures rendered control access and canvas identity at desktop sizes, plus the
drum-pad ribbon, the two-row command-bar cap and the muted-lane readout with a loop present.
`transport-auto-layout.mjs` checks that AUTO toggling and sensitivity changes preserve command-bar
height and lane position from 960 to 1920 px, with screenshots at 1730 px.
`contact-sheet.mjs` screenshots fixed looper scenes (empty, count-in, first take, armed later take,
FX with five lanes, Help, Audio Settings) at 1280×820, 1920×1080 and 1000×700 with the keyboard bottom
and hidden into `logs/contact-sheet/` plus a tiling `index.html`, and fails on a clipped lane clear
button with FX open, rec-red in an armed lane's canvas, or any console error.
`first-session.mjs` drives real buttons, synth keys and file controls in fresh browser profiles:
record, download the zip, reopen from automatic recovery, retry a malformed import, import the
download into another profile and clear/reload. PCM hashes must match across both round trips.
It polls completed recovery reads rather than treating an asynchronous predicate as a saved result.
Rust unit tests run through `cargo test`; `.github/workflows/rust-test.yml` runs them on native-code
changes. Runtime verification and the Windows/WSL command lane are owned by `docs/VERIFY.md`.

`fs-docs-verify.mjs` is the docs guard: cited paths exist, cited shas resolve, the `STATUS.md` rig
lap has ≤ 10 stops. A dead path a doc keeps on purpose says so on the same line — "(now `…`)",
"not yet built", "upstream" — and the guard skips it.

Each `*-verify.mjs` runs in plain Node with **no browser, no `AudioContext`, no audio hardware**, and runs
the real source; none carries a hand-ported copy of it. There are three kinds:

- **Pure imports.** Modules with no Web Audio, Tone or timer dependency load directly via Node's TS
  type-stripping (`fs-quantize-verify.mjs` imports `../src/audio/quantize.ts`). The looper's grid
  arithmetic (`src/audio/looper/grid-math.ts`) and the compensation formula
  (`src/audio/record-latency-math.ts`) are kept pure for this.
- **Rig guards.** `verify/harness/rig.ts` (typechecked) drives the real looper: `bootLooper()` loads a fresh
  module graph of `src/audio` per scenario over a fake Web Audio + Tone layer, renders 128-frame quanta
  through the real capture worklet and fires the app's timers on the same audio clock. A guard presses
  `rig.looper.recDub(0)` and reads track state, PCM, started sources, clicks and LED beats. `rig.stall(s)`
  models a blocked main thread, `rig.renderAhead(n)` a producer ahead of the clock the main thread reads,
  `rig.import(path)` loads any other `src/` module of the same generation. `RIG_LOGS=1` echoes the app's
  console. It cannot show real render timing, browser jitter, WebView2 or anything audible.
- **Modules under the hooks.** A guard that imports `verify/harness/hooks.ts` can load any `src/` module
  (Solid, `import.meta.env`, extensionless imports) and gets a fresh copy per `?g=N` query, so module-load
  state re-runs (`fs-layout-store-verify.mjs`). Worklet processors load with a `registerProcessor` shim
  (`fs-capture-packets-verify.mjs`, `fs-worklet-pop-verify.mjs`).

A guard counts only once it went red on a deliberately planted bug in the code it claims to cover. A
bug no public path can reveal is an equivalent mutant; name it in the commit message.

Every deterministic guard prints a final line `=== RESULT: N/N checks passed, 0 failed ===` and exits non-zero on any
failure.

## Run

```bash
pnpm verify        # run all guards, summarized (a few seconds)
pnpm check         # typecheck + lint + boundary + verify (the full static gate)
pnpm exec node verify/fs-grid-verify.mjs   # run one directly
```

`pnpm verify` is part of `pnpm check`, so a regression in any guard fails the standard gate.

## Browser probes

With Vite running, use `pnpm exec node verify/loop-end-stop.mjs`, and substitute the other probe
filenames above as needed. These browser probes accept
`--url=http://localhost:1421` for a separate verification server started with
`pnpm exec vite --port 1421`. Use a fresh server if HMR has left dynamically imported probe modules
with a different identity from the app's modules. `pnpm verify:jam` starts or reuses port 1420.

`golden-jam.mjs` also accepts `--url` for an already-running separate server.

## Add a guard

1. Create `verify/<name>-verify.mjs` (the `-verify.mjs` suffix is how `run-all.mjs` discovers it).
2. Run the real code: import a pure module directly; drive anything that touches Web Audio, Tone, timers
   or the looper through the rig or the hooks. When the logic you want reads `engine.ctx`, `clock.bpm()`
   or `engineState` inline and a rig scenario cannot reach it, extract the math into `grid-math.ts` or
   `record-latency-math.ts` (the source calls it with the live values) and import it. Never copy source
   logic into a guard.
3. Track checks with a `passed`/`failed` counter, print the `=== RESULT: N/N checks passed, M failed ===`
   line, and `process.exit(failed === 0 ? 0 : 1)`.
4. Plant realistic bugs in the covered code; each must turn the guard red. Revert them.
5. Run `pnpm verify` to confirm it's picked up and green.

## Why these are tracked

Tracking the guards lets a fresh clone on either development machine re-run the same checks.
Keep browser probes tracked too; deterministic math checks and runtime measurements cover different
failure modes.
