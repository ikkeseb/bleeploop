import { batch, createSignal, type Accessor } from 'solid-js';
import {
  ENGINE_LANES,
  decodeSnapshot,
  encodeSessionBytes,
  platform,
  sendEngine,
  type DeviceEvent,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type EngineEvent,
  type FeedFrame,
  type FxParamId,
  type LoadHeader,
  type LaneInfo,
  type LaneState,
} from '../../platform';
import type { PeakView, TrackState } from '../../audio/looper/looper';
import { FX_META, FX_PARAM_DEFS, validateFxStates, type FxState } from '../../audio/fx/metadata';
import type { SessionSource } from '../../audio/export/session-source';
import type { StemSnapshot } from '../../audio/export/stem-archive';
import type { LoadSessionPayload } from '../../audio/looper/session';
import { AUTO_RECORD_DEFAULT_SENSITIVITY } from '../../audio/looper/auto-record';
import { averageInterval, framesPerBar, maxWholeBars } from '../../audio/quantize';
import { readStoredNumber, writeStoredNumber } from '../../audio/persist';
import { readAudioDeviceSettings, writeAudioDeviceSettings } from '../../audio/audio-settings';
import { usingAsio } from '../../audio/audio-devices';
import { engineResync } from '../../audio/instrument';
import { engineInputLive, toggleEngineInput } from '../../audio/native-io';
import { notifyError, notifyInfo } from '../../notify';

/**
 * OWNS: engine mode's view of the native engine (`docs/plans/native-engine.md` § Stage 5, UI side): the
 * feed reducer, the device that runs, and the `looper` / `clock` / `master` shapes the UI reads, built on
 * engine commands and the feed. `audio.ts` beside this file picks these or the web modules by mode, so
 * the components change only their import.
 *
 * The engine owns the musical state: lanes, the transport (master, BPM and its lock), the beat and the
 * selection arrive on the feed, and nothing here predicts them. It does not echo settings, so this store
 * keeps them (lane volume, mute and FX, the take modes, click, master): it sends each change, mirrors the
 * engine's CLEAR (`Cleared`: the lane's mix resets) and COPY (`Copied`), and on a `reset` frame takes the
 * settings the engine remembers, so the screen shows what the engine plays. `engineSession` is the
 * engine as export, recovery and import see it: the engine's PCM with this store's mix.
 *
 * Invariant 6: a frame writes a Solid signal only when its value changed; the waveform rAF reads the
 * plain mirror (`plain`) and extrapolates the playhead from the feed's clock anchor.
 */

/** The web looper's public track shape (`TrackPublic`), filled from the feed's `LaneInfo`. */
interface TrackView {
  readonly state: TrackState;
  readonly lengthFrames: number;
  readonly armed: boolean;
  readonly autoArmed: boolean;
  readonly canUndo: boolean;
  readonly canReverse: boolean;
  readonly reversed: boolean;
  /** The frame of a pending loop-end stop (the web holds a ctx time; the UI tests only for null). */
  readonly stopAt: number | null;
  readonly retakePass: number;
}

const TRACK_STATE: Record<LaneState, TrackState> = {
  Empty: 'EMPTY',
  Recording: 'RECORDING',
  Overdubbing: 'OVERDUBBING',
  Playing: 'PLAYING',
  Stopped: 'STOPPED',
};

const EMPTY_INFO: LaneInfo = {
  state: 'Empty',
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  retakePass: 0,
};

function trackView(info: LaneInfo): TrackView {
  return {
    state: TRACK_STATE[info.state],
    lengthFrames: info.length,
    armed: info.armed,
    autoArmed: info.autoArmed,
    canUndo: info.canUndo,
    canReverse: info.canReverse,
    reversed: info.reversed,
    stopAt: info.stopAt,
    retakePass: info.retakePass,
  };
}

function sameTrack(a: TrackView, b: TrackView): boolean {
  return (
    a.state === b.state &&
    a.lengthFrames === b.lengthFrames &&
    a.armed === b.armed &&
    a.autoArmed === b.autoArmed &&
    a.canUndo === b.canUndo &&
    a.canReverse === b.canReverse &&
    a.reversed === b.reversed &&
    a.stopAt === b.stopAt &&
    a.retakePass === b.retakePass
  );
}

const lanes = Array.from({ length: ENGINE_LANES }, () => createSignal<TrackView>(trackView(EMPTY_INFO), { equals: sameTrack }));
const [masterFrames, setMasterFrames] = createSignal(0);
const [bpm, setBpmSignal] = createSignal(120);
const [bpmLocked, setBpmLocked] = createSignal(false);
const [selectedTrack, setSelectedTrack] = createSignal(0);
const [beat, setBeat] = createSignal(0);
const [countLeft, setCountLeft] = createSignal(0);
const [device, setDeviceSignal] = createSignal<DeviceStatus | null>(null);

// ── The settings this store keeps (the engine does not echo them) ──────────────────────────────────

const MAX_FIXED_BARS = 32; // the web looper's bar selector bound (`state.ts`)
const MASTER_KEY = 'lf.masterVolume'; // shared with `master.ts`: one saved level for both paths
const CLICK_KEY = 'lf.clickVolume'; // shared with `clock.ts`

const volumes = Array.from({ length: ENGINE_LANES }, () => createSignal(1));
const mutes = Array.from({ length: ENGINE_LANES }, () => createSignal(false));
const fxVersions = Array.from({ length: ENGINE_LANES }, () => createSignal(0));
const fx: FxState[][] = Array.from({ length: ENGINE_LANES }, defaultFx);
const [loopEndStop, setLoopEndStopSignal] = createSignal(false);
const [fixedLength, setFixedLengthSignal] = createSignal(false);
const [fixedBars, setFixedBarsSignal] = createSignal(4);
const [retake, setRetakeSignal] = createSignal(false);
const [autoRecord, setAutoRecordSignal] = createSignal(false);
const [autoSensitivity, setAutoSensitivitySignal] = createSignal(AUTO_RECORD_DEFAULT_SENSITIVITY);
const [metronome, setMetronomeSignal] = createSignal(false);
const [clickVolume, setClickVolumeSignal] = createSignal(readStoredNumber(CLICK_KEY, 0.7, 0, 1));
const [masterVolume, setMasterVolumeSignal] = createSignal(readStoredNumber(MASTER_KEY, 1, 0, 1));
const [masterMuted, setMasterMutedSignal] = createSignal(false);

function defaultFx(): FxState[] {
  return FX_META.map((m) => ({
    bypassed: true,
    params: Object.fromEntries(FX_PARAM_DEFS[m.kind].map((d) => [d.key, d.default])),
  }));
}

// ── The plain mirror the 60 fps draw loop reads (invariant 6) ──────────────────────────────────────

interface LanePeaks {
  min: Float32Array;
  max: Float32Array;
  count: number;
  version: number;
}

const plain = {
  state: Array.from({ length: ENGINE_LANES }, (): TrackState => 'EMPTY'),
  waiting: Array.from({ length: ENGINE_LANES }, () => false),
  muted: Array.from({ length: ENGINE_LANES }, () => false),
  retakePass: Array.from({ length: ENGINE_LANES }, () => 0),
  /** The master boundary a later take started on (its record head counts from here). */
  takeStart: Array.from({ length: ENGINE_LANES }, () => 0),
  master: 0,
  /** The feed's clock anchor, with the master grid's (`grid`); `rate` 0 while no device runs (the
   * playhead holds still). */
  clock: { frame: 0, atMs: 0, rate: 0, grid: 0 },
  /** Frames between a rendered frame and the moment it is heard (the output latency). */
  heardLag: 0,
  level: 0,
  clip: false,
  peaks: Array.from({ length: ENGINE_LANES }, (): LanePeaks => ({
    min: new Float32Array(0),
    max: new Float32Array(0),
    count: 0,
    version: 0,
  })),
};

/** The device frame the engine renders now, extrapolated from the last clock anchor. */
function renderedFrame(): number {
  const c = plain.clock;
  return c.rate > 0 ? c.frame + ((Date.now() - c.atMs) * c.rate) / 1000 : c.frame;
}

/** The device frame heard now: what was rendered one output latency ago. */
function heardFrame(): number {
  return renderedFrame() - plain.heardLag;
}

function phaseValue(): number {
  const m = plain.master;
  if (m <= 0) return 0;
  const p = (heardFrame() - plain.clock.grid) % m;
  return (p < 0 ? p + m : p) / m;
}

/** A later take's record head, 0..1 of the master; -1 before a master exists (first take). */
function recHeadFrac(i: number): number {
  const m = plain.master;
  if (m <= 0) return -1;
  if (plain.waiting[i]) return phaseValue();
  const f = (heardFrame() - plain.takeStart[i]) / m;
  return f < 0 ? 0 : f > 1 ? 1 : f;
}

/** The master boundary nearest `frame`: a lane event lands within a block of the boundary its take began on. */
function nearestBoundary(frame: number): number {
  const m = plain.master;
  if (m <= 0) return frame;
  const grid = plain.clock.grid;
  return grid + Math.round((frame - grid) / m) * m;
}

function peaksInto(i: number, out: PeakView): PeakView {
  const p = plain.peaks[i];
  out.min = p.min;
  out.max = p.max;
  out.count = p.count;
  out.version = p.version;
  return out;
}

// ── The feed reducer ──────────────────────────────────────────────────────────────────────────────

type EventListener = (ev: EngineEvent) => void;
const eventListeners = new Set<EventListener>();

/** Hear every engine event after the store applied it (the app puts a refusal on its lane; the native
 * smoke probe records beats). Returns the unsubscribe. */
export function onEngineEvent(listener: EventListener): () => void {
  eventListeners.add(listener);
  return () => eventListeners.delete(listener);
}

function applyLane(lane: number, frame: number, info: LaneInfo): void {
  const prev = plain.state[lane];
  const next = TRACK_STATE[info.state];
  const waiting = info.armed || info.autoArmed;
  if (next === 'RECORDING' && !waiting && (prev !== 'RECORDING' || plain.waiting[lane] || info.retakePass !== plain.retakePass[lane])) {
    plain.takeStart[lane] = nearestBoundary(frame);
  }
  plain.state[lane] = next;
  plain.waiting[lane] = waiting;
  plain.retakePass[lane] = info.retakePass;
  lanes[lane][1](trackView(info));
}

function applyEvent(ev: EngineEvent): void {
  switch (ev.type) {
    case 'Lane':
      applyLane(ev.lane, ev.frame, ev.info);
      break;
    case 'Transport':
      plain.master = ev.master;
      setMasterFrames(ev.master);
      setBpmSignal(ev.bpm);
      setBpmLocked(ev.locked);
      break;
    case 'Beat':
      setBeat(ev.beatInBar % 4);
      setCountLeft(ev.countLeft);
      break;
    case 'Selected':
      setSelectedTrack(ev.lane);
      break;
    case 'TakeRejected':
      console.error(`[engine] track ${ev.lane}'s ${ev.overdub ? 'overdub layer' : 'take'} rejected: input gap`);
      notifyError(
        `Track ${ev.lane + 1}: ${ev.overdub ? 'overdub layer' : 'take'} discarded`,
        'The audio input dropped out while it recorded. No damaged audio was kept; try again.',
      );
      break;
    case 'PassDropped':
      console.error(`[engine] track ${ev.lane}'s retake pass ${ev.pass} dropped: input gap`);
      notifyError(
        `Track ${ev.lane + 1}: take ${ev.pass} dropped`,
        'The audio input dropped out during it, so it was not kept, nor the take before it.',
      );
      break;
    case 'Copied':
      copyLaneMix(ev.from, ev.to);
      break;
    case 'Cleared':
      clearLaneMix(ev.lane);
      break;
  }
}

function applyDeviceEvent(ev: DeviceEvent): void {
  switch (ev.type) {
    case 'Lost':
      console.error(`[engine] ${ev.backend} device lost: ${ev.reason}`);
      notifyError('Audio device lost', ev.reason);
      break;
    case 'Recovered':
      notifyInfo('Audio device back', deviceLabel(ev.status));
      break;
    case 'Fallback':
      console.error(`[engine] fell back to ${deviceLabel(ev.status)}`);
      notifyInfo('Switched to another audio device', deviceLabel(ev.status));
      break;
    case 'ShareLost':
      console.error(`[engine] share output lost: ${ev.reason}`);
      notifyError('Share output stopped', ev.reason);
      forgetShare();
      break;
    case 'EngineFaulted':
      console.error('[engine] the engine faulted and was replaced');
      notifyError('The audio engine restarted', 'The loops were lost.');
      break;
  }
}

const deviceWaiters: ((status: DeviceStatus) => void)[] = [];

function setDevice(status: DeviceStatus | null): void {
  // Once per open: WASAPI may run output only (no capture device, a microphone Windows blocks).
  if (status && !status.inputOpen && (device()?.inputOpen ?? true)) {
    console.error(`[engine] ${status.outputName} opened without an input`);
    notifyError('The audio input did not open', 'The engine plays, but hears nothing. Check the input device and Windows microphone privacy.');
  }
  // No device: the playhead holds where it is until the next anchor arrives.
  if (!status) plain.clock = { ...plain.clock, frame: renderedFrame(), atMs: Date.now(), rate: 0 };
  plain.heardLag = status ? Math.max(0, status.alignFrames - status.inputFrames) : 0;
  setDeviceSignal(status);
  if (status) for (const resolve of deviceWaiters.splice(0)) resolve(status);
}

/** Resolves once a device runs (at once when one does). */
export function whenDevice(): Promise<DeviceStatus> {
  const running = device();
  return running ? Promise.resolve(running) : new Promise((resolve) => deviceWaiters.push(resolve));
}

function applyPeaks(lane: number, start: number, count: number, min: readonly number[], max: readonly number[]): void {
  const p = plain.peaks[lane];
  const need = Math.max(count, start + min.length);
  if (p.min.length < need) {
    const size = Math.max(need, p.min.length * 2, 256);
    const grownMin = new Float32Array(size);
    const grownMax = new Float32Array(size);
    grownMin.set(p.min.subarray(0, p.count));
    grownMax.set(p.max.subarray(0, p.count));
    p.min = grownMin;
    p.max = grownMax;
  }
  p.min.set(min, start);
  p.max.set(max, start);
  p.count = count;
  p.version++;
}

/**
 * Apply one feed frame, its signal writes as one batch. A `reset` frame REPLACES the view: a lane, the
 * transport or the selection it leaves out goes back to its empty default, the peaks are redrawn from
 * the frame alone, and the settings come from the engine's memory (`adoptSettings`).
 */
function applyFrame(f: FeedFrame): void {
  batch(() => applyFrameNow(f));
}

function applyFrameNow(f: FeedFrame): void {
  if (f.reset) {
    for (const p of plain.peaks) {
      p.count = 0;
      p.version++;
    }
    setCountLeft(0);
  }
  // The device and its clock first: the anchor is read after the frame's events, so a take the events
  // start snaps to the grid it carries.
  if (f.status !== undefined) setDevice(f.status);
  if (f.anchor) plain.clock = f.anchor;
  if (f.reset) adoptSettings(f.settings ?? []);
  for (const ev of f.events) {
    applyEvent(ev);
    for (const listener of eventListeners) listener(ev);
  }
  if (f.reset) {
    for (let i = 0; i < ENGINE_LANES; i++) {
      if (!f.events.some((ev) => ev.type === 'Lane' && ev.lane === i)) applyLane(i, 0, EMPTY_INFO);
    }
    if (!f.events.some((ev) => ev.type === 'Transport')) {
      applyEvent({ type: 'Transport', frame: 0, master: 0, bpm: bpm(), locked: false });
    }
    if (!f.events.some((ev) => ev.type === 'Selected')) setSelectedTrack(0);
  }
  for (const d of f.device) applyDeviceEvent(d);
  // No meter: no device runs, so the input reads silent.
  plain.level = f.meter?.peak ?? 0;
  plain.clip = f.meter?.clip ?? false;
  for (const p of f.peaks) applyPeaks(p.lane, p.start, p.count, p.min, p.max);
}

// ── The mix and the modes this store keeps ────────────────────────────────────────────────────────

function fxCommands(lane: number, states: readonly FxState[]): EngineCommand[] {
  const out: EngineCommand[] = [];
  FX_META.forEach((meta, k) => {
    out.push({ SetFxBypass: [lane, meta.kind, states[k].bypassed] });
    for (const def of FX_PARAM_DEFS[meta.kind]) out.push({ SetFxParam: [lane, def.key as FxParamId, states[k].params[def.key]] });
  });
  return out;
}

function laneCommands(lane: number): EngineCommand[] {
  return [{ SetVolume: [lane, volumes[lane][0]()] }, { SetMute: [lane, mutes[lane][0]()] }, ...fxCommands(lane, fx[lane])];
}

/** The engine cleared the lane: its mix is back to the defaults there (the web's `clear()`), so here too. */
function clearLaneMix(lane: number): void {
  volumes[lane][1](1);
  setMutePlain(lane, false);
  fx[lane] = defaultFx();
  fxVersions[lane][1]((v) => v + 1);
}

/** The engine copied lane `from` whole into `to` (its volume, mute and FX with it): mirror that. */
function copyLaneMix(from: number, to: number): void {
  volumes[to][1](volumes[from][0]());
  setMutePlain(to, mutes[from][0]());
  fx[to] = fx[from].map((s) => ({ bypassed: s.bypassed, params: { ...s.params } }));
  fxVersions[to][1]((v) => v + 1);
}

function setMutePlain(lane: number, on: boolean): void {
  mutes[lane][1](on);
  plain.muted[lane] = on;
}

/**
 * A reset frame: take over the settings the engine remembers (one missing from `settings` is at the
 * engine's default, which is the UI's) instead of pushing the UI's, so a WebView reload keeps a
 * playing session's mix and modes. The UI still sends what it persists and the engine lacks (the master
 * and click volumes on a first launch) and what it owns: the note target, the slot gains and the live
 * slot (`engineResync`).
 */
function adoptSettings(settings: readonly EngineCommand[]): void {
  setMasterMutedSignal(false);
  setMetronomeSignal(false);
  setLoopEndStopSignal(false);
  setFixedLengthSignal(false);
  setFixedBarsSignal(4);
  setRetakeSignal(false);
  setAutoRecordSignal(false);
  setAutoSensitivitySignal(AUTO_RECORD_DEFAULT_SENSITIVITY);
  for (let i = 0; i < ENGINE_LANES; i++) clearLaneMix(i);
  let masterKnown = false;
  let clickKnown = false;
  for (const c of settings) {
    if (typeof c === 'string') continue;
    if ('SetMasterVolume' in c) {
      masterKnown = true;
      setMasterVolumeSignal(c.SetMasterVolume);
      writeStoredNumber(MASTER_KEY, c.SetMasterVolume);
    } else if ('SetClickVolume' in c) {
      clickKnown = true;
      setClickVolumeSignal(c.SetClickVolume);
      writeStoredNumber(CLICK_KEY, c.SetClickVolume);
    } else if ('SetMasterMute' in c) setMasterMutedSignal(c.SetMasterMute);
    else if ('SetMetronome' in c) setMetronomeSignal(c.SetMetronome);
    else if ('SetLoopEndStop' in c) setLoopEndStopSignal(c.SetLoopEndStop);
    else if ('SetFixedLength' in c) setFixedLengthSignal(c.SetFixedLength);
    else if ('SetFixedBars' in c) setFixedBarsSignal(c.SetFixedBars);
    else if ('SetRetake' in c) setRetakeSignal(c.SetRetake);
    else if ('SetAutoRecord' in c) setAutoRecordSignal(c.SetAutoRecord);
    else if ('SetAutoSensitivity' in c) setAutoSensitivitySignal(c.SetAutoSensitivity);
    else if ('SetVolume' in c) volumes[c.SetVolume[0]][1](c.SetVolume[1]);
    else if ('SetMute' in c) setMutePlain(c.SetMute[0], c.SetMute[1]);
    else if ('SetFxBypass' in c) {
      const [l, kind, bypassed] = c.SetFxBypass;
      const k = FX_META.findIndex((m) => m.kind === kind);
      fx[l][k] = { ...fx[l][k], bypassed };
      fxVersions[l][1]((v) => v + 1);
    } else if ('SetFxParam' in c) {
      const [l, key, value] = c.SetFxParam;
      const k = FX_META.findIndex((m) => FX_PARAM_DEFS[m.kind].some((d) => d.key === key));
      fx[l][k] = { ...fx[l][k], params: { ...fx[l][k].params, [key]: value } };
      fxVersions[l][1]((v) => v + 1);
    }
    // The tempo and the selection arrive as events; the note target and the slots are the UI's.
  }
  const lacking: EngineCommand[] = [];
  if (!masterKnown) lacking.push({ SetMasterVolume: masterVolume() });
  if (!clickKnown) lacking.push({ SetClickVolume: clickVolume() });
  sendEngine(...lacking);
  engineResync();
}

const clampLane = (i: number) => Math.max(0, Math.min(ENGINE_LANES - 1, Math.trunc(i)));

function setVolume(i: number, v: number): void {
  const clamped = Math.max(0, Math.min(1.5, Number.isFinite(v) ? v : 1));
  volumes[i][1](clamped);
  sendEngine({ SetVolume: [i, clamped] });
}

function setMute(i: number, on: boolean): void {
  setMutePlain(i, on);
  sendEngine({ SetMute: [i, on] });
}

function setFxBypass(i: number, fxIndex: number, bypassed: boolean): void {
  const meta = FX_META[fxIndex];
  if (!meta) return;
  fx[i][fxIndex] = { ...fx[i][fxIndex], bypassed };
  fxVersions[i][1]((v) => v + 1);
  sendEngine({ SetFxBypass: [i, meta.kind, bypassed] });
}

function setFxParam(i: number, fxIndex: number, key: string, value: number): void {
  const meta = FX_META[fxIndex];
  const def = meta && FX_PARAM_DEFS[meta.kind].find((d) => d.key === key);
  if (!def || !Number.isFinite(value)) return;
  const bounded = Math.max(def.min, Math.min(def.max, value));
  const applied = def.integer ? Math.round(bounded) : bounded;
  const prev = fx[i][fxIndex];
  fx[i][fxIndex] = { ...prev, params: { ...prev.params, [key]: applied } };
  fxVersions[i][1]((v) => v + 1);
  sendEngine({ SetFxParam: [i, key as FxParamId, applied] });
}

function nextTakeMaxBars(): number {
  const master = masterFrames();
  return master > 0 ? Math.min(MAX_FIXED_BARS, maxWholeBars(master, framesPerBar(bpm(), engineSampleRate()))) : MAX_FIXED_BARS;
}

/** The first EMPTY lane (where COPY lands), or -1. */
function firstEmptyLane(): number {
  return plain.state.findIndex((s) => s === 'EMPTY');
}

/** Engine mode's `looper`: the facade the UI reads (`src/audio/looper/looper.ts` has the web one). */
export const engineLooper = {
  trackCount: ENGINE_LANES as typeof ENGINE_LANES,
  recDub: async (i: number): Promise<void> => sendEngine({ RecDub: i }),
  playStop: (i: number): void => sendEngine({ PlayStop: i }),
  stop: (i: number): void => sendEngine({ Stop: i }),
  undoLastOverdub: (i: number): void => sendEngine({ Undo: i }),
  reverse: (i: number): void => sendEngine({ Reverse: i }),
  copy: (i: number): number => {
    sendEngine({ Copy: i });
    return firstEmptyLane();
  },
  clear: (i: number): void => sendEngine({ Clear: i }),
  stopAll: (): void => sendEngine('StopAll'),
  playAll: (): void => sendEngine('PlayAll'),
  clearAll: (): void => sendEngine('ClearAll'),
  loopEndStopEnabled: loopEndStop,
  setLoopEndStopEnabled: (on: boolean): void => {
    setLoopEndStopSignal(on);
    sendEngine({ SetLoopEndStop: on });
  },
  retakeEnabled: retake,
  setRetakeEnabled: (on: boolean): void => {
    setRetakeSignal(on);
    sendEngine({ SetRetake: on });
  },
  selectedTrack,
  selectTrack: (i: number): void => sendEngine({ SelectTrack: clampLane(i) }),
  recDubSelected: (): void => sendEngine({ RecDub: selectedTrack() }),
  playStopSelected: (): void => sendEngine({ PlayStop: selectedTrack() }),
  track: (i: number): Accessor<TrackView> => lanes[i][0],
  trackInfo: (i: number): TrackView => lanes[i][0](),
  masterLengthFrames: masterFrames,
  /** MIC: the device input through an empty plugin slot (`toggleEngineInput`). */
  inputArmed: engineInputLive,
  toggleInput: async (): Promise<boolean> => toggleEngineInput(),
  inputArmRequested: (): boolean => false,
  peaksInto,
  phaseValue,
  levelValue: (): number => (plain.clip ? Math.max(1, plain.level) : plain.level),
  stateOf: (i: number): TrackState => plain.state[i] ?? 'EMPTY',
  mutedOf: (i: number): boolean => plain.muted[i] ?? false,
  waitingOf: (i: number): boolean => plain.waiting[i] ?? false,
  recHeadFrac,
  masterFramesValue: (): number => plain.master,
  fxState: (i: number): FxState[] => {
    fxVersions[i][0]();
    return fx[i];
  },
  setFxBypass,
  setFxParam,
  setVolume,
  setMute,
  trackVolume: (i: number): number => volumes[i][0](),
  trackMuted: (i: number): boolean => mutes[i][0](),
  fixedLengthEnabled: fixedLength,
  setFixedLengthEnabled: (on: boolean): void => {
    setFixedLengthSignal(on);
    sendEngine({ SetFixedLength: on });
  },
  fixedLengthBars: fixedBars,
  nextTakeMaxBars,
  setFixedLengthBars: (n: number): void => {
    const bars = Math.max(1, Math.min(MAX_FIXED_BARS, Math.round(n)));
    setFixedBarsSignal(bars);
    sendEngine({ SetFixedBars: bars });
  },
  autoRecordEnabled: autoRecord,
  setAutoRecordEnabled: (on: boolean): void => {
    setAutoRecordSignal(on);
    sendEngine({ SetAutoRecord: on });
  },
  autoRecordSensitivity: autoSensitivity,
  setAutoRecordSensitivity: (n: number): void => {
    const finite = Number.isFinite(n) ? n : autoSensitivity();
    const sensitivity = Math.max(1, Math.min(100, Math.round(finite)));
    setAutoSensitivitySignal(sensitivity);
    sendEngine({ SetAutoSensitivity: sensitivity });
  },
};

// ── Session: export, recovery and import (`src/audio/export/session-source.ts`) ─────────────────────

const FROM_SNAPSHOT = { Playing: 'PLAYING', Stopped: 'STOPPED', Overdubbing: 'OVERDUBBING' } as const;
const TO_LOAD = { PLAYING: 'Playing', STOPPED: 'Stopped' } as const;

/** The committed lanes: the engine's PCM (play order) with this store's mix. */
async function exportSnapshot(): Promise<StemSnapshot> {
  const { header, pcm } = decodeSnapshot(await platform.engine.snapshot());
  return {
    sampleRate: header.rate,
    masterLengthFrames: header.masterLengthFrames,
    tracks: header.tracks.map((t, k) => ({
      index: t.index,
      pcm: pcm[k],
      volume: volumes[t.index][0](),
      muted: mutes[t.index][0](),
      reversed: t.reversed,
      fx: fx[t.index].map((s) => ({ bypassed: s.bypassed, params: { ...s.params } })),
      state: FROM_SNAPSHOT[t.state],
    })),
  };
}

/**
 * Load a session into an all-empty engine: the loops go to the engine (which sets and locks the tempo
 * and starts the PLAYING lanes together), then their mix to the engine and this store. Checks what the
 * web looper's `loadSession` checks before sending anything; the engine checks again.
 */
async function loadSession(payload: LoadSessionPayload): Promise<void> {
  const rate = device()?.sampleRate;
  if (!rate) throw new Error('loadSession: no audio device is open');
  const { bpm, bars, masterLengthFrames: master, tracks } = payload;
  if (!plain.state.every((s) => s === 'EMPTY')) {
    throw new Error('loadSession: import never overwrites a session; clear all tracks first');
  }
  if (!Array.isArray(tracks) || tracks.length < 1) throw new Error('loadSession: payload has no tracks');
  if (!Number.isInteger(bpm) || bpm < 40 || bpm > 300) throw new Error(`loadSession: bpm must be an integer in 40..300, got ${bpm}`);
  if (!Number.isInteger(bars) || bars < 1) throw new Error(`loadSession: bars must be a positive integer, got ${bars}`);
  const expected = bars * framesPerBar(bpm, rate);
  if (master !== expected) throw new Error(`loadSession: BPM, bars and masterLengthFrames disagree (expected ${expected}, got ${master})`);
  const seen = new Set<number>();
  const loaded = tracks.map((t) => {
    if (!Number.isInteger(t.index) || t.index < 0 || t.index >= ENGINE_LANES) throw new Error(`loadSession: track index ${t.index} out of range`);
    if (seen.has(t.index)) throw new Error(`loadSession: duplicate track index ${t.index}`);
    seen.add(t.index);
    if (t.pcm.length !== master) throw new Error(`loadSession: track ${t.index + 1} pcm is ${t.pcm.length} frames, expected ${master}`);
    const state = t.state ?? 'PLAYING';
    if (state !== 'PLAYING' && state !== 'STOPPED') throw new Error(`loadSession: track ${t.index + 1} state must be PLAYING or STOPPED`);
    return { ...t, state, fx: validateFxStates(t.fx, `loadSession: track ${t.index + 1}`) };
  });
  const header: LoadHeader = {
    bpm,
    bars,
    masterLengthFrames: master,
    tracks: loaded.map((t) => ({ index: t.index, frames: master, reversed: t.reversed, state: TO_LOAD[t.state] })),
  };
  await platform.engine.loadSession(encodeSessionBytes(header, loaded.map((t) => t.pcm)));
  for (const t of loaded) {
    volumes[t.index][1](Math.max(0, Math.min(1.5, t.volume)));
    setMutePlain(t.index, t.muted);
    fx[t.index] = t.fx;
    fxVersions[t.index][1]((v) => v + 1);
    sendEngine(...laneCommands(t.index));
  }
}

/** The engine as export, recovery and import read and write it. */
export const engineSession: SessionSource = {
  trackCount: ENGINE_LANES,
  stateOf: (i) => plain.state[i] ?? 'EMPTY',
  trackInfo: (i) => lanes[i][0](),
  peaksInto,
  trackVolume: (i) => volumes[i][0](),
  trackMuted: (i) => mutes[i][0](),
  fxState: (i) => {
    fxVersions[i][0]();
    return fx[i];
  },
  masterFramesValue: () => plain.master,
  exportSnapshot,
  loadSession,
  bpm,
  sampleRate: () => engineSampleRate(),
  masterLevel: () => (masterMuted() ? 0 : masterVolume()),
};

// ── clock and master ──────────────────────────────────────────────────────────────────────────────

const clampBpm = (n: number) => Math.max(40, Math.min(300, Math.round(n)));

/** SetBpm, unless the tempo is locked. The feed's Transport echoes the tempo the engine took. */
function setBpm(n: number): void {
  if (bpmLocked() || !Number.isFinite(n)) return;
  const clamped = clampBpm(n);
  if (clamped !== bpm()) sendEngine({ SetBpm: clamped });
}

// Tap tempo stays in the UI (`docs/plans/native-engine.md` § Stage 2): the web clock's window rules.
const TAP_RESET_MS = 2000;
const TAP_MAX_HISTORY = 8;
let tapTimes: number[] = [];

function tap(now?: number): number {
  const t = now ?? performance.now();
  const last = tapTimes[tapTimes.length - 1];
  if (last !== undefined && t - last > TAP_RESET_MS) {
    tapTimes = [t];
    return bpm();
  }
  tapTimes = [...tapTimes, t].slice(-TAP_MAX_HISTORY);
  if (tapTimes.length >= 2) setBpm(60000 / averageInterval(tapTimes));
  return bpm();
}

/** Engine mode's `clock` (`src/audio/clock.ts` has the web one). */
export const engineClock = {
  bpm,
  setBpm,
  tap,
  /** The engine's pulse runs whenever a device does. */
  running: (): boolean => device() !== null,
  bpmLocked,
  beat,
  countLeft,
  metronomeOn: metronome,
  setMetronome: (on: boolean): void => {
    setMetronomeSignal(on);
    sendEngine({ SetMetronome: on });
  },
  clickVolume,
  setClickVolume: (v: number): void => {
    const clamped = Math.max(0, Math.min(1, v));
    setClickVolumeSignal(clamped);
    writeStoredNumber(CLICK_KEY, clamped);
    sendEngine({ SetClickVolume: clamped });
  },
};

/** Engine mode's `master` (`src/audio/master.ts` has the web one). */
export const engineMaster = {
  volume: masterVolume,
  setVolume: (v: number): void => {
    const clamped = Math.max(0, Math.min(1, v));
    setMasterVolumeSignal(clamped);
    writeStoredNumber(MASTER_KEY, clamped);
    sendEngine({ SetMasterVolume: clamped });
  },
  muted: masterMuted,
  setMuted: (on: boolean): void => {
    setMasterMutedSignal(on);
    sendEngine({ SetMasterMute: on });
  },
  /** Nothing to do at mount: the feed's first (reset) frame settles the level with the engine. */
  init: (): void => {},
};

// ── The device ────────────────────────────────────────────────────────────────────────────────────

/** The device that runs, or null. */
export const engineDevice = device;

/** The running device's rate; 48 kHz until one runs (nothing is on the grid before then). */
export function engineSampleRate(): number {
  return device()?.sampleRate ?? 48000;
}

function deviceLabel(s: DeviceStatus): string {
  return s.inputName === s.outputName ? s.outputName : `${s.inputName} → ${s.outputName}`;
}

let openTail: Promise<unknown> = Promise.resolve();

/**
 * Open (or switch to) the device Audio Settings names: ASIO's cached driver when the ASIO tier is in
 * use, else the saved WASAPI endpoints; the saved channel and buffer either way. Serialized, so rapid
 * picks land in order. Resolves null (and toasts) when the device did not open.
 */
export function openEngineDevice(): Promise<DeviceStatus | null> {
  const run = openTail.then(async () => {
    const s = readAudioDeviceSettings();
    const asio = usingAsio();
    const request: DeviceRequest = {
      backend: asio ? 'Asio' : 'Wasapi',
      input: asio ? null : s.inputDeviceId || null,
      output: asio ? null : s.outputDeviceId || null,
      inputChannel: s.inputChannel === '' ? null : Number(s.inputChannel),
      buffer: s.bufferFrames,
    };
    try {
      const status = await platform.engine.open(request);
      setDevice(status);
      return status;
    } catch (err) {
      console.error('[engine] device open failed', err);
      notifyError("Couldn't open the audio device", err);
      return null;
    }
  });
  openTail = run;
  return run;
}

// Share output: the master mirrored to a Windows render device while the engine runs on ASIO. The pick
// is saved and sent again at each launch; a lost endpoint forgets it.
const [share, setShareSignal] = createSignal(readAudioDeviceSettings().shareDeviceId);

/** Share output's endpoint ('' = off). */
export const engineShare = share;

function forgetShare(): void {
  setShareSignal('');
  writeAudioDeviceSettings({ shareDeviceId: '' });
}

/** Mirror the master to `id` ('' = off), and keep the pick for the next launch. */
export function setEngineShare(id: string): Promise<void> {
  setShareSignal(id);
  writeAudioDeviceSettings({ shareDeviceId: id });
  return platform.engine.setShare(id || null).catch((err: unknown) => {
    forgetShare();
    console.error('[engine] share output failed', err);
    notifyError("Couldn't start Share output", err);
  });
}

/** Send the saved Share pick to the engine (boot, once a device runs). */
export function restoreEngineShare(): void {
  if (share()) void setEngineShare(share());
}

/** Switch the capture channel without reopening the device ('' = auto). */
export function setEngineInputChannel(channel: string): void {
  platform.engine.setInputChannel(channel === '' ? null : Number(channel)).catch((err: unknown) => {
    console.error('[engine] input channel switch failed', err);
    notifyError("Couldn't switch the input channel", err);
  });
}

/** Subscribe to the feed; its first frame is a reset (`adoptSettings`). Returns the unsubscribe. */
export function startEngineStore(): () => void {
  return platform.engine.subscribe(applyFrame);
}
