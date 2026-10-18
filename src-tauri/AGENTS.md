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
| **UI (WebView2 / main)** | The Tauri window, every WebView2 COM call (`with_webview`: the Web MIDI permission deny, `lib.rs`), the plugin folder dialog (modal over the main window, `src/host/folders.rs`), the close guard (`lf://close-requested`), and the synchronous commands: `input_send` (the UI's one ordered batch of engine commands and note input, one in flight at a time), `midi_subscribe`, `plugin_asio_status`, `plugin_asio_device_info`, `lib.rs`'s own | IPC in; window events out (`plugin:param-changed`, `plugin:params-changed`, `plugin:editor-closed`) |
| **Command threads** (Tauri async runtime) | Nothing long-lived. Every other command in `host/commands.rs`, `engine_io/mode.rs` and `engine_io/midi_mode.rs` is `async fn`; blocking engine work (an open waits up to 15 s) goes to `spawn_blocking` | Device requests to the device owner; plugin requests to a slot owner (`owner_request_5s`, ≤5 s); a load spawns the owner and waits ≤15 s on its rendezvous (a setup that ends later never installs and is the owner's to release; a unit already going in at the bound stands: `engine_slot::LoadClaim`); device lists enumerate cpal directly (no stream opens) |
| **Device owner** (`lf-engine-owner`, `engine_io/owner.rs`) | Every device transition, one at a time; the cpal streams (Send: ownership is for ordering); building engines; the engine lock, only while no stream runs | A request channel with one-shot replies; polls what the callbacks latched and logs the glitch counters |
| **Device callbacks** (cpal driver threads) | Nothing: ASIO runs input then output in one bufferSwitch; WASAPI's output callback is the clock and its input callback feeds the join pipe. Each promotes itself to MMCSS Pro Audio on first entry and runs with flush-to-zero on (`engine_io/fpu.rs`) | `try_lock` on the engine (a miss plays silence and counts); atomics and rings; faults latch for the owner |
| **Share output** (a cpal WASAPI callback, `engine_io/share.rs`) | The mirror endpoint's stream, while ASIO plays | Pulls the post-limiter master from its pipe; never takes the engine lock |
| **Feed** (`lf-engine-feed`, `engine_io/feed.rs`) | The only reader of the engine's event ring (but for a rebuild, which drains the replaced engine's: `swap_engine`) and the device events; the mirror of lanes and transport a reload resyncs from | ~60 frames/s over a Tauri `Channel`; never PCM. Held (`FeedThread::hold`, an RAII guard) while the plugin folder dialog is open, since the UI thread reads no frame then: it keeps ticking, so the event ring is drained and the mirror stays true, sends nothing, and the close resyncs the page with one `reset` frame |
| **Plugin owner, per slot** (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}` / `lf-vst2-engine-{slot}`, `host/engine_slot.rs`) | The `!Send` plugin instance (clack main thread / VST3 component + controller / VST2 effect, with its `effStartProcess` and `effStopProcess`), its editor host window and Win32 pump, its tone saves, restarts and re-activation after an eviction, the ordered teardown | `OwnerRequest`s (polled every 20 ms, `OWNER_POLL`); the unit enters and leaves the engine through its `SlotHost`; params through the slot's event ring; the unit's fault bits, reported once per load |
| **Shutdown** (`lf-engine-shutdown`, on exit, and before the updater's installer: `update.rs`) | Drops native MIDI first (its ports close, its holds are released), stops the feed, saves every tone, unloads the plugins while the device plays, closes the device | Bounded at 8 s (`SHUTDOWN_WAIT`): a stuck plugin is left to process exit |
| **ASIO probe** (`lf-asio-probe`, `asio_startup.rs`) | One driver-resolving probe at a time, requested by the frontend after the UI is up (a retry after a failure and a driver switch probe again) | Its status report |
| **Scan children** (`app.exe --scan-one <path>`) | One process per bundle in a kill-on-close Job Object, 20 s timeout; two reader threads per child drain stdout/stderr with caps | Descriptor JSON |
| **Plugin GUI threads** | A floating CLAP editor runs the plugin's own window thread and only sets flags (`HostGuiImpl` → `EditorClosed`) the owner acks; a hosted editor embeds into the owner's host window and is pumped there. VST3 `performEdit`/`restartComponent` (ONE component handler per load, set at load) touch only the event ring, the window event and the `RestartFlags` atom the owner drains each turn; CLAP `params.rescan` sets a flag the owner drains; a VST2 plugin's host callback, from any thread, sets latches in its `HostContext` the owner drains each turn | Flags / events |
| **Native MIDI** (`lf-midi-ports` and the midir callbacks, `engine_io/midi`) | The app's only MIDI path: started with the engine host (`engine_io/mode.rs`, once per launch; none when the engine did not start) and dropped first at shutdown; the WebView's Web MIDI is denied (`lib.rs`), since a WinMM input port may be exclusive. The port thread opens the ports, follows hot-plug and runs the host's timers (learn's release waits, the queue's retry, the binding store's writes); a midir callback runs learn and the router for its message, under the one MIDI lock | `EngineHost::send` through one ordered queue, under that lock (taken before `settings` and `ends`); the device owner's rebuild through `EngineHost::set_rebuild_hook`; the UI's engine commands and notes in through `input_send` (under the input epoch its `midi_subscribe` answered), its learn calls through `midi_*`, its events out over `midi_subscribe`'s channel (`engine_io/midi_mode.rs`) |

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
- **Unload order is load-bearing:** the slot goes off live and the engine's gate closes on it
  (`EngineHost::silence_slot`, ≤1 s, else the unload is refused and the plugin stays; a plugin-requested
  restart skips it and stays live; answered by a block the engine rendered after taking that live-off,
  or, with no device running, by closing the gate in the engine at once) → `running=false` → `OwnerRequest::Wake` → join the owner, which
  removes the unit from the engine (crossfade to bypass, `stop` on the audio thread; ≤2 s,
  `REMOVE_TIMEOUT`) → saves the tone if it changed → deactivate → terminate → the module unloads
  LAST. A unit the engine does not hand back leaves the plugin loaded (leaked), never unloaded under a
  running processor (VST2's own order: § Plugin hosting). Both halves log
  their timing (`engine slot N VST3 teardown: remove=… release=… ms`,
  `owner joined in … ms`), in release too; a plugin's own release can take seconds (Archetype: ~4.8 s,
  the slot dry meanwhile).

## Operational bits (recurring)

- **`crates/lf-engine` is the pure engine** (briefing: its `src/lib.rs`); `src/engine_io` is its
  device side (briefing: its `mod.rs`); the plugin slots are `host/engine_slot.rs` (the load API and
  the one owner loop every format shares, `serve`) and each format's unit and owned plugin,
  `host/clap_engine.rs`, `host/vst3_engine.rs`, `src/host/vst2_engine.rs`, which
  `engine_io/plugins.rs` routes the `plugin_*` commands to. `tauri dev` watches all of `src-tauri/`,
  so an engine edit relaunches a running dev app.
- **An engine ASIO open opens the driver at another block size first** (`engine_io/cpal_driver.rs`,
  ~500 ms): opened again at the size it last ran, the previous rig's Focusrite driver landed two
  periods late. Whether the EVO 4's driver needs it is unmeasured (a `--lag=1 --preopen=0` run).
- **An ASIO driver is never asked for a buffer outside the range it reported to the probe** (cpal
  refuses it): a request outside opens at `engine_io::transition::asio_block`'s pick, and a one-size
  driver (set in its own control panel) gets no preopen. A size inside the range that the driver still
  refuses (sizes in steps, `min + k·step`, which the probe cannot see) opens once more at the driver's
  own size, with no preopen; `DeviceStatus.block` is the size the callbacks deliver.
- **Parallel worktrees must not share a target dir.** Each checkout's default `src-tauri/target` is
  already its own: never point two worktrees at one `CARGO_TARGET_DIR`. Sharing one, every worktree
  links the same `app_lib-<hash>` test binary and cargo judges path crates fresh by mtime, so one
  worktree can run another's build of `app` or `lf-engine`. From WSL, Windows cargo does not see a
  `CARGO_TARGET_DIR` exported in bash (WSLENV does not pass it). Each target grows to 12–16 GB, so
  at most two worktrees hold a build at a time (`dev`'s integration tree and one feature tree), and
  a feature tree is cleaned when its branch is merged: `cargo clean --manifest-path
  <absolute path>/src-tauri/Cargo.toml`, an absolute path, never `cd` then delete.
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
  `app_update_check` / `app_update_download` / `app_update_install` (`update.rs`; the release channel
  is `plugins.updater` in `tauri.conf.json`) ship in release and take nothing from the WebView; so do tone recall's
  `plugin_tone_take` / `plugin_tone_import` (raw bytes both ways) and `plugin_tone_forget`.
  `plugin_folder_add` takes its path from the native dialog alone; `plugin_folder_remove` takes one
  only to match a stored entry, and `plugin_scan_paths` (rig recall's scan of the saved plugins at
  launch) its paths only to match bundles the scan's own walk finds. Native MIDI's commands (`engine_io/midi_mode.rs`) ship in release and
  take only what they need: `input_send` an epoch and the batch's engine commands and note events,
  `midi_subscribe` a channel and the page's age (`performance.timeOrigin`), and the page's epoch to every
  other: `midi_learn` an action id and a track, `midi_cancel_learn` nothing more, `midi_forget` /
  `midi_set_momentary` / `midi_set_hold` a list revision and a listed index (and a flag), `midi_assign`
  those and a present port's id (no path is opened from it), `midi_import_legacy` the old list's text,
  parsed and never run as anything else.
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
  KeepAll). The Rust panic hook chains the default hook and logs location+payload, except on an
  audio callback's thread, where it does neither and the callback's panic counter reports it.
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
`force`), and for every VST3 factory that declares `kClassesDiscardable`, which is never cached and
gets a child on every scan. Delete that file to force a cold scan from outside the app. Pipe retention is capped at 1 MiB
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
- **Every WebView document gets a `frontendEpoch` from `host_init`** (one atomic step, never 0), and
  every request that changes a slot (load, unload, a parameter, the editor) carries it: one from a
  replaced document is refused, checked as it takes the slot. A load reserves its slot before foreign
  setup, and only that load moves the slot on: one that finishes for a replaced document, or that an
  unload cancelled, unloads what it loaded, and the slot takes no load until the plugin's unit has
  left the engine (an unload holds it the same way); a load for the new document waits for it
  (`engine_io/plugins.rs`). An unload that gives up on a handle still in use frees the slot early (§ Open threads). The `plugin_*` commands that can wait run on `spawn_blocking` and read the
  shared `PluginHostState` there (`host/commands.rs`).
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
  refuses a shell too, by its category); a bundle whose scan failed is listed beside them with a
  short host-worded category, never the child's output (Audio Settings' PLUGINS page, never the picker). A plugin reached twice keeps the
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
- **VST3 teardown order** (`vst3_engine.rs`: `Vst3::release`, `Vst3Plugin`'s drop): the unit leaves the
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
  - **Teardown:** the editor closes → the unit leaves the engine → `effStopProcess` →
    `effMainsChanged(0)` → the tone is saved if it changed → `effClose` → the `HostContext` → the
    module LAST.
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
  tone is checked against the plugin before its first call and its calls answer no verdict, so only
  a tone refused after a call, a program select that changed the parameter count, discards it); a
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

- Six timing tests failed once each in a loaded local `pnpm rust:check` (2026-10-16), green alone and on rerun:
  the CLAP `a_request_from_another_thread_never_waits_for_the_owner`, the `engine_slot` give-up-during-a-restart
  test, the feed thread's hold test, and three `engine_io` tests that record a first loop. The cause was read
  from the code, never reproduced: short wall bounds a loaded PC outlasts (500 ms to 5 s waits, the test
  device's `REMOVE_TIMEOUT` 2 s, and `record_loop`'s 1.5-bar stop sent from the test thread's wake, which the
  first take's floor to whole bars turns into a 2-bar loop when late). Tests now wait up to 10 to 30 s
  (`cfg(test)` variants of `REMOVE_TIMEOUT`, `CALLBACK_WAIT`, `START_WAIT`, the owner `ask`; `test_rig::PATIENCE`)
  and stamp the loop's stop on the engine's frame. Next check, only if one fails again: loop it under a
  parallel `cargo build` and read the failing line against these bounds.

- Tone recall: an owner's save of plugin P in slot S waits on that tone's own in-flight store write (an
  import of P into S under its write lock). A disk write that hangs therefore stalls that owner, and an
  unload joins it without a timeout (the app's exit is bounded). Other tones' writes never block it; a
  store worker doing the file I/O would remove the wait.
- An unload that gives up waiting (6 s) for a command still using the handle empties the slot while
  that command's drop of the handle still tears the plugin down (read from source, never run; no
  command found holds a handle that long: each owner round trip is ≤ 5 s). A load meanwhile fails on
  "already holds a unit" (rig recall keeps the plugin) or wins the slot first; the late owner's unit
  may sound for its crossfade. A load whose unit went in as its 15 s bound ran out, and whose report
  then took 2 s more, frees the slot the same way (`engine_slot::LoadClaim`). Next check: a slot test
  in `engine_io/plugins.rs` that holds a handle clone past the unload wait, then loads. A fix carries
  an owner-ended flag in `Leaving`.
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
- The ASIO callback is not lock-free: asio-sys holds `BUFFER_CALLBACK.lock()` around every callback,
  and cpal 0.18.1 takes its `asio_streams` mutex in them (`Start` reads the buffer size before either
  stream plays, so the owner does not contend there). Fix: patched ASIO dependencies, with the L1+L2
  latency measurements.
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
  possibly off the click, and the buffer select stale. Which messages a driver sends is unknown; the
  Focusrite driver this was written against is uninstalled, and the rig's driver is now the EVO 4's,
  whose EVO Control tray app is where the buffer size and the rate are set (`STATUS.md`). A fix keeps the loops (a rate the owner did not confirm discards them: `Owner::open`) and
  shows what runs, rather than adopting the driver's settings blindly. Next check, on the rig: change
  the buffer in the driver's panel, then the rate, each while loops play, and record a take after
  each: the log, the block size the callbacks get (the status reads it only at the open), and whether
  the take lands on the click. Under WASAPI the buffer select lists every
  size and the run ignores it (`cpal_driver::resolve_wasapi`).
- The feed has no backpressure: `turn` (`feed.rs` 396) sends every tick while `Channel.send` succeeds
  (`mode.rs` 231), and Tauri 2.11.5 queues large payloads unbounded until the page fetches them, so a
  stalled WebView grows a backlog of stale frames. Fix: a fixed number of outstanding frames with an ACK
  per subscription and sequence; drain meanwhile, resync on resume without breaking active gestures or
  session revisions, and keep one-shot events in their own bounded queue.
- A page that dies for good (a WebView crash, a shutdown) leaves the scope taps on (`Key::Scope`), so
  the engine keeps folding columns the feed drains and drops for nobody. Read from source, not
  reproduced, nothing audible.
- A punch-out inside a take's last quarter-beat commits the whole bars before it, where a stop there
  rounds up (owner's call).
- The feed's reset mirror carries no count: a WebView reload during a count-in shows no numeral until
  the next count beat arrives, and after the last one a later take reads WAITING FOR DOWNBEAT until
  its take starts.
- The no-device removal path (a 1-frame process and `stop` on the plugin owner's thread) has no test
  with a real unit, and the CLAP restart fixture's thread check would flag it.
- Native MIDI (rules: `docs/ARCHITECTURE.md` § Decided: native MIDI; the by-ear checks: `STATUS.md`
  § Not heard yet). Residuals, each needing an IPC call stalled past its 2 s bound or a clock moved
  back: a note-on whose batch timed out can land after its release and stick until a blur, a slot
  switch or a panic; a blur owed after a failure, answered late, can release a hold made after it; a
  page whose `performance.timeOrigin` reads older than its predecessor's (the wall clock moved back
  and the WebView restarted) is refused until the app restarts. Next check: log `input_send` round
  trips over a jam: does a 2 s stall ever happen? Only a hardware controller can settle
  (none has reached native MIDI yet): whether the port name Web MIDI stored equals WinMM's `szPname`
  (where it differs, an imported binding waits until the player assigns it), whether midir's id (the
  device-interface path) is stable across a restart and a replug, which interface class a port
  arrives on (the next check: `src/engine_io/midi/liveness.rs`'s header), and whether WinMM input is
  multi-client under `wdmaud2`. Owed: the release after the first with native MIDI removes the web
  build's `lf.midiLearn` key (`src/app/midi-actions.ts`). Its module briefing (`midi/mod.rs` header)
  restates ARCHITECTURE's subscription/epoch, nothing-is-stamped, no-device and rebuild-needs-no-WebView
  rules: replace only that overlap with pointers, keep its local access, batch, lock and retry detail.
- Archetype Plini (VST3) once stalled 4–14 s in 5 of 20 unloads, editor closed, and has not repeated
  since (cause unknown). The VST3 teardown and the unload log per-step timing in release too, so the
  next occurrence names its step.
- Crackle, the owner's two reports, measured on the Scarlett with silent soaks (`native:engine`; the runs' numbers:
  this thread's git history). **In a call:** Share output into the interface's own Windows endpoint
  ran short in bursts at a 20 ms setpoint and soaked clean at 40 ms (`engine_io/share.rs`); the
  mirror's first pull now plays silence (unsoaked). **While agents build** (ASIO 64, `--tone=1
  --load=test`): a normal-priority build on the audio thread's own core stretches the callback four to
  eight times its median with its thread on the CPU throughout (`tone::cycle_stamp`; MMCSS priority
  25, with or without plugins), and the loopback tone breaks (6 to 48 silent samples) where a
  callback finishes about 2.5 periods or more after the wakes' best phase, with no fault counter moving.
  A larger buffer does not help (256 broke too); a load kept off cores 0 to 3
  (`--load-affinity=FF00`) ran nothing long in two runs. The release log's `asio_late_finishes`
  (`callback::LATE_FINISH_PERIODS`) counts near misses too and missed one break at 256; `over_budget=N`
  marks every long callback. The cable returns through the same driver, so a break's side (input or
  output) is unknown. Open, with the next check: by ear, whether keeping builds off the audio core
  keeps a jam at ASIO 64 clean, and whether the call's crackle is gone (`STATUS.md` § Not heard yet);
  whether a below-normal build (the rig's tmux session starts them so) stalls it too, and which of
  the core's SMT sibling and caches matters (`--load-affinity=AAAA`); one-frame steps in the tone (no
  late callback near; side unknown); something added near the tone's frequency (a beat fading over
  0.6 s about 6.8 s into the stream, smaller ones minutes apart, seen at `--tone-level=0.125`; next:
  the input with no tone). A tester hears crackle too, less with a gate on their interface (their
  input side; their backend unknown, and the WASAPI join's bursts below would sound like that).
- Plugin-host gaps a source review found (2026-10-02; read from source, none reproduced), ranked by
  exposure on the owner's plugins. First: VST3 omits trailing inactive aux buses (the SDK's
  `activateBus` rule permits it) and passes short `setBusArrangements` arrays whose result is read
  back, not checked; a tone saved by an export or an unload before the next block has consumed an
  accepted parameter edit loses that edit. Then: two slots loading one VST3 DLL run its init
  unserialized; a CLAP restart can deliver a removed parameter id; a non-discardable VST3 module is
  unloaded. Lower: kReloadComponent only reactivates; a MIDI-only CLAP note port gets CLAP notes; CLAP
  editor edits never reach the drawer.
- The quit window (a close and an update share it: `src/app/close-guard.ts` `quitWithSave`): input
  stays open from the quit save's snapshot to the engine's shutdown, so a take in flight, or one a
  pedal or the UI commits in those 1-5 s, is not in the recovery. Next: an engine-owned quiesce through
  the command ring (stop admitting fresh presses, end captures as a punch-out keeps them, let block jobs
  finish while the device renders, then answer), after which the quit save reads its snapshot. And an
  update whose installer fails after the engine's shutdown (`ShellExecuteW` failing, or the installer
  quitting with an error after it started) leaves the app without an engine or with none open: the
  first says "restart BleepLoop", the second nothing; a recovery needs a process outside the app that
  watches the installer. Neither has been seen; only a real update shows them.
- `native:swap`'s editor-open round passes with every editor open failed: `src/debug/swap-stress.ts`
  83-97 logs the failure, counts the swap, and the runner accepts "0 failed". Fix: count a failed open
  as a failed scenario and report swaps done with an editor open (the runner's regex follows).
- After a WASAPI open the join can trim or starve within ~2.5 s, and a take that overlaps it is
  rejected (how often: `docs/VERIFY.md`, the `native:engine` baseline). Traced (the join trace in
  `engine_io/callback.rs`): once primed, capture delivers a packet early and a render callback comes a
  period late, so the ring sits up to two periods over its 25 ms setpoint, under the trim line, for the
  ~10 s the controller takes to drain it, and one more late callback trims. Why the endpoints start
  this way is unknown. The trims stay (D25): a rule that tolerated the bursts, replayed on nine traced
  opens, removed the trims but kept 27 to 29 ms more input queued for up to 3.5 s. `native:engine`
  forgives a WASAPI open's first 3 s (`probe.rs` `GRACE`). Past the open the join also starves in
  bursts minutes in, beside an ASIO process and alone under load; neighbour, load and priority class
  are not separated. Widening the setpoint is monitoring latency: the owner's call (D25).
- Loopback offsets the bars pass but nothing explains (`native:engine-loopback` on the Scarlett): each
  device open lands a whole number of frames off (−5..+4), the same for every take inside it, and once
  at 128 a 32 s multiply take stepped one frame late between its beats 24 and 32 (two relaunches did
  not repeat it). A stray sound reached the cable on 2026-10-02 (about 200 Hz decaying over ~1.4 s and a
  ~400 Hz tone every 6.000 s, mostly in the first take after a device switch), now failed by name as
  stray sound (likely also the one launch at 64 that read 149 times the floor bar past FADE's bar
  line); its source is unknown, other apps playing through the interface the suspect. Next check:
  the first EVO 4 loopback run (`--channel=0`), other apps' sound silenced, logging each open's offset.
- Two session loads the device side drops or applies against the caller's answer, each a red test
  kept `#[ignore]` in `engine_io/tests.rs` until its fix. A load the host gave up on (no answer within
  10 s, `engine_io/session.rs`) stays in the session port and plays once the callbacks resume
  (`d21_a_load_that_timed_out_never_applies_later`); the next check is whether a give-up can withdraw
  the sent job, or a late return be dropped by its generation. An unforced rate switch reads
  `holds_audio` before its stop (`Owner::open`), so a load that lands in the fade-out is dropped with no
  `LoopsDropped` (`d21_a_load_that_lands_while_an_unforced_rate_switch_stops_is_not_dropped_unasked`);
  the next check is re-reading it after the stop, before the build.
- Unused items `app` still warns of (`engine_io` has no blanket `allow(dead_code)` any more): in every
  build `asio_available` (`audio_output.rs`; `engine_io/probe.rs` keeps its own copy) and `plugin_gain`
  (`engine_io/plugins.rs`, no caller); without `asio` also `asio_startup.rs`'s unbuilt states and
  coordinator methods and `ASIO_PROBE_TIMEOUT` (`audio_output.rs`). Since when is unknown.
