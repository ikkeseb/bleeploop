# Work order: real-code gates and agent hygiene (OPEN)

Pieces 5-6 of the 2026-09-23 fresh-eyes pass over code, gates, docs and agent workflow (pieces 1-4
landed: guards that drive the real looper and one probe harness, both owned by `verify/README.md`,
comments/docs that state current intent, and the native playbooks as `pnpm native:*`/`rust:check`,
owned by `docs/VERIFY.md`). Build them in this order, one
piece per commit series, gates green before each push. Piece 6 waits on an owner decision. Taste findings from that day live in `docs/backlog-taste.md` and the hands-free milestone in
`docs/plans/pedalboard.md`; neither is repeated here. Delete this file when the last piece lands, after
folding what still binds into `verify/README.md`, the briefings or a call-site comment.

Piece 5 edits `src/audio/`: the dev-app rule in `AGENTS.md` applies.

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

- `verify/probes/record-stop-window.mjs` stays out of CI (its `@no-ci` line has the numbers). Its main
  cause, stale `currentFrame` stamps, landed in a8738fa. Rerun it until the remaining miss recurs, read
  `firstMisses` in its log, fix, then drop the `@no-ci`.
