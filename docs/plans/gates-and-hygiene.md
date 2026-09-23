# Work order: real-code gates and agent hygiene (OPEN)

Pieces 4-6 of the 2026-09-23 fresh-eyes pass over code, gates, docs and agent workflow (pieces 1-3
landed: guards that drive the real looper and one probe harness, both owned by `verify/README.md`, and
comments/docs that state current intent). Build them in this order, one
piece per commit series, gates green before each push. Piece 6 waits on an owner decision. Taste findings from that day live in `docs/backlog-taste.md` and the hands-free milestone in
`docs/plans/pedalboard.md`; neither is repeated here. Delete this file when the last piece lands, after
folding what still binds into `verify/README.md`, the briefings or a call-site comment.

Piece 5 edits `src/audio/`: the dev-app rule in `AGENTS.md` applies.

## 4. Playbooks become commands

**Why.** `docs/VERIFY.md` and `src-tauri/AGENTS.md` describe procedures every agent re-reads and runs
by hand: `WSLENV` plus `VITE_LF_PROBE` plus a log plus a grep for the verdict line, the kill
discipline, `cargo.exe` from WSL for both feature sets, a separate Vite port. A script runs the same
way every time and works the same for Claude Code and Codex.

**Do.** Node scripts under `scripts/` (the WSL `pnpm` wrapper runs them on Windows node):
`pnpm native:smoke|survey|swap` (launch, wait for the verdict line, stop), `pnpm native:kill` (`app`,
`cargo` and the port-1420 owner, never every node process), and `pnpm rust:check` (`cargo.exe check`
with and without `asio`, then `cargo.exe test --no-default-features`). The docs keep a command table
plus the gotchas no script can carry.

**Done when** each command has run once on the PC and printed its verdict, and the prose it replaces
is gone from `docs/VERIFY.md` and `src-tauri/AGENTS.md`.

## 5. One recorder-session object

**Why.** One capture's state spans about 10 `engineState` fields, 5 module-level `let`s in
`src/audio/looper/machine.ts` and 6 overdub fields per track. Five sites reset it by hand:
`releaseRecorderState` (14 assignments), `stop`, `clear` (about 20), `rejectRecordLoss` and
`finishOverdub`. A forgotten field is a bug class.

**Do.** A pure refactor, with no behaviour change: `recording: RecordSession | null` in the looper
state, `overdub: OverdubSession | null` on `Track`. Releasing a capture sets the field to `null`.

**Done when** the rig guards (`pnpm verify`) and `pnpm verify:jam` are green, and the next "Play first" jam in
`STATUS.md` runs on this code before anything else lands in `src/audio/looper/`.

## 6. L2 loopback probe (owner decision)

`docs/ARCHITECTURE.md` names L2, a physical loopback measurement, as the latency gate; no tool exists.
Meanwhile the rig lap sits at its 10-stop cap and the last play was 2026-09-18. With one cable from
the interface output to its input, a DEV command could schedule clicks on the real master grid, record
through the native input path into the real looper, and report how far the recorded transient lands
from the grid, in frames and ms per buffer size. The cable plays exactly when the click is heard, so
it measures the alignment half of Stops 1 and 3; feel stays with the ear. Unknown: whether the WebView
output and the ASIO input can share the interface on the rig. Default if the owner says nothing: not
built.

## Small fixes, any time

- Diagnose the intermittent that keeps `verify/probes/record-stop-window.mjs` out of CI (its `@no-ci`
  line has the numbers), then drop the `@no-ci`.
- `retry` in `src/audio/midi.ts` lacks `@public`; only `verify/probes/instrument-controls.mjs` calls it, so
  knip reports it as unused.
- `verify/probes/contact-sheet.mjs` shows only the web tier (synth pills), never the guitar-first screen.
  Add scenes with `pluginHost.available` on and a fake amp-sim loaded, using the pattern in
  `verify/probes/plugin-slot-pending.mjs`.
- `README.md` points at `rust-toolchain.toml` as if it sat at the root; it lives in `src-tauri/`.
- `src-tauri/AGENTS.md` cites "§ WSL lane" in `docs/VERIFY.md` and "§ ASIO startup" in
  `docs/ARCHITECTURE.md`; neither is a heading.
