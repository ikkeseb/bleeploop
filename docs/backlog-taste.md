# Taste backlog (not a gate)

Feel, wording and placement questions for the owner, answered with the app open: yes, no or change,
and the line leaves. Nothing here blocks a build, a release or a rig stop. Agents add one line per
finding (an audit's taste finding lands here, never in `STATUS.md`), keep the list short
enough for one sitting, and never re-rank the owner's answers. Provenance lives in the commit that
added a line.

## With the app open, `pnpm dev:asio`

- **Sample rate row (unseen):** Audio Settings' select under Buffer, "Device (44.1 kHz)" / "44.1 kHz"
  / "48 kHz" (under ASIO only the rates the driver runs; under WASAPI the device's alone, with a hint
  that Windows sets it). Clear beside Buffer, and is the hint useful?
- **END STOP reads as a record control:** with END STOP on and FIXED off, a stopped free take keeps
  every whole bar, as built (END STOP only stops playback). The owner asks other players before
  deciding on wording or behaviour.
- **Is the amp-sim live, from 1.5 m?** Only the slot's pill says so; the 4×22 px record meter is the
  only clipping cue.
- **Stage view (B), unseen on the rig:** a visualizer in two looks, Orbit and Strata (V switches).
  One answer per line:
  - Which look opens first on a new install: Orbit (built) or Strata, the closest to the looper's
    waveforms? After that the last chosen look is the one that opens.
  - Orbit's ring order: track 1 outermost (built: the most circumference) or innermost, by the input?
  - How much light: a wake behind the playhead plus a glow that breathes with each track's level
    (built); only the wake, or a wake a bar long instead of a beat?
  - The count-in numeral: warm white (built) or the armed track's amber?
  - Strata: what just played scrolls on dimmed, left of the now line (built), or the now line at the
    left edge with only what comes next?
  - No persistent state label is left (the message line still says a wait, TAKE n, FADING OUT and
    STOPPING AT LOOP END): MUTED is dim grey with a faint wake, STOPPED a still, slightly brighter grey.
    Distinct enough from where you stand, with the chips as the second cue?
  - The selected track: a 2 px warm-white circle outside its ring in Orbit, a row floor and an edge bar
    in Strata, and the warm chip. Readable from where you stand, or heavier?
  - An overdub in Strata keeps the loop's green waveform; the now-light, the head and the chip turn
    amber. Does it read as overdubbing from where you stand, or should the row take an amber wash?
  - The chips, the bar and the beat sit small at the edges at 0.7 opacity: readable with the guitar on?
  - The view switch is a learnable pedal action ("Stage view: next look"): worth a pedal, or V and the
    button only?
  - The PC keyboard's note keys are silent inside it (so drum-mode 1–4 select lanes and V switches the
    look): right, or should notes play?
  - The looper lanes keep drawing under the stage: stop them while it is open?
  - The pointer hides with the two buttons after 3 s: right for a stage, or keep it?
- **IN FX defaults (unheard, an agent's pick):** echo 1/8, feedback 0.4, level 0.5; reverb 0.5. The
  echo's level is scaled so its repeats carry the input's energy (at feedback 0.95 the first echo is
  about ⅓ of the level): does high feedback still feel right? The reverb is summed to mono.
- **A free take past the loop (unheard):** a stop 1.5 loops in keeps two loops, just under keeps one.
  Right by ear, or lean toward the longer take?
- **FADE (unheard):** the level is r² of a linear ramp from the press to the first bar line at or
  after its bars (2 bars pressed mid-bar lasts up to almost 3). A natural ending? Start on the next bar
  instead? Keep the − 2 BARS + stepper visible beside ■ ALL?
- **Command bar under 1100 px:** still three rows in the app (its extra tool icons): CLICK on at
  ≤ 1000 px, CLICK + FIXED at ≤ 1100 px. Two rows there need about 150 px less on row 2: the tool
  icons behind one menu?
- **Failed plugin bundles:** a bundle whose scan failed is simply missing from the picker; only the
  log says why. Show it greyed with the reason, or keep the picker clean?
- **Tone recall toasts:** a refused tone toasts at every load until the plugin is changed once (the
  saved settings are kept): say that they are kept? An import whose slot holds another plugin toasts
  "This session used X in slot B": enough, or offer to load it?
- **Update ready (unseen):** a warm-white dot on the Help cap, one toast and an "Update ready" section
  in Help. Visible enough, or a cap of its own?
- **MIDI learn row:** is "latching" clear to a guitarist? A long binding list makes the popover tall.

## By ear, when convenient

- Should the click be ON by default while recording?
- Levels: the click volume, the drum kit's pad balance, the plugin output-gain default.

## Parked ideas (for the long run)

- **The owner's own ideas, not scheduled:** full panel drag, with plugin editors inline, each filling
  its panel (the layout vision of 2026-06-17; the movable, hideable keyboard is built). COPY with a
  delay offset (2026-09-18) waits on STATUS D29. A time signature the player picks (3/4, 6/8 and so
  on; 2026-10-06): `beatsPerBar = 4` runs through the grid math, the click and the count-in, which is
  gate-adjacent timing code.
- **Not built on purpose** (no owner or tester ask; each would add its own lap): a FREE tempo-setting
  first take (it meets the same `beatsPerBar = 4` code as the time signature above); more built-in
  synths (they fill layers, they do not compete with
  plugins); a recent-jams shelf (keep the last N recovery archives on ✕ ALL and close, offered in
  IMPORT, M); resampling on import or recovery (S); one `.pill` primitive with type tokens; a rhythm
  guide, three GM-kit grooves on the master pulse instead of the click (M, ear-gated).
- **Dropped by the owner (2026-10-07):** a tester's "open device settings" button that opens the ASIO
  driver's own panel for rate and buffer: asio-sys 0.3.0 does not expose the call, and a change made
  there mid-run is the open thread in `src-tauri/AGENTS.md` (a panel change while the app runs).
- Looper aesthetic forks: ring/state colour = state vs track identity; Day/Night.
- Undo as visible history (layer count on ↶ UNDO); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- Stage looks beyond Orbit and Strata: Horizon (designed, not built: five lanes of road in perspective
  rolling toward a near edge where now is, the input glowing in the sky; a look is one module and one
  line in `src/ui/stage/views.ts`). The looks read each lane's waveform bins, its volume and the input
  meter, so they see neither lane FX nor a fade's level: a per-lane output peak in the feed would make
  the light honest, and band levels would allow a spectrum look.
