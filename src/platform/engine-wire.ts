/**
 * OWNS: the JSON wire between the UI and the native engine host (`docs/plans/native-engine.md`
 * § Stage 5, Wire): the payloads of the `engine_*` Tauri commands, the `Command` batch the UI sends and
 * the feed frame it reads back. The Rust mirror is `src-tauri/src/engine_io/wire.rs`; one fixture,
 * `verify/fixtures/engine-wire.json`, holds both sides to the same JSON.
 *
 * Serde's external tagging with the Rust variant names (`src-tauri/crates/lf-engine/src/api.rs`): a
 * unit variant is its name (`"PlayAll"`), a newtype `{"RecDub":0}`, a tuple `{"SetVolume":[0,0.8]}`, a
 * struct variant an object with camelCase fields. `FxParam`/`FxKind` travel as the TS keys
 * (`src/audio/fx/metadata.ts`), an `Instrument` as its id, a `Frame` (i64) as a JSON number.
 *
 * Commands go out as the typed values below (Tauri serialises them). Everything that comes back passes
 * a decoder that throws on a variant or a field it does not know how to read, so a drift between the
 * mirrors fails at the first frame instead of drawing garbage; unknown extra fields are ignored. Pure:
 * no Tauri, DOM or Solid import, so a Node guard imports this file directly.
 */

/** A device frame (Rust `lf_engine::Frame`, i64). */
export type Frame = number;

export type LaneState = 'Empty' | 'Recording' | 'Overdubbing' | 'Playing' | 'Stopped';
export type Refusal =
  | 'Stopping'
  | 'PlayFirst'
  | 'Reversed'
  | 'OtherRecording'
  | 'Empty'
  | 'NoUndo'
  | 'NoClear'
  | 'ConfirmClear';
/** The hands-free actions (`src/app/actions.ts`; GO LIVE stays with the plugin host). */
export type EngineAction = 'RecDub' | 'PlayStop' | 'Undo' | 'Clear' | 'NextTrack' | 'PrevTrack' | 'PlayAll' | 'StopAll';
/** The built-in instruments by id (`src/audio/synths/index.ts`). */
export type InstrumentId = 'lead' | 'pad' | 'piano' | 'organ' | 'bass' | 'drum';
export type FxKindId = 'filter' | 'pitch' | 'stutter' | 'delay' | 'reverb';
export type FxParamId = 'cutoff' | 'q' | 'semitones' | 'rate' | 'time' | 'feedback' | 'mix' | 'amount';
export type NoteTarget = { Builtin: InstrumentId } | { Slot: number };
export type AudioBackend = 'Wasapi' | 'Asio';

const LANE_STATES: readonly LaneState[] = ['Empty', 'Recording', 'Overdubbing', 'Playing', 'Stopped'];
const REFUSALS: readonly Refusal[] = [
  'Stopping',
  'PlayFirst',
  'Reversed',
  'OtherRecording',
  'Empty',
  'NoUndo',
  'NoClear',
  'ConfirmClear',
];
const ACTIONS: readonly EngineAction[] = ['RecDub', 'PlayStop', 'Undo', 'Clear', 'NextTrack', 'PrevTrack', 'PlayAll', 'StopAll'];
const INSTRUMENTS: readonly InstrumentId[] = ['lead', 'pad', 'piano', 'organ', 'bass', 'drum'];
const FX_KINDS: readonly FxKindId[] = ['filter', 'pitch', 'stutter', 'delay', 'reverb'];
const FX_PARAMS: readonly FxParamId[] = ['cutoff', 'q', 'semitones', 'rate', 'time', 'feedback', 'mix', 'amount'];
const BACKENDS: readonly AudioBackend[] = ['Wasapi', 'Asio'];

/** Lanes and plugin slots (`lf_engine::TRACK_COUNT`, `SLOT_COUNT`). */
export const ENGINE_LANES = 5;
export const ENGINE_SLOTS = 2;

/** Rust `lf_engine::Command`, as it crosses `engine_send`. */
export type EngineCommand =
  | 'PlayAll'
  | 'StopAll'
  | 'ClearAll'
  | 'AllNotesOff'
  | { RecDub: number }
  | { PlayStop: number }
  | { Stop: number }
  | { Undo: number }
  | { Reverse: number }
  | { Copy: number }
  | { Clear: number }
  | { SelectTrack: number }
  | { Action: EngineAction }
  | { ActionOn: [number, EngineAction] }
  | { SetBpm: number }
  | { SetMetronome: boolean }
  | { SetClickVolume: number }
  | { SetMasterVolume: number }
  | { SetMasterMute: boolean }
  | { SetLoopEndStop: boolean }
  | { SetFixedLength: boolean }
  | { SetFixedBars: number }
  | { SetRetake: boolean }
  | { SetAutoRecord: boolean }
  | { SetAutoSensitivity: number }
  | { SetVolume: [number, number] }
  | { SetMute: [number, boolean] }
  | { SetFxParam: [number, FxParamId, number] }
  | { SetFxBypass: [number, FxKindId, boolean] }
  | { SelectInstrument: NoteTarget }
  | { NoteOn: [number, number] }
  | { NoteOff: number }
  | { PitchBend: number }
  | { Modulation: number }
  | { SetSlotLive: [number, boolean] }
  | { SetSlotGain: [number, number] };

/** Rust `engine_io::DeviceRequest`: what `engine_open` opens (or switches to). */
export interface DeviceRequest {
  backend: AudioBackend;
  /** WASAPI capture / render endpoint id; null = the default. ASIO ignores both (the cached driver). */
  input: string | null;
  output: string | null;
  /** 0-based capture channel; null = auto. */
  inputChannel: number | null;
  /** Frames per device callback; null = the driver's default. */
  buffer: number | null;
}

/** Rust `engine_io::DeviceStatus`: the device that runs. */
export interface DeviceStatus {
  backend: AudioBackend;
  sampleRate: number;
  block: number;
  inputName: string;
  outputName: string;
  /** Input plus output latency the driver reports, and its input side (frames). */
  alignFrames: Frame;
  inputFrames: Frame;
}

/** Rust `lf_engine::LaneInfo` (the TS `TrackPublic`). */
export interface LaneInfo {
  state: LaneState;
  length: Frame;
  armed: boolean;
  autoArmed: boolean;
  canUndo: boolean;
  canReverse: boolean;
  reversed: boolean;
  stopAt: Frame | null;
  retakePass: number;
}

/** Rust `lf_engine::Event`, decoded to a `type`-tagged union. */
export type EngineEvent =
  | { type: 'Lane'; frame: Frame; lane: number; info: LaneInfo }
  | { type: 'Transport'; frame: Frame; master: Frame; bpm: number; locked: boolean }
  | { type: 'Beat'; frame: Frame; beatInBar: number; countLeft: number; clicked: boolean }
  | { type: 'Selected'; frame: Frame; lane: number }
  | { type: 'Refused'; frame: Frame; lane: number; reason: Refusal }
  | { type: 'TakeRejected'; frame: Frame; lane: number; overdub: boolean }
  | { type: 'PassDropped'; frame: Frame; lane: number; pass: number }
  | { type: 'Copied'; frame: Frame; from: number; to: number };

/** Rust `engine_io::DeviceEvent`, decoded to a `type`-tagged union. */
export type DeviceEvent =
  | { type: 'Lost'; backend: AudioBackend; reason: string }
  | { type: 'Recovered'; status: DeviceStatus }
  | { type: 'Fallback'; status: DeviceStatus }
  | { type: 'ShareLost'; reason: string }
  | { type: 'EngineFaulted' };

/**
 * Where the playhead comes from: the callback rendering device frame `frame` entered at Unix time
 * `atMs`, the device runs `rate` frames a second, and loop position 0 plays at `grid + k * master` (the
 * looper's grid anchor). The player hears a frame the output latency after it renders
 * (`DeviceStatus.alignFrames - inputFrames`).
 */
export interface ClockAnchor {
  frame: Frame;
  atMs: number;
  rate: number;
  grid: Frame;
}

/** The input's peak since the last frame (linear) and whether it clipped. */
export interface Meter {
  peak: number;
  clip: boolean;
}

/**
 * Changed waveform bins of one lane: bins `[start, start + min.length)` of 1024 frames each (the web
 * looper's `PEAK_FRAMES`), from loop position 0 (a take's first frame while it records), in the order
 * they play (a reversed lane reversed). `count` is the lane's valid bin total after this update
 * (0 = cleared).
 */
export interface PeakUpdate {
  lane: number;
  start: number;
  count: number;
  min: number[];
  max: number[];
}

/** One `engine_feed` message. */
export interface FeedFrame {
  seq: number;
  /** The first frame after a subscribe or a new engine: the UI replaces its state with this one. */
  reset: boolean;
  events: EngineEvent[];
  device: DeviceEvent[];
  /** Absent = unchanged; null = no device runs; else the device that runs now. */
  status?: DeviceStatus | null;
  anchor: ClockAnchor | null;
  meter: Meter | null;
  peaks: PeakUpdate[];
}

// ── Decoders ───────────────────────────────────────────────────────────────────────────────────────

type Obj = Record<string, unknown>;

function fail(what: string, got: unknown): never {
  throw new Error(`engine wire: ${what}, got ${JSON.stringify(got)}`);
}

function obj(v: unknown, what: string): Obj {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) fail(`${what} must be an object`, v);
  return v as Obj;
}

function num(v: unknown, what: string): number {
  if (typeof v !== 'number' || !Number.isFinite(v)) fail(`${what} must be a finite number`, v);
  return v;
}

function int(v: unknown, what: string, min = 0, max = Number.MAX_SAFE_INTEGER): number {
  if (!Number.isSafeInteger(v) || (v as number) < min || (v as number) > max) {
    fail(`${what} must be an integer in ${min}..${max}`, v);
  }
  return v as number;
}

function bool(v: unknown, what: string): boolean {
  if (typeof v !== 'boolean') fail(`${what} must be a boolean`, v);
  return v;
}

function str(v: unknown, what: string): string {
  if (typeof v !== 'string') fail(`${what} must be a string`, v);
  return v;
}

function oneOf<T extends string>(v: unknown, set: readonly T[], what: string): T {
  if (!set.includes(v as T)) fail(`${what} must be one of ${set.join('|')}`, v);
  return v as T;
}

function array(v: unknown, what: string, length?: number): unknown[] {
  if (!Array.isArray(v) || (length !== undefined && v.length !== length)) {
    fail(`${what} must be an array${length === undefined ? '' : ` of ${length}`}`, v);
  }
  return v;
}

/** An externally tagged enum value: a unit variant's name, or a one-key object. */
function tagged(v: unknown, what: string): [string, unknown] {
  if (typeof v === 'string') return [v, undefined];
  const o = obj(v, what);
  const keys = Object.keys(o);
  if (keys.length !== 1) fail(`${what} must carry exactly one variant`, v);
  return [keys[0], o[keys[0]]];
}

const lane = (v: unknown, what: string) => int(v, what, 0, ENGINE_LANES - 1);
const slot = (v: unknown, what: string) => int(v, what, 0, ENGINE_SLOTS - 1);
const frame = (v: unknown, what: string) => int(v, what, Number.MIN_SAFE_INTEGER);
const nullable = <T>(v: unknown, read: (v: unknown) => T): T | null => (v === null || v === undefined ? null : read(v));

function decodeLaneInfo(v: unknown): LaneInfo {
  const o = obj(v, 'LaneInfo');
  return {
    state: oneOf(o.state, LANE_STATES, 'LaneInfo.state'),
    length: int(o.length, 'LaneInfo.length'),
    armed: bool(o.armed, 'LaneInfo.armed'),
    autoArmed: bool(o.autoArmed, 'LaneInfo.autoArmed'),
    canUndo: bool(o.canUndo, 'LaneInfo.canUndo'),
    canReverse: bool(o.canReverse, 'LaneInfo.canReverse'),
    reversed: bool(o.reversed, 'LaneInfo.reversed'),
    stopAt: o.stopAt === null ? null : frame(o.stopAt, 'LaneInfo.stopAt'),
    retakePass: int(o.retakePass, 'LaneInfo.retakePass'),
  };
}

export function decodeEvent(raw: unknown): EngineEvent {
  const [name, payload] = tagged(raw, 'Event');
  const o = obj(payload, `Event.${name}`);
  const at = frame(o.frame, `${name}.frame`);
  switch (name) {
    case 'Lane':
      return { type: 'Lane', frame: at, lane: lane(o.lane, 'Lane.lane'), info: decodeLaneInfo(o.info) };
    case 'Transport':
      return {
        type: 'Transport',
        frame: at,
        master: int(o.master, 'Transport.master'),
        bpm: int(o.bpm, 'Transport.bpm'),
        locked: bool(o.locked, 'Transport.locked'),
      };
    case 'Beat':
      return {
        type: 'Beat',
        frame: at,
        beatInBar: int(o.beatInBar, 'Beat.beatInBar', 0, 255),
        countLeft: int(o.countLeft, 'Beat.countLeft', 0, 255),
        clicked: bool(o.clicked, 'Beat.clicked'),
      };
    case 'Selected':
      return { type: 'Selected', frame: at, lane: lane(o.lane, 'Selected.lane') };
    case 'Refused':
      return { type: 'Refused', frame: at, lane: lane(o.lane, 'Refused.lane'), reason: oneOf(o.reason, REFUSALS, 'Refused.reason') };
    case 'TakeRejected':
      return { type: 'TakeRejected', frame: at, lane: lane(o.lane, 'TakeRejected.lane'), overdub: bool(o.overdub, 'TakeRejected.overdub') };
    case 'PassDropped':
      return { type: 'PassDropped', frame: at, lane: lane(o.lane, 'PassDropped.lane'), pass: int(o.pass, 'PassDropped.pass') };
    case 'Copied':
      return { type: 'Copied', frame: at, from: lane(o.from, 'Copied.from'), to: lane(o.to, 'Copied.to') };
    default:
      return fail('unknown Event variant', raw);
  }
}

export function decodeDeviceStatus(raw: unknown): DeviceStatus {
  const o = obj(raw, 'DeviceStatus');
  return {
    backend: oneOf(o.backend, BACKENDS, 'DeviceStatus.backend'),
    sampleRate: int(o.sampleRate, 'DeviceStatus.sampleRate', 1),
    block: int(o.block, 'DeviceStatus.block'),
    inputName: str(o.inputName, 'DeviceStatus.inputName'),
    outputName: str(o.outputName, 'DeviceStatus.outputName'),
    alignFrames: int(o.alignFrames, 'DeviceStatus.alignFrames'),
    inputFrames: int(o.inputFrames, 'DeviceStatus.inputFrames'),
  };
}

export function decodeDeviceEvent(raw: unknown): DeviceEvent {
  const [name, payload] = tagged(raw, 'DeviceEvent');
  switch (name) {
    case 'Lost': {
      const o = obj(payload, 'DeviceEvent.Lost');
      return { type: 'Lost', backend: oneOf(o.backend, BACKENDS, 'Lost.backend'), reason: str(o.reason, 'Lost.reason') };
    }
    case 'Recovered':
      return { type: 'Recovered', status: decodeDeviceStatus(payload) };
    case 'Fallback':
      return { type: 'Fallback', status: decodeDeviceStatus(payload) };
    case 'ShareLost':
      return { type: 'ShareLost', reason: str(obj(payload, 'DeviceEvent.ShareLost').reason, 'ShareLost.reason') };
    case 'EngineFaulted':
      if (payload !== undefined) fail('EngineFaulted is a unit variant', raw);
      return { type: 'EngineFaulted' };
    default:
      return fail('unknown DeviceEvent variant', raw);
  }
}

function decodePeaks(raw: unknown): PeakUpdate {
  const o = obj(raw, 'peaks[]');
  const min = array(o.min, 'peaks.min').map((v) => num(v, 'peaks.min[]'));
  const max = array(o.max, 'peaks.max', min.length).map((v) => num(v, 'peaks.max[]'));
  const update: PeakUpdate = {
    lane: lane(o.lane, 'peaks.lane'),
    start: int(o.start, 'peaks.start'),
    count: int(o.count, 'peaks.count'),
    min,
    max,
  };
  if (update.start + min.length > update.count) fail('peaks bins must end within count', raw);
  return update;
}

export function decodeFeedFrame(raw: unknown): FeedFrame {
  const o = obj(raw, 'feed frame');
  const device = o.device === undefined || o.device === null ? [] : Array.isArray(o.device) ? o.device : [o.device];
  const out: FeedFrame = {
    seq: int(o.seq, 'feed.seq'),
    reset: bool(o.reset, 'feed.reset'),
    events: array(o.events, 'feed.events').map(decodeEvent),
    device: device.map(decodeDeviceEvent),
    anchor: nullable(o.anchor, (v) => {
      const a = obj(v, 'feed.anchor');
      return {
        frame: frame(a.frame, 'anchor.frame'),
        atMs: num(a.atMs, 'anchor.atMs'),
        rate: num(a.rate, 'anchor.rate'),
        grid: frame(a.grid, 'anchor.grid'),
      };
    }),
    meter: nullable(o.meter, (v) => {
      const m = obj(v, 'feed.meter');
      return { peak: num(m.peak, 'meter.peak'), clip: bool(m.clip, 'meter.clip') };
    }),
    peaks: o.peaks === undefined || o.peaks === null ? [] : array(o.peaks, 'feed.peaks').map(decodePeaks),
  };
  if ('status' in o) out.status = o.status === null ? null : decodeDeviceStatus(o.status);
  return out;
}

// ── Command and request validators (the fixture guard; the UI builds these values typed) ────────────

const UNIT_COMMANDS = ['PlayAll', 'StopAll', 'ClearAll', 'AllNotesOff'] as const;
const LANE_COMMANDS = ['RecDub', 'PlayStop', 'Stop', 'Undo', 'Reverse', 'Copy', 'Clear', 'SelectTrack'] as const;
const BOOL_COMMANDS = ['SetMetronome', 'SetMasterMute', 'SetLoopEndStop', 'SetFixedLength', 'SetRetake', 'SetAutoRecord'] as const;
const NUMBER_COMMANDS = [
  'SetBpm',
  'SetClickVolume',
  'SetMasterVolume',
  'SetFixedBars',
  'SetAutoSensitivity',
  'PitchBend',
  'Modulation',
] as const;

/**
 * Check that `raw` is a `Command` the Rust side reads; returns it typed. Throws otherwise. For the
 * fixture guard (the UI builds commands typed).
 * @public
 */
export function decodeCommand(raw: unknown): EngineCommand {
  const [name, p] = tagged(raw, 'Command');
  const pair = (what: string) => array(p, `${name} ${what}`, 2);
  if ((UNIT_COMMANDS as readonly string[]).includes(name)) {
    if (p !== undefined) fail(`${name} is a unit variant`, raw);
  } else if ((LANE_COMMANDS as readonly string[]).includes(name)) {
    lane(p, name);
  } else if ((BOOL_COMMANDS as readonly string[]).includes(name)) {
    bool(p, name);
  } else if ((NUMBER_COMMANDS as readonly string[]).includes(name)) {
    num(p, name);
  } else {
    switch (name) {
      case 'Action':
        oneOf(p, ACTIONS, 'Action');
        break;
      case 'ActionOn': {
        const [l, a] = pair('(lane, action)');
        lane(l, 'ActionOn.lane');
        oneOf(a, ACTIONS, 'ActionOn.action');
        break;
      }
      case 'SetVolume': {
        const [l, v] = pair('(lane, volume)');
        lane(l, 'SetVolume.lane');
        num(v, 'SetVolume.volume');
        break;
      }
      case 'SetMute': {
        const [l, v] = pair('(lane, muted)');
        lane(l, 'SetMute.lane');
        bool(v, 'SetMute.muted');
        break;
      }
      case 'SetFxParam': {
        const [l, k, v] = array(p, 'SetFxParam (lane, param, value)', 3);
        lane(l, 'SetFxParam.lane');
        oneOf(k, FX_PARAMS, 'SetFxParam.param');
        num(v, 'SetFxParam.value');
        break;
      }
      case 'SetFxBypass': {
        const [l, k, v] = array(p, 'SetFxBypass (lane, kind, bypassed)', 3);
        lane(l, 'SetFxBypass.lane');
        oneOf(k, FX_KINDS, 'SetFxBypass.kind');
        bool(v, 'SetFxBypass.bypassed');
        break;
      }
      case 'SelectInstrument': {
        const [target, v] = tagged(p, 'NoteTarget');
        if (target === 'Builtin') oneOf(v, INSTRUMENTS, 'NoteTarget.Builtin');
        else if (target === 'Slot') slot(v, 'NoteTarget.Slot');
        else fail('unknown NoteTarget variant', p);
        break;
      }
      case 'NoteOn': {
        const [n, v] = pair('(note, velocity)');
        int(n, 'NoteOn.note', 0, 127);
        num(v, 'NoteOn.velocity');
        break;
      }
      case 'NoteOff':
        int(p, 'NoteOff.note', 0, 127);
        break;
      case 'SetSlotLive': {
        const [s, v] = pair('(slot, live)');
        slot(s, 'SetSlotLive.slot');
        bool(v, 'SetSlotLive.live');
        break;
      }
      case 'SetSlotGain': {
        const [s, v] = pair('(slot, gain)');
        slot(s, 'SetSlotGain.slot');
        num(v, 'SetSlotGain.gain');
        break;
      }
      default:
        fail('unknown Command variant', raw);
    }
  }
  return raw as EngineCommand;
}

/**
 * Check that `raw` is a `DeviceRequest`; returns it typed. Throws otherwise. For the fixture guard.
 * @public
 */
export function decodeDeviceRequest(raw: unknown): DeviceRequest {
  const o = obj(raw, 'DeviceRequest');
  return {
    backend: oneOf(o.backend, BACKENDS, 'DeviceRequest.backend'),
    input: nullable(o.input, (v) => str(v, 'DeviceRequest.input')),
    output: nullable(o.output, (v) => str(v, 'DeviceRequest.output')),
    inputChannel: nullable(o.inputChannel, (v) => int(v, 'DeviceRequest.inputChannel')),
    buffer: nullable(o.buffer, (v) => int(v, 'DeviceRequest.buffer', 1)),
  };
}
