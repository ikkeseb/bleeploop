# Taste backlog — NOT a gate

Feel, wording, placement. No functional risk anywhere on this list; nothing here blocks a build, a
release or a rig stop. The owner reads it with the app open, says yes / no / change,
and lines leave. Agents append ONE line per finding (an audit's taste finding lands here, never as a
`STATUS.md` stop) and never re-rank the owner's answers. Provenance for a line lives in the commit that added it,
not here.

## One eye-lap, `pnpm dev:asio`, screen by screen

- **Command bar (2026-09-02):** the two-row form whenever one row cannot fit (CLICK/FIXED/AUTO/TAP on
  row 2 at the Tauri default 1280); the ⬇/⬆ EXPORT/IMPORT icons in the tool cluster; the 4 px record-level
  meter left of MIC (−60..0 dBFS, green, red within 1 dB of full, a cyan tick at the AUTO trigger
  level while AUTO is on) — big enough? right place? AUTO reserves its sensitivity control's width
  so toggling it or changing the value keeps the stage in place.
- **Count-in numeral (2026-09-02):** the big red 4-3-2-1 in the armed lane's well over "COUNT-IN" —
  size, colour, and whether a later take's "WAITING FOR DOWNBEAT" should count beats to the boundary too.
- **Error toasts:** bottom-right stack — placement, copy, feel; may overlap the bottom lane's right
  cluster when full. The failure toasts: "Monitor device lost — switched to the backup path",
  "Guitar/line input device lost", "Input device lost — mic disarmed", "MIDI device disconnected —
  <name>", "Track N: the take failed to start", "Track N: playback failed to restart", the dual-slot
  editor refusal, import-of-a-bad-file.
- **Help popover:** wording + section order (guitar GO LIVE, ASIO slot swap, separate MIC path,
  COPY, REV, CLICK/FIXED/AUTO REC, volume-detent line and Session section). Overdub is "OVERDUB" (lane word) / "overdubs"
  (Help) / "Overdubbing" (screen reader) across three surfaces — one word?
- **Play-path signals from 1.5 m (2026-09-23):** nothing in the looper zone says the amp-sim is live
  (only the slot's pill); the 4×22 px record meter is the only clipping cue; the BPM lock pulse fires
  mid-count-in beside the big "4", which is rec-red inside an amber ARMED lane.
- **Plugin name at 1280 px (2026-09-23):** at the default window the slot header reads "Probe Amp Sim…"
  (the VST3 tag gives way to the ellipsis) while ~100 px stand empty before INPUT LIVE; at 1000 px it is
  "Probe Amp …". The picker beside it still reads "Probe Amp Sim (vst3)". Give the name that width, or
  is the picker enough?
- **Plugin row at 1000 px (2026-09-23):** with a plugin loaded, idle or INPUT LIVE alike, slot A stacks
  three rows (GO LIVE / EDITOR, PARAMS, the picker) and grows ~84→122 px over a synth slot; each lane on
  the play path gives up ~6 px. A tighter plugin row, or accept three rows at this width?
- **Synth pills at 1000 px (2026-09-23):** the Lead pill sits on the tail of "SOURCE · SYNT…" in both
  slots and covers its ellipsis. Pills off the label's row, a shorter label, or none in synth mode?
- **Audio Settings:** the "rec align" trim row after the buffer row — placement + wording; status chips
  moved into the diagnostics block (unratified).
- **MIDI learn row (2026-09-23, built, unseen):** above diagnostics — action picker + LEARN (a cyan
  LISTENING while it waits), then one line per binding (action · CC/note · channel · momentary or
  latching, the port on hover, ✕ forgets). Right place and words? Is "latching" clear to a guitarist?
- **Lanes:** the selected-lane warm-white edge (no rail, owner 2026-09-23) — strong enough under
  PLAYING? the ARMED amber dashed-ring pulse; the 10 px state word with its LED; from 1.5 m little
  carries (state word, REC core, four 5 px beat dots) — the "stage view" idea; undo/reverse cap placement (a per-track
  properties surface?); the empty well says nothing at all (no first-run guidance in the looper).
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
- **Minimum window:** at 960 × 600 with the keyboard visible, lane controls are vertically clipped. Improve minimum-height layout while keeping the normal five-lane proportions.
- **Non-hue state cues (2026-09-22, built, unseen):** toast severity glyph (⚠ error / ✓ done) in a
  16 px accent column beside the stripe; the system lamp's warn ring; the record meter's 1 px ring at
  clipping and the 3 px AUTO notch. Right size, right weight?
- **"On" pills (2026-09-23 restyle):** engaged CLICK / FIXED / AUTO REC / RETAKE / END STOP is a
  warm-white legend on a lifted face with an edge, no hue — reads as ON at a glance, or add `● CLICK`?
- **Hit targets under 24 px (2026-09-22):** `transport__step` 22 px, `toast__close` 20 px, the `lf-range`
  12 px band.
- **Audio Settings popover (2026-09-22):** non-modal for the keyboard yet modal for the pointer.
- **Empty plugin scan note (2026-09-22, built):** one muted mono line per slot, "No plugins found ·
  CLAP in … · rescan ⟳ in the command bar", hidden below 640 px window height so the drum pads stay
  reachable. Wording, and should it live in the picker's place instead of its own row?
- **Fallback copy (2026-09-22, built):** "INPUT LIVE · WEB MONITOR" after a native monitor fault; the
  STOPPED lane core's "play first to overdub"; slots read A/B on screen but "slot 1/2" in ARIA labels.

## By ear, when convenient

- Click volume (`lf.clickVolume`). Pad/kit gain balance. Synth-attack clicks. Plugin output-gain
  default. Synth presets (DEPRIORITIZED — a re-listen left it unsure they are off-key).
- MIDI feel: mod-wheel vibrato rate/depth (5.5 Hz, 0..0.35), pitch-bend, CC64 sustain; vibrato is
  bypassed at depth 0 (wheel engage from rest must not click).
- Four product calls from the first code review, each open to reversal:
  pointer-clicked buttons blur so Space/Enter always drive the selected track; sustain-pedal state
  survives a slot/synth swap; export master excludes STOPPED tracks; vibrato bypass at depth 0.
- Open polish: SR spoken output; mic-arm flow; the ~530 ms beat-LED resync at commit; the free-run
  metronome "1" anchored to an arbitrary wall moment + the REC accent-jump (a "stable downbeat" job).
- **ASIO slot swap (D6 answered (b)):** Help now explains that another amp slot needs UNLOAD of the
  first plugin, even after INPUT LIVE is turned off. Check whether that is discoverable enough.
- **Click default (2026-09-18 jam):** should the click be ON by default while recording?
- Fixed auto-commit enters playback ~C+drain (~137 ms) late on pass 1 only.

## Parked ideas (for the long run)

- Looper aesthetic forks: ring/state colour = state vs track-identity; Day/Night.
- Undo as visible history (layer count on ↶ DUB); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- Input FX (owner request): delay/stutter/reverb BEFORE the record tap, printed into the take, beside
  today's per-track post FX. Open design: the native monitor bypasses Web Audio, so the player would
  not hear what is recorded; C and grid-synced stutter need their own answer.

## Craft — `pnpm check` + screenshots, no ear

- **Help and vocabulary:** `Help.tsx` never mentions RETAKE, END STOP, ▶/■ ALL or ✕ ALL, and its
  trigger in `app.tsx` is named "Keyboard & layout help" though it is the only product guide; one
  feature, several words: END STOP / ENDING / STOPPING AT LOOP END / ■ NOW, AUTO REC / LISTEN /
  WAITING FOR INPUT, track vs lane vs take in COPY's aria/title/Help (`Looper.tsx`, `Transport.tsx`);
  the tempo numeral's `aria-label="BPM"` hides the value and its locked title says "loop length"
  (`Transport.tsx`). [verified: Help, trigger; reader: words, tempo]
- **Accessibility:** the keyboard show/hide cap (`app.tsx`) and GO LIVE / EDITOR
  (`PluginControls.tsx`) still flip their label AND set `aria-pressed` ("Hide keyboard, pressed"); 30
  tab stops reach lane 1, 14 of them synth pills (a roving-tabindex `radiogroup` per slot removes 10);
  the `transport__beats` div carries an `aria-label` with no role. [reader]
- **CSS discipline:** four pill implementations disagree on padding, radius and engaged alpha
  (`.tgl`, `.transport__tgl`, `.lp-pb`, `.fxp-mod__toggle`), plus two steppers (22 vs 24 px), three
  button resets and three visually-hidden copies; colour literals bypass tokens in `src/app.css` (the
  baked `%2346d4e8` caret without the other caret's INVARIANT note, `#14171d` ×4,
  `rgba(148,168,215,.15)` ×5, `#6b7387` ×2, the popover shadow ×3); 18 font sizes and no type tokens,
  the empty-plugin-scan note computing to 6.5–7 px at ≤1280 (`plugin-controls.css`); synth pills
  render mixed case through the spec's uppercase `.tgl`, not a listed delta in `src/ui/AGENTS.md`.
  [reader]
- **Component seams:** `hasMaster`/`loopBars`/`anyTrackIn` are re-derived in three components;
  `Transport.tsx` rebuilds `toggleInput()`'s three false-cases from booleans [verified]; two effects
  write signals where a memo or JSX binding would do (`Looper.tsx`, `Transport.tsx`), and COPY's
  visibility rides `canReverse()` while a refused copy is silent (`Looper.tsx`). [reader]
