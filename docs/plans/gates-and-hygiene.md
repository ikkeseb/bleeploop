# Work order: real-code gates and agent hygiene (OPEN)

Pieces 5-7 of the 2026-09-23 fresh-eyes pass (7 from the 2026-09-24 loopback measurement) over code, gates, docs and agent workflow (pieces 1-4
landed: guards that drive the real looper and one probe harness, both owned by `verify/README.md`,
comments/docs that state current intent, and the native playbooks as `pnpm native:*`/`rust:check`,
owned by `docs/VERIFY.md`). Build them in this order, one
piece per commit series, gates green before each push. Piece 6 waits on an owner decision. Taste findings from that day live in `docs/backlog-taste.md` and the hands-free milestone in
`docs/plans/pedalboard.md`; neither is repeated here. Delete this file when the last piece lands, after
folding what still binds into `verify/README.md`, the briefings or a call-site comment.

## 5. One recorder-session object: landed, the jam is owed

One capture's state now lives in `engineState.recording: RecordSession | null` and one overdub's in
`Track.overdub: OverdubSession | null` (`src/audio/looper/state.ts`); releasing the recorder is one
write per field. The rig guards, `pnpm verify:jam` and `pnpm probe --ci` are green on it. **Open:** the
next "Play first" jam in `STATUS.md` runs on this code before anything else lands in
`src/audio/looper/`.

## After the jam: queued behind the looper freeze

- **A late lane start drops its head.** PLAY ALL (and any idle restart) reads one anchor, now + 20 ms
  (`machine.ts`), then starts the lanes one by one; `startPlayback` (`src/audio/looper/playback.ts`)
  clamps a start that arrives after its `when` to `ctx.currentTime` and shifts the offset, so the lane
  stays in phase but loses its first 0.3–15 ms on the first pass. The budget is ~10 ms of main-thread
  stall inside the gesture (render bursts of 384/512 frames eat the rest); no late lane in ~200
  unloaded or churned gestures, so a GC pause or preemption is needed. Rarer: a start that takes effect
  1–3 quanta after the clamp plays 128–384 frames behind the grid until restarted (13 of 247 late
  starts), and under heavy churn deferred graph changes gave 40–137 ms late starts (production
  relevance unknown). Proposed: clamp to `ctx.currentTime + 256 / sampleRate`, the lead `fx.ts`
  already uses (out-of-phase late starts fell from 13/247 to 1/203 under the same planted stalls), and
  make `verify/probes/playback-restart.mjs` assert each surviving lane's first sound at its own start
  time, in phase. Owner call: phase over head is the designed fallback (the comment at the clamp).
- `src/audio/looper/transport-actions.ts`'s header still calls itself the single action-routing layer
  with MIDI "future"; `src/app/actions.ts` and `src/app/midi-actions.ts` now sit above it.
- knip reports `framesToBoundary` in `src/audio/looper/grid-math.ts` unused; deleting it is the owner's
  call.

## 6. L2 loopback probe (owner decision)

`docs/ARCHITECTURE.md` names L2, a physical loopback measurement, as the latency gate. `pnpm
native:loopback` (2026-09-24) now measures the alignment half with one cable from an interface output
into an input; feel stays with the ear. Open: whether it becomes an in-app calibration (STATUS D18).

## 7. Takes on one timeline across launches (open)

`pnpm native:loopback` (2026-09-24) showed that a take's offset follows the plugin bridge's queue
nearly ms for ms. Landed, measured the same day:

- **An armed ASIO input clocks the producer** (`Hop1Pipe::pace_on_input`, woken by the capture
  callback). On QPC the input ring wandered by up to 6.5 ms and a stall left it ~80 ms deep for ~30 s;
  the input and monitor rings now run without a drift controller, the monitor on a one-callback
  cushion. RT fell from 50–51 to 44.4 ms at 256. The producer was never behind (`pace_late` 0, debug
  build). WASAPI keeps the timer: its late capture callbacks ran the producer ~5 % fast.
- **The worklet reads the bridge ring itself.** The main-thread drain into a second ring stalled and
  rejected take B in about one launch of three, and the controller learned those stalls as drift. The
  WebView2 buffer is now transferred to `plugin-pcm-source`, which holds the queue on one setpoint and
  settles it back after any step (its header); Rust marks production jumps in header word 7. Seven
  launches: no take rejected, residual +8.2..+12.4 ms at trim 60 in six.

Open: one launch in seven read −8 ms (take B 17 ms earlier than take A, the controller's level swinging
±8 ms on a ~12 s period) and one wound the controller to +143 ppm from a start offset (−10.8 ms/min in
take A). Hypothesis, unmeasured: the worklet sees the queue only at render bursts, aliased against the
producer's blocks, so the level and a settle's mean sit up to a block off and wander with the two
clocks' phase. The candidate fix is timestamps (QPC per hop-1 block against the render's output
timestamp) instead of queue counts. Proof: `native:loopback` drift under ~2 ms/min and residual spread
under ~3 ms over five launches, no rejected take.
