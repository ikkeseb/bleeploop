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
- **Invariant 6 lives here:** the 60 fps canvas draw loop reads a plain mutable object, never a
  signal (`looper/waveform.ts` reads non-reactive looper getters). One exception: the bar grid reads
  the BPM and sample-rate signals only when the master loop's length changes, and caches the bar
  count (`waveform.ts` `rasterise`). The stage view's own loop (`stage/stage-loop.ts`) holds the same
  line through a plain feed that Solid effects write, checked by `verify/guards/stage-draw.mjs`.
  Measured cost + the fix pattern: `docs/ARCHITECTURE.md` invariant 6.
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
