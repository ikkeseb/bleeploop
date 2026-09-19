/**
 * THE CAPABILITY BOUNDARY.
 *
 * `src/platform/` is the ONLY place in the app permitted to import `@tauri-apps/*`
 * (enforced by `scripts/check-boundary.mjs`). Everything in `audio/` and `ui/` depends
 * on these interfaces, never on Tauri directly — so the entire frontend builds and runs
 * standalone in a browser via `pnpm dev`, and gains native capabilities under Tauri later
 * with zero rewrite.
 *
 * Three capabilities are DECLARED here, but only ONE of them actually differs per platform:
 *   - PluginHost      — native VST/CLAP hosting (web: stub; tauri: invoke/listen). The real seam.
 *   - AudioInputSource — mic/line. getUserMedia works inside WebView2, so tauri reuses the web one
 *                        verbatim (`tauriPlatform = { ...webPlatform, kind, pluginHost }`).
 *   - MidiBackend      — W3C Web MIDI. WebView2 v149 ships it natively and `lib.rs` auto-grants the
 *                        permission, so tauri reuses the web one verbatim too. NO midir bridge and no
 *                        `tauri-plugin-midi` exist, and neither crate is in Cargo.toml.
 *
 * Audio buffers NEVER cross this boundary as PCM — native audio reaches the Web Audio
 * graph only as an AudioNode (MediaStream / SharedArrayBuffer ring).
 */

export type PluginSlot = 0 | 1;
export type PluginFormat = 'clap' | 'vst3';
export type EditorMode = 'floating' | 'embedded';

export interface PluginDescriptor {
  id: string;
  name: string;
  format: PluginFormat;
  path: string;
  /** Output-gain kind: `true` = audio effect (amp-sim/FX), `false` = instrument (synth), `null` =
   * unclassified. Read at scan from CLAP `features()` / VST3 `subCategories`. The plugin bridge uses
   * it to pick the per-slot output-gain default (falling back to the input-bus count when null). */
  isEffect: boolean | null;
}

export interface PluginInfo {
  slot: PluginSlot;
  descriptor: PluginDescriptor;
}

/** One plugin parameter's metadata. `id` is the stable CLAP param id `setParameter` takes. */
export interface PluginParamDesc {
  id: number;
  name: string;
  minValue: number;
  maxValue: number;
  defaultValue: number;
  /** The LIVE value at enumeration time (same units as min/max) — what a freshly mounted drawer shows. */
  value: number;
}

/** A native audio capture device — what the plugin-input device picker lists. */
export interface AudioInputDevice {
  id: string;
  name: string;
  channels: number;
}

/** A native audio OUTPUT device — what the low-latency monitor device picker lists. */
export interface AudioOutputDevice {
  id: string;
  name: string;
  channels: number;
}

export interface PluginHost {
  /** False in the browser build — UI then surfaces the six built-in synths in both slots. */
  readonly available: boolean;
  /**
   * Tell the native host the engine's AudioContext sample rate, so it `activate()`s plugins at the
   * matching rate (the drift controller resamples the residual). Call once at startup before the
   * first `loadPlugin`. No-op in the web build.
   */
  init(sampleRate: number): Promise<void>;
  /**
   * List installed plugins. The native side remembers each bundle's result under a size + mtime
   * fingerprint, so a launch spawns no scan child for unchanged plugins; `force` (the picker's
   * rescan button) scans everything again, which is also how a previously failed bundle is retried.
   */
  scanPlugins(force?: boolean): Promise<PluginDescriptor[]>;
  /**
   * `id` is required, not optional: a single `.clap`/`.vst3` bundle can export multiple plugin
   * descriptors, so `(slot, path)` alone would silently load `descriptor[0]`. Pass the
   * `PluginDescriptor.id` from `scanPlugins()` to pick the exact one.
   */
  loadPlugin(slot: PluginSlot, path: string, id: string): Promise<PluginInfo>;
  unloadPlugin(slot: PluginSlot): Promise<void>;
  /**
   * List the plugins currently loaded in the native slots (frontend-reload wedge resync). A WebView
   * reload resets the frontend's slot state to synth defaults while the Rust host keeps its plugins
   * loaded; the frontend queries this at init to find + unload the strays (else the next load hits
   * "slot N already has a plugin loaded" and the editor/GO-LIVE chrome never shows). Empty in the web
   * build.
   */
  listLoaded(): Promise<PluginInfo[]>;
  /**
   * Route a live note to the plugin. `velocity` is the CLAP-normalised 0..1 form (the input
   * router divides MIDI velocity by 127). The note rides the plugin's process-input event queue on
   * the audio thread via a main→audio ring — never a main-thread call.
   */
  noteOn(slot: PluginSlot, note: number, velocity: number): Promise<void>;
  noteOff(slot: PluginSlot, note: number): Promise<void>;
  openEditor(slot: PluginSlot, mode: EditorMode): Promise<void>;
  closeEditor(slot: PluginSlot): Promise<void>;
  setParameter(slot: PluginSlot, paramId: number, value: number): Promise<void>;
  /** Enumerate the loaded plugin's parameters (stable ids + ranges). Empty in the web build. */
  listParams(slot: PluginSlot): Promise<PluginParamDesc[]>;
  /**
   * Subscribe to param changes ORIGINATED BY THE PLUGIN'S OWN EDITOR — a knob drag in the
   * hosted GUI. Fires `{slot, id, value}` where `id` is the same stable param id `setParameter`/
   * `listParams` use and `value` is the new normalised 0..1 value. Returns an unsubscribe fn. No-op
   * (returns a no-op unsubscribe) in the web build.
   */
  onParamChanged(cb: (e: { slot: PluginSlot; id: number; value: number }) => void): () => void;
  /**
   * Subscribe to a WHOLESALE parameter change reported by the plugin itself — a preset loaded in its
   * own GUI, a program change (CLAP `params.rescan`, VST3 `restartComponent(kParamValuesChanged)`).
   * The values the UI holds are stale after this; re-run `listParams`. Returns an unsubscribe fn. No-op
   * in the web build.
   */
  onParamsChanged(cb: (slot: PluginSlot) => void): () => void;
  /**
   * Subscribe to editor-closed events emitted when the user closes a hosted/floating editor (its own
   * close box), so the UI can re-sync an "editor open" toggle. Returns an unsubscribe fn. No-op in the
   * web build.
   */
  onEditorClosed(cb: (slot: PluginSlot) => void): () => void;
  /**
   * Subscribe to a TERMINAL native stream fault: the slot's cpal capture (`kind: 'input'`) or
   * low-latency monitor (`kind: 'output'`) stream died — interface unplugged, ASIO driver reset. cpal's
   * error callback ends that stream for good, so by the time this fires the native side has ALREADY
   * dropped the stream and cleared the RT state that fed it; the JS side only reconciles its OWN state
   * (fall back to the web monitor path, clear the armed flag + the record-latency registration). Never
   * fires for a user-initiated disarm, and never for a recoverable underrun/overrun. Returns an
   * unsubscribe fn. No-op in the web build.
   */
  onStreamFault(cb: (e: { slot: PluginSlot; kind: 'input' | 'output' }) => void): () => void;
  /**
   * Plugin preset/state via CLAP's `state` extension. Opaque plugin-defined bytes:
   * `saveState` serialises the live plugin, `loadState` restores it. The looper/preset story
   * depends on this round-trip surviving load → unload → reload.
   */
  saveState(slot: PluginSlot): Promise<Uint8Array>;
  loadState(slot: PluginSlot, bytes: Uint8Array): Promise<void>;

  // ── Native audio INPUT ──────────────────────────────────────────────────────────────────
  // Route a hardware guitar/line signal INTO the slot's loaded plugin so an FX plugin (amp-sim)
  // processes a live signal and the "wet" output can be monitored. cpal-native: PCM never crosses
  // this boundary — the wet signal still returns via the SharedBuffer path as an AudioNode. Web
  // build: `listInputDevices` is empty, `armInput` rejects, `disarmInput` no-ops.
  /** Enumerate native capture devices (cpal). Empty in the web build. */
  listInputDevices(): Promise<AudioInputDevice[]>;
  /**
   * Feed `deviceId`'s capture stream (or the default input when omitted/null) INTO the plugin in
   * `slot`, isolating one input `channel` (0-based; omitted/null = auto-pick). A multi-input
   * interface exposes all its inputs as one interleaved stream, so the channel pick selects which one
   * reaches the plugin. Rejects if the slot holds no plugin or the plugin exposes no audio-input bus
   * (a pure synth) — the caller surfaces that to the UI.
   */
  armInput(slot: PluginSlot, deviceId?: string | null, channel?: number | null): Promise<void>;
  /** Stop feeding input to the slot's plugin. Idempotent; no-op in the web build. */
  disarmInput(slot: PluginSlot): Promise<void>;

  // ── Native low-latency monitor ─────────────────────────────────────────────────────────
  // A cpal OUTPUT stream on the same physical device as the capture (one crystal) plays the slot's
  // wet plugin signal LIVE, bypassing the WebView2 round-trip (branch-2, which stays the looper
  // record tap). No PCM crosses this boundary — cpal is Rust-internal. The web build returns `[]`,
  // and arm/disarm/setMonitorGain are no-ops. The caller is responsible for muting the web monitor
  // while the native one is armed (else the wet doubles → flam).
  /** Enumerate native output devices (cpal/WASAPI-shared). Empty in the web build. */
  listOutputDevices(): Promise<AudioOutputDevice[]>;
  /**
   * Arm the native low-latency monitor on `slot`: open a cpal OUTPUT stream on `deviceId` (or the
   * default output when omitted/null) fed the slot's wet plugin output. Rejects if the slot holds no
   * plugin or the stream can't open. Independent of input arming (a synth OR an FX can be monitored).
   */
  armMonitor(slot: PluginSlot, deviceId?: string | null): Promise<void>;
  /** Drop the slot's native monitor stream. Idempotent; no-op in the web build. */
  disarmMonitor(slot: PluginSlot): Promise<void>;
  /**
   * Set the slot's native-monitor output gain (linear, 0..1.5 like the web output gain). Stored
   * directly into a Rust atomic (no IPC round-trip on the audio thread). No-op in the web build.
   */
  setMonitorGain(slot: PluginSlot, gain: number): Promise<void>;
  /**
   * Set the user-facing master factor for every native monitor stream (linear, 0..1). This scales
   * only the cpal audible path; the plugin signal feeding `recordTap` stays full. No-op in the web
   * build.
   */
  setMasterGain(gain: number): Promise<void>;
  /**
   * The slot's native-monitor output latency ("cpal_out") in SECONDS — the time from the RT producer
   * emitting a wet sample to the player hearing it through the cpal output stream. Read once per record
   * arm by the looper's automatic record-latency compensation (it SUBTRACTS this; the player aligns their
   * natively-monitored guitar to the heard click, self-correcting for it). 0 when the monitor is disarmed
   * or in the web build (no native monitor ⇒ no compensation).
   */
  monitorLatencySeconds(slot: PluginSlot): Promise<number>;

  // ── Global RT buffer size ───────────────────────────────────────────────────────────────
  /**
   * Set the global RT block size (frames; one of audio-settings `BUFFER_FRAMES_OPTIONS`). The dominant
   * latency knob — smaller = lower monitor latency, higher underrun risk. Re-paces BOTH slots' producer
   * loops at the new block WITHOUT reloading the plugins (params/editor/state preserved); a brief
   * audible gap during the rebuild. No PCM crosses the boundary; no-op in the web build.
   */
  setBufferSize(frames: number): Promise<void>;

  // ── ASIO low-latency tier ───────────────────────────────────────────────────────────────────────
  /**
   * Whether an ASIO low-latency device is available (the native build compiled the ASIO host AND a
   * device was found at startup). The Audio Settings toggle enables itself on this. Always false in the
   * web build.
   */
  asioAvailable(): Promise<boolean>;
  /** Cached default ASIO driver and its actual channel counts; null without an ASIO device. */
  asioDeviceInfo(): Promise<{ name: string; inputChannels: number; outputChannels: number } | null>;
  /**
   * Set the ASIO-tier preference. When enabled (and available) the native capture + monitor use ASIO
   * for low latency; disabled forces WASAPI-shared. Takes effect on the NEXT arm — a live stream keeps
   * the host it was opened with (same "applies on next arm" rule as the device/buffer pickers). No-op
   * in the web build.
   */
  setAsioEnabled(enabled: boolean): Promise<void>;
}

/** A live audio input, delivered as an AudioNode on the caller's shared AudioContext. */
export interface OpenedInput {
  node: AudioNode;
  sampleRate: number;
  /** Releases the underlying device/stream. */
  close(): void;
}

export interface AudioInputOpenOptions {
  /**
   * Isolate one physical input channel before returning the node (0-based). Explicit picks request
   * enough channels to keep the interface lanes discrete, then route only this ChannelSplitter
   * output. Omitted/null keeps the legacy auto path, which accepts the browser's mono preference and
   * lets capture.ts centre/sum whatever the interface actually returns.
   */
  channel?: number | null;
  /** Reports the opened device dying while live. See AudioInputSource.open. */
  onLost?: () => void;
}

export interface AudioInputSource {
  /**
   * Opens the input on the given context. Three outcomes, and callers distinguish all three:
   * - resolves an OpenedInput on success;
   * - resolves null when the capability is absent (e.g. no getUserMedia) — surfaced as "no input available";
   * - REJECTS on denial/open failure (permission denied, device busy, NotFoundError) — surfaced as an
   *   arm-failure error toast (see ui/transport/Transport.tsx onToggleMic + audio/looper/capture.ts armInput).
   * Implementers must NOT collapse denial into null.
   *
   * `options.onLost` reports the device DYING while open (interface unplugged, device disabled) — at
   * most once per opened input, and never for the caller's own `close()`. The handle is dead when it
   * fires: the graph stays wired to a source that now yields only silence, so the caller must run its
   * full disarm path. Implementers that cannot detect loss simply never call it.
   *
   * Device selection is deliberately not accepted here yet. Audio Settings stores a native cpal
   * endpoint id, while getUserMedia requires an origin-specific MediaDeviceInfo.deviceId; treating
   * those strings as interchangeable would make an explicit selection fail or open the wrong device.
   */
  open(ctx: AudioContext, options?: AudioInputOpenOptions): Promise<OpenedInput | null>;
}

export interface MidiBackend {
  /**
   * W3C Web MIDI via navigator.requestMIDIAccess — Chromium/Edge natively, and WebView2 v149
   * natively too (lib.rs auto-grants the permission), so BOTH platforms use the same web
   * implementation and there is no shim. Returns null if unsupported (the on-screen + computer
   * keyboard remain fully playable).
   */
  requestAccess(): Promise<MIDIAccess | null>;
}

export type PlatformKind = 'web' | 'tauri';

export interface Platform {
  readonly kind: PlatformKind;
  readonly pluginHost: PluginHost;
  readonly audioInput: AudioInputSource;
  readonly midi: MidiBackend;
}
