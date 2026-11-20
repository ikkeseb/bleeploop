# MASKIN 2: the panel direction, third pass (the lead designer's direction, after the owner's answer of 10:58)

The design's idea in words, as the mockups beside this file were drawn. Earlier passes and the
owner's notes on them stayed in the design job's scratch space; the mockups here are the passes he
accepted.

## What he said (read by the lead designer; the file has his words)

- The parts are acceptable one by one ("the elements do not look bad, seen in isolation"): keys
  with depth, the fader, the pan knob, a panel feel. Keep that material; it is programmed CSS.
- **The top is cluttered and hard to find one's way in** (`uoversiktlig`, hard to orient). He said
  it of this direction and of the flat one. The record-mode keys and their value fields (`70`,
  `SENS 50`) are laid out "a bit off".
- The fonts are still wrong (the tempo, the dB read-out).
- The round lane buttons look "childish, or simple" and need a rethink.
- He is not sure the waveform suits this design.
- Flat can work too. Two generated pictures, the photoreal one included, he rejected: do not chase
  a photograph.
- What he asks for: more whole, more polished, more thought through.

## The idea of this pass: order first

**One instrument, read in three bands, each with one job, and nothing in a band that does not
belong to its job.** The eye must find its way without reading: the top band is TIME and the
whole machine, the thin band under it is WHAT IS PLAYED IN, the body is THE FIVE LOOPS. Depth is
spent only on what is pressed, turned or slid, exactly as in the parts he accepted; everything
else is flat panel, print and dark glass. Fewer things, larger gaps, one alignment grid.

Hold it against Neural DSP's plugin faces, Universal Audio's plugins, Elektron's panels, the
Boss RC-505 MK II: each is calm because every control has an obvious home. Never copy a face.

## The three bands (1920x1080)

### 1. Command bar: ONE row, about 76 px tall

Left to right, in four groups separated by clear gaps of panel (at least 28 px; a fine printed
vertical tick between groups is allowed, a box is not):

1. `BLEEPLOOP` (logotype, small) and **the one display**: a recessed dark glass window, wide and
   low (about 460x56), the only display in the bar. On it: the tempo `120` as the largest figure
   with `BPM` and `4/4` small beside it and the padlock tiny; four beat lamps (the second lit);
   then the loop: `4 BARS` and the position `2.2`; the loop's progress as ONE fine line along the
   display's lower edge with four bar ticks, 31 % lit. Two type sizes on the glass. `TAP` is a
   key directly right of it.
2. **Transport**, three keys of one height: `■ ALL`, `FADE` and `✕ ALL` (the recessed destructive
   key).
3. **Record modes**, five latched keys of one size in one row, each with its LED bar: `CLICK`,
   `FIXED`, `RETAKE`, `AUTO REC`, `END STOP`.
4. **Master** (the stereo LED meter with its scale, the knob, the value `100`) and the seven
   tools as small printed line icons with no keycap (a compact cluster, two rows of icons is
   fine): nobody plays them.

**A key carries its own value. There are no separate value fields and no steppers in the bar**
(this moves a thing; it is the lead designer's proposal and the round says so): `FADE` shows
`2 BARS`, `CLICK` shows `70`, `FIXED` shows `4 BARS`, `AUTO REC` shows `50` as a second, smaller
line inside the key in the numerals face (bright when the mode is on, secondary when off);
`RETAKE` and `END STOP` have none. The value is changed by dragging or scrolling on the key.
Every key in the bar has the same height and sits on the same baseline; legends align.

### 2. Slots: ONE thin row, about 60 px tall

Slot A takes about two thirds, slot B the rest, divided by a milled groove. A slot is a signal
line read left to right, not a rack of plates:

`A` (the slot letter, printed) · the input `In 1` (a small value window with a chevron) · `LIVE`
(a latched key with its LED) → the units as low plates of one height (about 44 px) on a fine
signal line with small arrowheads → the empty `+` bay → at the slot's right end its thin
horizontal LED meter and its volume (a small knob and `-0.9 dB`).

A unit plate, native or loaded, has ONE anatomy: a round power lamp-button at its left (bypass),
the unit's name, and at its right end one small thing that says what it is: a native unit shows
a tiny live miniature of what it does (OTT: three small band bars moving; ECHO: three fading
taps), the loaded plugin shows a small `VST3` mark and the opens-a-window glyph (its EDITOR).
PARAMS is not a key any more: it lives in the plate's menu (a small chevron at the plate's very
end, the same on every plate: editor, parameters, move, remove). Nothing touches or passes a
plate's edge. A bypassed unit (ECHO): lamp dark, name in the secondary tone, the signal line
dimmed where it passes it. Clicking a native unit opens its face below the row (not shown open
on the main screen).

### 3. The five lanes: everything below, five strips of equal height (about 182 px each)

A lane is head | glass | mix, and the columns align to the pixel across the five.

- **Head (about 280 px).** The lane number printed large (about 64 px, the numerals face) with
  the state word under it as a lit legend in the state colour, no plate behind it (`FROM A`
  beside OVERDUB); the main button (the variant chosen from `button.png`); `STOP` (a key) and
  `CLR` (the smaller recessed key). The lane's state is the first thing read in the lane.
- **Glass.** ONE pane for all five lanes, recessed, dark (not pure black: `#07090b`), a faint
  reflection gradient at its top edge. The waveform as chosen from `wave.png`. Bar numbers
  `1 2 3 4` and bar lines as faint display graphics; one crisp playhead through all five lanes
  at 31 %.
- **Mix (about 400 px).** One row of keys: `FX`, `MUTE` (latched, LED) and `REV`, `UNDO`, `EDIT`
  (momentary), two groups by spacing alone. Under it one line: the level fader (recessed slot,
  cap, printed scale with the 0 dB mark), ONE small value window with the level and the pan
  (`-2.5 dB`, `L20`), and the pan knob.
- Lane 3 (selected, overdubbing from slot A) is the lane the eye goes to: its strip one tone up,
  its head the brightest. No outline. Lanes are divided by a milled groove.

## Material (the parts he accepted, made consistent)

Neutral-cool charcoal panel (about `#15171b`; never brown, never navy), with a very fine grain
(`parts/relay/out/panel-grain.png` is a grey grain tile-able enough at 1672x941: overlay it at
low strength, or use a fine SVG noise) and a light from the top of the screen: each band is a
touch lighter at its top than at its bottom. A key: a low cap with a 1 px top highlight, a face
that falls slightly, a 2-3 px side wall, a short contact shadow, radius about 4 px; never a pill,
never a 1 px outline as its only form. Keys sit directly on the panel: no wells round groups, no
group captions, no boxes round values except the small dark value windows. An LED is a small
solid shape with a tight bloom (at most 6 px); engaged chrome is warm white `#fff3e0`; colour is
spent on sound only (REC `#ff3b52`, OVERDUB `#ffae2a`, PLAYING `#45d295`). Meters are LED ladders:
green to -12 dB, amber from -12 to -3, red above -3; at the moment shown (master about -6 dB) the
last lit segments are amber and the red ones are visible unlit.

## Type

Three roles as CSS custom properties in the page's CSS (`--font-legend`, `--font-num`,
`--font-name`), so a change is one line. Until the owner picks from `type.png`, build with
candidate A: Instrument Sans (`../fonts/InstrumentSans-Variable.ttf`) for all three; legends 600
caps with 0.06em tracking, numerals 500 with `font-variant-numeric: tabular-nums`, unit names
650-700. Legends never under 12 px.

## What must not change

The order (command bar, slots A | B, five lanes; a lane is head | glass | mix), every word, number
and state in `content.md` (except: PARAMS moves into the plate's menu, and the bar's values move
into their keys, as above), the state colours' hues, the next-action glyph on the main button,
three silhouettes for latched, momentary and destructive keys.

## Where it turns to slop

A border round everything; boxes of equal weight; many small things in a ragged row; grey on
grey; a control without an obvious home; ornament of hardware that reports nothing (screws,
wood, brushed streaks, wear).
