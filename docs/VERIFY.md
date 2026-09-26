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
  or read the inline image), else a plain `node` Playwright script on `verify/harness/probe.ts` (what
  every probe in `verify/probes/` does).
- Any click is a user gesture that unlocks the AudioContext (the `BleepLoop` header title is a safe
  one).
- **From WSL on the PC:** the `pnpm` wrapper hands a `/mnt/c` checkout to Windows `pnpm.exe`, so
  every `pnpm` command, including the native ones below, runs on Windows node, Windows `cargo` and
  the Windows Playwright browsers. That is the working lane. Vite then listens on Windows localhost
  only: `curl` from WSL hangs, and Linux node cannot drive it. A `pnpm install` that wants to rebuild
  `node_modules` aborts without a TTY — pass `--config.confirmModulesPurge=false`. An ad-hoc
  `tauri dev` from WSL takes env through `WSLENV=A:B`. Only the by-ear gates need a person at the PC.
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

These commands run on Windows node, and from WSL through the `pnpm` wrapper:

| Command | What it does |
|---|---|
| `pnpm rust:check` | `cargo check` without and with `asio`, then `cargo test`, all `--no-default-features`, from `src-tauri/`. It fails the asio step when `CPAL_ASIO_DIR` lacks the SDK. |
| `pnpm native:smoke` | Editor smoke (`src/debug/editor-smoke.ts`): every installed plugin loads into slot 0, and its editor opens into the host window and closes. |
| `pnpm native:survey` | Restart survey (`src/debug/restart-survey.ts`): which plugins raise a restart or rescan request, and on which parameter. Also prints a `value-check` line per plugin (controller value vs what the host set). |
| `pnpm native:swap` | Swap stress (`src/debug/swap-stress.ts`): every ordered pair is swapped in place in slot 0, first with the editor closed, then open, after tweaking the loaded plugin. A step that takes longer than 30 s prints `TIMEOUT`. |
| `pnpm native:recall` | Rig recall (`src/debug/recall-restart.ts`), five launches: two plugins loaded and an input channel set come back after a restart and after a WebView reload, unarmed, and a close through the window's close button right after that reload keeps them for the next launch; a launch whose app.exe is killed while restoring makes the next one skip the recall with one log line and one toast; the launch after that is clean. |
| `pnpm native:loopback` | Loopback sync (`src/debug/loopback-sync.ts`), ASIO, in its own app profile (`scripts/loopback-probe.tauri.json`): with an interface output cabled into an input (`--channel=<0-based>`, default 0), records the click as a FIXED first take and half-beat pulses as a later take, and prints `residual` (where a perfectly timed hit lands against the grid, + = late), the drift inside a take and `RT`, the native round trip a guitarist hears. `--plugin=` picks the effect (default Pro-Q), `--trim=<ms>` a rec align, `--bars=` the take length. |
| `pnpm native:spike` | The native-engine Stage 1 premise spike (`docs/plans/native-engine.md` § Stage 1): builds the debug app with ASIO and runs `--probe-engine-spike` (one-callback ASIO alignment, the echo round trip with Pro-Q, the amp-sim's callback cost, WASAPI) and `--probe-share` one process at a time, then prints one PASS/FAIL table. Needs the loopback cable (default line out R → input 2) and makes audible chirps; runs at the device's current rate. `--only=`, `--blocks=`, `--launches=`, `--long-min=` narrow it. |
| `pnpm native:engine` | The native-engine Stage 4 rig run (`docs/plans/native-engine.md` § Stage 4): builds the debug app with ASIO and runs one `--probe-engine` process (`src-tauri/src/engine_io/probe.rs`): the engine at ASIO 128 with the amp-sim and Pro-Q in its two slots, a loop on lane 0, a 10-minute soak, 20 backend/buffer switches and 4 plugin swaps while the loop plays. Prints each phase's counters and block time and one PASS/FAIL per check; exit 0 = all pass. Needs no cable; the take records `--in` (default input 1). `--seconds=`, `--switches=`, `--swaps=`, `--buffer=`, `--amp=`/`--proq=` (empty: no plugin) narrow it; `--mute=1` plays silence. `--lag=1 --in=1 --seconds=10` runs only the lag phase instead (needs the loopback cable, audible chirps): the Stage 1 A2 bar on the engine's own open path, the chirp's lag against the driver's reported latency within 1 ms; `--preopen=0` skips the ASIO preopen (`src-tauri/src/engine_io/cpal_driver.rs`) for a before-and-after. |
| `pnpm native:engine-smoke` | The UI on the native engine (`src/debug/engine-smoke.ts`), ASIO, in its own app profile with engine mode on (`scripts/engine-probe.tauri.json`; the runner writes the toggle file there): the device reopens at `--buffer=` (default 128); a FIXED 1-bar first take counts in 4-3-2-1 with the click (feed and screen) and plays one bar at the device's rate; a later take, an overdub with UNDO, STOP ALL, PLAY ALL and CLEAR change the lanes; `--plugin=<name>[:<format>]` (e.g. `Pro-Q:vst3`) also loads that plugin into slot 1, goes live and checks that the input meter moves. Fails on any other frontend `console.error`; the run ends with a close through the window, as the close button does. |
| `pnpm native:engine-recovery` | Engine mode's session paths (`src/debug/engine-recovery.ts`), ASIO, in engine-smoke's profile, two launches: two lanes recorded from the input with the click on (the loopback cable gives them audio; a silent lane fails) are exported to a zip, cleared and imported back with the same PCM and mix, and once autosave holds the jam app.exe is killed with the loops playing; the relaunch restores the same lanes from recovery, then clears them (which deletes the recovery) and closes through its window. |
| `pnpm native:engine-loopback` | Where takes land against the click on the engine (`src/debug/engine-loopback.ts`), ASIO, in engine-smoke's profile, one launch: with an interface output cabled into input 2 (`--channel=<0-based>`, default 1), at each of `--buffers=` (default `64,128,256`) a FIXED first take of the click (A), a later take of lane 1's playback (B), STOP ALL → PLAY ALL, the click again (C) and lane 1 again (D), a multiply take of the click at two loops (E) and, over the grown loop, the click again (F). An offset is the recorded click's onset against its beat in the committed lane PCM, less the detector's lag on the engine's ideal click; nothing is fitted. Bars: \|A\| ≤ 2 ms, spread ≤ 1 ms, drift ≤ 0.1 ms/min, the accent on beat 1, and B = 2·A, C = A, D = B, E = A, F = A within 0.1 ms, and lane 1 holds its loop twice, bit for bit, after the multiply; any failed bar fails the run. `--plugin=` picks the live effect (default `Pro-Q:vst3`; `none` = MIC, the input dry), `--bars=` the take length; `--echo=1` has IN FX's ECHO (1/16, no feedback, level 0.5) on while A records and bars its echo one sixteenth after each click at half its level, C = A then saying the echo left the dry click in place; with MIC, `--unload-first=<plugin>` loads and unloads that plugin in slot 1 first, and the take's `peak gain` (cable × slot gain) should equal a run without it. Audible clicks. |
| `pnpm release:smoke` | The RELEASE build on the engine, driven from outside (`scripts/release-smoke.mjs`). Build it first in a profile of its own: `pnpm exec tauri build --no-bundle --features asio --config scripts/release-smoke.tauri.json`. It launches `src-tauri/target/release/app.exe` with WebView2's remote-debugging port and attaches Playwright over CDP: the UI boots with no CSP violation or uncaught error, on the engine; ASIO at `--buffer=` (default 128) and `--channel=` are picked in Audio Settings; the feed moves the beat LEDs, the meter and a playhead; a FIXED 1-bar take with the click goes armed → rec → play with a waveform; CLEAR ALL and the OS close end the process within 60 s with no ERROR in the release log. `--fresh` starts from an empty profile, `--exe=` drives another exe (the owner's identifier is refused without `--owner-profile`). It hears nothing: timing is `native:engine-loopback`'s. That build leaves an exe with the smoke identifier at the path `pnpm build:app` writes. |
| `pnpm native:kill` | Stops `app`, `cargo` and whatever owns port 1420. |

A `native:*` probe launches `tauri dev` (WASAPI; `--asio` for ASIO) with the probe's
`VITE_LF_PROBE`, waits for its verdict line, stops the run and prints
`=== <probe>: PASS|FAIL: … ===`. `native:recall` launches once per phase (`VITE_LF_PROBE_PHASE`) and
lets the app close itself between phases, except after the check phase, whose window the runner sends
the OS close (as the close button does), and in the crash phase, where it kills app.exe alone. Only
`native:recall` restores plugins at launch, from a record of its own, never the owner's; the other
probes start from empty slots. Every probe runs in a profile of its own and writes its engine toggle
first: the web-path probes (smoke, survey, swap, recall, loopback) `off`, in
`scripts/classic-probe.tauri.json` or loopback's own (their first run scans plugins cold), the engine
probes `on`. The owner's `pnpm dev:asio` starts in engine mode; the web path is Audio Settings →
engine → web audio, then a restart. Windows open on the PC desktop and
no gesture is needed. It refuses to start while an app is running. `--<knob>=<value>` sets `VITE_LF_PROBE_<KNOB>` (`--filter=Pro-Q,Saturn`;
each probe's header lists its knobs), and the full log lands in `logs/native-<probe>.log`. It
blocks until the verdict, so an agent harness should run it in the background.

- **No Playwright into WebView2.** For Tauri/native verification: grep `tauri dev` stdout for
  `[diag]`. **What reaches that stdout: only Rust `invoke('diag')`/`log::info!` lines +
  vite-forwarded `[console.error]`; plain `console.log` from WebView2 does NOT.**
- The committed `native:*` probes (table above) runtime-gate the paths they name. For any other
  native path, temp-wire a probe that drives the PRODUCTION fns, grep, then revert.
- `tauri dev` does NOT self-terminate — stop it with `pnpm native:kill`. **Never kill all node: the
  agent session may be a node process.**
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
  report; frontend console forwarding truncates long JSON. `verify/probes/render-cursor.mjs` exercises the
  production sampler with controlled timing inputs; `verify/probes/render-clock.mjs` proves that the DEV
  clock observer preserves PCM. Both run through `pnpm probe`.
- **When to run the plugin probes, and their baselines** (the verdict alone doesn't say this). Narrow
  `smoke`, `survey` and `swap` to `--filter="Surge XT Effects,Pro-Q,Gojira"` (CLAP and VST3, a
  separated controller, FabFilter's latency restarts, Neural DSP's slow teardown) unless the change
  reaches every plugin (scan, load, the editor host) or a plugin was just installed (survey that one):
  a full sweep opens every installed plugin on the owner's desktop.
  - `native:engine-loopback` after a change to the engine's alignment, click, grid, device open or
    snapshot. Baseline (2026-09-26, Scarlett 2i2 3rd gen, 44.1 kHz, a cable from line out R into
    input 2; six launches, Pro-Q 3 and MIC): A −0.11..+0.09 ms at ASIO 64, 128 and 256, spread
    0.001 ms, drift ≤ 0.001 ms/min, B − 2·A ≤ 0.002 ms, C − A and D − B 0.000, the accent on beat 1;
    no take rejected. Each device open lands a whole number of frames off (−5..+4), the same for every
    take inside it (cause unknown). With the multiply phases (2026-09-26, Pro-Q, three launches): E − A
    and F − A 0.000 and lane 1 bit-exact twice at 64, 128 and 256, except once at 128, where the 32 s E
    take stepped one frame late between its beats 24 and 32 and F kept that frame; two relaunches at
    128 did not repeat it (a one-frame slip inside an open; cause unknown).
  - `native:loopback` after a change to record compensation, the plugin bridge or the drift
    controller. Baseline (2026-09-24, Scarlett 2i2 3rd gen, ASIO 256, Pro-Q 3, a cable from line
    out R into input 2): residual +64/+65 ms at trim 0. With the worklet reading the ring directly,
    seven launches at trim 60: residual +8.2..+12.4 ms in six, −8.1 ms in one; RT 44.4 ms; no take
    rejected (before: about one launch in three); drift inside take A 0.7–3.2 ms/min in six,
    −10.8 ms/min in one where the controller wound to +143 ppm from a start offset. WASAPI
    (`pnpm exec node scripts/native-probe.mjs loopback-sync --channel=1`), measured before the worklet
    change: RT ~280 ms, residual −125 ms at trim 60. The probe runs the debug build. Two of about twelve launches failed GO LIVE
    with an `arm_monitor` timeout (cause unknown); a relaunch passed. The gate `[diag]` line's
    `pace_late`/`pace_reanchors`/`pace_late_max_us` say whether the producer kept its clock,
    `input_fill_max` how late it ran, `monitor_pads` what the same-clock monitor padded.
  - `native:smoke` after any change to `editor_window.rs` or either host's editor open/close path.
    Baseline (2026-09-23, WASAPI, the app mostly on the default ~15 ms timer tick): `complete: 30
    opened, 0 failed, of 30`, each close 110–250 ms. Windows grants the app a 1 ms tick only some of
    the time, whatever the global resolution reads, so a close near 800 ms points at a Win32 wait loop
    that counts rounds instead of keeping a deadline (`src-tauri/AGENTS.md`, timed waits) or at the
    plugin's own teardown, which stretches with the tick too.
  - `native:survey` when a plugin is installed. Restart flags arrive asynchronously, a line or two
    after their cause: `--settle=150` waits per parameter so a flag can be pinned on one. Baseline
    (2026-09-23, 30 plugins): 32 `restartComponent`, all `latency`, all FabFilter; no CLAP
    `request_restart`.
  - `native:swap` after a change to load, unload or swap. The full matrix is every ordered pair twice
    (1740 swaps for 30 plugins, hours): narrow it with `--filter`. Read the per-step ms in the log too:
    a swap that completes in 10 s is a freeze to the person waiting. Baseline (2026-09-23,
    `--filter="Surge XT Effects,Pro-Q,Gojira"`, CLAP and VST3, mostly the same tick): `complete: 24
    swapped, 0 failed`, each swap 40–380 ms; a swap away from Archetype Gojira 500–920 ms, ~600 ms of it the
    plugin's own teardown (`release=` in the `VST3 teardown` line; ~200 ms at a 1 ms tick).
  - `native:recall` after a change to the boot chain, load or unload, the close guard, or
    `src/audio/rig-recall.ts`. Baseline (2026-09-23, WASAPI, the default Surge XT Effects CLAP + Surge
    XT VST3): `PASS: 5 phases` in about 1 min, twice; the check phase's close reached the page
    250–450 ms after its verdict line, inside the settle window; the crash phase killed app.exe
    100–180 ms after the marker's line, and the next launch still found the marker. A run that fails
    midway can leave the probe's record behind: the next run's save phase unloads it and fails once
    ("run again").


## Mac vs PC split

On the Mac there is no Rust toolchain — `src-tauri/` is unverifiable (no `cargo`, `cfg(windows)`
paths). On Mac: verify the TS half (`pnpm check` / `build`), drive the web build via Playwright, and
adversarial-review any Rust by reading; defer `cargo check` + the runtime gate to the PC.
