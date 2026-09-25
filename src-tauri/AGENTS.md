# src-tauri/ — native host briefing (Windows / Rust)

Everything Rust/native lives here: the CLAP/VST3 plugin host, cpal audio I/O + ASIO tier, plugin
editor windows. The root `AGENTS.md` routes here — read this before any work in this subtree (some
harnesses also auto-load it once a session touches `src-tauri/`; `CLAUDE.md` beside it is a one-line
adapter). It holds the load-bearing gotchas and operational bits for **P9–P11 (all DONE + LIVE on
PC)**. **The Mac has NO Rust toolchain:** `cargo check` + every
runtime gate are PC-only; on Mac, adversarial-review Rust by reading. On the PC the gate is
`pnpm rust:check`, which also runs from WSL; `tauri dev`, the `native:*` plugin probes and
`native:kill` are in `docs/VERIFY.md` § Native / Tauri verification. Only the by-ear gates need a
person at the PC. Both `cargo check --features asio` and no-asio must stay green (CI runs the no-asio
half).

## Thread & ownership map

Who runs where, what each thread owns, and how data moves between them. The rules are in bold; the
rest is the map.

| Thread | Owns | In | Out |
|---|---|---|---|
| **UI (WebView2 / main)** | The Tauri window, every WebView2 COM call (`with_webview` closures: `create_shared_ring` allocates + posts the hop-1 SharedBuffer, `SlotHandle::teardown` `Close()`s it, the permission auto-grant), the JS side | `window.emit` events (`plugin:param-changed`, `plugin:params-changed`, `plugin:editor-closed`, `plugin:stream-fault`, `lf://close-requested`); posted SharedBuffers | IPC commands |
| **Command threads** (Tauri async runtime) | Nothing long-lived. Every `#[tauri::command]` in `host/commands.rs` is `async fn`, so it runs on the runtime pool, not the UI thread | IPC args | Briefly lock `PluginHostState.slots`; push `PluginEvent`s into the per-slot event ring (`enqueue_event`, Mutex'd Producer); send `OwnerRequest`s and block ≤5 s on a one-shot reply (`owner_request_5s`); spawn the owner thread at load and block ≤15 s on the rendezvous `ready_rx` (a result that arrives later is the owner's to undo: `spawn_rt_then_ready`); enumerate cpal devices directly (no stream is opened) |
| **Per-slot owner** (`lf-clap-owner-{slot}` / `lf-vst3-owner-{slot}`) | The `!Send` plugin instance (clack main thread / VST3 component + controller), the `NativeIo` with BOTH `!Send` cpal streams, the editor host window + its Win32 pump, fault reporting | `request_rx` (CLAP ≤20 ms; VST3 ≤2 s idle; hosted editors poll at 20 ms), CLAP callback / restart / params-rescan requests, the `EditorClosed` flag, the cpal fault flags | Replies; `plugin:editor-closed`; `plugin:params-changed`; `plugin:stream-fault` (`poll_faults`); every ~2 s `mirror_diag` + `report_new_rt_faults` + (DEV) `emit_gate`. Spawns + joins the RT thread (`RtJoinGuard`, also on panic unwind); `deactivate` runs here after the join. A plugin-requested restart is serviced here too — CLAP `request_restart` (`service_restart`: join RT → deactivate → activate → respawn) and VST3 `restartComponent` cycle flags (`service_vst3_restart`: join RT, which ran `setProcessing(0)` → `setActive(0)` → `activate_component` → respawn); rings, the reconciled block and the learned hop-1 drift round-trip via `RtExit` / `Vst3RtExit` |
| **Per-slot RT** (`lf-clap-rt-{slot}` / `lf-vst3-rt-{slot}`) | The process loop: MMCSS Pro Audio, clocked by the capture callback's `InputWake` while an ASIO input is armed (`Hop1Pipe::pace_on_input`), otherwise paced by `PaceTimer`; drains ≤`MAX_EVENTS_PER_BLOCK` events, pulls mono from the input ring through `InPipe`, renders one D-block, sums to mono, `Hop1Pipe` resamples D→C into the SharedBuffer ring, `OutMonitorPipe` pushes wet into the monitor ring | Event ring (Consumer), input ring (Consumer), the JS-written header words + `BLOCK_CONFIG_GEN` / `input_gen` / `monitor_gen` (atomics) | Hop-1 ring, monitor ring (Producer), `ProducerDiag` atomics, latched `RtFault` bits |
| **cpal capture callback** (driver thread) | Nothing — `audio_input::open_input_stream` downmixes to mono and pushes into the input ring's Producer via `try_lock` (contended ⇒ that callback's frames drop, `input_overruns`++), then signals `InputWake`; the error callback latches `input_fault` | Device samples | Input ring |
| **cpal output callback** (driver thread) | Nothing — `audio_output` pops the monitor ring's Consumer via `try_lock`, applies monitor gain × master gain × the declick fade, duplicates mono across channels; starves count `monitor_starves`; the error callback latches `monitor_fault`. Under ASIO both callbacks ride ONE duplex driver | Monitor ring | Device |
| **Scan children** (`app.exe --scan-one <path>`) | One process per bundle inside a kill-on-close Job Object, 20 s timeout; two reader threads per child drain stdout/stderr with caps | Bundle path | Descriptor JSON |
| **Plugin GUI threads** | A floating CLAP editor runs the plugin's own window thread and signals `HostGuiImpl::closed` → the `EditorClosed` flag, acked on the owner thread. A hosted editor embeds into the owner-thread host window and is pumped there. VST3 `performEdit`/`restartComponent` (ONE component handler per load, set at load; an editor never sets its own) arrive on the owner thread: the event ring + `plugin:param-changed`, `plugin:params-changed` for value/title flags, or the `RestartFlags` atom (drained once per owner turn; a foreign-thread call is safe because it only touches atomics); CLAP `params.rescan` sets the main-thread flag the owner drains | User input | Flags / events |

- **Only the owner thread touches the instance, the cpal streams and the editor.** Command threads
  reach it through `OwnerRequest`; the RT thread through rings + atomics. Never `run_on_main_thread`
  a plugin call.
- **Only the UI thread makes WebView2 COM calls** (`with_webview`); a non-UI caller recovers the
  result over a channel.
- **Unload order is load-bearing:** `running=false` → `OwnerRequest::Wake` (a VST3 owner idles in a
  2 s `recv_timeout` that a store does not wake) → join the owner (which joins the RT thread) →
  `Close()` the SharedBuffer on the UI thread. Nothing may still write the mapping when it closes.
  Both halves log their timing (`VST3 teardown … ms: editor=… module=…`, `owner joined in … ms`),
  in release too: a slow unload names its step in the log.
- **The RT thread never logs, locks or allocates in steady state.** Failures latch `RtFault` bits;
  the one allowed allocation is a pipe rebuild on a generation bump, outside the rt_alloc guard.
- **Native cpal/ASIO ownership lives in the concrete `host::native_io::NativeIo`** — keep it
  there, not duplicated into CLAP/VST3 or behind a `PluginBackend` trait; plugin lifecycle is the
  one place the formats genuinely differ.

## Operational bits (recurring)

- **`crates/lf-engine` is the pure native engine** (`docs/plans/native-engine.md`, dormant until its
  flip). Its briefing is the crate doc in `crates/lf-engine/src/lib.rs`; the engine's device side is
  `src/engine_io` (briefing: its `mod.rs`), its plugin units and engine-mode owners
  `host/engine_slot.rs`, `host/clap_engine.rs`, `host/vst3_engine.rs`. Nothing on the live line calls
  them. `tauri dev` watches all of `src-tauri/`, so an engine edit relaunches a running dev app.
- **Parallel worktrees need their own `CARGO_TARGET_DIR`.** Sharing one, every worktree links the same
  `app_lib-<hash>` test binary and cargo judges path crates fresh by mtime, so one worktree can run
  another's build of `app` or `lf-engine` (seen 2026-09-25).
- **ASIO is a cargo OPT-IN feature** carried by the npm scripts (`pnpm dev:asio`, `pnpm build:app`);
  a plain `cargo build` must work without the LLVM/ASIO SDK, and `tauri dev` forces
  `--no-default-features` anyway.
- **Prod-realistic `tauri dev`:** `LF_FORCE_48K` default OFF ⇒ native rate; `=1` re-arms the 48k
  P9.4 drift rig (`$env:LF_FORCE_48K='1'` before the dev command). `[profile.dev.package."*"]
  opt-level=3`; `app`/`app_lib` stay opt-level=0.
- **ASIO SDK env** (`LIBCLANG_PATH`, `CPAL_ASIO_DIR`) is set PERMANENTLY in the User-scope env on
  the dev PC — a fresh shell inherits it, no inline setting needed (`pnpm dev:asio` just works).
  `CPAL_ASIO_DIR` must hold an EXTRACTED SDK with `common/` and `host/pc/` directly under it; `asio-sys`
  only rebuilds when its fingerprint changes (a `cargo update`, a crate bump), so a green
  `cargo check --features asio` can be a stale `target/` cache over a missing SDK — `pnpm rust:check`
  confirms the SDK directory before its asio step. `cpal` 0.18.2 moved to
  `asio-sys` 0.4; cpal stays pinned at 0.18.1 until that pair is built and heard on the rig.
- **PROD-EXE recipe:** raw `cargo build --release` = a DEV-mode binary (wants devUrl). The real exe
  = `pnpm build:app`; launch DIRECTLY (`Start-Process app.exe`), never via stdout-redirect.
- **Release IPC surface:** `capabilities/default.json` grants only event listen/unlisten. `diag` and
  plugin state save/load are registered only under `debug_assertions` until production save/recall
  ships; keep the handler cfg and frontend `import.meta.env.DEV` surface in lockstep.
- **Sample-rate selector "C2" — DECIDED (owner), NOT BUILT:** swappable 44.1/48k, default device
  native; RETIRES the `LF_FORCE_48K` dev hack (keep a 48k force for the P9.4 gate). Separate Rust
  increment.
- **Editor-hang Win32 gotcha (recurring):** a host window Win32-OWNED across threads deadlocks on
  close (sync cross-thread activation `SendMessage` vs a stopped pump). Fix lives in
  `host/editor_window.rs`: owner-LESS window + `drain_after_editor_teardown()` +
  `show_host_window_front()` one-shot `HWND_TOP` (no `WS_EX_TOPMOST`).
- **Timed Win32 waits round up to the timer tick Windows grants the process**, and that tick can be
  15.6 ms while the global resolution reads 1 ms, even after the app's own `timeBeginPeriod(1)`.
  Bound a wait loop by a deadline, never a round count (`drain_after_editor_teardown`); pace RT work
  with the high-res waitable timer (`PaceTimer`). A plugin's own waits stretch the same way; the
  measured numbers sit with the native baselines in `docs/VERIFY.md`.
- **Release logging:** `tauri-plugin-log` registers UNCONDITIONALLY → Stdout (the dev grep
  convention) + a rotated file at `%LOCALAPPDATA%\com.bleeploop.app\logs\bleeploop.log` (2 MB,
  KeepAll). The Rust panic hook chains the default hook and logs location+payload.
- **The CSP (`tauri.conf.json` `security.csp`) applies to BUILT apps only** — `tauri dev` serves from
  vite and is not covered, so a new asset origin, a CDN font or an `eval` breaks in release alone.
  Violations reach the release log as `[csp]` lines (`src/platform/logging.ts`); after adding a new
  kind of resource, run `pnpm build:app` once and grep that log. `style-src` is exempt from Tauri's
  nonce injection on purpose (a nonce would void `'unsafe-inline'`, which Solid's template styles need).
- **RT threads never log.** They latch `RtFault` bits in `ProducerDiag`; the owner thread reports
  each category once per plugin load. Add new RT failure paths to that latch, not `log::*`.

## Native-host verify ops

The out-of-process plugin scan is testable WITHOUT the full app — `cargo build` then
`target/debug/app.exe --scan-one "<plugin path>"` prints the descriptor JSON and exits (the
`--scan-one` dispatch runs before Tauri starts). The scan spawns one child per `.clap`/`.vst3` for
crash + hang isolation (**20s per-child timeout** — a heavy/licensed VST3 like Neural DSP can hang
on load in the headless child) — but only for bundles whose binary size/mtime changed since the
cache (plugin-scan.json beside the release log dir under %LOCALAPPDATA%) last saw them (a launch spawns
nothing; a remembered failure is retried only by the picker's rescan button = `plugin_scan`
`force`). Delete that file to force a cold scan from outside the app. Pipe retention is capped at 1 MiB stdout / 64 KiB stderr, and each
child process tree sits in a kill-on-close Job Object. DEV CLI tools also exist: `--probe-asio` /
`--probe-asio-duplex`.
Stale `<old-path>\rc500\…` build path on dev start → `rm -rf src-tauri/target/debug/build`.
(Driving and grepping a running `tauri dev`, and stopping it: `docs/VERIFY.md`.)

## CLAP host + the audio transport (P9)

All in `src-tauri/src/host/` (commands/scan/state/rt_alloc/editor_window/transport/clap/vst3 —
`transport.rs` loses its WebView bridge at the native engine's flip and its pipes move to the
engine's device module: `docs/plans/native-engine.md`).
- **Cross-process audio = WebView2 `CreateSharedBuffer` + `PostSharedBufferToScript`** (via `with_webview`).
  The COM object is parked as a raw owning pointer (`SharedBufferHandle`, `transport.rs`) between the two
  UI-thread hops — never an `AgileReference`: this interface has no proxy, so the wrap fails on every load.
  The buffer surfaces in JS as a **regular ArrayBuffer, NOT a SAB, and `Atomics` are UNSUPPORTED on it in
  Chromium 149** → one ring read with plain ordered loads (x86-64 TSO + Rust-side `AtomicU32::from_ptr`
  release/acquire). JS transfers the buffer to the `plugin-pcm-source` worklet, which reads it on the render
  thread (→ `engine.recordTap`/looper bus) and writes the consumer header words; its queue policy is in
  the worklet's header. `chrome.webview` stays in `host.tauri.ts`; `audio/plugin-bridge.ts` is
  WebView2-agnostic.
- **Every WebView document gets a `frontendEpoch` from `host_init`.** Native slots reserve an explicit
  `Loading` state before foreign setup, and every posted buffer carries that epoch. On reload, stale
  reservations are cancelled; a late owner must tear down instead of parking, and JS releases its buffer.
- **`!Send` `PluginInstance` (clack) lives on a dedicated per-slot owner thread**, NOT `tauri::State`; only
  the `Send` `Stopped` processor crosses to the RT thread + back. State holds `Send+Sync` control handles.
- **RT thread is alloc-free** (`rt_allocs:0`, measured by a DEV `#[global_allocator]` shim): pre-grown
  event buffers; notes/params arrive over one main→audio `rtrb` event ring.
- **Drift control** (`Hop1Pipe`: resampler + `DriftController` + `PaceTimer`, shared by CLAP+VST3): pace the
  producer with **absolute-deadline accumulation + a high-res waitable timer** (`CreateWaitableTimerExW` +
  `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) — a relative `thread::sleep(period−elapsed)` rounds up to the
  Windows timer tick → producer runs slow → crackle. The PC's QPC↔sound-card mismatch is **~400ppm**; the
  loop cancels it. **0 underruns is the definitive sync proof**, not the drift proxy. An armed ASIO input
  replaces the timer with the interface clock (`pace_on_input`), so the input and monitor rings need no
  controller; WASAPI keeps the timer (late capture callbacks ran the producer ~5 % fast). The hop-1
  controller's level is the worklet's smoothed queue; the producer bumps header word 7 where production
  jumps, and the worklet settles the queue back on its setpoint instead of feeding the step to the loop.
- **Surge param ids are hash-like, NOT 0-based** (`first=825615485`); sending an unknown id CRASHES the
  plugin → always enumerate via `listParams`, never invent an id.
- Test plugin **Surge XT** (`winget install SurgeSynth.SurgeXT` → `…\CLAP\Surge Synth Team\`, + Surge XT
  Effects); scan walks `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`, `CLAP_PATH`.
  Loader is `clack_host::entry::PluginEntry::load` (unsafe), NOT `PluginBundle`.

## Plugin editors + the VST3 host (P10)

The known-fragile area: read this whole section before any plugin-GUI/VST3 work.
- **Plugin editors embed into a host-owned top-level Win32 window** (`CreateWindowExW`, owned by the main
  window — NOT reparented into the WebView2 surface). The owner thread runs a Win32 message pump
  (`PeekMessage` + `MsgWaitForMultipleObjectsEx(20ms)`) ONLY while a hosted editor is open. GUI calls go
  via the owner channel, NOT `run_on_main_thread`.
- **Editor size is the plugin's, measured not computed:** `editor_window::set_client_size` sizes the
  CLIENT area by measuring the real frame (DPI-correct), at creation and on every plugin-initiated
  resize — VST3 `IPlugFrame::resizeView` (then `onSize` with the granted size) and hosted-CLAP
  `request_resize` both land there. Never answer a resize `kResultOk`/`Ok` without resizing; the view
  lays out for the size you confirm. Fixtures: `host/vst3_resize_fixture.rs`, `clap::resize_tests`.
- **Crate `vst3` 0.3.0** (coupler-rs; only dep `com-scrape-types`, no `windows`/`windows-core` conflict).
  `ComPtr<IAudioProcessor>` is already `Send+Sync`. Source-verify against the crate source
  (`~/.cargo/registry/src/index.crates.io-*/vst3-0.3.0/src/bindings.rs`), NOT web docs; methods are
  **camelCase**; `kResultTrue == kResultOk == 0` (compare `== kResultTrue`, never as a bool).
- **Teardown order:** `setProcessing(0)`→join→`setActive(0)`→`terminate`→drop COM objs→drop
  `Vst3Module` LAST. Its RAII drop pairs successful `InitDll` with `ExitDll`, then calls
  `FreeLibrary` (any plugin-DLL `ComPtr` must drop first — its vtbl lives in the module). The shared
  `RtJoinGuard` also stop+joins during owner-stack unwind; never replace it with a bare `JoinHandle`.
- **Surge XT VST3 is SEPARATED-component** (`component.cast::<IEditController>()` is None) → `obtain_controller`
  (`getControllerClassId`→`createInstance`→`initialize`); a JUCE separated controller's `createView` returns
  null until it gets the in-process `AudioProcessor` pointer over a connection-point `notify(IMessage)` (host
  `LfMessage`/`LfAttributeList` + `IConnectionPoint` cross-connect).
- Scanner handles BOTH VST3 forms: single-file `.vst3` AND folder bundle `Contents/x86_64-win/<inner>.vst3`;
  descriptor `id` = the class TUID hex. `host/vst3.rs` stays a child mod OF `host/clap.rs` (`#[path]` decl, so
  `vst3::Steinberg` imports don't collide with clack and `super::` keeps meaning); `unload`/`note_on/off`/
  event-ring/bridge are format-agnostic + shared.
- **Deferred past P10:** `setComponentState`/VST3 save-load state (IBStream MemStream, by-eye-gated).

## Native audio input → wet monitoring, ASIO (P11)

Guitar → cpal input stream (owner thread, `!Send`) → `rtrb` ring → RT loop → plugin input bus → wet,
split two ways: branch 1 = native cpal-out (the low-latency monitor, one clock); branch 2 = the
existing P9 ring → looper record tap (lag-tolerant, records wet "for free").
- **VST3 input:** query `getBusInfo` AFTER `setBusArrangements`, size buffers to the reported channel count,
  don't assume the requested arrangement (same crash class as the Surge hash-param id). Guard zero-input
  plugins (synths) so both slots stay unregressed. `activate_component` (`host/vst3.rs`) is the ONE owner
  of that sequence — load and every plugin-requested restart run it, and each RT spawn takes the
  `Activation` it produced, so a `kIoChanged` that changes a count resizes the next producer's buffers.
  Only the input-bus PRESENCE is frozen at load (NativeIo arming + the web UI's synth/effect kind); a
  flip across a restart is logged, not rebuilt.
- **Host-set VST3 params go to BOTH halves:** the ring feeds the processor, `OwnerRequest::
  SetParamNormalized` feeds the edit controller on the owner thread (`clap::set_param` is the one
  entry). The controller is what the plugin GUI shows and what raises a controller-decided
  `restartComponent` (FabFilter latency modes) — drop the mirror and no drawer change can ever
  restart a plugin again. `performEdit` (GUI → host) is never mirrored back. A 30-plugin
  restart survey backs this: FabFilter raises `kLatencyChanged`, Neural DSP and Surge never do.
- **`InPipe`** (rubato `FixedAsync::Output`, cpal `R_in`→render `D`) + a `DriftController` on the cpal-ring
  fill (off while the capture clocks the producer) resamples the capture; `R_in` plumbed owner→RT via `diag.input_rate` + an `input_gen` counter (bump
  on every arm/disarm so the RT rebuilds + flushes the ring). `InPipe::new` (re)build sits OUTSIDE the
  rt_alloc guard (a one-shot non-perf-moment alloc, absorbed by the ~30ms bridge queue).
- **ASIO tier** (`--features asio`): routes BOTH capture + monitor through ONE full-duplex driver, ONE clock.
  cpal ASIO = ONE driver per device: once a stream holds it, cpal can't re-resolve the device or re-query
  configs → the duplex Device + configs are resolved ONCE per process (`audio_output::resolve_asio_cache`
  behind the `asio_startup.rs` coordinator; `cpal::Device` is Send+Sync, so the cache is a static).
  Both streams build from the cache. **Never call the resolver from `run()`:** it loads the driver DLL
  in-process; the frontend requests it after the UI is up (the **ASIO startup** paragraph in `docs/ARCHITECTURE.md` § Audio architecture). **ASIO `Stream::drop` only removes
  callbacks** (never `driver.stop`, never tears down `asio_streams`) → `host::native_io::NativeIo` keeps
  both streams alive across disarm (output plays silence → no drone; re-arm makes ZERO cpal calls → no
  BadMode). Each retained stream stores its actual backend. A backend transition is allowed only while both
  directions are logically disarmed, drops BOTH retained streams before releasing the process-level
  `ASIO_DUPLEX_HOLDER`, then rebuilds on the requested backend. Rearming a retained input updates its
  channel selection under the capture-producer mutex before publishing `input_gen`; callback selection
  reads belong inside the same lock. Channel changes preserve the driver streams.
  ASIO uses the cached default driver, not the WASAPI device IDs. Settings display its cached name and
  channel counts; Windows device picks remain saved for WASAPI.
- **ASIO timestamps:** retain cpal's per-package `overflow-checks=false` in Cargo.toml for its wrapped
  epoch conversion. Output latency uses the checked playback-minus-callback duration from the SAME
  callback, never the absolute epoch.
- **A native latency estimator was tried and rejected:** the native duration was sampled right after
  publishing a source tail, while the browser read its latest tail at a later, arbitrary main-thread
  time, so the two observations never described the same tail. A median or a seqlock does not repair
  that. A future native estimator must carry a common source position AND its timestamp across both
  observations, then prove the relationship on the marker path (`docs/VERIFY.md`).
- **Test-rig gotcha:** the ASIO probe is a frontend-requested command, so a `tauri dev` log shows
  `[asio] probe starting` → `cached ASIO "…"` → `[asio] probe result … Ready` AFTER `host_init`, and
  nothing ASIO-related before it; `pnpm dev:asio -- -- --disable-asio` shows `DisabledByFlag` and no
  probe. Read `plugin_asio_device_info` for cached metadata and confirm the opened backend through the
  goLive log. Boot applies saved buffer and ASIO preferences, then awaits the probe, before enabling
  plugin selection or scanning (the load-time block cap keys on `asio_available()`). The DEV
  `--probe-asio` CLI DOES load every driver (cpal enumeration initialises them): not a harmless check.
- **Audio Settings + buffer size:** a global Audio Settings popover (topbar gear) holds the input/channel/
  output device pickers (arm toggles stay per-slot) + a live buffer-size dropdown (64/128/256/512/1024). The
  CLAP+VST3 RT loops watch process-global `BLOCK_CONFIG_GEN` and re-pace to the new D-block with NO plugin
  reload (`Hop1Pipe::rebuild_for_block` preserves `write_frames`; `DriftController::set_block` keeps the
  learned `integ`). Frontend buffer/driver writes serialize and persist only after acknowledgement;
  global device `<select>`s aren't disabled while a slot is armed (change applies next arm — an owner design call).

## Open threads (no gate)

- `plugin_note_off` / `plugin_set_param` now answer `Err` on a full event ring or an unlisted param id
  (audit B2–B4); the frontend still `void`s them, so a rejection only reaches the release log. A UI
  reaction (slider snaps back to the plugin's value, a stuck-note cue) is unbuilt.
- GO LIVE under ASIO timed out in `arm_monitor` ("timed out waiting on channel", then `disarm_input`
  timed out too) on two of about twelve `pnpm native:loopback` launches on 2026-09-24; a relaunch
  passed. Cause unknown.
- The CLAP and VST3 restart fixtures' `plugin_requested_restart_cycles_activation_on_the_owner_without_reload`
  fails under machine load (254 to 1792 hop-1 frames dropped; 3 of 30 runs on 2026-09-25 with other
  builds running). The test stops reading the ring from its last caught-up wait until after the
  respawn, while the producer keeps writing into ~10 ms of slack: a fixture defect, not the host's
  (the fix: drain in every wait).
- CLAP and VST3 duplicate the load choreography (`load`/`vst3_load` and both owner mains): one
  shared owner would keep B1/B11 from returning. Not built.

