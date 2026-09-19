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
- **Help popover:** wording + section order (REV row, CLICK/FIXED naming, volume-detent line, the new
  MIC line + Session section, "FIXED N" / "AUTO N"). Overdub is "OVERDUB" (lane word) / "overdubs"
  (Help) / "Overdubbing" (screen reader) across three surfaces — one word?
- **Audio Settings:** the "rec align" trim row after the buffer row — placement + wording; status chips
  moved into the diagnostics block (unratified).
- **Lanes:** the selected-lane cyan edge (subtle under green PLAYING — tune, or a left rail); the
  ARMED amber dashed-ring pulse; the 8 px lane state word; from 1.5 m nothing carries (state word,
  12 px REC core, four 5 px beat dots) — the "stage view" idea; undo/reverse cap placement (a per-track
  properties surface?); the empty well says nothing at all (no first-run guidance in the looper).
- **Looper prominence:** does the first-run stage read looper-as-hero? `DEFAULT_STAGE_WEIGHTS` in
  `layout-store.ts` (keyboard 0.55 / looper 2.0, instrument = autoSize, keyboard BOTTOM). Keyboard
  de-emphasis is open — rebalance, don't remove (default-hidden / slimmer strip).
- **Plugin drawer:** one by-eye lap with a real plugin drawer (params scroll INSIDE the card, header
  pinned); slot vertical-expand.
- **Grey on grey:** the TEXT tokens measure AA-clean — do NOT re-tune them; the residual is
  surface-on-surface (`--well` α .04 vs `--glass` α .045, same hue) + the lane state word. A LIVE
  `src/app.css` token session over HMR; agent recommendation from the four `--well` variant shots:
  dark-step.
- **Keyboard transport feel:** 1–5 / Space / Enter (drum mode: pads own 1–4, only 5 selects); silent
  no-op on a refused Space (toast or cue?).
- **COPY pill (2026-09-18):** "⧉ COPY" after REV in the lane's pill row, hidden while no lane is
  EMPTY, always targets the first EMPTY lane — right place and word? Floated: a right-click
  context menu (none exists in the app yet) for picking the target lane. The pill row with DUB + REV
  + COPY all showing at 1280 px is unseen. Help popover does not mention COPY yet.
- **Muted lane (2026-09-10):** grey state colour, MUTED word, ring off, waveform at 30 % in its state
  hue — does it carry from 1.5 m, and should the wave go grey instead of dim green?
- **Lane pills at the default window (2026-09-10):** 20 px tall / 9 px text at 1280×820 (24 px at
  1920) while the keyboard ribbon takes 134 px there — a taller pill rung, and/or a slimmer ribbon
  default (`DEFAULT_STAGE_WEIGHTS`) so the lanes get the height?
- **Failed plugin bundles (2026-09-10):** a bundle whose scan child failed (timeout, crash, unparsable)
  is remembered as failed and simply missing from the picker; only the log says why. Show it greyed
  with the reason, or keep the picker clean and leave it to the log?
- **Audio Settings "sample rate: soon" (2026-09-10):** reads as a placeholder in a product; show the
  live context rate read-only until the C2 selector lands?

- **Minimum window:** at 960 × 600 with the keyboard visible, lane controls are vertically clipped. Improve minimum-height layout while keeping the normal five-lane proportions.

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
- **ASIO slot swap (D6 answered (b)):** "new track, new amp" needs UNLOAD of the live slot first —
  teach that in UI/copy; wording and placement open.
- **Click default (2026-09-18 jam):** should the click be ON by default while recording?
- Fixed auto-commit enters playback ~C+drain (~137 ms) late on pass 1 only.

## Parked ideas (for the long run)

- Looper aesthetic forks: ring/state colour = state vs track-identity; Day/Night.
- Undo as visible history (layer count on ↶ DUB); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- Pedal mode: most USB footswitches send keystrokes, so 1–5/Space/Enter already work — a Help section
  + a one-key UNDO binding; MIDI-CC foot control next.
