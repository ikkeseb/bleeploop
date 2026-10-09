import { batch, createSignal, type Accessor } from 'solid-js';
import {
  ENGINE_LANES,
  decodeOpenError,
  decodeSnapshot,
  encodeSessionBytes,
  platform,
  sendEngine,
  type DeviceEvent,
  type DeviceRequest,
  type DeviceStatus,
  type EngineCommand,
  type EngineEvent,
  type EngineToggle,
  type FeedFrame,
  type FxParamId,
  type InputSendId,
  type InputSendParamId,
  type LoadHeader,
  type LaneInfo,
  type LaneMix,
  type LaneState,
  type ScopeUpdate,
  SCOPE_SOURCES,
} from '../../platform';
import { firstTakeSpan, openingSpan, type LoadSessionPayload, type PeakView, type TrackState } from './looper-types';
import { FX_META, FX_PARAM_DEFS, validateFxStates, type FxParamDef, type FxState } from './fx-metadata';
import type { ClearToken, SessionSource } from '../../session/session-source';
import type { StemSnapshot } from '../../session/stem-archive';
import { AUTO_RECORD_DEFAULT_SENSITIVITY } from './auto-record';
import { averageInterval, clampBars, framesPerBar, maxWholeBars } from './quantize';
import { readStoredNumber, writeStoredNumber } from './persist';
import { readAudioDeviceSettings, writeAudioDeviceSettings, type AudioDeviceSettings } from './audio-settings';
import {
  saveSlotInputChannels,
  setAsioEnabled,
  setBufferSize,
  setSampleRatePick,
  slotInputChannels,
  switchAsioDriver,
  usingAsio,
} from './audio-devices';
import { autosave } from '../../session/autosave';
import { engineResync } from './instrument';
import { withAt } from './instrument-slots';
import { engineInputLive, toggleEngineInput } from './native-io';
import { notifyError, notifyInfo } from '../../notify';

/**
 * OWNS: the UI's view of the native engine (invariants 3 and 6): the feed reducer, the device that
 * runs, and the `looper` / `clock` / `master` shapes the UI reads (through `audio.ts` beside this
 * file), built on engine commands and the feed.
 *
 * The engine owns the musical state: lanes, the transport (master, BPM and its lock), the beat and the
 * selection arrive on the feed, and nothing here predicts them. Each lane's mix (volume, MUTE, DUB
 * FEEDBACK, pan, FX) arrives too, as the engine applied it (`Mix`); a mix control shows its gesture's value
 * until the engine has it (the lane mix section below). The toggled settings (CLICK, END STOP, FIXED,
 * RETAKE, AUTO REC, each input send on or off) arrive as the engine applied them (`Toggled`): a button,
 * a key or a pedal sends the engine's toggle (`toggleSetting`), which the engine judges against its own
 * value, and the signal follows the event, never the press. The other settings the engine does not echo,
 * so this store keeps them (FIXED's bars, AUTO REC's sensitivity, FADE's bars, the click and master
 * levels, the sends' values; FADE's bars, the levels and the sends across a restart too): it sends each
 * change. On a `reset` frame it takes the settings the engine remembers, toggled ones included, so the
 * screen shows what the engine plays. `engineSession` is the
 * engine as export, recovery and import see it: the engine's PCM with each lane's mix as the engine
 * applied it (the snapshot's; an import's load carries its saved mix, which the engine applies with the
 * loops and reports as each lane's `Mix`), and the token of
 * the player's clear that emptied the looper (recovery deletes the jam for it, and keeps it for a new
 * engine's empty lanes). `openEngineDevice` turns the engine's refusal of a switch to another rate into
 * the player's confirm.
 *
 * Invariant 6: a frame writes a Solid signal only when its value changed; the waveform rAF reads the
 * plain mirror (`plain`) and extrapolates the playhead from the feed's clock anchor. The live scope
 * taps land in that mirror too and nowhere else (`applyScope`, read through `scopeInto`): a frame of
 * them writes no signal, and a draw loop reading them subscribes to nothing.
 */

/** A lane's public shape, filled from the feed's `LaneInfo`. */
interface TrackView {
  readonly state: TrackState;
  readonly lengthFrames: number;
  readonly armed: boolean;
  readonly autoArmed: boolean;
  readonly canUndo: boolean;
  readonly canReverse: boolean;
  readonly reversed: boolean;
  /** The frame of a pending stop, at the loop end or where a fade ends (the UI tests only for
   * null). */
  readonly stopAt: number | null;
  /** FADE: the lane fades out and stops at `stopAt`. */
  readonly fading: boolean;
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
  fading: false,
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
    fading: info.fading,
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
    a.fading === b.fading &&
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
const [feedResets, setFeedResets] = createSignal(0);
/** The engine's rate: its device's, kept while no device runs (a stopped device leaves the engine). Taken
 * from an open's answer, and from a feed frame's status after the frame's events, which are still the
 * engine's that ran before it (a new engine's come with its reset). */
const [engineRate, setEngineRate] = createSignal(48000);

// ── The settings this store keeps (the engine does not echo them) ──────────────────────────────────

const MAX_FIXED_BARS = 32; // the bar selector bound, the engine's `MAX_FIXED_BARS`
/** The engine's lane buffer: the host builds every engine with `HostConfig { max_loop_seconds: 60.0 }`
 * (`src-tauri/src/engine_io/mod.rs`). It bounds a multiply (`nextTakeMaxBars`). */
const LANE_BUFFER_SECONDS = 60;
const MASTER_KEY = 'lf.masterVolume'; // shared with `master.ts`: one saved level for both paths
const CLICK_KEY = 'lf.clickVolume'; // shared with `clock.ts`

/** Per lane: it waits behind a count-in the engine runs (the reducer's, set as the events arrive:
 * `applyEvent`'s `Beat`, `applyLane`, `endCount`). */
const counted = Array.from({ length: ENGINE_LANES }, () => createSignal(false));
const [loopEndStop, setLoopEndStopSignal] = createSignal(false);
const [fixedLength, setFixedLengthSignal] = createSignal(false);
const [fixedBars, setFixedBarsSignal] = createSignal(4);
/** FADE's lengths in bars (Rust `looper::FADE_BARS`) and the default. */
export const FADE_BARS = [1, 2, 4, 8] as const;
const DEFAULT_FADE_BARS = 2;
/** FADE's bars outlive a restart, as the master and click levels do: restored at boot, sent to an engine
 * that lacks them, adopted (and kept) from one that has them. */
const FADE_BARS_KEY = 'lf.fadeBars';
/** The longest of `FADE_BARS` at most `n`, at least the shortest (the engine's `set_fade_bars`). */
const fadeBarsOf = (n: number): number => [...FADE_BARS].reverse().find((b) => b <= n) ?? FADE_BARS[0];
const [fadeBars, setFadeBarsSignal] = createSignal(
  fadeBarsOf(readStoredNumber(FADE_BARS_KEY, DEFAULT_FADE_BARS, FADE_BARS[0], FADE_BARS[FADE_BARS.length - 1])),
);

function adoptFadeBars(n: number): void {
  const bars = fadeBarsOf(n);
  setFadeBarsSignal(bars);
  writeStoredNumber(FADE_BARS_KEY, bars);
}
const [retake, setRetakeSignal] = createSignal(false);
const [autoRecord, setAutoRecordSignal] = createSignal(false);
const [autoSensitivity, setAutoSensitivitySignal] = createSignal(AUTO_RECORD_DEFAULT_SENSITIVITY);
const [metronome, setMetronomeSignal] = createSignal(false);
const [clickVolume, setClickVolumeSignal] = createSignal(readStoredNumber(CLICK_KEY, 0.7, 0, 1));
const [masterVolume, setMasterVolumeSignal] = createSignal(readStoredNumber(MASTER_KEY, 1, 0, 1));
const [masterMuted, setMasterMutedSignal] = createSignal(false);

// ── The input sends (ECHO, REVERB, RING MOD on the live input): a rig setting, not a session's, so kept in
// localStorage like the master and click levels, restored at boot and sent to an engine that lacks them ─

/** An input send param's range and default (Rust `InputSendParam::range`); the echo's time is an index
 * into the lane delay's divisions. */
export interface InputSendParamDef extends FxParamDef {
  key: InputSendParamId;
  send: InputSendId;
}

const DELAY_TIME = FX_PARAM_DEFS.delay[0];
const INPUT_SEND_PARAMS: readonly InputSendParamDef[] = [
  { ...DELAY_TIME, key: 'echoTime', send: 'echo', label: 'Time' },
  { key: 'echoFeedback', send: 'echo', label: 'Fbk', min: 0, max: 0.95, step: 0.01, default: 0.4 },
  { key: 'echoLevel', send: 'echo', label: 'Level', min: 0, max: 1, step: 0.01, default: 0.5 },
  { key: 'reverbLevel', send: 'reverb', label: 'Level', min: 0, max: 1, step: 0.01, default: 0.5 },
  { key: 'ringFreq', send: 'ring', label: 'Freq', min: 20, max: 1500, step: 1, default: 440, unit: 'Hz' },
  { key: 'ringLevel', send: 'ring', label: 'Level', min: 0, max: 1, step: 0.01, default: 0.5 },
];
const INPUT_SENDS: readonly InputSendId[] = ['echo', 'reverb', 'ring'];
const inputSendKey = (id: string) => `lf.inputSend.${id}`;

const inputSendOn = Object.fromEntries(
  INPUT_SENDS.map((id) => [id, createSignal(readStoredNumber(inputSendKey(id), 0, 0, 1) === 1)]),
) as Record<InputSendId, ReturnType<typeof createSignal<boolean>>>;
const inputSendValues = Object.fromEntries(
  INPUT_SEND_PARAMS.map((d) => [d.key, createSignal(readStoredNumber(inputSendKey(d.key), d.default, d.min, d.max))]),
) as Record<InputSendParamId, ReturnType<typeof createSignal<number>>>;

function adoptInputSend(id: InputSendId, on: boolean): void {
  inputSendOn[id][1](on);
  writeStoredNumber(inputSendKey(id), on ? 1 : 0);
}

/** The value `key` takes: clamped to its range, a division rounded to its index. */
function inputSendValue(key: InputSendParamId, value: number): number | null {
  const def = INPUT_SEND_PARAMS.find((d) => d.key === key);
  if (!def || !Number.isFinite(value)) return null;
  const bounded = Math.max(def.min, Math.min(def.max, value));
  return def.integer ? Math.round(bounded) : bounded;
}

function adoptInputSendParam(key: InputSendParamId, value: number): void {
  const applied = inputSendValue(key, value);
  if (applied === null) return;
  inputSendValues[key][1](applied);
  writeStoredNumber(inputSendKey(key), applied);
}

/** The input sends, for the command bar's IN FX control. */
export const engineInputSends = {
  params: INPUT_SEND_PARAMS,
  on: (id: InputSendId): boolean => inputSendOn[id][0](),
  /** The send's switch, as a press: the engine's toggle (`toggleSetting`). */
  toggle: (id: InputSendId): void => toggleSetting({ Send: id }),
  /** On or off outright: initialization and scripts, not a control. */
  setOn: (id: InputSendId, on: boolean): void => {
    adoptInputSend(id, on);
    sendEngine({ SetInputSend: [id, on] });
  },
  value: (key: InputSendParamId): number => inputSendValues[key][0](),
  setValue: (key: InputSendParamId, value: number): void => {
    adoptInputSendParam(key, value);
    sendEngine({ SetInputSendParam: [key, inputSendValues[key][0]()] });
  },
  /** Some send is on: the IN FX control reads engaged. */
  anyOn: (): boolean => INPUT_SENDS.some((id) => inputSendOn[id][0]()),
};

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

/** Columns the scope mirror holds: about four seconds of 4 ms columns, a ring per source. */
const SCOPE_COLUMNS = 1024;

/**
 * A non-reactive view of the scope's columns, filled into a caller-owned object by `scopeInto` so the
 * rAF draw loop reads them with no allocation and no Solid subscription (invariant 6). `lo`/`hi` are
 * refs into the store's own arrays, one per source in `SCOPE_SOURCES` order (the five lanes after
 * their FX, the monitor, the master output); each is a ring of `SCOPE_COLUMNS` columns whose newest column
 * sits at `at - 1` and whose valid columns are the `count` before it. The newest column covers `bin`
 * frames from `frame`, and each older one `bin` frames earlier. `epoch` bumps whenever the trace
 * broke (a `gap`, a reset): a draw that holds state across frames starts over.
 * @public (the look that draws it is not built yet; `verify/guards/stage-draw.mjs` allows `scopeInto`)
 */
export interface ScopeView {
  lo: readonly Float32Array[] | null;
  hi: readonly Float32Array[] | null;
  at: number;
  count: number;
  frame: number;
  bin: number;
  epoch: number;
}

const plain = {
  state: Array.from({ length: ENGINE_LANES }, (): TrackState => 'EMPTY'),
  waiting: Array.from({ length: ENGINE_LANES }, () => false),
  muted: Array.from({ length: ENGINE_LANES }, () => false),
  retakePass: Array.from({ length: ENGINE_LANES }, () => 0),
  /** A count-in runs: from its first beat as it ARRIVES (one output latency before its numeral is
   * shown) until the lane behind it leaves its waiting state (`endCount`). */
  counting: false,
  /** The master boundary a later take started on (its record head counts from here). */
  takeStart: Array.from({ length: ENGINE_LANES }, () => 0),
  /** The frames the lane's record head sweeps: the loop, or a multiply window past it; 0 for a free
   * take, which sweeps the loops it has reached (`laterTakeFrames`). */
  takeFrames: Array.from({ length: ENGINE_LANES }, () => 0),
  /** A first take's opening span (`openingSpan`), taken as it starts (`recSpanFrames`). */
  opening: Array.from({ length: ENGINE_LANES }, () => 1),
  /** The span the lane's take has reached (`recSpanFrames`); 0 as a take starts. */
  span: Array.from({ length: ENGINE_LANES }, () => 0),
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
  /** The live scope taps' columns (`ScopeView`), per source a ring of lows and one of highs: written
   * only by `applyScope`, read only by `scopeInto`. Preallocated, so a frame of columns allocates
   * nothing. */
  scope: {
    lo: Array.from({ length: SCOPE_SOURCES }, () => new Float32Array(SCOPE_COLUMNS)),
    hi: Array.from({ length: SCOPE_SOURCES }, () => new Float32Array(SCOPE_COLUMNS)),
    /** Where the next column lands. */
    at: 0,
    count: 0,
    /** The device frame the newest column covers, and the frames one column covers. */
    frame: 0,
    bin: 0,
    epoch: 0,
  },
  /** Per lane, bumped when its committed loop may have changed: a waveform update while it neither
   * records nor overdubs, and the end of a take or layer. Still while a layer sums, whose snapshot is
   * the loop before it (`engineSession.revision`). */
  revision: Array.from({ length: ENGINE_LANES }, () => 0),
  /** Reset frames applied: the engine this view follows (a new one: another rate, a fault; or a new
   * subscriber, a WebView reload). */
  resets: 0,
  /** Per lane: a `Cleared` took its loop and its own `Lane` event has not arrived yet (a feed tick may
   * split them). */
  clearing: Array.from({ length: ENGINE_LANES }, () => false),
  /** The player's clear that emptied the looper, in engine `gen` at its `rate` (`engineSession.clearToken`). */
  clear: null as { gen: number; rate: number } | null,
};

const capturing = (s: TrackState) => s === 'RECORDING' || s === 'OVERDUBBING';
/** The lane holds a committed loop (overdubbing included; a take in flight has none yet). */
const holdsLoop = (s: TrackState) => s === 'PLAYING' || s === 'STOPPED' || s === 'OVERDUBBING';
/** A `Cleared` in the frame being applied took a loop (`noteClear`). */
let tookLoop = false;

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

/** When device frame `frame` is heard, on `Date.now()`'s clock; now while no device runs. */
function heardAtMs(frame: number): number {
  const c = plain.clock;
  return c.rate > 0 ? c.atMs + ((frame - c.frame + plain.heardLag) * 1000) / c.rate : Date.now();
}

/** The longest a beat waits to be shown: a stale anchor must not park it. */
const MAX_BEAT_WAIT_MS = 1000;
const beatTimers = new Set<ReturnType<typeof setTimeout>>();
/** Bumped when a lane leaves its waiting state (`endCount`): a beat of the count it waited behind, still
 * to be shown, writes no numeral (the next count's may already be up). */
let countGen = 0;

/** Lane `lane` left its waiting state (its take began, or the arm was cancelled): the count it waited
 * behind is over, so the numeral reads 0 until the next count's first beat is heard, whatever beat is
 * still to be shown. */
function endCount(lane: number): void {
  countGen++;
  plain.counting = false;
  counted[lane][1](false);
  setCountLeft(0);
}

/** Run `show` when device frame `frame` is heard (the beat LED and the count-in numeral, as the playhead
 * is drawn): a beat arrives on the feed as the engine renders it, one output latency early. A few writes
 * a second, each one change (invariant 6). */
function whenHeard(frame: number, show: () => void): void {
  const wait = Math.min(MAX_BEAT_WAIT_MS, heardAtMs(frame) - Date.now());
  if (wait <= 0) {
    show();
    return;
  }
  const timer = setTimeout(() => {
    beatTimers.delete(timer);
    show();
  }, wait);
  beatTimers.add(timer);
}

/** The frames lane `i`'s take is drawn across while it records, the span its record head sweeps: the
 * master (a multiply's window, which grows the loop to it; a free take's loops so far, which may grow
 * it), or before a master a first take's doubling span (`firstTakeSpan`). The waveform places peak bins
 * by frame over it, so bins and head share one scale. */
function recSpanFrames(i: number): number {
  const m = plain.master;
  const elapsed = heardFrame() - plain.takeStart[i];
  // A free take (E10) runs until the press: the lane spans the loops it has reached, never promising a
  // close at the loop's end.
  const span =
    m <= 0
      ? firstTakeSpan(plain.opening[i], elapsed)
      : plain.takeFrames[i] > 0
        ? Math.max(m, plain.takeFrames[i])
        : Math.max(1, Math.ceil(elapsed / m)) * m;
  // A new anchor may step the extrapolated head back: a span once reached holds, so the bins are
  // re-placed once as it grows, never back and forth.
  if (span > plain.span[i]) plain.span[i] = span;
  return plain.span[i];
}

/** A take's record head, 0..1 of its span (`recSpanFrames`); a later take waiting for its downbeat rides
 * the loop phase, a first take waiting (count-in, AUTO listening) has none: -1. */
function recHeadFrac(i: number): number {
  if (plain.waiting[i]) return plain.master > 0 ? phaseValue() : -1;
  const f = (heardFrame() - plain.takeStart[i]) / recSpanFrames(i);
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

function scopeInto(out: ScopeView): ScopeView {
  const s = plain.scope;
  out.lo = s.lo;
  out.hi = s.hi;
  out.at = s.at;
  out.count = s.count;
  out.frame = s.frame;
  out.bin = s.bin;
  out.epoch = s.epoch;
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
    plain.takeFrames[lane] = laterTakeFrames();
    plain.opening[lane] = openingSpan(bpm(), engineSampleRate());
    plain.span[lane] = 0;
  }
  if (capturing(prev) && !capturing(next)) plain.revision[lane]++;
  // The press's count beat precedes the lane's own event on the feed, so a lane that starts waiting
  // while a count runs is behind it; one that was waiting already is marked by the beat (`applyEvent`).
  if (plain.waiting[lane] && !waiting) endCount(lane);
  else if (!plain.waiting[lane] && waiting && plain.counting) counted[lane][1](true);
  if (next === 'EMPTY') cancelOverlays(lane);
  plain.clearing[lane] = false;
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
    case 'Beat': {
      const { beatInBar, countLeft: left } = ev;
      // A count runs from the frame its beat arrives, and every waiting lane is behind it; only the
      // numeral waits to be heard.
      if (left > 0) {
        plain.counting = true;
        for (let i = 0; i < ENGINE_LANES; i++) if (plain.waiting[i]) counted[i][1](true);
      }
      // A count's end with no lane waiting behind it (a reset frame replays the lanes first, the drained
      // beats after them) is over here: no lane is left to end it (`endCount`).
      else if (plain.counting && !plain.waiting.some(Boolean)) plain.counting = false;
      const gen = countGen;
      whenHeard(ev.frame, () => {
        setBeat(beatInBar % 4);
        if (gen === countGen) setCountLeft(left);
      });
      break;
    }
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
    // COPY, CLEAR and a pedal's MUTE reach the lane's mix as its next `Mix`; a pending gesture on the lane
    // they replace is over.
    case 'Copied':
      cancelOverlays(ev.to);
      break;
    case 'Cleared':
      // Before the lane's own Lane event: its state is still the one the clear ended.
      if (holdsLoop(plain.state[ev.lane])) {
        plain.clearing[ev.lane] = true;
        tookLoop = true;
      }
      cancelOverlays(ev.lane);
      break;
    case 'Mix':
      applyMix(ev.lane, ev.mix);
      break;
    case 'Toggled':
      applyToggle(ev.toggle, ev.on);
      break;
  }
}

/** A toggled setting as the engine applied it (`Toggled`): its signal, and an input send's saved value. */
function applyToggle(toggle: EngineToggle, on: boolean): void {
  if (typeof toggle === 'object') adoptInputSend(toggle.Send, on);
  else if (toggle === 'Click') setMetronomeSignal(on);
  else if (toggle === 'EndStop') setLoopEndStopSignal(on);
  else if (toggle === 'Fixed') setFixedLengthSignal(on);
  else if (toggle === 'Retake') setRetakeSignal(on);
  else setAutoRecordSignal(on);
}

/** Switch a toggled setting: the engine's toggle, which flips the value it has when the press lands (or
 * names a refusal on the selected lane, which `boot.ts` shows), so a pedal and a click never undo each
 * other. The setting's signal follows the engine's `Toggled`. */
export function toggleSetting(toggle: EngineToggle): void {
  void sendEngine({ Action: { Toggle: toggle } });
}

function applyDeviceEvent(ev: DeviceEvent): void {
  switch (ev.type) {
    case 'Lost':
      console.error(`[engine] ${ev.backend} device lost: ${ev.reason}`);
      notifyError('Audio device lost', ev.reason);
      // Kept as why no device runs; every reader gates it on the device status, which a recovery or a
      // fallback in the same or a later frame may already have filled.
      setOpenFailure(ev.reason);
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
      notifyError(
        'The audio engine restarted',
        'The loops left with it. Restart BleepLoop to get back the last saved ones; a new recording replaces them.',
      );
      break;
    case 'LoopsDropped':
      console.error(`[engine] the loops recorded at ${ev.from} Hz left the engine: it was rebuilt at ${ev.to} Hz`);
      notifyInfo(
        'Loops kept in recovery',
        `The audio engine was rebuilt at ${kHz(ev.to)} for the backup device, and your loops were recorded at ` +
          `${kHz(ev.from)}. Reconnect ${ev.device} and restart BleepLoop to get them back.`,
      );
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
  if (!capturing(plain.state[lane])) plain.revision[lane]++;
}

/**
 * One frame's scope columns into the mirror, oldest first. A batch the engine says it cannot splice
 * onto the trace (`gap`: a full ring, a device-frame skip, a held feed, a new engine) drops what the
 * mirror holds and bumps the epoch, so no draw joins two moments that never followed each other.
 */
function applyScope(u: ScopeUpdate): void {
  const s = plain.scope;
  if (u.gap) {
    s.count = 0;
    s.epoch++;
  }
  const columns = u.min[0]?.length ?? 0;
  for (let k = 0; k < columns; k++) {
    for (let src = 0; src < SCOPE_SOURCES; src++) {
      s.lo[src][s.at] = u.min[src][k];
      s.hi[src][s.at] = u.max[src][k];
    }
    s.at = (s.at + 1) % SCOPE_COLUMNS;
    if (s.count < SCOPE_COLUMNS) s.count++;
  }
  if (columns === 0) return;
  s.frame = u.frame + (columns - 1) * u.bin;
  s.bin = u.bin;
}

/** The trace the mirror holds is void: the engine this view follows changed, or it was replaced. */
function dropScope(): void {
  plain.scope.count = 0;
  plain.scope.epoch++;
}

/**
 * After a frame's events: a clear that took a loop and left no lane holding one hands the recovery its
 * token, bound to this engine; any loop still or again held cancels it (a partial clear, a commit). A
 * lane whose `Cleared` came without its own event yet counts as empty.
 */
function noteClear(): void {
  const loops = plain.state.some((s, i) => holdsLoop(s) && !plain.clearing[i]);
  if (loops) plain.clear = null;
  else if (tookLoop) plain.clear = { gen: plain.resets, rate: engineRate() };
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
    for (let i = 0; i < ENGINE_LANES; i++) plain.revision[i]++;
    dropScope();
    plain.resets++;
    setFeedResets(plain.resets);
    plain.clearing.fill(false);
    for (const timer of beatTimers) clearTimeout(timer);
    beatTimers.clear();
    plain.counting = false;
    for (const [, setCounted] of counted) setCounted(false);
    setCountLeft(0);
    for (let i = 0; i < ENGINE_LANES; i++) cancelOverlays(i);
  }
  // The device and its clock first: the anchor is read after the frame's events, so a take the events
  // start snaps to the grid it carries.
  if (f.status !== undefined) {
    const before = device();
    setDevice(f.status);
    // The owner reopened on its own (a lost device back, a fallback): check the picks against it. A
    // status of the same device may be older than a channel switch, so it is not read for this.
    const moved = !before || before.backend !== f.status?.backend || before.inputName !== f.status.inputName;
    if (f.status && moved) dropLackingPicks(slotInputChannels().map(channelOf), f.status.inputChannels);
  }
  if (f.anchor) plain.clock = f.anchor;
  if (f.reset) adoptSettings(f.settings ?? []);
  tookLoop = false;
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
  noteClear();
  if (f.status) setEngineRate(f.status.sampleRate);
  for (const d of f.device) applyDeviceEvent(d);
  // No meter: no device runs, so the input reads silent.
  plain.level = f.meter?.peak ?? 0;
  plain.clip = f.meter?.clip ?? false;
  for (const p of f.peaks) applyPeaks(p.lane, p.start, p.count, p.min, p.max);
  if (f.scope) applyScope(f.scope);
}

// ── The mix and the modes this store keeps ────────────────────────────────────────────────────────

/** A lane's mix in the wire's shape (`LaneMix`): each effect's params exactly its defs' keys, the pan
 * only off the centre (as the engine writes it). */
function wireMix(volume: number, muted: boolean, dubFeedback: number, pan: number, states: readonly FxState[]): LaneMix {
  const wireFx = FX_META.map((meta, k) => ({
    bypassed: states[k].bypassed,
    params: Object.fromEntries(FX_PARAM_DEFS[meta.kind].map((def) => [def.key, states[k].params[def.key]])),
  }));
  return { volume, muted, dubFeedback, ...(pan === 0 ? {} : { pan }), fx: wireFx };
}

// ── The lane mix: the engine's, and what a mix control shows until the engine has it ──────────────────
//
// A lane's mix (volume, MUTE, DUB FEEDBACK, pan, the five effects) is the one the engine applied, written only
// from the feed: its `Mix` (sent whenever the applied mix changes: a command, COPY, CLEAR, a pedal's MUTE,
// a load) and a reset frame. Every reader reads it but a mix control, which shows its OVERLAY while one is
// set: a gesture (a drag, a held key, a select's change, a bypass press) writes the overlay from the value
// the control shows and sends the command. The overlay holds through the gesture whatever Mix arrives;
// after it, it holds until a Mix whose value equals it (both sides normalised, `sameMixValue`), the failed
// submission of its own latest command, or a cancel: the lane's `Cleared`, a `Copied` into it, an import's
// or recovery's load, a reset frame, the lane going EMPTY, the control's disposal. Only the UI writes these
// values (COPY, CLEAR, a load and a reset are the cancels), so an equal Mix is a safe acknowledgement and
// no timeout is needed. MUTE has no overlay: it sends the engine's toggle, as a pedal does, and shows the
// engine's mute.

/** A lane's mix as this store holds it: the wire's `LaneMix`, its effects as the FX drawer reads them. */
interface MixView {
  readonly volume: number;
  readonly muted: boolean;
  readonly dubFeedback: number;
  /** -1 (hard left) to 1 (hard right); a wire mix without one is centred (0). */
  readonly pan: number;
  readonly fx: readonly FxState[];
}

const defaultMix = (): MixView => ({ volume: 1, muted: false, dubFeedback: 1, pan: 0, fx: defaultFx() });

function sameMix(a: MixView, b: MixView): boolean {
  return (
    a.volume === b.volume &&
    a.muted === b.muted &&
    a.dubFeedback === b.dubFeedback &&
    a.pan === b.pan &&
    a.fx.every((s, k) => {
      const t = b.fx[k];
      const keys = Object.keys(s.params);
      return s.bypassed === t.bypassed && keys.length === Object.keys(t.params).length && keys.every((key) => s.params[key] === t.params[key]);
    })
  );
}

/** Each lane's mix as the engine applied it (the feed's). */
const mixes = Array.from({ length: ENGINE_LANES }, () => createSignal<MixView>(defaultMix(), { equals: sameMix }));

/** Take the engine's mix of `lane`: its own copy (an FX state is never shared with another lane's). */
function adoptMix(lane: number, mix: LaneMix | MixView): void {
  mixes[lane][1]({
    volume: mix.volume,
    muted: mix.muted,
    dubFeedback: mix.dubFeedback,
    pan: mix.pan ?? 0,
    fx: mix.fx.map((s) => ({ bypassed: s.bypassed, params: { ...s.params } })),
  });
  plain.muted[lane] = mix.muted;
}

/** A mix control's key among its lane's overlays: the volume, DUB FEEDBACK, the pan, effect `k`'s bypass
 * (`fx<k>`) or one of its params (`fx<k>.<param>`). */
export type MixKey = 'volume' | 'dubFeedback' | 'pan' | `fx${number}` | `fx${number}.${string}`;

/** A control's overlay: the value its last gesture asked for, and that write's revision. */
interface Overlay {
  readonly value: number | boolean;
  readonly rev: number;
}

type Overlays = Readonly<Partial<Record<MixKey, Overlay>>>;

const overlays = Array.from({ length: ENGINE_LANES }, () => createSignal<Overlays>({}));
/** Per lane, the controls whose gesture runs (a pointer down, a key held): no Mix ends their overlay. */
const holding = Array.from({ length: ENGINE_LANES }, () => new Set<MixKey>());
let overlayRev = 0;

/** The effect index of an `fx…` key and the param it names (none for its bypass). */
function fxKeyOf(key: MixKey): { k: number; param: string | null } | null {
  const m = /^fx(\d+)(?:\.(.+))?$/.exec(key);
  return m ? { k: Number(m[1]), param: m[2] ?? null } : null;
}

/** `key`'s value in `mix`. */
function mixValue(mix: MixView, key: MixKey): number | boolean {
  if (key === 'volume') return mix.volume;
  if (key === 'dubFeedback') return mix.dubFeedback;
  if (key === 'pan') return mix.pan;
  const at = fxKeyOf(key);
  const state = at ? mix.fx[at.k] : undefined;
  if (!at || !state) return NaN;
  return at.param === null ? state.bypassed : (state.params[at.param] ?? NaN);
}

/** The value a setter sends for `key`: the engine's clamp (a value that is no number: the volume and DUB
 * FEEDBACK at unity, the pan centred, an effect param refused, null), an integer param rounded. */
function clampMix(key: MixKey, v: number): number | null {
  if (key === 'volume') return Math.max(0, Math.min(1.5, Number.isFinite(v) ? v : 1));
  if (key === 'dubFeedback') return Math.max(0, Math.min(1, Number.isFinite(v) ? v : 1));
  if (key === 'pan') return Number.isFinite(v) ? Math.max(-1, Math.min(1, v)) + 0 : 0; // -0 as 0

  const at = fxKeyOf(key);
  const meta = at && FX_META[at.k];
  const def = meta && at.param !== null ? FX_PARAM_DEFS[meta.kind].find((d) => d.key === at.param) : undefined;
  if (!def || !Number.isFinite(v)) return null;
  const bounded = Math.max(def.min, Math.min(def.max, v));
  return def.integer ? Math.round(bounded) : bounded;
}

/** An overlay's value and a Mix's are one value: each through the setter's clamp, then as the engine's
 * f32 holds it (a Mix's numbers crossed as f32). */
function sameMixValue(key: MixKey, a: number | boolean, b: number | boolean): boolean {
  if (typeof a === 'boolean' || typeof b === 'boolean') return a === b;
  const norm = (v: number) => {
    const c = clampMix(key, v);
    return c === null ? NaN : Math.fround(c);
  };
  return norm(a) === norm(b);
}

/** What lane `i`'s control `key` shows: its overlay, else the engine's value. */
function shownMix(i: number, key: MixKey): number | boolean {
  const o = overlays[i][0]()[key];
  return o ? o.value : mixValue(mixes[i][0](), key);
}

/** Drop `key`'s overlay on lane `i`: only the one revision `rev` wrote, when given. */
function dropOverlay(i: number, key: MixKey, rev?: number): void {
  const cur = overlays[i][0]();
  const o = cur[key];
  if (!o || (rev !== undefined && o.rev !== rev)) return;
  const next = { ...cur };
  delete next[key];
  overlays[i][1](next);
}

/** A cancel: lane `lane`'s overlays go, and its controls show the engine's mix. */
function cancelOverlays(lane: number): void {
  if (Object.keys(overlays[lane][0]()).length > 0) overlays[lane][1]({});
}

/** A gesture on lane `i`'s control `key`: show `value` and send `command`. Its own failed submission
 * drops the overlay, unless a newer write replaced it. */
function writeMix(i: number, key: MixKey, value: number | boolean, command: EngineCommand): void {
  const rev = ++overlayRev;
  overlays[i][1]((cur) => ({ ...cur, [key]: { value, rev } }));
  // `took` false: the batch was not wholly taken (the native host pushes it command by command and stops
  // at the first refused, keeping the accepted prefix), so this command may still have reached the
  // engine. The overlay goes anyway: if the engine did take it, its Mix brings the value back.
  void sendEngine(command).then((took) => {
    if (!took) dropOverlay(i, key, rev);
  });
}

/** The engine's `Mix` for `lane`: it becomes the lane's mix, and ends each overlay it equals whose
 * gesture is over. */
function applyMix(lane: number, mix: LaneMix): void {
  adoptMix(lane, mix);
  const applied = mixes[lane][0]();
  const cur = overlays[lane][0]();
  const met = (Object.keys(cur) as MixKey[]).filter((key) => {
    const o = cur[key];
    return o !== undefined && !holding[lane].has(key) && sameMixValue(key, o.value, mixValue(applied, key));
  });
  if (met.length === 0) return;
  const next = { ...cur };
  for (const key of met) delete next[key];
  overlays[lane][1](next);
}

/** The mix controls' side of a gesture on lane `i`'s control `key`: it starts (`on`, a pointer down or a
 * key held) or ends. At its end an overlay the engine's mix already equals goes at once: its equal Mix
 * arrived while the gesture held it. */
function holdMix(i: number, key: MixKey, on: boolean): void {
  if (on) {
    holding[i].add(key);
    return;
  }
  holding[i].delete(key);
  const o = overlays[i][0]()[key];
  if (o && sameMixValue(key, o.value, mixValue(mixes[i][0](), key))) dropOverlay(i, key, o.rev);
}

/** Lane `i`'s control `key` was disposed: its gesture and overlay end with it. */
function dropMix(i: number, key: MixKey): void {
  holding[i].delete(key);
  dropOverlay(i, key);
}

/**
 * A reset frame: take over the settings the engine remembers (one missing from `settings` is at the
 * engine's default, which is the UI's) instead of pushing the UI's, so a WebView reload keeps a
 * playing session's mix and modes. A lane's mix from here stands only where the frame has no `Mix` for
 * the lane: its Mix events, applied after this, are the mix the engine last applied. The UI still sends what it persists and the engine lacks (the master
 * and click volumes, FADE's bars and the input sends on a first launch) and what it owns: the note
 * target, the slot gains and the live slot (`engineResync`).
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
  // A lane's mix from the settings, for a lane the reset frame has no `Mix` for (its Mix events follow).
  const laneMixes = Array.from({ length: ENGINE_LANES }, () => ({ volume: 1, muted: false, dubFeedback: 1, pan: 0, fx: defaultFx() }));
  let masterKnown = false;
  let clickKnown = false;
  let fadeKnown = false;
  const sendsKnown = new Set<string>();
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
    } else if ('SetInputSend' in c) {
      sendsKnown.add(c.SetInputSend[0]);
      adoptInputSend(c.SetInputSend[0], c.SetInputSend[1]);
    } else if ('SetInputSendParam' in c) {
      sendsKnown.add(c.SetInputSendParam[0]);
      adoptInputSendParam(c.SetInputSendParam[0], c.SetInputSendParam[1]);
    } else if ('SetMasterMute' in c) setMasterMutedSignal(c.SetMasterMute);
    else if ('SetMetronome' in c) setMetronomeSignal(c.SetMetronome);
    else if ('SetLoopEndStop' in c) setLoopEndStopSignal(c.SetLoopEndStop);
    else if ('SetFixedLength' in c) setFixedLengthSignal(c.SetFixedLength);
    else if ('SetFixedBars' in c) setFixedBarsSignal(c.SetFixedBars);
    else if ('SetFadeBars' in c) {
      fadeKnown = true;
      adoptFadeBars(c.SetFadeBars);
    }
    else if ('SetRetake' in c) setRetakeSignal(c.SetRetake);
    else if ('SetAutoRecord' in c) setAutoRecordSignal(c.SetAutoRecord);
    else if ('SetAutoSensitivity' in c) setAutoSensitivitySignal(c.SetAutoSensitivity);
    else if ('SetVolume' in c) laneMixes[c.SetVolume[0]].volume = c.SetVolume[1];
    else if ('SetMute' in c) laneMixes[c.SetMute[0]].muted = c.SetMute[1];
    else if ('SetDubFeedback' in c) laneMixes[c.SetDubFeedback[0]].dubFeedback = c.SetDubFeedback[1];
    else if ('SetPan' in c) laneMixes[c.SetPan[0]].pan = c.SetPan[1];
    else if ('SetFxBypass' in c) {
      const [l, kind, bypassed] = c.SetFxBypass;
      const k = FX_META.findIndex((m) => m.kind === kind);
      laneMixes[l].fx[k].bypassed = bypassed;
    } else if ('SetFxParam' in c) {
      const [l, key, value] = c.SetFxParam;
      const k = FX_META.findIndex((m) => FX_PARAM_DEFS[m.kind].some((d) => d.key === key));
      laneMixes[l].fx[k].params[key] = value;
    }
    // The tempo and the selection arrive as events; the note target and the slots are the UI's.
  }
  laneMixes.forEach((m, i) => adoptMix(i, m));
  const lacking: EngineCommand[] = [];
  if (!masterKnown) lacking.push({ SetMasterVolume: masterVolume() });
  if (!clickKnown) lacking.push({ SetClickVolume: clickVolume() });
  if (!fadeKnown) lacking.push({ SetFadeBars: fadeBars() });
  // The input sends the engine lacks: values first, so a send that comes on comes on with them.
  for (const d of INPUT_SEND_PARAMS) {
    if (!sendsKnown.has(d.key)) lacking.push({ SetInputSendParam: [d.key, inputSendValues[d.key][0]()] });
  }
  for (const id of INPUT_SENDS) {
    if (!sendsKnown.has(id)) lacking.push({ SetInputSend: [id, inputSendOn[id][0]()] });
  }
  sendEngine(...lacking);
  engineResync();
}

const clampLane = (i: number) => Math.max(0, Math.min(ENGINE_LANES - 1, Math.trunc(i)));

/** The volume fader of lane `i` (0..1.5). */
function setVolume(i: number, v: number): void {
  const value = clampMix('volume', v) ?? 1;
  writeMix(i, 'volume', value, { SetVolume: [i, value] });
}

/** DUB FEEDBACK of lane `i`, clamped to 0..1 as the engine clamps it. */
function setDubFeedback(i: number, v: number): void {
  const value = clampMix('dubFeedback', v) ?? 1;
  writeMix(i, 'dubFeedback', value, { SetDubFeedback: [i, value] });
}

/** The pan of lane `i`, -1 (hard left) to 1 (hard right), clamped as the engine clamps it (a value that
 * is no number centres it). */
function setPan(i: number, v: number): void {
  const value = clampMix('pan', v) ?? 0;
  writeMix(i, 'pan', value, { SetPan: [i, value] });
}

function setFxBypass(i: number, fxIndex: number, bypassed: boolean): void {
  const meta = FX_META[fxIndex];
  if (!meta) return;
  writeMix(i, `fx${fxIndex}`, bypassed, { SetFxBypass: [i, meta.kind, bypassed] });
}

function setFxParam(i: number, fxIndex: number, key: string, value: number): void {
  const mixKey: MixKey = `fx${fxIndex}.${key}`;
  const applied = clampMix(mixKey, value);
  if (applied === null) return;
  writeMix(i, mixKey, applied, { SetFxParam: [i, key as FxParamId, applied] });
}

/**
 * The bars a later take records over a loop of `loopBars` whole bars when `requested` are asked for: the
 * engine's `later_take_bars` (`src-tauri/crates/lf-engine/src/grid.rs`). Up to the loop, the request (a
 * shorter take repeats across the loop); past it, a multiply: whole loops, floored, at most the largest
 * multiple within `maxBars`. No loop yet (`loopBars` 0): the request within [1, maxBars].
 */
export function laterTakeBars(requested: number, loopBars: number, maxBars: number): number {
  if (loopBars < 1) return clampBars(requested, maxBars);
  if (requested <= loopBars) return clampBars(requested, loopBars);
  return Math.min(Math.floor(requested / loopBars), Math.max(1, Math.floor(maxBars / loopBars))) * loopBars;
}

/** The longest FIXED take, as the engine's `next_take_max_bars` rules it: 32 before a loop; over one of
 * whole bars, the longest multiply of it the lane buffer holds; over one of no whole number of bars (a
 * foreign import), its whole bars. */
function nextTakeMaxBars(): number {
  const master = masterFrames();
  if (master <= 0) return MAX_FIXED_BARS;
  const rate = engineSampleRate();
  const fpb = framesPerBar(bpm(), rate);
  if (master % fpb !== 0) return Math.min(MAX_FIXED_BARS, maxWholeBars(master, fpb));
  const bufferBars = maxWholeBars(Math.ceil(LANE_BUFFER_SECONDS * rate), fpb);
  return laterTakeBars(MAX_FIXED_BARS, master / fpb, Math.min(MAX_FIXED_BARS, bufferBars));
}

/** The window a later take records, as the engine's `configure_end` bounds it: FIXED's bars as a later
 * take records them (past the loop, a multiply), the loop under RETAKE (it rolls at the loop's length)
 * or over a loop of no whole number of bars; 0 for a free take (E10), which runs until the press and
 * whose length the stop picks. */
function laterTakeFrames(): number {
  const master = plain.master;
  if (master <= 0 || retake()) return master;
  const fpb = framesPerBar(bpm(), engineSampleRate());
  if (!fixedLength()) return master % fpb === 0 ? 0 : master;
  const max = nextTakeMaxBars();
  const bars = master % fpb === 0 ? laterTakeBars(fixedBars(), master / fpb, max) : clampBars(fixedBars(), max);
  return bars * fpb;
}

/** TRIM (F16): lane `i` keeps its first `bars` bars as heard, repeated across the loop; one UNDO gives
 * the loop back. The engine judges it and names a refusal on the feed. */
export function trimLane(i: number, bars: number): void {
  sendEngine({ Trim: [clampLane(i), Math.max(1, Math.round(bars))] });
}

/** FADE, the command bar's. The engine judges a press and names a refusal on the feed. */
export const engineFade = {
  /** FADE's length in bars (one of `FADE_BARS`). */
  bars: fadeBars,
  setBars: (n: number): void => {
    adoptFadeBars(n);
    sendEngine({ SetFadeBars: fadeBars() });
  },
  /** Every playing lane fades out over the bars and stops on the bar line; a second press stops them now. */
  fadeAll: (): void => void sendEngine({ Action: 'FadeAll' }),
  /** Some lane is fading. */
  fading: (): boolean => lanes.some(([track]) => track().fading),
};

/** DUB FEEDBACK per lane, the FX drawer's control: what it shows (`shownMix`) and its gesture. */
export const engineDubFeedback = {
  value: (i: number): number => shownMix(i, 'dubFeedback') as number,
  set: setDubFeedback,
};

/** The first EMPTY lane (where COPY lands), or -1. */
function firstEmptyLane(): number {
  return plain.state.findIndex((s) => s === 'EMPTY');
}

/** The `looper` the UI reads (`audio.ts`). */
export const engineLooper = {
  trackCount: ENGINE_LANES as typeof ENGINE_LANES,
  recDub: async (i: number): Promise<void> => void sendEngine({ RecDub: i }),
  playStop: (i: number): void => void sendEngine({ PlayStop: i }),
  stop: (i: number): void => void sendEngine({ Stop: i }),
  undoLastOverdub: (i: number): void => void sendEngine({ Undo: i }),
  reverse: (i: number): void => void sendEngine({ Reverse: i }),
  copy: (i: number): number => {
    sendEngine({ Copy: i });
    return firstEmptyLane();
  },
  clear: (i: number): void => void sendEngine({ Clear: i }),
  stopAll: (): void => void sendEngine('StopAll'),
  playAll: (): void => void sendEngine('PlayAll'),
  clearAll: (): void => void sendEngine('ClearAll'),
  loopEndStopEnabled: loopEndStop,
  /** END STOP's press: the engine's toggle (`toggleSetting`). The setters below are for scripts. */
  toggleLoopEndStop: (): void => toggleSetting('EndStop'),
  setLoopEndStopEnabled: (on: boolean): void => {
    setLoopEndStopSignal(on);
    sendEngine({ SetLoopEndStop: on });
  },
  retakeEnabled: retake,
  /** RETAKE's press: the engine's toggle, refused while a take records. */
  toggleRetake: (): void => toggleSetting('Retake'),
  setRetakeEnabled: (on: boolean): void => {
    setRetakeSignal(on);
    sendEngine({ SetRetake: on });
  },
  selectedTrack,
  selectTrack: (i: number): void => void sendEngine({ SelectTrack: clampLane(i) }),
  track: (i: number): Accessor<TrackView> => lanes[i][0],
  trackInfo: (i: number): TrackView => lanes[i][0](),
  /** Lane `i` waits behind a count-in the engine runs (reactive; the draw loop reads `countingValue`). */
  trackCounted: (i: number): boolean => counted[i][0](),
  masterLengthFrames: masterFrames,
  /** MIC, reached here only by the native probes (the player sets a slot to Off and goes live): the
   * device input dry through a slot without a plugin (`toggleEngineInput`). */
  inputArmed: engineInputLive,
  toggleInput: async (): Promise<boolean> => toggleEngineInput(),
  inputArmRequested: (): boolean => false,
  peaksInto,
  scopeInto,
  /** The live scope taps on or off (`SetScope`): the look that draws the columns asks for them and
   * owns turning them off (`stage/StageView.tsx`). A remembered host setting, so a rebuilt engine
   * gets it back, while `adoptSettings` ignores it: the view sends the state it wants on mount
   * instead of trusting the engine's. */
  setScope: (on: boolean): void => void sendEngine({ SetScope: on }),
  phaseValue,
  levelValue: (): number => (plain.clip ? Math.max(1, plain.level) : plain.level),
  stateOf: (i: number): TrackState => plain.state[i] ?? 'EMPTY',
  mutedOf: (i: number): boolean => plain.muted[i] ?? false,
  waitingOf: (i: number): boolean => plain.waiting[i] ?? false,
  /** A count-in runs (from its first beat's arrival), for the draw loop: no signal read. */
  countingValue: (): boolean => plain.counting,
  recHeadFrac,
  recSpanFrames,
  masterFramesValue: (): number => plain.master,
  /** The master grid's anchor (`lf_engine::Overview::grid`): loop position 0 plays at
   * `gridValue() + k * masterFramesValue()`. The draw loop places a frame-stamped scope column on the
   * loop with it, the way `phaseValue` places the playhead; no signal read. */
  gridValue: (): number => plain.clock.grid,
  /** Lane `i`'s effects as the engine applied them. */
  fxState: (i: number): readonly FxState[] => mixes[i][0]().fx,
  setFxBypass,
  setFxParam,
  setVolume,
  setPan,
  /** MUTE's press: the engine's toggle, as a pedal's (the engine refuses it on an EMPTY lane). */
  toggleMute: (i: number): void => void sendEngine({ ActionOn: [i, 'Mute'] }),
  /** Mute lane `i` or not, outright: for scripts (the native probes), not a control. */
  setMute: (i: number, on: boolean): void => void sendEngine({ SetMute: [i, on] }),
  /** Lane `i`'s volume, mute and pan as the engine applied them. */
  trackVolume: (i: number): number => mixes[i][0]().volume,
  trackMuted: (i: number): boolean => mixes[i][0]().muted,
  trackPan: (i: number): number => mixes[i][0]().pan,
  /** What lane `i`'s mix control `key` shows (the volume, DUB FEEDBACK, the pan, an effect's param): its
   * gesture's overlay, else the engine's value. */
  mixShown: (i: number, key: 'volume' | 'dubFeedback' | 'pan' | `fx${number}.${string}`): number => shownMix(i, key) as number,
  /** What effect `k`'s bypass key on lane `i` shows, as `mixShown`. */
  fxBypassShown: (i: number, k: number): boolean => shownMix(i, `fx${k}`) as boolean,
  holdMix,
  dropMix,
  fixedLengthEnabled: fixedLength,
  /** FIXED's press: the engine's toggle, refused while a take records or RETAKE overrides it. */
  toggleFixedLength: (): void => toggleSetting('Fixed'),
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
  /** AUTO REC's press: the engine's toggle, refused while a take records or once a loop locked the tempo. */
  toggleAutoRecord: (): void => toggleSetting('AutoRec'),
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

// ── Session: export, recovery and import (`src/session/session-source.ts`) ─────────────────────────

const FROM_SNAPSHOT = { Playing: 'PLAYING', Stopped: 'STOPPED', Overdubbing: 'OVERDUBBING' } as const;
const TO_LOAD = { PLAYING: 'Playing', STOPPED: 'Stopped' } as const;

/** The committed lanes: the engine's PCM (play order), each with its mix as the engine applied it where
 * the snapshot pinned the loops (the mix the wet master renders with); with `master`, the engine's wet
 * master too (or why it has none). A fader moved after the pin (the copy and the render take seconds)
 * changes neither. */
async function exportSnapshot(options: { master?: boolean } = {}): Promise<StemSnapshot> {
  const { header, pcm, master } = decodeSnapshot(await platform.engine.snapshot(options.master === true));
  return {
    ...(master ? { master } : {}),
    ...(header.masterError !== undefined ? { masterError: header.masterError } : {}),
    sampleRate: header.rate,
    masterLengthFrames: header.masterLengthFrames,
    bpm: header.bpm,
    tracks: header.tracks.map((t, k) => ({
      index: t.index,
      pcm: pcm[k],
      volume: t.mix.volume,
      muted: t.mix.muted,
      reversed: t.reversed,
      fx: validateFxStates(t.mix.fx, `snapshot: track ${t.index + 1}`),
      dubFeedback: t.mix.dubFeedback,
      pan: t.mix.pan ?? 0,
      state: FROM_SNAPSHOT[t.state],
    })),
  };
}

/**
 * Load a session into an all-empty engine: the loops and each lane's mix go to the engine in one load
 * (which sets and locks the tempo, sets each lane's mix and starts the PLAYING lanes together, so the
 * first loaded sample plays at its saved level), then, once the engine took it, the mix to this store.
 * Checks the payload before sending anything; the engine checks again.
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
    const states = validateFxStates(t.fx, `loadSession: track ${t.index + 1}`);
    // A session saved before DUB FEEDBACK sums, as it did; one saved before pan is centred.
    const pan = clampMix('pan', t.pan ?? 0) ?? 0;
    const mix = wireMix(Math.max(0, Math.min(1.5, t.volume)), t.muted, Math.max(0, Math.min(1, t.dubFeedback ?? 1)), pan, states);
    return { index: t.index, pcm: t.pcm, reversed: t.reversed, state, fx: states, mix };
  });
  const header: LoadHeader = {
    bpm,
    bars,
    masterLengthFrames: master,
    tracks: loaded.map((t) => ({ index: t.index, frames: master, reversed: t.reversed, state: TO_LOAD[t.state], mix: t.mix })),
  };
  await platform.engine.loadSession(encodeSessionBytes(header, loaded.map((t) => t.pcm)));
  // The engine applied the mix with the loops, and each loaded lane's `Mix` shows it; a control's pending
  // gesture is over (a refused load changes nothing).
  for (let i = 0; i < ENGINE_LANES; i++) cancelOverlays(i);
}

/** The engine as export, recovery and import read and write it. */
export const engineSession: SessionSource = {
  trackCount: ENGINE_LANES,
  stateOf: (i) => plain.state[i] ?? 'EMPTY',
  trackInfo: (i) => lanes[i][0](),
  revision: (i) => plain.revision[i] ?? 0,
  // A new engine since (a reset frame) voids it: its empty lanes are not the player's clear.
  clearToken: (): ClearToken | null => (plain.clear?.gen === plain.resets ? plain.clear : null),
  spendClear: (token) => {
    if (plain.clear === token) plain.clear = null;
  },
  trackVolume: (i) => mixes[i][0]().volume,
  trackMuted: (i) => mixes[i][0]().muted,
  trackDubFeedback: (i) => mixes[i][0]().dubFeedback,
  trackPan: (i) => mixes[i][0]().pan,
  fxState: (i) => mixes[i][0]().fx,
  masterFramesValue: () => plain.master,
  exportSnapshot,
  loadSession,
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

// Tap tempo stays in the UI: it averages the taps and sends SetBpm.
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

/** The `clock` the UI reads (`audio.ts`). */
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
  /** CLICK's press: the engine's toggle (`toggleSetting`). `setMetronome` is for scripts. */
  toggleMetronome: (): void => toggleSetting('Click'),
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

/** The `master` the UI reads (`audio.ts`). */
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

/** Reset frames the feed has delivered, 0 before its first. An owner whose command must not reach the
 * engine before the feed has said what the engine has waits for this: `adoptSettings` takes the
 * engine's remembered settings on that frame, and `verify/probes/engine-seam.mjs` holds the UI to
 * sending nothing ahead of it. `SetScope` is the one such owner (`src/app.tsx`). */
export const engineResets = feedResets;

// Why no device runs: the last failed open's error text, or the lost device's reason (the feed's
// `Lost`), until an open succeeds. While no device runs, the command-bar lamp, Audio Settings, the
// plugin slots and the rescan button say so instead of reading as "starting" or "no plugins".
const [openFailure, setOpenFailure] = createSignal<string | null>(null);

/** The last failed open's or lost device's reason, null once an open succeeded. Read it with
 * `engineDevice()`: a failed switch, or the owner reopening a lost device on its own, leaves a device
 * running with this still set. */
export const engineOpenFailure = openFailure;

/** The engine's rate, the UI's frame conversions read it: the running device's, else the one the engine
 * kept when its device stopped; 48 kHz until one runs (nothing is on the grid before then). */
export function engineSampleRate(): number {
  return device()?.sampleRate ?? engineRate();
}

/** A sample rate as the player reads it: "44.1 kHz". */
function kHz(rate: number): string {
  return `${rate / 1000} kHz`;
}

function deviceLabel(s: DeviceStatus): string {
  // An output-only device has no input name.
  return !s.inputOpen || s.inputName === s.outputName ? s.outputName : `${s.inputName} → ${s.outputName}`;
}

let openTail: Promise<unknown> = Promise.resolve();

/** The saved picks that name a device. */
type DevicePicks = Pick<
  AudioDeviceSettings,
  'inputDeviceId' | 'inputChannel' | 'slotInputChannels' | 'outputDeviceId' | 'bufferFrames' | 'sampleRate' | 'asioEnabled'
>;

/** A device as this store asks for it: the request, and the picks that named it. */
interface DeviceChoice {
  request: DeviceRequest;
  picks: DevicePicks;
}

/** A device choice that runs. */
type Running = DeviceChoice & { status: DeviceStatus };

/** The device that runs, as this store opened it. A switch the player declines, one whose recovery save
 * fails, or one that fails to open puts its picks back. */
let opened: DeviceChoice | null = null;

/** A saved channel pick as the engine takes it: null = auto. */
const channelOf = (pick: string): number | null => (pick === '' ? null : Number(pick));

/** `choice` with each slot's channel pick `picks` (switched in place, or dropped as lacking). */
function withSlotPicks(choice: DeviceChoice, picks: readonly [string, string]): DeviceChoice {
  const { backend, input, output, buffer, sampleRate } = choice.request;
  return {
    request: { backend, input, output, buffer, sampleRate, inputChannels: [channelOf(picks[0]), channelOf(picks[1])] },
    picks: { ...choice.picks, slotInputChannels: [picks[0], picks[1]] },
  };
}

/**
 * Reset to Auto each slot's saved pick the device does not give that slot: `asked` is the channel each
 * slot asked for (null = auto), `inUse` what the device's status says each slot reads (a device without
 * the pick reads auto). So a slot's input never names a channel it does not read.
 */
function dropLackingPicks(asked: readonly (number | null)[], inUse: readonly number[] | undefined): void {
  if (!inUse) return;
  const picks = slotInputChannels();
  const lacking = (s: 0 | 1) => picks[s] !== '' && asked[s] === Number(picks[s]) && inUse[s] !== asked[s];
  if (!lacking(0) && !lacking(1)) return;
  const kept: [string, string] = [lacking(0) ? '' : picks[0], lacking(1) ? '' : picks[1]];
  saveSlotInputChannels(kept); // the device side logs the fallback
  if (opened) opened = withSlotPicks(opened, kept);
}

/** The channel each slot of `request` asks for. */
const askedChannels = (request: DeviceRequest): (number | null)[] =>
  'inputChannels' in request ? request.inputChannels : [request.inputChannel, request.inputChannel];

/** The device the saved picks name. */
function picked(): DeviceChoice {
  const s = readAudioDeviceSettings();
  const asio = usingAsio();
  return {
    request: {
      backend: asio ? 'Asio' : 'Wasapi',
      input: asio ? null : s.inputDeviceId || null,
      output: asio ? null : s.outputDeviceId || null,
      inputChannels: [channelOf(s.slotInputChannels[0]), channelOf(s.slotInputChannels[1])],
      buffer: s.bufferFrames,
      sampleRate: s.sampleRate,
    },
    picks: {
      inputDeviceId: s.inputDeviceId,
      inputChannel: s.inputChannel,
      slotInputChannels: s.slotInputChannels,
      outputDeviceId: s.outputDeviceId,
      bufferFrames: s.bufferFrames,
      sampleRate: s.sampleRate,
      asioEnabled: s.asioEnabled,
    },
  };
}

/**
 * Open (or switch to) the device Audio Settings names: ASIO's cached driver when the ASIO tier is in
 * use, else the saved WASAPI endpoints; the saved channel, buffer and rate pick either way. Serialized, so rapid
 * picks land in order. Resolves null (and toasts) when the device did not open; the picks go back to
 * the device that runs.
 *
 * A device at another rate than the engine's while it holds audio is refused (`OpenError::RateChange`):
 * the loops cannot play there. The player confirms; declined, the picks go back to the device that runs
 * (which resolves); confirmed, the loops go to the recovery first (`switchDroppingLoops`).
 */
export function openEngineDevice(): Promise<DeviceStatus | null> {
  return serialize(async () => (await openPicked()).status);
}

/** Run `op` after every open queued before it (a failed one does not stop the queue). */
function serialize<T>(op: () => Promise<T>): Promise<T> {
  const run = openTail.then(op);
  openTail = run.catch(() => undefined);
  return run;
}

/** `openEngineDevice`'s open, inside the queue. `declined`: the player kept the device that runs.
 * `restore` (a driver switch's) puts the driver that ran back when a recovery save fails. */
async function openPicked(restore?: () => Promise<void>): Promise<{ status: DeviceStatus | null; declined: boolean }> {
  const wanted = picked();
  try {
    let runs: Running;
    try {
      runs = { ...wanted, status: await platform.engine.open(wanted.request) };
    } catch (err) {
      const refused = decodeOpenError(err);
      if (refused.type !== 'RateChange') throw err;
      const ask =
        `${refused.device} runs at ${kHz(refused.to)}. Your loops were recorded at ${kHz(refused.from)} and ` +
        `cannot play there. They stay in recovery and come back the next time BleepLoop starts at ` +
        `${kHz(refused.from)}.\n\nSwitch anyway?`;
      if (!window.confirm(ask)) {
        putBackPicks();
        return { status: device(), declined: true };
      }
      runs = await switchDroppingLoops(wanted, restore);
    }
    setOpenFailure(null);
    setDevice(runs.status);
    setEngineRate(runs.status.sampleRate);
    opened = { request: runs.request, picks: runs.picks };
    dropLackingPicks(askedChannels(runs.request), runs.status.inputChannels);
    return { status: runs.status, declined: false };
  } catch (err) {
    console.error('[engine] device open failed', err);
    notifyError("Couldn't open the audio device", err);
    setOpenFailure(errorText(err));
    // The device that ran keeps running, or the owner reopens it (`Owner::open`,
    // `src-tauri/src/engine_io/owner.rs`): its picks go back, saved and shown.
    putBackPicks();
    return { status: null, declined: false };
  }
}

/** An open's error as one short line (the toast keeps the whole text). */
function errorText(err: unknown): string {
  const text = (err instanceof Error ? err.message : String(err)).split('\n')[0].trim();
  return text.length > 120 ? `${text.slice(0, 119)}…` : text || 'unknown error';
}

/**
 * Switch the ASIO driver live ('' = automatic): the host's device owner closes a device running on
 * ASIO (nothing may hold the driver while it is replaced), drops the cached driver, starts this one and
 * opens the device again on it; then the saved picks open. A driver that does not start leaves ASIO off,
 * so the picks open on WASAPI and Audio Settings says why. A refused switch reopens what ran. A switch
 * the player declines because the new driver runs at another rate, or whose recovery save fails, goes
 * back to the driver that ran.
 */
export function switchEngineAsioDriver(driver: string): Promise<DeviceStatus | null> {
  return serialize(async () => {
    const previous = readAudioDeviceSettings().asioDriver;
    const switchBack = async () => {
      try {
        await switchAsioDriver(previous);
      } catch (err) {
        console.error('[engine] ASIO driver switch back failed', err);
        notifyError("Couldn't switch back to the previous ASIO driver", err);
      }
    };
    try {
      await switchAsioDriver(driver);
    } catch (err) {
      console.error('[engine] ASIO driver switch failed', err);
      notifyError("Couldn't switch the ASIO driver", err);
      return (await openPicked()).status;
    }
    if (previous === driver) return (await openPicked()).status;
    const reopened = await openPicked(switchBack);
    if (!reopened.declined) return reopened.status;
    await switchBack();
    return (await openPicked()).status;
  });
}

/** Save the picks of the device that runs again (a declined or failed switch): Audio Settings shows them. */
function putBackPicks(): void {
  if (!opened) return;
  const { bufferFrames, sampleRate, asioEnabled, slotInputChannels: slots, ...ids } = opened.picks;
  writeAudioDeviceSettings(ids);
  saveSlotInputChannels(slots);
  void setBufferSize(bufferFrames);
  void setSampleRatePick(sampleRate);
  void setAsioEnabled(asioEnabled);
}

/**
 * The player confirmed a switch that drops the loops from the engine. The device stops first, so a take
 * or layer in flight punches out and commits (STATUS E3); the recovery saves the engine's snapshot, read
 * now, whatever the lanes here still show (the commit's feed frame may not be in yet); only then does
 * the switch go ahead, forced. A save that fails reopens the device that ran instead, so the switch never
 * loses loops the recovery does not hold; a driver switch's `restore` first puts the driver that ran back,
 * or its request would open on the new driver and be refused again. Resolves with the device that runs,
 * as it was asked for.
 */
async function switchDroppingLoops(wanted: DeviceChoice, restore?: () => Promise<void>): Promise<Running> {
  await platform.engine.close();
  try {
    await autosave.saveNow();
  } catch (err) {
    console.error('[engine] the recovery save before a rate change failed', err);
    notifyError('The device did not switch', 'The loops could not be saved to recovery first, so they stay here.');
    if (!opened) throw err;
    const ran = opened;
    putBackPicks();
    await restore?.();
    return { ...ran, status: await platform.engine.open(ran.request) };
  }
  return { ...wanted, status: await platform.engine.open(wanted.request, true) };
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

/** Each slot's capture channel pick ('' = auto, else 0-based; `audio-devices.ts` keeps it). */
export const engineSlotInputChannels = slotInputChannels;

/**
 * Switch slot `slot`'s capture channel ('' = auto) without reopening the device, and keep the pick for
 * the next open. On the open queue, so a switch never races an open: an open asked before it runs on
 * the old pick and the switch then applies in place; one asked after it reads the new pick. A channel
 * the running device refuses puts the previous pick back.
 */
export function setEngineSlotInputChannel(slot: 0 | 1, channel: string): Promise<void> {
  return serialize(async () => {
    const before = slotInputChannels()[slot];
    saveSlotInputChannels(withAt(slotInputChannels(), slot, channel));
    if (!device()) return; // the next open takes it
    try {
      await platform.engine.setSlotInputChannel(slot, channelOf(channel));
      if (opened) opened = withSlotPicks(opened, slotInputChannels());
    } catch (err) {
      saveSlotInputChannels(withAt(slotInputChannels(), slot, before));
      console.error('[engine] input channel switch failed', err);
      notifyError("Couldn't switch the input channel", err);
    }
  });
}

/** Both slots' capture channel at once (the native loopback probe's pick, `src/debug/engine-loopback.ts`). */
export function setEngineInputChannel(channel: string): void {
  for (const slot of [0, 1] as const) void setEngineSlotInputChannel(slot, channel);
}

/** Subscribe to the feed; its first frame is a reset (`adoptSettings`). Returns the unsubscribe. */
export function startEngineStore(): () => void {
  return platform.engine.subscribe(applyFrame);
}
