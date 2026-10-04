/**
 * Browser implementation of the capability boundary. Zero Tauri/Rust dependency.
 * This is what `pnpm dev` runs against.
 */
import type { AppUpdate, AppUpdates, EngineHost, LogFolder, MidiBackend, Platform, PluginHost } from './host';
import {
  ENGINE_LANES,
  decodeFeedFrame,
  decodeLoadSession,
  encodeSessionBytes,
  splitSessionBytes,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type FeedFrame,
  type FxParamId,
  type LaneMix,
  type LoadHeader,
  type SnapshotHeader,
  type SnapshotTrack,
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
  /** What `snapshot()` answers (a probe sets it; null answers an empty engine). A track without its
   * `mix` gets the lane's as the fake's commands and events left it (`fakeMixes`), read when asked; a
   * probe that scripts one keeps it. Asked with the master, the fake adds its stand-in (`fakeMaster`) to
   * these stems, or `masterError` when set. */
  snapshotBytes: ArrayBuffer | null;
  /** A probe-scripted master render failure: while set, a snapshot asked with the master carries this
   * error instead of one, as the engine's does when its render fails. */
  masterError: string | null;
  /** Every `snapshot()`'s `master` flag, in order: an export asks for the master, a recovery never. */
  readonly snapshots: boolean[];
  /** A probe-scripted render in progress: while set, `snapshot()` reads its answer when asked and hands
   * it back once this settles, as the engine's snapshot returns only after its master render. */
  snapshotHold: Promise<void> | null;
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
 * A fresh or cleared lane's mix, as the engine's (`LaneMix::default`): unity, unmuted, a plain sum, every
 * effect bypassed at its defaults (the defaults of `FX_PARAM_DEFS`, `src/ui/state/fx-metadata.ts`, which
 * this file may not import; the engine-wire guard holds them equal).
 */
export function defaultLaneMix(): LaneMix {
  return {
    volume: 1,
    muted: false,
    dubFeedback: 1,
    fx: [
      { bypassed: true, params: { cutoff: 1200, q: 2 } },
      { bypassed: true, params: { semitones: 0 } },
      { bypassed: true, params: { rate: 1 } },
      { bypassed: true, params: { time: 1, feedback: 0.4, mix: 0.3 } },
      { bypassed: true, params: { amount: 0.3 } },
    ],
  };
}

const FX_ORDER = ['filter', 'pitch', 'stutter', 'delay', 'reverb'] as const;
const copyMix = (m: LaneMix): LaneMix => ({ ...m, fx: m.fx.map((f) => ({ bypassed: f.bypassed, params: { ...f.params } })) });

/**
 * Each FX param's range as the engine applies it (`lf_engine::dsp::fx::FX_PARAM_DEFS`): clamped, and an
 * `integer` one rounded half up. The UI's `FX_PARAM_DEFS` holds the same; the engine-wire guard holds
 * them equal.
 */
export const FX_PARAM_RANGES: Readonly<Record<string, { min: number; max: number; integer: boolean }>> = {
  cutoff: { min: 120, max: 14000, integer: false },
  q: { min: 0.1, max: 14, integer: false },
  semitones: { min: -12, max: 12, integer: true },
  rate: { min: 0, max: 3, integer: true },
  time: { min: 0, max: 3, integer: true },
  feedback: { min: 0, max: 0.95, integer: false },
  mix: { min: 0, max: 1, integer: false },
  amount: { min: 0, max: 1, integer: false },
};

const clamp = (v: number, min: number, max: number) => Math.max(min, Math.min(max, v));

/** Each lane's mix as the fake engine holds it: what the commands sent and the events emitted set, as
 * the engine applies them (a reset frame's settings, a mix command, CLEAR, COPY, a pedal's MUTE). */
const fakeMixes: LaneMix[] = Array.from({ length: ENGINE_LANES }, defaultLaneMix);
/** Per source lane, the mix its last COPY command latched: the engine copies the source's mix when the
 * COPY applies, and its Copied comes only when the PCM job ends. A COPY the engine drops (no EMPTY lane)
 * leaves its latch to the next one. `Action` COPY acts on the engine's selected lane, which the fake
 * does not know: its Copied takes the source's mix then. */
const copyLatches: (LaneMix | null)[] = Array.from({ length: ENGINE_LANES }, () => null);

function applyMixCommand(c: EngineCommand): void {
  if (typeof c !== 'object') return;
  const at = (lane: number) => fakeMixes[lane] as LaneMix | undefined;
  if ('SetVolume' in c) {
    const m = at(c.SetVolume[0]);
    const v = c.SetVolume[1];
    if (m) m.volume = Number.isFinite(v) ? clamp(v, 0, 1.5) : 0;
  } else if ('SetMute' in c) {
    const m = at(c.SetMute[0]);
    if (m) m.muted = c.SetMute[1];
  } else if ('SetDubFeedback' in c) {
    const m = at(c.SetDubFeedback[0]);
    const v = c.SetDubFeedback[1];
    if (m) m.dubFeedback = Number.isFinite(v) ? clamp(v, 0, 1) : 1;
  } else if ('SetFxBypass' in c) {
    const [lane, kind, bypassed] = c.SetFxBypass;
    const fx = at(lane)?.fx[FX_ORDER.indexOf(kind)];
    if (fx) fx.bypassed = bypassed;
  } else if ('SetFxParam' in c) {
    const [lane, key, value] = c.SetFxParam;
    const fx = at(lane)?.fx.find((f) => key in f.params);
    const range = FX_PARAM_RANGES[key];
    if (!fx || !range || !Number.isFinite(value)) return;
    fx.params[key] = clamp(range.integer ? Math.round(value) : value, range.min, range.max);
  } else if ('Copy' in c) {
    if (fakeMixes[c.Copy]) copyLatches[c.Copy] = copyMix(fakeMixes[c.Copy]);
  } else if ('ActionOn' in c && c.ActionOn[1] === 'Copy') {
    const lane = c.ActionOn[0];
    if (fakeMixes[lane]) copyLatches[lane] = copyMix(fakeMixes[lane]);
  }
}

function applyMixFrame(frame: Pick<FeedFrame, 'reset' | 'settings' | 'events'>): void {
  if (frame.reset) {
    fakeMixes.forEach((_, i) => (fakeMixes[i] = defaultLaneMix()));
    copyLatches.fill(null);
    frame.settings?.forEach(applyMixCommand);
  }
  for (const ev of frame.events) {
    if (ev.type === 'Cleared') fakeMixes[ev.lane] = defaultLaneMix();
    else if (ev.type === 'Copied') {
      fakeMixes[ev.to] = { ...(copyLatches[ev.from] ?? copyMix(fakeMixes[ev.from])), dubFeedback: ev.feedback };
      copyLatches[ev.from] = null;
    } else if (ev.type === 'Muted') fakeMixes[ev.lane].muted = ev.on;
  }
}

/** A load sets each loaded lane's mix whole, as the engine clamps it (over a mix sent to the EMPTY
 * lane before). */
function applyMixLoad(header: LoadHeader): void {
  for (const { index: lane, mix } of header.tracks) {
    if (!fakeMixes[lane]) continue;
    fakeMixes[lane] = defaultLaneMix();
    applyMixCommand({ SetVolume: [lane, mix.volume] });
    applyMixCommand({ SetMute: [lane, mix.muted] });
    applyMixCommand({ SetDubFeedback: [lane, mix.dubFeedback] });
    mix.fx.forEach((f, k) => {
      applyMixCommand({ SetFxBypass: [lane, FX_ORDER[k], f.bypassed] });
      for (const [key, value] of Object.entries(f.params)) applyMixCommand({ SetFxParam: [lane, key as FxParamId, value] });
    });
  }
}

/** The fake's per-lane mix model, for the engine-wire guard (Node cannot reach `webEngineFake.send`).
 * @public */
export const fakeMixModel = {
  command: applyMixCommand,
  frame: applyMixFrame,
  load: applyMixLoad,
  lane: (i: number): LaneMix => copyMix(fakeMixes[i]),
};

/**
 * The fake's stand-in for the engine's wet master: the stems summed dry under each track's volume and
 * mute (its snapshot mix) and the master volume and mute the UI last sent (`sent`, as the engine host
 * keeps them). NOT the engine's sound (no FX, no reverb, no limiter); it only lets the browser tier's
 * export run, so no probe may claim to test the master's sound with it.
 */
function fakeMaster(header: SnapshotHeader, pcm: readonly Float32Array[], sent: readonly EngineCommand[]): Float32Array {
  let master = 1;
  let masterMuted = false;
  for (const c of sent) {
    if (typeof c !== 'object') continue;
    if ('SetMasterVolume' in c) master = c.SetMasterVolume;
    else if ('SetMasterMute' in c) masterMuted = c.SetMasterMute;
  }
  const out = new Float32Array(header.masterLengthFrames);
  if (masterMuted) return out;
  header.tracks.forEach((t, k) => {
    if (t.mix.muted) return;
    const gain = t.mix.volume * master;
    pcm[k].forEach((x, i) => (out[i] += gain * x));
  });
  return out;
}

/** What the fake's `snapshot(master)` answers, read from its state when asked: the probe's scripted
 * stems, each track with its mix. */
function snapshotAnswer(master: boolean): ArrayBuffer {
  if (!webEngineFake.snapshotBytes) return encodeSessionBytes({ rate: fakeRate(), masterLengthFrames: 0, bpm: 120, tracks: [] }, []).buffer;
  const { header, pcm } = splitSessionBytes(webEngineFake.snapshotBytes.slice(0));
  const scripted = header as Omit<SnapshotHeader, 'tracks'> & { tracks: (Omit<SnapshotTrack, 'mix'> & { mix?: LaneMix })[] };
  const stems: SnapshotHeader = { ...scripted, tracks: scripted.tracks.map((t) => ({ ...t, mix: t.mix ?? copyMix(fakeMixes[t.index]) })) };
  if (!master) return encodeSessionBytes(stems, pcm).buffer;
  if (webEngineFake.masterError !== null) return encodeSessionBytes({ ...stems, masterError: webEngineFake.masterError }, pcm).buffer;
  const mono = fakeMaster(stems, pcm, webEngineFake.sent);
  return encodeSessionBytes({ ...stems, master: { frames: stems.masterLengthFrames } }, pcm, { left: mono, right: mono.slice() }).buffer;
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
  snapshotHold: null,
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
    commands.forEach(applyMixCommand);
  },
  async setShare(endpoint) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.shares.push(endpoint);
  },
  async snapshot(master) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.snapshots.push(master);
    const answer = snapshotAnswer(master);
    const hold = webEngineFake.snapshotHold;
    if (hold) await hold;
    return answer;
  },
  async loadSession(bytes) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.loadedSessions.push(bytes.slice());
    // The engine refuses a header it cannot read, and sets each lane's mix with its loop.
    applyMixLoad(decodeLoadSession(bytes.slice().buffer).header);
  },
  subscribe(onFrame) {
    engineSubscribers.add(onFrame);
    return () => engineSubscribers.delete(onFrame);
  },
  emit(raw) {
    const frame = decodeFeedFrame(raw);
    applyMixFrame(frame);
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
