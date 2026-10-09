# `verify/`: guards and probes

Two categories of JS-side check, one directory each. The engine's behaviour (the looper, click, grid,
capture, synths, FX, the device side on a fake driver) is `cargo test`: `pnpm test:engine` (`cargo
test -p lf-engine`; skipped where cargo is absent) and `pnpm rust:check` (the
whole workspace; `.github/workflows/rust-test.yml` on native-code changes, `ci.yml`'s `engine` job on
ubuntu). Neither directory reaches the native half, WebView2, real rig latency or anything audible;
those are verified by running the app and measuring it (`docs/VERIFY.md`, `STATUS.md`).

| | `guards/` | `probes/` |
|---|---|---|
| Runs in | plain Node: no browser, no audio hardware | headless Chromium against the real UI on a Vite dev server, on the engine fake |
| Command | `pnpm verify` (part of `pnpm check`, the pre-push hook) | `pnpm probe <name>` · `--all` · `--ci` · `--list` |
| CI | `ci.yml` | `browser-lifecycle.yml` runs `pnpm probe --ci` |
| Speed | a few seconds for all | seconds to minutes each |
| Sees | the real source's logic, file formats, the engine wire, repository contracts | the running UI through `window.__lf` and the DOM: the commands a gesture sends, the screen a feed frame draws, IndexedDB, the export's archive, rendered layout |
| Documented in | the header of each guard | the header of each probe: what it drives, what it proves, what it cannot see |

`harness/` holds what both share: the module hooks for guards (`hooks.ts`) and the probe module
(`probe.ts`). It is TypeScript and typechecked.

## Guards

Each guard runs the real source; none carries a hand-ported copy of it. There are two kinds:

- **Pure imports.** Modules with no DOM or timer dependency load directly via Node's TS type-stripping
  (`verify/guards/quantize.mjs` imports `src/ui/state/quantize.ts`).
- **Modules under the hooks.** A guard that imports `verify/harness/hooks.ts` can load any `.ts` module
  under `src/` (Solid, `import.meta.env`, extensionless imports) and gets a fresh copy per `?g=N` query, so module-load
  state re-runs (`verify/guards/layout-store.mjs`).

`verify/guards/docs.mjs` is the docs guard: cited paths exist, cited shas resolve (a backticked 7–10 hex characters with a letter; skipped in a shallow clone; the repo
started from one squashed commit, so no doc may cite an earlier sha), the invariant titles in
`AGENTS.md` and `docs/ARCHITECTURE.md` match, `STATUS.md`'s next jam has ≤ 5 items, and no tracked
`.md` holds an em dash. A dead path a doc keeps on purpose says so on the same line ("(now `…`)",
"not yet built", "upstream"), and the guard skips it.

A guard counts only once it went red on a deliberately planted bug in the code it claims to cover. A
bug no public path can reveal is an equivalent mutant; name it in the commit message. The engine's
tests hold to the same rule (the lf-engine briefing, `src-tauri/crates/lf-engine/src/lib.rs`).

### Add a guard

1. Create `verify/guards/<name>.mjs`; `run-guards.mjs` runs every `.mjs` in that directory.
2. Run the real code: import a pure module directly, or load it under the hooks. Engine behaviour is an
   lf-engine test, not a guard. When the logic you want sits inline in a module with DOM or timer
   dependencies, extract it into a pure module the source calls with the live values, and import that.
   Never copy source logic into a guard.
3. Track checks with a `passed`/`failed` counter, print `=== RESULT: N/N checks passed, M failed ===`
   and `process.exit(failed === 0 ? 0 : 1)`. The runner fails a guard that exits non-zero or prints no positive `N/N checks passed` count.
4. Plant realistic bugs in the covered code; each must turn the guard red. Revert them.
5. Run `pnpm verify`. One guard alone: `node verify/guards/<name>.mjs`.

## Probes

A probe drives the real frontend in headless Chromium through `window.__lf`, the DOM and dynamic
`import('/src/…')` of the app's own modules. The engine is the DEV fake in `src/platform/host.web.ts`,
on when an init script sets `window.__lfEngineFake = true` before the app loads: it records every
command the UI sends (`__lf.native.sent`) and hands the feed frames the probe scripts
(`__lf.native.emit`) to the UI. Nothing answers a command but a lane's mix, which the fake reports as
the engine's `Mix` once it changes (its seams `holdEcho`, `holdApply` and `refuseMix` hold or refuse that),
and a toggle (CLICK, END STOP, FIXED, RETAKE, AUTO REC, a send), which it switches or refuses as the
engine's gate would on the scripted looper and answers with `Toggled` or `Refused`,
so a probe proves gesture → command and frame → screen, never the engine
(`verify/probes/engine-seam.mjs` is the pattern). Native MIDI is faked the same way: the fake records the
outbox's batches (`__lf.native.batches`, each with its epoch; their input events flattened in
`__lf.native.inputSent`: notes by owner, blurs, note targets) and native MIDI calls (`__lf.native.midiCalls`),
answers a learn or its cancel with a `learning` event and nothing else, and hands the UI the native MIDI
events a probe scripts (`__lf.native.midiEmit`): the router and learn are Rust's tests. Its seams lose a
batch (`failSends`), answer what native MIDI dropped (`dropped`), hold a batch's answer (`sendHold`) or a
MIDI call's (`midiHold`), refuse an edit (`editAnswer`), and, set by an init script, hold the subscribe's
epoch (`window.__lfMidiSubscribeHold`) or script the boot's import answer (`window.__lfMidiImportAnswer`). What still runs for real in the page: the recovery worker and IndexedDB, the export's archive. A snapshot's track carries the lane's mix as the fake's commands and events left it
(or the one a probe scripts). The export's master is the engine's; asked with the master, the fake
answers a dry sum under those tracks' volume and mute and the master's the UI sent (NOT the engine's
sound: no probe tests the master's sound).
Where a probe needs another native answer (plugin host, ASIO, device
lists), it substitutes the platform host in the page, so it proves the frontend's handling of that
answer, never the native side. It sees Chromium, not WebView2 on the rig.

`pnpm probe` starts Vite on a free port (no HMR, no file watcher), runs each probe as its own process
with `--url`, and stops Vite afterwards. A probe passes when it exits 0 after printing
`=== RESULT: <name> passed ===`; its output lands in `logs/probes/<name>.log`. `--url=<server>` reuses a
running server instead; other arguments (`--case=…`, `--headed`) pass through to the probe. Run
directly, `node verify/probes/<name>.mjs` targets http://localhost:1420.

A probe whose header carries `@no-ci <reason>` stays out of `pnpm probe --ci` and CI; `pnpm probe --list`
prints the reasons.

### Add a probe

1. Create `verify/probes/<name>.mjs` and call `probe(async ({ open }) => { … })` from
   `verify/harness/probe.ts` once. `open()` returns the page with `__lf` ready plus its console errors; an
   uncaught page error fails the probe unless the page opts out with `allowPageErrors`.
2. Assert with `node:assert`; a throw is a failure. Print the measurements the assertions read, so a red
   run's log is evidence.
3. Write the header: what it drives, what it proves, what it cannot see, how to run it.
4. Break the covered code once and watch the probe go red, as for a guard.
