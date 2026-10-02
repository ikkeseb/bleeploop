# STATUS: the rig lap

The owner's ear, eye or decision on the PC: ONE ordered lap plus the decisions that block work. Taste:
`docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md` § Open threads. The app runs one
native audio engine (`docs/ARCHITECTURE.md`); this is the engine lap.

**Machine verification (Windows):** the push gates are `AGENTS.md`'s. The rig probes, when to run
each, and their baselines and latest readings: `docs/VERIFY.md` § When to run the plugin probes.
Last rig pass, 2026-09-29 (the engine-only app): `native:smoke`, `survey`, `swap`, `recall`,
`tone-recall`, `engine-smoke`, `engine-recovery` and `release:smoke` green; `native:engine-loopback`
20/20 at 64 and 256, 18/20 at 128 (a detector reading v0.1.0 repeats). Since then (2026-10-01):
`native:export-master` (WASAPI) for the export's engine-rendered master; the sample-rate pick on
ASIO, `native:engine-smoke` at 48 and 44.1 kHz and `native:engine` after the switches. Owed once the
loopback cable is in: `native:engine-loopback --rate=48000` (alignment at 48 kHz). Driver latency
reports are not guitar latency; after a relevant change, rerun only the affected check.

**Last play: 2026-09-28** (engine, a local release build, ASIO; a second player on a WASAPI build
of their own): worked well overall, no issue found; the WASAPI player heard delay on DI monitoring
(`src-tauri/AGENTS.md` § Open threads); no stop marked.

## Play first

Install the latest release from GitHub → Releases (CI-built, ASIO SDK 2.3.4 where local builds use
2.3.3), or `pnpm dev:asio`. Audio Settings: ASIO, buffer 128. Load the amp-sim, pick the guitar's
input on its slot, GO LIVE, play a real jam
**before reading further**, write the opinion down. If it feels off, that outranks every green check:
say what felt wrong and re-scope.
With a MIDI footswitch plugged: learn REC/DUB onto it (Audio Settings → midi learn, one tap) and take
the jam's records with the foot. One press, one action? Still learned after the next restart? Then
switch it to HOLD and hold it through one overdub.

## The rig lap: in plug order, each stop a yes/no

Nothing is plugged or reconfigured twice. Mark each stop ✔ / ✘ with one line and update "Last play".
A stop dies when it passes; past 10 stops, consolidate or flag it (AGENTS.md). Detail: § Stop detail.

1. **ASIO 128 · amp-sim live: feel and the first take.** GO LIVE (VST3, Petrucci): the guitar through
   the amp feels immediate; master fader scales the wet, not the recorded level. Loop against the
   click with no trim: first take, overdub and FIXED 4 keep their attacks and endings on the click.
   IN FX: ECHO and REVERB on the guitar still feel immediate, the echo sits on the tempo,
   and a take recorded with them sounds as it did live. RING MOD (never heard): its Freq range
   20–1500 Hz and 440 Hz default are an agent's pick: keep, or name what to change.
2. **Same rig · buffer 256, then 64, then 48 kHz.** The take still lands on the click at each size;
   the switch gap is short; loading a plugin while loops play crossfades in without a click. Sample
   rate → 48 kHz with loops playing: the confirm asks, the driver reopens at 48 kHz (the select says
   so), and a new take there lands on the click and sounds as clean as at 44.1.
3. **The take itself, at 128.** Count-in feels right (1 bar, accent on 1, no dead air). FIXED 2
   stops on the downbeat after exactly 2 bars. Free record: stop ~on the downbeat after N bars → "N
   bars"; try an early and a mid-bar stop. Click: silent when idle, stops with stop-all, count-in still
   forced with click off. A later track starts at master phase with no seam against its tail. Punch
   out of a sustained note: is the layer seam clean? Undo swap and reverse are click-free. Multiply:
   over a 1-bar loop, FIXED 4 on another lane: the loop becomes 4 bars at the commit and the first lane
   plays on with no seam or click there. FIXED off: a take stopped ~1.6 loops in records on to two
   loops and grows the loop, ~1.3 loops in keeps one at once. TRIM: halve an 8-bar lane while it
   plays: its first 4 bars from the next loop start, ↶ UNDO brings the 8 back, no click at either swap.
   FADE (2 bars) while three lanes play: the level falls smoothly to the bar line, the lanes stop there,
   PLAY ALL brings them back at their level. DUB FEEDBACK 50 % on a lane, dub two passes: the old
   layers fade; 0 % replaces; ↶ UNDO brings the loop before the dub back.
4. **Long session · grid.** Same jam, 10+ min: loops and click stay tight, no LED hop at commit,
   later takes on-grid, a flam-free commit-beat click; tempo is locked mid-count-in; a free record
   past 60 s auto-closes on a bar (is that UX fine?).
5. **Reload + editors.** Load → GO LIVE → close and reopen the app → the plugin is back, not live, with
   the amp-sim's knobs (editor and drawer) where you left them, and
   one GO LIVE re-arms. Editor in front; close → reopen, no hang. FabFilter editor open: a drawer
   slider moves its knob and back; the editor's own size menu → the host window follows.
6. **Fault injection.** Yank the interface while loops play → a toast, the loops and the plugin stay;
   reconnect → the same device comes back (or WASAPI takes over) and the loops play on. When an
   open fails and no device runs, the command bar's lamp reads amber (never seen): clear enough?
7. **Inputs + AUTO REC.** A slot on In 1 records only physical input 1, In 2 only input 2; both slots
   live at once, each on its own input, both heard and recorded.
   AUTO REC: a muted-guitar noise floor must not arm, a real attack must (sensitivity, onset, feel).
8. **Session files + Share output.** Export, CLEAR ALL, import the zip: the loops come back on the
   grid and the amp-sim sounds as it did at the export (a slot holding another plugin gets a toast);
   open a stem and the master in a DAW; the export's toast says where the zip went: is it there? Kill
   the app mid-jam → relaunch restores it. Share output → OBS, Chrome and Discord hear the master.
9. **Synths and FX.** The six synths and the lane FX by ear: anything off?
10. **MIDI controller, only if one is plugged (skip otherwise).** Unplug mid-note →
    toast + note release. Mod-wheel vibrato, pitch-bend, CC64 sustain feel.

## Decisions

None open. When work blocks on an owner decision, add a table here: `#`, the question, and the
default if nothing is said.

## Stop detail

### Stops 1–2: alignment on the engine

A take starts the driver's reported input plus output latency (plus the plugin's latency and the
limiter's pre-delay) after its downbeat (`ProcessContext::align_frames`, lf-engine `api.rs`); there is
no trim. On the dev rig (Scarlett 2i2, loopback cable) the report holds within 0.1 ms at ASIO 64, 128
and 256, once the engine opens the driver at another block size first: a relaunch at the size the
driver last ran otherwise lands about two periods late (`docs/ARCHITECTURE.md` § Measured premise). Round trip with Pro-Q: 8.1 ms at 64, 15.1 ms at 128, 26.8 ms at 256. In the running app
through the cable (`pnpm native:engine-loopback`, six launches): the take lands within 0.12 ms of the
click at 64, 128 and 256, a loop re-recorded from playback adds no error of its own, and STOP ALL →
PLAY ALL keeps the click, its accent and the loops in place. A cable is a perfect player; whether a
guitarist's take feels on the click is this stop.

### Stop 3: take mechanics

- One grid for count-in, undo and reverse (lf-engine `looper.rs` and `grid.rs`). Known v1: an
  over-long FIXED bars/bpm pick records the largest whole-bar fit in 60 s.
- **Free-record stop:** bars come from the device clock with a quarter-beat grace; a press while the
  tail is in flight is honoured after the commit.
- **Click = transport mode** (owner's call): forced for the count-in, on during rec/play, SILENT when
  idle or all-stopped (the beat-LED still runs).

### Stop 5: reload + editors

- **Same file in both slots:** only the first opener gets an editor (why: the comment at
  `editorAffinity` in `src/ui/state/instrument.ts`).
- **Editor-to-front:** dropping behind after a click into BleepLoop is intended.

### Stop 6: fault paths

The engine's device owner (`src-tauri/src/engine_io/owner.rs`): a lost device keeps the engine, its
loops and its slots, tries the same device again, then falls back to WASAPI's defaults; the UI toasts
each step. Proven on the fake driver (`engine_io/tests.rs`), not by a yank on the rig. A fallback at
another rate drops the loops from the engine and keeps them in recovery, with a toast; a player's device
pick at another rate with loops asks first. On the lap, if the WASAPI default runs at another rate than
ASIO: yank with loops playing, read the toast, replug and relaunch: the loops come back.

### Stop 8: session lifecycle

Formats: the zip layout and `session.json` as before (`docs/ARCHITECTURE.md` § Audio architecture);
the PCM comes from `engine_snapshot` (`docs/ARCHITECTURE.md` § Audio architecture, Session files).
The master and the stems both include STOPPED tracks; import works only while every lane is EMPTY.
Recovery starts once a device runs.
