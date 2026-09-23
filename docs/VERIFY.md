# BleepLoop runtime verification

**This file is the playbook**, for every harness; the root `AGENTS.md` routes here.

The app is verified by **driving it and measuring**, not by reading code or trusting a typecheck.
Subagent "typecheck green" self-reports are NOT sufficient — always run the runtime probe yourself.
Static gates first (`pnpm check`, `pnpm build`), then the runtime probe below. Deterministic
audio-logic guards live in `verify/` (`pnpm verify`, ≈1s; see `verify/README.md`).

Git hooks are local checkout state. Before relying on the push gate, inspect the file returned by
`git rev-parse --git-path hooks/pre-push`; it must run `pnpm check`. If absent, install the configured
hook with `pnpm exec simple-git-hooks`. An existing `node_modules` directory does not prove that the
package's prepare script installed hooks in this checkout.

## Browser harness (web build, `pnpm dev` on http://localhost:1420)

- **Playwright, NOT claude-in-chrome.** claude-in-chrome is blocked on `localhost:1420` by an
  enterprise Chrome policy; Playwright launches its own browser (COI true, no extensions). Use
  whichever driver the session has: **playwright-cli** when present (proven 2026-07-10; async work =
  `eval "(async () => {…})()"`), else the **Playwright MCP** (`browser_evaluate` takes a `() => {…}`
  function — wrap async in an IIFE; screenshot: OMIT `filename` so it lands in `.playwright-mcp/`,
  or read the inline image), else a plain `node` Playwright script (what `verify/golden-jam.mjs`
  does).
- Any click is a user gesture that unlocks the AudioContext (the `BleepLoop` header title is a safe
  one).
- **From WSL on the PC:** the `pnpm` wrapper hands a `/mnt/c` checkout to Windows `pnpm.exe`, so
  `pnpm dev`, `pnpm check` and `pnpm verify:jam` all run on Windows node with the Windows Playwright
  browsers — that is the working lane. Vite then listens on Windows localhost only: `curl` from WSL
  hangs, so drive probes with `node.exe script.mjs` (Windows Playwright), never Linux node. A
  `pnpm install` that wants to rebuild `node_modules` aborts without a TTY — pass
  `--config.confirmModulesPurge=false`. The Rust gate runs from WSL too: there is no Linux `cargo`, but
  Windows `cargo.exe` (`%USERPROFILE%\.cargo\bin`, on the interop PATH) builds the `/mnt/c` checkout,
  honours `rust-toolchain.toml` and inherits the User-scope ASIO SDK env — `cargo.exe check
  --no-default-features [--features asio]` and `cargo.exe test --no-default-features` from `src-tauri/`
  are the commands. `tauri dev` runs from WSL too (`pnpm dev:wasapi > logs/<name>.log 2>&1` in the
  background; the window opens on the PC desktop; pass env to the Windows side through `WSLENV=A:B`);
  stop it with `powershell.exe -NoProfile -c "Get-Process app,cargo | Stop-Process -Force"` plus the
  port-1420 owner PID. Only the by-ear gates need a person at the PC.
- You can drive reactive UI state directly (e.g. `__lf.clock.setBpmLocked(true)`) instead of
  reproducing the full looper flow, then assert via DOM reads + screenshot. The web build hides
  Tauri-only chrome (settings gear) — drive the reactive state directly instead. If a probe needs a
  missing handle, add it to `__lf` and keep it so the next probe can reuse it.

## The DEV debug hook

`window.__lf` (built in `src/debug/lf.ts` behind a declared `LfDebug` interface, installed by `app.tsx` in DEV only) exposes the audio engine, instrument controls, clock,
looper, master, layout store, audio-device settings, MIDI manager, native platform/bridge, record
latency controls, session import/export, notification store, popover drivers, and `transport()` (raw
Tone transport handle). This is how you drive and inspect the app from Playwright.

## Measuring audio (not hearing it)

- The looper's capture worklet taps `engine.recordTap`, so **synth/plugin output is recordable
  without a mic** — record real loops headless by triggering notes; no `getUserMedia` needed. This
  is central to how the app is verified.
- Tap `engine.masterGain` with an AnalyserNode and read the peak after triggering notes, or use
  `looper.trackPeak(i)` / `looper.captureQuanta()` (the worklet heartbeat).
- Trigger notes with `__lf.ensureActive()` then
  `__lf.inputRouter.handle({type:'on',note,velocity,source})` — `velocity` is MIDI **0..127** (a 0..1
  value plays ~40 dB too quiet and looks like a gain bug; it isn't).
- `__lf.looper.levelValue()` reads the record-tap peak (linear, fast decay) headless — the command-bar
  meter draws it on a −60..0 dBFS scale.
- An AnalyserNode on `masterGain` measures *pre-limiter* (linear) — per-track volume scales the
  reading proportionally. Measure synth gain with CLEAN note-isolation (a hanging note pollutes the
  peak).

## Recurring web-verify gotchas

- **Web MIDI shows `unsupported` under Playwright's Chromium** — expected (real Chrome/Edge has it;
  WebView2 has the native path). Not a bug.
- `getUserMedia` is unavailable headless (can't drive mic-arm).
- `getComputedStyle` right after a manual `classList` mutation in a Playwright `evaluate` returns
  STALE values → **verify rendered CSS by screenshot, not getComputedStyle**.
- **playwright-cli specifics (2026-07-02):** `fill` alone does NOT fire Solid's `onChange` (native
  `change` fires on blur/Enter — follow with `playwright-cli press Enter`); a leading-minus value
  parses as a CLI flag (pass it after `--`); `screenshot --filename` lands in the CLI's cwd (repo
  root — move it out), bare `screenshot` lands in `.playwright-cli/`; the `default` session is
  SHARED across concurrent Claude sessions on this machine — use a named session
  (`playwright-cli -s=lf …`) so a parallel session can't hijack the tab mid-probe.
- `file://` is BLOCKED in Playwright (mockups need `python3 -m http.server 8765`).

## Native / Tauri verification (PC only)

- **No Playwright into WebView2.** For Tauri/native verification: grep `tauri dev` stdout for
  `[diag]`. **What reaches that stdout: only Rust `invoke('diag')`/`log::info!` lines +
  vite-forwarded `[console.error]`; plain `console.log` from WebView2 does NOT.**
- Since P10.3 there are no DEV auto-load probes — to runtime-gate a native path, temp-wire a probe
  that drives the PRODUCTION fns, grep, then revert.
- `tauri dev` does NOT self-terminate — kill with `Get-Process app,cargo | Stop-Process -Force` +
  the vite node on port 1420 (`(Get-NetTCPConnection -LocalPort 1420 -State Listen).OwningProcess`);
  **NEVER kill all node — the Claude Code session may be a node process.**
- Screenshot the native window by its handle: `PrintWindow(hwnd, 3)` (client only + full content)
  from a DPI-aware process captures it without focus. Never grab the screen (`CopyFromScreen`
  after `SetForegroundWindow`): it captures whatever window is on top, including the owner's.
- Runtime-gate logs go under gitignored `logs/` (e.g. `logs/dev-asio.log`), not the repo root.
- `tauri dev` watches ALL of `src-tauri/` — a doc edit there (`AGENTS.md` included) rebuilds and
  relaunches the app mid-probe. Write docs after the run, or outside `src-tauri/`.
- Background the dev run and poll the log until it prints the line you want, rather than blocking on
  it. (*Claude Code specifics:* background via the PowerShell `run_in_background` tool, poll with a
  Bash `run_in_background` `until grep -q … ; do sleep 2; done` loop — foreground `sleep` and
  PowerShell `Start-Sleep`+chaining are blocked by that harness.)
- Plugin-scan and ASIO probes work WITHOUT the full app (`--scan-one`, `--probe-asio`) — see
  `src-tauri/AGENTS.md` "Native-host verify ops".
- Output latency can be measured silently through the production callback with a DEV build:
  `src-tauri/target/debug/app.exe --probe-output-latency asio 256 4` or
  `src-tauri/target/debug/app.exe --probe-output-latency wasapi default 4`.
  The arguments select backend, requested buffer and seconds. Run driver probes sequentially and
  close their streams before opening another configuration. The result compares callback period,
  driver presentation delay and the production median; it does not measure physical DAC latency.
- DEV marker comparison: in a fresh isolated app profile, load exactly one effect, start the engine,
  go LIVE and settle for at least three seconds. A temporary DEV entry can then call
  `runMarkerProbe({ slot: 0 })` from `src/debug/marker-probe.ts`. The runner sends its JSON to native
  diagnostics and returns it. Relaunch for another run; native storage is deliberately one-shot.
  Use a separate Vite port and a Tauri config overlay with a distinct app `identifier`. Verify the
  matching directory under Windows LocalAppData before calling it a separate profile. Tauri 2.11.2
  drops WindowConfig.dataDirectory while converting WebviewAttributes, so that field alone does not
  isolate this version. A separate origin isolates localStorage/IndexedDB; it is not a separate profile.
  Keep the regular app mounted and exercise production startup/load functions. Run on an empty jam
  with web mic disarmed. Do not automate WebView2 through Playwright. The native `diag` log contains the complete
  report; frontend console forwarding truncates long JSON. `verify/render-cursor.mjs` exercises the
  production sampler with controlled timing inputs; `verify/render-clock.mjs` proves that the DEV
  clock observer preserves PCM. Both accept the browser probes' `--url` option.
- Plugin restart survey (which installed plugins raise a restart/rescan request, and on which
  parameter): `WSLENV=VITE_LF_PROBE VITE_LF_PROBE=restart-survey pnpm dev:wasapi > logs/<name>.log 2>&1 &`
  from WSL. `src/debug/restart-survey.ts` loads every scanned plugin into slot 0, sweeps each
  parameter min → max → default, re-reads `listParams` (a `value-check` line: controller value vs
  what the host set) and unloads; `[survey]` lines sit next to the host's
  `restartComponent(...)` / `request_restart` lines in the log (~6 min for 30 plugins, no gesture
  needed). `VITE_LF_PROBE_FILTER=Pro-Q,Saturn` narrows it, `VITE_LF_PROBE_SETTLE=150` adds a
  per-parameter wait when a flag needs attributing to one parameter (they arrive asynchronously, a
  line or two after the cause). Rerun it when a plugin is installed. Baseline
  (2026-09-10, 30 plugins): 32 `restartComponent` calls, all `kLatencyChanged`, all FabFilter; no
  `kReloadComponent` or `kIoChanged`; no CLAP `request_restart`.
- Editor smoke (does every installed plugin's editor open into the host window and close without a
  hang): `WSLENV=VITE_LF_PROBE VITE_LF_PROBE=editor-smoke pnpm dev:wasapi > logs/<name>.log 2>&1 &`
  from WSL. `src/debug/editor-smoke.ts` loads each plugin into slot 0, opens the editor, holds it
  (`VITE_LF_PROBE_HOLD`, default 1500 ms), closes, unloads; `[smoke] opened/closed … in N ms` lines sit
  next to the host's `editor embedded into host window (WxH)` / `editor closed` lines, and the final
  `complete: N opened, M failed` line is the verdict. Windows open on the PC desktop; no gesture
  needed. Run it after any change to `editor_window.rs` or either host's editor open/close path. Baseline
  (2026-09-12, WASAPI, 44.1 kHz): `complete: 30 opened, 0 failed, of 30`, each closing in 110–150 ms.
- Swap stress (does switching plugins IN PLACE complete after the loaded one was tweaked):
  `VITE_LF_PROBE=swap-stress`, same launch and `VITE_LF_PROBE_FILTER` as the editor smoke.
  `src/debug/swap-stress.ts` swaps every ordered pair in slot 0, editor closed then open, sweeping the
  first params (`VITE_LF_PROBE_PARAMS`, default 8) before each swap; every step is timed and a step
  past 30 s prints `TIMEOUT` naming it. Verdict line: `[swap] complete: N swapped, M failed`. Read the
  per-step ms too: a swap that completes in 10 s is a freeze to the person waiting.


## Mac vs PC split

On the Mac there is no Rust toolchain — `src-tauri/` is unverifiable (no `cargo`, `cfg(windows)`
paths). On Mac: verify the TS half (`pnpm check` / `build`), drive the web build via Playwright, and
adversarial-review any Rust by reading; defer `cargo check` + the runtime gate to the PC.
