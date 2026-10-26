# BleepLoop runtime verification

**This file is the playbook**, for every harness; the root `AGENTS.md` routes here.

The app is verified by **driving it and measuring**, not by reading code or trusting a typecheck.
Static checks first (the ones the change can reach; root `AGENTS.md`, the `main` line), then the runtime probe below. The engine's
behaviour is `cargo test -p lf-engine` (`pnpm test:engine`; CI runs it on a push to `main` or a pull
request into it that changes native code, not on Markdown alone); the frontend's
deterministic guards and browser probes live in `verify/` (see `verify/README.md`).

Git hooks are local checkout state. Before relying on the push gate, inspect the file returned by
`git rev-parse --git-path hooks/pre-push`; it must run `node scripts/pre-push.mjs`. If absent or
older, install the configured hook with `pnpm exec simple-git-hooks`. An existing `node_modules`
directory does not prove that the package's prepare script installed hooks in this checkout. The hook
gates what is pushed, not the working tree: it refuses a push unless every pushed ref is the checked-out
HEAD and the tree is clean (untracked files count; deletes need no gate), then runs `pnpm check` once.
Push any other commit from a clean checkout of it.

The owner's clones also run a public-hygiene commit check (`pre-commit`, `commit-msg`). Its script
and denylist never enter the tree: their master copy, setup and open threads are in the owner's
private maintainer repo, cloned into the gitignored maintainer/ folder, and the hooks are installed
from maintainer/hooks/ into the clone's own hooks folder. From each worktree that will commit, run
`bash "$(git rev-parse --git-path hooks/check.sh)" --self-test`. It does not see `--no-verify` (the
deliberate bypass), commits it did not make (`am`, `cherry-pick`, `rebase`, `revert`, merged-in
content, an amend of a commit made without it), the author identity, tag messages, branch names, or
the content of binary and `-diff` files.

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
the UI's note input (`input`), instrument and plugin-slot controls, the engine store's clock, looper and master,
the layout store, audio-device settings, native MIDI's UI state (`midi`), the platform and the engine fake
(`native`), session import/export and autosave, the notification store and the popover drivers. This
is how you drive and inspect the app from Playwright.

## Measuring audio (not hearing it)

The browser build is silent, so sound is measured on the engine: offline in `cargo test -p
lf-engine` (rendered PCM, frame by frame; the synths and FX against reference renders), and in the
running app through the `native:*` probes below (`native:engine-loopback` through a cable). In a
probe, a note is an input event: `__lf.input.note(owner, note, velocity, on)` hands it to native MIDI's
router (the fake records it in `__lf.native.inputSent`; native MIDI's events are scripted with
`__lf.native.midiEmit`), and `velocity` is MIDI **0..127** (the router sends the engine `v/127`).

## Recurring web-verify gotchas

- **No MIDI device ever shows in the browser build:** MIDI is native (the app never opens Web MIDI), so
  a probe scripts the ports and bindings with `__lf.native.midiEmit`. Not a bug.
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
| `node scripts/native-probe.mjs editor-premise --case=<reload\|other\|b-first>` | The editor wedge the affinity guard exists for, measured past the guard (`src/debug/editor-premise.ts`): one plugin file (`--filter=`, default Archetype; `--format=`) in both slots, then A's editor after A goes Off and loads it again (`reload`), B's after A's opened and closed (`other`), or B's first with A's never opened (`b-first`). Each open races 45 s; the verdict line reads `no wedge` or `WEDGED` with the host's answer. Its PASS only means the run finished. Opens the plugin's editor on the desktop. |
| `pnpm native:survey` | Restart survey (`src/debug/restart-survey.ts`): which plugins raise a restart or rescan request, and on which parameter. Also prints a `value-check` line per plugin (controller value vs what the host set). |
| `pnpm native:swap` | Swap stress (`src/debug/swap-stress.ts`): every ordered pair is swapped in place in slot 0, first with the editor closed, then open, after tweaking the loaded plugin. A step that takes longer than 30 s prints `TIMEOUT`. |
| `pnpm native:recall` | Rig recall (`src/debug/recall-restart.ts`), five launches: two plugins loaded and an input channel set come back after a restart and after a WebView reload, unarmed, and a close through the window's close button right after that reload keeps them for the next launch; a launch whose app.exe is killed while restoring makes the next one skip the recall with one log line and one toast; the launch after that is clean. |
| `pnpm native:spike` | The engine's premise spike (`docs/ARCHITECTURE.md` § Measured premise): builds the debug app with ASIO and runs `--probe-engine-spike` (one-callback ASIO alignment, the echo round trip with Pro-Q, the amp-sim's callback cost, WASAPI) and `--probe-share` one process at a time, then prints one PASS/FAIL table. Needs the loopback cable (default line out R → input 2) and makes audible chirps; runs at the device's current rate. `--only=`, `--blocks=`, `--launches=`, `--long-min=` narrow it. |
| `pnpm native:engine` | The device side's rig run: builds the debug app with ASIO and runs one `--probe-engine` process (`src-tauri/src/engine_io/probe.rs`): the engine at ASIO 128 with the amp-sim and Pro-Q in its two slots, a loop on lane 0, a 10-minute soak, 20 backend/buffer switches and 4 plugin swaps while the loop plays. Prints each phase's counters and block time (with `over_budget=N` when N callbacks took their whole period or more) and one PASS/FAIL per check; exit 0 = all pass. `--scene=heavy` soaks the probe's heavy scene instead of one idle loop (all five lanes with every effect, the input sends, the slots live, a held chord, the click: the probe's header). `--profile=rig` builds and runs the optimized `target/rig/app.exe` (`[profile.rig]` in `src-tauri/Cargo.toml`); the debug build's block times include `app_lib` at opt-level 0, so they are not the release build's. Needs no cable but for `--tone=` and `--lag=1`; the take records `--in` (default input 1). `--seconds=`, `--switches=`, `--swaps=`, `--buffer=`, `--amp=`/`--proq=` (empty: no plugin) narrow it; `--mute=1` plays silence; `--share=<endpoint id>` turns Share output on after the open (the id the release log names, `wasapi:{…}.{…}`), so the soak counts the mirror's starves, overruns and trims. `--cycle=` names the switches' round (`asio64`, `asio128`, `asio256`, `wasapi`, comma-separated; `--backend=wasapi --buffer=default --cycle=wasapi` closes and reopens WASAPI at every switch, with no ASIO in the process), `--hold=` the seconds each switch plays (default 2), `--pause=` closes the device and waits that many ms before every switch to WASAPI, `--log=<name>` the log's name. After every WASAPI phase it prints the join's pushes and pulls since that open (the `j` lines: time, frames, the fill each pull found, zero-fills, trims; the debug build keeps an open's first ~10 s). `--tone=1 --mute=1` (ASIO; needs the loopback cable from output `--out=`, 0 or 1 and default 1, into that input, which slot 1 then reads) plays a steady sine through the soak and watches its return for loopback discontinuities, each tied to the nearest callback that ran long or entered late: the `tone` check ends `tone: clean`, `tone: discontinuities` or `tone: not measured` (the probe's header; an event may be the input side's). Each long or late callback in its log says when it finished after the wakes' best phase, in periods, and what share of the time-stamp counter's ticks its thread ran (near 100 %: not taken off the CPU; `tone::cycle_stamp` says what that cannot tell). `--tone-from=S` starts the tone S seconds into the stream, before the setup (default: 10 s, after it), and `--tone-level=X` plays it at amplitude X (default 0.25); an event that opens with no residual spike gets a trace of its windows' phase and level. `--load=test` runs a build's load beside the soak (the workspace's `cargo test` in cargo's order at normal priority, pass after pass, from the tone's calibration; `--load-affinity=<hex>` keeps it to those processors), prints each pass and the seconds loaded, and exits 3 when the probe passed but the load never ran, failed or ran no test. `--lag=1 --in=1 --seconds=10` runs only the lag phase instead (needs the loopback cable, audible chirps): the premise's alignment bar on the engine's own open path, the chirp's lag against the driver's reported latency within 1 ms; `--preopen=0` skips the ASIO preopen (`src-tauri/src/engine_io/cpal_driver.rs`) for a before-and-after. |
| `pnpm native:engine-smoke` | The UI on the native engine (`src/debug/engine-smoke.ts`), ASIO, in its own app profile (`scripts/engine-probe.tauri.json`): the device reopens at `--buffer=` (default 128) and the rate pick `--rate=` (44100 or 48000; default none: the driver's own), picked as Audio Settings does, and must then run at that rate; a FIXED 1-bar first take counts in 4-3-2-1 with the click (feed and screen) and plays one bar at the rate that runs; a later take, an overdub with UNDO, STOP ALL, PLAY ALL and CLEAR change the lanes; `--plugin=<name>[:<format>]` (e.g. `Pro-Q:vst3`) also loads that plugin into slot 1, goes live and checks that the input meter moves. Fails on any other frontend `console.error`; the run ends with a close through the window, as the close button does. |
| `pnpm native:engine-recovery` | Engine mode's session paths (`src/debug/engine-recovery.ts`), ASIO, in engine-smoke's profile, two launches: two lanes recorded from the input with the click on (the loopback cable gives them audio; a silent lane fails) are exported to a zip, cleared and imported back with the same PCM and mix, and once autosave holds the jam app.exe is killed with the loops playing; the relaunch restores the same lanes from recovery, then clears them (which deletes the recovery) and closes through its window. |
| `pnpm native:tone-recall` | Tone recall on the engine (`src/debug/tone-recall.ts`), WASAPI, in its own profile (`scripts/tone-probe.tauri.json`), master muted, three launches. Save: loads two plugins (default `Pro-Q 3:vst3,Surge XT Effects:clap`, `--plugins=`), moves their first parameters through the host's set_param, unloads and loads both, keeps per slot the first parameter that came back exactly and away from its default, moves it once more and closes, so the exit path saves. Check: rig recall restores both at that value; an export carries a tone per slot; moving the values and importing restores them and keeps slot A's GO LIVE; an import with slot B emptied toasts and loads nothing there, and the plugin's next load restores the session's value; the runner kills app.exe 3.5 s after the last change. After: that change is back (the debounced save alone). Hears nothing and cannot reach a plugin's own editor (`performEdit`, CLAP output events and `mark_dirty` are the Rust fixtures'). |
| `pnpm native:export-master` | The export's wet master on the engine (`src/debug/export-master.ts`), WASAPI, in its own profile (`scripts/export-probe.tauri.json`), one launch: the output moves to the endpoint named like "CABLE Input" (VB-Audio Virtual Cable) when there is one, else the master volume goes to 0.1; one Lead note (velocity 100) through native MIDI's router (`input`) into a FIXED 1-bar take at 120 BPM, click off; `buildExportBundle`'s session.json says `master.kind` 'wet-engine', the master WAV is stereo, one loop long and not silent, and its onset lies within 2 frames of the stem's. Clears the looper and closes through its window. Hears nothing: how the master sounds is the owner's ear. |
| `pnpm native:midi` | The UI's note sources through native MIDI's router (`src/debug/midi-switch.ts`), WASAPI, in its own profile (`scripts/midi-probe.tauri.json`), one launch with one WebView reload inside it, the master muted and at volume 0 (read back from the feed's reset frame before any note), the click off, no plugin, no take. Native MIDI's resync reaches a new listener (ports, bindings, learning null, held); the page's Web MIDI request is refused; PC keys through the keyboard's own handler: a keydown lights its key from native MIDI's held set, its keyup clears it, two keys on one note (an octave shift between them) hold it until the last lets go, a window blur and a slot switch to the other built-in synth each release a held key and its later keyup sends nothing, a key held across a reload (the old document silenced) is released at the new document's subscribe (a newer epoch; its first resync already holds no note) and the new document's keys land under its epoch, and a learn listening across that reload is cancelled (the new resync says learning null). It reads the document's own events and epoch from `devMidi` (`src/ui/state/midi.ts`, DEV). Fails on any other frontend `console.error` or a Rust ERROR line. Needs no MIDI port and drives no MIDI-side traffic; it cannot hear the instrument (the feed has no instrument level). |
| `node scripts/native-probe.mjs bench-load --asio` | The MIDI latency benchmark's jam load, unattended (`src/debug/bench-load.ts`), in engine-smoke's profile, one launch with `LF_MIDI_BENCH` set (§ MIDI latency benchmark): ASIO 128 at the driver's rate, master volume 0, the amp-sim (Archetype Petrucci X, else Pro-Q 3) live in slot 1, three FIXED 1-bar lanes at 120 BPM with the click, the stage view open. It plays no note; it holds the load until the benchmark's stalls have stopped for 15 s (or `--hold=` seconds, default 900, a failure), then empties the lanes and closes through the window. Passes only when the log carries the benchmark's report. |
| `pnpm native:engine-loopback` | Where takes land against the click on the engine (`src/debug/engine-loopback.ts`), ASIO, in engine-smoke's profile, one launch: with an interface output cabled into input 2 (`--channel=<0-based>`, default 1), at each of `--buffers=` (default `64,128,256`), at the rate pick `--rate=` (as engine-smoke's), a FIXED first take of the click (A), a later take of lane 1's playback (B), STOP ALL → PLAY ALL, the click again (C) and lane 1 again (D), a multiply take of the click at two loops (E) and, over the grown loop, the click again (F); then, after CLEAR ALL and a first take of half the bars, a FREE take (FIXED off) of the click stopped 1.75 loops in, which records on to two loops and grows the loop (G, E10), and lane 2 TRIMmed to half the small loop's bars and taken through the cable by lane 3 (H, F16); then FADE (all) over two bars, pressed half a second before a loop boundary with lane 1 alone heard, while lane 4 takes the cable from that boundary (I). An offset is the recorded click's onset (the 30 % crossing in the 10 ms before its window's peak) against its beat in the committed lane PCM, less the detector's lag on the engine's ideal click; nothing is fitted. Sound between the clicks that is not theirs is logged per take, and at 0.3 of a click or more fails the run as stray sound (source unknown; a self-test proves the bar catches a doubled click). Bars: \|A\| ≤ 2 ms, spread ≤ 1 ms, drift ≤ 0.1 ms/min, the accent on beat 1, and B = 2·A, C = A, D = B, E = A, F = A, G = A, H = B within 0.1 ms, lane 1 holds its loop twice, bit for bit, after the multiply and after the free take, the trimmed lane's PCM is its first bars repeated bit for bit, every beat of H clicks, and UNDO gives the trimmed lane back bit for bit; in I the fading lanes stop on the downbeat two bars on, each beat's click through the cable falls, at ((8 − k)/8)² of the first within 10 % for k ≤ 5 (the fade's squared ramp), silence follows the bar line, and PLAY ALL brings lane 1 back at its level (the input meter's peak within 0.8..1.25 of before, its volume unchanged); any failed bar fails the run. `--plugin=` picks the live effect (default `Pro-Q:vst3`; `none` = MIC, the input dry), `--bars=` the take length; `--echo=1` has IN FX's ECHO (1/16, no feedback, level 0.5) on while A records and bars, beat by beat, an echo after every click, each one sixteenth after it (within 0.05 ms) at half its level (a check that first fails a synthetic take whose echoes stop partway), C = A then saying the echo left the dry click in place; with MIC, `--unload-first=<plugin>` loads and unloads that plugin in slot 1 first, and the take's `peak gain` (cable × slot gain) should equal a run without it. Audible clicks. |
| `pnpm release:smoke` | The RELEASE build on the engine, driven from outside (`scripts/release-smoke.mjs`). It needs a release exe in a profile of its own. For a release a runner builds it, in the job that builds the shipped exe (the same pinned ASIO SDK) and with nothing compiled on the PC: `gh workflow run build-exe.yml --ref <branch> -f smoke=true`, whose `BleepLoop-smoke-<sha>` artifact goes in through `--exe=` (the commands: § The owner's loopback run below; a run that built another commit has no such artifact). Or build it here: `pnpm exec tauri build --no-bundle --features asio --config scripts/release-smoke.tauri.json`, which leaves `src-tauri/target/release/app.exe`, the default. It launches the exe with WebView2's remote-debugging port and attaches Playwright over CDP: the UI boots with no CSP violation or uncaught error, on the engine. The exe must name the checkout's HEAD as its build commit in Help → About this build; a build of another commit, of a tree with uncommitted changes or made without git fails there (`scripts/release-exe-rule.mjs`), so a commit after the build needs a new build or a checkout of the built commit. ASIO at `--buffer=` (default 128) is picked in Audio Settings, and the driver that answered is named in the run's `device` line rather than judged by brand (`--driver=<substring>` pins it when a run must prove one interface); slot A goes Off on input `--channel=` (default 1 = input 2) and live in its header; the feed moves the beat LEDs, the meter and a playhead; a FIXED 1-bar take with the click goes armed → rec → play with a waveform, neither flat nor run away (the meter at −1 dBFS or more during the take, or more than half the bar's columns tall: the loop through the cable feeding back, or the level too hot); CLEAR ALL and the OS close end the process within 60 s with no ERROR in the release log. `--fresh` starts from an empty profile, `--exe=` drives another exe (the owner's identifier is refused without `--owner-profile`). It hears nothing: timing is `native:engine-loopback`'s. The take records the input first, which needs the loopback cable at a working level: with the line out turned low the clicks read 9 of 63 px, under its 15 % bar. A flat input take fails the run when this release must prove the input path again (`scripts/release-input-rule.mjs`), judged against the last release (the nearest `v*` tag that is not this version's own, so the run belongs on a clean tree after the version bump): no such tag, no answer from git, a tree that is not clean (commit first), a changed path under `src-tauri/src/engine_io/` or `src-tauri/src/host/`, `audio_output.rs` or `asio_startup.rs`, a `src-tauri/Cargo.toml` that differs beyond its `[package]` version line, or a `cpal` or `asio-sys` entry of `src-tauri/Cargo.lock` that differs, came or went; otherwise lane 1 is cleared and a held Organ chord (four PC keys: 47 of 69 px on the rig) is recorded in its place, and the `take` line says which take passed. Only a run with the cable in proves the input path. The build on the PC leaves its exe, with the smoke identifier, at the path `pnpm build:app` writes. |
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
- Background the dev run and poll the log until it prints the line you want, rather than blocking
  on it.
- **The app runs at Normal priority class on the rig, as the owner's does.** Windows processes
  started from the rig PC's logon tmux session run at below-normal priority (the scheduled task's
  priority 7), `cargo` included. `native:engine`, `native:spike` and `release:smoke` raise the app
  they spawn to Normal (MMCSS callback threads sit at 24–25 either way); `native:probe` launches
  through `tauri dev` and keeps the shell's class, as does an exe started by hand:
  `cmd.exe /c start /normal /b /wait <exe> …` launches at normal. Whether the class moves a result is
  not settled (`src-tauri/OPEN-THREADS.md`, the join).
- The plugin scan works WITHOUT the full app (`--scan-one`): see `src-tauri/AGENTS.md`
  "Native-host verify ops".
- **Every rig baseline is bound to the interface that produced it.** The numbers below were measured on
  one audio interface and one ASIO driver, and a different interface invalidates them until they are
  measured again on it: the latency figures, the block-size behaviour, the join's timing under WASAPI,
  and the driver-specific workarounds the device side carries (`src-tauri/AGENTS.md`, the preopen at
  another block size). Name the interface beside any baseline you add, as the ones below do, and never
  compare a result on one against a baseline from another. Which interfaces the owner has:
  `STATUS.md` header.
- **When to run the plugin probes, and their baselines** (the verdict alone doesn't say this). Narrow
  `smoke`, `survey` and `swap` to `--filter="Surge XT Effects,Pro-Q,Gojira"` (CLAP and VST3, a
  separated controller, FabFilter's latency restarts, Neural DSP's slow teardown) unless the change
  reaches every plugin (scan, load, the editor host) or a plugin was just installed (survey that one):
  a full sweep opens every installed plugin on the owner's desktop.
  - `native:engine` after a change to the device owner, the callbacks, the pipes or the slot
    handshake. Baseline (Scarlett 2i2 3rd gen, ASIO 128, Archetype Petrucci X and Pro-Q 3,
    2026-09-25): every check passes, every counter 0 over 224 763 callbacks, 20 switches and 4 swaps;
    soak block time p99.9 < 33 %, max < 52 %; every ASIO re-open retries its input build once (a
    BadMode, then fine); unloading Archetype while a second instance runs takes ~4.8 s, the plugin's
    own teardown, the slot dry meanwhile. Share output's soak (`--share=`, ASIO 64, no plugins,
    `--mute=1 --seconds=1500`, into the Scarlett's own Windows endpoint, 2026-10-06): every counter 0
    over 26 minutes at the 40 ms setpoint; at 20 ms it ran short in bursts about 10.8 minutes apart, so
    soak 25 minutes or more. Each soak minute's line carries its block time and the glitch diagnostics
    (`asio_phase_slips`, `asio_late_finishes`, `clipped_blocks`), which no check fails on: they time a
    spike. WASAPI (Scarlett, 2026-10-03 and 10-05): in an open's first ~2.5 s the join can trim (1–2)
    or starve (2–4), in 9 of 85 and 17 of 100 opens after ASIO had run in the process
    (`--cycle=asio64,wasapi`) and 1 of 43 and 2 of 31 with none (`--backend=wasapi --buffer=default
    --cycle=wasapi`), while the soak, every ASIO phase and the swaps stay 0. `engine.xruns` also counts
    a block a starve or a trim damaged and the block after it, so such an open reads two to seven. The
    counter check forgives those in an open's first 3 s (D25) and prints what it forgave (`forgiven in
    …`); a take that overlaps one still fails `loop`. With another app holding the microphone the input
    can run 0.87 % fast, past what the join's controller holds: `join_trims` every 3–6 s, each skipping
    ~25 ms of input (real time or an artefact: unknown). The mechanism: `src-tauri/OPEN-THREADS.md`.
  - `native:engine-loopback` after a change to the engine's alignment, click, grid, device open or
    snapshot. Baseline (Scarlett 2i2 3rd gen, a cable from line out R into input 2, Pro-Q, 48 kHz,
    2026-10-03): 21/21 bars at 64, 128 and 256, no stray sound, no take rejected, A +0.08 ms and B
    +0.17 ms at every size, spread at most 0.003 ms. At 44.1 kHz (2026-09-26 and 27, Pro-Q 3 and MIC):
    A −0.11..+0.09 ms at each size, spread 0.001 ms, drift ≤ 0.001 ms/min, B − 2·A ≤ 0.002 ms, C − A
    and D − B 0.000, and the multiply, free-take, TRIM and FADE phases passed (the FADE beats within
    about 0.01 of ((8 − k)/8)²). Silence other apps' sound on the interface before a run: what they play
    through its line outs reaches the cable and fails the run as stray sound. The offsets nothing
    explains yet: `src-tauri/OPEN-THREADS.md`.
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

### The owner's loopback run (`release:smoke` with the cable, EVO 4)

The owner cables and sets the levels; an agent may run step 2 from WSL (the `pnpm` wrapper) while the
owner watches EVO Control. Last passed 7/7 with an input take for v0.8.8 on 2026-10-26, the owner's
guitar standing in for the cable (step 1); with the cable on 2026-10-16 (8a18e5ad, ASIO 128 at
44.1 kHz).

1. **Before.** Close BleepLoop (installed or dev) and anything playing sound; the screen unlocked. Note
   EVO Control's output ring (it also moves Windows' volume for the device), input 1's gain, the
   monitor mix, and the rate and buffer size. Front headphone jack empty when the cable runs (a headphone mutes the rear
   outputs). The guitar cable: out of the guitar, its other end left in the front instrument jack
   (input 1, which overrides the rear Mic/Line 1); the left monitor's cable out of rear output L and
   the guitar cable in its place. The mic stays in input 2 and the right monitor on output R (the click
   plays on both outputs), the speakers turned down. Monitor mix fully to the computer's playback: at
   50/50 the interface sends the cable's input straight back out. Input 1's gain at 12 o'clock, the
   output ring at 7 dots: the window is narrow, since the take's slot is live and the cable closes a
   loop through the app. 4 dots read flat (2 of 60 px), 6 dots 6 of 60 (under the 15 % bar), 7 dots
   passed (18 of 60 px, meter max 0.78), about 9 ran away.
   The owner's guitar in input 1 stands in for the cable when the cable stays out (v0.8.6 to v0.8.8): the
   headphones stay in, since nothing is cabled at the rear and the owner must hear the click; one string
   plucked on the beat through the one bar passed; a steady strum fills the bar and
   reads as run away (475 of 672 columns), since that bar is the click's few peaks.
2. **Run** (the PC · PowerShell · the repo checkout, clean; or WSL in the same checkout under /mnt/c). The
   exe must be built from the commit checked out; no `BleepLoop-smoke-<sha>` artifact for it yet:
   `gh workflow run build-exe.yml --ref main -f smoke=true`, ~20 min. Check out the commit being
   released, not a branch (a branch checked out in an agent's worktree refuses `git switch`).
   ```powershell
   git fetch; git switch --detach origin/main; git status --short   # must print nothing
   $sha = git rev-parse HEAD
   Remove-Item -Recurse -Force logs/release-smoke/exe -ErrorAction SilentlyContinue
   gh run download -n "BleepLoop-smoke-$sha" -D logs/release-smoke/exe
   pnpm release:smoke --exe=logs/release-smoke/exe/app.exe --fresh --driver=Audient --channel=0
   ```
   `--channel=0` takes input 1. `--fresh` empties the smoke's own profile, so no saved driver or rate
   pick from an earlier interface steers the open (drop it on a rerun); `--driver=Audient` fails the
   run unless the driver that answered has `Audient` in its name, as `Audient USB Audio ASIO Driver` does (else an unrelated driver's pass reads as one).
   No `--owner-profile`: it is only for an exe in the owner's profile, and the runner's exe carries the
   smoke's. Clicks on the outputs; Ctrl+C stops it.
3. **Read.** PASS: `=== release-smoke: PASS: 7/7 checks passed (… s) ===`, its take line `PASS take:
   input take passed: …`; `logs/release-smoke/after-take.png` should show, by eye (the script counts no
   peaks), lane 1 with four separate click peaks, the first the tallest. FAIL: `=== release-smoke:
   FAIL: …`, the failed check saying why. `FAIL take: the input take is flat …`: cable seated, input 1
   not muted, the output ring one dot up, run again. `FAIL take: the input take ran away …`: the take
   reached −1 dBFS or filled the bar, the loop through the cable feeding back or the level too hot
   (lane 1 a solid block): one dot down. `FAIL device: the ASIO driver is "…", not the "Audient" this
   run pinned …`: launch that exe, pick the Audient driver in Audio Settings, close it, run again
   without `--fresh`. Shots and the run's log slice land in `logs/release-smoke/`.
4. **After.** `git switch main` (or the branch you were on). Cable back in the guitar, the left
   monitor back in output L, then the output ring, gain, monitor mix and headphones as noted. The
   script sets no Windows default device or volume, and the app has no code that does (it plays
   through the ASIO driver); the output ring is the interface's own. It writes only the smoke's
   profile (`%LOCALAPPDATA%\com.bleeploop.release-smoke`), never the owner's `com.bleeploop.app`. It
   asks the driver for 128-frame blocks at its own rate (on 2026-10-16 the driver opened at 256 and took
   128): if EVO Control's buffer or rate moved, set it back.

## MIDI latency benchmark (DEV, PC only)

How long a MIDI note takes from the sender to the engine applying it, also while the UI stalls, in
the DEV app (`src-tauri/src/engine_io/midi_bench.rs`). A change to the MIDI input path runs it before
and after; it passes when no note is lost or stuck and the p99 from sender to applied is no worse than
before, inside the stalls too. Every number is frame-based, so none sees the callback's wake jitter:
the onsets of notes recorded back through the loopback cable would (not built). It needs a loopback
MIDI port that WinMM lists (loopMIDI; Windows MIDI Services' own loopbacks expose no WinMM port, and
making one needs its SDK runtime); without one it waits 60 s and logs the output ports it sees.
Baseline (2026-10-09, loopMIDI, Focusrite ASIO 128 at 44.1 kHz, the load set up by `bench-load`, a
note every 25 ms, a 250 ms UI stall every 3 s, 120 s warm-up, two runs of 2500 notes a side): no note
lost, stuck or duplicated, no xrun or discontinuity in any run. Web MIDI (f2b632c4): sender->applied
p99 226–227 ms, 251–252 ms in stalls, 4.0–4.2 ms outside. Native MIDI (30376341): p99 3.1 ms, in
stalls and outside alike; one run had 6 notes one block late. Lock waits (d33f93af, the same load,
24000 notes over 10 minutes, p99 3.1 ms, no xrun, 73 notes one block late): no `settings` or `ends`
wait reached 1 ms, a third of a 128-frame block (max 15 µs on the input path, 42 µs on the feed; the
callback takes neither); a second command ring is reconsidered only if one does. Run from PowerShell
at the PC, in the repo root, with no dev app running:

```powershell
$env:LF_MIDI_BENCH = "loopMIDI;count=2000;stall_every_ms=3000;stall_ms=250;label=before"
node scripts/native-probe.mjs bench-load --asio
```

`bench-load` sets up the jam load unattended and holds it until the benchmark ends (§ Native / Tauri
verification); with `pnpm dev:asio` instead, set it up by hand (an amp-sim or Pro-Q live
in slot 1, three lanes looping, the stage view open). No other controller, and no on-screen or PC keyboard, may play during a run: a note of
the benchmark's played by hand can stand in for a lost one. The run waits for the device, warms up, sends `count` note-ons, writes
`logs/midi-bench-<label>-<unix s>.json`, appends its summary line to `logs/midi-bench.log` and logs it
(`[midi-bench]`). Stop with `pnpm native:kill`; `Remove-Item Env:LF_MIDI_BENCH` before an ordinary
run. Keys (`;`-separated, defaults in brackets): the port name's part first, `interval_ms` [25],
`burst` [1] note-ons per interval, `base` [0] and `notes` [24] (notes 0 to 23, which no player
uses), `count` [2000], `warmup_s` [5], `timeout_ms` [1000], `stall_every_ms` [no stalls] and
`stall_ms` [250], `label` [run], `out` [`logs/`]. A run whose (note, velocity) ids would come back
sooner than twice `timeout_ms` is refused (logged).

- **sent, applied, lost, stuck, dup, stray:** a send no applied `NoteOn` matched within `timeout_ms`
  is lost; a matched note whose next applied record of that note is not a `NoteOff` is stuck (the JSON
  adds `offs_sent` and `offs_applied`); an applied note of the benchmark's no send waited for is a
  duplicate; any other note is a stray, left out.
- **sender->applied** (the ship rule's number), p50, p99, max in ms: from the sender's `Instant` to the
  entry of the callback whose block applied the note (its frame turned into time through the
  callbacks' own stamps): the port, Web MIDI or the native input, the IPC and the wait for a block.
  Output latency is not in it (a constant).
- **arrival->applied (diag):** from where the note reached native code (`engine_send` before the
  switch, the native input after): a diagnostic, never a before/after comparison.
- **in stalls / outside:** split by whether the send fell inside one of the frontend's stalls. The
  page measures its clock's offset to Rust's over 20 round trips first; the JSON gives the offset's
  uncertainty (`stalls.clock_uncertainty_ms`, half the shortest round trip), which the windows'
  edges carry.
- **late, max_blocks:** notes the first callback after their arrival did not apply (a full command
  table, a lock wait).
- **callbacks, xruns, engine_xruns, gaps, lock_misses, discontinuities:** the device during the run; a
  discontinuity is a block that did not start where the last ended, or the device stopping. The JSON
  adds `stamps_lost`, `record_refused` and `engine_rebuilds`: any of them above 0 makes the run's
  numbers incomplete. Taking the engine's note record can cost one `lock_misses` in the warm-up.
- **Lock waits** (`settings` and `ends`, by taker: `send` the input path, `feed`, `other`): count,
  total, max and a histogram, in the JSON and the log. `$env:LF_LOCK_WAITS = "60"` alone logs them every
  60 s (`[lock-waits]`, totals since the start).
