# src-tauri/: native host briefing (Windows / Rust)

Everything Rust/native lives here: the native audio engine (`crates/lf-engine`) and its device side
(`src/engine_io`), the CLAP/VST3/VST2 plugin host, the ASIO tier, plugin editor windows. The root
`AGENTS.md` routes here: read this before any work in this subtree (`CLAUDE.md` beside it is a
one-line adapter). It holds the router-level gotchas and operational bits; the engine's rules live in
its two briefings (the crate doc of `crates/lf-engine/src/lib.rs`, the module doc of
`src/engine_io/mod.rs`). Which gate a change runs: root `AGENTS.md` (the `main` line);
`pnpm rust:check` runs on Windows and from WSL. `tauri dev`, the `native:*` probes and `native:kill`
are in `docs/VERIFY.md` § Native / Tauri verification. Only the by-ear gates need a person at the PC. Both `cargo check --features asio` and no-asio must stay green
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
| **Feed** (`lf-engine-feed`, `engine_io/feed.rs`) | The only reader of the engine's event ring (but for a rebuild, which drains the replaced engine's: `swap_engine`) and the device events; the mirror of lanes and transport a reload resyncs from | ~60 frames/s over a Tauri `Channel`; never PCM. The page acks (`engine_feed_ack`), and 30 frames unacked withhold the subscriber; held (`FeedThread::hold`, an RAII guard) while the plugin folder dialog is open, since the UI thread reads no frame then. Held or withheld it keeps ticking, so the event ring is drained and the mirror stays true, sends nothing, and ends with one catch-up frame (a reset after another engine or an overflow); a channel whose acks stand still is told to resubscribe (`feed.rs` module doc) |
| **Plugin owner, per instance** (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}` / `lf-vst2-engine-{slot}`, `host/engine_slot.rs`; during a hot swap a slot has two, the playing one and the candidate, `host/engine_slot/swap.rs`) | The `!Send` plugin instance (clack main thread / VST3 component + controller / VST2 effect, with its `effStartProcess` and `effStopProcess`), its editor host window and Win32 pump, its tone saves, restarts and re-activation after an eviction, the ordered teardown. An owner whose plugin a swap replaced never puts its unit in again (no restart, no re-activation): it waits for its teardown | `OwnerRequest`s (polled every 20 ms, `OWNER_POLL`); the unit enters and leaves the engine through its `SlotHost`; params through the slot's event ring; the unit's fault bits, reported once per load |
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
- **Plugin lifecycle runs on its owner, never on an instance the engine holds** (invariant 4): load,
  activate, a plugin-requested restart (CLAP `request_restart`, VST3 `restartComponent`, VST2
  `audioMasterIOChanged`) and teardown. A hot swap loads the next plugin while the current one plays
  and crossfades between them in the engine; outside one, an empty FX slot passes dry, an empty
  instrument slot is silent, and loops and click never wait.
- **Unload order is load-bearing:** the slot goes off live and the engine's gate closes on it
  (`EngineHost::silence_slot`, ≤1 s, else the unload is refused and the plugin stays; a plugin-requested
  restart skips it and stays live; answered by a block the engine rendered after taking that live-off,
  or, with no device running, by shutting the gate in the engine at once until that live-off applies) → `running=false` → `OwnerRequest::Wake` → join the owner, which
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
  at most two worktrees hold a build at a time (the integration tree, on `main` or `dev`, and one feature tree), and
  a feature tree is cleaned when its branch is merged: `cargo clean --manifest-path
  <absolute path>/src-tauri/Cargo.toml`, an absolute path, never `cd` then delete.
- **ASIO is a cargo OPT-IN feature** carried by the npm scripts (`pnpm dev:asio`, `pnpm build:app`);
  a plain `cargo build` must work without the LLVM/ASIO SDK, and `tauri dev` forces
  `--no-default-features` anyway. `[profile.dev.package."*"]` and lf-engine build at opt-level 3;
  `app`/`app_lib` stay opt-level 0.
- **ASIO SDK env** (`LIBCLANG_PATH`, `CPAL_ASIO_DIR`) is set PERMANENTLY in the User-scope env on
  the dev PC: a fresh shell inherits it, no inline setting needed (`pnpm dev:asio` just works).
  `CPAL_ASIO_DIR` must hold an EXTRACTED SDK with `common/` and `host/pc/` directly under it (on the dev
  PC the checkout's gitignored `asio-sdk/`: the `build-exe` workflow's pinned zip, its `ASIOSDK/`); `asio-sys`
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
  `export_save_part` (`export_save.rs`) takes a session export's bytes, a file name, an offset and a
  total, and no path: it writes only `<name>.<total>.part` and a free final name in the Downloads folder,
  replaces no finished file, refuses a name with `/`, `\`, `:`, `..` or a control character or one not
  ending in `.zip` before it touches the disk, and answers the full path to the WebView alone (never a
  log or an error string).
  `plugin_folder_add` takes its path from the native dialog alone; `plugin_folder_remove` takes one
  only to match a stored entry, and `plugin_scan_paths` (rig recall's scan of the saved plugins at
  launch) its paths only to match bundles the scan's own walk finds. Native MIDI's commands (`engine_io/midi_mode.rs`) ship in release and
  take only what they need: `input_send` an epoch and the batch's engine commands and note events,
  `midi_subscribe` a channel and the page's age (`performance.timeOrigin`), and the page's epoch to every
  other: `midi_learn` an action id and a track, `midi_cancel_learn` nothing more, `midi_forget` /
  `midi_set_momentary` / `midi_set_hold` a list revision and a listed index (and a flag), `midi_assign`
  those and a present port's id (no path is opened from it), `midi_import_legacy` the old list's text,
  parsed and never run as anything else. The feed's `engine_feed_ack` takes a frame `seq` or `null`
  only, and answers whether the page should subscribe again.
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
cache (plugin-scan.json beside the release log dir under %LOCALAPPDATA%) last saw them (an unchanged cached bundle spawns
nothing; a remembered failure is retried when its binary changes or by the picker's rescan button = `plugin_scan`
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
  (`engine_io/plugins.rs`). A command that races an unload is refused busy; a load whose report runs past its bound frees the slot early (`OPEN-THREADS.md`). The `plugin_*` commands that can wait run on `spawn_blocking` and read the
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
  waits on that tone's own in-flight import (`OPEN-THREADS.md`). Why a save skips the store: `tone.rs`.

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

In `OPEN-THREADS.md` beside this file: read it before diagnosing a native defect or editing code that cites it.
