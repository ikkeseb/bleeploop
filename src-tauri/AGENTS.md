# src-tauri/: native host briefing (Windows / Rust)

Everything Rust/native lives here: the native audio engine (`crates/lf-engine`) and its device side
(`src/engine_io`), the CLAP/VST3/VST2 plugin host, the ASIO tier, plugin editor windows. The root
`AGENTS.md` routes here: read this before any work in this subtree (`CLAUDE.md` beside it is a
one-line adapter). It holds the router-level gotchas and operational bits; the engine's rules live in
its two briefings (the crate doc of `crates/lf-engine/src/lib.rs`, the module doc of
`src/engine_io/mod.rs`). The gate is `pnpm rust:check` (Windows, also from WSL); `tauri dev`, the
`native:*` probes and `native:kill` are in `docs/VERIFY.md` § Native / Tauri verification. Only the
by-ear gates need a person at the PC. Both `cargo check --features asio` and no-asio must stay green
(CI runs the no-asio half).

## Thread & ownership map

Who runs where and what each thread owns. The rules are in bold below the table.

| Thread | Owns | Talks to others through |
|---|---|---|
| **UI (WebView2 / main)** | The Tauri window, every WebView2 COM call (`with_webview`: the Web MIDI permission auto-grant, `lib.rs`), the plugin folder dialog (modal over the main window, `src/host/folders.rs`), the close guard (`lf://close-requested`), and the synchronous commands: `engine_send` (so batches keep IPC order), `plugin_asio_status`, `plugin_asio_device_info`, `lib.rs`'s own | IPC in; window events out (`plugin:param-changed`, `plugin:params-changed`, `plugin:editor-closed`) |
| **Command threads** (Tauri async runtime) | Nothing long-lived. Every other command in `host/commands.rs` and `engine_io/mode.rs` is `async fn`; blocking engine work (an open waits up to 15 s) goes to `spawn_blocking` | Device requests to the device owner; plugin requests to a slot owner (`owner_request_5s`, ≤5 s); a load spawns the owner and waits ≤15 s on its rendezvous (a result that arrives later is the owner's to undo); device lists enumerate cpal directly (no stream opens) |
| **Device owner** (`lf-engine-owner`, `engine_io/owner.rs`) | Every device transition, one at a time; the cpal streams (Send: ownership is for ordering); building engines; the engine lock, only while no stream runs | A request channel with one-shot replies; polls what the callbacks latched and logs the glitch counters |
| **Device callbacks** (cpal driver threads) | Nothing: ASIO runs input then output in one bufferSwitch; WASAPI's output callback is the clock and its input callback feeds the join pipe. Each promotes itself to MMCSS Pro Audio on first entry and runs with flush-to-zero on (`engine_io/fpu.rs`) | `try_lock` on the engine (a miss plays silence and counts); atomics and rings; faults latch for the owner |
| **Share output** (a cpal WASAPI callback, `engine_io/share.rs`) | The mirror endpoint's stream, while ASIO plays | Pulls the post-limiter master from its pipe; never takes the engine lock |
| **Feed** (`lf-engine-feed`, `engine_io/feed.rs`) | The only reader of the engine's event ring (but for a rebuild, which drains the replaced engine's: `swap_engine`) and the device events; the mirror of lanes and transport a reload resyncs from | ~60 frames/s over a Tauri `Channel`; never PCM. Held (`FeedThread::hold`, an RAII guard) while the plugin folder dialog is open, since the UI thread reads no frame then: it keeps ticking, so the event ring is drained and the mirror stays true, sends nothing, and the close resyncs the page with one `reset` frame |
| **Plugin owner, per slot** (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}` / `lf-vst2-engine-{slot}`, `host/engine_slot.rs`) | The `!Send` plugin instance (clack main thread / VST3 component + controller / VST2 effect, with its `effStartProcess` and `effStopProcess`), its editor host window and Win32 pump, its tone saves, restarts and re-activation after an eviction, the ordered teardown | `OwnerRequest`s (polled every 20 ms, `OWNER_POLL`); the unit enters and leaves the engine through its `SlotHost`; params through the slot's event ring; the unit's fault bits, reported once per load |
| **Shutdown** (`lf-engine-shutdown`, on exit, and before the updater's installer: `update.rs`) | Stops the feed, saves every tone, unloads the plugins while the device plays, closes the device | Bounded at 8 s (`SHUTDOWN_WAIT`): a stuck plugin is left to process exit |
| **ASIO probe** (`lf-asio-probe`, `asio_startup.rs`) | One driver-resolving probe at a time, requested by the frontend after the UI is up (a retry after a failure and a driver switch probe again) | Its status report |
| **Scan children** (`app.exe --scan-one <path>`) | One process per bundle in a kill-on-close Job Object, 20 s timeout; two reader threads per child drain stdout/stderr with caps | Descriptor JSON |
| **Plugin GUI threads** | A floating CLAP editor runs the plugin's own window thread and only sets flags (`HostGuiImpl` → `EditorClosed`) the owner acks; a hosted editor embeds into the owner's host window and is pumped there. VST3 `performEdit`/`restartComponent` (ONE component handler per load, set at load) touch only the event ring, the window event and the `RestartFlags` atom the owner drains each turn; CLAP `params.rescan` sets a flag the owner drains; a VST2 plugin's host callback, from any thread, sets latches in its `HostContext` the owner drains each turn | Flags / events |
| **Native MIDI** (`lf-midi-ports`, `engine_io/midi`) | Built and tested, **never started by the app**: MIDI arrives through the WebView's Web MIDI, and WinMM input ports are exclusive, so the two cannot share a controller | none |

- **Only a slot's owner thread touches its plugin instance and editor.** Commands reach it through
  `OwnerRequest`; the audio thread only through the unit it installed (rings + atomics). Never
  `run_on_main_thread` a plugin call.
- **Only the UI thread makes WebView2 COM calls** (`with_webview`); a non-UI caller recovers the
  result over a channel. The plugin folder dialog follows it: `plugin_folder_add` shows it through
  `run_on_main_thread`, owned by the main window, and holds no lock while it is open. Shown from a
  command thread with that owner it would be the editor-hang deadlock below.
- **The audio path never logs, locks (beyond `try_lock`), allocates or waits** (invariant 5): the
  engine callback, the Share callback and every plugin `process`. A unit latches fault bits
  (`FAULT_START`/`FAULT_PROCESS`/`FAULT_PARAM`/`FAULT_EVENTS`) and the owner reports each once per load; add a new
  RT failure path to that latch, not `log::*`. DEV builds count RT allocations (`host/rt_alloc.rs`).
- **Plugin lifecycle runs on its owner with the slot bypassed** (invariant 4): load, activate, a
  plugin-requested restart (CLAP `request_restart`, VST3 `restartComponent`, VST2
  `audioMasterIOChanged`) and teardown; an FX slot
  passes dry, an instrument slot is silent, loops and click never wait.
- **Unload order is load-bearing:** `running=false` → `OwnerRequest::Wake` → join the owner, which
  removes the unit from the engine (crossfade to bypass, `stop` on the audio thread; ≤2 s,
  `REMOVE_TIMEOUT`) → deactivate → terminate → the module unloads LAST. A unit the engine does not hand
  back leaves the plugin loaded (leaked), never unloaded under a running processor (VST2's own order:
  § Plugin hosting). Both halves log
  their timing (`engine slot N VST3 teardown: remove=… release+deactivate+terminate+module=… ms`,
  `owner joined in … ms`), in release too; a plugin's own release can take seconds (Archetype: ~4.8 s,
  the slot dry meanwhile).

## Operational bits (recurring)

- **`crates/lf-engine` is the pure engine** (briefing: its `src/lib.rs`); `src/engine_io` is its
  device side (briefing: its `mod.rs`); the plugin units and owners are `host/engine_slot.rs`,
  `host/clap_engine.rs`, `host/vst3_engine.rs`, `src/host/vst2_engine.rs`, which
  `engine_io/plugins.rs` routes the `plugin_*` commands to. `tauri dev` watches all of `src-tauri/`,
  so an engine edit relaunches a running dev app.
- **An engine ASIO open opens the driver at another block size first** (`engine_io/cpal_driver.rs`,
  ~500 ms): opened again at the size it last ran, the rig's Focusrite driver lands two periods late.
- **An ASIO driver is never asked for a buffer outside the range it reported to the probe** (cpal
  refuses it): a request outside opens at `engine_io::transition::asio_block`'s pick, and a one-size
  driver (set in its own control panel) gets no preopen. A size inside the range that the driver still
  refuses (sizes in steps, `min + k·step`, which the probe cannot see) opens once more at the driver's
  own size, with no preopen; `DeviceStatus.block` is the size the callbacks deliver.
- **Parallel worktrees must not share a target dir.** Each checkout's default `src-tauri/target` is
  already its own: never point two worktrees at one `CARGO_TARGET_DIR`. Sharing one, every worktree
  links the same `app_lib-<hash>` test binary and cargo judges path crates fresh by mtime, so one
  worktree can run another's build of `app` or `lf-engine`. From WSL, Windows cargo does not see a
  `CARGO_TARGET_DIR` exported in bash (WSLENV does not pass it).
- **ASIO is a cargo OPT-IN feature** carried by the npm scripts (`pnpm dev:asio`, `pnpm build:app`);
  a plain `cargo build` must work without the LLVM/ASIO SDK, and `tauri dev` forces
  `--no-default-features` anyway. `[profile.dev.package."*"]` and lf-engine build at opt-level 3;
  `app`/`app_lib` stay opt-level 0.
- **ASIO SDK env** (`LIBCLANG_PATH`, `CPAL_ASIO_DIR`) is set PERMANENTLY in the User-scope env on
  the dev PC: a fresh shell inherits it, no inline setting needed (`pnpm dev:asio` just works).
  `CPAL_ASIO_DIR` must hold an EXTRACTED SDK with `common/` and `host/pc/` directly under it; `asio-sys`
  only rebuilds when its fingerprint changes (a `cargo update`, a crate bump), so a green
  `cargo check --features asio` can be a stale `target/` cache over a missing SDK; `pnpm rust:check`
  confirms the SDK directory before its asio step. cpal stays pinned at `=0.18.1` (why: the
  engine_io briefing).
- **PROD-EXE recipe:** raw `cargo build --release` = a DEV-mode binary (wants devUrl). The real exe
  = `pnpm build:app`; launch DIRECTLY (`Start-Process app.exe`), never via stdout-redirect.
- **Release IPC surface:** `capabilities/default.json` grants only event listen/unlisten. `diag` is
  registered only under `debug_assertions`; keep the handler cfg and frontend `import.meta.env.DEV`
  surface in lockstep. Help's `app_log_dir` / `app_open_log_dir` (`lib.rs`) and the updater's
  `app_update_check` / `app_update_install` (`update.rs`; the release channel is `plugins.updater` in
  `tauri.conf.json`) ship in release and take nothing from the WebView; so do tone recall's
  `plugin_tone_take` / `plugin_tone_import` (raw bytes both ways) and `plugin_tone_forget`.
  `plugin_folder_add` takes its path from the native dialog alone; `plugin_folder_remove` takes one
  only to match a stored entry.
- **Sample-rate pick:** 44.1/48 kHz or the device's own (Audio Settings); the rules,
  and why WASAPI keeps its endpoint's rate on cpal 0.18.1, live in the engine_io briefing
  (`src/engine_io/mod.rs` § Rules).
- **Editor-hang Win32 gotcha (recurring):** a host window Win32-OWNED across threads deadlocks on
  close (sync cross-thread activation `SendMessage` vs a stopped pump). Fix lives in
  `host/editor_window.rs`: owner-LESS window + `drain_after_editor_teardown()` +
  `show_host_window_front()` one-shot `HWND_TOP` (no `WS_EX_TOPMOST`), so an editor that drops
  behind after a click into BleepLoop is intended.
- **Timed Win32 waits round up to the timer tick Windows grants the process**, and that tick can be
  15.6 ms while the global resolution reads 1 ms. Bound a wait loop by a deadline, never a round count
  (`drain_after_editor_teardown`). A plugin's own waits stretch the same way; the measured numbers sit
  with the native baselines in `docs/VERIFY.md`.
- **Release logging:** `tauri-plugin-log` registers UNCONDITIONALLY → Stdout (the dev grep
  convention) + a rotated file at `%LOCALAPPDATA%\com.bleeploop.app\logs\bleeploop.log` (2 MB,
  KeepAll). The Rust panic hook chains the default hook and logs location+payload.
- **The CSP (`tauri.conf.json` `security.csp`) applies to BUILT apps only.** `tauri dev` serves from
  vite and is not covered, so a new asset origin, a CDN font or an `eval` breaks in release alone.
  Violations reach the release log as `[csp]` lines (`src/platform/logging.ts`); after adding a new
  kind of resource, run `pnpm build:app` once and grep that log. `style-src` is exempt from Tauri's
  nonce injection on purpose (a nonce would void `'unsafe-inline'`, which Solid's template styles need).

## Native-host verify ops

The out-of-process plugin scan is testable WITHOUT the full app: `cargo build`, then
`target/debug/app.exe --scan-one "<plugin path>"` prints the descriptor JSON and exits (the
`--scan-one` dispatch runs before Tauri starts). The scan walks its built-in roots, then the
player's own folders (plugin-folders.json beside the cache: user data, so an unreadable one is an
error to the folder commands, is never overwritten, and the scan logs it and walks the built-in
roots alone; `src/host/folders.rs`), one scan at a time in the process. It spawns one child per `.clap`/`.vst3`
and per `.dll` the VST2 pre-filter lets through (`src/host/pe.rs` reads each one's headers and export
names without loading it: a 64-bit DLL that names `VSTPluginMain` or `main` gets a child, a 32-bit
one is reported as unsupported with no child, any other DLL is ignored, and one it cannot read goes to
the child), for crash + hang isolation (**20s per-child timeout**: a heavy/licensed VST3 like Neural DSP can hang
on load in the headless child), but only for bundles whose binary size/mtime changed since the
cache (plugin-scan.json beside the release log dir under %LOCALAPPDATA%) last saw them (a launch spawns
nothing; a remembered failure is retried only by the picker's rescan button = `plugin_scan`
`force`). Delete that file to force a cold scan from outside the app. Pipe retention is capped at 1 MiB
stdout / 64 KiB stderr, and each child process tree sits in a kill-on-close Job Object. DEV probes on
the debug `app.exe`, each exiting before Tauri starts: `--probe-engine` (`pnpm native:engine`, the
device side on the rig: `engine_io/probe.rs`), `--probe-engine-spike` and `--probe-share` (`pnpm
native:spike`: the premise numbers in `docs/ARCHITECTURE.md` § Measured premise).
A dev start that names a build path from an old checkout location (a moved or renamed clone) →
`rm -rf src-tauri/target/debug/build`.
(Driving and grepping a running `tauri dev`, and stopping it: `docs/VERIFY.md`.)

## Plugin hosting (CLAP + VST3 + VST2)

All in `src-tauri/src/host/`. The known-fragile area: read this whole section before any plugin or
plugin-GUI work.
- **Every WebView document gets a `frontendEpoch` from `host_init`** (one atomic step, never 0). A
  slot is reserved for the document that asked before foreign setup; a reload cancels the
  reservation, and a load that finishes for a replaced document unloads again (`engine_io/plugins.rs`).
- **The `!Send` instance (clack `PluginInstance`, the VST3 component, the VST2 effect) lives on its
  owner thread**, NOT `tauri::State`; only the `Send` unit (`ClapUnit`, `Vst3Unit`, `Vst2Unit`: the
  processor with its buffers and lists) enters the engine, which never drops one (a drop frees memory and calls into the DLL).
- **Surge param ids are hash-like, NOT 0-based** (`first=825615485`); sending an unknown id CRASHES the
  plugin → always enumerate via `listParams`, never invent an id (`plugin_set_param` refuses an id the
  plugin never listed).
- Test plugin **Surge XT** (`winget install SurgeSynth.SurgeXT` → `…\CLAP\Surge Synth Team\`, + Surge XT
  Effects); scan walks `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`, `CLAP_PATH`,
  the same three for VST3, then VST2's (`%ProgramFiles%\VSTPlugins`, `%ProgramFiles%\Steinberg\VSTPlugins`,
  `%COMMONPROGRAMFILES%\VST2`, `%COMMONPROGRAMFILES%\Steinberg\VST2`, the registry's `VSTPluginsPath`,
  `VST_PATH`), then the folders the player added in Audio Settings (`src/host/folders.rs`). A
  built-in root yields its own format, a player's folder every format. A 32-bit VST2 and a VST2 shell
  (several plugins in one file) are listed as unsupported, with why, and never hosted (a load
  refuses a shell too, by its category). A plugin reached twice keeps the
  FIRST spelling met, and no path is canonicalised on its way into a descriptor: the tone store and
  the frontend hash `(format, path, id)` byte for byte, so a respelled path orphans its saved tone.
  Loader is `clack_host::entry::PluginEntry::load` (unsafe), NOT `PluginBundle`.
- **Plugin editors embed into a host-created top-level Win32 window** (`CreateWindowExW`, OWNER-LESS
  as the editor-hang gotcha above says, NOT reparented into the WebView2 surface), except a CLAP
  plugin that offers its own floating window, which gets that first (`host/clap.rs`, "Preferred path"). An owner pumps its thread's Win32 messages every
  turn, editor or not: a JUCE plugin (Neural DSP) runs its message thread there, and unpumped, a
  host-set parameter never reached its saved state (measured with `pnpm native:tone-recall`, Archetype
  Petrucci). Each pump call is bounded (64 messages or 2 ms, `editor_window::pump_thread_messages`), so
  a plugin whose messages repost themselves cannot keep an owner from its requests, saves or unload.
  GUI calls go via the owner channel, NOT `run_on_main_thread`.
- **Editor size is the plugin's, measured not computed:** `editor_window::set_client_size` sizes the
  CLIENT area by measuring the real frame (DPI-correct), at creation and on every plugin-initiated
  resize: VST3 `IPlugFrame::resizeView` (then `onSize` with the granted size), hosted-CLAP
  `request_resize` and VST2 `audioMasterSizeWindow` all land there. Never answer a resize `kResultOk`/`Ok` without resizing; the view
  lays out for the size you confirm. The one exception is a CLAP `request_resize` from a thread other
  than the window's (a cross-thread `SetWindowPos` would wait for the owner): it is acknowledged and
  queued, the owner applies the latest on its turn or reverts the plugin with `set_size`, and a resize
  on the window's own thread drops what was queued before it. VST2 does the same with its answer:
  on the owner thread the window is resized at once and the answer is 1 only for the exact size
  (a clamped window goes back to the size it had), from any other thread the size is latched and the
  answer is 0. Fixtures: `host/vst3_resize_fixture.rs`, `clap::resize_tests`,
  `src/host/vst2_engine_tests.rs`.
- **Crate `vst3` 0.3.0** (coupler-rs; only dep `com-scrape-types`, no `windows`/`windows-core` conflict).
  `ComPtr<IAudioProcessor>` is already `Send+Sync`. Source-verify against the crate source
  (`~/.cargo/registry/src/index.crates.io-*/vst3-0.3.0/src/bindings.rs`), NOT web docs; methods are
  **camelCase**; `kResultTrue == kResultOk == 0` (compare `== kResultTrue`, never as a bool).
- **VST3 teardown order** (`vst3_engine.rs`: `teardown`, `Vst3Plugin`'s drop): the unit leaves the
  engine (`setProcessing(0)` on the audio thread) → a separated controller disconnects and terminates →
  `setActive(0)`, only on an active component (a failed restart leaves it inactive) → `terminate` →
  the COM objects drop → `Vst3Module` LAST. Its RAII drop pairs successful `InitDll` with `ExitDll`,
  then calls `FreeLibrary` (any plugin-DLL `ComPtr` must drop first: its vtbl lives in the module).
- **Surge XT VST3 is SEPARATED-component** (`component.cast::<IEditController>()` is None) → `obtain_controller`
  (`getControllerClassId`→`createInstance`→`initialize`); a JUCE separated controller's `createView` returns
  null until it gets the in-process `AudioProcessor` pointer over a connection-point `notify(IMessage)` (host
  `LfMessage`/`LfAttributeList` + `IConnectionPoint` cross-connect).
- Scanner handles BOTH VST3 forms: single-file `.vst3` AND folder bundle `Contents/x86_64-win/<inner>.vst3`
  (the walk yields the outer path and never enters a bundle);
  descriptor `id` = the class TUID hex. `host/vst3.rs` stays a child mod OF `host/clap.rs` (`#[path]` decl, so
  `vst3::Steinberg` imports don't collide with clack and `super::` keeps meaning); the engine units and
  owners are child mods the same way.
- **VST3 buses:** query `getBusInfo` AFTER `setBusArrangements`, size buffers to the reported channel count,
  don't assume the requested arrangement (same crash class as the Surge hash-param id). Guard zero-input
  plugins (synths). `activate_component` (`host/vst3.rs`) is the ONE owner of that sequence: load and
  every restart run it, and each install takes the `Activation` it produced, so a `kIoChanged` that
  changes a count resizes the unit's buffers; the unit's kind (effect with an input bus, instrument
  without) follows each activation.
- **Host-set VST3 params go to BOTH halves:** the slot's event ring feeds the processor,
  `OwnerRequest::SetParamNormalized` feeds the edit controller on the owner thread
  (`EngineSlotHandle::set_param` is the one entry; the controller hears only what the ring took).
  The controller is what the plugin GUI shows and what raises a controller-decided `restartComponent`
  (FabFilter latency modes). Without the mirror, no drawer change can ever restart a plugin again.
  `performEdit` (GUI → host) is never mirrored back. A 30-plugin restart survey backs this: FabFilter
  raises `kLatencyChanged`, Neural DSP and Surge never do.
- **VST2 (64-bit only; `src/host/vst2_abi.rs` declares the interface, `host/vst2.rs` the loader and the
  host callback, `src/host/vst2_engine.rs` the unit and the owner).** What must not be got wrong:
  - **The host callback is called from ANY thread** (the plugin's GUI thread, a thread of its own, the
    audio thread from inside `setParameter` or a process call), so it is RT-safe by construction: it
    only stores into its instance's `HostContext` (atomics and the automation latch) and reads
    const-initialised thread-locals. The owner drains the latches each turn. A reported parameter
    change reaches the UI with the value the plugin gave; it is never fetched with `getParameter` and
    never sent back with `setParameter`. Only on the owner thread, outside a process call, does
    `audioMasterIdle` pump messages and `audioMasterSizeWindow` touch the editor's window.
  - **Both pointer arrays of a process call ALWAYS hold 64 entries** (the bound `validate` accepts):
    a row per declared pin, every other entry at a scratch row, so a plugin that grows its pin count
    never reads or writes past an array. After `audioMasterIOChanged` the unit makes NO plugin call
    (`HostContext::halted`) until the owner has cycled the plugin and rebuilt the rows. The engine's
    note-offs are discarded with the rest, so the unit keeps the keys it sent and releases them in
    the first slice after the cycle. A resume consumes what the plugin reported from inside it
    BEFORE it reads the layout: a report that lands after the read stays latched, and the unit goes
    in halted until the owner's next turn cycles it.
  - **`effStartProcess`/`effStopProcess` and `effMainsChanged` run on the OWNER**, never the audio
    thread (unlike VST3's `setProcessing`); the unit's `stop` calls nothing. The audio thread sends
    one dispatcher opcode, `effProcessEvents`. A dispatcher's return is its opcode's own: 0 from
    `effOpen`, `effMainsChanged`, `effStartProcess`, `effStopProcess` or `effSetChunk` is no failure.
  - **Teardown:** the unit leaves the engine → `effStopProcess` → `effMainsChanged(0)` → the editor
    closes → the tone is saved if it changed → `effClose` → the `HostContext` → the module LAST.
    **After `effClose` the `AEffect` pointer is dead:** never read it, never clear its `resvd1`.
  - **A unit the engine does not hand back leaks TOGETHER with its effect, its `HostContext` and its
    module** (`mem::forget`): the unit may still be inside the plugin, and the plugin still calls the
    host. An effect `open_effect` refuses was never called, so it is never closed: its module and
    context stay loaded too.
  - The slot is mono: the input feeds the first two inputs, the output is the mean of the FIRST
    stereo pair (never of all pins: a multi-output instrument's auxiliaries would dilute it).
    Parameter ids are indices `0..numParams`, values 0..1.
- **Tone recall (briefing: `host/tone.rs`):** a load restores the plugin's stored state before it
  activates (CLAP `state.load`; VST3 `IComponent::setState`, then the controller's
  `setComponentState` and `setState`, through the host `MemStream` in `vst3.rs`; VST2 `effSetChunk`
  with its bank chunk and no program selected after it, or, for a plugin without chunks,
  `effSetProgram` first and then every parameter), and the owner saves it
  on its own thread. No request pushes state into a running plugin. The VST3 load creates the
  controller and sets its handler BEFORE activation (the SDK host's order). A plugin that refuses its
  tone is discarded and created again before it activates (it may have taken half the state; a VST2
  tone is checked against the plugin before its first call, and its calls answer no verdict, so that
  instance is never discarded); a
  session import goes through its tone's write lock in the store (`ToneStore::import`), never an owner
  request, and its bytes reach only the reload's load that passes the import's reload token
  (`ToneHandoff`). An owner's poll never waits on a lock held across disk I/O; its save of a tone
  waits on that tone's own in-flight import (§ Open threads). Why a save skips the store: `tone.rs`.

## ASIO tier

- **cpal ASIO = ONE driver per device, its metadata resolved once per driver pick** (each run and
  preopen finds the driver again by name as a new cpal device, `engine_io/cpal_driver.rs` `find_asio`): once a stream holds it, cpal
  can't re-resolve the device or re-query configs, so the duplex Device + configs are cached
  (`audio_output::resolve_asio_cache` behind the `asio_startup.rs` coordinator; a static `Arc`, as
  `cpal::Device` is Send+Sync). Only a driver switch (`plugin_asio_switch`) replaces it, on the device
  owner, which closes its ASIO run first and reopens it after, so no open or recovery takes the cache
  midway. Picking a driver by name loads each driver cpal lists before it once (cpal 0.18.1 has no
  by-name constructor), an accepted cost. ASIO uses the cached driver (the saved pick, else automatic:
  the default output's), not the WASAPI device ids.
- **Never call the resolver from `run()`:** it loads the driver DLL in-process, and a broken driver
  would hang or crash the app before any window exists; the frontend requests the probe after the UI
  is up (`asio_startup.rs` owns the rules). cpal's device enumeration loads every driver it names;
  `plugin_asio_drivers` (asio-sys' registry list) loads none.
- **ASIO timestamps:** retain cpal's per-package `overflow-checks=false` in Cargo.toml for its wrapped
  epoch conversion. Every latency is a delta within ONE stream (input: callback − capture; output:
  playback − callback), never the absolute epoch and never across streams (each has its own time base).
- **Test-rig gotcha:** the ASIO probe is a frontend-requested command, so a `tauri dev` log shows
  `[asio] probe starting` → `cached ASIO "…"` → `[asio] probe result … Ready` once the UI is up: before
  the engine device opens and `host_init` runs when ASIO is the saved pick (`src/ui/state/audio-devices.ts`,
  `initAudioDeviceSettings`), never from `run()`; `pnpm dev:asio -- -- --disable-asio` shows `DisabledByFlag` and no
  probe. Read `plugin_asio_device_info` for the cached metadata and `engine_status` (or the
  `[engine_io]` open lines) for the backend that opened.

## Open threads (no gate)

- Tone recall: an owner's save of plugin P in slot S waits on that tone's own in-flight store write (an
  import of P into S under its write lock). A disk write that hangs therefore stalls that owner, and an
  unload joins it without a timeout (the app's exit is bounded). Other tones' writes never block it; a
  store worker doing the file I/O would remove the wait.
- `clap_engine.rs`, `vst3_engine.rs` and `src/host/vst2_engine.rs` each carry the whole owner
  choreography (load, restart, eviction, teardown): one shared owner would keep B1/B11 from returning. Not built.
- VST2 host gaps (read from source, none reproduced; the only real plugins tried are ReaJS and
  ReaStream, which load, open their editors and swap, and whose `native:tone-recall` fails because
  neither has a parameter that holds a value):
  a restart whose unit the engine does not hand back within 2 s leaves the unit silent until the
  plugin's next `audioMasterIOChanged`; `effSetChunk` answers no verdict, so a chunk the plugin
  could not use is reported as restored;
  `effIdle` is sent once per `audioMasterNeedIdle`, not until it answers 0; a program is selected
  without `effBeginSetProgram`/`effEndSetProgram`; the plugin gets notes only (no CC, pitch bend
  or SysEx) and a time info without transport; `FAULT_PROCESS` has no VST2 site (its process call
  answers nothing).
- A unit whose restart failed stays parked, bypassed, until the plugin's next restart request; a device
  change does not retry it.
- An output callback that misses the engine lock reads as a duplex fault. Two ASIO overloads each
  reported to a different stream alone count once in `xruns` (`callback::Run::stream_error`).
- The panic hook (`lib.rs`) allocates and logs on whatever thread panicked, the audio thread included.
- The dry signal steps without a ramp on an instrument installed into a live slot.
- Two live slots on the same capture channel sum it (+6 dB).
- An ASIO period the driver drops without its overload report is not flagged as damage (input and
  output stay in step; the take is spliced there). `asio_phase_slips` counts it in the log only (after a run's first
  second; two in consecutive half-seconds count once), beside a lasting move of the driver's phase, which the host clock cannot tell from a drop (asio-sys drops
  the driver's sample position; `callback::PhaseSlips`). A single late wake is no signal: on the rig's
  USB driver at 64 frames, wakes more than a period behind the best phase came ~180 times a second
  with every fault counter 0 (2026-10-07).
- A rate or buffer change made in the ASIO driver's own control panel while the app runs (read from
  source 2026-10-07, never run on a device; Audio Settings sends a fixed-size driver's users there): a
  reset, resync or rate change reaches the run as cpal's `StreamInvalidated`, which the owner handles
  as a lost device, without the fade-out an in-app switch gets, and the recovery reopens the saved
  request (`transition::fallbacks`), likely putting the app's rate and buffer back over the panel's. A
  `kAsioBufferSizeChange`, which cpal 0.18.1 accepts with no error, would leave the run on the new
  block size with its latency alignment frozen at the old one (`callback::LATENCY_SAMPLES`), a take
  possibly off the click, and the buffer select stale. Which messages the Focusrite driver sends is
  unknown. A fix keeps the loops (a rate the owner did not confirm discards them: `Owner::open`) and
  shows what runs, rather than adopting the driver's settings blindly. Next check, on the rig: change
  the buffer in the driver's panel, then the rate, each while loops play, and record a take after
  each: the log, the block size the callbacks get (the status reads it only at the open), and whether
  the take lands on the click. Under WASAPI the buffer select lists every
  size and the run ignores it (`cpal_driver::resolve_wasapi`).
- A punch-out inside a take's last quarter-beat commits the whole bars before it, where a stop there
  rounds up (owner's call).
- The feed's reset mirror carries no count: a WebView reload during a count-in shows no numeral until
  the next count beat arrives, and after the last one a later take reads WAITING FOR DOWNBEAT until
  its take starts.
- The no-device removal path (a 1-frame process and `stop` on the plugin owner's thread) has no test
  with a real unit, and the CLAP restart fixture's thread check would flag it.
- Native MIDI (never started): a pedal binding's port occurrence is recounted on every hot-plug, so two
  same-named controllers can swap bindings; a port back within one 1 s poll keeps a dead connection.
- Archetype Plini (VST3) once stalled 4–14 s in 5 of 20 unloads, editor closed, and has not repeated
  since (cause unknown). The VST3 teardown and the unload log per-step timing in release too, so the
  next occurrence names its step.
- Crackle, the owner's two reports, measured on the rig 2026-10-06 with silent `--probe-engine`
  soaks (`docs/VERIFY.md`, `native:engine`). **In a call** (ASIO 64, a call on the same interface
  through Windows audio, every 5–10 minutes, heard by the far end too; reportedly at 256 as well):
  the release log of that day shows Share output mirroring into the Scarlett's own Windows endpoint.
  Soaked that way on an idle machine, the mirror ran short in two bursts 10 min 48 s apart (15 events
  of two or three short 10 ms callbacks each: 37 `share_starves`, 2 trims in 25 minutes), a gap each
  for whoever hears the mirror, while the ASIO callbacks counted nothing. The mirror's setpoint went
  from 20 to 40 ms (`engine_io/share.rs`), and the same soak then counted nothing in 26 minutes (one
  run, other work on the machine at times). Whether this was the crackle the owner heard is the
  owner's ear's to say (`STATUS.md` § Not heard yet). Into the virtual cable at 256 the owner's log
  has no starve in a three-hour session. The three or four at some mirror opens were cpal's empty
  start: the endpoint's first pull asks for its whole buffer, which ran the ring short; that first
  pull now plays silence (`share.rs`, a test pins it; not yet soaked on the rig).
  **While agents build** (heavy, twice, once with BleepLoop closed): not reproduced, no ear was at
  the PC, but narrowed. ASIO 64 counted nothing in 20 minutes of soaks under 16 and 32 busy threads
  at normal priority, clean `cargo check`s at below-normal priority and a WSL-side load, nor in 70
  minutes without a load of this session's making: a build breaks neither the engine nor the driver's
  ASIO side. What a build does to another app follows its priority: a normal-priority thread doing
  1 ms of work every 10 ms lost a third of its turns to 16 busy normal-priority threads and ran up
  to 29 ms late beside a normal-priority `cargo check`; beside the same check at below-normal
  priority and beside a WSL load it ran at most 6 ms late and lost none. Windows processes started
  from the rig PC's logon tmux session run at below-normal priority (its scheduled task's), builds
  included; a WSL session opened from a terminal window has a normal-priority host (its children
  were not sampled). Which kind built when it crackled, and what was playing, is unknown. Next check:
  the owner's ear on music through the interface during a `pnpm rust:check` from a normal-priority
  shell, then from the tmux session. A full `pnpm rust:check` at normal priority beside a silent
  ASIO 64 soak (2026-10-07, 15 min, `cargo test` 479 s of it): no counter moved, but one block took
  151 % of its period, where idle soaks peak at 27–74 %; when is unknown (the probe's soak lines now
  carry each minute's block time). Not measured: memory pressure. A tester hears crackle at times too, less marked with a gate on his
  interface, which puts some of it on the input side there; on WASAPI the join's bursts (below)
  would sound like that (his backend is unknown).
- Plugin-host gaps a source review found (2026-10-02; read from source, none reproduced), ranked by
  exposure on the owner's plugins. First: VST3 omits trailing inactive aux buses (the SDK's
  `activateBus` rule permits it) and passes short `setBusArrangements` arrays whose result is read
  back, not checked; a tone saved by an export or an unload before the next block has consumed an
  accepted parameter edit loses that edit; two concurrent edits can leave a VST3's processor and controller at
  different values. Then: two slots loading one VST3 DLL run its init unserialized; a failed VST3 editor
  attach drops the frame before the view; a CLAP restart can deliver a removed parameter id; a
  non-discardable VST3 module is unloaded. Lower: kReloadComponent only reactivates; a MIDI-only CLAP
  note port gets CLAP notes; CLAP editor edits never reach the drawer; an editor open that succeeds at
  its timeout stays open; CLAP visibility and connection-loss callbacks do nothing; the scanner caches
  factories that forbid it.
- After a WASAPI open the join can trim or starve within ~2.5 s, and a take that overlaps it is
  rejected: 9 of 85 opens after ASIO had run in the process, 1 of 43 without (`docs/VERIFY.md`,
  `native:engine`'s baseline). Traced (the join trace in `engine_io/callback.rs`): once the pipe has
  primed, the capture side delivers one packet more than its time (about three opens in four, 0.2–1.1 s
  in) and a render callback comes a period late without asking for more (four opens in five after ASIO,
  one in ten without), so pulls find the ring up to two 10 ms periods over its 25 ms setpoint, just
  under the trim line at twice the setpoint, for the ~10 s the controller takes to drain it. From there
  a trim takes only a push and a pull swapping order on one tick, a render callback 20–30 ms late that
  asks for two periods (the trim is judged on the fill before the pull: the ring is cut to the setpoint
  and the pull leaves it a period short), or one more late render callback; a starve follows a trim
  when a capture wake is late, or comes at the start when capture delivers its packets two at a time.
  Empty plugin slots, `--mute` and a 3 s pause after the ASIO close do not remove it. Why the endpoints
  start this way is unknown (the callbacks' timing was measured, not the device); Signal Desktop and
  Focusrite Notifier ran throughout. By the pipe's own sizing rule (`PipeConfig::setpoint`) these pushes
  and pulls ask for 33–43 ms. The trims stay (D25): a trim rule that tolerated these bursts, replayed
  through the pipe against nine traced opens, removed all 8 trims and 2 of 9 starves but kept 27 to
  29 ms more input queued for up to 3.5 s after each burst, so it did not land. `native:engine`'s
  counter check forgives the join's trims and starves in a WASAPI open's first 3 s (`probe.rs`
  `GRACE`); a take that overlaps one is still rejected.
  Past the open too (2026-10-06, 44.1 kHz, no plugins, `--mute`): the join starves in bursts minutes
  into a run. Beside an ASIO 64 engine in a second process, on an idle machine: a starve about every
  8 s for two minutes, twice, 10 min 28 s apart (62 `join_starves` in 20 minutes; the ASIO process
  counted nothing), and after each burst two to three minutes with up to 16 % fewer render callbacks
  and no counter moving. Alone: 18 starves 55–134 s after the open in one run (under CPU load, the
  app at below-normal priority class), 2 at 129 s in a second, none in a third of 190 s at normal
  class. Neighbour, load and class are not separated (four runs). Share output showed the same
  rhythm into the same interface (the crackle thread above); the join's setpoint is monitoring
  latency, so widening it is the owner's call (D25).
- The release profile warns of four unused items in `app` (`Duration` in `host/vst3.rs`,
  `promote_pro_audio` in `host/clap.rs`, `teardown` in `host/vst3.rs`, `asio_available` in
  `audio_output.rs`); since when is unknown.
