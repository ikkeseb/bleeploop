# src/ui/ — frontend briefing

The root `AGENTS.md` routes here — read this before any work in this subtree (`CLAUDE.md` beside it
is a one-line adapter). UI-only edits are safe while the dev app runs.

- **Stay fresh-eyes:** keep proposing ideas and viewpoints, render + screenshot; the owner's eye is
  the gate. Type: Geist for words, Geist Mono only for changing numeric read-outs (both vendored,
  `src/assets/fonts/`); no display/retro font. The surface language ("Instrument": matte tone steps,
  colour only for sound, warm-white = engaged) and its tokens live in the `src/app.css` header — use
  the tokens. `looper/waveform.ts` reads the colour tokens once per lane mount: a token edit over
  HMR shows stale canvas colours until a page reload.
- **The rendered app is the looper-UI spec.** Before looper-UI work, run `pnpm probe contact-sheet`
  and look at `logs/contact-sheet/index.html` (fixed scenes at three window sizes); the `src/app.css`
  tokens define the surface. Composition that binds: the looper is the hero (~65 % of the height) with
  five full-width horizontal lanes (linear peak/body waveforms over a bar grid), one command bar owns
  the whole transport, the source row is compact and the keyboard is a thin ribbon. Each lane: a
  REC/DUB-only round core with a separate PLAY/STOP + CLR pair, **MUTE** as the 2nd right-cluster cap,
  volume **0..1.5** with a 0 dB detent at 1.0.
- **Empty-lane right cluster is disabled by design** (`disabled={isEmpty()}` in `Looper.tsx`) — ask
  before changing it; decision D4: volume on an EMPTY track stays non-settable.
- **Layout:** `layout/SplitStack.tsx` is the N-pane seam and panes never remount on
  resize/keyboard-move. On-screen piano/pad keys are deliberately pointer-only (avoids a
  53-tab-stop tab trap); the computer-keyboard mapping is the pointer-free *fallback* play path
  (hiding the keyboard pauses it). Not built: free-form panel drag/move + inline VSTs that FILL a
  panel — `SplitStack` + `layout-store` is the seam.
- **Invariant 6 lives here:** the 60 fps canvas draw loop reads a plain mutable object, never a
  signal (`looper/waveform.ts` reads non-reactive looper getters). Measured cost + the fix
  pattern: `docs/ARCHITECTURE.md` invariant 6.
- **Engine mode:** components take `looper`, `clock`, `master` and `sampleRate` from
  `state/audio.ts`, which picks the web or the engine implementation; a direct import from
  `src/audio/` bypasses engine mode without an error at runtime. `verify/guards/audio-facade.mjs`
  fails it; its exceptions are the web-only paths, each with its reason.
- **One lane derivation:** a lane's display state, word, well message and count-in come from
  `looper/lane-state.ts`; the looper lanes and the stage view (`src/ui/stage/`) both read it, so a new
  state lands there once.
- **Error toasts** (`toast/Toasts.tsx` renders `src/notify.ts`) sit ADDITIVELY beside the
  `console.error` sites, which feed the release log — keep both.
