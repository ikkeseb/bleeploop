# STATUS: what the owner's ear still owes

The owner's ear, eye or decision on the PC: the next jam's checks, what no jam has heard yet, and the
decisions that block work. Taste: `docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md`
§ Open threads.

**The rig's interface:** an **Audient EVO 4** (`Audient USB Audio ASIO Driver`) since 2026-10-09. It
replaced a **Scarlett 2i2 3rd gen** whose Focusrite ASIO driver is uninstalled, and every timing and
latency baseline in this repo is the Scarlett's (`docs/VERIFY.md`): none can be measured again without
that driver, a result on the EVO 4 answers none of them, and an oddity there is the new driver's until
shown otherwise, the ASIO preopen written for the Focusrite included (`src-tauri/AGENTS.md`). What the
EVO 4 has shown: `release:smoke` 7/7 with an input take (2026-10-16 for v0.8.5 through the cable,
2026-10-17 to 10-26 for v0.8.6 to v0.8.8 with the guitar in its place), so its driver runs and its input reaches a loop.
Not measured on it: where a take lands against the click (`native:engine-loopback`, `--channel=0` for
input 1) and the round trip. Cabling and EVO Control's settings for a rig run: `docs/VERIFY.md` § The
owner's loopback run.

**What the EVO 4 changes for playing:** the guitar's front instrument jack is channel 1, and a slot's
Auto input reads In 2 on a device with two or more inputs (`input_channel` in
`src-tauri/src/engine_io/transition.rs`), so the guitar's slot needs In 1 picked. A headphone in the
front jack mutes the rear monitor outputs (SmartMuting, no setting). Its Loop-back channels are a
digital send inside the driver and do not stand in for the rig's cable. EVO Control (system tray) sets
the ASIO buffer and rate, the handle for a check no device has run: a change there while the app plays
(`src-tauri/OPEN-THREADS.md`). Its monitor mix fully clockwise plays only the app's wet
signal. Its main volume is also Windows' volume for the device and scales the ASIO output, so at a
tolerable Windows volume the guitar plays quieter than on the Scarlett, in Ableton too; the app passes
a slot's plugin at unity and the master stops at 0 dB, so the gap is loudness, not a lost gain stage
(parked: `docs/backlog-taste.md` § Parked ideas).

**Machine verification (Windows):** the push gates are `AGENTS.md`'s; the rig probes, when to run each,
and their baselines: `docs/VERIFY.md` § When to run the plugin probes. Every rig probe last passed on
the Scarlett (2026-09-29 to 10-05; `native:engine`'s counter check forgives a WASAPI open's first 3 s:
D25). Driver latency reports are not guitar latency; after a relevant change, rerun only the affected
check. A check that needs the device free may run while the owner is away from the PC; one that needs
the cable needs the owner to cable it.

**Last play: 2026-09-28** (engine, a local release build, ASIO; a second player on a WASAPI build
of their own): worked well overall, no issue found; the WASAPI player heard delay on DI monitoring
(they play on ASIO since; the owner counts it resolved).

## Next jam

Install the latest release, v0.8.8 (published 2026-10-26 and CI-built, it holds every check below;
an installed v0.8.7 offers it through the UPDATE pill), not `pnpm dev:asio`. In Audio Settings pick
the Audient ASIO driver (a saved Focusrite pick toasts as not installed) at buffer 128 and put the
guitar's slot on In 1; turn EVO Control's monitor mix fully clockwise. Play a real jam with these in
it and say what felt off. After the jam, agents note what it tried under "Last play", drop what it
answered and refill this list from § Not heard yet: at most five, and the docs guard counts them.

1. **The amp-sim live, and takes on the click.** GO LIVE on the amp-sim (VST3, Petrucci) and record a
   first take, an overdub and a FIXED 4: does the guitar feel immediate, and does every take keep its
   attacks and endings on the click?
2. **A MIDI footswitch, if one is plugged (MIDI now runs natively, outside the WebView).** Try the
   pedal you learned before the update, then learn REC/DUB onto it (one tap) and take the jam's
   records with your foot: does it fire with no relearn, and is every press one action?
3. **A later take from stopped loops.** Stop every loop (■ ALL) and press REC on an empty track: one
   bar of count-in clicks (4-3-2-1), then every track starts from the top on the downbeat and the
   take records from there; does it feel like the first take's count-in, and do the old loops and
   the new take sit together? (machine: `tests/later_arm.rs`; no rig probe arms it, so this is its
   first run on a real device)
4. **The stage view's new look, SCOPE, over the jam.** B opens the stage view on SCOPE (V still
   reaches Orbit and Strata), one live scope per bay, what the speaker plays on top with the guitar
   as a cyan line over it and then each lane after its FX: does it say what is locked and what is
   alive from where you stand, and is it worth leaving open while you play?
5. **End with FADE, then reopen.** FADE (2 bars) while three lanes play, then close and reopen the
   app: are the loops and the plugin back (not live) with the amp-sim's knobs where you left them,
   and does one GO LIVE re-arm?

## Not heard yet

Built, and proven by machine where a probe reaches, but never played. A line leaves when a jam answers
it.

### On the listening sheet

No rig, no guitar: `pnpm listen` writes `logs/listen/index.html` and the owner answers there. Each
clip is a synthetic plucked phrase through the real engine: it settles clicks, the grid and curves,
never the owner's guitar, latency, a driver, a pedal or feel.

- An overdub punched in and out over a held note (the 5 ms edges, D23): is every click gone?
  `punch-edges`
- STOP, PLAY and ↶ UNDO on a lane mid-note (the 5 ms edges, D23): is every click gone? `stop-play`
- TRIM halves an 8-bar lane while it plays and ↶ UNDO brings the 8 back: is there a click at either
  swap? `trim-swap`
- REV while a lane plays: does the turn of direction click? `reverse-turn`
- A longer later take grows the loop (FIXED 4 over a 1-bar loop makes it 4 bars): is there a seam?
  `multiply-seam`
- A later lane joins at master phase: is there a seam? `later-lane`
- CLEAR on a lane whose delay is still echoing: does the echo stop at the press, with nothing of what
  was cleared coming back? `clear-tail`
- The count-in: one bar, the accent on 1: does it sound right? `count-in`
- The 6/8 click: are the softer eighths between the dotted quarters heard, and not busy? `click-6-8`
- The lane delay with a high feedback over a loop, its repeats now a whole division apart (no longer
  drifting about 3 ms each): do they sit on the click and still sound right? `delay-on-click`
- FADE (2 bars) while three lanes play: does the level fall smoothly to the bar line, with the lanes
  stopping there? `fade`
- DUB FEEDBACK at 50 % over two passes, then 0 %: do the old layers fade at 50 %, and does 0 %
  replace them? `dub-feedback`
- IN FX ECHO on the input at its default: are its repeats on the tempo? `in-fx`
- RING MOD (never heard) at its 440 Hz default, an agent's pick: keep it, or name what to change.
  `ring-mod`
- ↔ WIDE (a copy 20 ms behind, the pan mirrored) on a riff, without and then with: is 20 ms the
  gap? `wide`
- Pan hard left, centre, hard right: does a hard pan play +3 dB on its side and leave the other side
  silent? `pan`

### With the app open, no guitar

The owner at the 27" display with the installed release, or an agent's screenshots: what an eye
settles.

- A fresh install's first screen (nothing of the app's stored yet: slot A asks `Pick your amp-sim`
  with its input pick and GO LIVE, slot B folds to `Add a second sound`, the keyboard stays hidden
  until a built-in synth is picked, lane 1 says `SPACE or your pedal records here` until the first
  loop): does it say plug in, pick your sound, record? Only an empty profile shows it; an install
  that stored anything opens as before. (machine: `verify/probes/legacy-storage.mjs`, the contact
  sheet's scene 8)
- The main screen's restyle (2026-11-07; the principles in `src/ui/AGENTS.md`, the look in the
  `src/app.css` header: the waves as smooth bodies, grey for silent, a mode on but locked, the lanes a
  step larger at 2560, one bar ruler, the loop's length in bars only), from the chair on the 27":
  does every lane's state read at a glance?
- The selected lane, marked by its lit number alone: is it findable from the chair?
- A FADE on the restyled waves: does it read as the loop sinking, in motion?
- The command-bar popovers (CLICK, SENS, IN FX) now open under the bar, 10 px lower: does each still
  read as its pill's?
- A toast that is neither an error nor a finished action is now a dim note: does it still catch the
  eye when it must?
- The 48 kHz switch and the mid-jam update ask are the app's own dialog (Esc declines): does it read
  as a question to answer before playing on?
- A disabled control fades by tone, and a lane core's hover is a rim tone that a press darkens, with
  no grow or spring: do the disabled tools still read as present from the chair?
- SCOPE's beam moves on with the bar when the live columns stop, and only a fresh column takes it:
  does it ever jump, stutter or lag the bar when the view opens mid-loop?
- In the stage view: does the count-in read from where you stand?
- Audio Settings in four pages behind a rail (DEVICE · MIDI PEDALS · PLUGINS · DIAGNOSTICS): does a
  pedal's port pick take fewer moves?
- The rail's lamp and state word on each page: does the lamp read from the chair?
- The command bar's five mode pills, an LED on each engaged one, END STOP among them: do they read
  from 1.5 m?
- CLICK volume and AUTO REC SENS in popovers: is a slider in a popover one move too many mid-jam?
- Lane state colour lands on the beat, with no fade: does a quick stop-then-play read as a flicker?
- The count-in numeral fills the lane at every window size: does 4-3-2-1 read from 1.5 m?
- The FX pill's lamp while any lane effect is on: does it read as FX, not as a state LED?
- The one-row FX drawer (the lanes on a 53 px rung while it shows params; five lanes at 1000x700 with
  the keyboard): are the FX keys and the 91 px sliders big enough to hit while playing?
- 6/8: do BPM and TAP count the dotted quarter, and does the count-in read 2-1? (machine:
  `tests/meter.rs`)
- TAP along a 6/8 song, unprompted: do you tap the dotted quarter or every eighth? (Counting eighths
  would change the BPM's unit in the engine, the wire's meter table and the 6/8 sessions already
  saved.)
- With no device: is the command bar's amber lamp clear enough?
- The lane's mix follows the engine (D21): does a dragged fader, DUB FEEDBACK or FX knob stay where it
  was left?
- MUTE toggles in the engine (D21): do two quick presses make two toggles?
- An older session opened after pan per lane: does every lane open centred?
- The right cluster grew from 240 to 264 px for pan: is it still fine at 1000 px wide?
- A session export: does the toast say where the zip went, and do a stem and the master open in a
  DAW?
- The UPDATE pill and its panel (the download in MB with a bar, the signature check, the installer;
  machine: `verify/probes/app-update.mjs`), over a real GitHub download: do the numbers climb at a
  watchable rate? Only a real install answers.
- The UPDATE pill: is it findable without being told?
- The update's last frame before the app quits: does it read as progress, not a freeze?
- Help in two tabs (Reference unscrolled at every window size, Manual with About this build at its
  foot): can it be read at a glance mid-jam, from playing distance?
- One running app (`src-tauri/src/single_instance.rs`; machine: its unit tests on the real kernel
  mutex, with no window): does a second launch bring the window up on top, restoring a minimized
  one? Only a release launch shows it.
- A second launch, a reopen during a quit and an installer's relaunch: does each start the app once,
  with nothing ever open twice?

### Only on the rig

Guitar in, ASIO, a pedal where named: feel, latency, devices, drivers, plugins, MIDI hardware, and
anything else that needs the interface or a real plugin.

- The amp-sim live: does the master fader scale the wet sound, never the recorded level?
- GO LIVE pressed once mid-note (the 5 ms edges, D23): is the click gone?
- An attack played right on a DUB's punch-in (D23): does it still sound whole?
- A MIDI binding after a restart and after a move to another USB socket: does it fire with no
  relearn? (ASSIGN fixes one listed as "not connected" in Audio Settings → MIDI learn.)
- The pedal switched to HOLD: does holding it through one overdub work?
- A press that cancels a later take's count-in: do the loops stay stopped and silent?
- SCOPE on the device (the next jam's 4): no eye has seen it draw a live pixel. With its columns flowing (only while
  that look shows), does the app stay tight on ASIO 128? (Frame cost measured on the browser rig only,
  `pnpm probe frame-cost`: script p99 under 0.5 ms, a feed frame about 30 µs, 635-byte JSON; paint
  and real WebView2 are not. Machine: `src-tauri/crates/lf-engine/tests/scope.rs`,
  `verify/probes/scope-mirror.mjs`, the stage-view probe's `scope` group.)
- With SCOPE flowing: does the guitar still feel immediate?
- The pedal action "Stage view: next look": does it still feel right?
- The same plugin file in both slots (D29): the editor belongs to the slot whose load came first,
  and the other slot's is refused with a toast; does the toast say what to do? (machine: the
  `instrument-controls` and `rig-recall` probes)
- Archetype in both slots (D29): does the app never hang?
- A plugin bundle whose scan failed, listed on Audio Settings' PLUGINS page with a host-worded reason
  (no real failed bundle seen yet; the picker stays clean, a rescan retries it): does the row say
  enough, or should the picker grey it?
- With loops playing, keep the plugin folder dialog open a minute, with a pedal's CLEAR or a rejected
  take while it is open, then close it: do the waveforms, the count and the toasts land without a
  jump? (machine: the `feed.rs` tests, `engine-seam` and `meter-picker`)
- A gesture held across that dialog: does it keep its value?
- GO LIVE: does the pill ever lag the guitar going live, or light while the dry guitar is silent?
- Hammer GO LIVE from a pedal and the cap together, during a plugin load too: one switch per press,
  none lost?
- CLR, the CLEAR key and a pedal share one arm, SURE? showing while a second press would clear: does
  SURE? read in time from the guitar?
- A slip on CLR: does it never cost a loop?
- Buffer 256 and 64: does a take land on the click at each size, with a short switch gap? (machine:
  `tests/slots.rs`)
- A plugin loaded while loops play: does it crossfade in without a click?
- An amp-sim unloaded or swapped while GO LIVE is on: is the swap a clean gap, with no dry DI heard or
  recorded between the plugins? (machine: `tests/slots.rs`, `engine_io/plugins.rs`)
- A plugin-requested restart while live (a FabFilter latency-mode change): does the slot stay live?
- An instrument entering or leaving a slot while GO LIVE is on: is the change inaudible mid-note?
  (machine: `tests/slots.rs`)
- The input meter on each live slot's card (its own input, a held peak, CLIP), with INPUT LIVE and the
  "A LIVE" badge: does the meter read from where you stand?
- The slot card's CLIP: does it light when the interface clips, and only then?
- The first take's count-in: does the tempo lock mid-count-in?
- FIXED 2: does the take stop after exactly 2 bars?
- A FIXED pick too long for 60 s: does it record the largest whole-bar fit?
- A free record stopped near the downbeat: does it keep its N bars?
- A free record stopped early or mid-bar: does it behave?
- A PLAY/STOP (or REC/DUB) press while a take's tail is in flight: is the take committed, stopped?
- The click: is it silent when idle, does it stop with STOP ALL, and does the count-in click with
  CLICK off?
- 48 kHz with loops playing: does the confirm ask, the driver reopen at 48 kHz, and a new take land
  on the click as clean as at 44.1?
- IN FX ECHO and REVERB on the guitar: are they immediate?
- A take through IN FX: does it sound as it did live?
- RING MOD's 20–1500 Hz range, an agent's pick: keep it, or name what to change.
- With FIXED off, a take stopped about 1.6 loops in grows the loop to two, about 1.3 keeps one: does
  that feel right?
- ↔ WIDE on your own guitar riff: does it sound like two guitars?
- DUB FEEDBACK, then ↶ UNDO: does the loop come back?
- 10 minutes or more: do the loops and the click stay tight, with no LED hop or click flam at a
  commit?
- A free record past 60 s closes at the cap, mid-bar: fine, or should it close on the last bar line?
- Plugin editors: does each open in front, and close and reopen with no hang?
- FabFilter's editor open: does a drawer slider move its knob, and does its own size menu resize the
  host window?
- A drawer slider dragged fast, or while a preset loads in the plugin's own editor: does the amp-sim's
  knob ever sit off its slider after a quick sweep? (machine: `plugin-param-refused` probe)
- A tempo or meter changed while the device is stopped or switching: do the loops play at it once the
  device opens? (machine: `engine_io/tests.rs`)
- A real 64-bit VST2 plugin whose parameters hold a value (so far a fixture, and ReaJS and ReaStream
  in `native:smoke` and `native:swap`): does it play live with no click on load?
- That VST2's editor: does it open, resize and keep redrawing (`effIdle` runs each owner turn until
  it answers 0), and does a knob there move the drawer's slider?
- That VST2 after a restart: do its settings come back?
- Yank the interface while loops play: is there a toast with the loops and the plugin kept, and on
  reconnect does the device come back (or WASAPI take over) with the loops playing on? (fake driver
  only so far)
- At a WASAPI rate unlike ASIO's: do the loops come back from recovery after a relaunch?
- A saved ASIO driver uninstalled since it was picked (the rig's Focusrite): the app starts on the
  automatic driver, possibly an unrelated one (a controller's), and Audio Settings keeps the pick
  marked not installed; does the toast show at launch, name both and say what to pick? (machine: the
  `asio-driver` probe; a driver still installed whose device is gone fails to start instead)
- A headset plugged in mid-jam on WASAPI with System default picked: does the sound move to it, a take
  in flight keep recording, and Audio Settings name it? (fake driver only; a mic at another format is
  unmeasured)
- A headset at another rate plays converted until a Buffer change reopens it (asking first while
  loops are held): fine, or keep the converted stream?
- An on-screen MUTE while a pedal's CLEAR is armed (D21): does it cancel the CLEAR?
- An import or a recovery (D21): does each lane play at its saved mix from its first sample?
- Pan per lane: does a pan move glide, with no zipper?
- Two slots live at once on In 1 and In 2: is each heard, each recording only its own input?
- AUTO REC: does a muted guitar's noise floor leave it unarmed, and a real attack arm it?
- Session files: export, CLEAR ALL, import: are the loops back on the grid and the amp-sim as at the
  export?
- Kill the app mid-jam: does the relaunch restore it?
- Share output: do OBS, Chrome and Discord hear the master with no stutter as Share turns on?
- Share output on a call of 20 minutes or more, also into the interface's own Windows output: no
  dropout? (machine: a 26-minute soak at the 40 ms mirror buffer)
- Share output without the click (ASIO only): does a stream hear the loops, the plugins and the
  guitar but no count-in or CLICK while the headphones click? On a loud mix the stream may sound a
  touch louder on the beat. (machine: `src-tauri/crates/lf-engine/tests/share_mix.rs`)
- The six synths and the lane FX, played by ear: anything off?
- Native MIDI on a built-in synth: do mod-wheel vibrato, pitch-bend and CC64 sustain work through a
  jam?
- A MIDI controller unplugged mid-note: is there a toast, and is the note released?
- 20 quick replugs (under a second each) with notes held: does each release them, with the bindings
  firing after each?
- RETAKE and AUTO REC from a learned pedal: one press, one toggle?
- The same pedal mid-take (and AUTO REC once a loop locks the tempo): is the press refused with its
  reason on the lane, as the greyed button is?
- The update from the release that carries the in-jam download to the next: commit a take during the
  download; is it back after the restart?

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
| D30 | Three remote branches, one unlanded commit each, kept as they are: `pw16/heap-probe` (22a2b33d, a candidate: the stage draw's clock and level reads in typed arrays, the allocation probe on the freshness path), `proto11/settings-a` (f6ed7aee, a WIP Audio Settings prototype, ungated), `proto11/bar-c` (297d6acd, a prototype: one TAKE chip holding the five mode pills). Land, rework or delete each? | Kept untouched until the owner says |
| D31 | A press just past the loop's 1 waits for the next pass: a later take starts a whole loop late (rendered: `tests/later_arm.rs`, the ignored test; 8 s on 4 bars at 120, up to 60 s), and END STOP, UNDO and REVERSE share that boundary (read from source). Snap a press within a grace back to the 1, or keep waiting a loop? It changes how the looper plays. | Keeps waiting a loop until the owner says |
