# The main screen: design principles

What BleepLoop's main screen stands for, in the order a conflict between them is settled. They draw
on the owner-approved "Instrument" surface language (the token header in `src/app.css`, which owns
the values) and on the owner's notes from the design rounds of 2026-10-28 to 11-07. A working
document of the climb in `HILL.md`: when the climb lands, what still binds folds into
`src/ui/AGENTS.md` and the `src/app.css` header, and this folder goes.

The screen is played standing, guitar in hand, about 1.5 m from a 27" 2560x1440 display, feet on a
pedal. It is an instrument, not a DAW window.

## 1. Read from the chair, without reading

Each lane's state (empty, armed, recording, overdubbing, playing, stopped, muted, fading) and where
the loop is read within a second from 1.5 m, by colour, fill and the playhead alone. A state word
confirms; it never carries the state alone. Two states that look alike from the chair are a defect.

## 2. Colour is sound

Red records, amber overdubs, green plays, as on an RC-505's rings. Nothing else on the main screen
spends hue: no amber for a chrome warning, no red for an idle record key, no green for a mode. What
is silent is tone: a muted or stopped lane's wave is grey, quieter than any sounding one, and still
plainly there. Chrome that is engaged is lit warm-white.

## 3. The wave is the face of the instrument

The take is what the player looks at most. It is drawn as sound: a smooth, anti-aliased, filled
shape with a confident body, never a bar chart, a stair-stepped outline or a ghost. It holds up at
full size on a 2560 screen beside the waveform rendering of the best DAWs. Its colour is its state,
its brightness its importance (playing over stopped over muted), and one crisp playhead per row
lines up down the display.

## 4. Calm by default; alive only where sound is

Nothing glows, pulses, washes or decorates on the main screen. Surfaces separate by tone and one
hairline of top light, never bevels, drop shadows or blur. The only motion is the playhead, the take
filling in and a count-in. Glow and spectacle belong in the stage view.

## 5. One hierarchy: lanes, then transport, then slots, then the keyboard

The five lanes are the instrument and take the eye first; the command bar second; the plugin slots
third; the on-screen keyboard is a fallback and recedes into the body.

## 6. One machine, one key family

Every pressable word is one key: one face, one height per row, one radius scale, one engaged recipe
(a lit legend on a face a step up), one disabled recipe (a faint legend, still present). One type
scale (Geist; Geist Mono with tabular figures for changing numbers). The lanes are rows of one body
and their waves one display. Nothing reads as bolted on.

## 7. Every pixel earns its place

No stray marks, decorative dots, unlit lamps that read as dust or labels too small to read from the
chair. Columns align across rows and across the drawer; a group's heading spans its group.

## 8. An evolution, not a new product

A little more like today's design, open to new ideas: the climb refines the Instrument language the
owner approved and keeps the fixed layout (the command bar on top, slots A | B, five lanes as head |
wave | mix, the FX drawer under its lane).

## Reference products

Named as words, never as screenshots or targets: Ableton Live's Session view (calm grid, legibility
at a glance), Teenage Engineering's OP-1 field (one material, few colours each with a meaning, the
display as the hero), Apple's Logic Pro (craft and waveform rendering). The Boss RC-505 MK II is the
model the looper follows: its ring colours are the state colours here.
