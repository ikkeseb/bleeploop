# STATUS: what the owner's ear still owes

The owner's ear, eye or decision on the PC: the next jam's checks, what no jam has heard yet, and the
decisions that block work. Taste: `docs/backlog-taste.md` (not a gate). Non-gate threads: `AGENTS.md`
§ Open threads.

**Machine verification (Windows):** the push gates are `AGENTS.md`'s. The rig probes, when to run
each, and their baselines and latest readings: `docs/VERIFY.md` § When to run the plugin probes.
Last rig pass, 2026-09-29 (the engine-only app): `native:smoke`, `survey`, `swap`, `recall`,
`tone-recall`, `engine-smoke`, `engine-recovery` and `release:smoke` green; `native:engine-loopback`
20/20 at 64 and 256, 18/20 at 128 (a detector reading v0.1.0 repeats). Since then (2026-10-01):
`native:export-master` (WASAPI) for the export's engine-rendered master; the sample-rate pick on
ASIO, `native:engine-smoke` at 48 and 44.1 kHz and `native:engine` after the switches. 2026-10-03, the
loopback cable in, with the stray-sound bar, the input-gap rejection and four plugin-host fixes:
`native:engine-loopback --rate=48000` 21/21 at 64, 128 and 256, no stray sound, no take rejected;
`native:smoke`, `swap` (filtered), `recall` and `tone-recall` green; `native:engine` green but its
counter check, which WASAPI's switch phases fail on main too (`docs/VERIFY.md`, its baseline). The
CLAP port layout (every declared port, a failed call silent): `native:smoke` and `swap` with Surge XT and
Pro-Q, and `engine-smoke` with Surge XT Effects (CLAP) live, green. v0.5.2 (that layout) passed its
`release:smoke` with the cable in once the line out was turned up (`docs/VERIFY.md`, its row). v0.6.0
(the counted later take, the stage view's two looks, RETAKE and AUTO REC as pedal actions) is released:
its `release:smoke` drove an exe a runner built (`build-exe` run by hand with `smoke`), with no cable,
and passed 6/6 on the synth take, that take's first run against a release build (the Organ's waveform
13 of 69 px, over the 15 % bar by 3 px); the input path stands as v0.5.2's cable proved it. No rig
probe arms a take from stopped loops, so the jam is the counted later take's first run on a real
device. Driver latency reports are not guitar latency; after a relevant change, rerun only the affected
check.

**Last play: 2026-09-28** (engine, a local release build, ASIO; a second player on a WASAPI build
of their own): worked well overall, no issue found; the WASAPI player heard delay on DI monitoring
(`src-tauri/AGENTS.md` § Open threads).

## Next jam

Install the latest release (CI-built, ASIO SDK 2.3.4 where local builds use 2.3.3) or run
`pnpm dev:asio`; Audio Settings: ASIO, buffer 128. Play a real jam with these in it and say what felt
off. After the jam, agents note what it tried under "Last play", drop what it answered and refill this
list from § Not heard yet: at most five, and the docs guard counts them.

1. **The amp-sim live, and takes on the click.** GO LIVE (VST3, Petrucci): does the guitar through the
   amp feel immediate? The first take, an overdub and FIXED 4 keep their attacks and endings on the
   click; the master fader scales the wet, not the recorded level. Press GO LIVE once mid-note, and
   punch a DUB in and out over a held note: does either click (D23)? Then the same on `agent/d23-swaps`
   (`git switch agent/d23-swaps`, `pnpm dev:asio`), plus an UNDO and a lane's STOP and PLAY mid-note:
   is every click gone, and does an attack right on the punch-in still sound whole?
2. **A MIDI footswitch, if one is plugged.** Learn REC/DUB onto it (Audio Settings → MIDI learn, one
   tap) and take the jam's records with the foot: one press, one action? Still learned after a
   restart? Then switch it to HOLD and hold it through one overdub.
3. **A later take from stopped loops.** Stop every loop (■ ALL), then REC on an empty track: one bar of
   count-in clicks (4-3-2-1), then every track starts from the top on the downbeat after it and the
   take records from there. Does it feel like the first take's count-in, and do the old loops and the
   new take sit together? A press that cancels the count leaves the loops stopped and silent (machine:
   `tests/later_arm.rs`; no rig probe arms it, so this is its first run on a real device).
4. **The stage view over the jam.** B opens it, V switches Orbit and Strata (seen only on the engine
   fake): does the light follow what is heard, does the count-in read from where you stand, does the
   learned pedal action "Stage view: next look" feel right, and does the app stay tight with it open on
   ASIO 128?
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
  PLAY/STOP press while the take's tail is in flight commits it stopped (a REC/DUB press there: D24).
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
- Yank the interface while loops play: a toast, the loops and the plugin stay; reconnect: the same
  device comes back (or WASAPI takes over) and the loops play on (proven only on the fake driver). If
  the WASAPI default runs at another rate than ASIO, the loops come back from recovery after a
  relaunch. With no device running, the command bar's lamp reads amber: clear enough?
- Two slots live at once on In 1 and In 2: each heard, each recording only its own input.
- AUTO REC: a muted-guitar noise floor does not arm it, a real attack does.
- Session files: export, CLEAR ALL, import: the loops come back on the grid and the amp-sim sounds as
  at the export; a stem and the master open in a DAW; the export's toast says where the zip went: is
  it there? Kill the app mid-jam: the relaunch restores it.
- Share output: OBS, Chrome and Discord hear the master.
- The six synths and the lane FX by ear.
- A MIDI controller: unplugged mid-note gives a toast and releases the note; mod-wheel vibrato,
  pitch-bend and CC64 sustain feel.
- RETAKE and AUTO REC from a learned pedal: one press, one toggle; mid-take (and AUTO REC once a loop
  locks the tempo) the press is refused with its reason on the lane, as the greyed button is.

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
| D19 | A DUB pressed while a playing lane's UNDO, REVERSE or TRIM swap waits for its loop boundary makes the swap heard at once, mid-loop. Hold the DUB to the boundary (a held looper command also holds a STOP or punch-out behind it for up to a loop), keep it and write the exception into the engine briefing, or refuse the DUB with a reason (a wire change)? Red tests, ignored: `tests/overdub_undo_reverse.rs`, `tests/trim.rs`. | Keep it, as an exception. |
| D20 | A plugin call that outputs NaN or an infinity is silenced. Should it also damage the running take or layer, as an input gap does (the layer is dropped)? Today a bad stretch leaves silence in it; with DUB FEEDBACK 0 that replaces the loop there. | No. |
| D21 | Make the engine's applied state the one authority: the feed carries it whole, the snapshot and the load carry each lane's mix with its PCM, and the settings mirror and the UI's mix copies go. L, wire changes, the fader's feel to re-check. The races it closes reproduce as ignored red tests (`d21_*` in `src-tauri/src/engine_io/tests.rs`, run with `-- --ignored`); the one a player meets: an import or recovery plays a lane saved muted or quieter for ~90 ms at full level. Sending the mix before the load alone lets a recovery that loses the race to a live take rewrite that take's mix (`recovery-import-failure`). A timed-out load that later plays and a rate switch that drops a load are a separate protocol question. | Not started. |
| D22 | Remove the native MIDI stack (`src-tauri/src/engine_io/midi/`, about 1.8k lines with tests, and `midir`)? It is built and tested but never started; MIDI arrives through Web MIDI. | Keep it. |
| D23 | The engine had no ramp at most of its edges: a sustained note stepped there 11 to 32 times its own slope. Built on `agent/d23-ramps` (not main): an overdub's first and last 5 ms are stored linear ramps (DUB FEEDBACK ramps from 1 too; the punch-out fades back to what the dub overwrote, with no tail after the press) and GO LIVE ramps the slot's input over 5 ms (the plugin's tail rings out). Machine-proven: `seam_continuity` a and the two live tests in `tests/slots.rs` green, every exact-sum test re-derived, engine-smoke and the 48 kHz loopback green on the branch. It changes recorded audio at a dub's edges (an attack right on the punch-in is softened over 5 ms). `agent/d23-swaps`, stacked on it, crossfades an UNDO swap and ramps a lane's PLAY and STOP mid-loop over 5 ms (played audio only; a scheduled END STOP and a finished FADE still cut on their frame); `seam_continuity` b and g green, two cross-family reviews' findings fixed, engine-smoke and the loopback green on it. A STOP inside an undo crossfade fades the two loops on their own ramps (no step). The branch holds main as of the counted later take (main has brought no Rust since): on the merge the count's restart keeps a still-sounding tail before the grid moves, as an idle PLAY ALL does there, and the engine suite passes (442 tests); the workspace check and the rig probes were not rerun on it. So the jam plays the tree that would land; a read of the whole branch by a third model found nothing more. Land the branches after the jam hears them? | Land both if the jam hears no click and no dulled attack. |
| D24 | A REC/DUB press while a first take's aligned tail is in flight is swallowed: it counts as a repeated stop, which cannot lengthen the take (`tests/first_take.rs` free_d encodes this). Start one overdub on the commit frame instead, as REC to DUB does mid-take? The engine's held commands make it small (a recorder flag and a wait until the window's end). Red test, ignored: free_i in `tests/first_take.rs`. | Keep it a stop. |
| D25 | In the first ~2.5 s after a WASAPI open the input join can trim or starve (about one open in ten after ASIO ran in the process, one in 43 without), and a take that overlaps it is rejected; `native:engine`'s counter check fails on it (`src-tauri/AGENTS.md` § Open threads). First, with Signal Desktop and Focusrite Notifier closed by the owner, rerun the two rig series: is it this PC? Then: change the trim rule so a late double pull and a push and a pull swapping order no longer trim (no latency change; 6 of the 9 traced, by a reading of the traces: no replay of them through the pipe proves it yet), raise the setpoint to the pipe's own rule (33–43 ms: 8–18 ms more input latency on WASAPI, owing the L1+L2 measurements), or accept it on the fallback path and scope the counter check? | The rerun, then the trim rule; the setpoint stays. |
