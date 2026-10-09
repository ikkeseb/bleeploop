# Stage look: "Scope"

**A working document.** It lives here while the look is open, and when the look lands what still binds
moves into `src/ui/AGENTS.md`, `docs/backlog-taste.md` or a comment at the call site, and this file is
deleted (`AGENTS.md`: keep working documents few and short-lived). The data it draws is built and
machine-verified; the look is not built.

`id: 'scope'`, `name: 'Scope'`, first in `STAGE_VIEWS` so it is the default; Orbit and Strata stay
behind the V switch.

## The idea

**Light is the proof.** Every column is drawn ADDITIVELY at low alpha, and the sweep is one master
loop long. A pass that lands on the pass before it therefore accumulates light, so a locked loop
GLOWS. A one-off, a dub's new layer, an FX sweep, a fade, live guitar, peaks dim and dies. Nothing
has to move for the picture to say which lanes are locked and which are alive, which is what carries
it from across the room.

This is the correction a taste read landed on the first draft of this spec: opaque columns made
"locked reads as a standing wave" a figure of speech, since a first pass and a hundredth pass are
pixel-identical under `source-over`. Additive makes it literal. The sweep being one LOOP, not one bar,
makes it exact for a loop of any length: a four-bar loop folded into one bar's width smears even when
it is perfectly locked.

Honest about what it is: the columns are a live min/max envelope at 4 ms, not a cycle-accurate
oscilloscope trace. The light, not the waveform's detail, is the information.

## Two layers, and why both

Each lane bay draws TWO things, and the order matters:

1. **The contour, the lane's identity.** The recorded take's envelope, dim and static, in the existing
   vocabulary: `laneChanged`, `peaksInto`, `sampleEnvelope`, `contourColor`, `contourAlpha`,
   `holdsAudio`, `mutedLook`, the same helpers Orbit and Strata use. It says *this lane holds eight
   bars shaped like this*, and it is visible when the lane is muted, stopped or recording, when its
   live output is silence.
2. **The live light, written over it.** The additive scope columns. They say *and this is what it is
   doing right now*.

This is not the first draft's mistake repeated. There the recorded envelope WAS the picture; here it is
the quiet substrate at low alpha and the live light is the event. It also earns its place three ways
the live layer cannot: a MUTED lane still shows its loop in grey, a STOPPED lane shows it brighter
than muted, and a take being recorded still draws in rec-red. All three are asserted per look by
`verify/probes/stage-view.mjs`'s `pixels` group, and a live-only look fails them because a muted,
stopped or recording lane has no output to draw. Read that group before you draw anything: its
thresholds are the spec.

## Layout, top to bottom

Six bays, inside the HUD's insets (`hudPad(h)` at the sides, the chips row above, `hudMsg(h)` for the
message line below):

| Bay | Shows | Height |
|---|---|---|
| hero | the engine's output (`SCOPE_MASTER`) as the filled additive body, and the monitor (`SCOPE_MONITOR`) as a line over it | 2.0 units |
| 1..5 | each lane after its FX | 1.0 unit each |

The hero bay is the only place two drawing languages meet, and it is where the eye should go: what
comes out of the speaker now, in one region instead of two. The monitor is a 1.5 px `pal.cyan` line
with a cheap glow (one wide low-alpha pass, one narrow bright pass, never `shadowBlur`), riding over
the master's body. During a first take the lanes are silent and the master is nearly the monitor, so
the guitar still owns the top of the screen.

Each bay is a well: `pal.surf1`, ONE hairline of top light at low alpha, square corners. No bottom
edge in the state colour, no second border: the trace carries the colour, and the app's surface
language gives a bay one hairline and a tone step, nothing more. A left gutter of about 44 CSS px
carries the bay's name, `OUT`/`IN` stacked in the hero and `1`..`5` below, in **Geist Mono**,
`pal.faint`, and `pal.engaged` for the selected lane. The owner's call (2026-10-09): `src/ui/AGENTS.md`
reserves Geist Mono for changing numeric read-outs, but that rule is written for the command bar and
the lane chrome, where the eye jumps between numbers. A full-screen performance view is a different
context and mono reads as instrument panel, which is what this look is for. The rule is worth
narrowing to the looper chrome rather than the whole app; the owner decides whether that text changes. The selected lane also gets a `pal.engaged` bar on its left edge, as Strata already does, and
its well one tone step brighter.

There is no room wash and nothing breathes on the bar. Nothing in the sound breathes per bar, the
owner has already corrected the stage-lighting reading of this view once, and the hero bay is the
room's level.

## The trace mechanic

One offscreen canvas, created and sized in `layout` (never per frame), `dw x dh`, holding every bay's
LIVE trace in its own band. Its context is set to `globalCompositeOperation = 'lighter'` in `layout`.
The contour layer is drawn straight onto the main canvas each frame, under the blit, cached by
`laneChanged` as Orbit and Strata cache it.

Per frame:

1. Pull the new columns through `looper.scopeInto(view)` into a caller-owned view, allocating nothing.
2. For each new column, draw one vertical line on the offscreen, in that bay's band, from its `min` to
   its `max` scaled to 70 % of the bay's half height, at x = the column's **loop** phase x width, at
   an alpha near 0.3 from `ALPHAS`. Column width is `ceil(width / columnsPerLoop)`, at least 1 device
   px; where a loop is long enough that several columns share a pixel, fold them (the lowest min, the
   highest max) into that one column.
3. Blit the offscreen onto the main canvas with `drawImage`.
4. Draw the live beam over it: one wide line at low alpha and one 1.5 px line at full, at the current
   x.

At each sweep's end (the x wraps), fade the whole offscreen once:
`globalCompositeOperation = 'destination-out'` with alpha 0.55, one `fillRect`, then back to
`'lighter'`. A pass therefore retains about 45 %, and a column that lands on itself every pass settles
near `0.3 / 0.55` of full light while a one-off tops out at 0.3. One composite per sweep, never per
frame.

**Cap the accumulation** so a loud locked lane settles near 0.8 of its own colour and never climbs
toward white: white is `pal.engaged`, which means *selected* in this app, and a locked loop must not
look selected. The per-column alpha and the fade factor are the two tuning knobs; expose them as named
constants with that reasoning in a comment, because this is what the owner's eye will judge first.

**The sweep's span.** One master loop: `columnsPerLoop = looper.masterFramesValue() / bin`, and a
column `k` behind the newest sits `k * bin` frames earlier in the loop. Loop phase comes from
`light.phase`. Draw the bar lines as faint ticks (`feed.beatsPerLoop / 4` of them) so the grid still
reads inside a multi-bar sweep. With no master loop yet (`beatsPerLoop === 0`), sweep one bar from the
clock's own beat fields in `light` and a fixed fallback window, so a first take still moves.

**A gap** (the scope mirror's epoch moved: a ring overran, a device-frame skip, a new engine) means
the next columns do not follow the ones before them. Do NOT clear the offscreen: an xrun costs one
2.7 ms window, and wiping a four-second trace for it is a full-screen blink mid-jam, which the owner
will read as a bug. Just stop drawing the beam for that frame and let the next sweep's own fade carry
the old trace away. A `reset` (a WebView reload, a device change, a 48 kHz switch) is the one case that
clears, because the view it is resyncing to is a different engine.

## Colour

The app's language, unchanged. Lanes take `lightColor(pal, i)`: `rec`, `dub`, `faint` when muted, else
`play`. The hero's body is `pal.engaged` at low alpha, going `pal.rec` while `light.clip`; the monitor
line is `pal.cyan`.

## Count-in and reduced motion

`layout` writes `countX`, `countY`, `countSize` so the numeral sits centred over the lane stack; the
drawing dims under it while `feed.counting`. Under `feed.reduced`: no persistence, no beam glow, just the
current sweep's columns alone, drawn still, opaque rather than additive.

## Turning the taps on, and off

The engine folds nothing until something sends `Command::SetScope(true)`, so this look's module is
what switches the data on. Two traps the review found before the look existed:

- **Send `false` when the view closes or unmounts.** The command is a remembered host setting
  (`Key::Scope`), replayed into every new engine on a rate change, a fault rebuild or a device switch.
  Nothing in the tree sends `false` today, so the first code that turns it on owns turning it off, or
  the taps run for the rest of the session with nothing drawing them.
- **Never send `false` and `true` inside one audio block.** The engine's master column is folded after
  the block's limiter has run, so a close and a reopen landing in the same block (2.7 ms at 128 frames)
  can leave one 4 ms column covering both sides of the switch, with no gap flagged. Found in review,
  left unfixed on purpose: the trigger needs the view to close and reopen faster than a person can, and
  the cost is three pixels at a moment this look clears its own canvas anyway. Just do not drive the
  command from anything that could fire twice.
- **Do not inherit the latch across a reload.** A `reset` frame carries the remembered settings and
  `adoptSettings` ignores `SetScope` silently, so a reloaded page can be looking at an engine whose
  taps are already on while its own store thinks they are off. Send the state this view wants on mount
  rather than assuming the engine's.

## What the hero bay can and cannot say

After the fix round the master source is the engine's whole output: after the limiter and with the live
monitor summed in. So the body is honest about what the speaker plays, the limiter's gain reduction
included.

Three limits stay, and the drawing must not pretend otherwise.

1. A lane's own column is tapped after its FX but BEFORE its pan and without its share of the shared
   reverb bus, so a hard-panned or reverb-heavy lane reads quieter in its own bay than it sounds in the
   room.
2. The master passes the limiter, which delays what it carries by its pre-delay (about 6 ms, around one
   and a half columns), so a transient appears in its lane's bay about 1.5 columns BEFORE it appears in
   the hero. Accepted: relabelling the master's columns would break the one frame sequence every source
   splices by, and the two bays are far enough apart on screen that four pixels of offset is invisible.
   If the owner's eye ever catches it, that is the decision to revisit.
3. A driver period dropped without an overload report leaves the frame sequence intact, so no gap fires
   and the trace draws straight through a break the player heard.

## `hit(x, y)`

The lane whose bay contains `y`, else `-1`. The hero bay is not a lane and returns `-1`.

## What must not slip

- **Allocate nothing in the steady state.** The probe's budget is under 32 bytes a frame. Use `ALPHAS`
  for every fractional alpha, no template strings, no `toFixed`, no array or object literal, no
  gradient or pattern per frame. Set every composite mode and style that does not change in `layout`.
- **Mean frame script time under 6 ms** at 1920x1080, and the frame must run at least 240 times in
  20 s. The per-frame work is a handful of new columns plus one `drawImage` and a few lines; keep it
  that way.
- **The stage-draw guard.** Only `./`-sibling imports; no `solid-js`; from `../state/audio` only
  `looper`, `PEAK_FRAMES`, `PeakView`, `TrackState`; and `looper.` only for the names in the guard's
  `PLAIN` set, which now includes `scopeInto`. Everything else comes from `feed` and `light`.
- **A resting lane must rest.** `restMoved < 40` px over 300 ms once every loop is stopped, against
  `playMoved > 400` for a playing one. So a stopped lane's live trace must finish fading and then hold
  completely still: the per-sweep fade cannot keep nibbling at it forever. Settle it to zero and stop
  compositing that band.
- **Every assert in the `pixels` group, per look.** They are written against the recorded contour, so
  the contour layer must carry them: `play.play > 1500`, `empty.play === 0` with
  `empty.lit < play.lit / 3`, `muted.play < play.play * 0.02`, `muted.grey > empty.grey + 500`,
  `stop.play < play.play * 0.02` with `stop.bright > muted.bright + 1000`, `dub.dub > 60` with
  `dub.play > 500`, `rec.rec > 300`. Check the census helper's own colour classification before you
  pick an alpha: an additive 0.3-alpha green may not classify as green. The hero bay's `pal.engaged`
  body counts into `lit` and `bright` too, so verify it does not break `empty.lit` or `stop.bright`.
- **`legibility` and `count`:** a chip number's cap height stays at least 14 px and the count-in
  numeral's box at least 120 px tall.
