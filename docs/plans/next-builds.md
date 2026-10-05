# Next builds: specified, not built (2026-10-04)

A working document: each section is a reviewed spec a writer can build from. When a section lands,
fold what still binds into the briefings and delete the section; delete the file when it is empty.
Line numbers are as of commit `a43e9bc4` and move.

Order: D25 is built on the branch `d25-trim-rule` and waits on the owner (STATUS D25). Delayed
COPY waits on the owner (STATUS D29). D21 and per-lane pan landed: D21's rules live in
`src-tauri/src/engine_io/settings.rs`'s header, the lf-engine briefing and
`src/ui/state/engine-store.ts`; pan's law and glide in `src-tauri/crates/lf-engine/src/effects.rs`.

## D25: the WASAPI join's trim rule

The owner's decision (2026-10-04): skip the rerun with other apps closed and change the trim rule;
the setpoint stays. A second model family's read: the settled setpoint is unchanged, but audio the
old rule dropped stays queued until the controller drains it, and the join aligns a take by the
setpoint, so a take in that window could be accepted but late. The replay measures it.

- Rule: in `src-tauri/src/engine_io/pipes.rs` `PullPipe::pull_piece`, trim when the fill exceeds
  `2 * target + ceil(n * in_rate / out_rate)`, a nominal pull allowance (not the exact input the
  resampler takes). Same drop-to-target action, controller state and counters. Share output builds
  the same pipe: it is in scope.
- Proof without hardware: bounded fixtures of the logged join traces (the `j` lines of the
  2026-10-03 `native:engine` runs: nine trimming or starving opens; no whole logs, no local paths)
  under `src-tauri/src/engine_io/fixtures/wasapi-join/` (not yet built), replayed by a `cfg(test)`
  module through the real pipe as push and pull operations (fill evolves; the old rule must first
  reproduce the traced outcomes), reporting each open's excess queued input and how long it lasts.
  Two seeded witnesses must be red under the old rule and green under the new: a fill of 2508 frames with a 480-frame pull, and 2609 with a late 960-frame double pull. Threshold tests
  (480 frames at 48 kHz: 2880 does not trim, 2881 does; 960; unequal rates); a stalled puller still
  trims once; the open that only starves still starves (this rule does not fix starves); a take over
  a tolerated burst commits (the join helper in `callback.rs`'s tests).
- Rig after (the seat, on the PC): `pnpm native:engine --cycle=asio64,wasapi --seconds=3
  --switches=60 --swaps=0 --mute=1 --amp= --proq=` until at least 85 WASAPI opens, and the no-ASIO
  cycle (`--backend=wasapi --buffer=default --cycle=wasapi`) until at least 43. Baseline: of 85, 8
  opens trimmed, 2 starved, 9 faulty; of 43, 1. Record the counts in `docs/VERIFY.md` and the native
  briefing, and close or narrow D25.

## Delayed COPY: waits on the owner (STATUS D29)
