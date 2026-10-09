/**
 * Browser implementation of the capability boundary. Zero Tauri/Rust dependency.
 * This is what `pnpm dev` runs against.
 */
import type { AppUpdate, AppUpdates, EngineHost, InputHost, LogFolder, MidiHost, Platform, PluginHost } from './host';
import {
  ENGINE_LANES,
  decodeFeedFrame,
  decodeLoadSession,
  encodeSessionBytes,
  splitSessionBytes,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type EngineEvent,
  type EngineToggle,
  type FeedFrame,
  type FxParamId,
  type LaneMix,
  type LaneState,
  type LoadHeader,
  type Refusal,
  type SnapshotHeader,
  type SnapshotTrack,
} from './engine-wire.ts'; // explicit .ts: Node guards import this file
import { decodeInputItem, decodeMidiEvent, type Dropped, type ImportReport, type InputEvent, type InputItem, type MidiEvent } from './midi-wire.ts';

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
  async pluginFolders() {
    return { builtin: [], user: [], unsupported: [] };
  },
  async addPluginFolder() {
    return null; // no native folder dialog in the browser build
  },
  async removePluginFolder() {
    return { builtin: [], user: [], unsupported: [] };
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
  /** Every engine command sent (`InputHost.send`'s engine items), in order, batches flattened. */
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
   * Probe seam, the engine's report: while set, the `Mix` each application owes waits (applied, not yet
   * reported), queued in order, one per changed lane per application (a lane going A, B, A owes three);
   * cleared, the queue is reported in order. A reset frame drops it.
   */
  holdEcho: boolean;
  /** Probe seam, the engine's application: while set, the commands a batch carries wait unapplied (and
   * unreported); cleared, they apply in order and their `Mix` follows. */
  holdApply: boolean;
  /** Probe seam: while set, a batch that carries a mix command is refused (`InputHost.send` rejects, as a
   * host that could not take it): its commands are recorded in `sent`, and nothing in it applies. */
  refuseMix: boolean;
  /**
   * Decode `raw` as a feed frame (the real decoder) and hand it to the subscribers, as the native feed
   * would. Only probes call it, through `__lf.native`. A frame's waveform `peaks` and its `scope`
   * columns pass straight through, so a probe scripts what the looper and the stage view draw; the
   * frames the fake makes itself (a lane's `Mix`, a toggle's answer) carry neither.
   * @public
   */
  emit(raw: unknown): void;
  /** Every input event the UI sent (`InputHost.send`'s input items: notes by owner, blurs, note targets,
   * panics), in order, batches flattened. Recorded only while the fake engine is on, as `sent`. */
  readonly inputSent: InputEvent[];
  /** Every `InputHost.send` call as it came (a refused one too): its epoch and its items, in order. */
  readonly batches: { epoch: number; items: InputItem[] }[];
  /** Probe seam: the next this many `InputHost.send` calls reject, as a batch lost on its way (only
   * `batches` has it; nothing in it applies). */
  failSends: number;
  /** Probe seam: while set, a batch `InputHost.send` takes is recorded and applied at once and answered
   * once this settles, as a call whose answer is still on its way (the outbox sends nothing else
   * meanwhile). */
  sendHold: Promise<void> | null;
  /** Probe seam: while set, every batch `InputHost.send` takes answers it, as native MIDI does when it
   * dropped some of the batch (a press with no device running); the batch is recorded and applied. */
  dropped: Dropped | null;
  /** Every native MIDI call (`MidiHost`), in order: its method's name and arguments, the page's epoch
   * first, e.g. `['learn', 1, 'recDub', null]` or `['forget', 1, revision, index]`; `['subscribe']`. */
  readonly midiCalls: [string, ...unknown[]][];
  /** Probe seam: while set, every native MIDI call but `subscribe` is recorded at once and answers once
   * this settles, as a call still on its way (one that never settles: the UI gives up on it in time). */
  midiHold: Promise<void> | null;
  /** Probe seam: what an edit by index (`forget`, `setMomentary`, `setHold`, `assign`) answers: false
   * stands for native MIDI refusing an edit made against an older list. */
  editAnswer: boolean;
  /**
   * Decode `raw` as a native MIDI event (its Rust serde JSON, the real decoder) and hand it to the
   * subscriber, as the native channel would. The fake answers `learn` and `cancelLearn` with a
   * `learning` event and nothing else on its own: no router, no learn (native MIDI's are Rust-tested).
   * Only probes call it, through `__lf.native`.
   * @public
   */
  midiEmit(raw: unknown): void;
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
  } else if ('SetPan' in c) {
    const m = at(c.SetPan[0]);
    const v = c.SetPan[1];
    const pan = Number.isFinite(v) ? clamp(v, -1, 1) : 0;
    // As the engine's mix carries it: the pan only off the centre.
    if (m && pan !== 0) m.pan = pan;
    else if (m) delete m.pan;
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
  } else if ('ActionOn' in c && c.ActionOn[1] === 'Mute') {
    // MUTE's toggle (the engine's refuses an EMPTY lane, which the fake does not know).
    const m = at(c.ActionOn[0]);
    if (m) m.muted = !m.muted;
  }
}

/** The commands `applyMixCommand` changes a lane's mix or a COPY latch with. */
const MIX_COMMANDS = ['SetVolume', 'SetMute', 'SetDubFeedback', 'SetPan', 'SetFxBypass', 'SetFxParam'] as const;
const isMixCommand = (c: EngineCommand): boolean =>
  typeof c === 'object' && (MIX_COMMANDS.some((k) => k in c) || ('ActionOn' in c && c.ActionOn[1] === 'Mute'));

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
    // A scripted report (a stale or foreign one): the UI last heard it, and the fake reports its own mix
    // again when that differs (`echoChanges`).
    else if (ev.type === 'Mix' && reported[ev.lane]) reported[ev.lane] = copyMix(ev.mix);
  }
  // A reset frame reports every lane's mix as it leaves it: what an engine before it owed is gone.
  if (frame.reset) {
    echoQueue.length = 0;
    fakeMixes.forEach((m, i) => (reported[i] = copyMix(m)));
  }
}

// ── The fake's `Mix`: each change of a lane's mix reported once, as the engine's feed does ─────────────

/** Each lane's mix as the fake last reported it. */
const reported: LaneMix[] = Array.from({ length: ENGINE_LANES }, defaultLaneMix);
/** The reports owed, in the order the changes were applied: each a copy of its lane's mix as that
 * application left it. */
const echoQueue: { readonly lane: number; readonly mix: LaneMix }[] = [];
let echoScheduled = false;
let echoHeld = false;
let applyHeld = false;
/** The commands `holdApply` keeps unapplied, in order. */
const heldCommands: EngineCommand[] = [];
/** The last meter a frame carried: a report repeats it (a frame without one reads the input silent). */
let lastMeter: FeedFrame['meter'] = null;

const sameLaneMix = (a: LaneMix, b: LaneMix): boolean => JSON.stringify(a) === JSON.stringify(b);

/** The last report lane `lane` owes, else the last one it gave. */
function lastOwed(lane: number): LaneMix {
  for (let i = echoQueue.length - 1; i >= 0; i--) if (echoQueue[i].lane === lane) return echoQueue[i].mix;
  return reported[lane];
}

/** After one application (a batch, a load, a scripted frame, the held commands' release): owe a report
 * for each lane whose mix now differs from the last one owed (or reported), and give them a microtask
 * later: never inside the call that changed the mix. Changes within one application coalesce into one
 * report per lane; separate applications queue separate reports (a lane going A, B, A owes three). */
function echoChanges(): void {
  fakeMixes.forEach((m, lane) => {
    if (!sameLaneMix(m, lastOwed(lane))) echoQueue.push({ lane, mix: copyMix(m) });
  });
  if (echoScheduled || echoHeld || echoQueue.length === 0) return;
  echoScheduled = true;
  queueMicrotask(() => {
    echoScheduled = false;
    if (!echoHeld) flushEchoes();
  });
}

/** Report each owed lane mix, in order, that differs from the last reported: one frame per report. */
function flushEchoes(): void {
  const owed = echoQueue.splice(0);
  for (const { lane, mix } of owed) {
    if (sameLaneMix(mix, reported[lane])) continue;
    reported[lane] = mix;
    const frame: FeedFrame = {
      seq: 0,
      reset: false,
      events: [{ type: 'Mix', frame: 0, lane, mix: copyMix(mix) }],
      device: [],
      anchor: null,
      meter: lastMeter,
      peaks: [],
      scope: null,
    };
    for (const onFrame of engineSubscribers) onFrame(frame);
  }
}

// ── The fake's toggles: CLICK, END STOP, FIXED, RETAKE, AUTO REC and each input send, as the engine owns them ──

/** Each toggled setting as the fake holds it, by `toggleKey`: a setter sets it, a toggle switches it. */
const fakeToggles = new Map<string, boolean>();
const toggleKey = (t: EngineToggle): string => (typeof t === 'string' ? t : `Send:${t.Send}`);
/** What the fake knows of the looper for the toggles' gate, as the scripted frames left it: each lane's
 * state, the master, the tempo's lock, the selected lane (a refusal's lane: the scripted `Selected`, not
 * a `SelectTrack` the UI sent, which the fake does not answer). */
const gateView = { states: Array.from({ length: ENGINE_LANES }, (): LaneState => 'Empty'), master: 0, locked: false, selected: 0 };

/** The setters that set a toggled setting outright: the setting and the value. */
function setterToggle(c: EngineCommand): [EngineToggle, boolean] | null {
  if (typeof c !== 'object') return null;
  if ('SetMetronome' in c) return ['Click', c.SetMetronome];
  if ('SetLoopEndStop' in c) return ['EndStop', c.SetLoopEndStop];
  if ('SetFixedLength' in c) return ['Fixed', c.SetFixedLength];
  if ('SetRetake' in c) return ['Retake', c.SetRetake];
  if ('SetAutoRecord' in c) return ['AutoRec', c.SetAutoRecord];
  if ('SetInputSend' in c) return [{ Send: c.SetInputSend[0] }, c.SetInputSend[1]];
  return null;
}

/** The engine's `Looper::toggle_gate`: FIXED, RETAKE and AUTO REC not while a lane records or overdubs,
 * FIXED not under RETAKE over a loop, AUTO REC not once the tempo is locked. */
function toggleRefusal(key: string): Refusal | null {
  const capturing = gateView.states.some((s) => s === 'Recording' || s === 'Overdubbing');
  if (key === 'Fixed' && capturing) return 'FixedCapturing';
  if (key === 'Fixed' && fakeToggles.get('Retake') === true && gateView.master > 0) return 'FixedRetake';
  if (key === 'Retake' && capturing) return 'RetakeCapturing';
  if (key === 'AutoRec' && capturing) return 'AutoRecCapturing';
  if (key === 'AutoRec' && gateView.locked) return 'AutoRecLocked';
  return null;
}

/** What the fake answers on the feed for the toggles it applied, a microtask later (`flushToggles`). */
const toggleAnswers: EngineEvent[] = [];
let togglesScheduled = false;

/** Apply `c` to the fake's toggles as the engine does, each applied command answered once: a setter
 * sets one and is answered by `Toggled` (changed or not); a toggle (`{Action:{Toggle}}`, or `ActionOn`
 * with its lane ignored) is judged and switched, answered by `Toggled` or by `Refused` on the selected
 * lane. */
function applyToggleCommand(c: EngineCommand): void {
  const set = setterToggle(c);
  const action = set || typeof c !== 'object' ? null : 'Action' in c ? c.Action : 'ActionOn' in c ? c.ActionOn[1] : null;
  if (set) {
    fakeToggles.set(toggleKey(set[0]), set[1]);
    toggleAnswers.push({ type: 'Toggled', frame: 0, toggle: set[0], on: set[1] });
  } else if (action === null || typeof action !== 'object' || !('Toggle' in action)) {
    return;
  } else {
    const key = toggleKey(action.Toggle);
    const reason = toggleRefusal(key);
    if (reason) toggleAnswers.push({ type: 'Refused', frame: 0, lane: gateView.selected, reason });
    else {
      const on = !(fakeToggles.get(key) ?? false);
      fakeToggles.set(key, on);
      toggleAnswers.push({ type: 'Toggled', frame: 0, toggle: action.Toggle, on });
    }
  }
  if (togglesScheduled) return;
  togglesScheduled = true;
  queueMicrotask(flushToggles);
}

/** Hand the toggles' answers to the UI, one frame: never inside the call that sent the toggle. */
function flushToggles(): void {
  togglesScheduled = false;
  const events = toggleAnswers.splice(0);
  if (events.length === 0) return;
  const frame: FeedFrame = { seq: 0, reset: false, events, device: [], anchor: null, meter: lastMeter, peaks: [], scope: null };
  for (const onFrame of engineSubscribers) onFrame(frame);
}

/** A scripted frame's looper state and toggles, for the gate: a reset starts from an empty looper and
 * the settings it carries. */
function applyToggleFrame(frame: Pick<FeedFrame, 'reset' | 'settings' | 'events'>): void {
  if (frame.reset) {
    gateView.states.fill('Empty');
    gateView.master = 0;
    gateView.locked = false;
    gateView.selected = 0;
    fakeToggles.clear();
    frame.settings?.forEach(applyToggleCommand);
  }
  for (const ev of frame.events) {
    if (ev.type === 'Lane') gateView.states[ev.lane] = ev.info.state;
    else if (ev.type === 'Transport') {
      gateView.master = ev.master;
      gateView.locked = ev.locked;
    } else if (ev.type === 'Selected') gateView.selected = ev.lane;
    else if (ev.type === 'Toggled') fakeToggles.set(toggleKey(ev.toggle), ev.on);
  }
}

/** A command as the fake applies it: a lane's mix, and the toggles. */
function applyCommand(c: EngineCommand): void {
  applyMixCommand(c);
  applyToggleCommand(c);
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
    applyMixCommand({ SetPan: [lane, mix.pan ?? 0] });
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

// ── The fake's native MIDI and input: recorded, never run ───────────────────────────────────────────

const midiCalls: [string, ...unknown[]][] = [];
const inputSent: InputEvent[] = [];
const batches: { epoch: number; items: InputItem[] }[] = [];
let midiSubscriber: ((event: MidiEvent) => void) | null = null;
/** The last input epoch the fake's subscribe answered. */
let fakeEpoch = 0;
/** The learn the fake was asked for, for its `learning` answers and `cancelLearn`'s. */
let fakeLearning: { action: string; target: number | null } | null = null;

/** Hand `raw` (the Rust serde JSON) to the subscriber a microtask later, as the native channel would. */
function emitMidi(raw: unknown): void {
  const event = decodeMidiEvent(raw);
  queueMicrotask(() => midiSubscriber?.(event));
}

/**
 * Pre-boot probe seams, read when the boot reaches them (an init script sets them before the app loads):
 * `__lfMidiSubscribeHold`, a promise the subscribe's epoch waits for (input queued meanwhile waits with it),
 * and `__lfMidiImportAnswer`, what the boot's `importLegacy` answers (`'fail'`: it rejects).
 */
const preBoot = () =>
  globalThis as { __lfMidiSubscribeHold?: Promise<void>; __lfMidiImportAnswer?: ImportReport | 'fail' };

/** Record a native MIDI call at once; its answer waits for `midiHold` while a probe holds it. */
async function midiCall<T>(call: [string, ...unknown[]], answer: () => T): Promise<T> {
  midiCalls.push(call);
  const hold = webEngineFake.midiHold;
  if (hold) await hold;
  return answer();
}

const webMidi: MidiHost = {
  subscribe(onEvent) {
    midiCalls.push(['subscribe']);
    midiSubscriber = onEvent;
    const epoch = ++fakeEpoch;
    const hold = preBoot().__lfMidiSubscribeHold;
    return {
      epoch: hold ? hold.then(() => epoch) : Promise.resolve(epoch),
      stop() {
        if (midiSubscriber === onEvent) midiSubscriber = null;
      },
    };
  },
  learn(epoch, action, target) {
    return midiCall(['learn', epoch, action, target], () => {
      fakeLearning = { action, target };
      emitMidi({ learning: { learning: fakeLearning } });
      return true;
    });
  },
  cancelLearn(epoch) {
    return midiCall(['cancelLearn', epoch], () => {
      const was = fakeLearning !== null;
      fakeLearning = null;
      emitMidi({ learning: { learning: null } });
      return was;
    });
  },
  forget(epoch, revision, index) {
    return midiCall(['forget', epoch, revision, index], () => webEngineFake.editAnswer);
  },
  setMomentary(epoch, revision, index, momentary) {
    return midiCall(['setMomentary', epoch, revision, index, momentary], () => webEngineFake.editAnswer);
  },
  setHold(epoch, revision, index, hold) {
    return midiCall(['setHold', epoch, revision, index, hold], () => webEngineFake.editAnswer);
  },
  assign(epoch, revision, index, portId) {
    return midiCall(['assign', epoch, revision, index, portId], () => webEngineFake.editAnswer);
  },
  importLegacy(epoch, json) {
    return midiCall(['importLegacy', epoch, json], () => {
      const scripted = preBoot().__lfMidiImportAnswer;
      if (scripted === 'fail') throw new Error('The fake refused the import (__lfMidiImportAnswer)');
      return scripted ?? { already: false, unreadable: null, imported: [], blocked: [], rejected: [], skipped: [] };
    });
  },
};

const webInput: InputHost = {
  async send(epoch, items) {
    if (!engineForced()) throw new Error(NO_ENGINE);
    const read = items.map(decodeInputItem);
    batches.push({ epoch, items: read });
    if (webEngineFake.failSends > 0) {
      webEngineFake.failSends--;
      throw new Error('The fake lost the batch (failSends)');
    }
    const commands = read.flatMap((item) => ('engine' in item ? [item.engine] : []));
    if (webEngineFake.refuseMix && commands.some(isMixCommand)) {
      // It reached the host, which ran none of it.
      webEngineFake.sent.push(...commands);
      throw new Error('The fake engine refused the batch (refuseMix)');
    }
    for (const item of read) {
      if ('engine' in item) runCommand(item.engine);
      else inputSent.push(item.input);
    }
    echoChanges();
    const answer = webEngineFake.dropped;
    const hold = webEngineFake.sendHold;
    if (hold) await hold;
    return answer;
  },
};

/** One engine command, as the fake takes it: recorded, then applied, or held while `holdApply` is set. */
function runCommand(command: EngineCommand): void {
  webEngineFake.sent.push(command);
  if (applyHeld) heldCommands.push(command);
  else applyCommand(command);
}

/**
 * The engine host's browser stand-in: the browser build has no engine (`available` false) unless a DEV
 * probe forces this fake on. Forced on, it answers `open()` with a canned device, records every batch in `sent` and hands a
 * probe-scripted frame from `emit()` to the subscribers. Not a second looper: what it answers by itself is
 * a lane's mix, reported as the engine's `Mix` once it changes (a mix command, a scripted COPY, CLEAR or
 * MUTE, a load), and a toggle (`{Action:{Toggle}}`), switched or refused as the engine's gate would on the
 * looper the scripted frames show, answered by `Toggled` or `Refused`; every other state a probe asserts
 * on is scripted.
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
  get holdEcho() {
    return echoHeld;
  },
  set holdEcho(on: boolean) {
    echoHeld = on;
    if (!on) echoChanges();
  },
  get holdApply() {
    return applyHeld;
  },
  set holdApply(on: boolean) {
    applyHeld = on;
    if (on) return;
    heldCommands.splice(0).forEach(applyCommand);
    echoChanges();
  },
  refuseMix: false,
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
    echoChanges();
  },
  subscribe(onFrame) {
    engineSubscribers.add(onFrame);
    return () => engineSubscribers.delete(onFrame);
  },
  inputSent,
  batches,
  failSends: 0,
  sendHold: null,
  dropped: null,
  midiCalls,
  midiHold: null,
  editAnswer: true,
  midiEmit: emitMidi,
  emit(raw) {
    const frame = decodeFeedFrame(raw);
    applyMixFrame(frame);
    applyToggleFrame(frame);
    if (frame.meter) lastMeter = frame.meter;
    for (const onFrame of engineSubscribers) onFrame(frame);
    // A scripted COPY, CLEAR or pedal MUTE changed a lane's mix: its report follows, as the engine's.
    echoChanges();
  },
};

export const webPlatform: Platform = {
  kind: 'web',
  pluginHost: webPluginHost,
  engine: webEngineFake,
  logs: webLogFolder,
  updates: webUpdates,
  midi: webMidi,
  input: webInput,
};
