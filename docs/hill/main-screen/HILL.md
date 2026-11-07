# Hill: the main screen

A council-scored climb (the `hill` skill's § Council): the score is a judged panel, not a runner.
Working document; it lives on the climb's branch and goes when the climb lands (see
`PRINCIPLES.md`, whose principles the rubric draws on).

## Artifact

The main screen and its states in the browser rig, on the engine fake: the command bar, the slot
row, the five lanes (head | wave | mix), the FX drawer, empty and busy states. Captured by
`pnpm probe contact-sheet` (`CS_VIEWPORTS=2560x1440,1920x1080`, the keyboard at the bottom, as by
default), every capture under `flock /tmp/seat-heavy.lock`.

**Paths the climb may change:** `src/app.css`, `src/ui/**/*.css`, `src/ui/looper/waveform.ts`, and a
component's markup or class names where a look needs it (`src/ui/**/*.tsx`), never its behaviour.
**Never:** `src/ui/state/`, `src/platform/`, `src-tauri/`, the hot-swap work, `verify/` (the probes
score it; the one exception is the contact sheet's viewport override, taken from the r3 variants),
the rubric below, the packet's shots.

## Base, and why

Branch `hill/main-c1-glass`, from `dev` b6865c35 plus the r3 "B glass" variant's surface (CSS and
`waveform.ts`), one commit. In the r3 council (2026-11-06, four delegates, three families, blind)
B scored highest (Overall 7.7 against A 7.4, C 6.4, dev 5.4) and every delegate ranked it first: it
reads as one instrument, and it is the calmest. Its wave is not kept as it stands: the owner's note
on r3 was that dev's wave was better than every variant's, and three delegates saw stair-stepped
edges on the variants' waves. Round 1 therefore tests dev's kind of wave (a confident filled body)
made smooth, inside B's display, against both dev and B in one packet.

## Score

The council skill's panel, blind, the same packet to every delegate each round: `opus` @ high
(weight 1), `gpt-6.1-sol` @ high (1), `gpt-6-astra` @ medium (1), `gemini-4-argon-medium` (0.5).
The round's score is the weighted mean of the delegates' own Overall. Every round's packet holds dev
(today's app), the incumbent and the candidate, on shuffled letters; the gain is the candidate's
weighted Overall minus the incumbent's, in that same packet.

**Shots per page** (2560x1440 unless named): `13-jam` (and `13-jam` at 1920x1080), `1-empty`,
`2-first-take-recording`, `4-count-in`, `5b-fx-all-on`, `9-amp-sim-live`, `14-modes-on`.

## Rubric (PROVISIONAL: which qualities count is the owner's)

Each 1 to 10; Overall is the delegate's own judgment, not a mean. A 9 holds up beside Ableton
Live's Session view, Teenage Engineering's OP-1 field and Apple's Logic Pro.

- **Glance:** from 1.5 m, within a second, is each lane's state and the loop's position clear
  without reading a word? A 9: every lane's state reads from across the room.
- **Hero:** does the eye land on the lanes first, the transport second, the slots third, the keyboard
  last? A 9: the looper is plainly the instrument.
- **Wave:** does each take look like sound, drawn with the craft of the best DAWs? A 9: smooth,
  anti-aliased, a confident body, state and importance legible in it, no stair-steps or ghosts.
- **Calm:** is the screen quiet while a lot happens, with colour only for sound? A 9: nothing glows,
  pulses or decorates; every hue on screen is something sounding or recording.
- **One system:** surfaces, keys, type and spacing as one designed instrument? A 9: one material,
  one key family, one type scale, one rhythm.
- **Controls:** does every control read as pressable and its state as true from the chair? A 9:
  engaged reads instantly, disabled still reads as present, nothing ambiguous.
- **Craft:** alignment, edges, legibility, detail at full size. A 9: nothing off, cramped, stray or
  amateur under a close look.

## Target and budget

The owner's brief (2026-11-06 and 11-07): a few rounds of UI/UX lift on the main screen, principles
first, not run non-stop and not burning the whole quota; Opus is the builder. Every scored round goes
to the owner for review, the whole screen state by state, before and after.

**Stop at the first of:** the council's weighted Overall at 9 or above; two rounds in a row with a
gain under 0.5 against the incumbent in the same packet; four rounds; or the director's call to
round off (quota). Budget end is not approval: the result waits on this branch for the owner's
pick, and nothing lands on `dev` from here without it.

## Record

One row per round. Overall is the council's weighted mean in that round's packet.

| Round | Commit | Dev | Incumbent | Candidate | Gain | Kept | Tokens (k) per model | Wall |
|---|---|---:|---:|---:|---:|---|---|---|
| 1 | f3dbbfe1 | 5.2 | 5.2 (dev) | 7.8 | +2.6 | yes | build: opus 726 (four stages); council: opus 155, sol 120, astra 95, argon 80 | ~2 h |
| 2 | 65103a53 | 4.5 | 7.4 (r1) | 7.9 | +0.47 | yes | build: opus 412 (two stages); council: opus 163, sol 575, astra 222, argon 47 | ~1 h 15 m |

Round 1 notes: the packet held dev, the r3 B variant as it stood and the candidate. B fell from 7.7
(r3, six categories) to 5.2 here; its wave scored 3.1 against dev's 4.9, as the owner's note on r3 said.
Every delegate ranked the candidate first. Open for round 2 (all four agree): a bigger wave body, fading
legible without words, a 2 px playhead at 2560, the last non-sound hues in chrome (A LIVE, INPUT LIVE,
the status dot), larger lane and drawer type at 2560, a quieter keyboard, the selection's full outline.

Round 2 notes: the packet held dev, round 1 and the candidate. Three delegates ranked the candidate
first; the Opus delegate ranked round 1 first (7.5 against 7.0: the fading drain read as played against
unplayed, the hollow muted wave as an empty lane, the wave's dark rim as a pen line). The gain is under
0.5: one round of two toward the stop. Open for round 3 (three or four delegates agree): fading must
read from the whole body, not as a played/unplayed split; muted needs a body or a stronger contour; one
thin edge instead of the dark rim; FIXED and AUTO REC show their remembered "on" beside the lock; the
LOOP ring neutral while nothing plays; larger FX drawer type at 2560; the 1920 command bar's half-empty
second row; the lane number crowding A LIVE; selection that reads in the empty state.
