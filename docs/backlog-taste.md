# Taste backlog — NOT a gate

Feel, wording, placement. No functional risk anywhere on this list; nothing here blocks a build, a
release or a rig stop. The owner reads it with the app open, says yes / no / change,
and lines leave. Agents append ONE line per finding (an audit's taste finding lands here, never as a
`STATUS.md` stop) and never re-rank the owner's answers. Provenance for a line lives in the commit that added it,
not here.

## One eye-lap, `pnpm dev:asio`, screen by screen

- **END STOP reads as a record control (by ear, 2026-09-24):** with END STOP on and FIXED off, a stopped
  free take kept every whole bar, which is as built (END STOP only stops playback); the name suggested
  otherwise. The owner asks other players before deciding on wording or behaviour.

- **Command bar (2026-09-02):** the two-row form whenever one row cannot fit (CLICK/FIXED/AUTO/TAP on
  row 2 at the Tauri default 1280); the ⬇/⬆ EXPORT/IMPORT icons in the tool cluster; the 4 px record-level
  meter left of IN FX (−60..0 dBFS, green, red within 1 dB of full, a cyan tick at the AUTO trigger
  level while AUTO is on) — big enough? right place? AUTO reserves its sensitivity control's width
  so toggling it or changing the value keeps the stage in place. Engine mode at 960 and 1100 px
  (2026-09-28): the wordmark folds to its dot so the master stays on row 1. Still three rows in the app
  (its extra tool icons): CLICK on at ≤ 1000 px, CLICK + FIXED at ≤ 1100 px — two rows there need about
  150 px less on row 2 (the tool icons behind one menu?).
- **Count-in numeral (2026-09-02):** the big red 4-3-2-1 in the armed lane's well over "COUNT-IN" —
  size, colour, and whether a later take's "WAITING FOR DOWNBEAT" should count beats to the boundary too.
- **Error toasts:** bottom-right stack — placement, copy, feel; may overlap the bottom lane's right
  cluster when full. The device toasts: "Audio device lost", "Switched to another audio device",
  "Audio device back", "The audio input did not open", "MIDI device disconnected — <name>"; the
  dual-slot editor refusal, import-of-a-bad-file.
- **Help popover:** wording + section order (guitar GO LIVE, an Off slot for raw input, COPY, REV, CLICK/FIXED/AUTO REC, volume-detent line, Session and Pedals sections, the Looper keys
  as a chip row, the "Whole song" section: its heading is an agent's word). Overdub is "OVERDUB" (lane word) / "overdubs" (Help) / "Overdubbing" (screen
  reader) across three surfaces — one word?
- **Play-path signals from 1.5 m (2026-09-23):** nothing in the looper zone says the amp-sim is live
  (only the slot's pill); the 4×22 px record meter is the only clipping cue; the BPM lock pulse fires
  mid-count-in beside the big "4", which is rec-red inside an amber ARMED lane.
- **Audio Settings:** status chips moved into the diagnostics block (unratified).
- **MIDI learn row (2026-09-23, extended 2026-09-27, built, unseen):** above diagnostics — an action
  picker grouped into track and global actions, an ON TRACK row for a track action (the selected track
  or 1–5), LEARN (a cyan LISTENING while it waits, then "Let go of the pedal, and try it once this line
  is gone", up to 10 s for a latching pedal), then one line per binding (action · track, CC/note ·
  channel, a momentary/latching switch, HOLD on a record pedal, the port on hover, ✕ forgets). Right
  place and words? Is "latching" clear to a guitarist? A long list makes the popover tall.
- **Refused global pedal presses (2026-09-27, built, unseen):** TAP while the tempo is locked, FIXED
  while recording or under RETAKE say why in the selected lane's well: the right place for a global
  reason?
- **Lanes:** the selected-lane warm-white edge (no rail, owner 2026-09-23) — strong enough under
  PLAYING? the ARMED amber dashed-ring pulse; the 10 px state word with its LED; from 1.5 m little
  carries (state word, REC core, four 5 px beat dots) — the stage view (B) is built for that; undo/reverse cap placement (a per-track
  properties surface?); the empty well says nothing at all (no first-run guidance in the looper).
- **Stage view (2026-09-26, built, unseen):** B, the command-bar cap or a learned pedal: big state
  pillars (PLAY / STOP / DUB / ARMED / MUTED / ENDING…), a four-block beat bar, the count-in in the
  lane. Readable from where you stand with the guitar? MUTED and STOP are both grey: distinct enough?
  The PC keyboard's note keys are silent inside it (so drum-mode 1–4 select lanes): right, or should
  notes play? BAR n / N (2026-09-27) sits between LOOP and the beat bar, the total at half size, dim:
  readable at 1.5–3 m, total too small?
- **IN FX (2026-09-26, built, unheard):** the pill after the record meter and its ECHO/REVERB/RING MOD
  popover. Defaults are an agent's pick: echo 1/8, feedback 0.4, level 0.5; reverb 0.5. The echo's level
  is scaled so its repeats carry the input's energy (at feedback 0.95 the first echo is ~⅓ of the level):
  does high feedback still feel like it should? The reverb is summed to mono as ½(L+R).
- **FIXED past the loop (2026-09-26, built, unseen):** over a loop, + steps a bar at a time up to the
  loop, then a whole loop at a time (the multiply); the title explains it. Clear enough?
- **A free take past the loop (E10, 2026-09-27, built, unseen/unheard):** the lane's record head spans
  the loops reached so far (it jumps back to the middle when a pass starts); a stop 1.5 loops in keeps
  two, just under keeps one. Clear, and right by ear, or lean toward the longer take?
- **✂ TRIM (2026-09-27, built, unseen):** the pill after ⧉ COPY opens KEEP FIRST [−] N [+] BARS · TRIM,
  N starting at half the loop; with six lane pills showing, COPY and TRIM shrink to their glyphs. The
  waveform shows the trim at once while the sound switches at the next loop start (as UNDO and REV do).
  Halve track rounds an odd loop down (7 keeps 3). A second TRIM (or TRIM after ↶ UNDO) in the same
  loop: the loop plays whole to its end, then only the latest trim (2 then 5 keeps 2+2+1). Readable,
  and right?
- **FADE (2026-09-27, built, unheard/unseen):** the level is r² of a linear ramp from the press to the
  stop (−12 dB half-way); it starts at the press and ends on the first bar line at or after its bars
  (2 bars pressed mid-bar lasts up to almost 3). FADE plus a − 2 BARS + stepper beside ■ ALL (~170 px
  on row 1); FADING in END STOP's ENDING colours. Natural ending? Start on the next bar instead? Keep
  the stepper visible?
- **DUB FEEDBACK (2026-09-27, built, unheard/unseen):** the lane FX drawer's last module, slider OLD
  from 100 % down to REPLACE at 0. Clear, right place? What does 50 % continuous dubbing sound like?
- **Update ready (2026-09-29, built, unseen):** a warm-white dot on the Help cap, one toast ("BleepLoop
  vX is ready: open Help (?) to update") and an "Update ready" section above Help's reference with the
  release notes and UPDATE AND RESTART. Visible enough, or a cap of its own in the command bar?
- **CLEAR's confirm (2026-09-27):** in engine mode every pedal press between two CLEAR presses makes
  the second ask again; an on-screen click in between does not (web mode alike). Keep?
- **↶ UNDO (2026-09-27):** was ↶ DUB; it undoes an overdub or a TRIM, and the pedal action reads
  'Undo / redo'. Right words?
- **Tone recall (2026-09-27, built, unheard):** tweak the amp-sim in its editor and in the PARAMS drawer,
  restart, and import a session: does it sound exactly as left each time? A refused tone toasts
  "<name>: saved settings could not be restored; it loaded with its defaults" at every load until the
  plugin is changed once (the saved settings are kept): say that they are kept? An import whose slot
  holds another plugin toasts "This session used X in slot B — load it there to hear the session's tone":
  enough, or offer to load it?
- **About this build (2026-09-26, built, unseen):** Help's last section — version · commit, Copy
  diagnostics, Open log folder, below the fold at 1280×820. Findable when a tester is asked for it?
- **Looper prominence:** does the first-run stage read looper-as-hero? `DEFAULT_STAGE_WEIGHTS` in
  `layout-store.ts` (keyboard 0.55 / looper 2.0, instrument = autoSize, keyboard BOTTOM). Keyboard
  de-emphasis is open — rebalance, don't remove (default-hidden / slimmer strip).
- **Plugin drawer:** one by-eye lap with a real plugin drawer (params scroll INSIDE the card, header
  pinned); slot vertical-expand.
- **Keyboard transport feel:** 1–5, ↑↓ (or PgUp/PgDn, ←→) next/prev, Space, Enter, Backspace = UNDO,
  Delete twice = CLEAR (drum mode: pads own 1–4, only 5 selects). A refused key shows its reason for
  1.6 s as an amber boxed note in the selected lane's well with an amber lane edge; the first Delete
  says "press again to clear" for the 2.5 s window; a lane a key selects below the lane stack's fold
  scrolls into view, smoothly unless reduced motion is on (2026-09-23, built, unseen) — readable from
  the guitar? right colour and length? Backspace/Delete the right keys?
- **COPY pill (2026-09-18):** "⧉ COPY" after REV in the lane's pill row, hidden while no lane is
  EMPTY, always targets the first EMPTY lane — right place and word? Floated: a right-click
  context menu (none exists in the app yet) for picking the target lane. The pill row with DUB + REV
  + COPY all showing at 1280 px is unseen. Help now explains the first-empty target.
- **Muted lane (2026-09-10):** grey state colour, MUTED word, ring off, waveform at 30 % in its state
  hue — does it carry from 1.5 m, and should the wave go grey instead of dim green?
- **Lane pills at the default window (2026-09-10):** 20 px tall / 9 px text at 1280×820 (24 px at
  1920) while the keyboard ribbon takes 134 px there — a taller pill rung, and/or a slimmer ribbon
  default (`DEFAULT_STAGE_WEIGHTS`) so the lanes get the height?
- **Failed plugin bundles (2026-09-10):** a bundle whose scan child failed (timeout, crash, unparsable)
  is remembered as failed and simply missing from the picker; only the log says why. Show it greyed
  with the reason, or keep the picker clean and leave it to the log?
- **Non-hue state cues (2026-09-22, built, unseen):** toast severity glyph (⚠ error / ✓ done) in a
  16 px accent column beside the stripe; the system lamp's warn ring; the record meter's 1 px ring at
  clipping and the 3 px AUTO notch. Right size, right weight?
- **"On" pills (2026-09-23 restyle):** engaged CLICK / FIXED / AUTO REC / RETAKE / END STOP is a
  warm-white legend on a lifted face with an edge, no hue — reads as ON at a glance, or add `● CLICK`?
- **Slider focus ring (2026-09-30):** every slider's hit area is 24 px tall now, and a
  keyboard-focused slider's ring wraps that box, 12 px taller than before. Fine, or tighten it?
- **Audio Settings popover (2026-09-22):** non-modal for the keyboard yet modal for the pointer.
- **Empty plugin scan note (2026-09-22, built):** a muted mono note per slot, "No plugins found · CLAP
  in … · rescan ⟳ in the command bar", hidden below 640 px window height so the drum pads stay
  reachable; 9.5 px and wrapping to two lines since 2026-09-30 (it computed to 7 px). Wording, and
  should it live in the picker's place instead of its own row?
- **Fallback copy (2026-09-22, built):** the STOPPED lane core's "play first to overdub"; slots read A/B
  on screen but "slot 1/2" in ARIA labels.

## By ear, when convenient

- Click volume (`lf.clickVolume`). Pad/kit gain balance. Synth-attack clicks. Plugin output-gain
  default. Synth presets (DEPRIORITIZED — a re-listen left it unsure they are off-key).
- MIDI feel: mod-wheel vibrato rate/depth (5.5 Hz, 0..0.35), pitch-bend, CC64 sustain; vibrato is
  bypassed at depth 0 (wheel engage from rest must not click).
- Three product calls from the first code review, each open to reversal:
  pointer-clicked buttons blur so Space/Enter always drive the selected track; sustain-pedal state
  survives a slot/synth swap; vibrato bypass at depth 0.
- Open polish: SR spoken output; the ~530 ms beat-LED resync at commit; the free-run
  metronome "1" anchored to an arbitrary wall moment + the REC accent-jump (a "stable downbeat" job).
- **Click default (2026-09-18 jam):** should the click be ON by default while recording?

## Parked ideas (for the long run)

- Looper aesthetic forks: ring/state colour = state vs track-identity; Day/Night.
- Undo as visible history (layer count on ↶ UNDO); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- A visualizer in the stage view (a tester's idea, 2026-09-30): each track drawn as something that
  moves with its own sound. The look is open; BleepLoop builds its own and shares no code with other
  projects. The UI already holds each lane's waveform bins and playhead, so a wave-style view needs no
  engine change. A spectrum needs per-lane band levels in the feed. Neither sees lane FX or the live
  input.
- The README clip, next round (owner, 2026-09-30): a pointer that clicks through a jam as a player
  would (a lane's FX, a plugin swap, IN FX), and slow pans and zooms onto what changes.
  `verify/probes/readme-media.mjs` steps frames on a paused clock, so Playwright's recorded cursor
  does not apply: draw the pointer in the page, and compose the camera over the captured frames.

## Craft — `pnpm check` + screenshots, no ear

- **Vocabulary:** one feature, several words: END STOP / ENDING / STOPPING AT LOOP END / ■ NOW,
  AUTO REC / LISTEN / WAITING FOR INPUT, track vs lane vs take in COPY's aria/title/Help (`Looper.tsx`,
  `Transport.tsx`). Help itself now covers every control (2026-09-30). [reader]
- **CSS discipline:** four pill implementations disagree on padding, radius and engaged alpha
  (`.tgl`, `.transport__tgl`, `.lp-pb`, `.fxp-mod__toggle`), plus two steppers (22 vs 24 px), three
  button resets and three visually-hidden copies; 18 font sizes and no type tokens; a few one-off
  colour literals near a token (`#f6f1e7` on the tempo numeral, `--text`/`--engaged` at an alpha in
  two glows and the splitter grip). [reader]
- **Component seams:** `hasMaster`/`loopBars`/`anyTrackIn` are re-derived in three components;
  `Transport.tsx` rebuilds `toggleInput()`'s three false-cases from booleans [verified]; two effects
  write signals where a memo or JSX binding would do (`Looper.tsx`, `Transport.tsx`), and COPY's
  visibility rides `canReverse()` while a refused copy is silent (`Looper.tsx`). [reader]
