# `verify/` — guards and probes

Two categories of automated check, one directory each. Neither reaches the native half, WebView2,
real rig latency or anything audible; those are verified by running the app and measuring it
(`docs/VERIFY.md`, `STATUS.md`). Rust unit tests run through `cargo test`
(`.github/workflows/rust-test.yml` on native-code changes).

| | `guards/` | `probes/` |
|---|---|---|
| Runs in | plain Node: no browser, no `AudioContext`, no audio hardware | headless Chromium against the real app on a Vite dev server |
| Command | `pnpm verify` (part of `pnpm check`, the pre-push hook) | `pnpm probe <name>` · `--all` · `--ci` · `--list` |
| CI | `ci.yml` | `browser-lifecycle.yml` runs `pnpm probe --ci` |
| Speed | a few seconds for all | seconds to minutes each |
| Sees | the real source's logic, timing on a simulated audio clock, file formats, repository contracts | the running app through `window.__lf` and the DOM: real Web Audio, real worklets, IndexedDB, rendered layout |
| Documented in | the header of each guard | the header of each probe: what it drives, what it proves, what it cannot see |

`harness/` holds what both share: the rig for guards (`rig.ts`, `hooks.ts`, the fakes) and the probe
module (`probe.ts`). It is TypeScript and typechecked.

## Guards

Each guard runs the real source; none carries a hand-ported copy of it. There are three kinds:

- **Pure imports.** Modules with no Web Audio, Tone or timer dependency load directly via Node's TS
  type-stripping (`verify/guards/quantize.mjs` imports `src/audio/quantize.ts`). The looper's grid
  arithmetic (`src/audio/looper/grid-math.ts`) and the compensation formula
  (`src/audio/record-latency-math.ts`) are kept pure for this.
- **Rig guards.** `verify/harness/rig.ts` drives the real looper: `bootLooper()` loads a fresh module
  graph of `src/audio` per scenario over a fake Web Audio + Tone layer, renders 128-frame quanta through
  the real capture worklet and fires the app's timers on the same audio clock. A guard presses
  `rig.looper.recDub(0)` and reads track state, PCM, started sources, clicks and LED beats. `rig.stall(s)`
  models a blocked main thread, `rig.renderAhead(n)` a producer ahead of the clock the main thread reads,
  `rig.import(path)` loads any other `src/` module of the same generation. `RIG_LOGS=1` echoes the app's
  console. It cannot show real render timing, browser jitter, WebView2 or anything audible.
- **Modules under the hooks.** A guard that imports `verify/harness/hooks.ts` can load any `src/` module
  (Solid, `import.meta.env`, extensionless imports) and gets a fresh copy per `?g=N` query, so module-load
  state re-runs (`verify/guards/layout-store.mjs`). Worklet processors load with a `registerProcessor` shim
  (`verify/guards/capture-packets.mjs`, `verify/guards/worklet-pop.mjs`).

`verify/guards/docs.mjs` is the docs guard: cited paths exist, cited shas resolve, the `STATUS.md` rig
lap has ≤ 10 stops. A dead path a doc keeps on purpose says so on the same line — "(now `…`)",
"not yet built", "upstream" — and the guard skips it.

A guard counts only once it went red on a deliberately planted bug in the code it claims to cover. A
bug no public path can reveal is an equivalent mutant; name it in the commit message.

### Add a guard

1. Create `verify/guards/<name>.mjs`; `run-guards.mjs` runs every `.mjs` in that directory.
2. Run the real code: import a pure module directly; drive anything that touches Web Audio, Tone, timers
   or the looper through the rig or the hooks. When the logic you want reads `engine.ctx`, `clock.bpm()`
   or `engineState` inline and a rig scenario cannot reach it, extract the math into `grid-math.ts` or
   `record-latency-math.ts` (the source calls it with the live values) and import it. Never copy source
   logic into a guard.
3. Track checks with a `passed`/`failed` counter, print `=== RESULT: N/N checks passed, M failed ===`
   and `process.exit(failed === 0 ? 0 : 1)`. The runner fails a guard with no RESULT line or 0 checks.
4. Plant realistic bugs in the covered code; each must turn the guard red. Revert them.
5. Run `pnpm verify`. One guard alone: `node verify/guards/<name>.mjs`.

## Probes

A probe drives the real frontend in headless Chromium through `window.__lf`, the DOM and dynamic
`import('/src/…')` of the app's own modules. Where it needs a native answer (plugin host, ASIO, device
lists), it substitutes the platform host in the page, so it proves the frontend's handling of that
answer, never the native side. It sees real Web Audio rendering in Chromium, not WebView2 on the rig.

`pnpm probe` starts Vite on a free port (no HMR, no file watcher), runs each probe as its own process
with `--url`, and stops Vite afterwards. A probe passes when it exits 0 after printing
`=== RESULT: <name> passed ===`; its output lands in `logs/probes/<name>.log`. `--url=<server>` reuses a
running server instead; other arguments (`--case=…`, `--headed`) pass through to the probe. Run
directly, `node verify/probes/<name>.mjs` targets http://localhost:1420.

A probe whose header carries `@no-ci <reason>` stays out of `pnpm probe --ci` and CI; `pnpm probe --list`
prints the reasons. The golden jam (`pnpm verify:jam`) is one of these: the separately dispatched
`golden-jam.yml` runs it.

### Add a probe

1. Create `verify/probes/<name>.mjs` and call `probe(async ({ open }) => { … })` from
   `verify/harness/probe.ts` once. `open()` returns the page with `__lf` ready plus its console errors; an
   uncaught page error fails the probe unless the page opts out with `allowPageErrors`.
2. Assert with `node:assert`; a throw is a failure. Print the measurements the assertions read, so a red
   run's log is evidence.
3. Write the header: what it drives, what it proves, what it cannot see, how to run it.
4. Break the covered code once and watch the probe go red, as for a guard.
