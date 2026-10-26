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

Install the latest release (v0.8.8, published 2026-10-26 and CI-built, holds every check below; an
installed v0.8.7 offers it through the UPDATE pill) rather than `pnpm dev:asio`. Audio Settings:
the Audient ASIO driver (a saved Focusrite pick toasts as not installed), buffer 128; the guitar's
slot on In 1; EVO Control's monitor mix fully clockwise. Play a real jam with these in it and say
what felt off.
After the jam, agents note what it tried under "Last play", drop what it answered and refill this
list from § Not heard yet: at most five, and the docs guard counts them.

1. **The amp-sim live, and takes on the click.** GO LIVE (VST3, Petrucci): does the guitar through the
   amp feel immediate? The first take, an overdub and FIXED 4 keep their attacks and endings on the
   click; the master fader scales the wet, not the recorded level. Then the 5 ms edges (D23, landed
   unheard): press GO LIVE once mid-note, punch a DUB in and out over a held note, and UNDO, STOP and
   PLAY a lane mid-note: is every click gone, and does an attack right on the punch-in still sound whole?
2. **A MIDI footswitch, if one is plugged** (MIDI now runs natively, outside the WebView). Does the
   pedal learned before the update fire with no relearn? One listed as "not connected" in Audio
   Settings → MIDI learn: ASSIGN it to the pedal. Learn REC/DUB onto it (one tap) and take the jam's
   records with the foot: one press, one action? Still learned after a restart? Then switch it to
   HOLD and hold it through one overdub.
3. **A later take from stopped loops.** Stop every loop (■ ALL), then REC on an empty track: one bar of
   count-in clicks (4-3-2-1), then every track starts from the top on the downbeat after it and the
   take records from there. Does it feel like the first take's count-in, and do the old loops and the
   new take sit together? A press that cancels the count leaves the loops stopped and silent (machine:
   `tests/later_arm.rs`; no rig probe arms it, so this is its first run on a real device).
4. **The stage view's new look, SCOPE, over the jam.** B opens it and SCOPE is what it opens on (V
   still reaches Orbit and Strata). Each bay is a live scope: the top one is what the speaker plays
   with the guitar as a cyan line over it, then one per lane after its FX. Does it say what is locked
   and what is alive, from where you stand, and is it worth leaving open while you play? Then what
   only a device answers: with the columns flowing (they flow only while this look shows), does the
   app stay tight on ASIO 128, does the guitar still feel immediate, and does the pedal action
   "Stage view: next look" still feel right? Also: does the count-in read from where you stand?
5. **End with FADE, then reopen.** FADE (2 bars) while three lanes play: the level falls smoothly to
   the bar line and the lanes stop there. Close and reopen the app: the loops and the plugin come back
   (not live), the amp-sim's knobs where you left them, and one GO LIVE re-arms.

## Not heard yet

Built, and proven by machine where a probe reaches, but never played. A line leaves when a jam answers
it.

- The designer walk's fixes: the command-bar popovers (CLICK, SENS, IN FX) open under the bar, not
  their pill; a toast that is neither an error nor a finished action reads as a dim note; the 48 kHz
  switch and the mid-jam update ask are the app's own dialog (Esc declines); a disabled control fades
  by tone; the lane core's hover is a rim tone and a press darkens it, with no grow or spring. Does a
  popover 10 px lower read as its pill's, does the note toast still catch the eye when it must, does
  the in-app ask read as a question to answer before playing on, and do the disabled tools still read
  as present from the chair?
- A plugin bundle whose scan failed is listed on Audio Settings' PLUGINS page with a host-worded
  reason, the picker stays clean and the rescan retries it (no real failed bundle seen yet): does the
  row say enough, or should the picker grey it?
- SCOPE's beam moves on with the bar when the live columns stop, and only a fresh column takes it:
  does it ever jump, stutter or lag the bar when the view opens mid-loop?
- GO LIVE lights only from the engine's live flag, and a lost press toasts once: does the pill ever
  lag the guitar going live, or light while the dry guitar is silent? Each press is now the engine's
  toggle: hammer GO LIVE from a pedal and the cap together, during a plugin load too: one switch per
  press, none lost?
- Audio Settings in four pages behind a rail (DEVICE · MIDI PEDALS · PLUGINS · DIAGNOSTICS), a lamp
  and a state word each: does a pedal's port pick take fewer moves, and does the lamp read from the
  chair?
- The command bar: CLICK volume and AUTO REC SENS in popovers, an LED on each engaged mode pill, END
  STOP with the modes. Do the five pills read from 1.5 m, and is a slider in a popover one move too
  many mid-jam?
- CLR, the CLEAR key and a pedal share the engine's arm, SURE? showing while a second press would
  clear: does it read in time from the guitar, and does a slip on CLR never cost a loop?
- Lane state colour lands on the beat, no fade: does a quick stop-then-play read as a flicker?
- The count-in numeral fills the lane well at every window size, and the FX pill carries a lamp while
  any lane effect is on: does 4-3-2-1 read from 1.5 m, and the lamp as FX, not as a state LED?
- The one-row FX drawer (the lanes on a 53 px rung while it shows params; five lanes at 1000x700 with
  the keyboard): are the FX keys and the 91 px sliders big enough to hit while playing?
- Buffer 256 and 64: the take on the click at each size, a short switch gap, and a plugin loaded while
  loops play crossfading in without a click (machine: `tests/slots.rs`).
- An amp-sim unloaded or swapped while GO LIVE is on: the slot goes off live first, so no dry DI is
  heard or recorded between the plugins (machine: `tests/slots.rs`, `engine_io/plugins.rs`); a
  plugin-requested restart (a FabFilter latency-mode change) keeps the slot live. Is the swap a clean
  gap with no dry flash?
- An instrument entering or leaving a slot while GO LIVE is on: the dry guitar fades across where it
  stepped before (machine: `tests/slots.rs`). Is the change inaudible mid-note?
- The input meter on each live slot's card (its own input, a held peak, CLIP), INPUT LIVE and the "A
  LIVE" badge: does the meter read from where you stand, and does CLIP light when the interface clips
  and only then?
- The count-in (1 bar, accent on 1, the tempo locking mid-count-in); FIXED 2 stopping after exactly 2
  bars (an over-long FIXED pick records the largest whole-bar fit in 60 s); a free record stopped
  near the downbeat keeping its N bars, an early or mid-bar stop behaving; a PLAY/STOP (or REC/DUB)
  press while the take's tail is in flight committing it stopped.
- The click: silent when idle, stopping with STOP ALL, the count-in clicking with CLICK off.
- Time signatures (machine: `tests/meter.rs`): in 6/8 BPM and TAP count the dotted quarter, the
  count-in reads 2-1 and the eighths click softer between. TAP along a 6/8 song unprompted: on the
  dotted quarter or every eighth? (Counting eighths changes the BPM's unit in the engine, the wire's
  meter table and the 6/8 sessions already saved.) Are the soft eighths heard, not busy?
- A later track starts at master phase with no seam; does reverse's turn of direction click?
- 48 kHz with loops playing: the confirm asks, the driver reopens at 48 kHz, and a new take lands on
  the click as clean as at 44.1.
- IN FX on the guitar: ECHO and REVERB immediate, the echo on the tempo, a take sounding as it did
  live. RING MOD (never heard): its 20–1500 Hz range and 440 Hz default are an agent's pick: keep, or
  name what to change.
- Multiply: FIXED 4 over a 1-bar loop grows it to 4 bars with no seam; with FIXED off a take stopped
  ~1.6 loops in grows to two loops, ~1.3 keeps one.
- ↔ WIDE (the copy 20 ms behind, mirrored pan): on a guitar riff, two guitars, and is 20 ms the gap?
- TRIM halves an 8-bar lane while it plays and ↶ UNDO brings the 8 back: no click at either swap?
- DUB FEEDBACK 50 % over two passes fades the old layers, 0 % replaces, ↶ UNDO brings the loop back.
- 10+ minutes: loops and click stay tight, no LED hop or click flam at a commit. A free record past
  60 s closes at the cap, mid-bar: fine, or close on the last bar line?
- Editors: in front; close and reopen with no hang; with FabFilter's open, a drawer slider moves its
  knob and its own size menu resizes the host window.
- A drawer slider dragged fast, or while a preset loads in the plugin's own editor, ends where the
  plugin ends (one set in flight per plugin); a tempo or meter changed while the device is stopped or
  switching is the one the loops play at once it opens (machine: `plugin-param-refused` probe,
  `engine_io/tests.rs`). Does the amp-sim's knob ever sit off its slider after a quick sweep?
- A real 64-bit VST2 plugin whose parameters hold a value (so far a fixture, and ReaJS and ReaStream in
  `native:smoke` and `native:swap`): plays live with no click on load, its editor opens and resizes, a
  knob there moves the drawer's slider, and its settings come back after a restart.
- Yank the interface while loops play: a toast, the loops and the plugin stay; reconnect: the device
  comes back (or WASAPI takes over) and the loops play on (fake driver only); at a WASAPI rate unlike
  ASIO's the loops come back from recovery after a relaunch. With no device, is the command bar's
  amber lamp clear enough?
- A saved ASIO driver uninstalled since it was picked (the rig's Focusrite): the app starts on the
  automatic driver, possibly an unrelated one (a controller's), a toast names both, and Audio Settings
  keeps the pick marked not installed (machine: the `asio-driver` probe). On the rig, does the toast
  show at launch and say what to pick? (A driver still installed whose device is gone fails to start
  instead.)
- A headset plugged in mid-jam on WASAPI with System default picked: the sound moves to it, a take in
  flight keeps recording, and Audio Settings names it (fake driver only; a mic at another format is
  unmeasured). One at another rate plays converted until a Buffer change reopens it (asking first while
  loops are held): fine, or keep the converted stream?
- The lane's mix follows the engine (D21): a dragged fader, DUB FEEDBACK or FX knob stays where it was
  left; MUTE toggles in the engine (two quick presses, two toggles; an on-screen MUTE cancels a pedal's
  armed CLEAR); an import or recovery plays each lane at its saved mix from its first sample.
- Pan per lane: hard left or right is +3 dB on that side and silence on the other, a move glides with
  no zipper, an older session opens centred. The right cluster grew from 240 to 264 px: still fine at
  1000 px wide?
- CLEAR stops a lane's delay echo at the press, so no echo of what was cleared comes back.
- Two slots live at once on In 1 and In 2: each heard, each recording only its own input.
- AUTO REC: a muted-guitar noise floor does not arm it, a real attack does.
- Session files: export, CLEAR ALL, import brings the loops back on the grid and the amp-sim as at the
  export; a stem and the master open in a DAW; the toast says where the zip went. Kill the app mid-jam:
  the relaunch restores it.
- Share output: OBS, Chrome and Discord hear the master with no stutter as Share turns on, and a call
  of 20 minutes or more no dropout, also into the interface's own Windows output (machine: a 26-minute
  soak at the 40 ms mirror buffer).
- Share output without the click: a stream hears the loops, the plugins and the guitar but no count-in
  or CLICK while the headphones click (ASIO only; `src-tauri/crates/lf-engine/tests/share_mix.rs`). On
  a loud mix the stream may sound a touch louder on the beat than the headphones.
- SCOPE on a device: no eye has seen it draw a live pixel, and the frame cost with columns flowing is
  unmeasured (next jam's 4; machine: `src-tauri/crates/lf-engine/tests/scope.rs`,
  `verify/probes/scope-mirror.mjs`, the stage-view probe's `scope` group).
- The six synths and the lane FX by ear.
- The lane delay's repeats land a whole division apart, no longer drifting ~3 ms each: with a high
  feedback over a loop, do they sit on the click and still sound right?
- Native MIDI through a jam: mod-wheel vibrato, pitch-bend and CC64 sustain on a built-in synth;
  unplugged mid-note, a toast and the note released; 20 quick replugs (under a second each) with notes
  held, each releasing them and the bindings firing after each.
- The MIDI bindings across the update, a restart and a move to another USB socket: they fire with no
  relearn (ASSIGN fixes one listed as "not connected").
- RETAKE and AUTO REC from a learned pedal: one press, one toggle; mid-take (and AUTO REC once a
  loop locks the tempo) the press is refused with its reason on the lane, as the greyed button is.
- The UPDATE pill and its panel (the download in MB with a bar, the signature check, the installer;
  machine: `verify/probes/app-update.mjs`): over a real GitHub download, do the numbers climb at a
  watchable rate, is the pill findable untold, and does the last frame before the app quits read as
  progress, not a freeze? Only a real install answers. The download now runs while the jam plays, and
  the jam is saved after it: on the update from the release that carries this to the next, commit a
  take during the download; is it back after the restart?
- Help in two tabs (Reference unscrolled at every window size, Manual with About this build at its
  foot): can it be read at a glance mid-jam, from playing distance?

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
| D29 | The same plugin file in both slots: its GUI binds to the module's first LOAD (measured on Archetype VST3: B's first editor open hangs although A never opened one), but the editor guard hands ownership to the first slot that OPENS, so a later loader that opens first still hangs the app. Move the guard into the Rust host (it sees load order, leaks and teardown; the frontend's mirrored bookkeeping goes), or move ownership to the first load in the frontend guard? The cross-family read recommends the host. | Frontend: ownership moves to the first load (the ordinary case closed, no `src-tauri/` change); the host guard only on the owner's word. Its proof is a browser probe case: B refused when A loaded first; `editor-premise` (past the frontend guard) measures only the host guard |
