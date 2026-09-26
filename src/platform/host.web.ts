/**
 * Browser implementation of the capability boundary. Zero Tauri/Rust dependency.
 * This is what `pnpm dev` runs against.
 */
import type {
  AudioInputSource,
  EngineHost,
  MidiBackend,
  OpenedInput,
  Platform,
  PluginHost,
} from './host';
import {
  decodeFeedFrame,
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
  async loadPlugin(_slot, _path, _id, _loadToken) {
    throw new Error(NO_NATIVE_HOST);
  },
  async unloadPlugin() {
    /* no-op */
  },
  async listLoaded() {
    return []; // no native host in the browser build
  },
  async noteOn() {
    /* no-op — no native plugin in the browser build */
  },
  async noteOff() {
    /* no-op */
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
  onStreamFault() {
    return () => {}; // no native cpal streams in the browser build ⇒ nothing can fault
  },
  async saveState() {
    // No live plugin in the browser build — loadPlugin already threw before any caller gets here.
    throw new Error(NO_NATIVE_HOST);
  },
  async loadState() {
    /* no-op */
  },
  async listInputDevices() {
    return [];
  },
  async armInput() {
    // Unreachable from the app (the arm UI is gated on `available`, and no plugin can load in the
    // browser build) — throw for parity with loadPlugin/openEditor rather than silently succeeding.
    throw new Error(NO_NATIVE_HOST);
  },
  async disarmInput() {
    /* no-op — no native input in the browser build */
  },
  async listOutputDevices() {
    return [];
  },
  async armMonitor() {
    // Unreachable from the app (the monitor UI is gated on `available`, and no plugin can load in the
    // browser build) — throw for parity with armInput rather than silently succeeding.
    throw new Error(NO_NATIVE_HOST);
  },
  async disarmMonitor() {
    /* no-op — no native monitor in the browser build */
  },
  async setMonitorGain() {
    /* no-op — no native monitor in the browser build */
  },
  async setMasterGain() {
    /* no-op — no native monitor in the browser build */
  },
  async monitorLatencySeconds() {
    return 0; // no native monitor in the browser build ⇒ no record-latency compensation
  },
  async setBufferSize() {
    /* no-op — no native audio pipeline in the browser build */
  },
  async asioAvailable() {
    return false; // no native ASIO host in the browser build
  },
  async asioStatus() {
    return { status: 'not-compiled' as const, detail: '' };
  },
  async asioProbe() {
    return { status: 'not-compiled' as const, detail: '' };
  },
  async asioDeviceInfo() {
    return null;
  },
  async setAsioEnabled() {
    /* no-op — no native audio pipeline in the browser build */
  },
};

const webAudioInput: AudioInputSource = {
  async open(ctx, options): Promise<OpenedInput | null> {
    if (!navigator.mediaDevices?.getUserMedia) return null;
    const requestedChannel = options?.channel;
    const channel =
      typeof requestedChannel === 'number' &&
      Number.isInteger(requestedChannel) &&
      requestedChannel >= 0
        ? requestedChannel
        : null;
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
        // An explicit Ch 1 pick still needs at least two discrete lanes. Requesting one lets the
        // browser down-mix Ch 1 + Ch 2 before Web Audio can isolate either of them. Higher indexes
        // require enough returned channels to reach that splitter output; failure rejects and is
        // surfaced by the existing MIC arm toast rather than silently opening a different lane.
        channelCount: channel === null ? 1 : { min: Math.max(2, channel + 1) },
      },
    });
    const tracks = stream.getTracks();
    let source: MediaStreamAudioSourceNode | null = null;
    let splitter: ChannelSplitterNode | null = null;
    let selectedMono: GainNode | null = null;
    const release = (): void => {
      for (const track of tracks) track.stop();
      source?.disconnect();
      splitter?.disconnect();
      selectedMono?.disconnect();
    };
    let node: AudioNode;
    // Wiring can throw after getUserMedia resolved (e.g. a channel the context cannot split). No close
    // handle exists yet, so release the live tracks + created nodes here and rethrow: the MIC arm
    // toast surfaces the error.
    try {
      source = ctx.createMediaStreamSource(stream);
      node = source;
      if (channel !== null) {
        splitter = ctx.createChannelSplitter(channel + 1);
        selectedMono = ctx.createGain();
        selectedMono.channelCount = 1;
        selectedMono.channelCountMode = 'explicit';
        selectedMono.channelInterpretation = 'discrete';
        source.connect(splitter);
        splitter.connect(selectedMono, channel);
        node = selectedMono;
      }
    } catch (err) {
      release();
      throw err;
    }
    // A yanked interface ENDS its tracks; the MediaStreamAudioSourceNode stays in the graph and just
    // produces silence, so 'ended' is the only signal the caller can act on. Latched + unsubscribed on
    // the first report, so a multi-track stream reports once and `close()`'s own `track.stop()` (which
    // must not fire it at all) can't turn a user disarm into a loss.
    let dead = false;
    const onEnded = (): void => {
      if (dead) return;
      dead = true;
      for (const track of tracks) track.removeEventListener('ended', onEnded);
      options?.onLost?.();
    };
    for (const track of tracks) track.addEventListener('ended', onEnded);
    return {
      node,
      sampleRate: ctx.sampleRate,
      close() {
        dead = true;
        for (const track of tracks) track.removeEventListener('ended', onEnded);
        release();
      },
    };
  },
};

const webMidi: MidiBackend = {
  async requestAccess() {
    if (!navigator.requestMIDIAccess) return null;
    return navigator.requestMIDIAccess({ sysex: false });
  },
};

/** The web engine fake (below) plus what a probe reads and scripts through `__lf.native`. */
export interface EngineFake extends EngineHost {
  /** Every command sent, in order (batches flattened). */
  readonly sent: EngineCommand[];
  /** Every request `open()` received. */
  readonly opened: DeviceRequest[];
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

/**
 * The engine host's browser stand-in: engine mode is off (`available` false) unless a DEV probe forces
 * it on. Forced on, it answers `open()` with a canned device, records every batch in `sent` and hands a
 * probe-scripted frame from `emit()` to the subscribers. Not a second looper: nothing answers a command
 * by itself, so a probe asserts gesture → command and frame → DOM.
 */
export const webEngineFake: EngineFake = {
  get available() {
    return engineForced();
  },
  sent: [],
  opened: [],
  async mode() {
    return engineForced();
  },
  async setMode() {
    if (!engineForced()) throw new Error(NO_ENGINE);
  },
  async open(request) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.opened.push(request);
    fakeStatus = {
      backend: request.backend,
      sampleRate: 48000,
      block: request.buffer ?? 256,
      inputName: 'Fake input',
      outputName: 'Fake output',
      alignFrames: 0,
      inputFrames: 0,
    };
    return fakeStatus;
  },
  async close() {
    fakeStatus = null;
  },
  async status() {
    return fakeStatus;
  },
  async setInputChannel() {
    if (!engineForced()) throw new Error(NO_ENGINE);
  },
  async send(commands) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    webEngineFake.sent.push(...commands);
  },
  async setShare() {
    if (!engineForced()) throw new Error(NO_ENGINE);
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
  audioInput: webAudioInput,
  midi: webMidi,
};
