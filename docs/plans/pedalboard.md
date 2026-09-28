# The hands-free looper (Pedalboard mode): landed, and the not-built list

Decided 2026-09-23 from the product lens over that day's audit. The promise heads `README.md`: a
guitarist's hands are on the guitar, so every looper action must be reachable by foot. The keys (a
keystroke footswitch sends them) and learned MIDI messages reach the named actions of
`src/app/actions.ts` through `src/app/transport-keys.ts` and `src/app/midi-actions.ts`.
The looper already exposed every action, so the milestone was adapters, and nothing here needed
the rig to prove correctness. The milestone landed; this file now holds the not-built list and goes
when that list is folded into `docs/backlog-taste.md`. Of the engine-bound asks, F8 (synth plugins on
the device clock), F14 multiply and F15 input FX landed with the engine; D12 (controller data, the
wheels, to plugin slots) is not built.

## Pieces, in build order

| # | Piece | Size | Seam | Proof |
|---|---|---|---|---|
| 1 | Refusal cue — **landed**: `refuseOnLane` in `src/ui/looper/gates.ts`, drawn in the lane's well by `Looper.tsx` | M | `src/ui/looper/` | `pnpm verify:jam` § keys: a refused Space shows its reason on the selected lane only, and the cue leaves by itself |
| 2 | Action layer + keys — **landed**: the named-action table in `src/app/actions.ts`; ↑↓ / PgUp PgDn / ←→ next/prev, Backspace UNDO, Delete twice CLEAR in `src/app/transport-keys.ts` | S | `src/app/` | `pnpm verify:jam` § keys: UNDO restores the exact pre-dub PCM and redoes, next/prev wrap, CLEAR refuses a single press, one after another looper key (arrow or digit) or after the window, and clears on a double press |
| 3 | MIDI learn — **landed**: a learned CC/note runs its action per port and channel, in `src/app/midi-actions.ts` behind the consume-first hook of `src/ui/state/midi.ts`; the learn row in Audio Settings | M | `src/app/`, `src/ui/state/midi.ts`, `src/ui/settings/` | `pnpm probe midi-learn`: a learned CC survives a reload and records; momentary, latching and reversed pedals fire once per press, on the press; unlearned CC64/1/123 reach the router; a learned CC64 does not sustain, a learned note does not sound |
| 4 | Rig recall — **landed**: `src/ui/state/rig-recall.ts`, run by `src/app/boot.ts` after the scan; each slot's last plugin reloads through `restorePlugin` (the `selectPlugin` path), with its tone on the engine, never armed and never over a source the player picked first, and an in-flight marker turns a launch that died restoring into one skipped recall. Each slot's input channel is remembered with its source (`src/ui/state/audio-devices.ts`) | M | `src/ui/state/`, `src/app/` | `pnpm probe rig-recall`; `pnpm native:recall`: a restart brings back both plugins and the channel, a WebView reload and a close right after it the plugins, all unarmed; a launch killed while restoring makes the next one skip |
| 5 | Help, first screen ordered by the promise — **landed**: guitar, then the looper keys, then the pedals; the MIDI/synth lines move to an "Other layers" section below Transport | S | `src/ui/settings/Help.tsx` | `pnpm probe contact-sheet` scene `6-help`: the three sections fill the first screen at 1280×820 and 1000×700; wording and order stay taste (`docs/backlog-taste.md` § Help popover) |
| 6 | Foot vocabulary — **landed** 2026-09-27: every looper control by foot. The lane actions (REC/DUB, PLAY/STOP, UNDO, CLEAR, MUTE, REV, COPY) take a target in the learn row (the selected track, or 1–5; REC/DUB on a track also selects it); TAP, CLICK, END STOP, FIXED, FADE and IN FX's sends are global actions; the engine resolves every press (its lane, HALVE's bars, HOLD's lane) and every press disarms a pending CLEAR; HOLD on a momentary REC/DUB pedal; the learn reads a pedal from its release whatever the hold, and each binding line switches its kind; the stage view shows BAR n / N | M | `src/app/`, `src/ui/settings/`, `src/ui/stage/` | `pnpm probe midi-learn`: targets, the global toggles, HOLD, a 1.2 s learn hold, a release after the panel closed or during the next LEARN, the kind switch, old bindings, the engine fake; `pnpm probe stage-view`: the bar counter across 2- and 4-bar loop boundaries |

The only ear or foot moment is one pedal press during the next "Play first" jam; it rides that
session (`STATUS.md` § Play first) instead of adding a stop. The stage view landed after the actions
(2026-09-26): `src/ui/stage/`, B or the named action `stageView`, so a pedal can open it; its eye lap is
a `docs/backlog-taste.md` line. Tone recall landed after it (2026-09-27): each plugin's
settings come back with it at every load and ride in a session export (`src-tauri/src/host/tone.rs`,
`src/ui/state/slot-tones.ts`); its ear check rides STATUS Stops 5 and 8.

## Explicitly not built

- **FREE tempo-setting first take, 3/4 and 6/8.** `beatsPerBar = 4` runs through grid math, click and
  count-in; no owner or tester ask; gate-adjacent timing code.
- **Scenes and songs; panel drag; Day/Night; more built-in synths.** Taste or capacity with no effect
  on playing; each adds its own eye lap. Built-ins fill layers, they do not compete with plugins.
- **Recent-jams shelf:** keep the last N recovery archives on ✕ ALL and close, offered in IMPORT (M).
- **Per-lane pan; resample on import/recovery** (S each). No pan exists today.
- **One `.pill` primitive + type tokens; a first-run cue on the selected empty lane** (M, S). The
  findings behind them: `docs/backlog-taste.md` § Craft.
- **Rhythm guide:** three GM-kit grooves on the master pulse as an alternative to the click (M,
  ear-gated).

## Open questions that would change this plan

- Whether the owner plays with a footswitch or MIDI foot controller, and what it sends. None → the
  pick moves to the first downloadable release (v0.1.0, published 2026-09-28).
