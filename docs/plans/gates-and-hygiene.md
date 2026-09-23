# Work order: real-code gates and agent hygiene (OPEN)

Six pieces from the 2026-09-23 fresh-eyes pass over code, gates, docs and agent workflow. Build them in
this order, one piece per commit series, gates green before each push. Piece 6 waits on an owner
decision. Taste findings from that day live in `docs/backlog-taste.md` and the hands-free milestone in
`docs/plans/pedalboard.md`; neither is repeated here. Delete this file when the last piece lands, after
folding what still binds into `verify/README.md`, the briefings or a call-site comment.

Pieces 1, 3 and 5 may edit `src/audio/`: the dev-app rule in `AGENTS.md` applies.

## 1. Guards drive the real looper

**Why.** 42 `MIRRORS` tags in 18 guards are hand-ported copies of `machine.ts`, `capture.ts` and
`clock.ts`, the code that matters most. `verify/fs-mirror-drift-verify.mjs` re-hashes the source ranges
so a port that goes stale fails. The guards prove the copy, not the app, and every refactor of those
files trips the canary.

**Recipe** (a 115-line spike, not committed, ran the real modules under plain Node in about 1 s):

- `module.registerHooks` loaded with `node --import`. Resolve: `tone` to a fake; `*?worker&url` to a
  data-URL module exporting a string; `*?worker` to an empty class; extensionless relative imports
  under `src/` to `.ts`; `solid-js` with the `browser` condition; bare specifiers from outside the repo
  against the repo's `package.json`. Load: replace `import.meta.env` with `({ DEV: false })` in
  `src/**/*.ts`.
- Globals: a fake `AudioContext` with a settable `currentTime`, `sampleRate`, the `create*` methods the
  looper calls and `audioWorklet.addModule`; an `OfflineAudioContext` whose `startRendering` returns an
  impulse at frame 512 (the limiter-latency measurement in `engine.start`); an `AudioWorkletNode` that
  keeps its `processorOptions`; `self.crossOriginIsolated = true`; a no-op `localStorage`.
- Tone fake: one chainable Proxy for every node class. Its `context` must return
  `{ rawContext, isOffline: false, immediate(), now() }`; the FX chain reads all four.
- Driving: wrap `processorOptions.ringSab` in a `RingBuffer`, push packets in the
  `src/audio/capture-packet.ts` format with absolute frame timestamps while advancing `currentTime`,
  and yield to the event loop so the 25 ms drain timer runs.
- Spike result: REC, count-in, two bars of input, REC again. Master = exactly 2 bars at 120 BPM, BPM
  locked, lane PLAYING.

**Do.** Build the harness in TypeScript under `verify/` and add it to the typecheck. Convert the
ported guards one at a time to drive the real modules, deleting each port and its tag.

**Done when** `pnpm verify` prints 0 MIRRORS tags, every converted guard went red on a deliberately
planted bug before it was kept (bug reverted), `verify/fs-mirror-drift-verify.mjs` and its section in
`verify/README.md` are deleted, and `pnpm verify:jam` is green.

## 2. One probe harness, every probe automated

**Why.** 26 of 40 browser probes run in no automation. All 26 passed on the PC on 2026-09-23
(`record-stop-window.mjs` is the slowest at 166 s), but a red one would go unnoticed.
`verify/recovery-capacity.mjs` and `verify/recovery-failure.mjs` ignore `--url` and always hit port
1420. The `--url` parser is copied 38 times and `chromium.launch` 40 times; no shared module exists.
`verify/marker-probe.mjs` is pure Node but lacks the `-verify.mjs` suffix, so `pnpm verify` never runs
it. Nothing explains the `fs-` prefix.

**Do.** Split `verify/` into guards (plain Node, `pnpm verify`) and probes (browser). Write one shared
probe module: launch, `--url`, wait for `__lf`, the result line. Add `pnpm probe <name>|--all|--list`,
which starts its own Vite on a free port and stops it afterwards. Run every probe in
`browser-lifecycle.yml`. Each probe's header comment becomes its documentation, replacing the
per-probe paragraph in `verify/README.md`. Moving files moves paths that `STATUS.md` and the briefings
cite; the docs guard lists them.

**Done when** every probe runs in CI or its header says why it cannot, `verify/README.md` describes
categories instead of listing probes, and `pnpm check` is green.

## 3. Comments and docs state current intent

**Why.** 27 % of TypeScript lines are comments, half or more in `src/platform/host.ts`,
`src/audio/clock.ts` and `src/audio/looper/looper.ts`. About 70 comment lines carry history (wave and
phase labels such as `W3:` and `P11.3`, "the old topbar … is gone"). Comments still describe glass
cards and an aurora background after the 2026-09-23 restyle; `--glass*` survive as legacy token names
in `src/app.css`. An agent reads a comment as an instruction, so a stale one misleads. The docs have
the same problem: dated correction notes in `docs/ARCHITECTURE.md`; the record-latency formula owned by
`STATUS.md` Stop 1, though it binds whatever the lap says; a looper-UI spec that is a mockup production
has deliberately left, read together with a delta list in `src/ui/AGENTS.md`.

**Do.**
- Sweep the comments so each states what the code does now and why. A cheaper agent may run the sweep;
  the orchestrating session reviews every hunk. Before piece 1 lands, a comment edit inside a MIRRORS
  range is safe (the hash drops full-line comments) and a code edit is not.
- Move the formula into the header of `src/audio/record-latency-math.ts`; `STATUS.md` and
  `src/audio/AGENTS.md` point there.
- Strip the dated correction notes from `docs/ARCHITECTURE.md`.
- Propose retiring the mockup as the spec, with the contact sheet and the `src/app.css` tokens as the
  visual reference. The owner confirms before the mockup file goes.
- Extend `verify/fs-docs-verify.mjs` to fail when the invariant titles in `AGENTS.md` and
  `docs/ARCHITECTURE.md` diverge.

**Done when** every remaining wave/phase label and glass/aurora mention in `src/` states a current
fact, the formula has one home, and `pnpm check` is green.

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

## 5. One recorder-session object (after piece 1)

**Why.** One capture's state spans about 10 `engineState` fields, 5 module-level `let`s in
`src/audio/looper/machine.ts` and 6 overdub fields per track. Five sites reset it by hand:
`releaseRecorderState` (14 assignments), `stop`, `clear` (about 20), `rejectRecordLoss` and
`finishOverdub`. A forgotten field is a bug class.

**Do.** A pure refactor, with no behaviour change: `recording: RecordSession | null` in the looper
state, `overdub: OverdubSession | null` on `Track`. Releasing a capture sets the field to `null`.

**Done when** the piece-1 guards and `pnpm verify:jam` are green, and the next "Play first" jam in
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

- `retry` in `src/audio/midi.ts` lacks `@public`; only `verify/instrument-controls.mjs` calls it, so
  knip reports it as unused.
- `verify/contact-sheet.mjs` shows only the web tier (synth pills), never the guitar-first screen.
  Add scenes with `pluginHost.available` on and a fake amp-sim loaded, using the pattern in
  `verify/plugin-slot-pending.mjs`.
- `README.md` points at `rust-toolchain.toml` as if it sat at the root; it lives in `src-tauri/`.
- `src-tauri/AGENTS.md` cites "§ WSL lane" in `docs/VERIFY.md` and "§ ASIO startup" in
  `docs/ARCHITECTURE.md`; neither is a heading.
