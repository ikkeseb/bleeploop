# STATUS — the rig lap

The rig lap below is code-green and machine-proven where a machine can prove it. What is owed is the
owner's ear, eye or decision on the PC. This file is ONE ordered lap plus the decisions that block work — not a
backlog. Taste (feel, wording, placement; no functional risk) lives in `docs/backlog-taste.md` and is
NOT a gate. Non-gate code/product threads: `AGENTS.md` § Open threads.

**External tester intake:** `docs/plans/tester-feedback.md` holds the open reports and owner requests,
including audible short-track silence, plugin-switch freeze, export uncertainty and armed-click
behavior. These are not reproduced fixes or additional rig stops; prior machine proofs do not close them.

**Machine verification completed on Windows, 2026-09-19:** folder rename repaired (pnpm links and
Rust build caches); clean Git synced to origin. Check/build, Rust checks with/without ASIO, 25 Rust
tests and golden jam 78/78 passed; ASIO release built, started and closed normally. Browser probes
passed: export-context, recovery-transactions, recovery-close, recovery-playback, capture-clock,
overdub-window, render-cursor, capture-loss, master-latency and loop-end-stop (`verify/`). Five
60-second tracks saved without measured playback gaps. Silent native output probes at 44.1 kHz:
Focusrite ASIO 128/256 reported 8.55/14.44 ms; WASAPI median 12.00 ms; no stream faults or invalid
timestamps. These are output-driver reports, not physical guitar latency. This work is complete;
repeat affected checks after relevant changes or a new failure, rather than requesting this lap again.
The human checks below remain open; resume them when the owner chooses a rig session.

**Last play: 2026-09-18** (`pnpm dev:asio`, a free jam on two tracks; buffer and plugin: unknown).
Verdict: worked well; latency possibly a hair off — Stop 1 stays open, no stop
was walked as a yes/no. Previous rig lap: 2026-08-29 (ASIO, INPUT LIVE, guitar — produced the MIC
input-channel finding; what else it covered: unknown). **Still owed by ear: the whole lap below.**

## Play first

`pnpm dev:asio`, load the amp-sim, play a real jam **before reading any further**. Form an opinion and
write it down. If it feels off, that outranks every green check in this repo — say what felt wrong and
re-scope; do not defend the gates. This stands alone: it does not chain to the lap below.

## The rig lap — in plug order, each stop a yes/no

Sequenced so nothing is plugged or reconfigured twice. Mark each stop ✔ / ✘ with one line, then update
the "Last rig lap" line above. A stop dies when it passes; past 10 stops, consolidate or flag it (AGENTS.md).
Detail per stop is in § Stop detail below.

1. **ASIO · amp-sim live · buffer 256 — latency + the first take.** GO LIVE (VST3, Petrucci): wet
   audible, master fader scales the wet without changing recorded-loop level. Audio Settings → rec align
   retains any saved trim. Automatic compensation now includes measured limiter delay and reported
   native output latency, with paired browser render-clock sampling. Play and loop against the click; first take, overdub and FIXED 4 should keep
   their attacks and endings. Physical alignment after the software fixes remains unverified.
2. **Same rig · buffer 128.** C still the same per config; the `ASIO_MAX_BLOCK_FRAMES` 256→128 lever
   (~28 ms, crackle-risk — bump `INPUT_TARGET_SECONDS` a couple ms only if 128/64 crackles); switch to
   1024 once for the declick fade-up, back to 256.
3. **The take itself, at 256.** Count-in feels right (1 bar, accent on 1, no dead air at the head).
   FIXED 2 auto-stops on the downbeat after exactly 2 bars. Free record: play N bars, stop ~on the
   downbeat → reads "N bars"; try an early and a mid-bar stop. Click = transport mode: silent when idle,
   stops with stop-all, count-in still forced with click off. A completed later track starts
   IMMEDIATELY at master phase — no audible seam vs the recorded tail. Overdub punch-out: sustained
   note, punch out, listen to the layer seam. Undo boundary swap and reverse are click-free.
4. **Long session · grid.** Keep the same jam running 10+ min: no LED hop at commit, later takes sound
   on-grid, the commit-beat click is flam-free with the metronome on. With the metronome on, the
   loop's "1" is never silent right after a buffer/ASIO switch; retuning tempo mid-count-in is blocked;
   a free record past 60 s auto-closes on a bar (is that UX fine?).
5. **Reload + editors.** Load → GO LIVE → reload the WebView → re-load + re-arm OK (the reload wedge).
   Editor springs to front; close → reopen → disarm still no hang. Second editor on the same plugin
   FILE from the other slot: refused with a toast (wording → taste). With a FabFilter editor open,
   a drawer slider moves the editor's knob, and Pro-Q 3 Processing Mode → Linear Phase from the
   drawer restarts the plugin without a fault (controller mirror, machine-proven, not seen/heard).
   Pick another size in the editor's own size/zoom menu → the host window follows it, no clipped or
   floating view (resize contract, fixture-proven, not seen).
6. **Fault injection.** Yank/disable the armed interface while LIVE → within ~2 s three log layers
   (`cpal stream error` → `FAULTED (device lost)` → `fell back to the web monitor path`), a toast, and
   the wet audibly continues via the web path; reconnect + re-arm OK (ASIO holder released). Negative
   control: normal arm/disarm/editor use NEVER logs `owner-request cancelled` (provoke via a >5 s
   editor open if curious). Grep `[rec-comp] snapshot` per arm — does the generation-vs-settle
   `cpalOut=` split?
7. **MIC path + AUTO REC.** With a real interface: Audio Settings Ch 1 hears/records only physical
   input 1, Ch 2 only input 2. AUTO REC: muted-guitar noise floor must not arm; a real attack must —
   sensitivity, onset and click feel.
8. **Session files on the rig.** Remaining: import the exported zip through the native file picker
   and hear the restored grid; open a stem + the master in a DAW (master = wet render, honours master
   fader/mute). Native downloads and close/recovery are machine-verified below; dialog appearance
   and the real file-picker interaction were not automated.
9. **WASAPI A/B.** `pnpm dev:wasapi`, same jam: how much worse is the latency by ear? (Never A/B'd —
   ASIO has always been on.)
10. **MIDI controller — only if one is plugged (skip otherwise).** Unplug mid-note →
    toast + note release. Mod-wheel vibrato, pitch-bend, CC64 sustain feel.

## Decisions — five minutes each, no app open

Blocked on an owner decision, not on testing. The default column is what happens if nothing is said.

| # | Question | Default if silent |
|---|---|---|
| D1 | The BPM value ALREADY survives clearing every lane (only the lock and the loop LENGTH reset — `resetMaster` in `machine.ts`). Should the LENGTH survive too? It would force the next first take to the old bar count until CLEAR ALL. | stays as built |
| D14 | Lane state carries by hue: under deuteranopia REC red and PLAYING green read as the same yellow, in greyscale only the 8 px word and the core glyph separate them (shots and ΔE table in the 2026-09-22 audit; eye lines in `docs/backlog-taste.md`). Shape carrier (glyph in the left rail, larger word) or palette move? | stays as built |

**Answered 2026-09-23** (product lens over the 2026-09-23 audit; the promise now heads `README.md`):

- **D12 — not built** (the L controller IPC): MIDI into a plugin stays on the WebView latency path
  whatever the IPC carries (`native-io.ts` GO LIVE rejects a synth slot). Build instead: host-side
  sustain for native slots by dropping the `!activePlugin` guard on note-off deferral in
  `input-router.ts` (S). Limit: the plugin's own pedal behaviour never fires; bend and mod wheel stay
  built-in only.
- **D13 — build:** picking a synth or an instrument plugin in a slot makes it the MIDI slot; loading
  an effect plugin does not (`isEffect` comes from VST3 subCategories, tester F9). Today notes go into
  the amp sim when a synth is picked in B while A is active: silence with no explanation.
- **D15 — build, CHANGED MIRRORS reseed approved:** `startPlayback` rethrows a `src.start` failure
  inside the boundary `setTimeout` in `playback.ts`, so the re-arm never runs and the lane reads
  OVERDUBBING while later layers go into a buffer that never plays. A failed swap logs and returns
  the lane to a consistent state.

**Answered 2026-09-18:**

- **D2 / D7 — no:** guitar is recorded through the native input, never the mic/line path. The L3
  wizard is not built. More than one input channel: maybe later.
- **D3 — per-take `[rec-comp]` line logs in release builds too** (built, `record-latency.ts`); the
  snapshot line stays DEV-only.
- **D4 — stays disabled** (empty-lane right cluster). **D6 — (b):** teach the slot swap in UI/copy
  (line in `docs/backlog-taste.md`). **D9 — struck** (what was off: unknown).
- **D10 — COPY is built** (`copy` in `machine.ts`): the whole lane (loop, orientation, volume, mute,
  FX; not the undo history) lands in the first EMPTY lane, PLAYING in phase if the source plays.
  Golden-jam-proven; not heard or seen on the rig. Add-on idea, for later: copy with a delay offset
  and per-track pan (no pan exists today).
- **D11 — dropped**, all LOW, none hit in normal use.

## Stop detail

### Stop 1 — record-latency compensation (the rig gate)

The latest fixes address measured software causes: capture arms now use absolute render-frame
timestamps, compensated overdubs retain the complete punch window, and C includes measured limiter
delay plus driver-reported native output latency. The earlier claim that all tail loss came from a
single unstable C snapshot was too broad.

- **Formula:** with valid output timestamps, C is the median paired browser queue-tail presentation
  delay minus nativeOut, plus measured graph delay and trim, clamped at zero. Sampling keeps queue
  occupancy and render-cursor time together. The old `(hop1+hop2+128)/sr - nativeOut + clickOut`
  formula remains the unavailable-timestamp fallback, with graph delay and trim added.
  Native output retains monitor-ring residency plus the median valid callback-to-playback report;
  callback period is its startup/unsupported fallback.
- **Freeze:** first record use freezes the rolling-window median for the monitor generation. Re-arm,
  buffer changes and resnapshot reopen it. Saved trim remains live and is preserved; an old trim was
  tuned against older estimates and does not establish the new formula's accuracy.
- **Capture:** first and later takes use timestamped deadlines. Overdub collects through punch-out+C;
  STOP silences playback while that tail finishes. CLEAR cancels it. Capture loss rejects the damaged
  take or layer and preserves the earlier loop, resolving the former D8 choice in the implementation.
- **Unverified:** physical guitar alignment, converter latency, browser output reports, the fallback 128-frame
  allowance, native queue timing, and simultaneous native/synth source alignment. Native monitoring and buffering targets
  were not replaced.
- **Diagnostics:** `__lf.recordLatency.lastCompensation()` includes the measured graph term and
  `source` (`timestamp` or `reported`). The floor applies only to the reported fallback.
  `snapshot`/`resnapshot`, `setEnabled` and `setFloorEnabled` remain available. Audio Settings "rec align"
  controls the saved ±250 ms residual trim. Missing `[rec-comp]` during a guitar take means the
  native monitor was not registered; it does not prove zero latency.
- Master fader scales native wet without changing recorded level.

### Stop 3 — take mechanics

- **Count-in:** always-on 1 bar (free record stays the default). **Fixed-length:** known v1 — an extreme over-long
  bars/bpm pick silently records the largest whole-bar fit in 60 s.
- **Free-record wall-clock stop:** bars come from the wall clock (+ a quarter-beat grace for
  anticipated presses); an in-flight tail DEFERS the commit to the capture cap (frame-exact) and a
  play/stop press in that window is honoured after. Guard `fs-free-stop-verify.mjs` (OLD = 7 vs NEW = 8).
- **Click = transport mode** (owner's call): count-in still forced with click off; click during rec/play;
  SILENT when idle or all-stopped (beat-LED still runs) — kills the "accent on 2" wart.
- **Later track starts immediately at master phase** (resume() pattern) instead of waiting a boundary —
  closes the old finishLaterRecording join-boundary jitter item. **Overdub punch-out** now retains
  the timestamped compensated tail; the synthetic window probe verifies its final samples.
- **Undo** (one level, boundary-aligned swap, `fs-undo-verify.mjs`) and **reverse** (in-place, blocks
  overdub while reversed — RC-505 behaviour, `fs-reverse-verify.mjs`) ride the same grid discipline.

### Stop 4 — sync

The intermittent slip had a reproducible software mechanism: capture discarded a ring snapshot, then
counted from a later read of `ctx.currentTime`. The clock can advance within one JavaScript task.
An 8 ms forced interleaving reproduced a 512-frame early start with continuous PCM; absolute
timestamped capture removed the error in the same test. `verify/capture-clock.mjs` measures frame
zero, so a uniform slip cannot hide behind matching loop lengths.

The golden jam passes locally and on a clean GitHub runner (`golden-jam.yml`, on demand, twice green
2026-09-10 — the earlier loaded-runner failures no longer reproduce there), but a long native guitar
session is still unverified.

### Stop 5 — reload + editors

- **Reload wedge (FIXED):** a WebView reload used to strand Rust-held plugins ("slot N already has a
  plugin loaded"). `plugin_list_loaded` + `resyncNativeSlots()` in the app init chain unload strays via
  the production unload path; probe-proven both ways under WASAPI. Adopt-instead-of-unload across reload
  = future work noted in `instrument.ts`.
- **Dual-slot same-plugin editor (DIAGNOSED + GUARDED):** one plugin FILE = one loaded module = ONE GUI
  runtime whose message thread binds to the FIRST opener — a second editor on the same file wedges inside
  the plugin. NOT Neural-specific (Surge repro, both formats); mixed CLAP+VST3 of the same plugin is fine.
  Guard: `editorAffinity` in `src/audio/instrument.ts` + `PluginControls.toggleEditor`; affinity dies
  when the last slot holding the path unloads; `__lf` bypasses it (accepted).
- **Editor-to-front:** springs up in front at open, may drop behind when you click back into BleepLoop
  (intended, non-pinned).

### Stop 6 — fault paths

The stream-fault fallback: the three log
layers, the toast, the web-path continuation and the slot's "INPUT LIVE · WEB MONITOR" label were
runtime-probed on the web tier only.

### Stop 7 — MIC path

`MIC LIVE` reads Audio Settings Ch 1/Ch 2, requests enough discrete lanes, isolates only that
`ChannelSplitter` output, then centres the selected mono lane (`host.web.ts`, `capture.ts armInput`).
Machine proof: `fs-mic-input-channel-verify.mjs` 3/3 + a real Chromium stereo stream measured Ch 1
`0.000` / Ch 2 `0.177` RMS. Auto mode keeps the old advisory mono-sum. The Audio Settings **device** id
stays native cpal-only (browser `deviceId`s are origin-specific), so `MIC LIVE` opens the WebView default
input — a separate device-selection design, not claimed fixed. The MIC path is latency-uncompensated by
choice; its fix would be a loopback calibration wizard, not built (D2, D7).

### Stop 8 — session lifecycle

Export: `src/audio/export/` (master = WET stereo render through the real FX chains + limiter,
steady-state loopable PCM16; editable stems raw Float32; dry fallback flagged in `session.json`; master excludes STOPPED
tracks, stems include them; needs
≥1 committed track). Import: `src/audio/export/import.ts`, `machine.ts loadSession` — enabled only while
all-EMPTY; round-trip is byte-proven. Close guard: `lib.rs`, `app.tsx jamInProgress`,
`audio/autosave.ts` — web tier shows the browser's generic leave-page dialog instead. Local recovery
uses Float32 stems to preserve recorded samples exactly, including overdub headroom.

**First-session verification completed on Windows, 2026-09-19:** `verify/first-session.mjs`
recorded a real synth using UI controls in an empty browser profile, downloaded its zip, reopened
from automatic recovery and imported the file into a second empty profile. Both PCM hashes matched;
a malformed import was rejected and a later valid import succeeded; clear survived reload.
An isolated, temporarily instrumented ASIO release also delivered its zip to Downloads under the
production CSP; every sample of the downloaded stem matched the seeded loop. Real OS close events
with scripted confirm answers kept the window alive on Cancel and saved/closed on acceptance;
Reopening restored exact PCM. CLEAR ALL followed by an OS close before the normal two-second
autosave delay did not resurrect the earlier saved loop on reopening, and the empty
session did not request confirmation. Native input recording and perceived sound are outside this proof.
