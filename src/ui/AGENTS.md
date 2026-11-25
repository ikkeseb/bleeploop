# src/ui/: frontend briefing

The root `AGENTS.md` routes here: read this before any work in this subtree (`CLAUDE.md` beside it
is a one-line adapter). UI-only edits are safe while the dev app runs,
`state/` excepted (the root `AGENTS.md` rule).

- **Type and tokens:** Instrument Sans (vendored, `src/assets/fonts/`) in three roles, `--font-legend`,
  `--font-num` and `--font-name`, so a change of face is one line; no display or retro font. The wave's
  bar numbers (`looper/waveform.ts`) draw in `--font-num` too, every digit one advance wide.
  The surface language ("the panel": depth only on what is pressed, turned or slid; flat panel, print
  and dark glass for the rest; colour only for sound; warm white = engaged), its tokens and the shared
  recipes (`.k-key`, `.k-led`, `.k-glass`, `.k-win`, `.k-meter`, `.k-fader`, `.k-knob`) live in the
  `src/app.css` header: build a part from a recipe and the tokens, never a new depth of its own. The
  canvases (`looper/waveform.ts`, `stage/visual.ts`) read the colour tokens once per mount, as
  `#rrggbb`: a token edit over HMR shows stale canvas colours until a page reload.
- **The rendered app is the looper-UI spec.** Before looper-UI work, run `pnpm probe contact-sheet`
  and compare against `logs/contact-sheet/index.html` (fixed scenes at three window sizes). What
  binds: the instrument in hand first (the guitar the reference), the looper as the hero with five lanes, one command bar for the whole
  transport, lane volume **0..1.5** with a 0 dB detent at 1.0, and the owner's decisions below.
- **The main screen's principles,** drawn from the owner's design notes (2026-10/11); an earlier one
  settles a conflict. The screen is played standing, about 1.5 m from a 27" 2560x1440 display, an
  instrument in hand: an instrument, not a DAW window. How each is drawn (surfaces, colour, keys, the wave,
  selection, motion, sizes) is the `src/app.css` header's.
  1. *Read from the chair, without reading:* each lane's state and where the loop is read within a
     second by colour, fill and the playhead; a state word confirms, never carries alone. Two states
     that look alike from the chair are a defect.
  2. *Colour is sound,* as an RC-505's rings; what is silent is tone, quieter than anything sounding
     and still plainly there.
  3. *The wave is the face:* it holds up at full size at 2560 beside the best DAWs' waveforms; its
     colour is its state, its brightness its importance.
  4. *Calm by default:* motion only where it reports something (the playhead, a take filling in, a
     count-in, a fade, a brief confirm), never standing or decorative. Spectacle belongs in the stage
     view.
  5. *One hierarchy:* lanes, then the command bar, then the slots, then the keyboard (a fallback that
     recedes).
  6. *One machine, one key family;* nothing reads as bolted on.
  7. *Every pixel earns its place:* no stray marks or unreadable labels; columns align across rows
     and the drawer, and a group's heading spans its group.
  8. *An evolution, not a new product:* the layout stays (command bar on top, slots A | B, five lanes
     as head | wave | mix, the FX drawer under its lane).
  Reference products are named in words (Ableton Live's Session view, Teenage Engineering's OP-1
  field, Logic Pro), never copied from screenshots or made targets.
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
  count (`waveform.ts` `rasterise`); the meter's beats a bar reach it plain (`setGridBeats`). The stage view's own loop (`stage/stage-loop.ts`) holds the same
  line through a plain feed that Solid effects write. `verify/guards/stage-draw.mjs` checks both loops,
  the grid's exception by name and only inside its master-length gate, and the tuner's
  (`tuner/tuner-loop.ts`), which tells its Solid side the note, the in-tune state and the no-signal
  state through a callback, on change only; and the unit faces' (`units/ott-loop.ts`: the OTT face's glass
  and its plate's miniature; `units/echo-loop.ts`: the ECHO face's tape, its params and tempo a plain view
  the face's effect writes), which read the units' meter mirror (`units/unit-meters.ts`) the engine store
  writes from the feed's `unitMeters`, never a signal. The tuner overlay shows exactly while the feed's last
  `Tuner` event said on (every reset frame turns it off before its events apply); Esc and its close
  send `SetTuner(false)`, never a local flip.
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
  takes its dim and its light twice and breaks that arithmetic. EMBER's frame light blends with
  `screen` instead, so that sum binds only its one `lighter` pass (the ground rays' bake); its light
  must still read apart from SELECTED's warm white, and its ice, mixed from `--play`, must stay inside
  the play green the stage-view probe's census counts.
- **The engine's names:** components take `looper`, `clock`, `master`, `session` and `sampleRate`
  from `state/audio.ts` (the engine store behind them: `state/engine-store.ts`). A gesture sends a
  command and the feed shows the outcome (invariant 3). A lane mix control (volume, pan, DUB FEEDBACK, FX)
  shows its gesture's value until the feed's `Mix` has it, and MUTE sends the engine's toggle; the rule
  lives in `state/engine-store.ts` (the lane mix section), the controls' side in `looper/mix-gesture.ts`.
  CLICK, END STOP, FIXED, RETAKE, AUTO REC and the IN FX sends send the engine's toggle too, and show
  the feed's `Toggled`, never their own flip (`state/engine-store.ts` `toggleSetting`); GO LIVE's press
  sends the engine's toggle too, and what means a value (an unload's live-off, a reload's resume, an
  effect pick's auto-start) `SetSlotLive`; the cap shows the feed's `live` (`state/native-io.ts`).
- **The chain seam:** while `platform.chain.available` (the DEV fake with `__lfChainFake`; natively, DEV only,
  the DEV flag `localStorage['lf.chainNative'] = '1'`, read once per page: `src/platform/host.tauri.ts`) each slot's line draws its chain from the feed's `chains` (`units/ChainLine.tsx`,
  `state/chain.ts`): one plate anatomy, the `+`, moves by menu and drag, the OTT's and the ECHO's faces (one
  frame, `units/UnitFace.tsx`: home just under the plate, nothing drawn between them, moved and scaled by
  the player, kept per kind in `lf.unitFaces`; free-standing, drawn by `units/FaceHost.tsx` apart from the
  row, so a press beside one plays the looper, a fold leaves it open, and the stage view hides and keeps it,
  its draw loops idle meanwhile; one per kind, the last touched on top); otherwise the row is the fixed topology
  (`instrument/InstrumentSlot.tsx`), and the feed's
  `chains` apply to nothing. A plate moves
  when the feed says so, never on drop; a drop is judged at its release, from the plates' places when the drag
  began. A refusal the contract makes certain (a ninth unit, a source moved, an effect ahead of a source) is the
  slot's tag with nothing sent (`addRefusal`, `moveRefusal`), judged against the chain as the edits already
  sent leave it (a reloading source is still the source); the host resolves the rest. A gesture reaches only a
  seated unit, and a popover or a face of a unit that stops being seated closes. The IN FX pedals
  (`InFxEcho`, `InFxReverb`, `InFxRing`, `InFxOtt`) and their keys switch every built-in unit of the kind in
  both chains (`Toggle.Units`); the legacy sends stay off, their row undrawn.
- **One lane derivation:** a lane's display state, word, well message and count-in come from
  `looper/lane-state.ts`; the looper lanes and the stage view (`src/ui/stage/`) both read it, so a new
  state lands there once.
- **Every looper action is reachable by foot.** The looper's controls are the named actions of
  `src/app/actions.ts`; the keys (`src/app/transport-keys.ts`, which a keystroke footswitch sends)
  and learned MIDI (native, `src-tauri/src/engine_io/midi/actions.rs`, a port of its table; the
  actions the UI owns come back through `src/app/midi-actions.ts`) reach them only through it. A new
  looper control gets an action, and native MIDI's `ActionId` the same id (`midi-actions.ts`'s
  typecheck and `verify/fixtures/midi-wire.json` hold the lists equal), so a pedal can learn it. Not yet: CLEAR ALL, a TRIM
  other than half, the meter (a picker set before a loop, like the typed BPM), and the continuous
  settings (volumes, pan, FX).
- **Error toasts** (`toast/Toasts.tsx` renders `src/notify.ts`) sit ADDITIVELY beside the
  `console.error` sites, which feed the release log. Keep both.

## Open threads (non-gate)

- **Chain mode's activation.** The gate is `platform.chain.available` (`state/chain.ts` `chainShown`): with
  the DEV flag `lf.chainNative` off (always in release) `tauriChainHost` rejects every chain call before any
  IPC, so chain mode is the DEV fake's and that flag's alone. Built for it: contract §6's durable migration
  (`lf.rig`, `state/rig-recall.ts` `recallChains`, rules in `state/rig-record.ts`: the upgrade, the restore,
  the `pending` placement, the slot's level kept), the named pedal actions on `Toggle.Units` (native MIDI
  through `midi_subscribe_chain`, the UI's own press in `src/app/actions.ts`) and the legacy sends kept
  silent with their keys untouched (`state/engine-store.ts`). Not built: the activation itself (the flag
  and the legacy arms go) and `input_fx.rs`' retirement (contract §9, E3). Chain mode
  already routes notes to the slot (`{ Slot: n }`) and sends the slot's level as `SetSlotGain`, never the
  legacy `Builtin` target or `SetInstrumentGain`; a synth choice on a slot a seated SYNTH heads is a pick on
  it through the note router (§4, `state/chain.ts` `pickSynth`). Next check: a native restore on the PC
  (`tauri dev` with `lf.chainNative`, a v0.8.9 profile: the upgrade, a restart bringing each unit back
  under its id with its tone, a learned IN FX pedal on `Toggle.Units`).

- **Chain mode's edges on the fake, left at the branch's third review** (each read from source by a
  reviewer, none reproduced; the next engine step rewrites held edits and moves, so they are judged there):
  a drag whose chain shrinks to one plate ends without swallowing its release, so a release on the name can
  open Parameters (`units/ChainLine.tsx`, the unseat/alone effect); a fold with the focus in the `+` menu's
  portal loses it (the hand-off's `owned` flag sees plate portals only; its 1 s `lostAt` window is also
  fragile); `moveRefusal` (`state/chain.ts`) assumes the queued edits apply, while a scripted report clears the
  fake's queue, so a scripted waiting or leaving source makes the row and the fake disagree; a failed reload
  with the editor open leaves the UI thinking it closed (the fake keeps it open: the next press sends Open);
  the fake can emit `leaving` with `reloading` set, which contract §2 does not allow and the decoder accepts.
  The contract question beside them: a move's `to` indexes the chain as it stands, while the host applies
  it after the held edits ahead, so a held removal ahead shifts a target past 0 by one (translate in the UI,
  or index against the chain to come). Next check: a chain-ui case per edge on the engine step's fake.

- A reset that asks for the kept meter (`engine-store.ts` `meterAsked`) guards only its own frame: a
  later Transport that lands before the asked `SetMeter` applies writes the engine's 4/4 to `lf.meter`
  until that `SetMeter`'s own Transport writes the kept meter back. It is lost for good when the
  command ring refuses that `SetMeter` (`adoptSettings` ignores `sendEngine`'s result; native MIDI's
  queue sends the batch one command at a time, so the volumes before it can still land). A fix keeps
  the ask through later Transports until its echo, a refusal or the player's own pick ends it. Read
  from source, not reproduced. Next check: a meter-picker case with a non-reset Transport between the
  reset and the asked meter's echo, and one whose `SetMeter` the fake refuses.
