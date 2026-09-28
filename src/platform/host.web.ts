/**
 * Browser implementation of the capability boundary. Zero Tauri/Rust dependency.
 * This is what `pnpm dev` runs against.
 */
import type { EngineHost, LogFolder, MidiBackend, Platform, PluginHost } from './host';
import {
  decodeFeedFrame,
  encodeSessionBytes,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type FeedFrame,
} from './engine-wire.ts'; // explicit .ts: Node guards import this file

const NO_NATIVE_HOST =
  'Native VST host is unavailable in the browser build — use the built-in synths.';

const webPluginHost: PluginHost = {
  available: false,
  async init() {
    /* no-op — no native host in the browser build */
  },
  async scanPlugins() {
    return [];
  },
  async loadPlugin(_slot, _path, _id, _loadToken, _toneToken) {
    throw new Error(NO_NATIVE_HOST);
  },
  async unloadPlugin() {
    /* no-op */
  },
  async listLoaded() {
    return []; // no native host in the browser build
  },
  async openEditor() {
    throw new Error(NO_NATIVE_HOST);
  },
  async closeEditor() {
    /* no-op */
  },
  async setParameter() {
    /* no-op */
  },
  async listParams() {
    return [];
  },
  onParamChanged() {
    return () => {};
  },
  onParamsChanged() {
    return () => {};
  },
  onEditorClosed() {
    return () => {};
  },
  async takeTone() {
    return null; // no plugin loads in the browser build, so none keeps a tone
  },
  async importTone() {
    throw new Error(NO_NATIVE_HOST);
  },
  async forgetTone() {
    throw new Error(NO_NATIVE_HOST);
  },
  async listInputDevices() {
    return [];
  },
  async listOutputDevices() {
    return [];
  },
  async asioStatus() {
    return { status: 'not-compiled' as const, detail: '' };
  },
  async asioProbe() {
    return { status: 'not-compiled' as const, detail: '' };
  },
  async asioSwitch() {
    return { status: 'not-compiled' as const, detail: '' };
  },
  async asioDrivers() {
    return [];
  },
  async asioDeviceInfo() {
    return null;
  },
};

const webMidi: MidiBackend = {
  async requestAccess() {
    if (!navigator.requestMIDIAccess) return null;
    return navigator.requestMIDIAccess({ sysex: false });
  },
};

const NO_LOG_FILE = 'The browser build writes no log file.';

// Keep this below `webPluginHost`: probes that stand in for the native host flip the FIRST
// `available: false` in this file, which must stay the plugin host's.
const webLogFolder: LogFolder = {
  available: false,
  async path() {
    throw new Error(NO_LOG_FILE);
  },
  async open() {
    throw new Error(NO_LOG_FILE);
  },
};

/** The web engine fake (below) plus what a probe reads and scripts through `__lf.native`. */
export interface EngineFake extends EngineHost {
  /** Every command sent, in order (batches flattened). */
  readonly sent: EngineCommand[];
  /** Every request `open()` received, and whether it was forced. */
  readonly opened: DeviceRequest[];
  readonly forced: boolean[];
  /**
   * A probe-scripted refusal: while set, `open()` without `force` rejects with it as the native owner
   * refuses a switch to another rate while the engine holds audio (`OpenError::RateChange`). The rate
   * `open()` answers is `window.__lfEngineFakeRate` (48 kHz unless an init script or a probe sets it).
   */
  refusal: { device: string; from: number; to: number } | null;
  /** What `snapshot()` answers (a probe sets it; null answers an empty engine). */
  snapshotBytes: ArrayBuffer | null;
  /** Every session `loadSession()` received. */
  readonly loadedSessions: Uint8Array[];
  /** Every Share endpoint `setShare()` received. */
  readonly shares: (string | null)[];
  /** Every `setSlotInputChannel()` pick, in order: [slot, channel]. */
  readonly slotInputChannels: [number, number | null][];
  /**
   * Decode `raw` as a feed frame (the real decoder) and hand it to the subscribers, as the native feed
   * would. Only probes call it, through `__lf.native`.
   * @public
   */
  emit(raw: unknown): void;
}

const NO_ENGINE = 'The native engine is unavailable in the browser build.';

/** Forced on only by a DEV probe's init script (`verify/probes/engine-seam.mjs`), before the app loads.
 * Read when asked, never at module load: Node guards import this file without Vite's env. */
function engineForced(): boolean {
  return import.meta.env.DEV && (globalThis as { __lfEngineFake?: unknown }).__lfEngineFake === true;
}

const engineSubscribers = new Set<(frame: FeedFrame) => void>();
let fakeStatus: DeviceStatus | null = null;
const fakeRate = () => (globalThis as { __lfEngineFakeRate?: number }).__lfEngineFakeRate ?? 48000;

/**
 * The engine host's browser stand-in: the browser build has no engine (`available` false) unless a DEV
 * probe forces this fake on. Forced on, it answers `open()` with a canned device, records every batch in `sent` and hands a
 * probe-scripted frame from `emit()` to the subscribers. Not a second looper: nothing answers a command
 * by itself, so a probe asserts gesture → command and frame → DOM.
 */
export const webEngineFake: EngineFake = {
  get available() {
    return engineForced();
  },
  sent: [],
  opened: [],
  forced: [],
  refusal: null,
  snapshotBytes: null,
  loadedSessions: [],
  shares: [],
  slotInputChannels: [],
  async open(request, force = false) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.opened.push(request);
    webEngineFake.forced.push(force);
    if (webEngineFake.refusal && !force) throw { RateChange: { ...webEngineFake.refusal } };
    // A two-input device: auto (null) reads input 2, as the engine picks it.
    const picks = 'inputChannels' in request ? request.inputChannels : [request.inputChannel, request.inputChannel];
    fakeStatus = {
      backend: request.backend,
      sampleRate: fakeRate(),
      block: request.buffer ?? 256,
      inputName: 'Fake input',
      outputName: 'Fake output',
      alignFrames: 0,
      inputFrames: 0,
      inputOpen: true,
      inputChannels: [picks[0] ?? 1, picks[1] ?? 1],
    };
    return fakeStatus;
  },
  async close() {
    fakeStatus = null;
  },
  async status() {
    return fakeStatus;
  },
  async setSlotInputChannel(slot, channel) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.slotInputChannels.push([slot, channel]);
  },
  async send(commands) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.sent.push(...commands);
  },
  async setShare(endpoint) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.shares.push(endpoint);
  },
  async snapshot() {
    if (!engineForced()) throw new Error(NO_ENGINE);
    return webEngineFake.snapshotBytes?.slice(0) ?? encodeSessionBytes({ rate: fakeRate(), masterLengthFrames: 0, bpm: 120, tracks: [] }, []).buffer;
  },
  async loadSession(bytes) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.loadedSessions.push(bytes.slice());
  },
  subscribe(onFrame) {
    engineSubscribers.add(onFrame);
    return () => engineSubscribers.delete(onFrame);
  },
  emit(raw) {
    const frame = decodeFeedFrame(raw);
    for (const onFrame of engineSubscribers) onFrame(frame);
  },
};

export const webPlatform: Platform = {
  kind: 'web',
  pluginHost: webPluginHost,
  engine: webEngineFake,
  logs: webLogFolder,
  midi: webMidi,
};
