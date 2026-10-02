# Taste backlog (not a gate)

Feel, wording and placement questions for the owner, answered with the app open: yes, no or change,
and the line leaves. Nothing here blocks a build, a release or a rig stop. Agents add one line per
finding (an audit's taste finding lands here, never as a `STATUS.md` stop), keep the list short
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
- **Stage view (B):** readable from where you stand with the guitar? MUTED and STOP are both grey:
  distinct enough? The PC keyboard's note keys are silent inside it (so drum-mode 1–4 select lanes):
  right, or should notes play?
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

- **Not built on purpose** (no owner or tester ask; each would add its own lap): a FREE tempo-setting
  first take and 3/4 or 6/8 (`beatsPerBar = 4` runs through the grid math, the click and the count-in:
  gate-adjacent timing code); panel drag; more built-in synths (they fill layers, they do not compete
  with plugins); a recent-jams shelf (keep the last N recovery archives on ✕ ALL and close, offered in
  IMPORT, M); per-lane pan and resampling on import or recovery (S each; no pan exists); one `.pill`
  primitive with type tokens; a rhythm guide, three GM-kit grooves on the master pulse instead of the
  click (M, ear-gated).
- Looper aesthetic forks: ring/state colour = state vs track identity; Day/Night.
- Undo as visible history (layer count on ↶ UNDO); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- A visualizer in the stage view (a tester's idea): each track drawn as something that moves with its
  own sound. The look is open; BleepLoop builds its own and shares no code with other projects. The UI
  already holds each lane's waveform bins and playhead, so a wave-style view needs no engine change. A
  spectrum needs per-lane band levels in the feed. Neither sees lane FX or the live input.
