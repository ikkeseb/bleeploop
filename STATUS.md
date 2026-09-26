# STATUS — the rig lap

The owner's ear, eye or decision on the PC: ONE ordered lap plus the decisions that block work. Taste:
`docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md` § Open threads. Tester reports:
`docs/plans/tester-feedback.md` (not rig stops; machine proofs do not close them). Since v0.1.0 the
app runs on the native engine by default (`docs/plans/native-engine.md` § Stage 5); this is the
engine lap. The web path stays one switch away (Audio Settings → engine) until Stage 6.

**Machine verification, Windows, 2026-09-26 (engine mode):** `pnpm rust:check` 4/4 (438 tests), `pnpm check`
(40 guards) and `pnpm build` green; the browser probes 43/43 (`pnpm probe --ci`); on the rig (Scarlett
2i2, ASIO 128, loopback cable) `native:engine-smoke` with Pro-Q, `native:engine-recovery` (export,
import, recovery after a kill, real audio) and three same-size relaunches landing within 0.1 ms of
the driver's report; the web path's `native:smoke` in its own profile. Later that day:
`native:engine-loopback` (six launches, ASIO 64/128/256) and the release build on the engine,
`release:smoke`, in a fresh profile. In the evening, after multiply, IN FX, the stage view and
Help's diagnostics landed: `pnpm rust:check` (462 tests), `pnpm check`, `pnpm build`, the browser
probes 45/45; on the rig `native:engine-smoke` with Pro-Q and `native:engine-loopback` with its
multiply phases (three launches, 30/30 bars at 64/128/256), and MIC's gain after a plugin unload
measured through the cable.
Driver latency reports are not guitar latency; after a relevant change, rerun only the affected check.

**Last play: 2026-09-24** (web path, `pnpm dev:asio`, jam, two–three tracks, no pedal): the click too
quiet at full volume (built: twice the level); loops audibly out of sync with the click, worse after
STOP → PLAY ALL; END STOP unclear (`docs/backlog-taste.md`); no stop walked. The engine has not been
played by ear yet.

## Play first

Install the v0.1.0 draft (GitHub → Releases → the draft's installer), or `pnpm dev:asio`. Audio
Settings: ASIO, buffer 128, the guitar's input channel. Load the amp-sim, GO LIVE, play a real jam
**before reading further**, write the opinion down. If it feels off, that outranks every green check:
say what felt wrong and re-scope. If it feels right, publish the draft.
With a MIDI footswitch plugged: learn REC/DUB onto it (Audio Settings → midi learn, one tap) and take
the jam's records with the foot. One press, one action? Still learned after the next restart?
The draft is v0.1.0; what landed after it (multiply, IN FX, stage view, Help's diagnostics) runs from
`pnpm dev:asio` and is folded into Stops 1 and 3 below.

## The rig lap — in plug order, each stop a yes/no

Nothing is plugged or reconfigured twice. Mark each stop ✔ / ✘ with one line and update "Last play".
A stop dies when it passes; past 10 stops, consolidate or flag it (AGENTS.md). Detail: § Stop detail.

1. **ASIO 128 · amp-sim live — feel and the first take.** GO LIVE (VST3, Petrucci): the guitar through
   the amp feels immediate; master fader scales the wet, not the recorded level. Loop against the
   click with no trim: first take, overdub and FIXED 4 keep their attacks and endings on the click.
   IN FX (after MIC): ECHO and REVERB on the guitar still feel immediate, the echo sits on the tempo,
   and a take recorded with them sounds as it did live.
2. **Same rig · buffer 256, then 64.** The take still lands on the click at each size; the switch
   gap is short; loading a plugin while loops play crossfades in without a click.
3. **The take itself, at 128.** Count-in feels right (1 bar, accent on 1, no dead air). FIXED 2
   stops on the downbeat after exactly 2 bars. Free record: stop ~on the downbeat after N bars → "N
   bars"; try an early and a mid-bar stop. Click: silent when idle, stops with stop-all, count-in still
   forced with click off. A later track starts at master phase with no seam against its tail. Punch
   out of a sustained note: is the layer seam clean? Undo swap and reverse are click-free. Multiply:
   over a 1-bar loop, FIXED 4 on another lane: the loop becomes 4 bars at the commit and the first lane
   plays on with no seam or click there.
4. **Long session · grid.** Same jam, 10+ min: loops and click stay tight, no LED hop at commit,
   later takes on-grid, a flam-free commit-beat click; tempo is locked mid-count-in; a free record
   past 60 s auto-closes on a bar (is that UX fine?).
5. **Reload + editors.** Load → GO LIVE → close and reopen the app → the plugin is back, not live, and
   one GO LIVE re-arms. Editor in front; close → reopen, no hang. FabFilter editor open: a drawer
   slider moves its knob and back; the editor's own size menu → the host window follows.
6. **Fault injection.** Yank the interface while loops play → a toast, the loops and the plugin stay;
   reconnect → the same device comes back (or WASAPI takes over) and the loops play on.
7. **Inputs + AUTO REC.** Audio Settings Ch 1 records only physical input 1, Ch 2 only input 2.
   AUTO REC: a muted-guitar noise floor must not arm, a real attack must (sensitivity, onset, feel).
8. **Session files + Share output.** Export, CLEAR ALL, import the zip: the loops come back on the
   grid; open a stem and the master in a DAW (the master's FX come from the web path's render: close
   enough?). Kill the app mid-jam → relaunch restores it. Share output → OBS, Chrome and Discord hear
   the master.
9. **Synths, FX and WASAPI.** The six synths and the lane FX against the web path (Audio Settings →
   engine → web audio, restart): same character? Then WASAPI: how much worse is the latency by ear
   (takes land late there by design, `README.md`)?
10. **MIDI controller — only if one is plugged (skip otherwise).** Unplug mid-note →
    toast + note release. Mod-wheel vibrato, pitch-bend, CC64 sustain feel.

## Decisions — five minutes each, no app open

Blocked on an owner decision, not on testing. The default column is what happens if nothing is said.

| # | Question | Default if silent |
|---|---|---|
| D1 | The BPM value ALREADY survives clearing every lane (only the lock and the loop LENGTH reset). Should the LENGTH survive too? It would force the next first take to the old bar count until CLEAR ALL. | stays as built |
| D14 | Under deuteranopia REC red and PLAYING green still read as nearly the same yellow. Since the 2026-09-23 restyle a live capture is also a FILLED badge and a lit core face, and PLAYING a lit LED, so greyscale separates them by shape. Enough, or a palette move as well? | stays as built |
| D16 | Host a browser demo on Cloudflare Pages? It contradicts "the browser tier is a verification rig". | no |
| D17 | `LICENSE` and `authors` in `src-tauri/Cargo.toml` carry the GitHub handle (the no-names rule targets prose). Keep, or use a role? | stays as built |
| E3 | Engine: a take recording when the audio device drops | punch out at the last frame, keep it |
| E4 | Engine import through a native file dialog (`tauri-plugin-dialog`), or the WebView's file picker as today? | the WebView's picker |
| E6 | A true 0 dBFS ceiling in the ported limiter, or a literal port of today's? | literal port |
| E8 | Engine sessions: the snapshot's PCM crosses to TS once per save, so today's zip/WAV/recovery code stays (agents' call, 2026-09-26). Keep, or Rust writes the files? | keep |
| E9 | WASAPI takes land late on drivers that hide their buffering (~215 ms on the Focusrite; `docs/plans/native-engine.md` § Stage 1 W1). Accept as documented, or build the one reported term (~40 ms, invisible on this rig)? | accept |
| E10 | Multiply (F14) is FIXED past the loop only. Should a free later take (FIXED off) also run until the press and grow the loop, as the first take does, instead of closing at the loop length? | FIXED only |

**Answered 2026-09-26:** the 12 ms ASIO launch is not a stop (cause found and fixed: the engine opens
ASIO at another block size first); the first release is v0.1.0, on the engine (E1, E5 and E7 lapse);
E2 is built as its default (Share output to a user-picked endpoint).

**Answered 2026-09-24** (`docs/ARCHITECTURE.md` § Decided: one native audio engine):

- **D18 — no calibration build:** the native engine drops rec align.

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
- **D10 — COPY built**: the lane to the first EMPTY lane, in phase; not seen or heard.
- **D11 — dropped:** all LOW, none hit in normal use.

## Stop detail

### Stops 1–2 — alignment on the engine

A take starts the driver's reported input plus output latency (plus the plugin's latency and the
limiter's pre-delay) after its downbeat (`ProcessContext::align_frames`, lf-engine `api.rs`); there is
no trim. On the dev rig (Scarlett 2i2, loopback cable) the report holds within 0.1 ms at ASIO 64, 128
and 256, once the engine opens the driver at another block size first: a relaunch at the size the
driver last ran otherwise lands about two periods late (`docs/plans/native-engine.md` § Stage 1, Cause
and fix). Round trip with Pro-Q: 8.1 ms at 64, 15.1 ms at 128, 26.8 ms at 256. In the running app
through the cable (`pnpm native:engine-loopback`, six launches): the take lands within 0.12 ms of the
click at 64, 128 and 256, a loop re-recorded from playback adds no error of its own, and STOP ALL →
PLAY ALL keeps the click, its accent and the loops in place. A cable is a perfect player; whether a
guitarist's take feels on the click is this stop.

### Stop 3 — take mechanics

- One grid for count-in, undo and reverse (lf-engine `looper.rs` and `grid.rs`). Known v1: an
  over-long FIXED bars/bpm pick records the largest whole-bar fit in 60 s.
- **Free-record stop:** bars come from the device clock with a quarter-beat grace; a press while the
  tail is in flight is honoured after the commit.
- **Click = transport mode** (owner's call): forced for the count-in, on during rec/play, SILENT when
  idle or all-stopped (the beat-LED still runs).

### Stop 5 — reload + editors

- **Same file in both slots:** only the first opener gets an editor (why: the comment at
  `editorAffinity` in `src/audio/instrument.ts`).
- **Editor-to-front:** dropping behind after a click into BleepLoop is intended.
- One slot is live at a time on the engine (two would sum the dry input twice).

### Stop 6 — fault paths

The engine's device owner (`src-tauri/src/engine_io/owner.rs`): a lost device keeps the engine, its
loops and its slots, tries the same device again, then falls back to WASAPI's defaults; the UI toasts
each step. Proven on the fake driver (`engine_io/tests.rs`), not by a yank on the rig.

### Stop 8 — session lifecycle

Formats: the zip layout and `session.json` as before (`docs/ARCHITECTURE.md` § Audio architecture);
on the engine the PCM comes from `engine_snapshot` (`docs/plans/native-engine.md` § Stage 5, Session).
The master excludes STOPPED tracks, stems include them; import works only while every lane is EMPTY.
Recovery starts once a device runs.
