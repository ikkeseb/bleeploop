# `verify/` — deterministic regression guards

`pnpm verify` runs the deterministic `*-verify.mjs` guards in this directory. They check audio math,
file formats and repository contracts without a browser or audio hardware. They do not establish
that the running looper dispatches correctly or that the native app sounds right.

Browser probes run outside this command. `golden-jam.mjs` covers capture and dispatch;
`capture-clock.mjs` measures absolute capture frames under producer/consumer interleaving;
`overdub-window.mjs` checks compensated punch windows; `capture-loss.mjs` checks rollback after missing
packets; `master-latency.mjs` checks measured limiter delay;
`record-stop-window.mjs` checks first/later/AUTO/FIXED capture windows, padding, cancellation,
recorder release, playback-failure retry and the 60-second cap;
`overdub-timers.mjs` counts actual callback registration/cancellation through rapid stop/reuse,
compensated STOP, CLEAR and normal boundary rearming;
`loop-end-stop.mjs` measures playback deadlines and UI; `fx-grid.mjs` measures rhythmic effects;
`fx-pitch-cost.mjs` checks unused pitch allocation, offline DSP cost, and live enable/reset continuity;
`recovery-capacity.mjs`, `recovery-failure.mjs`, `recovery-worker.mjs` and `recovery-playback.mjs`
cover recovery fidelity, failure paths and main-thread load.
`recovery-transactions.mjs` injects actual IndexedDB transaction failures; `recovery-close.mjs` checks
close approval after a failed recovery deletion, with native close capabilities substituted.
`monitor-generation.mjs` controls delayed host replies through the actual frontend monitor lifecycle.
`marker-probe.mjs` checks DEV marker correlation and clock arithmetic; it does not run native audio.
`render-clock.mjs` checks the DEV worklet observer preserves PCM; `render-cursor.mjs` exercises the
production compensation sampler with paired queue/timestamp observations, invalid clocks and freeze.
`export-context.mjs` covers live/export isolation, lossless editable downloads and bounded ZIP work;
`synth-note-ownership.mjs` and `midi-note-ownership.mjs` measure note release, voice reuse and input owners;
`audio-settings-startup.mjs` exercises actual frontend orchestration with an instrumented host;
`layout-reachability.mjs` measures rendered control access and canvas identity at desktop sizes, plus the
drum-pad ribbon, the two-row command-bar cap and the muted-lane readout with a loop present.
`transport-auto-layout.mjs` checks that AUTO toggling and sensitivity changes preserve command-bar
height and lane position from 960 to 1920 px, with screenshots at 1730 px.
Rust unit tests run through `cargo test`; `.github/workflows/rust-test.yml` runs them on native-code
changes. Runtime verification and the Windows/WSL command lane are owned by `docs/VERIFY.md`.

`fs-docs-verify.mjs` is the docs guard: cited paths exist, cited shas resolve, the `STATUS.md` rig
lap has ≤ 10 stops. A dead path a doc keeps on purpose says so on the same line — "(now `…`)",
"not yet built", "upstream" — and the guard skips it.

Each `*-verify.mjs` runs in plain Node with **no browser, no `AudioContext`, no audio hardware**. It either:

- **imports the real source** via Node's TS type-stripping (e.g. `fs-quantize-verify.mjs` imports
  `../src/audio/quantize.ts` directly — it cannot drift from the source), or
- **ports** the pure logic into the script when the source can't run under Node (e.g. the worklet/looper
  guards, which depend on `AudioWorkletProcessor` / `ringbuf.js` / Tone). A ported guard mirrors a specific
  source path; if you change that path, update the port in lockstep.

Every deterministic guard prints a final line `=== RESULT: N/N checks passed, 0 failed ===` and exits non-zero on any
failure.

## Run

```bash
pnpm verify        # run all guards, summarized (≈1s)
pnpm check         # typecheck + lint + boundary + verify (the full static gate)
pnpm exec node verify/fs-grid-verify.mjs   # run one directly
```

`pnpm verify` is part of `pnpm check`, so a regression in any guard fails the standard gate.

## Browser probes

With Vite running, use `pnpm exec node verify/loop-end-stop.mjs`, and substitute the other probe
filenames above as needed. These browser probes accept
`--url=http://localhost:1421` for a separate verification server started with
`pnpm exec vite --port 1421`. Use a fresh server if HMR has left dynamically imported probe modules
with a different identity from the app's modules. `pnpm verify:jam` starts or reuses port 1420.

`golden-jam.mjs` also accepts `--url` for an already-running separate server.

## Mirrored ports and the drift canary

A ported (non-importing) guard mirrors a specific source function by hand, so it can go **silently stale**:
the source is refactored, the port keeps asserting the old math, and the guard stays green while proving
nothing. `fs-mirror-drift-verify.mjs` is a meta-guard that catches this. Each ported guard carries, next to
its ported math, a tag naming the source range it mirrors:

```js
// MIRRORS: src/audio/clock.ts@307-341 sha256:e39b945d6b36e271  (pulseTick — forced-clamp)
```

The meta-guard re-hashes that source range and fails if it no longer matches. The hash is over the
**normalized code** of the range (each line trimmed; blank lines and full-line comments dropped), so
churning a comment block never trips it — only a code change does. On failure it says whether the code
**MOVED** (same code, new line range — a benign shift) or **CHANGED** (genuinely different — re-verify the
port). Either way the fix is one command after you've confirmed the port:

```bash
node verify/fs-mirror-drift-verify.mjs --update   # re-baseline: reseed hashes + auto-repoint MOVED ranges
```

⚠ **A range that both MOVED and CHANGED cannot be auto-repointed** — the old code exists nowhere, so
`--update` refuses the tag and FAILs (writing a fresh hash at the old lines would baseline unrelated
code). Re-read the port, re-point the tag by hand at the correct new range with `sha256:PENDING`,
then run `--update` again to seed the hash.

**Block-comment-aware normalize.** Dropping "full-line comments" is block-comment-aware, not a naive
prefix test. A line is dropped when, after trimming, it is blank, starts with `//`, starts with `/*`, or
starts with `*`/`*/` **while beginning inside an open `/* … */` block**. The last clause is the important
one: a genuine CODE line that happens to start with `*` (e.g. a wrapped multiplication continued onto its
own line as `* b`) is **kept**, so a real code change can't hide behind a leading `*`. The script scans each
source file once to know which lines begin inside a block comment (a pragmatic scanner: it skips `//` line
comments and string literals, and assumes strings don't span lines — fine for a canary). All other
normalize rules are byte-identical to before, so hashes are unchanged wherever this blind spot was never
exercised.

**Coverage assertion.** The meta-guard also asserts that **every** `*-verify.mjs` (except itself) is
drift-protected by one of three mechanisms, and FAILs naming the options if a guard is left unprotected:

- **(a)** it imports the real source via a `../src/…` path (an import can't drift), OR
- **(b)** it carries ≥1 `// MIRRORS:` tag beside its hand-ported math, OR
- **(c)** it declares `// MIRRORS-EXEMPT: <reason>` — for a guard that genuinely ports nothing (pure spec
  constants, self-contained format checks, or a real import of non-`src/` logic such as
  `fs-boundary-guard-verify.mjs`, which imports `scripts/check-boundary.mjs` directly and so cannot drift).

This closes the hole where an untagged, non-importing guard would silently get zero protection.

When you **add** a ported guard: add a `// MIRRORS: <src>@<start>-<end> sha256:PENDING` tag next to the
ported block, then run `--update` to seed the real hash. When you **refactor source** a tag points at,
re-read it, confirm the port still mirrors it (fix the port if not), then `--update`. Prefer converting a
port to a real `../src` import wherever the module runs under Node — an import can't drift, so it needs no
tag. The looper's grid arithmetic (`src/audio/looper/grid-math.ts`) and the compensation formula
(`src/audio/record-latency-math.ts`) are kept PURE for exactly this: when the logic you want to guard reads
`engine.ctx` / `clock.bpm()` / `engineState` inline, extract the math into one of those modules (the source
calls it with the live values) and import it — the tag count `run-all.mjs` prints is the trend. If a new
guard truly ports nothing, give it `// MIRRORS-EXEMPT: <reason>` instead of a tag.

## Add a guard

1. Create `verify/<name>-verify.mjs` (the `-verify.mjs` suffix is how `run-all.mjs` discovers it).
2. Prefer importing the real `../src/...` module if it runs under Node (pure TS, no Web Audio/Tone deps);
   otherwise port the exact logic and note which source path it mirrors.
3. Track checks with a `passed`/`failed` counter, print the `=== RESULT: N/N checks passed, M failed ===`
   line, and `process.exit(failed === 0 ? 0 : 1)`.
4. Run `pnpm verify` to confirm it's picked up and green.

## Why these are tracked

Tracking the guards lets a fresh clone on either development machine re-run the same checks.
Keep browser probes tracked too; deterministic math checks and runtime measurements cover different
failure modes.
