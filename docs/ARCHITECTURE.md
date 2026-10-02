# BleepLoop architecture

A standalone Windows desktop app for instant musical jamming: an **instrument host** (two native
plugin slots + six built-in synths) sitting over an **RC-505 MK II–style 5-track looper**, all in
**one native audio engine**. Built on **Tauri v2**: Rust for the engine and the plugin hosts, a
WebView2 frontend for the UI.

## The load-bearing idea: one engine, a UI that only asks

Every sample is the engine's: the click, the looper, the synths, the FX, the mixer, the limiter and
the two plugin slots run in ONE device callback, clocked by the audio interface
(`src-tauri/crates/lf-engine`, pure; its device side `src-tauri/src/engine_io`). The WebView is the
UI: it sends commands (a batch per gesture, `engine_send`) and reads a feed (~60 frames/s: transport,
lanes, the grid anchor, the meter, waveform peaks; never PCM). The wire is
`src-tauri/src/engine_io/wire.rs`, mirrored in `src/platform/engine-wire.ts`. Settings, rig recall
and MIDI-learn bindings stay in the WebView's storage and are mirrored to native at boot; the engine
host replays every remembered setting into each new engine (`engine_io/settings.rs`).

`src/platform/` is the **only** place allowed to import `@tauri-apps/*` (enforced by
`scripts/check-boundary.mjs`, run via `pnpm check:boundary`). `ui/`, `session/` and `app/` depend on
its interfaces, never the reverse (`src/platform/host.ts`):

| Interface | Web impl | Tauri impl |
|---|---|---|
| `EngineHost` | a scriptable fake: records every send, emits the frames a probe scripts; not a second looper | `invoke()` + a Tauri Channel → the engine host |
| `PluginHost` | stub: `available=false` | `invoke()`/`listen()` → the CLAP/VST3 hosts |
| `MidiBackend` | `navigator.requestMIDIAccess` | **the same web impl, reused verbatim** |
| `LogFolder` | none | the release log's folder (Help's diagnostics) |
| `AppUpdates` | none, or a probe's script | the updater (`src-tauri/src/update.rs`), release builds only |

MIDI arrives through Web MIDI: WebView2 has it natively (`lib.rs` auto-grants the MIDI permission to
the app's own origin). The engine's native MIDI (midir, `engine_io/midi`) is built and off: WinMM
input ports are exclusive, so Web MIDI and native MIDI cannot hold one controller at once.

The browser build (`pnpm dev`) renders the whole UI and is silent: no engine is behind it, and nothing
in the app builds an AudioContext. The DEV engine fake (`window.__lfEngineFake`) is the seam every
browser probe drives.

**Live audio NEVER crosses this boundary as PCM.** A session save's snapshot does, once, off the RT
path (`EngineHost.snapshot` / `loadSession`).

## Stack

- **SolidJS 1.9 + TypeScript + Vite (8, rolldown) + pnpm.** Fine-grained signals, ~7-8 KB
  runtime, no VDOM. The 60 fps canvas hot path **bypasses framework reactivity** entirely
  (one `requestAnimationFrame` loop reading a plain mutable state object); signals are
  reserved for low-frequency chrome (transport mode, track LEDs, tempo, FX labels).
- **Rust:** lf-engine (rtrb rings; the synths, FX, reverb and limiter ported from Tone 15.1.22 on
  Blink and null-tested against its renders, `src-tauri/crates/lf-engine/src/dsp/mod.rs`; RustFFT),
  cpal 0.18.1 (pinned; ASIO a cargo opt-in feature), clack-host for CLAP and `vst3` for VST3.

## Audio architecture

The bus topology is `src-tauri/crates/lf-engine/src/engine.rs`'s header; the engine's rules are its
crate briefing (`src-tauri/crates/lf-engine/src/lib.rs`); the device side's are
`src-tauri/src/engine_io/mod.rs`. In short:

- **Master bus:** each lane through its FX chain, the shared reverb bus, the instruments (built-in
  or a plugin slot's) and the click, × master volume → the master limiter → out.
- **Live input:** each slot's input → the slot while it is live (an effect, or dry when empty) → the
  wet signal; the input sends (ECHO, REVERB, RING MOD) render wet only from it. The monitor (wet +
  sends) joins the output after the limiter under the same master volume: unlimited, and without the
  limiter's pre-delay. The record tap is the wet signal, aligned, plus the sends and the instruments.

- **One clock, no trim:** a lane plays loop position `(f - anchor) mod master` at device frame `f`;
  a take starts the driver's reported input + output latency (+ the largest live effect's latency +
  the limiter's pre-delay) after its downbeat. There is no user-facing record trim and no loopback
  calibration wizard (D18).
- **Frame-identical lanes by construction:** every committed lane is one master long, in integer
  frames; a later take longer than the master multiplies it and the other lanes tile out to it. State
  per lane: EMPTY → RECORDING → PLAYING ⇄ OVERDUBBING, plus STOPPED.
- **FX (per lane, fixed order, each bypassable):** Filter → Pitch → Stutter → Delay, with a reverb
  send into one shared reverb bus. Stutter phase and delay divisions follow the beat grid's origin (set
  by the first take, an import or an idle restart), which a multiply leaves in place.
- **END STOP** (optional): a stop waits for the next master-loop boundary; a second press stops at
  once. **RETAKE** (optional): a take whose length is known at arm slides its window one pass forward
  at each window end instead of committing; the stop keeps the last complete pass, and REC on another
  lane approves it and hands the recorder over on the pass edge.

**Output headroom:** the master limiter is a literal port of the Web Audio compressor the app used
before (E6), a finite-ratio compressor, not a guaranteed 0 dBFS ceiling: an offline 48 kHz render of a
440 Hz sine at amplitude 5 peaked at 1.116 after it. Summed lanes can therefore clip at the sink; gain
staging remains necessary, and a true ceiling needs an explicit distortion/latency choice and another
latency measurement. The live wet signal joins after the limiter and is not limited.

**Session files** (`src/session/`): export writes a zip of Float32 WAV stems, a PCM16 wet master and
`session.json`; import takes one back while every lane is EMPTY. The PCM comes from `engine_snapshot`
and goes back through `engine_load_session` (the bytes: `src/platform/engine-wire.ts`). The wet master
is the engine's: an export's snapshot asks for it, and the host renders it right after the copy, off
the audio thread, in a fresh engine the session's size (`src-tauri/crates/lf-engine/src/render.rs`)
from those same loops and the mix the host keeps (lane volume, mute and FX, master volume and mute),
every lane playing, frame 0 lined up with the stems. A failed render still exports, with a dry
mixdown (`master.kind` 'dry-fallback'); recovery autosaves never ask for a master.

**Session recovery:** committed track audio, mix settings and PLAYING/STOPPED state round-trip through
the archive. Legacy missing state and OVERDUBBING restore as PLAYING. Autosave saves once the committed
loops have held still, also while a take records or a layer sums (a lane mid-overdub is saved as its
loop before the layer); an abrupt crash inside that window can lose the latest committed change. The
engine cannot resample, so recovery keeps one jam per sample rate and a launch restores its own
rate's; only the player's clear that emptied the looper deletes one (rules: the header of
`src/session/autosave.ts`). Recovery starts once a device runs. Orderly native close flushes before
exit. Recovery is not a synchronous durability guarantee.

**Tone recall:** a tone is a plugin's saved state, one per slot and plugin identity
(format, path, id), so the same plugin in both slots keeps two tones; owned by
`src-tauri/src/host/tone.rs`, restored only inside a load, before activation. A load that
could not restore its tone never writes the plugin's defaults over it until the player changes
something; a plugin that refuses is created again, so it runs at its real defaults. A session import is
checked against the plugin session.json names and written under the store's lock; no earlier load of
that plugin in that slot stores over it.

**ASIO startup:** resolving the ASIO device loads and initialises the third-party driver DLL
in-process (asio-sys → `CoCreateInstance` + `ASIOInit`), and a broken driver hangs or crashes there
with no in-process remedy (a timeout bounds only the waiter; the driver keeps asio-sys' global lock
and possibly the loader lock). So `run()` never contacts the driver. `src-tauri/src/asio_startup.rs`
probes the driver at startup, requested by the frontend after the window is up
(`initAudioDeviceSettings` → `plugin_asio_probe`) and only when the saved preference is on, and again
only for a driver switch while nothing holds the driver; a saved "off" never asks. A sentinel file in
the app's local data dir marks an attempt in progress; found at the next launch it blocks the automatic probe until the user presses RETRY ASIO in Audio Settings. A
timed-out probe is never retried in the same process (restart). `app.exe --disable-asio` skips it for
that launch whatever the preference says. What this does NOT promise: that the app survives a driver
that crashes when the user later starts it, and the same in-process load happens again at the first
ASIO stream build. Upstream note: asio-sys 0.3 passes an uninitialised `ASIODriverInfo.sysRef`
(the SDK's application window handle) to the driver's `init`; a driver that uses it sees an
indeterminate value.

## Cross-cutting invariants (do not violate)

**This numbered list is THE numbering.** Source comments cite these by number ("invariant 6"), and
`AGENTS.md` carries the same titles in the same order. If the two diverge, every in-code citation
silently points at the wrong rule. `verify/guards/docs.mjs` fails when they do.

1. **The engine's device-frame clock is the single tempo/quantization authority.** Every grid-timed
   event (a command, a beat, a loop boundary, a take's window) lands on an absolute frame of the output
   callback's counter; nothing in the WebView keeps time. The UI extrapolates the playhead from the
   feed's anchor for display only.
2. **Commands and events cross the RT boundary only through rtrb rings.** A full event ring drops and
   counts; a full command table leaves the rest in the ring for the next block. Nothing blocks.
3. **The engine alone owns musical state: the UI sends commands and reads the feed.** A command is
   judged when it lands, on the engine's state, never on what the UI last saw; the feed carries the
   outcome. Live audio never crosses the platform boundary as PCM (a session save's snapshot does,
   once, off the RT path).
4. **Plugin lifecycle runs off the RT thread, with the slot bypassed.** Load, activate, restart and
   teardown run on the slot's owner thread; the callback crossfades a unit in and out and hands it
   back on a ring, never drops one (a drop frees memory and calls into the plugin's DLL). An effect
   slot passes dry meanwhile, an instrument slot is silent; loops and click never wait.
5. **The RT path (engine callback, plugin process) never allocates, logs, blocks or waits on a
   lock.** The callback only `try_lock`s the engine (a miss plays silence and counts); buffers are
   allocated and touched before the first callback; failures latch counters and fault bits that a
   non-RT thread reports. Tests run every `process` under `assert_no_alloc`; in DEV builds the device
   callback counts any allocation (`src-tauri/src/host/rt_alloc.rs`).
6. **No Solid signal WRITES from audio-path timers, and no signal READS in the 60 fps draw loop.**
   rAF + a plain mutable object only. A capture drain writing a fresh object into a track signal 40×/s
   once cost ~200 full-document layout events per 5 s of recording. The pattern: the feed handler
   writes one plain mirror and writes a signal only when its value changed; the waveform rAF reads the
   mirror (`src/ui/state/engine-store.ts`).
7. **All `@tauri-apps/*` confined to `src/platform/`.** CI-guarded.

## Decided: one native audio engine (2026-09-24)

Replaced "the looper stays in Web Audio". Click, looper, synths, FX, mixer, limiter and plugin
processing run in ONE Rust engine clocked by the audio device; the WebView is UI only and reads a
state feed, never PCM. Shipped as the default in v0.1.0; the Web Audio path was deleted after the
engine lap.

Why the earlier rejection no longer held:

- The synths and FX moved too, null-tested against reference renders of the Tone code, so no
  compensation moved to a synth ingress.
- No `AudioContext` has to be clocked from outside: the engine owns the device callback.
- The looper state machine has one owner in Rust and is verified offline by `cargo test -p lf-engine`
  (scripted renders, the old rig-guard scenarios as spec, a golden-jam port), not by ear.
- The pain was measured: through the loopback cable a Web Audio take landed ~65 ms late at trim 0,
  spread per launch and drifted 0.7–3.2 ms/min inside a take. It came from two clocks and the
  WebView's unreported output latency; one clock removes both, and the wet master is one stream that
  can be shared.

What remains is the driver's own report: alignment takes the device's reported input + output
latency, and a driver can under-report its converters. What stays true: on an arbitrary Windows
machine (WASAPI-shared, no ASIO) absolute latency is high and its report cannot be trusted.

### Measured premise

Measured before any engine code (the premise spike: `pnpm native:spike`, `app.exe
--probe-engine-spike` and `--probe-share`, `src-tauri/src/host/engine_spike.rs`,
`src-tauri/src/share_probe.rs`), then on the engine's own open path (`app.exe --probe-engine asio 128
--lag`). Rig: the dev PC, Scarlett 2i2 3rd gen at 44.1 kHz, a cable from line out R into input 2; a
64-frame chirp played out and captured on one frame counter, cross-correlated offline. A virtual cable
cannot stand in: it measures Windows' buffering, not the driver's report.

| Bar | Result on the rig |
|---|---|
| One callback (ASIO): input and output in one bufferSwitch on every cycle | holds at 64/128/256 |
| The driver's report alone puts a take on the grid: \|lag − (inLat + outLat)\| ≤ 1 ms, dry input | within 0.1 ms at 64/128/256, once the driver is opened at another block size first; reopened at the size it last ran, it lands about two periods late (`src-tauri/src/engine_io/cpal_driver.rs`) |
| One clock, stable across launches: spread ≤ 1 frame per run | holds; between sessions the landing moved 5 frames at 128 |
| Nothing drifts inside a take: ≤ 1 frame over 10 min | 0.000 at 128 and 256, +0.9 frames at 64 |
| No hidden buffering on the in-callback plugin path: round trip = lag + plugin latency | holds (Pro-Q 3, zero-latency mode) |
| Round trip at most half the Web Audio path's 44.4 ms at 256 | 8.1 ms at 64, 15.1 ms at 128, 26.8 ms at 256 (fails at 256 on this driver's report; accepted by the owner: 256 is the everyday DAW setting) |
| An amp-sim leaves room: 120 s at 128 and 256, 0 gaps, 0 xruns, 0 allocs, block p99.9 ≤ 50 % | Archetype Petrucci X: p99.9 21 % and 19 %, max 28 % and 21 % of the period |
| WASAPI takes align from timestamps | no: on the Focusrite WDM driver takes land +211 to +229 ms late against the engine's align. ~35–44 ms is an endpoint clock term cpal's stamps miss; the rest sits in the driver, which reports none of it, and a per-device constant would miss by ±9 ms between launches. WASAPI takes are documented as unaligned on such drivers; ASIO is the play path (decision E9) |
| A muted mirror is capturable (Share output) | no: process loopback captures after the session's mute and volume, so Share output goes to a user-picked endpoint (decision E2) |

**The measurement gate.** Replacing native monitoring or changing its buffering targets requires
both, before and after the change:

- **L1, the rig protocol:** the alignment, spread and drift bars above at 64/128/256 (no trim).
- **L2, a physical loopback measurement:** play the click out, capture it through the working
  guitar input, cross-correlate scheduled against heard: the premise spike, and `pnpm
  native:engine-loopback` for a take in the running app (baseline: `docs/VERIFY.md`).

## Known fragile piece: plugin editors

Plugin **GUI embedding** inside the WebView2 window is hard: WebView2 is always
top-most within its window (the "airspace" problem), so a child plugin HWND z-fights it.
**Editors use separate top-level OS windows.** CLAP can use a plugin-owned floating window or embed
into a host-owned top-level window; VST3 embeds into a host-owned top-level window. This is not a
panel inside the WebView. Editor requests run on the per-slot owner thread, which also pumps hosted
window messages. Native thread ownership is defined in `src-tauri/AGENTS.md`:
"Only a slot's owner thread touches its plugin instance and editor."

VST3 hosting is hand-written unsafe COM over `coupler-rs/vst3` (Rust has no turn-key VST3 host
crate); CLAP goes through `clack-host`. VST2 is not hosted (`vst-rs` is archived).
