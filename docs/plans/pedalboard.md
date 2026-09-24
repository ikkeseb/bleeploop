# The hands-free looper (Pedalboard mode): landed, and the not-built list

Decided 2026-09-23 from the product lens over that day's audit. The promise heads `README.md`: a
guitarist's hands are on the guitar, so every looper action must be reachable by foot. The keys (a
keystroke footswitch sends them) and learned MIDI messages reach the named actions of
`src/app/actions.ts` through `src/app/transport-keys.ts` and `src/app/midi-actions.ts`.
`src/audio/looper/looper.ts` already exposes every action, so the milestone is adapters: capture,
compensation and the rig-guarded algorithms stay untouched, and nothing here needs the rig to prove
correctness. The milestone landed; this file now holds the not-built list and goes when that list
is folded into `docs/backlog-taste.md` or the engine plan. Engine-bound asks (D12 controller data to
plugins, F8 synth plugins on the device clock, F14 multiply, F15 input FX): `docs/plans/native-engine.md`
§ After the flip.

## Pieces, in build order

| # | Piece | Size | Seam | Proof |
|---|---|---|---|---|
| 1 | Refusal cue — **landed**: `refuseOnLane` in `src/ui/looper/gates.ts`, drawn in the lane's well by `Looper.tsx` | M | `src/ui/looper/` | `pnpm verify:jam` § keys: a refused Space shows its reason on the selected lane only, and the cue leaves by itself |
| 2 | Action layer + keys — **landed**: the named-action table in `src/app/actions.ts`; ↑↓ / PgUp PgDn / ←→ next/prev, Backspace UNDO, Delete twice CLEAR in `src/app/transport-keys.ts` | S | `src/app/` | `pnpm verify:jam` § keys: UNDO restores the exact pre-dub PCM and redoes, next/prev wrap, CLEAR refuses a single press, one after another looper key (arrow or digit) or after the window, and clears on a double press |
| 3 | MIDI learn — **landed**: a learned CC/note runs its action per port and channel, in `src/app/midi-actions.ts` behind the consume-first hook of `src/audio/midi.ts`; the learn row in Audio Settings | M | `src/app/`, `src/audio/midi.ts`, `src/ui/settings/` | `pnpm probe midi-learn`: a learned CC survives a reload and records; momentary, latching and reversed pedals fire once per press, on the press; unlearned CC64/1/123 reach the router; a learned CC64 does not sustain, a learned note does not sound |
| 4 | Rig recall — **landed**: `src/audio/rig-recall.ts`, run by `src/app/boot.ts` after the scan; each slot's last plugin reloads through `restorePlugin` (the `selectPlugin` path), never armed and never over a source the player picked first, and an in-flight marker turns a launch that died restoring into one skipped recall. The input channel stays the one global Audio Settings choice | M | `src/audio/`, `src/app/` | `pnpm probe rig-recall`; `pnpm native:recall`: a restart brings back both plugins and the channel, a WebView reload and a close right after it the plugins, all unarmed; a launch killed while restoring makes the next one skip |
| 5 | Help, first screen ordered by the promise — **landed**: guitar, then the looper keys, then the pedals; the MIDI/synth lines move to an "Other layers" section below Transport | S | `src/ui/settings/Help.tsx` | `pnpm probe contact-sheet` scene `6-help`: the three sections fill the first screen at 1280×820 and 1000×700; wording and order stay taste (`docs/backlog-taste.md` § Help popover) |

The only ear or foot moment is one pedal press during the next "Play first" jam; it rides that
session (`STATUS.md` § Play first) instead of adding a stop. Deferred inside the milestone: the stage
view (eye-gated, needs the actions first) and VST3 tone-state recall (L Rust; VST3 save/load is "not
wired" and LoadState cancellation needs a design).

## Explicitly not built

- **VST3 plugin tone-state recall.** See above.
- **FREE tempo-setting first take, 3/4 and 6/8.** `beatsPerBar = 4` runs through grid math, click and
  count-in; no owner or tester ask; gate-adjacent timing code.
- **Scenes and songs; panel drag; Day/Night; more built-in synths.** Taste or capacity with no effect
  on playing; each adds its own eye lap. Built-ins fill layers, they do not compete with plugins.
- **Built-in dry INPUT source; a second input channel.** Rust effort unknown; the promise assumes an
  amp-sim plugin. "Maybe later" (STATUS D2/D7).
- **Diagnostics copy:** a copy-diagnostics button, the version in Help, open-log-folder and an issue
  template, so a tester report carries commit and driver (S).
- **Recent-jams shelf:** keep the last N recovery archives on ✕ ALL and close, offered in IMPORT (M).
- **Per-lane pan; resample on import/recovery** (S each). No pan exists today.
- **One `.pill` primitive + type tokens; a first-run cue on the selected empty lane** (M, S). The
  findings behind them: `docs/backlog-taste.md` § Craft.
- **Rhythm guide:** three GM-kit grooves on the master pulse as an alternative to the click (M,
  ear-gated).

## Open questions that would change this plan

- Whether the owner plays with a footswitch or MIDI foot controller, and what it sends. None → the
  pick moves to the first downloadable release (`docs/plans/release-prep.md`).
