# src-tauri/ — native host briefing (Windows / Rust)

Everything Rust/native lives here: the native audio engine (`crates/lf-engine`) and its device side
(`src/engine_io`), the CLAP/VST3 plugin host, the ASIO tier, plugin editor windows. The root
`AGENTS.md` routes here — read this before any work in this subtree (`CLAUDE.md` beside it is a
one-line adapter). It holds the router-level gotchas and operational bits; the engine's rules live in
its two briefings (the crate doc of `crates/lf-engine/src/lib.rs`, the module doc of
`src/engine_io/mod.rs`). **The Mac has NO Rust toolchain:** `cargo check` + every runtime gate are
PC-only; on Mac, adversarial-review Rust by reading. On the PC the gate is `pnpm rust:check`, which
also runs from WSL; `tauri dev`, the `native:*` probes and `native:kill` are in `docs/VERIFY.md` §
Native / Tauri verification. Only the by-ear gates need a person at the PC. Both
`cargo check --features asio` and no-asio must stay green (CI runs the no-asio half).

## Thread & ownership map

Who runs where and what each thread owns. The rules are in bold below the table.

| Thread | Owns | Talks to others through |
|---|---|---|
| **UI (WebView2 / main)** | The Tauri window, every WebView2 COM call (`with_webview`: the Web MIDI permission auto-grant, `lib.rs`), the close guard (`lf://close-requested`), and the synchronous commands: `engine_send` (so batches keep IPC order), `plugin_asio_status`, `plugin_asio_device_info`, `lib.rs`'s own | IPC in; window events out (`plugin:param-changed`, `plugin:params-changed`, `plugin:editor-closed`) |
| **Command threads** (Tauri async runtime) | Nothing long-lived. Every other command in `host/commands.rs` and `engine_io/mode.rs` is `async fn`; blocking engine work (an open waits up to 15 s) goes to `spawn_blocking` | Device requests to the device owner; plugin requests to a slot owner (`owner_request_5s`, ≤5 s); a load spawns the owner and waits ≤15 s on its rendezvous (a result that arrives later is the owner's to undo); device lists enumerate cpal directly (no stream opens) |
| **Device owner** (`lf-engine-owner`, `engine_io/owner.rs`) | Every device transition, one at a time; the cpal streams (Send: ownership is for ordering); building engines; the engine lock, only while no stream runs | A request channel with one-shot replies; polls what the callbacks latched and logs the glitch counters |
| **Device callbacks** (cpal driver threads) | Nothing: ASIO runs input then output in one bufferSwitch; WASAPI's output callback is the clock and its input callback feeds the join pipe. Each promotes itself to MMCSS Pro Audio on first entry | `try_lock` on the engine (a miss plays silence and counts); atomics and rings; faults latch for the owner |
| **Share output** (a cpal WASAPI callback, `engine_io/share.rs`) | The mirror endpoint's stream, while ASIO plays | Pulls the post-limiter master from its pipe; never takes the engine lock |
| **Feed** (`lf-engine-feed`, `engine_io/feed.rs`) | The only reader of the engine's event ring and the device events; the mirror of lanes and transport a reload resyncs from | ~60 frames/s over a Tauri `Channel`; never PCM |
| **Plugin owner, per slot** (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}`, `host/engine_slot.rs`) | The `!Send` plugin instance (clack main thread / VST3 component + controller), its editor host window and Win32 pump, its tone saves, restarts and re-activation after an eviction, the ordered teardown | `OwnerRequest`s (polled every 20 ms, `OWNER_POLL`); the unit enters and leaves the engine through its `SlotHost`; params through the slot's event ring; the unit's fault bits, reported once per load |
| **Shutdown** (`lf-engine-shutdown`, on exit, and before the updater's installer: `update.rs`) | Stops the feed, saves every tone, unloads the plugins while the device plays, closes the device | Bounded at 8 s (`SHUTDOWN_WAIT`): a stuck plugin is left to process exit |
| **ASIO probe** (`lf-asio-probe`, `asio_startup.rs`) | The one driver-resolving probe per process, requested by the frontend after the UI is up | Its status report |
| **Scan children** (`app.exe --scan-one <path>`) | One process per bundle in a kill-on-close Job Object, 20 s timeout; two reader threads per child drain stdout/stderr with caps | Descriptor JSON |
| **Plugin GUI threads** | A floating CLAP editor runs the plugin's own window thread and only sets flags (`HostGuiImpl` → `EditorClosed`) the owner acks; a hosted editor embeds into the owner's host window and is pumped there. VST3 `performEdit`/`restartComponent` (ONE component handler per load, set at load) touch only the event ring, the window event and the `RestartFlags` atom the owner drains each turn; CLAP `params.rescan` sets a flag the owner drains | Flags / events |
| **Native MIDI** (`lf-midi-ports`, `engine_io/midi`) | Built and tested, **never started by the app**: MIDI arrives through the WebView's Web MIDI, and WinMM input ports are exclusive, so the two cannot share a controller | — |

- **Only a slot's owner thread touches its plugin instance and editor.** Commands reach it through
  `OwnerRequest`; the audio thread only through the unit it installed (rings + atomics). Never
  `run_on_main_thread` a plugin call.
- **Only the UI thread makes WebView2 COM calls** (`with_webview`); a non-UI caller recovers the
  result over a channel.
- **The audio path never logs, locks (beyond `try_lock`), allocates or waits** (invariant 5): the
  engine callback, the Share callback and every plugin `process`. A unit latches fault bits
  (`FAULT_START`/`FAULT_PROCESS`/`FAULT_PARAM`) and the owner reports each once per load; add a new
  RT failure path to that latch, not `log::*`. DEV builds count RT allocations (`host/rt_alloc.rs`).
- **Plugin lifecycle runs on its owner with the slot bypassed** (invariant 4): load, activate, a
  plugin-requested restart (CLAP `request_restart`, VST3 `restartComponent`) and teardown; an FX slot
  passes dry, an instrument slot is silent, loops and click never wait.
- **Unload order is load-bearing:** `running=false` → `OwnerRequest::Wake` → join the owner, which
  removes the unit from the engine (crossfade to bypass, `stop` on the audio thread; ≤2 s,
  `REMOVE_TIMEOUT`) → deactivate → terminate → the module unloads LAST. A unit the engine does not hand
  back leaves the plugin loaded (leaked), never unloaded under a running processor. Both halves log
  their timing (`engine slot N VST3 teardown: remove=… release+deactivate+terminate+module=… ms`,
  `owner joined in … ms`), in release too; a plugin's own release can take seconds (Archetype: ~4.8 s,
  the slot dry meanwhile).

## Operational bits (recurring)

- **`crates/lf-engine` is the pure engine** (briefing: its `src/lib.rs`); `src/engine_io` is its
  device side (briefing: its `mod.rs`); the plugin units and owners are `host/engine_slot.rs`,
  `host/clap_engine.rs`, `host/vst3_engine.rs`, which `engine_io/plugins.rs` routes the `plugin_*`
  commands to. `tauri dev` watches all of `src-tauri/`, so an engine edit relaunches a running dev app.
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
  the dev PC — a fresh shell inherits it, no inline setting needed (`pnpm dev:asio` just works).
  `CPAL_ASIO_DIR` must hold an EXTRACTED SDK with `common/` and `host/pc/` directly under it; `asio-sys`
  only rebuilds when its fingerprint changes (a `cargo update`, a crate bump), so a green
  `cargo check --features asio` can be a stale `target/` cache over a missing SDK — `pnpm rust:check`
  confirms the SDK directory before its asio step. cpal stays pinned at `=0.18.1` (why: the
  engine_io briefing).
- **PROD-EXE recipe:** raw `cargo build --release` = a DEV-mode binary (wants devUrl). The real exe
  = `pnpm build:app`; launch DIRECTLY (`Start-Process app.exe`), never via stdout-redirect.
- **Release IPC surface:** `capabilities/default.json` grants only event listen/unlisten. `diag` is
  registered only under `debug_assertions`; keep the handler cfg and frontend `import.meta.env.DEV`
  surface in lockstep. Help's `app_log_dir` / `app_open_log_dir` (`lib.rs`) ship in release and take
  nothing from the WebView, and so do the updater's `app_update_check` / `app_update_install`
  (`update.rs`: the release channel is `plugins.updater` in `tauri.conf.json`); so do tone recall's `plugin_tone_take` / `plugin_tone_import` (raw bytes
  both ways) and `plugin_tone_forget`.
- **Sample-rate selector "C2" — DECIDED (owner), NOT BUILT:** swappable 44.1/48k, default device
  native. The engine already rebuilds at another rate (`OpenError::RateChange`); the pick is unbuilt.
- **Editor-hang Win32 gotcha (recurring):** a host window Win32-OWNED across threads deadlocks on
  close (sync cross-thread activation `SendMessage` vs a stopped pump). Fix lives in
  `host/editor_window.rs`: owner-LESS window + `drain_after_editor_teardown()` +
  `show_host_window_front()` one-shot `HWND_TOP` (no `WS_EX_TOPMOST`).
- **Timed Win32 waits round up to the timer tick Windows grants the process**, and that tick can be
  15.6 ms while the global resolution reads 1 ms. Bound a wait loop by a deadline, never a round count
  (`drain_after_editor_teardown`). A plugin's own waits stretch the same way; the measured numbers sit
  with the native baselines in `docs/VERIFY.md`.
- **Release logging:** `tauri-plugin-log` registers UNCONDITIONALLY → Stdout (the dev grep
  convention) + a rotated file at `%LOCALAPPDATA%\com.bleeploop.app\logs\bleeploop.log` (2 MB,
  KeepAll). The Rust panic hook chains the default hook and logs location+payload.
- **The CSP (`tauri.conf.json` `security.csp`) applies to BUILT apps only** — `tauri dev` serves from
  vite and is not covered, so a new asset origin, a CDN font or an `eval` breaks in release alone.
  Violations reach the release log as `[csp]` lines (`src/platform/logging.ts`); after adding a new
  kind of resource, run `pnpm build:app` once and grep that log. `style-src` is exempt from Tauri's
  nonce injection on purpose (a nonce would void `'unsafe-inline'`, which Solid's template styles need).

## Native-host verify ops

The out-of-process plugin scan is testable WITHOUT the full app — `cargo build` then
`target/debug/app.exe --scan-one "<plugin path>"` prints the descriptor JSON and exits (the
`--scan-one` dispatch runs before Tauri starts). The scan spawns one child per `.clap`/`.vst3` for
crash + hang isolation (**20s per-child timeout** — a heavy/licensed VST3 like Neural DSP can hang
on load in the headless child) — but only for bundles whose binary size/mtime changed since the
cache (plugin-scan.json beside the release log dir under %LOCALAPPDATA%) last saw them (a launch spawns
nothing; a remembered failure is retried only by the picker's rescan button = `plugin_scan`
`force`). Delete that file to force a cold scan from outside the app. Pipe retention is capped at 1 MiB
stdout / 64 KiB stderr, and each child process tree sits in a kill-on-close Job Object. DEV probes on
the debug `app.exe`, each exiting before Tauri starts: `--probe-engine` (`pnpm native:engine`, the
device side on the rig: `engine_io/probe.rs`), `--probe-engine-spike` and `--probe-share` (`pnpm
native:spike`: the premise numbers in `docs/ARCHITECTURE.md` § Measured premise).
Stale `<old-path>\rc500\…` build path on dev start → `rm -rf src-tauri/target/debug/build`.
(Driving and grepping a running `tauri dev`, and stopping it: `docs/VERIFY.md`.)

## Plugin hosting (CLAP + VST3)

All in `src-tauri/src/host/`. The known-fragile area: read this whole section before any plugin or
plugin-GUI work.
- **Every WebView document gets a `frontendEpoch` from `host_init`** (one atomic step, never 0). A
  slot is reserved for the document that asked before foreign setup; a reload cancels the
  reservation, and a load that finishes for a replaced document unloads again (`engine_io/plugins.rs`).
- **The `!Send` instance (clack `PluginInstance`, the VST3 component) lives on its owner thread**, NOT
  `tauri::State`; only the `Send` unit (`ClapUnit`, `Vst3Unit`: the processor with its buffers and
  lists) enters the engine, which never drops one (a drop frees memory and calls into the DLL).
- **Surge param ids are hash-like, NOT 0-based** (`first=825615485`); sending an unknown id CRASHES the
  plugin → always enumerate via `listParams`, never invent an id (`plugin_set_param` refuses an id the
  plugin never listed).
- Test plugin **Surge XT** (`winget install SurgeSynth.SurgeXT` → `…\CLAP\Surge Synth Team\`, + Surge XT
  Effects); scan walks `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`, `CLAP_PATH`.
  Loader is `clack_host::entry::PluginEntry::load` (unsafe), NOT `PluginBundle`.
- **Plugin editors embed into a host-owned top-level Win32 window** (`CreateWindowExW`, owned by the main
  window — NOT reparented into the WebView2 surface). An owner pumps its thread's Win32 messages every
  turn, editor or not: a JUCE plugin (Neural DSP) runs its message thread there, and unpumped, a
  host-set parameter never reached its saved state (measured with `pnpm native:tone-recall`, Archetype
  Petrucci). Each pump call is bounded (64 messages or 2 ms, `editor_window::pump_thread_messages`), so
  a plugin whose messages repost themselves cannot keep an owner from its requests, saves or unload.
  GUI calls go via the owner channel, NOT `run_on_main_thread`.
- **Editor size is the plugin's, measured not computed:** `editor_window::set_client_size` sizes the
  CLIENT area by measuring the real frame (DPI-correct), at creation and on every plugin-initiated
  resize — VST3 `IPlugFrame::resizeView` (then `onSize` with the granted size) and hosted-CLAP
  `request_resize` both land there. Never answer a resize `kResultOk`/`Ok` without resizing; the view
  lays out for the size you confirm. Fixtures: `host/vst3_resize_fixture.rs`, `clap::resize_tests`.
- **Crate `vst3` 0.3.0** (coupler-rs; only dep `com-scrape-types`, no `windows`/`windows-core` conflict).
  `ComPtr<IAudioProcessor>` is already `Send+Sync`. Source-verify against the crate source
  (`~/.cargo/registry/src/index.crates.io-*/vst3-0.3.0/src/bindings.rs`), NOT web docs; methods are
  **camelCase**; `kResultTrue == kResultOk == 0` (compare `== kResultTrue`, never as a bool).
- **VST3 teardown order** (`vst3_engine.rs`: `teardown`, `Vst3Plugin`'s drop): the unit leaves the
  engine (`setProcessing(0)` on the audio thread) → a separated controller disconnects and terminates →
  `setActive(0)`, only on an active component (a failed restart leaves it inactive) → `terminate` →
  the COM objects drop → `Vst3Module` LAST. Its RAII drop pairs successful `InitDll` with `ExitDll`,
  then calls `FreeLibrary` (any plugin-DLL `ComPtr` must drop first — its vtbl lives in the module).
- **Surge XT VST3 is SEPARATED-component** (`component.cast::<IEditController>()` is None) → `obtain_controller`
  (`getControllerClassId`→`createInstance`→`initialize`); a JUCE separated controller's `createView` returns
  null until it gets the in-process `AudioProcessor` pointer over a connection-point `notify(IMessage)` (host
  `LfMessage`/`LfAttributeList` + `IConnectionPoint` cross-connect).
- Scanner handles BOTH VST3 forms: single-file `.vst3` AND folder bundle `Contents/x86_64-win/<inner>.vst3`;
  descriptor `id` = the class TUID hex. `host/vst3.rs` stays a child mod OF `host/clap.rs` (`#[path]` decl, so
  `vst3::Steinberg` imports don't collide with clack and `super::` keeps meaning); the engine units and
  owners are child mods the same way.
- **VST3 buses:** query `getBusInfo` AFTER `setBusArrangements`, size buffers to the reported channel count,
  don't assume the requested arrangement (same crash class as the Surge hash-param id). Guard zero-input
  plugins (synths). `activate_component` (`host/vst3.rs`) is the ONE owner of that sequence — load and
  every restart run it, and each install takes the `Activation` it produced, so a `kIoChanged` that
  changes a count resizes the unit's buffers; the unit's kind (effect with an input bus, instrument
  without) follows each activation.
- **Host-set VST3 params go to BOTH halves:** the slot's event ring feeds the processor,
  `OwnerRequest::SetParamNormalized` feeds the edit controller on the owner thread
  (`EngineSlotHandle::set_param` is the one entry; the controller hears only what the ring took).
  The controller is what the plugin GUI shows and what raises a controller-decided `restartComponent`
  (FabFilter latency modes) — drop the mirror and no drawer change can ever restart a plugin again.
  `performEdit` (GUI → host) is never mirrored back. A 30-plugin restart survey backs this: FabFilter
  raises `kLatencyChanged`, Neural DSP and Surge never do.
- **Tone recall (briefing: `host/tone.rs`):** a load restores the plugin's stored state before it
  activates — CLAP `state.load`; VST3 `IComponent::setState`, then the controller's
  `setComponentState` and `setState`, through the host `MemStream` (`vst3.rs`) — and the owner saves it
  on its own thread. No request pushes state into a running plugin. The VST3 load creates the
  controller and sets its handler BEFORE activation (the SDK host's order). A plugin that refuses its
  tone is discarded and created again before it activates (it may have taken half the state); a
  session import goes through its tone's write lock in the store (`ToneStore::import`), never an owner
  request, and its bytes reach only the reload's load that passes the import's reload token
  (`ToneHandoff`). An owner's turn never waits on a lock held across disk I/O. Why a save skips the
  store: `tone.rs`.

## ASIO tier

- **cpal ASIO = ONE driver per device, resolved once per driver pick:** once a stream holds it, cpal
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
  `[asio] probe starting` → `cached ASIO "…"` → `[asio] probe result … Ready` AFTER `host_init`, and
  nothing ASIO-related before it; `pnpm dev:asio -- -- --disable-asio` shows `DisabledByFlag` and no
  probe. Read `plugin_asio_device_info` for the cached metadata and `engine_status` (or the
  `[engine_io]` open lines) for the backend that opened.

## Open threads (no gate)

- Tone recall: an owner's save of plugin P in slot S waits on that tone's own in-flight store write (an
  import of P into S under its write lock). A disk write that hangs therefore stalls that owner, and an
  unload joins it without a timeout (the app's exit is bounded). Other tones' writes never block it; a
  store worker doing the file I/O would remove the wait.
- `plugin_set_param` answers `Err` on a full event ring or an unlisted param id; a UI reaction (the
  slider snaps back to the plugin's value) is unbuilt.
- A VST3 unit drops a param past 64 distinct ids in one block without a fault bit (`MAX_PARAM_QUEUES`).
- `clap_engine.rs` and `vst3_engine.rs` each carry the whole owner choreography (load, restart,
  eviction, teardown): one shared owner would keep B1/B11 from returning. Not built.
- A unit whose restart failed stays parked, bypassed, until the plugin's next restart request; a device
  change does not retry it.
- Each ASIO overload counts twice in `xruns` (both streams' error callbacks count it), and an output
  callback that misses the engine lock reads as a duplex fault.
- The panic hook (`lib.rs`) allocates and logs on whatever thread panicked, the audio thread included.
- A slot, lane or master gain ramping to 0 is not snapped to its target and can sit subnormal.
- `crates/lf-engine/tests/slots.rs` checks a removal's fade with `<=` where `==` is meant.
- The dry signal steps without a ramp on a live toggle and on an instrument installed into a live slot.
- Two live slots on the same capture channel sum it (+6 dB).
- An ASIO period the driver drops without its overload report is not flagged (input and output stay in
  step; the take is spliced there).
- A punch-out inside a take's last quarter-beat commits the whole bars before it, where a stop there
  rounds up (owner's call).
- The no-device removal path (a 1-frame process and `stop` on the plugin owner's thread) has no test
  with a real unit, and the CLAP restart fixture's thread check would flag it.
- Native MIDI (never started): a pedal binding's port occurrence is recounted on every hot-plug, so two
  same-named controllers can swap bindings; a port back within one 1 s poll keeps a dead connection.
