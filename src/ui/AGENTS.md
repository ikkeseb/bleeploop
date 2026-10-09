# src/ui/: frontend briefing

The root `AGENTS.md` routes here: read this before any work in this subtree (`CLAUDE.md` beside it
is a one-line adapter). UI-only edits are safe while the dev app runs,
`state/` excepted (the root `AGENTS.md` rule).

- **Type and tokens:** Geist for words, Geist Mono only for changing numeric read-outs (both vendored,
  `src/assets/fonts/`); no display or retro font. The surface language ("Instrument": matte tone steps,
  colour only for sound, warm-white = engaged) and its tokens live in the `src/app.css` header; use
  the tokens. `looper/waveform.ts` reads the colour tokens once per lane mount: a token edit over
  HMR shows stale canvas colours until a page reload.
- **The rendered app is the looper-UI spec.** Before looper-UI work, run `pnpm probe contact-sheet`
  and compare against `logs/contact-sheet/index.html` (fixed scenes at three window sizes). What
  binds: guitar first, the looper as the hero with five lanes, one command bar for the whole
  transport, lane volume **0..1.5** with a 0 dB detent at 1.0, and the owner's decisions below.
- **Empty-lane right cluster is disabled by design** (`disabled={isEmpty()}` in `Looper.tsx`). Ask
  before changing it; decision D4: volume on an EMPTY track stays non-settable.
- **Layout:** `layout/SplitStack.tsx` is the N-pane seam and panes never remount on
  resize/keyboard-move. On-screen piano/pad keys are deliberately pointer-only (avoids a
  53-tab-stop tab trap); the computer-keyboard mapping is the pointer-free *fallback* play path
  (hiding the keyboard pauses it). Not built: free-form panel drag/move + inline VSTs that FILL a
  panel; `SplitStack` + `layout-store` is the seam.
- **Notes are native MIDI's:** `keyboard/Keyboard.tsx` keeps the gestures (pointer capture, key repeat,
  the note picked at press time, blur and unmount cleanup) and hands each hold and release to
  `platform.input` under its physical owner (`pointer:<id>`, `key:<code>`); ownership, sustain, the
  wheels and the note target are the native router's (`docs/ARCHITECTURE.md` § Decided: native MIDI).
  A key lights from its own press and from `state/midi.ts`'s held notes.
- **Invariant 6 lives here:** the 60 fps canvas draw loop reads a plain mutable object, never a
  signal (`looper/waveform.ts` reads non-reactive looper getters). One exception: the bar grid reads
  the BPM and sample-rate signals only when the master loop's length changes, and caches the bar
  count (`waveform.ts` `rasterise`). The stage view's own loop (`stage/stage-loop.ts`) holds the same
  line through a plain feed that Solid effects write, checked by `verify/guards/stage-draw.mjs`.
  Measured cost + the fix pattern: `docs/ARCHITECTURE.md` invariant 6.
- **A stage look draws two layers** (`src/ui/stage/`): the lane's recorded contour, dim and static,
  from `stage/visual.ts`'s shared helpers, and over it, where the look has them, the engine's live
  scope columns. The contour is not decoration: the stage-view probe's `pixels` group asserts per look
  that a MUTED lane draws its loop in grey, a STOPPED one brighter than muted and a take in flight in
  rec-red, and a live-only look has nothing to draw in those states. The columns cost engine work and
  are off until a look asks: the look carries `wantsScope` and `src/app.tsx` owns the ask, since
  `StageView.tsx` and the look mount only while the view is open and a page reloaded with it open
  would leave the taps folding for nobody. Additive light settles at `alpha / fade` per pass, which is
  also its ceiling, so keep the sum of every additive pass on one pixel below warm white, which means
  SELECTED here, and make the per-column strips TILE the trace: a strip that overlaps its neighbour
  takes its dim and its light twice and breaks that arithmetic.
- **The engine's names:** components take `looper`, `clock`, `master`, `session` and `sampleRate`
  from `state/audio.ts` (the engine store behind them: `state/engine-store.ts`). A gesture sends a
  command and the feed shows the outcome (invariant 3). A lane mix control (volume, pan, DUB FEEDBACK, FX)
  shows its gesture's value until the feed's `Mix` has it, and MUTE sends the engine's toggle; the rule
  lives in `state/engine-store.ts` (the lane mix section), the controls' side in `looper/mix-gesture.ts`.
  CLICK, END STOP, FIXED, RETAKE, AUTO REC and the IN FX sends send the engine's toggle too, and show
  the feed's `Toggled`, never their own flip (`state/engine-store.ts` `toggleSetting`).
- **One lane derivation:** a lane's display state, word, well message and count-in come from
  `looper/lane-state.ts`; the looper lanes and the stage view (`src/ui/stage/`) both read it, so a new
  state lands there once.
- **Every looper action is reachable by foot.** The looper's controls are the named actions of
  `src/app/actions.ts`; the keys (`src/app/transport-keys.ts`, which a keystroke footswitch sends)
  and learned MIDI (native, `src-tauri/src/engine_io/midi/actions.rs`, a port of its table; the
  actions the UI owns come back through `src/app/midi-actions.ts`) reach them only through it. A new
  looper control gets an action, and native MIDI's `ActionId` the same id (`midi-actions.ts`'s
  typecheck and `verify/fixtures/midi-wire.json` hold the lists equal), so a pedal can learn it. Not yet: CLEAR ALL, a TRIM
  other than half, and the continuous settings (volumes, pan, FX).
- **Error toasts** (`toast/Toasts.tsx` renders `src/notify.ts`) sit ADDITIVELY beside the
  `console.error` sites, which feed the release log. Keep both.

## Open threads (non-gate)

- **The stage beam and the HUD disagree where no scope column has arrived.** In
  `logs/stage-view/1920x1080-scope-later-take.png` the HUD reads bar 3 of 8 and lane 2's take in
  flight ends at 0.32 of the sweep, which agrees with it, while the beam stands at 0.75. With no
  column the beam is the DRAWN playhead (`scope.ts`: `beam[0] = -2`, then `light.phase`), so the two
  readings come from different places and one of them is wrong in that picture. Likely confined to the
  browser rig, where the look runs with no columns at all; on a device the taps are on whenever SCOPE
  shows, so the drawn playhead carries the beam only for the first few frames. Unknown whether the
  scripted scene's anchor simply disagrees with its own events. Next diagnostic check, cheap: a probe
  case that reads the HUD's bar and the beam's x in one no-column scene and asserts they agree, before
  anyone reads the beam as a defect on a device.
