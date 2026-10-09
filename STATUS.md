# STATUS: what the owner's ear still owes

The owner's ear, eye or decision on the PC: the next jam's checks, what no jam has heard yet, and the
decisions that block work. Taste: `docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md`
§ Open threads.

**The rig's interface:** every baseline in this repo was measured on a **Scarlett 2i2 3rd gen** (ASIO,
44.1 kHz, a cable from line out R into input 2). It was replaced by an **Audient EVO 4** on 2026-10-09
and the Focusrite ASIO driver was uninstalled the same day, so no Scarlett baseline here can be measured
again without installing that driver back. The EVO 4 has never been run by the app: its ASIO driver
(`Audient USB Audio ASIO Driver`) is unmeasured here, and the device side's driver-specific workarounds
were written for the Focusrite driver alone, the preopen at another block size for its two-period
lateness (`src-tauri/AGENTS.md`), so that workaround now runs against a driver it has never been measured
on. A result on the EVO 4 therefore answers none of the baselines below until they are measured again on
it, and an oddity seen there is the new driver's until shown otherwise.

**What the EVO 4 changes for the rig,** read from its manual (User Manual 5.0) and confirmed on the PC:
the guitar goes in the front instrument jack, which is channel 1 and overrides the rear Mic/Line 1; the
mic is on channel 2, which the loopback cable the rig protocol needs also wants, so the two now compete
for one input. Monitors sit on the rear L/R and headphones on the front jack, and SmartMuting mutes the
monitor outputs whenever a headphone jack is in, by design and with no setting to turn it off. Its own
Loop-back channels are a digital send inside the driver, so they do NOT stand in for the rig's cable:
they skip the converters the round trip is there to measure. The saved ASIO pick is still the
Focusrite's, which no longer exists, so the first launch falls back to automatic and will likely open an
unrelated driver until `Audient USB Audio ASIO Driver` is picked once in Audio Settings
(`src-tauri/AGENTS.md` § Open threads). Its EVO Control app sets the ASIO buffer size and the sample rate
from the system tray, which is the handle for a check no device has ever run: a rate or buffer changed in
the driver's own panel while the app plays.

**Machine verification (Windows):** the push gates are `AGENTS.md`'s; the rig probes, when to run each,
and their baselines: `docs/VERIFY.md` § When to run the plugin probes. Where the rig stands: every rig
probe passed on the engine-only app between 2026-09-29 and 10-05 (`native:engine`'s counter check
forgives the join's trims and starves in a WASAPI open's first 3 s: D25); `native:engine-loopback
--rate=48000` last passed 21/21 at 64, 128 and 256 on 2026-10-03. `release:smoke` last passed 7/7 on
2026-10-07 on v0.8.2's runner build, its input take through the loopback cable into In 2. Driver latency
reports are not guitar latency; after a relevant change, rerun only the affected check.

**Last play: 2026-09-28** (engine, a local release build, ASIO; a second player on a WASAPI build
of their own): worked well overall, no issue found; the WASAPI player heard delay on DI monitoring
(they play on ASIO since; the owner counts it resolved).

## Next jam

Install the latest release (CI-built, ASIO SDK 2.3.4 where local builds use 2.3.3) or run
`pnpm dev:asio`; Audio Settings: ASIO, buffer 128. Play a real jam with these in it and say what felt
off. After the jam, agents note what it tried under "Last play", drop what it answered and refill this
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

- Buffer 256 and 64: the take on the click at each size, a short switch gap, and a plugin loaded while
  loops play crossfades in without a click (machine: `tests/slots.rs`).
- The count-in (1 bar, accent on 1, no dead air; the tempo locks mid-count-in); FIXED 2 stops on the
  downbeat after exactly 2 bars (an over-long FIXED pick records the largest whole-bar fit in 60 s); a
  free record stopped near the downbeat after N bars keeps N, an early or a mid-bar stop behaves, and a
  PLAY/STOP press while the take's tail is in flight commits it stopped (a REC/DUB press there counts as a stop too).
- The click: silent when idle, stops with STOP ALL, and the count-in still clicks with CLICK off.
- A later track starts at master phase with no seam; reverse's flip adds no step (machine:
  `tests/seam_continuity.rs`), but does its turn of direction click?
- 48 kHz: Sample rate → 48 kHz with loops playing: the confirm asks, the driver reopens at 48 kHz (the
  select says so), and a new take lands on the click and sounds as clean as at 44.1.
- IN FX on the guitar: ECHO and REVERB still feel immediate, the echo sits on the tempo, and a take
  recorded with them sounds as it did live. RING MOD (never heard): its Freq range 20–1500 Hz and
  440 Hz default are an agent's pick: keep, or name what to change.
- Multiply: over a 1-bar loop, FIXED 4 on another lane grows the loop to 4 bars with no seam. FIXED
  off: a take stopped ~1.6 loops in grows to two loops, ~1.3 keeps one.
- TRIM: halve an 8-bar lane while it plays (its first 4 bars from the next loop start); ↶ UNDO brings
  the 8 back; no click at either swap.
- DUB FEEDBACK 50 %, two passes: the old layers fade; 0 % replaces; ↶ UNDO brings the loop back.
- 10+ minutes: loops and click stay tight, no LED hop at a commit, no flam on the commit beat's click.
  A free record past 60 s closes at the 60 s cap, not on a bar line (at 137 BPM a quarter bar past bar
  34's; the 34-bar loop comes in there, mid-bar): fine, or close on the last bar line?
- Editors: in front; close → reopen with no hang; with FabFilter's editor open, a drawer slider moves
  its knob, and the editor's own size menu resizes the host window.
- A 64-bit VST2 plugin in a slot (machine: `src-tauri/src/host/vst2_engine_tests.rs`, a fixture;
  ReaJS and ReaStream passed `native:smoke` and `native:swap`, and ReaStream's chunk was saved and
  restored, but neither has a parameter that holds a value, so no real plugin's settings were read
  back): it plays live with no click on load, its editor opens and resizes, a
  knob moved there moves the drawer's slider, and it comes back at its settings after a restart.
- Yank the interface while loops play: a toast, the loops and the plugin stay; reconnect: the same
  device comes back (or WASAPI takes over) and the loops play on (proven only on the fake driver). If
  the WASAPI default runs at another rate than ASIO, the loops come back from recovery after a
  relaunch. With no device running, the command bar's lamp reads amber: clear enough?
- The lane's mix follows the engine (D21): a volume fader, DUB FEEDBACK or FX knob dragged and let
  go stays where it was left, with no jump back; MUTE now toggles in the engine, so two quick presses
  are two toggles, and an on-screen MUTE cancels a pedal's armed CLEAR. An import or recovery plays
  each lane at its saved mix from its first sample.
- Pan per lane: the small slider beside each volume fader. Hard left or right is +3 dB on that side
  and silence on the other (the reverb stays in the middle); a move glides with no zipper noise; a
  session saved before pan opens centred and sounds as before. The lane's right cluster is wider for
  it (240 to 264 px), so the waveform is a little narrower: still fine at 1000 px wide?
- CLEAR stops a lane's delay echo at the press (it faded over 20 ms before), so a CLEAR or an import
  never brings back the echoes of what was cleared.
- Two slots live at once on In 1 and In 2: each heard, each recording only its own input.
- AUTO REC: a muted-guitar noise floor does not arm it, a real attack does.
- Session files: export, CLEAR ALL, import: the loops come back on the grid and the amp-sim sounds as
  at the export; a stem and the master open in a DAW; the export's toast says where the zip went: is
  it there? Kill the app mid-jam: the relaunch restores it.
- Share output: OBS, Chrome and Discord hear the master with no stutter as Share turns on, and a
  call of 20 minutes or more hears no dropout in it, also with the interface's own Windows output as the Share device (machine: a
  26-minute `native:engine --share` soak; the mirror's buffer went from 20 to 40 ms after it ran
  short in bursts about every 10.5 minutes there).
- The live scope taps (`lf_engine::scope`) and SCOPE, the look that draws them: per lane after its FX,
  the live monitor, and the engine's output, a min/max envelope in 4 ms columns over the feed, off
  until a look asks. Proven offline (`src-tauri/crates/lf-engine/tests/scope.rs`,
  `verify/probes/scope-mirror.mjs`), costed at under a few tenths of a percent of a 64-frame block, and
  the look is machine verified on real batches now: the stage-view probe's `scope` group measures that
  a repeated pass accumulates and a one-off decays, at both column densities and across batch
  boundaries, within a few percent of what the look's own constants predict. No eye has seen it draw a
  live pixel on a device, and the frame cost WITH columns flowing is still unmeasured (the budget case
  runs the look with none). That check is next jam's 4.
- The six synths and the lane FX by ear.
- Native MIDI, a controller and a footswitch through a jam: mod-wheel vibrato, pitch-bend and CC64
  sustain on a built-in synth; unplugged mid-note, a toast and the note released; 20 quick replugs
  (under a second each) with notes held: each releases them, and the bindings fire after each.
- The MIDI bindings across the update, a restart and a replug: the pedals learned before the update
  fire with no relearn (ASSIGN in the learn row fixes one listed as "not connected"), and they still
  fire after the app restarts and after the controller moves to another USB socket.
- RETAKE and AUTO REC from a learned pedal: one press, one toggle; mid-take (and AUTO REC once a loop
  locks the tempo) the press is refused with its reason on the lane, as the greyed button is.

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
