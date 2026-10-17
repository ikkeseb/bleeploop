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
mic is on channel 2, so the loopback cable goes from rear output L into the front instrument jack, the
guitar's cable doing it, and the mic stays (`docs/VERIFY.md` § The owner's loopback run); a slot's Auto input reads In 2 on a device with two or more inputs
(`input_channel` in `src-tauri/src/engine_io/transition.rs`), so the guitar on channel 1 needs In 1
picked in the slot. Monitors sit on the rear L/R and headphones on the front jack, and SmartMuting mutes the
monitor outputs whenever a headphone jack is in, by design and with no setting to turn it off. Its own
Loop-back channels are a digital send inside the driver, so they do NOT stand in for the rig's cable:
they skip the converters the round trip is there to measure. The saved ASIO pick is still the
Focusrite's, which no longer exists, so the first launch falls back to automatic and will likely open an
unrelated driver until `Audient USB Audio ASIO Driver` is picked once in Audio Settings
(`src-tauri/AGENTS.md` § Open threads). Its EVO Control app sets the ASIO buffer size and the sample rate
from the system tray, which is the handle for a check no device has ever run: a rate or buffer changed in
the driver's own panel while the app plays. To hear only the app's wet signal, its monitor mix (EVO
Control's mixer button, then the knob) goes fully clockwise, playback only; toward the inputs the dry
guitar plays beside the amp-sim. Its main volume is also Windows' volume for the device and scales the
ASIO output with it, so at a volume where Windows sounds are tolerable the guitar plays quieter than on
the Scarlett, in Ableton as well (owner, 2026-10-16). The app passes a slot's plugin at unity and the
master stops at 0 dB, so the gap is loudness, not a lost gain stage; turning the other apps down in
Windows' Volume mixer helped little. Parked: `docs/backlog-taste.md` § Parked ideas.

**Machine verification (Windows):** the push gates are `AGENTS.md`'s; the rig probes, when to run each,
and their baselines: `docs/VERIFY.md` § When to run the plugin probes. Where the rig stands: every rig
probe passed on the engine-only app between 2026-09-29 and 10-05 (`native:engine`'s counter check
forgives the join's trims and starves in a WASAPI open's first 3 s: D25); `native:engine-loopback
--rate=48000` last passed 21/21 at 64, 128 and 256 on 2026-10-03. `release:smoke` last passed 7/7 on
2026-10-16 on the runner build of 8a18e5ad (v0.8.5) on the EVO 4 at ASIO 128, its input take through
the cable from output L into In 1. Driver latency
reports are not guitar latency; after a relevant change, rerun only the affected check.
**The input path on the EVO 4:** `release:smoke`'s input take passed through the cable on 2026-10-16,
so the Audient driver's input reaches a loop. Where a take lands against the click is
`native:engine-loopback`'s, never run on the EVO 4 (its default input is 2: `--channel=0` for the
owner's cabling), and no Scarlett baseline answers it. A check that needs the device free may run while
the owner is away from the PC; one that needs the cable needs the owner to cable it.

**Last play: 2026-09-28** (engine, a local release build, ASIO; a second player on a WASAPI build
of their own): worked well overall, no issue found; the WASAPI player heard delay on DI monitoring
(they play on ASIO since; the owner counts it resolved).

## Next jam

Install v0.8.5 (published 2026-10-16, CI-built, ASIO SDK 2.3.4 where local builds use 2.3.3; an
installed v0.8.4 offers it in Help) rather than `pnpm dev:asio`: it has SCOPE and the UPDATE pill.
Audio Settings: the Audient ASIO driver, buffer 128; the guitar's slot on In 1; EVO Control's monitor mix fully clockwise. Play a real jam with these in it and say what felt off.
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

- The designer walk's fixes (2026-10-17, measured in the rig, never played): every command-bar popover
  (CLICK, SENS, IN FX) now opens under the bar, not under its pill, so IN FX no longer covers END STOP; the
  BPM editor keeps the numeral's box while you type; GO LIVE keeps INPUT LIVE's width; Tab wraps inside
  Audio Settings and Help; a toast that is neither an error nor a finished action (update offer, loops
  kept, driver not installed, plugin to load, device fallback) reads as a dim note, not a green check.
  Second pass the same day: the 48 kHz switch and the mid-jam update ask are the app's own dialog (focus
  on Cancel, Esc declines), not a Windows box; Help keeps one height on both tabs; an open Audio Settings
  ends above a toast; the ASIO row says what runs ("off · WASAPI runs"); a toast with an Audio Settings
  button no longer also says "in Audio Settings"; DUB FEEDBACK reads KEEP and PITCH's knob SHIFT; Help's
  Manual shows the EXPORT and IMPORT icons themselves. Third pass, on the owner's Darkroom answers: every
  disabled control in the bar and the tool cluster fades by tone (the enabled tool icons a step brighter so
  the two states differ); the Audio Settings gear is Lucide's settings icon (the owner's pick, B, from six candidates in `bleeploop-push16` r1); every lane-well cue sits in one box
  and the count-in numeral stands beside its word; Tab walks the two-row bar as drawn; the UPDATE pill has a
  mode pill's size; FIXED and FADE both keep their unit to 1000 px; the IN FX popover stacks its sends so
  every slider is 214 px long (was about 40), an off send saying in one line what it adds. Does a popover 10 px lower read as its
  pill's, does the note toast still catch the eye when it must, does the in-app ask read as a question to
  answer before playing on, and do the disabled tools still read as present from the chair?
- A plugin bundle whose scan failed (2026-10-17, machine-verified, no real failed bundle seen yet): it is
  listed on Audio Settings' PLUGINS page beside the unsupported ones, "N plugins found but not loaded",
  file name, failed or unsupported, and a host-worded reason (timed out, crashed, report unreadable; never
  the child's own output); the picker stays clean and the rescan button retries it. The owner chose both
  shots in `bleeploop-push16` r4 without a word: does the row say enough, or should the picker grey it?
- SCOPE's beam (2026-10-17, machine-verified, never seen on a device): when the live columns stop while the
  loop moves (the view closed and reopened, a look stepped back to SCOPE, a held feed) the beam moves on with
  the bar instead of freezing at the last column; a column takes the beam only when its own device frame
  is fresh, so a late or sporadic batch (even whole loops behind) neither pulls it back nor pushes it
  ahead, a multiply or a restart starts it over, and the HUD's bar is the bar the beam stands in (it read
  bar 1 over the loop's last half beat). Does the beam ever jump, stutter or lag the bar when you open the
  view mid-loop?
- GO LIVE lights only from the engine's live flag, now in the feed: with no audio device running it stays
  unlit until one does, and a lost press toasts once ("Could not go live. Press GO LIVE again."). Does
  the pill ever lag the guitar going live, and is it ever lit while the dry guitar is silent?
- Audio Settings is four pages behind a rail (DEVICE · MIDI PEDALS · PLUGINS · DIAGNOSTICS) with a lamp
  and a state word each, the round trip an ordinary row under the picks; a MIDI toast's button lands on
  MIDI PEDALS. Does a pedal's port pick take fewer moves than before, and does the lamp read from the chair?
- The command bar's CLICK volume and AUTO REC SENS sliders sit in popovers, the engaged mode pills carry
  an LED, END STOP stands with the modes: do the five pills still read from 1.5 m, and is a slider in a
  popover one move too many mid-jam?
- CLR, the CLEAR key and a pedal share one arm, the engine's: arm with one, confirm with another; SURE?
  shows exactly while a second press would clear (machine: `tests/actions.rs`, the `jam-keys` probe). Does
  it read in time from the guitar, and does a slip on CLR never cost a loop?
- Lane state colour lands on the beat, no fade (rec, dub, play, mute): does a quick stop-then-play read as a
  flicker from the guitar? The core's hover fade stays (machine: the contact sheet, no timing).
- The count-in numeral now fills the lane well at every window size, and the FX pill carries a lamp while
  any lane effect is on: does 4-3-2-1 read from 1.5 m, and does the lamp read as FX, not as a state LED?
- The FX drawer sits on one row (title, five FX keys, DUB FEEDBACK inline) with every engaged FX's params on
  one row under the keys, and the lanes take a 53 px rung while it shows params in a stack under 440 px:
  five lanes stay on screen at 1000x700 with the keyboard; are the FX keys and the 91 px sliders still big
  enough to hit while playing?
- Buffer 256 and 64: the take on the click at each size, a short switch gap, and a plugin loaded while
  loops play crossfades in without a click (machine: `tests/slots.rs`).
- An instrument (a plugin or a built-in synth) entering or leaving a slot while GO LIVE is on: the dry
  guitar fades across, each part on its own path, where it stepped by up to half full scale before
  (machine: `tests/slots.rs`). Is the change inaudible mid-note?
- The input meter on each live slot's card (its own input, a held peak, CLIP), INPUT LIVE and the
  selected lane's "A LIVE" badge (machine: `live-meter` probe, `the_meter_reads_each_slots_own_capture_channel`).
  From where you stand with the guitar, does the meter read, and does CLIP light when the interface
  clips and never when it does not?
- The count-in (1 bar, accent on 1, no dead air; the tempo locks mid-count-in); FIXED 2 stops on the
  downbeat after exactly 2 bars (an over-long FIXED pick records the largest whole-bar fit in 60 s); a
  free record stopped near the downbeat after N bars keeps N, an early or a mid-bar stop behaves, and a
  PLAY/STOP press while the take's tail is in flight commits it stopped (a REC/DUB press there counts as a stop too).
- The click: silent when idle, stops with STOP ALL, and the count-in still clicks with CLICK off.
- Time signatures, picked beside BPM (machine: `tests/meter.rs`, the `meter-picker` probe): in 6/8
  BPM and TAP count the dotted quarter, two beats a bar, so the count-in reads 2-1 with the eighths
  clicking softer between. Play a 6/8 song and TAP along unprompted for a minute: did the taps land
  on the dotted quarter or on every eighth? Counting eighths changes the BPM's unit: the engine, the
  wire's meter table on both sides, and a rule for 6/8 sessions already saved (their file records the
  dotted quarter, and no build re-reads it in another unit). Are the soft eighths heard, not busy?
- A later track starts at master phase with no seam; reverse's flip adds no step (machine:
  `tests/seam_continuity.rs`), but does its turn of direction click?
- 48 kHz: Sample rate → 48 kHz with loops playing: the confirm asks, the driver reopens at 48 kHz (the
  select says so), and a new take lands on the click and sounds as clean as at 44.1.
- IN FX on the guitar: ECHO and REVERB still feel immediate, the echo sits on the tempo, and a take
  recorded with them sounds as it did live. RING MOD (never heard): its Freq range 20–1500 Hz and
  440 Hz default are an agent's pick: keep, or name what to change.
- Multiply: over a 1-bar loop, FIXED 4 on another lane grows the loop to 4 bars with no seam. FIXED
  off: a take stopped ~1.6 loops in grows to two loops, ~1.3 keeps one.
- ↔ WIDE (COPY wide: the copy 20 ms behind, mirrored pan; a centred lane goes half left, the copy
  half right; machine: `tests/copy.rs`): on a guitar riff, does it sound like two guitars, and is
  20 ms the right gap?
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
- A saved ASIO driver uninstalled since it was picked: the app starts on the automatic driver, a
  toast names both, and Audio Settings keeps the pick, marked not installed (machine: the
  `asio-driver` probe). On a rig whose old interface's driver is gone, does the toast show at launch
  and say what to pick? (A driver still installed whose device is gone fails to start instead.)
- A headset plugged in mid-jam on WASAPI with System default picked: the sound moves to it by itself
  (Windows moves the stream; at the same rate nothing reopens and a take in flight keeps recording, a
  mono mic too) and Audio Settings names it (proven only on the fake driver; whether the input follows
  a mic at another format is unmeasured). One at another rate plays converted until a Buffer change
  reopens it at its own rate (asking first while loops are held): fine, or keep the converted stream?
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
- Share output without the click: a stream or a call hears the loops, the plugins and the guitar but
  no count-in and no CLICK, while the headphones still click (ASIO only; proven offline and on the fake
  driver, `src-tauri/crates/lf-engine/tests/share_mix.rs`). On a loud mix the stream may sound a
  touch louder on the beat than what you hear: the click no longer pushes its limiter down.
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
- The lane delay's repeats now sit on the beat: each lands a whole division after the last, where each
  used to fall another ~3 ms behind (about 19 ms by the 8th), as the input ECHO already does (machine:
  the echo test in `src-tauri/crates/lf-engine/src/dsp/fx/delay.rs`). With a high feedback over a
  loop, do the repeats sit on the click, and does the delay still sound right?
- Native MIDI, a controller and a footswitch through a jam: mod-wheel vibrato, pitch-bend and CC64
  sustain on a built-in synth; unplugged mid-note, a toast and the note released; 20 quick replugs
  (under a second each) with notes held: each releases them, and the bindings fire after each.
- The MIDI bindings across the update, a restart and a replug: the pedals learned before the update
  fire with no relearn (ASSIGN in the learn row fixes one listed as "not connected"), and they still
  fire after the app restarts and after the controller moves to another USB socket.
- RETAKE and AUTO REC from a learned pedal: one press, one toggle; mid-take (and AUTO REC once a loop
  locks the tempo) the press is refused with its reason on the lane, as the greyed button is.
- The update affordance, rebuilt after the owner could not find the old one: an offer now raises an
  UPDATE pill in the command bar's tool cluster (present ONLY while there is an offer) over its own
  panel, and the install reports its three waits: the download in MB with a determinate bar, the
  signature check, the installer. The offer is gone from Help. Proven on a scripted updater
  (`verify/probes/app-update.mjs`, five pages); what no probe reaches is the real install: whether the
  numbers climb at a watchable rate over a real GitHub download, whether the pill is findable without
  being told where it is, and whether the app's last frame before it quits reads as progress rather
  than a freeze. Only the next real release answers that, from the installed app.
- Help in two tabs: `?` opens Reference (the looper keys, the keystroke pedals, the play keys, the
  octave and the drum pads, unscrolled at every window size, tighter below 720 px tall) and Manual holds
  the rest, About this build at its foot. Can it be read at a glance mid-jam, from playing distance?

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
