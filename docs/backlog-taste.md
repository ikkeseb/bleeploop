# Taste backlog (not a gate)

Feel, wording and placement questions for the owner, answered with the app open: yes, no or change,
and the line leaves. Nothing here blocks a build, a release or a rig stop. Agents add one line per
finding (an audit's taste finding lands here, never in `STATUS.md`), keep the list short
enough for one sitting, and never re-rank the owner's answers. Provenance lives in the commit that
added a line.

## With the app open, `pnpm dev:asio`

- **Sample rate row (unseen):** Audio Settings' select under Buffer, "Device (44.1 kHz)" / "44.1 kHz"
  / "48 kHz" (under ASIO only the rates the driver runs; under WASAPI a read-out of the device's own
  rate, no caret, with a hint that Windows sets it). Clear beside Buffer, and is the hint useful?
- **END STOP reads as a record control:** with END STOP on and FIXED off, a stopped free take keeps
  every whole bar, as built (END STOP only stops playback). The owner asks other players before
  deciding on wording or behaviour.
- **Stage view (B): SCOPE is built and machine verified, and a taste review says it misses the bar.**
  The owner found both older looks (Orbit, Strata) primitive; the diagnosis was that each drew the same
  recorded overview plus a playhead, so nothing on screen knew what was sounding. SCOPE draws the
  engine's live columns instead, additively, so a pass that lands on the pass before it accumulates
  light: a locked loop glows, a one-off peaks and dies. That part works, and it is the thing to build
  the next design around: in `logs/stage-view/1920x1080-scope-rich-b.png` lane 1 has a white-hot core
  where the loop has landed while lane 2 right under it is a dim grey contour, and which is which reads
  at any distance with no label.
  The rest misses. A taste read against the owner's own bar (a show that is striking to watch and
  impressive, yet considered and polished; less information and more mood, read from across the
  room) says: polished and legible, but it reads as a dark-mode DAW arrangement view, which is the
  instrument panel the owner does not want. Named costs, in order: a gutter of OUT/IN/1 to 5 labels,
  eight vertical gridlines, an inset frame and a 132 px empty band under lane 5 (12 % of the screen);
  the HERO bay is the largest region and the dimmest, with the guitar a cyan hairline inside a grey
  shape, and empty in five of eight scenes; and a lane carrying an overdub shows three hues at once
  rather than one hue at three intensities. Worst of all for the brief: side by side on the contact
  sheets it is recognisably STRATA with an inset frame, a gutter and an OUT/IN bay added, so whatever
  the owner disliked about STRATA is still in frame. The reviewer's one change: make the top bay carry
  the show, the room's output as a full-height additive field with the guitar wide and bright through
  it, so the largest region on screen is also the brightest.
  That is a reviewer's read of stills from a scripted engine, not the owner's, and the glow building
  pass by pass is motion no still shows. The owner's eye settles it (`STATUS.md` next jam, 4). Mockups on the
  old data were tried and rejected, so a new visual proposal waits for that. The earlier per-look
  questions are superseded (git history holds them). A hitch the owner saw was under the MIDI
  benchmark's deliberate 250 ms UI stalls, not seen in an ordinary run yet.
- **IN FX defaults (unheard, an agent's pick):** echo 1/8, feedback 0.4, level 0.5; reverb 0.5. The
  echo's level is scaled so its repeats carry the input's energy (at feedback 0.95 the first echo is
  about ⅓ of the level): does high feedback still feel right? The reverb is summed to mono.
- **A free take past the loop (unheard):** a stop 1.5 loops in keeps two loops, just under keeps one.
  Right by ear, or lean toward the longer take?
- **FADE (unheard):** the level is r² of a linear ramp from the press to the first bar line at or
  after its bars (2 bars pressed mid-bar lasts up to almost 3). A natural ending? Start on the next bar
  instead? Keep the − 2 BARS + stepper visible beside ■ ALL?
- **Command bar with an UPDATE offer:** the sliders sit in popovers and the five record modes carry lamps
  (`proto11/bar-ab` landed), so the bar holds two rows from 960 px with every mode on. The UPDATE pill
  joins row 2's tools, so with every mode on at 1000 px and below it breaks onto a third row (108 → 146 px)
  while an offer stands (measured 2026-10-17). The tool icons behind one menu, or live with it?
- **From the designer walk (2026-10-17), small calls:** the IN FX popover's sliders are about 40 px long in a
  480 px panel; FIXED drops its unit
  ("4") below 1110 px while FADE keeps "2 BARS"; the ARMED cue "WAITING FOR DOWNBEAT" is bare text while the
  other cues sit in a box; the ASIO row's unticked box beside "WASAPI" can read as "WASAPI off"; Tab order
  in the two-row bar runs row 2, row 1, row 2's tools (visual `order` only). Disabled by tone or by
  opacity, and the FX drawer with all five FX on at 1000x700, are Darkroom rounds (`bleeploop-walk13`).
- **Failed plugin bundles:** a bundle whose scan failed is simply missing from the picker; only the
  log says why. Show it greyed with the reason, or keep the picker clean?
- **Tone recall toasts:** a refused tone toasts at every load until the plugin is changed once (the
  saved settings are kept): say that they are kept? An import whose slot holds another plugin toasts
  "This session used X in slot B": enough, or offer to load it?
- **MIDI learn row:** is "latching" clear to a guitarist? A long binding list makes the popover tall.

## By ear, when convenient

- Should the click be ON by default while recording?
- Levels: the click volume, the drum kit's pad balance, the plugin output-gain default.

## Parked ideas (for the long run)

- **The owner's own ideas, not scheduled:** full panel drag, with plugin editors inline, each filling
  its panel (the layout vision of 2026-06-17; the movable, hideable keyboard is built). A meter per
  track (one lane in 4/4 over another in 3/4; 2026-10-15): more than passing a lane its meter
  (`grid.rs` module doc).
- **Parked by the owner (2026-10-16): a built-in compressor** right after a slot's plugin, one knob, no
  look-ahead (no added latency), recorded with the take: the density OTT gives the guitar in Ableton,
  for the loudness the EVO 4 lacks at a Windows volume the owner tolerates (`STATUS.md`, the EVO 4).
  The engine's `dsp::compressor` already runs the master limiter. Larger alternative: a plugin chain
  in a slot (amp-sim → OTT).
- **Not built on purpose** (no owner or tester ask; each would add its own lap): a FREE tempo-setting
  first take (it meets the bar math every meter shares, `grid.rs`); more built-in
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
  line in `src/ui/stage/views.ts`). Orbit and Strata read each lane's waveform bins, its volume and the
  input meter, so they see neither lane FX nor a fade's level; SCOPE is the look that reads the live
  columns (`lf_engine::scope`), and the same tap would make the two older looks honest. Band levels,
  which a spectrum look would want, are not built.
