/**
 * THE CAPABILITY BOUNDARY.
 *
 * `src/platform/` is the ONLY place in the app permitted to import `@tauri-apps/*`
 * (enforced by `scripts/check-boundary.mjs`). Everything in `ui/`, `session/` and `app/` depends
 * on these interfaces, never on Tauri directly — so the entire frontend builds and renders
 * standalone in a browser via `pnpm dev` (silent: the browser build has no engine).
 *
 * Four capabilities are DECLARED here, but only THREE of them actually differ per platform:
 *   - PluginHost      — native VST/CLAP hosting (web: stub; tauri: invoke/listen). The real seam.
 *   - EngineHost      — the native audio engine (`docs/plans/native-engine.md`; web: a scriptable fake
 *                        for probes).
 *   - LogFolder       — the release log's folder, for Help's diagnostics (web: none).
 *   - MidiBackend      — W3C Web MIDI. WebView2 v149 ships it natively and `lib.rs` auto-grants the
 *                        permission, so tauri reuses the web one verbatim (`tauriPlatform = { ...webPlatform,
 *                        kind, pluginHost, engine, logs }`). The engine's native MIDI (midir,
 *                        `src-tauri/src/engine_io/midi`) stays off: WinMM ports are exclusive, and Web MIDI
 *                        keeps the controller.
 *
 * Live audio NEVER crosses this boundary as PCM; a session save's snapshot does, once, off the RT path
 * (`EngineHost.snapshot` / `loadSession`).
 */
import type { DeviceRequest, DeviceStatus, EngineCommand, FeedFrame } from './engine-wire';

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
  /** The load's answer: what it did with the plugin's stored tone (`src-tauri/src/host/tone.rs`).
   * Absent: nothing was stored. */
  tone?: 'restored' | 'failed';
}

/** A session import's tone, stored by the native host (`PluginHost.importTone`): the plugin it
 * belongs to, and whether the slot holds that plugin now. */
export interface ToneImport {
  /** Non-null: the slot holds that plugin, and its load stopped saving over the stored tone. The
   * imported tone is parked for the reload that passes this token to its `loadPlugin`; a reload that
   * does not happen hands it back (`forgetTone`). */
  reloadToken: number | null;
  name: string;
  format: PluginFormat;
  path: string;
  id: string;
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
   * Tell the native host the engine device's sample rate, so it `activate()`s plugins at the matching
   * rate. Call once a device runs, before the first `loadPlugin`. No-op in the web build.
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
   * `PluginDescriptor.id` from `scanPlugins()` to pick the exact one. `toneToken`: the session import's
   * reload token (`ToneImport.reloadToken`) when this load is that reload.
   */
  loadPlugin(slot: PluginSlot, path: string, id: string, loadToken: number, toneToken?: number): Promise<PluginInfo>;
  unloadPlugin(slot: PluginSlot): Promise<void>;
  /**
   * List the plugins currently loaded in the native slots (frontend-reload wedge resync). A WebView
   * reload resets the frontend's slot state to synth defaults while the Rust host keeps its plugins
   * loaded; the frontend queries this at init to find + unload the strays (else the next load hits
   * "slot N already has a plugin loaded" and the editor/GO-LIVE chrome never shows). Empty in the web
   * build.
   */
  listLoaded(): Promise<PluginInfo[]>;
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
   * Tone recall (`src-tauri/src/host/tone.rs`): every load restores the plugin's
   * stored tone by itself (`PluginInfo.tone`). `takeTone` saves the slot's tone now, through its
   * owner (the store gets it as from any save), and hands back the tone file's bytes (a session
   * export's), or null when the plugin keeps no state. `importTone` stores a session's tone under
   * `plugin`, the plugin session.json names for it (the host refuses a tone file of any other plugin),
   * and says whether `slot` holds that plugin now; it loads and swaps nothing. `forgetTone` drops the
   * tone an import parked under `reloadToken` for a reload that did not happen. All three reject in the
   * browser build.
   */
  takeTone(slot: PluginSlot): Promise<Uint8Array | null>;
  importTone(
    slot: PluginSlot,
    bytes: Uint8Array,
    plugin: Pick<PluginDescriptor, 'format' | 'path' | 'id'>,
  ): Promise<ToneImport>;
  forgetTone(slot: PluginSlot, reloadToken: number): Promise<void>;

  // ── Devices: the engine opens one input and one output (`EngineHost.open`); these list them ─────────
  /** Enumerate native capture devices. Empty in the web build. */
  listInputDevices(): Promise<AudioInputDevice[]>;
  /** Enumerate native output devices (the output and Share output pickers). Empty in the web build. */
  listOutputDevices(): Promise<AudioOutputDevice[]>;

  // ── ASIO low-latency tier ───────────────────────────────────────────────────────────────────────
  /** The startup coordinator's status (`src-tauri/src/asio_startup.rs`). Never touches the driver. */
  asioStatus(): Promise<AsioStatusReport>;
  /**
   * Request the ASIO driver probe (`src-tauri/src/asio_startup.rs`). The frontend calls this AFTER the
   * window is up, at boot only when the saved preference is on (`explicit=false`), and from the Audio
   * Settings toggle / Retry (`explicit=true`, which may proceed past a blocked or failed earlier attempt).
   * `driver` is the saved driver pick ('' = automatic; a name no longer installed falls back to it).
   * Resolves with the resulting status; `ready` means an ASIO device is available. Bounded by the
   * native probe deadline (a hung driver yields `timed-out`, never a hang here).
   */
  asioProbe(explicit: boolean, driver: string): Promise<AsioStatusReport>;
  /**
   * Switch to another ASIO driver without a restart ('' = automatic): the host drops the cached driver
   * and probes `driver` in its place, as `asioProbe(true, driver)` would. The device owner runs it: a
   * device on ASIO closes first and opens again after, on the new driver (unless that one runs at
   * another rate while the engine holds loops: the next `open` asks). A timed-out probe answers
   * `timed-out` and needs a restart, as at startup.
   */
  asioSwitch(driver: string): Promise<AsioStatusReport>;
  /** The installed ASIO drivers' names, read from the registry without loading any. Empty without ASIO. */
  asioDrivers(): Promise<string[]>;
  /** The cached ASIO driver, its actual channel counts and buffer range; null without an ASIO device. */
  asioDeviceInfo(): Promise<AsioDeviceInfo | null>;
}

/**
 * ASIO startup status (mirrors `AsioStartupStatus` in `src-tauri/src/asio_startup.rs`). `not-compiled`
 * = no ASIO in this binary (and the web build); `disabled-by-flag` = launched with `--disable-asio`;
 * `unprobed` = nothing asked yet (saved preference off); `probing`; `ready`; `failed` (explicitly
 * retryable); `blocked` (an earlier launch's probe never completed, explicit retry needed);
 * `timed-out` (restart required).
 */
export type AsioStartupStatus =
  | 'not-compiled'
  | 'disabled-by-flag'
  | 'unprobed'
  | 'probing'
  | 'ready'
  | 'failed'
  | 'blocked'
  | 'timed-out';
export interface AsioStatusReport {
  status: AsioStartupStatus;
  /** Human-readable reason for `failed` / `blocked` / `timed-out`; empty otherwise. */
  detail: string;
}

/** The cached ASIO driver (`plugin_asio_device_info`). */
export interface AsioDeviceInfo {
  name: string;
  inputChannels: number;
  outputChannels: number;
  /** The buffer sizes the driver takes, in frames; null when it did not say. Equal: one size only, set
   * in the driver's own control panel. */
  bufferMin: number | null;
  bufferMax: number | null;
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

/**
 * The native audio engine (`src-tauri/src/engine_io`): one device, the looper, synths, FX, mixer and
 * plugin slots in the audio callback. The UI sends commands and reads the feed; no PCM crosses. The
 * payloads are `engine-wire.ts`.
 */
export interface EngineHost {
  /** False in the browser build (unless a DEV probe forces the web fake on, `host.web.ts`). */
  readonly available: boolean;
  /** Open the device, or switch to another; resolves with the device that runs. Rejects with the wire's
   * `OpenError` (`decodeOpenError`): a switch to another rate while the engine holds audio is refused
   * unless `force` (the player confirmed dropping the loops from the engine). */
  open(request: DeviceRequest, force?: boolean): Promise<DeviceStatus>;
  close(): Promise<void>;
  /** The device that runs, or null. */
  status(): Promise<DeviceStatus | null>;
  /** A plugin slot's capture channel (0-based; null = auto: input 2 on a device with two or more),
   * switched without reopening the device; the running device keeps it for its own reopens. Rejects for
   * a channel the device lacks (nothing changes) and while no device runs. */
  setSlotInputChannel(slot: number, channel: number | null): Promise<void>;
  /** A batch of commands, applied in order at the next block. Fire-and-forget: engine refusals come
   * back on the feed; a rejection means the batch never reached the engine. */
  send(commands: readonly EngineCommand[]): Promise<void>;
  /** Share output's endpoint (a WASAPI render id, as `listOutputDevices` lists them), or null for off.
   * The mirror runs while the device is ASIO. */
  setShare(endpoint: string | null): Promise<void>;
  /** The committed lanes' PCM and what they are (`engine-wire.ts` § Session bytes). */
  snapshot(): Promise<ArrayBuffer>;
  /** Load a session into an all-empty engine (`engine-wire.ts` § Session bytes). */
  loadSession(bytes: Uint8Array<ArrayBuffer>): Promise<void>;
  /** Subscribe to the feed (~60 frames/s while something changes); the first frame has `reset`.
   * Returns the unsubscribe. */
  subscribe(onFrame: (frame: FeedFrame) => void): () => void;
}

/**
 * The release log's folder (tauri-plugin-log's rotated file, `src-tauri/src/lib.rs`). Help's "About
 * this build" names it in the copied diagnostics and opens it, so a tester can attach the log to a
 * report.
 */
export interface LogFolder {
  /** False in the browser build: it writes no log file, and Help hides Open log folder. */
  readonly available: boolean;
  /** The folder's path, `%LOCALAPPDATA%` standing in for the user's profile. */
  path(): Promise<string>;
  /** Open the folder in Explorer. */
  open(): Promise<void>;
}

export type PlatformKind = 'web' | 'tauri';

export interface Platform {
  readonly kind: PlatformKind;
  readonly pluginHost: PluginHost;
  readonly engine: EngineHost;
  readonly logs: LogFolder;
  readonly midi: MidiBackend;
}
