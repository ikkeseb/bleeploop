/**
 * Browser implementation of the capability boundary. Zero Tauri/Rust dependency.
 * This is what `pnpm dev` runs against.
 */
import type { AppUpdate, AppUpdates, EngineHost, LogFolder, MidiBackend, Platform, PluginHost } from './host';
import {
  decodeFeedFrame,
  encodeSessionBytes,
  splitSessionBytes,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type FeedFrame,
  type SnapshotHeader,
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
  async loadPlugin(_slot, _path, _id, _toneToken) {
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

/** A DEV probe's scripted updater (`verify/probes/app-update.mjs`), set by an init script as
 * `window.__lfUpdateFake` before the app loads. */
interface UpdateScript {
  /** What `check()` answers. */
  update: AppUpdate | null;
  /** Counts `install()` calls. */
  installs: number;
  /** Set: `install()` rejects with it. */
  fail?: string;
}

/** Read when asked, never at module load: Node guards import this file without Vite's env. */
function updateScript(): UpdateScript | null {
  if (!import.meta.env.DEV) return null;
  return (globalThis as { __lfUpdateFake?: UpdateScript }).__lfUpdateFake ?? null;
}

/** The browser build has no updater; a DEV probe's script stands in for one. */
const webUpdates: AppUpdates = {
  get available() {
    return updateScript() !== null;
  },
  async check() {
    return updateScript()?.update ?? null;
  },
  async install() {
    const script = updateScript();
    if (!script) throw new Error('The browser build has no updater.');
    script.installs++;
    if (script.fail) throw new Error(script.fail);
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
  /** What `snapshot()` answers (a probe sets it; null answers an empty engine). Asked with the master,
   * the fake adds its stand-in (`fakeMaster`) to these stems, or `masterError` when set. */
  snapshotBytes: ArrayBuffer | null;
  /** A probe-scripted master render failure: while set, a snapshot asked with the master carries this
   * error instead of one, as the engine's does when its render fails. */
  masterError: string | null;
  /** Every `snapshot()`'s `master` flag, in order: an export asks for the master, a recovery never. */
  readonly snapshots: boolean[];
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

/**
 * The fake's stand-in for the engine's wet master: the stems summed dry under the lane volumes and
 * mutes and the master volume and mute the UI last sent (`sent`, as the engine host keeps them). NOT the
 * engine's sound (no FX, no reverb, no limiter); it only lets the browser tier's export run, so no probe
 * may claim to test the master's sound with it.
 */
function fakeMaster(header: SnapshotHeader, pcm: readonly Float32Array[], sent: readonly EngineCommand[]): Float32Array {
  const volume = new Map<number, number>();
  const muted = new Map<number, boolean>();
  let master = 1;
  let masterMuted = false;
  for (const c of sent) {
    if (typeof c !== 'object') continue;
    if ('SetVolume' in c) volume.set(c.SetVolume[0], c.SetVolume[1]);
    else if ('SetMute' in c) muted.set(c.SetMute[0], c.SetMute[1]);
    else if ('SetMasterVolume' in c) master = c.SetMasterVolume;
    else if ('SetMasterMute' in c) masterMuted = c.SetMasterMute;
  }
  const out = new Float32Array(header.masterLengthFrames);
  if (masterMuted) return out;
  header.tracks.forEach((t, k) => {
    if (muted.get(t.index)) return;
    const gain = (volume.get(t.index) ?? 1) * master;
    pcm[k].forEach((x, i) => (out[i] += gain * x));
  });
  return out;
}

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
  masterError: null,
  snapshots: [],
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
  async snapshot(master) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.snapshots.push(master);
    const bytes = webEngineFake.snapshotBytes?.slice(0) ?? encodeSessionBytes({ rate: fakeRate(), masterLengthFrames: 0, bpm: 120, tracks: [] }, []).buffer;
    if (!master || !webEngineFake.snapshotBytes) return bytes;
    const { header, pcm } = splitSessionBytes(bytes);
    const stems = header as SnapshotHeader;
    if (webEngineFake.masterError !== null) return encodeSessionBytes({ ...stems, masterError: webEngineFake.masterError }, pcm).buffer;
    const mono = fakeMaster(stems, pcm, webEngineFake.sent);
    return encodeSessionBytes({ ...stems, master: { frames: stems.masterLengthFrames } }, pcm, { left: mono, right: mono.slice() }).buffer;
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
  updates: webUpdates,
  midi: webMidi,
};
