# BleepLoop runtime verification

**This file is the playbook**, for every harness; the root `AGENTS.md` routes here.

The app is verified by **driving it and measuring**, not by reading code or trusting a typecheck.
Static gates first (`pnpm check`, `pnpm build`), then the runtime probe below. The engine's
behaviour is `cargo test -p lf-engine` (`pnpm test:engine`; CI runs it on every push that is not
docs-only); the frontend's
deterministic guards and browser probes live in `verify/` (see `verify/README.md`).

Git hooks are local checkout state. Before relying on the push gate, inspect the file returned by
`git rev-parse --git-path hooks/pre-push`; it must run `pnpm check`. If absent, install the configured
hook with `pnpm exec simple-git-hooks`. An existing `node_modules` directory does not prove that the
package's prepare script installed hooks in this checkout.

## Browser harness (web build, `pnpm dev` on http://localhost:1420: the UI, silent)

- **Drive the browser with Playwright,** which launches its own browser (claude-in-chrome does not
  run from WSL): **playwright-cli** when the session has it (async work =
  `eval "(async () => {…})()"`), else a plain `node` Playwright script on `verify/harness/probe.ts`
  (what every probe in `verify/probes/` does).
- **From WSL on the PC:** the `pnpm` wrapper hands a `/mnt/c` checkout to Windows `pnpm.exe`, so
  every `pnpm` command, including the native ones below, runs on Windows node, Windows `cargo` and
  the Windows Playwright browsers. That is the working lane. Vite then listens on Windows localhost
  only: `curl` from WSL hangs, and Linux node cannot drive it. A `pnpm install` that wants to rebuild
  `node_modules` aborts without a TTY: pass `--config.confirmModulesPurge=false`. An ad-hoc
  `tauri dev` from WSL takes env through `WSLENV=A:B`. Only the by-ear gates need a person at the PC.
- The browser build has no engine. For looper UI, turn on the engine fake (an init script setting
  `window.__lfEngineFake = true` before the app loads), script feed frames with `__lf.native.emit`
  and read what the UI sent in `__lf.native.sent` (pattern: `verify/probes/engine-seam.mjs`), then
  assert via DOM reads + screenshot. The web build hides Tauri-only chrome (settings gear); drive
  the reactive state directly instead. If a probe needs a missing handle, add it to `__lf` and keep
  it so the next probe can reuse it.

## The DEV debug hook

`window.__lf` (built in `src/debug/lf.ts` behind a declared `LfDebug` interface, installed by `app.tsx` in DEV only) exposes
the input router, instrument and plugin-slot controls, the engine store's clock, looper and master,
the layout store, audio-device settings, the MIDI manager, the platform and the engine fake
(`native`), session import/export and autosave, the notification store and the popover drivers. This
is how you drive and inspect the app from Playwright.

## Measuring audio (not hearing it)

The browser build is silent, so sound is measured on the engine: offline in `cargo test -p
lf-engine` (rendered PCM, frame by frame; the synths and FX against reference renders), and in the
running app through the `native:*` probes below (`native:engine-loopback` through a cable). In a
probe, a note is a command: `__lf.inputRouter.handle({type:'on',note,velocity,source})` sends it to
the engine (the fake's `sent`), and `velocity` is MIDI **0..127** (a 0..1 value plays ~40 dB too
quiet and looks like a gain bug; it isn't).

## Recurring web-verify gotchas

- **Web MIDI shows `unsupported` under Playwright's Chromium**, as expected (real Chrome/Edge has it;
  WebView2 has the native path). Not a bug.
- `getComputedStyle` right after a manual `classList` mutation in a Playwright `evaluate` returns
  STALE values → **verify rendered CSS by screenshot, not getComputedStyle**.
- **playwright-cli specifics:** `fill` alone does NOT fire Solid's `onChange` (native `change` fires
  on blur/Enter: follow with `playwright-cli press Enter`); a leading-minus value parses as a CLI flag
  (pass it after `--`); `screenshot --filename` lands in the CLI's cwd (the repo root: move it out),
  bare `screenshot` lands in `.playwright-cli/`; the `default` session is SHARED across concurrent
  agent sessions on this machine, so take a named one (`playwright-cli -s=lf …`) that a parallel
  session cannot hijack mid-probe.
- `file://` is BLOCKED in Playwright (mockups need `python3 -m http.server 8765`).

## Native / Tauri verification (PC only)

These commands run on Windows node, and from WSL through the `pnpm` wrapper:

| Command | What it does |
|---|---|
| `pnpm rust:check` | `cargo check` without and with `asio`, then `cargo test`, all `--no-default-features`, from `src-tauri/`. It fails the asio step when `CPAL_ASIO_DIR` lacks the SDK. |
| `pnpm native:smoke` | Editor smoke (`src/debug/editor-smoke.ts`): every installed plugin loads into slot 0, and its editor opens (a CLAP plugin's own floating window when it offers one, else the host window) and closes. |
| `pnpm native:survey` | Restart survey (`src/debug/restart-survey.ts`): which plugins raise a restart or rescan request, and on which parameter. Also prints a `value-check` line per plugin (controller value vs what the host set). |
| `pnpm native:swap` | Swap stress (`src/debug/swap-stress.ts`): every ordered pair is swapped in place in slot 0, first with the editor closed, then open, after tweaking the loaded plugin. A step that takes longer than 30 s prints `TIMEOUT`. |
| `pnpm native:recall` | Rig recall (`src/debug/recall-restart.ts`), five launches: two plugins loaded and an input channel set come back after a restart and after a WebView reload, unarmed, and a close through the window's close button right after that reload keeps them for the next launch; a launch whose app.exe is killed while restoring makes the next one skip the recall with one log line and one toast; the launch after that is clean. |
| `pnpm native:spike` | The engine's premise spike (`docs/ARCHITECTURE.md` § Measured premise): builds the debug app with ASIO and runs `--probe-engine-spike` (one-callback ASIO alignment, the echo round trip with Pro-Q, the amp-sim's callback cost, WASAPI) and `--probe-share` one process at a time, then prints one PASS/FAIL table. Needs the loopback cable (default line out R → input 2) and makes audible chirps; runs at the device's current rate. `--only=`, `--blocks=`, `--launches=`, `--long-min=` narrow it. |
| `pnpm native:engine` | The device side's rig run: builds the debug app with ASIO and runs one `--probe-engine` process (`src-tauri/src/engine_io/probe.rs`): the engine at ASIO 128 with the amp-sim and Pro-Q in its two slots, a loop on lane 0, a 10-minute soak, 20 backend/buffer switches and 4 plugin swaps while the loop plays. Prints each phase's counters and block time (with `over_budget=N` when N callbacks took their whole period or more) and one PASS/FAIL per check; exit 0 = all pass. `--scene=heavy` soaks the probe's heavy scene instead of one idle loop (all five lanes with every effect, the input sends, the slots live, a held chord, the click: the probe's header). `--profile=rig` builds and runs the optimized `target/rig/app.exe` (`[profile.rig]` in `src-tauri/Cargo.toml`); the debug build's block times include `app_lib` at opt-level 0, so they are not the release build's. Needs no cable; the take records `--in` (default input 1). `--seconds=`, `--switches=`, `--swaps=`, `--buffer=`, `--amp=`/`--proq=` (empty: no plugin) narrow it; `--mute=1` plays silence; `--share=<endpoint id>` turns Share output on after the open (the id the release log names, `wasapi:{…}.{…}`), so the soak counts the mirror's starves, overruns and trims. `--cycle=` names the switches' round (`asio64`, `asio128`, `asio256`, `wasapi`, comma-separated; `--backend=wasapi --buffer=default --cycle=wasapi` closes and reopens WASAPI at every switch, with no ASIO in the process), `--hold=` the seconds each switch plays (default 2), `--pause=` closes the device and waits that many ms before every switch to WASAPI, `--log=<name>` the log's name. After every WASAPI phase it prints the join's pushes and pulls since that open (the `j` lines: time, frames, the fill each pull found, zero-fills, trims; the debug build keeps an open's first ~10 s). `--lag=1 --in=1 --seconds=10` runs only the lag phase instead (needs the loopback cable, audible chirps): the premise's alignment bar on the engine's own open path, the chirp's lag against the driver's reported latency within 1 ms; `--preopen=0` skips the ASIO preopen (`src-tauri/src/engine_io/cpal_driver.rs`) for a before-and-after. |
| `pnpm native:engine-smoke` | The UI on the native engine (`src/debug/engine-smoke.ts`), ASIO, in its own app profile (`scripts/engine-probe.tauri.json`): the device reopens at `--buffer=` (default 128) and the rate pick `--rate=` (44100 or 48000; default none: the driver's own), picked as Audio Settings does, and must then run at that rate; a FIXED 1-bar first take counts in 4-3-2-1 with the click (feed and screen) and plays one bar at the rate that runs; a later take, an overdub with UNDO, STOP ALL, PLAY ALL and CLEAR change the lanes; `--plugin=<name>[:<format>]` (e.g. `Pro-Q:vst3`) also loads that plugin into slot 1, goes live and checks that the input meter moves. Fails on any other frontend `console.error`; the run ends with a close through the window, as the close button does. |
| `pnpm native:engine-recovery` | Engine mode's session paths (`src/debug/engine-recovery.ts`), ASIO, in engine-smoke's profile, two launches: two lanes recorded from the input with the click on (the loopback cable gives them audio; a silent lane fails) are exported to a zip, cleared and imported back with the same PCM and mix, and once autosave holds the jam app.exe is killed with the loops playing; the relaunch restores the same lanes from recovery, then clears them (which deletes the recovery) and closes through its window. |
| `pnpm native:tone-recall` | Tone recall on the engine (`src/debug/tone-recall.ts`), WASAPI, in its own profile (`scripts/tone-probe.tauri.json`), master muted, three launches. Save: loads two plugins (default `Pro-Q 3:vst3,Surge XT Effects:clap`, `--plugins=`), moves their first parameters through the host's set_param, unloads and loads both, keeps per slot the first parameter that came back exactly and away from its default, moves it once more and closes, so the exit path saves. Check: rig recall restores both at that value; an export carries a tone per slot; moving the values and importing restores them and keeps slot A's GO LIVE; an import with slot B emptied toasts and loads nothing there, and the plugin's next load restores the session's value; the runner kills app.exe 3.5 s after the last change. After: that change is back (the debounced save alone). Hears nothing and cannot reach a plugin's own editor (`performEdit`, CLAP output events and `mark_dirty` are the Rust fixtures'). |
| `pnpm native:export-master` | The export's wet master on the engine (`src/debug/export-master.ts`), WASAPI, in its own profile (`scripts/export-probe.tauri.json`), one launch: the output moves to the endpoint named like "CABLE Input" (VB-Audio Virtual Cable) when there is one, else the master volume goes to 0.1; one Lead note (velocity 100) through the input router into a FIXED 1-bar take at 120 BPM, click off; `buildExportBundle`'s session.json says `master.kind` 'wet-engine', the master WAV is stereo, one loop long and not silent, and its onset lies within 2 frames of the stem's. Clears the looper and closes through its window. Hears nothing: how the master sounds is the owner's ear. |
| `pnpm native:engine-loopback` | Where takes land against the click on the engine (`src/debug/engine-loopback.ts`), ASIO, in engine-smoke's profile, one launch: with an interface output cabled into input 2 (`--channel=<0-based>`, default 1), at each of `--buffers=` (default `64,128,256`), at the rate pick `--rate=` (as engine-smoke's), a FIXED first take of the click (A), a later take of lane 1's playback (B), STOP ALL → PLAY ALL, the click again (C) and lane 1 again (D), a multiply take of the click at two loops (E) and, over the grown loop, the click again (F); then, after CLEAR ALL and a first take of half the bars, a FREE take (FIXED off) of the click stopped 1.75 loops in, which records on to two loops and grows the loop (G, E10), and lane 2 TRIMmed to half the small loop's bars and taken through the cable by lane 3 (H, F16); then FADE (all) over two bars, pressed half a second before a loop boundary with lane 1 alone heard, while lane 4 takes the cable from that boundary (I). An offset is the recorded click's onset (the 30 % crossing in the 10 ms before its window's peak) against its beat in the committed lane PCM, less the detector's lag on the engine's ideal click; nothing is fitted. Sound between the clicks that is not theirs is logged per take, and at 0.3 of a click or more fails the run as stray sound (source unknown; a self-test proves the bar catches a doubled click). Bars: \|A\| ≤ 2 ms, spread ≤ 1 ms, drift ≤ 0.1 ms/min, the accent on beat 1, and B = 2·A, C = A, D = B, E = A, F = A, G = A, H = B within 0.1 ms, lane 1 holds its loop twice, bit for bit, after the multiply and after the free take, the trimmed lane's PCM is its first bars repeated bit for bit, every beat of H clicks, and UNDO gives the trimmed lane back bit for bit; in I the fading lanes stop on the downbeat two bars on, each beat's click through the cable falls, at ((8 − k)/8)² of the first within 10 % for k ≤ 5 (the fade's squared ramp), silence follows the bar line, and PLAY ALL brings lane 1 back at its level (the input meter's peak within 0.8..1.25 of before, its volume unchanged); any failed bar fails the run. `--plugin=` picks the live effect (default `Pro-Q:vst3`; `none` = MIC, the input dry), `--bars=` the take length; `--echo=1` has IN FX's ECHO (1/16, no feedback, level 0.5) on while A records and bars, beat by beat, an echo after every click, each one sixteenth after it (within 0.05 ms) at half its level (a check that first fails a synthetic take whose echoes stop partway), C = A then saying the echo left the dry click in place; with MIC, `--unload-first=<plugin>` loads and unloads that plugin in slot 1 first, and the take's `peak gain` (cable × slot gain) should equal a run without it. Audible clicks. |
| `pnpm release:smoke` | The RELEASE build on the engine, driven from outside (`scripts/release-smoke.mjs`). It needs a release exe in a profile of its own. For a release a runner builds it, in the job that builds the shipped exe (the same pinned ASIO SDK) and with nothing compiled on the PC: `gh workflow run build-exe.yml --ref main -f smoke=true`, then `gh run download <run id> -n "BleepLoop-smoke-$(git rev-parse HEAD)" -D logs/release-smoke/exe` and `--exe=logs/release-smoke/exe/app.exe` (a run that built another commit has no such artifact; `gh` does not overwrite the last run's exe, so empty the folder first). Or build it here: `pnpm exec tauri build --no-bundle --features asio --config scripts/release-smoke.tauri.json`, which leaves `src-tauri/target/release/app.exe`, the default. It launches the exe with WebView2's remote-debugging port and attaches Playwright over CDP: the UI boots with no CSP violation or uncaught error, on the engine. The exe must name the checkout's HEAD as its build commit in Help → About this build; a build of another commit, of a tree with uncommitted changes or made without git fails there (`scripts/release-exe-rule.mjs`), so a commit after the build needs a new build or a checkout of the built commit. ASIO at `--buffer=` (default 128) is picked in Audio Settings; slot 1 goes Off on input `--channel=` (default 1 = input 2) and live in its header; the feed moves the beat LEDs, the meter and a playhead; a FIXED 1-bar take with the click goes armed → rec → play with a waveform; CLEAR ALL and the OS close end the process within 60 s with no ERROR in the release log. `--fresh` starts from an empty profile, `--exe=` drives another exe (the owner's identifier is refused without `--owner-profile`). It hears nothing: timing is `native:engine-loopback`'s. The take records the input first, which needs the loopback cable at a working level: with the line out turned low the clicks read 9 of 63 px, under its 15 % bar. A flat input take fails the run when this release must prove the input path again (`scripts/release-input-rule.mjs`), judged against the last release (the nearest `v*` tag that is not this version's own, so the run belongs on a clean tree after the version bump): no such tag, no answer from git, a tree that is not clean (commit first), a changed path under `src-tauri/src/engine_io/` or `src-tauri/src/host/`, `audio_output.rs` or `asio_startup.rs`, a `src-tauri/Cargo.toml` that differs beyond its `[package]` version line, or a `cpal` or `asio-sys` entry of `src-tauri/Cargo.lock` that differs, came or went; otherwise lane 1 is cleared and a held Organ chord (four PC keys: 47 of 69 px on the rig) is recorded in its place, and the `take` line says which take passed. Only a run with the cable in proves the input path. The build on the PC leaves its exe, with the smoke identifier, at the path `pnpm build:app` writes. |
| `pnpm native:kill` | Stops `app`, `cargo` and whatever owns port 1420. |

A `native:*` probe run by `scripts/native-probe.mjs` launches `tauri dev` (WASAPI; `--asio` for ASIO; `native:engine`
and `native:spike` instead build the app and launch it directly, headless) with the probe's
`VITE_LF_PROBE`, waits for its verdict line, stops the run and prints
`=== <probe>: PASS|FAIL: … ===`. `native:recall` launches once per phase (`VITE_LF_PROBE_PHASE`) and
lets the app close itself between phases, except after the check phase, whose window the runner sends
the OS close (as the close button does), and in the crash phase, where it kills app.exe alone. Only
`native:recall` and `native:tone-recall` restore plugins at launch, each from a record of its own, never the owner's; the other
probes start from empty slots. Every probe runs in a profile of its own, never the owner's: smoke,
survey, swap and recall in `scripts/classic-probe.tauri.json` (their first run scans plugins cold), the
engine probes in `scripts/engine-probe.tauri.json`, tone-recall in its own. Windows open on the PC desktop and
no gesture is needed. It refuses to start while an app is running. `--<knob>=<value>` sets `VITE_LF_PROBE_<KNOB>` (`--filter=Pro-Q,Saturn`;
each probe's header lists its knobs), and the full log lands in `logs/native-<probe>.log`. It
blocks until the verdict, so an agent harness should run it in the background.

- **No Playwright into WebView2,** `release:smoke`'s CDP attach to the release exe excepted (the table
  above). A standalone debug build with the frontend built into it (`tauri build --debug --no-bundle`; a
  plain `cargo build` loads the dev server instead), started with WebView2's remote-debugging port,
  may be driven over CDP the same way, for a check that must run without `tauri dev` (it does not
  watch the tree, so it can run while the tree is being edited). For Tauri/native verification: grep `tauri dev` stdout for
  `[diag]`. **What reaches that stdout: only Rust `invoke('diag')`/`log::info!` lines + the
  frontend's `console.error`, forwarded through `frontend_log` as `[webview][ERROR]`
  (`src/platform/logging.ts`); plain `console.log` from WebView2 does NOT.**
- The committed `native:*` probes (table above) runtime-gate the paths they name. For any other
  native path, temp-wire a probe that drives the PRODUCTION fns, grep, then revert.
- `tauri dev` does NOT self-terminate: stop it with `pnpm native:kill`. **Never kill all node: the
  agent session may be a node process.**
- Screenshot the native window by its handle: `PrintWindow(hwnd, 3)` (client only + full content)
  from a DPI-aware process captures it without focus. Never grab the screen (`CopyFromScreen`
  after `SetForegroundWindow`): it captures whatever window is on top, including the owner's.
- Runtime-gate logs go under gitignored `logs/` (e.g. `logs/dev-asio.log`), not the repo root.
- `tauri dev` watches ALL of `src-tauri/`: a doc edit there (`AGENTS.md` included) rebuilds and
  relaunches the app mid-probe. Write docs after the run, or outside `src-tauri/`.
- Background the dev run and poll the log until it prints the line you want, rather than blocking on
  it. (*Claude Code specifics:* background via the PowerShell `run_in_background` tool, poll with a
  Bash `run_in_background` `until grep -q … ; do sleep 2; done` loop; that harness blocks foreground
  `sleep` and PowerShell `Start-Sleep`+chaining.)
- **The app runs at Normal priority class on the rig, as the owner's does.** Windows processes
  started from the rig PC's logon tmux session run at below-normal priority (the scheduled task's
  priority 7), `cargo` included. `native:engine`, `native:spike` and `release:smoke` raise the app
  they spawn to Normal (MMCSS callback threads sit at 24–25 either way); `native:probe` launches
  through `tauri dev` and keeps the shell's class, as does an exe started by hand:
  `cmd.exe /c start /normal /b /wait <exe> …` launches at normal. Whether the class moves a result is
  not settled (`src-tauri/AGENTS.md` § Open threads, the join).
- The plugin scan works WITHOUT the full app (`--scan-one`): see `src-tauri/AGENTS.md`
  "Native-host verify ops".
- **When to run the plugin probes, and their baselines** (the verdict alone doesn't say this). Narrow
  `smoke`, `survey` and `swap` to `--filter="Surge XT Effects,Pro-Q,Gojira"` (CLAP and VST3, a
  separated controller, FabFilter's latency restarts, Neural DSP's slow teardown) unless the change
  reaches every plugin (scan, load, the editor host) or a plugin was just installed (survey that one):
  a full sweep opens every installed plugin on the owner's desktop.
  - `native:engine` after a change to the device owner, the callbacks, the pipes or the slot
    handshake. Baseline (2026-09-25, ASIO 128, Archetype Petrucci X and Pro-Q 3): every check passes,
    every counter 0 over 224 763 callbacks, 20 switches and 4 swaps; soak block time p99.9 < 33 %, max
    < 52 %; every ASIO re-open retries its input build once (a BadMode, then fine); unloading
    Archetype while a second instance ran took 4.8 s, nearly all of it the plugin's own teardown, the
    slot dry meanwhile. Share output's soak (`--share=`, ASIO 64, no plugins, `--mute=1
    --seconds=1500`, the mirror into the Scarlett's own Windows endpoint, 2026-10-06): every counter
    0 over 26 minutes at the 40 ms setpoint; at 20 ms, 37 `share_starves` and 2 trims in two bursts
    10 min 48 s apart, so soak 25 minutes or more (a shorter run can sit between two bursts).
    Each soak minute's line carries that minute's block time and the glitch diagnostics
    (`asio_phase_slips`, `clipped_blocks`), which no check fails on: they time a spike.
    On WASAPI with another app holding the microphone the input can run 0.87 %
    fast, past what the join's controller holds: `join_trims` every 3–6 s, each skipping ~25 ms of
    input (real time or an artefact: unknown). In the first ~2.5 s after a WASAPI open the join can
    trim (1–2) or starve (2–4): 9 of 85 WASAPI opens after ASIO had run in the process
    (`--cycle=asio64,wasapi`) and 1 of 43 with no ASIO in it (`--backend=wasapi --buffer=default
    --cycle=wasapi`) on the Scarlett at 48 kHz with Signal Desktop and Focusrite Notifier running
    (2026-10-03); 17 of 100 after ASIO and 2 of 31 without, with both plugins and 2 swaps, at 44.1 kHz
    with Focusrite Notifier alone (2026-10-05), while the soak, every ASIO phase and the swaps stay 0.
    `engine.xruns` also counts a block whose input a starve or a trim damaged, and the block after it,
    so such an open reads two to seven xruns. The counter check forgives the join's trims and starves
    and their engine xruns in an open's first 3 s (D25) and prints what it forgave (`forgiven in …`);
    a take that overlaps one still fails `loop`. The mechanism: `src-tauri/AGENTS.md` § Open threads.
  - `native:engine-loopback` after a change to the engine's alignment, click, grid, device open or
    snapshot. Baseline (2026-09-26, Scarlett 2i2 3rd gen, 44.1 kHz, a cable from line out R into
    input 2; six launches, Pro-Q 3 and MIC): A −0.11..+0.09 ms at ASIO 64, 128 and 256, spread
    0.001 ms, drift ≤ 0.001 ms/min, B − 2·A ≤ 0.002 ms, C − A and D − B 0.000, the accent on beat 1;
    no take rejected. Each device open lands a whole number of frames off (−5..+4), the same for every
    take inside it (cause unknown). With the multiply phases (2026-09-26, Pro-Q, three launches): E − A
    and F − A 0.000 and lane 1 bit-exact twice at 64, 128 and 256, except once at 128, where the 32 s E
    take stepped one frame late between its beats 24 and 32 and F kept that frame; two relaunches at
    128 did not repeat it (a one-frame slip inside an open; cause unknown). With the free-take and TRIM
    phases (2026-09-27, Pro-Q, one launch): G − A and H − B 0.000 at 64, 128 and 256, H 32/32 beats,
    lane 1 bit-exact twice after G, the trimmed lane and its UNDO bit-exact; no take rejected. With the
    FADE phase (2026-09-27, Pro-Q, two launches): at 64, 128 and 256 the fading lanes stopped on the
    downbeat two bars on, the beats of I read 1.000 0.756 0.556 0.386 0.250 0.139 0.062 0.016 against
    ((8 − k)/8)² 1 0.766 0.563 0.391 0.250 0.141 0.063 0.016, past the bar line a quarter of the floor
    bar, and lane 1 came back at 1.000 after PLAY ALL; the first launch failed that last bar once at 256
    (0.563: a meter read every 20 ms missed the bar's accent, now read every 4 ms over two bars).
    After the per-slot input and the record-compensation latch (2026-09-28, a guitar cable from line
    out R into input 2, input gain at 11 o'clock, peak gain 0.33; the cable used before gave no signal
    on either input): 20/20 bars at 64 and 256; at 128, in two launches, B and D (re-recorded, peak
    gain 0.106) each read one beat 147–250 ms early, near its window's start (once take A too), which
    fails spread and drift while every other beat sits within 0.005 ms. v0.1.0's code on the same rig
    reads the same in B and D, so it predates the change. On the engine-only app (2026-09-29, the same
    rig): the same B and D reading at 128; one launch at 64 read 149× the floor bar past the fade's bar
    line in I, its relaunch 0.302. Cause of the B and D reading (2026-10-02, 48 kHz, five launches,
    every stray logged): sound that is not the click reaches the cable, about 200 Hz decaying over
    ~1.4 s and a ~400 Hz tone repeating every 6.000 s, mostly in the first take after a device switch
    (B and D replay A's lane), and the old onset (the first 30 % crossing anywhere before the window's
    peak) took it for the click; the onset now stays by the peak and such sound fails by name (the stray
    bar). Its source is unknown: other apps playing through the interface (its driver mixes their audio
    into the same line outs; Signal Desktop and Focusrite Notifier run on this PC) are the suspect. The I
    reading is likely the same (not re-observed). At 256 every bar passed; at 64 and 128 every bar but
    those the sound moved (spread, drift, once the accent), |A| at most 0.125 ms. Silence other apps'
    sound on the interface before a loopback run. With the stray bar and the input-gap rejection
    (2026-10-03, 48 kHz, Pro-Q, one launch): 21/21 bars at 64, 128 and 256, no stray sound, no take
    rejected, A +0.08 ms and B +0.17 ms at every size, spread at most 0.003 ms.
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
    `src/ui/state/rig-recall.ts`. Baseline (2026-09-23, WASAPI, the default Surge XT Effects CLAP + Surge
    XT VST3): `PASS: 5 phases` in about 1 min, twice; the check phase's close reached the page
    250–450 ms after its verdict line, inside the settle window; the crash phase killed app.exe
    100–180 ms after the marker's line, and the next launch still found the marker. A run that fails
    midway can leave the probe's record behind: the next run's save phase unloads it and fails once
    ("run again").
