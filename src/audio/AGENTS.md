# src/audio/ — audio engine + looper briefing

The root `AGENTS.md` routes here — read this before any work in this subtree (`CLAUDE.md` beside it
is a one-line adapter). Decisions + the master signal flow (buses, the SILENT `recordTap` branch,
limiter placement): `docs/ARCHITECTURE.md` § Audio architecture — read it before touching
`engine.ts` or bus wiring. Invariants 1–5 (root `AGENTS.md`) bind every file here.

## Before you edit

- **Check that the dev app is closed** (the `app` process) and ask for it to be closed if not.
  Vite hot-reloads each save into the live WebView; a reloaded audio module builds a NEW engine on
  top of the old one, whose loops keep playing — every save stacks another layer of sound, and the
  mangled app may refuse to close.
- **Read the `OWNS:` line.** Every `looper/*.ts` and audio owner file opens with one, naming the
  decisions it owns (where master length is decided, where BPM locks, where C is applied). Pure grid
  + compensation arithmetic lives in `looper/grid-math.ts` and `record-latency-math.ts`, which
  `verify/` imports directly — extend those rather than copying math into a verifier.
- **After looper/capture/state-machine changes run `pnpm verify:jam`.** `pnpm verify` drives the
  real looper on a fake audio layer (`verify/README.md`); it cannot see real render timing, the
  browser or WebView2.

## One grid

Looper↔click sync = ONE ctx-time grid. Click, LED, count-in (always-on 1 bar; free-record stays the
default), fixed-length record, undo and reverse all ride `masterStartTime` + the exact loop
`beatPeriod`; reverse is in-place, click-free and **blocks overdub**.

## Record-latency compensation

BUILT: timestamped capture windows, measured output terms and C frozen at first record use, plus the
bridge queue's shift since the freeze. Between takes the bridge keeps hop-2 on its setpoint, never
during one (`plugin-bridge.ts`).
BleepLoop monitors natively, so input+plugin latency CANCELS; C compensates the record path only and
is 0 unless a native monitor is armed. The formula and its freeze: the header of
`record-latency-math.ts`; it is APPLIED in `looper/machine.ts` (see its `OWNS:` line). Guardrails, rig
protocol and debug levers: **`STATUS.md` § Stop 1** — read it before ANY latency/rig work.

## Gotchas — do not relearn

- **The anti-flam clamp SURVIVES pulse unification** — B→B load-bearing at commit, not
  handoff-only.
- **`FxChain.dispose()` on clear is NOT a quick win** — `clear()` keeps `t.fx` alive on purpose
  (the wiring rides `t.gain`); a bare dispose tears out the routing and needs a lazy rebuild.
- **`clear()` resets FX in lockstep with vol/mute** — an owner product call, not an oversight.
- **`Math.round(bpm)` in `clock.ts` is a negligible red herring** (0.011 ms, no accumulation).
- **Live notes are scheduled at `ctx.currentTime + 5 ms`** in `input-router.ts`, bypassing Tone's
  100 ms lookAhead (the ~110 → ~17 ms input-latency win) — keep live notes off Tone's transport.
- **Uncompensated latency, by choice:** inline PitchShift FX; the mic path (its fix is the L3
  wizard, not a guess-term).
