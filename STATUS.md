# STATUS — the rig lap

The owner's ear, eye or decision on the PC: ONE ordered lap plus the decisions that block work. Taste:
`docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md` § Open threads. Tester reports:
`docs/plans/tester-feedback.md` (not rig stops; machine proofs do not close them). Direction since
2026-09-24: one native audio engine (`docs/plans/native-engine.md`); this lap serves the shipping
line until its flip.

**Machine verification, Windows, 2026-09-19:** check/build, Rust checks and tests, the golden jam, an ASIO release and the browser probes (`verify/`) all passed.
Driver latency reports are not guitar latency; after a relevant change, rerun only the affected check.

**Last play: 2026-09-24** (`pnpm dev:asio`, jam, two–three tracks, no pedal): the click too quiet at full
volume (built: twice the level); loops audibly out of sync with the click, worse after STOP → PLAY ALL
(measured, see Stop 1); END STOP unclear (`docs/backlog-taste.md`); no stop walked.

## Play first

`pnpm dev:asio`, load the amp-sim, play a real jam **before reading further**, write the opinion down.
If it feels off, that outranks every green check: say what felt wrong and re-scope.
With a MIDI footswitch plugged: learn REC/DUB onto it (Audio Settings → midi learn, one tap) and take
the jam's records with the foot. One press, one action? Still learned after the next restart?

## The rig lap — in plug order, each stop a yes/no

Nothing is plugged or reconfigured twice. Mark each stop ✔ / ✘ with one line and update "Last play".
A stop dies when it passes; past 10 stops, consolidate or flag it (AGENTS.md). Detail: § Stop detail.

1. **ASIO · amp-sim live · buffer 256 — latency + the first take.** GO LIVE (VST3, Petrucci): wet
   audible; master fader scales the wet, not the recorded level; rec align keeps a saved trim. Loop
   against the click: first take, overdub and FIXED 4 keep their attacks and endings.
2. **Same rig · buffer 128.** C still the same per config; switch to 1024 once for the declick fade-up,
   back to 256.
3. **The take itself, at 256.** Count-in feels right (1 bar, accent on 1, no dead air). FIXED 2
   stops on the downbeat after exactly 2 bars. Free record: stop ~on the downbeat after N bars → "N
   bars"; try an early and a mid-bar stop. Click: silent when idle, stops with stop-all, count-in still
   forced with click off. A later track starts at master phase with no seam against its tail. Punch
   out of a sustained note: is the layer seam clean? Undo swap and reverse are click-free.
4. **Long session · grid.** Same jam, 10+ min: no LED hop at commit, later takes on-grid, a flam-free
   commit-beat click. Metronome on: the loop's "1" is never silent after a buffer/ASIO switch; tempo
   is locked mid-count-in; a free record past 60 s auto-closes on a bar (is that UX fine?).
5. **Reload + editors.** Load → GO LIVE → reload the WebView, then close and reopen the app → the
   plugin is back each time, not live, and one GO LIVE re-arms. Editor in front; close → reopen →
   disarm, no hang. Second editor on the same plugin FILE from the other slot: refused with a toast.
   FabFilter editor open: a drawer slider moves its knob, and turning the knob moves the slider, also
   after a close → reopen; Pro-Q 3 Processing Mode → Linear Phase from the drawer restarts it without
   a fault. The editor's own size/zoom menu → the host window follows, nothing clipped or floating.
6. **Fault injection.** Yank the armed interface while LIVE → within ~2 s three log layers
   (`cpal stream error` → `FAULTED (device lost)` → `fell back to the web monitor path`), a toast, the
   wet continues via the web path; reconnect + re-arm OK. Negative
   control: normal use NEVER logs `owner-request cancelled`.
7. **MIC path + AUTO REC.** Audio Settings Ch 1 records only physical input 1, Ch 2 only input 2.
   AUTO REC: a muted-guitar noise floor must not arm, a real attack must (sensitivity, onset, feel).
8. **Session files on the rig.** Import the exported zip through the native file picker and hear the
   restored grid; open a stem + the master (wet, honours master fader/mute) in a DAW.
9. **WASAPI A/B.** `pnpm dev:wasapi`, same jam: how much worse is the latency by ear?
10. **MIDI controller — only if one is plugged (skip otherwise).** Unplug mid-note →
    toast + note release. Mod-wheel vibrato, pitch-bend, CC64 sustain feel.

## Decisions — five minutes each, no app open

Blocked on an owner decision, not on testing. The default column is what happens if nothing is said.

| # | Question | Default if silent |
|---|---|---|
| D1 | The BPM value ALREADY survives clearing every lane (only the lock and the loop LENGTH reset — `resetMaster` in `machine.ts`). Should the LENGTH survive too? It would force the next first take to the old bar count until CLEAR ALL. | stays as built |
| D14 | Under deuteranopia REC red and PLAYING green still read as nearly the same yellow. Since the 2026-09-23 restyle a live capture is also a FILLED badge and a lit core face, and PLAYING a lit LED, so greyscale separates them by shape. Enough, or a palette move as well? | stays as built |
| D16 | Host a browser demo on Cloudflare Pages? It contradicts "the browser tier is a verification rig". | no |
| D17 | `LICENSE` and `authors` in `src-tauri/Cargo.toml` carry the GitHub handle (the no-names rule targets prose). Keep, or use a role? | stays as built |
| E2 | If the Stage 1 silent-share test fails: Share output goes to a user-picked endpoint, or rely on OBS/Discord app capture only? | user-picked endpoint |
| E3 | Engine: a take recording when the audio device drops (needed by Stage 4) | punch out at the last frame, keep it |
| E4 | Engine import needs a native file dialog: add `tauri-plugin-dialog` (Stage 5)? | yes |
| E5 | v0.1.0's recovery records (IndexedDB) at the engine upgrade: drop with a release note, or hand the bytes over once (Stage 5)? | drop, release note |
| E6 | A true 0 dBFS ceiling in the ported limiter (Stage 3), or a literal port of today's? | literal port |

**Answered 2026-09-24** (`docs/ARCHITECTURE.md` § Decided: one native audio engine):

- **D18 — no calibration build:** the native engine drops rec align; on the shipping line the owner
  sets rec align +69 by hand.
- **E1 — publish v0.1.0:** yes, once the CI-built exe is heard on the rig (the gating on its exact
  commit is in `build-exe.yml`).
- **E7 — v0.1.0 rec align:** ships at 0; the release notes say to set it by ear.

**Answered 2026-09-23** (product lens over the 2026-09-23 audit; the promise now heads `README.md`):

- **D12-S — built:** host-side sustain and release of pedal-held notes on plugin sinks, verified by
  `verify/probes/instrument-routing.mjs`; the plugin's own pedal behaviour never fires; bend and mod wheel
  stay built-in only.
- **D13 — built:** picking a synth or instrument plugin makes its slot the MIDI slot; an effect plugin
  does not, verified by `verify/probes/instrument-routing.mjs`.
- **D15 — built:** a failed overdub boundary swap keeps the layer and stops the lane, verified by
  `verify/probes/overdub-timers.mjs --case=swapFail` and `verify/guards/overdub.mjs`.

**Answered 2026-09-18:**

- **D2 / D7 — no:** guitar records through the native input, never the mic/line path; no L3 wizard; more input channels maybe later.
- **D3 — built:** the per-take `[rec-comp]` line logs in release too (`record-latency.ts`); the snapshot line is DEV-only.
- **D4 — stays disabled** (empty-lane right cluster).
- **D6 — (b):** teach the slot swap in UI/copy (line in `docs/backlog-taste.md`).
- **D9 — struck** (what was off: unknown).
- **D10 — COPY built** (`copy` in `machine.ts`): the lane to the first EMPTY lane, in phase; not seen or heard.
- **D11 — dropped:** all LOW, none hit in normal use.

## Stop detail

### Stop 1 — record-latency compensation (the rig gate)

The formula C, its freeze and why native monitoring cancels input+plugin latency: the header of
`src/audio/record-latency-math.ts`. A saved trim stays live but predates the current formula.

- **Capture:** timestamped windows (`docs/ARCHITECTURE.md` § Audio architecture); overdub collects
  through punch-out+C (STOP silences playback meanwhile, CLEAR cancels).
- **Measured 2026-09-24 through a loopback cable** (`pnpm native:loopback`, baseline in
  `docs/VERIFY.md`): at trim 0 a perfectly timed hit lands ~65 ms late on this rig (the WebView
  output's real latency is above what it reports); since the worklet reads the bridge ring directly,
  rec align +60 leaves +8..+12 ms in six launches of seven, so +69 is this rig's value (−8 ms in the
  seventh: not pursued, the engine removes the bridge). The native round trip a guitarist hears is 44 ms at buffer 256 (WASAPI: ~280 ms).
  The bridge queue is the record path's delay: the worklet holds it on one setpoint and settles it
  back after any step, and C follows its smoothed shift since the freeze
  (`src/audio/worklets/plugin-pcm-source.ts`, `record-latency.ts`). Inside a take the offset drifts
  0.7–3.2 ms/min in most launches. No further bridge work: the engine replaces it
  (`docs/plans/native-engine.md`).
- **Unverified:** converter latency on its own, the fallback 128-frame allowance, simultaneous
  native/synth source alignment, whether the ~65 ms holds across restarts, buffer sizes and output
  devices.
- **Diagnostics:** `__lf.recordLatency` `lastCompensation()` (graph term; `source` `timestamp` or
  `reported`, the floor applies to `reported` only), `snapshot`/`resnapshot`, `setEnabled`,
  `setFloorEnabled`; "rec align" = the saved ±250 ms trim. No `[rec-comp]` line on a guitar take =
  the native monitor was not registered.

### Stop 3 — take mechanics

- One grid for count-in, undo and reverse: `src/audio/AGENTS.md` § One grid. Known v1: an over-long
  FIXED bars/bpm pick silently records the largest whole-bar fit in 60 s.
- **Free-record stop:** bars come from the wall clock with a quarter-beat grace (`planFreeStop` in
  `src/audio/looper/grid-math.ts`); a press while the tail is in flight is honoured after the commit.
- **Click = transport mode** (owner's call): forced for the count-in, on during rec/play, SILENT when
  idle or all-stopped (the beat-LED still runs).

### Stop 4 — sync

The capture-clock slip and its fix: `docs/ARCHITECTURE.md` § Audio architecture (Looper capture),
probe `verify/probes/capture-clock.mjs`. The golden jam is green locally and in `golden-jam.yml`; a long
native guitar session is unverified.

### Stop 5 — reload + editors

- **Reload wedge:** wrong answer = a "slot N already has a plugin loaded" toast after reload
  (`resyncNativeSlots()` unloads strays; probe-proven under WASAPI).
- **Same file in both slots:** only the first opener gets an editor (why: the comment at
  `editorAffinity` in `src/audio/instrument.ts`).
- **Editor-to-front:** dropping behind after a click into BleepLoop is intended.
- Sustain down, re-strike a note on a VST/CLAP instrument: does it sound? (Its note-off and note-on
  are two unordered invokes from `input-router.ts`.)

### Stop 6 — fault paths

Probed on the web tier only. After the fault the slot reads "INPUT LIVE · WEB MONITOR".

### Stop 7 — MIC path

Channel routing: `docs/ARCHITECTURE.md` § Audio architecture (Looper capture), proof
`verify/guards/mic-input-channel.mjs`. `MIC LIVE` opens the WebView default input, not the Audio Settings
device (native-only; browser `deviceId`s are origin-specific) — a separate design. Uncompensated by
choice (D2, D7).

### Stop 8 — session lifecycle

Formats: `docs/ARCHITECTURE.md` § Audio architecture. The master excludes STOPPED tracks, stems
include them; import works only while every lane is EMPTY.

**Machine-verified, Windows, 2026-09-19:** `verify/probes/first-session.mjs` round-tripped a take through download, recovery and import (PCM hashes matched); an ASIO release's downloaded stem was sample-exact.
OS close events kept the window on Cancel, saved on accept, never resurrected a cleared jam. Dialogs and the file picker were not automated.
