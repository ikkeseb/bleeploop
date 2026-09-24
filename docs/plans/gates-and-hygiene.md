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

`pnpm native:loopback` (2026-09-24) showed that a take's offset follows the plugin bridge's hop-2 fill
nearly ms for ms. Landed, measured the same day:

- **The producer was never behind** (`pace_late` 0 in steady state, debug build; no release build
  needed). It ran on QPC while capture ran on the interface clock, so the input ring wandered by up to
  6.5 ms and a stall left it ~80 ms deep for ~30 s. An armed ASIO input now clocks the producer
  (`Hop1Pipe::pace_on_input`, woken by the capture callback); the input and monitor rings run without
  a drift controller, the monitor on a one-callback cushion. RT fell from 50–51 to 44.4 ms at 256.
  WASAPI keeps the timer: its capture callbacks arrive late often enough that the timer fallback ran
  the producer ~5 % fast.
- **The hop-2 controller learned stalls as drift** (a stall wound it to 230 ppm, an input arm to
  −284 ppm: 14–22 ms/min of stretch in later takes). Its level is now frames produced minus
  render-clock frames (`trackRateLevel` in `plugin-bridge.ts`); a step is held off it and moved into
  its base, and Rust marks production jumps in hop-1 header word 7. Drift inside take A is now
  under 2 ms/min in 8 of 12 launches, spread inside a take 0.2–0.9 ms.

Open: across launches the residual still spreads by 5–10 ms at trim 60 (−0.9..+16 ms over twelve
launches), and a launch or two in five wound the controller to −50..−90 ppm early (5–9 ms/min in that
take); the header epoch landed for that, and two launches since read 1.4 and 0.7 ms/min, not yet five.
Take B is rejected in about one launch of three: bursts of worklet underruns with the producer on
time, so the main-thread drain stalls (seen before any of this landed too). Proof: `native:loopback` drift under ~2 ms/min and
residual spread under ~3 ms over five launches, no rejected take.
