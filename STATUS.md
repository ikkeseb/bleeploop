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
ASIO, `native:engine-smoke` at 48 and 44.1 kHz and `native:engine` after the switches. Owed once the
loopback cable is in: `native:engine-loopback --rate=48000` (alignment at 48 kHz). Driver latency
reports are not guitar latency; after a relevant change, rerun only the affected check.

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
   click; the master fader scales the wet, not the recorded level.
2. **A MIDI footswitch, if one is plugged.** Learn REC/DUB onto it (Audio Settings → MIDI learn, one
   tap) and take the jam's records with the foot: one press, one action? Still learned after a
   restart? Then switch it to HOLD and hold it through one overdub.
3. **48 kHz.** Sample rate → 48 kHz with loops playing: the confirm asks, the driver reopens at 48 kHz
   (the select says so), and a new take lands on the click and sounds as clean as at 44.1.
4. **IN FX on the guitar.** ECHO and REVERB still feel immediate, the echo sits on the tempo, and a take
   recorded with them sounds as it did live. RING MOD (never heard): its Freq range 20–1500 Hz and
   440 Hz default are an agent's pick: keep, or name what to change.
5. **End with FADE, then reopen.** FADE (2 bars) while three lanes play: the level falls smoothly to
   the bar line and the lanes stop there. Close and reopen the app: the loops and the plugin come back
   (not live), the amp-sim's knobs where you left them, and one GO LIVE re-arms.

## Not heard yet

Built, and proven by machine where a probe reaches, but never played. A line leaves when a jam answers
it.

- Buffer 256 and 64: the take on the click at each size, a short switch gap, and a plugin loaded while
  loops play crossfades in without a click.
- The count-in (1 bar, accent on 1, no dead air; the tempo locks mid-count-in); FIXED 2 stops on the
  downbeat after exactly 2 bars (an over-long FIXED pick records the largest whole-bar fit in 60 s); a
  free record stopped near the downbeat after N bars keeps N, an early or a mid-bar stop behaves, and a
  press while the take's tail is in flight is honoured after the commit.
- The click: silent when idle, stops with STOP ALL, and the count-in still clicks with CLICK off.
- A later track starts at master phase with no seam; punching out of a sustained note leaves a clean
  layer seam; the undo swap and reverse are click-free.
- Multiply: over a 1-bar loop, FIXED 4 on another lane grows the loop to 4 bars with no seam. FIXED
  off: a take stopped ~1.6 loops in grows to two loops, ~1.3 keeps one.
- TRIM: halve an 8-bar lane while it plays (its first 4 bars from the next loop start); ↶ UNDO brings
  the 8 back; no click at either swap.
- DUB FEEDBACK 50 %, two passes: the old layers fade; 0 % replaces; ↶ UNDO brings the loop back.
- 10+ minutes: loops and click stay tight, no LED hop at a commit, no flam on the commit beat's click;
  a free record past 60 s auto-closes on a bar (fine?).
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

## Decisions

When work blocks on an owner decision, it gets a row here: `#`, the question, and the default if
nothing is said.

| # | Question | Default |
|---|---|---|
| D19 | A DUB pressed while a playing lane's UNDO, REVERSE or TRIM swap waits for its loop boundary makes the swap heard at once, mid-loop. Hold the DUB to the boundary (a held looper command also holds a STOP or punch-out behind it for up to a loop), keep it and write the exception into the engine briefing, or refuse the DUB with a reason (a wire change)? Red tests, ignored: `tests/overdub_undo_reverse.rs`, `tests/trim.rs`. | Keep it, as an exception. |
| D20 | A plugin call that outputs NaN or an infinity is silenced. Should it also damage the running take or layer, as an input gap does (the layer is dropped)? Today a bad stretch leaves silence in it; with DUB FEEDBACK 0 that replaces the loop there. | No. |
| D21 | Make the engine's applied state the one authority: the feed carries it whole, the snapshot and the load carry each lane's mix with its PCM, and the settings mirror and the UI's mix copies go (two reviews found the races the copies cause: COPY or CLEAR followed by a setting, a dropped event left unrepaired, an import heard at the old mix, an export whose master and session.json disagree). L each, wire changes, the fader's feel to re-check. | Not started. |
| D22 | Remove the native MIDI stack (`src-tauri/src/engine_io/midi/`, about 1.8k lines with tests, and `midir`)? It is built and tested but never started; MIDI arrives through Web MIDI. | Keep it. |
