# Next milestone: the hands-free looper (Pedalboard mode)

Decided 2026-09-23 from the product lens over that day's audit. The promise heads `README.md`: a
guitarist's hands are on the guitar, so every looper action must be reachable by foot. The keys (a
keystroke footswitch sends them) and learned MIDI messages reach the named actions of
`src/app/actions.ts` through `src/app/transport-keys.ts` and `src/app/midi-actions.ts`.
`src/audio/looper/looper.ts` already exposes every action, so the milestone is adapters: capture,
compensation and the rig-guarded algorithms stay untouched, and nothing here needs the rig to prove
correctness. This file is deleted when the milestone lands; what still binds moves to the briefings.

## Pieces, in build order

| # | Piece | Size | Seam | Proof |
|---|---|---|---|---|
| 1 | Refusal cue — **landed**: `refuseOnLane` in `src/ui/looper/gates.ts`, drawn in the lane's well by `Looper.tsx` | M | `src/ui/looper/` | `pnpm verify:jam` § keys: a refused Space shows its reason on the selected lane only, and the cue leaves by itself |
| 2 | Action layer + keys — **landed**: the named-action table in `src/app/actions.ts`; ↑↓ / PgUp PgDn / ←→ next/prev, Backspace UNDO, Delete twice CLEAR in `src/app/transport-keys.ts` | S | `src/app/` | `pnpm verify:jam` § keys: UNDO restores the exact pre-dub PCM and redoes, next/prev wrap, CLEAR refuses a single press, one after another looper key (arrow or digit) or after the window, and clears on a double press |
| 3 | MIDI learn — **landed**: a learned CC/note runs its action per port and channel, in `src/app/midi-actions.ts` behind the consume-first hook of `src/audio/midi.ts`; the learn row in Audio Settings | M | `src/app/`, `src/audio/midi.ts`, `src/ui/settings/` | `pnpm probe midi-learn`: a learned CC survives a reload and records; momentary, latching and reversed pedals fire once per press; unlearned CC64/1/123 reach the router; a learned CC64 does not sustain, a learned note does not sound |
| 4 | Rig recall, frontend only: restore each slot's plugin path and input channel at launch through the existing load path; GO LIVE stays one press. Plugin tone state excluded | M | `src/audio/instrument.ts`, `src/audio/native-io.ts` | native probe under `tauri dev` (a plugin, no guitar): restart, both slots reload the same path and channel; arming stays with STATUS Stops 5/6 |
| 5 | Help: a "Pedals" section (keystroke footswitches, MIDI learn); first screen ordered by the promise | S | `src/ui/settings/Help.tsx` | eye lap |

The only ear or foot moment is one pedal press during the next "Play first" jam; it rides that
session (`STATUS.md` § Play first) instead of adding a stop. Deferred inside the milestone: the stage
view (eye-gated, needs the actions first) and VST3 tone-state recall (L Rust; VST3 save/load is "not
wired" and LoadState cancellation needs a design).

## Explicitly not built

- **D12 full controller IPC to CLAP/VST3.** MIDI into a plugin stays on the WebView latency path
  whatever the IPC carries; the S sustain alternative is built (STATUS D12-S).
- **Native monitor path for synth plugins (tester F8).** Changes native buffering: needs the L1+L2
  measurements first (`docs/ARCHITECTURE.md`), and the promise ranks synth layers second.
- **Native looper core.** Falsified unless the L1/L2 gates say otherwise; it would end Mac development.
- **VST3 plugin tone-state recall.** See above.
- **Multiply (a later take k× master).** Adds unheard seams on top of the tile seams already owed to
  the ear (`docs/plans/tester-feedback.md`). Revisit once those are heard.
- **FREE tempo-setting first take, 3/4 and 6/8.** `beatsPerBar = 4` runs through grid math, click and
  count-in; no owner or tester ask; gate-adjacent timing code.
- **Scenes and songs; panel drag; Day/Night; more built-in synths.** Taste or capacity with no effect
  on playing; each adds its own eye lap. Built-ins fill layers, they do not compete with plugins.
- **Input FX.** Owner request, but the design is open: the native monitor bypasses Web Audio, so the
  player would not hear what gets recorded. Design first.
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
- Whether a MIDI-keyboard player without ASIO or guitar (the tester's setup) is someone BleepLoop is
  for. Yes → F8 becomes a roadmap item.
- Whether the first downloadable release comes before any new feature.
