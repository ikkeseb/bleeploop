# BleepLoop — Architecture

A standalone Windows desktop app for instant musical jamming: an **instrument host** (two
native-VST slots + six built-in Web Audio synths) sitting over an **RC-505 MK II–style
5-track looper**. Built on **Tauri v2** (Rust backend + web frontend).

## The load-bearing idea: a thin capability boundary

Native VST hosting is the only thing that *forces* Tauri/Rust — everything else (synths,
looper, FX, MIDI, keyboard, waveforms) is pure Web Audio / TypeScript. So the whole
frontend is built and verified **standalone in a browser** (`pnpm dev`, port 1420) with
**zero compile-time or runtime dependency on Tauri or Rust**, and the native shell is
layered on later without a frontend rewrite.

`src/platform/` is the **only** place allowed to import `@tauri-apps/*` (enforced by
`scripts/check-boundary.mjs`, run via `pnpm check:boundary`). It exposes three interfaces
that `audio/` and `ui/` depend on — never the reverse:

| Interface | Web impl | Tauri impl |
|---|---|---|
| `PluginHost` | stub: `available=false`, six synths fill both slots | `invoke()`/`listen()` → Rust CLAP/VST3 host |
| `AudioInputSource` | `getUserMedia` → MediaStreamAudioSourceNode | **the same web impl, reused verbatim** |
| `MidiBackend` | `navigator.requestMIDIAccess` (Chromium/Edge native) | **the same web impl, reused verbatim** |

Three interfaces are declared; in practice only **`PluginHost`** differs. `tauriPlatform` is literally
`{ ...webPlatform, kind: 'tauri', pluginHost: tauriPluginHost }` — WebView2 v149 has native Web MIDI
(`lib.rs` auto-grants the MIDI/microphone permission kinds to the app's own origin) and `getUserMedia` works inside it, so neither needed a native
path. An earlier version of this table promised a `tauri-plugin-midi` shim over `midir`; it was never
built and neither crate is in `Cargo.toml`. (Corrected 2026-07-25 — the claim had also been copied
into code comments in `src/platform/host.ts`.)

Runtime selection: `isTauri() ? tauriPlatform : webPlatform`. `@tauri-apps/api` is pure JS,
so installing it now is safe — `isTauri()` simply returns false in a browser.

**Audio buffers NEVER cross this boundary as PCM.** Native VST audio reaches the Web Audio
graph only as an *AudioNode* (MediaStream or a SharedArrayBuffer ring), never as
IPC-serialized samples (latency/jitter/drift would be unacceptable).

## Stack

- **SolidJS 1.9 + TypeScript + Vite (8, rolldown) + pnpm.** Fine-grained signals, ~7-8 KB
  runtime, no VDOM. The 60 fps canvas hot path **bypasses framework reactivity** entirely
  (one `requestAnimationFrame` loop reading a plain mutable state object); signals are
  reserved for low-frequency chrome (transport mode, track LEDs, tempo, FX labels).
- **Tone.js 15.1.22** under a thin `AudioEngine` facade — synth voices, the
  "Tale of Two Clocks" lookahead transport, and FX classes, on *our* shared AudioContext
  (not lock-in; raw nodes interop via `Tone.getContext().rawContext`).
  Built-in poly synths own bounded reusable voice pools so release tails cannot drop new attacks.
  The mono bass uses last-held-note priority. Input ownership distinguishes MIDI ports/channels and
  individual pointers; releasing one owner cannot stop another owner's held note.
- **ringbuf.js 0.4.0** — wait-free SPSC SharedArrayBuffer ring for AudioWorklet→main PCM.

## Audio architecture

Wet export builds a separate `OfflineContext` and passes it explicitly to every FX node.
It never swaps Tone's global context across an async operation. Editable stems use Float32 WAV;
the flattened stereo master remains PCM16.

One shared `AudioContext({ latencyHint: 'interactive' })`, created lazily on first user
gesture; `Tone.setContext(ctx)` is called **before any Tone node** (asserted in
`engine.ts`). Master graph:

```
synths (Tone) ── instrumentBus ─┐
mic/line (getUserMedia) ────────┴─ looperInputBus ─┬─ masterGain ─ limiter ─ destination  (audible)
                                                  └─ recordTap   (SILENT record-only mirror)

plugin worklet ─ gain ─┬─ recordTap                         (record; always full)
                        └─ webMonitorGain ─ masterGain       (audible; muted for native monitor)
```

Three details the old version of this diagram got wrong, all load-bearing: `recordTap` is a
**separate silent branch** off `looperInputBus` (not an annotation on the audible edge) and is where
the looper captures; a hard-knee `DynamicsCompressor` **limiter** sits between `masterGain` and
`destination`, so loops are captured pre-limiter (clean) while playback stays protected; and plugin
wet does **not** join `instrumentBus` — it connects straight to `recordTap` for record plus a
separately-muteable `webMonitorGain → masterGain` for audible, which is how arming the native monitor
silences the web path without touching the record tap. (Corrected 2026-07-25.)

**Limiter discipline:** loops are captured pre-limiter (clean), playback is protected. Per-synth
gain staging upstream is the real headroom; the limiter is the net. Per-track looper volume
(`looper.setVolume`, mute via `looper.setMute`) sits upstream of it too.

- **Looper capture:** getUserMedia (EC/NS/AGC off) → selected input channel → centred mono →
  `capture` AudioWorkletNode whose `process()` publishes each 128-frame quantum WITH its absolute
  render-frame timestamp in one complete ring packet (no allocation, no per-quantum postMessage). An explicit Ch N pick
  requests enough discrete lanes and routes only ChannelSplitter output N; auto keeps the legacy
  advisory-mono/sum path. Main thread drains the ring for the record buffer + incremental min/max
  waveform peaks. Recording windows use those timestamps, including compensated overdub punch-in/out;
  main-thread drain timing cannot move a take. Dropped capture packets reject the affected take or layer.
  Timing probe: `verify/capture-clock.mjs`.
- **Playback/overdub:** per-track AudioBuffer via AudioBufferSourceNode (`loop=true`),
  started/stopped at quantized absolute `currentTime`. Overdub = double-buffer + sample-
  aligned source swap at the next `loopEnd`.
- **Frame-identical tracks by construction:** looper master-loop length is stored in **integer
  frames**; later tracks record exactly `masterLengthFrames`, so all tracks are frame-identical and
  cannot drift relative to each other. State machine per track: EMPTY → RECORDING → PLAYING ⇄
  OVERDUBBING, plus STOPPED.
- **Musical stop:** optional END STOP schedules playing sources at the next master-loop boundary;
  a second press stops immediately. The click shares the final activity deadline. Capture stop/commit
  behavior is unchanged.
- **RETAKE:** optional; a take whose length is known at arm (FIXED first take, any later take) slides
  its capture window one pass forward at each window end instead of committing, setting the finished
  pass aside. The stop gesture keeps the last complete pass; REC on another lane also approves and
  hands the recorder over on the pass edge.
- **FX (per track, fixed order, each bypassable):** Filter → PitchShift → Stutter → Delay →
  Reverb. Stutter phase and delay divisions use the looper's explicit frame-derived grid, also supplied
  by the offline renderer. Pitch via Tone.PitchShift only (no offline HQ/WSOLA mode). Reverb defaults to one
  shared send bus (or algorithmic) — not five ConvolverNodes — to protect WebView2 CPU.

## Cross-cutting invariants (do not violate)

**This numbered list is THE numbering.** Source comments cite these by number ("invariant 6"), and
`AGENTS.md` carries the same list in the same order — if the two ever diverge again, every in-code
citation silently points at the wrong rule. (They did diverge: the rAF rule was 5 here and 6 there,
and two source files cited different numbers for it. Unified 2026-07-25.)

1. **The Web Audio `AudioContext` is the single tempo/quantization authority.** Every grid-timed
   event is scheduled on ctx time. The native P11 cpal monitor is a second audible path by design,
   slaved by the drift controller, never a second tempo authority.
2. **Never touch `Tone` before the engine has run `setContext`.** No `getTransport()`, no Tone node
   construction, until `engine.ctx` exists — `clock.ts` routes all transport access through a `tp()`
   helper that touches `engine.ctx` first. (This silently broke the P3 metronome once.)
3. **COOP `same-origin` + COEP `require-corp`** on both Vite (done, P0) and the Tauri asset
   protocol (P7) → `crossOriginIsolated` → SharedArrayBuffer. Ship a postMessage-batched
   fallback if isolation is ever unavailable.
   - **P9 native-audio transport (verified on WebView2 v149, P9.0):** plugin PCM crosses
     Rust→renderer via WebView2 `CreateSharedBuffer`/`PostSharedBufferToScript` — OS shared memory
     that surfaces in JS as a **regular `ArrayBuffer`** (a *separate* mechanism from
     `crossOriginIsolated` SAB; both are needed, on different hops). **`Atomics` are unsupported on
     that non-shared ArrayBuffer in Chromium 149**, so hop 1 (WebView2 buffer → drain) uses plain
     ordered reads on x86-64 TSO + Rust-side release stores (spike-proven); hop 2 (drain →
     `plugin-pcm-source` worklet) is a real `ringbuf.js` SAB where Atomics work. PCM never crosses
     as IPC samples — native audio reaches Web Audio only as an AudioNode.
4. **`?worker&url`** for all first-party TS worklets (forces TS→JS transpile + a plain URL
   for `addModule`). Bare `?url` ships un-transpiled TS; `?worker` wraps an IIFE for
   `new Worker()`. Prebuilt JS worklets load via `?url`/`/public`.
5. **No allocation in `AudioWorkletProcessor.process()`** — pre-allocate in the constructor.
6. **No Solid signal WRITES from audio-path timers, and no signal READS in the 60 fps draw loop** —
   rAF + a plain mutable object only. Widened from the draw-loop-only wording 2026-07-25: the capture
   drain was writing a fresh object into a track signal 40×/s, which cost ~200 full-document layout
   events per 5 s of recording. Both halves are the same mistake, and `engineState.loopPhasePlain`
   is the pattern to copy; the write-half fix (measured 200 → 1 layouts per 5 s) is `state.ts`'s
   `sameTrack` equality + the `displayState`/`coreGlyph` memos in `Looper.tsx`.
7. **All `@tauri-apps/*` confined to `src/platform/`** — CI-guarded.

## Decided: the looper stays in Web Audio

Moving the looper, click and master mix native ("one clock by construction") was examined and
rejected. Treat it as falsified unless the measurements below say otherwise:

- The six Web Audio synths stay web-side, so the compensation problem would move to the synth
  ingress rather than disappear.
- A live `AudioContext` cannot be externally clocked (Chromium paces rendering off its own output
  stream). The real shape would be the existing two-hop ring plus a main-thread pump on the WHOLE
  audible path, turning today's compensated constants into dropout risk under main-thread jank.
- The `verify/` suite and the Playwright/`__lf` harness are web-side and there is no Playwright into
  WebView2: the looper state machine would be re-verified by ear in RT Rust.
- Loop playback runs through the Tone FX chain, which would need an alloc-free RT Rust rewrite or a
  round trip back into Web Audio. Mac development of the core would end (no Rust toolchain there).

What stays true: on an arbitrary Windows machine (WASAPI-shared, no ASIO) absolute latency is high
and `ctx.outputLatency` cannot be trusted in either direction.

**The measurement gate.** Replacing native monitoring or changing its buffering targets requires
both, before and after the change:

- **L1 — the rig protocol:** a stable compensation `C` per take, then the saved trim, re-confirmed at
  buffer 64/128/256 and on an overdub.
- **L2 — a physical loopback measurement:** play the click out, capture it through the working
  guitar input, cross-correlate scheduled against heard.

A native master-OUT tier is worth a spike only if L2 confirms real absolute-latency pain, gated on
measured dropout under main-thread stress. A native looper core comes back on the table only if
that tier proves insufficient AND native synths + FX are explicitly budgeted. If that day comes,
`record-latency.ts` and the compensation sites in `looper/machine.ts` are DELETED, not ported: one
clock needs no record-path compensation.

## Known fragile piece (matches the brief's caveat)

Plugin **GUI embedding** inside the WebView2 window is genuinely hard: WebView2 is always
top-most within its window (the "airspace" problem), so a child plugin HWND z-fights it.
**Editors use separate top-level OS windows.** CLAP can use a plugin-owned floating window or embed
into a host-owned top-level window; VST3 embeds into a host-owned top-level window. This is not a
panel inside the WebView. Editor requests run on the per-slot owner thread, which also pumps hosted
window messages. Native thread ownership is defined in `src-tauri/AGENTS.md`:
"Only the owner thread touches the instance, the cpal streams and the editor."

Other notable risks: no turn-key VST3 host crate in Rust (lead with CLAP via `clack-host`,
which has a working reference host; VST3 via `coupler-rs/vst3` is hand-written unsafe COM —
last, P10). VST2 is dead (`vst-rs` archived).
