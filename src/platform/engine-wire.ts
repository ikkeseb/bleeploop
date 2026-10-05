/**
 * OWNS: the JSON wire between the UI and the native engine host: the payloads of the `engine_*` Tauri commands, the `Command` batch the UI sends and
 * the feed frame it reads back. The Rust mirror is `src-tauri/src/engine_io/wire.rs`; one fixture,
 * `verify/fixtures/engine-wire.json`, holds both sides to the same JSON.
 *
 * Serde's external tagging with the Rust variant names (`src-tauri/crates/lf-engine/src/api.rs`): a
 * unit variant is its name (`"PlayAll"`), a newtype `{"RecDub":0}`, a tuple `{"SetVolume":[0,0.8]}`, a
 * struct variant an object with camelCase fields. `FxParam`/`FxKind` travel as the TS keys
 * (`src/ui/state/fx-metadata.ts`), an `InputSend`/`InputSendParam` as its key (`api.rs`), an `Instrument`
 * as its id, a `Frame` (i64) as a JSON number.
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
  | 'ConfirmClear'
  | 'Capturing'
  | 'NoTrim'
  | 'NoMute'
  | 'NoReverse'
  | 'NoCopy'
  | 'NoFreeLane'
  | 'Fading'
  | 'NoFade';
/** The hands-free actions (`src/app/actions.ts`; GO LIVE stays with the plugin host). `Halve` is TRIM to
 * the first half of the loop's bars; `Hold` and `Release` are HOLD's press and release, by the number of
 * the control (0..255) whose press the release ends; `FadeAll` is FADE. */
export type EngineAction =
  | 'RecDub'
  | 'PlayStop'
  | 'Undo'
  | 'Clear'
  | 'NextTrack'
  | 'PrevTrack'
  | 'PlayAll'
  | 'StopAll'
  | 'Mute'
  | 'Reverse'
  | 'Copy'
  | 'Halve'
  | { Hold: number }
  | { Release: number }
  | 'FadeAll';
/** The built-in instruments by id (`src/ui/state/instruments.ts`). */
export type InstrumentId = 'lead' | 'pad' | 'piano' | 'organ' | 'bass' | 'drum';
export type FxKindId = 'filter' | 'pitch' | 'stutter' | 'delay' | 'reverb';
export type FxParamId = 'cutoff' | 'q' | 'semitones' | 'rate' | 'time' | 'feedback' | 'mix' | 'amount';
/** Where the notes go: a built-in instrument, a plugin slot's plugin, or nowhere (`'Off'`: a slot whose
 * source is off; switching to it releases the held notes as any switch does). */
export type NoteTarget = { Builtin: InstrumentId } | { Slot: number } | 'Off';
/** The input sends (Rust `lf_engine::InputSend`, `InputSendParam`): ECHO, REVERB and RING MOD on the live
 * input. */
export type InputSendId = 'echo' | 'reverb' | 'ring';
export type InputSendParamId = 'echoTime' | 'echoFeedback' | 'echoLevel' | 'reverbLevel' | 'ringFreq' | 'ringLevel';
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
  'Capturing',
  'NoTrim',
  'NoMute',
  'NoReverse',
  'NoCopy',
  'NoFreeLane',
  'Fading',
  'NoFade',
];
/** The unit actions; `Hold` and `Release` carry their control (`decodeAction`). */
const ACTIONS: readonly Extract<EngineAction, string>[] = [
  'RecDub',
  'PlayStop',
  'Undo',
  'Clear',
  'NextTrack',
  'PrevTrack',
  'PlayAll',
  'StopAll',
  'Mute',
  'Reverse',
  'Copy',
  'Halve',
  'FadeAll',
];
const INSTRUMENTS: readonly InstrumentId[] = ['lead', 'pad', 'piano', 'organ', 'bass', 'drum'];
const FX_KINDS: readonly FxKindId[] = ['filter', 'pitch', 'stutter', 'delay', 'reverb'];
const FX_PARAMS: readonly FxParamId[] = ['cutoff', 'q', 'semitones', 'rate', 'time', 'feedback', 'mix', 'amount'];
/** Each effect's params, in `FX_KINDS` (chain) order and its defs' order (`FX_PARAM_DEFS`). */
const FX_KIND_PARAMS: readonly (readonly FxParamId[])[] = [['cutoff', 'q'], ['semitones'], ['rate'], ['time', 'feedback', 'mix'], ['amount']];
const INPUT_SENDS: readonly InputSendId[] = ['echo', 'reverb', 'ring'];
const INPUT_SEND_PARAMS: readonly InputSendParamId[] = ['echoTime', 'echoFeedback', 'echoLevel', 'reverbLevel', 'ringFreq', 'ringLevel'];
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
  /** A hands-free press the engine does not run as an action (TAP, a toggle, an input send, GO LIVE), sent
   * before the setting it changes: it disarms a pending pedal CLEAR, which the setting alone does not. */
  | 'Press'
  | { RecDub: number }
  | { PlayStop: number }
  | { Stop: number }
  | { Undo: number }
  | { Reverse: number }
  | { Copy: number }
  /** TRIM (F16): the lane keeps its first `bars` bars as heard, repeated across the loop. */
  | { Trim: [number, number] }
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
  /** FADE's length: 1, 2, 4 or 8 bars. */
  | { SetFadeBars: number }
  | { SetFixedLength: boolean }
  | { SetFixedBars: number }
  | { SetRetake: boolean }
  | { SetAutoRecord: boolean }
  | { SetAutoSensitivity: number }
  | { SetVolume: [number, number] }
  | { SetMute: [number, boolean] }
  /** DUB FEEDBACK, 0..1: what an overdub keeps of the loop it passes over (0 replaces it). */
  | { SetDubFeedback: [number, number] }
  /** A lane's pan, -1 (hard left) to 1 (hard right), 0 the centre; the engine clamps it and glides there. */
  | { SetPan: [number, number] }
  | { SetFxParam: [number, FxParamId, number] }
  | { SetFxBypass: [number, FxKindId, boolean] }
  | { SelectInstrument: NoteTarget }
  | { NoteOn: [number, number] }
  | { NoteOff: number }
  | { PitchBend: number }
  | { Modulation: number }
  /** GO LIVE: the slot's own input (its capture channel, `EngineHost.setSlotInputChannel`) feeds it; both
   * slots may be live at once. */
  | { SetSlotLive: [number, boolean] }
  /** A slot's output level (linear, 0..), heard and recorded; an empty live slot's is its input's level. */
  | { SetSlotGain: [number, number] }
  /** A built-in instrument's output level (linear, 0.., default 1), smoothed, heard and recorded, whether
   * or not it is the note target. */
  | { SetInstrumentGain: [InstrumentId, number] }
  | { SetInputSend: [InputSendId, boolean] }
  | { SetInputSendParam: [InputSendParamId, number] };

/** Rust `engine_io::DeviceRequest`: what `engine_open` opens (or switches to). The capture channels are
 * each plugin slot's (`inputChannels`), or one for both (`inputChannel`); 0-based, null = auto (input 2
 * on a device with two or more). */
export type DeviceRequest = {
  backend: AudioBackend;
  /** WASAPI capture / render endpoint id; null = the default. ASIO ignores both (the cached driver). */
  input: string | null;
  output: string | null;
  /** Frames per device callback; null = the driver's default. */
  buffer: number | null;
  /** The engine's rate, 44100 or 48000; null = the device's own. ASIO runs it when the driver can, else
   * its own; WASAPI runs the endpoint's own (`DeviceStatus.sampleRate` says what runs). A request
   * without it asks for the device's own. */
  sampleRate: number | null;
} & ({ inputChannels: [number | null, number | null] } | { inputChannel: number | null });

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
  /** False when WASAPI opened output only (no capture device, or its stream failed): the engine's
   * input is silence. Always true on ASIO. */
  inputOpen: boolean;
  /** The capture channel each plugin slot reads (0-based): its pick, or auto (input 2 on a device with
   * two or more) where it has none or the device lacks it. The native engine always sends it; a
   * scripted probe frame may leave it out. */
  inputChannels?: [number, number];
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
  /** A pending stop: at the loop end (END STOP), or where a fade ends. */
  stopAt: Frame | null;
  /** FADE: the lane fades out and stops at `stopAt`. */
  fading: boolean;
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
  /** COPY into `to` is done; `feedback` is the DUB FEEDBACK the engine copied (the source's may have moved
   * since). */
  | { type: 'Copied'; frame: Frame; from: number; to: number; feedback: number }
  /** The engine cleared the lane (CLEAR, a pedal's confirmed CLEAR, every lane on CLEAR ALL): its volume,
   * mute and FX are back to their defaults. Before the lane's Lane event in the same frame. */
  | { type: 'Cleared'; frame: Frame; lane: number }
  /** The engine's MUTE toggle (a pedal's, the MUTE button's) switched the lane's mute; the lane's `Mix`
   * follows, and the UI reads the mute from that. */
  | { type: 'Muted'; frame: Frame; lane: number; on: boolean }
  /** The lane's mix as the engine applied it, sent when it differs from the last one delivered (each
   * lane once from a new engine); a reset frame carries the last one the host read, per lane. The FX params
   * crossed as f32, so a value sent with more than seven significant digits comes back rounded. */
  | { type: 'Mix'; frame: Frame; lane: number; mix: LaneMix };

/** Rust `engine_io::DeviceEvent`, decoded to a `type`-tagged union. */
export type DeviceEvent =
  | { type: 'Lost'; backend: AudioBackend; reason: string }
  | { type: 'Recovered'; status: DeviceStatus }
  | { type: 'Fallback'; status: DeviceStatus }
  | { type: 'ShareLost'; reason: string }
  | { type: 'EngineFaulted' }
  /** A loss's fallback rebuilt the engine at `to` Hz, not its `from`, whether or not that device then
   * started: the loops left with the old engine. `device` is the lost one. */
  | { type: 'LoopsDropped'; device: string; from: number; to: number };

/**
 * Rust `engine_io::OpenError`: why `engine_open` did not open. A refusal: `device` runs at `to` Hz while
 * the engine, at `from` Hz, holds audio, so a switch would drop the loops (open again with `force` once the
 * player confirms). A failure: the open's text.
 */
export type OpenError = { type: 'RateChange'; device: string; from: number; to: number } | { type: 'Failed'; text: string };

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
  /** Reset frames only: the settings the engine remembers, in replay order, as the UI sent them; a
   * lane's mix as the engine last applied it (its `Mix`, also among the reset's events), or as sent
   * while no engine has reported the lane yet. A setting missing from it is at the engine's default. */
  settings?: EngineCommand[];
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

/** An `Action`: a unit action's name, or `{"Hold": control}` / `{"Release": control}` (a u8). */
function decodeAction(v: unknown, what: string): EngineAction {
  const [name, control] = tagged(v, what);
  if (name === 'Hold' || name === 'Release') int(control, `${what}.${name}`, 0, 255);
  else if (control !== undefined) fail(`${what} ${name} is a unit variant`, v);
  else oneOf(name, ACTIONS, what);
  return v as EngineAction;
}
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
    fading: bool(o.fading, 'LaneInfo.fading'),
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
      return {
        type: 'Copied',
        frame: at,
        from: lane(o.from, 'Copied.from'),
        to: lane(o.to, 'Copied.to'),
        feedback: num(o.feedback, 'Copied.feedback'),
      };
    case 'Cleared':
      return { type: 'Cleared', frame: at, lane: lane(o.lane, 'Cleared.lane') };
    case 'Muted':
      return { type: 'Muted', frame: at, lane: lane(o.lane, 'Muted.lane'), on: bool(o.on, 'Muted.on') };
    case 'Mix':
      return { type: 'Mix', frame: at, lane: lane(o.lane, 'Mix.lane'), mix: decodeLaneMix(o.mix, 'Mix.mix') };
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
    inputOpen: bool(o.inputOpen, 'DeviceStatus.inputOpen'),
    ...(o.inputChannels === undefined ? {} : { inputChannels: slotChannels(o.inputChannels) }),
  };
}

function slotChannels(v: unknown): [number, number] {
  const [a, b] = array(v, 'DeviceStatus.inputChannels', ENGINE_SLOTS);
  return [int(a, 'DeviceStatus.inputChannels[0]'), int(b, 'DeviceStatus.inputChannels[1]')];
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
    case 'LoopsDropped': {
      const o = obj(payload, 'DeviceEvent.LoopsDropped');
      return { type: 'LoopsDropped', ...rateChange(o, 'LoopsDropped') };
    }
    default:
      return fail('unknown DeviceEvent variant', raw);
  }
}

function rateChange(o: Obj, what: string): { device: string; from: number; to: number } {
  return { device: str(o.device, `${what}.device`), from: int(o.from, `${what}.from`, 1), to: int(o.to, `${what}.to`, 1) };
}

/** What an `engine_open` rejection holds: a refusal object, or a failure's text (anything else is read as
 * its text, so an IPC error never masks as a refusal). */
export function decodeOpenError(raw: unknown): OpenError {
  if (typeof raw === 'object' && raw !== null && 'RateChange' in raw) {
    return { type: 'RateChange', ...rateChange(obj((raw as Obj).RateChange, 'OpenError.RateChange'), 'RateChange') };
  }
  return { type: 'Failed', text: raw instanceof Error ? raw.message : String(raw) };
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
  if (o.settings !== undefined) out.settings = array(o.settings, 'feed.settings').map(decodeCommand);
  return out;
}

// ── Command and request validators (the fixture guard; the UI builds these values typed) ────────────

const UNIT_COMMANDS = ['PlayAll', 'StopAll', 'ClearAll', 'AllNotesOff', 'Press'] as const;
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
        decodeAction(p, 'Action');
        break;
      case 'ActionOn': {
        const [l, a] = pair('(lane, action)');
        lane(l, 'ActionOn.lane');
        decodeAction(a, 'ActionOn.action');
        break;
      }
      case 'SetVolume': {
        const [l, v] = pair('(lane, volume)');
        lane(l, 'SetVolume.lane');
        num(v, 'SetVolume.volume');
        break;
      }
      case 'Trim': {
        const [l, bars] = pair('(lane, bars)');
        lane(l, 'Trim.lane');
        int(bars, 'Trim.bars', 1);
        break;
      }
      case 'SetFadeBars':
        int(p, 'SetFadeBars', 1);
        break;
      case 'SetDubFeedback': {
        const [l, v] = pair('(lane, feedback)');
        lane(l, 'SetDubFeedback.lane');
        num(v, 'SetDubFeedback.feedback');
        break;
      }
      case 'SetPan': {
        const [l, v] = pair('(lane, pan)');
        lane(l, 'SetPan.lane');
        num(v, 'SetPan.pan');
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
        else if (target !== 'Off' || v !== undefined) fail('unknown NoteTarget variant', p);
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
      case 'SetInstrumentGain': {
        const [i, v] = pair('(instrument, gain)');
        oneOf(i, INSTRUMENTS, 'SetInstrumentGain.instrument');
        num(v, 'SetInstrumentGain.gain');
        break;
      }
      case 'SetInputSend': {
        const [s, v] = pair('(send, on)');
        oneOf(s, INPUT_SENDS, 'SetInputSend.send');
        bool(v, 'SetInputSend.on');
        break;
      }
      case 'SetInputSendParam': {
        const [k, v] = pair('(param, value)');
        oneOf(k, INPUT_SEND_PARAMS, 'SetInputSendParam.param');
        num(v, 'SetInputSendParam.value');
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
  const base = {
    backend: oneOf(o.backend, BACKENDS, 'DeviceRequest.backend'),
    input: nullable(o.input, (v) => str(v, 'DeviceRequest.input')),
    output: nullable(o.output, (v) => str(v, 'DeviceRequest.output')),
    buffer: nullable(o.buffer, (v) => int(v, 'DeviceRequest.buffer', 1)),
    // Absent (a request from before the pick): the device's own rate, as the Rust side reads it.
    sampleRate: o.sampleRate === undefined ? null : nullable(o.sampleRate, (v) => int(v, 'DeviceRequest.sampleRate', 1)),
  };
  const channel = (v: unknown, what: string) => nullable(v, (c) => int(c, what));
  if (o.inputChannels === undefined) return { ...base, inputChannel: channel(o.inputChannel, 'DeviceRequest.inputChannel') };
  const [a, b] = array(o.inputChannels, 'DeviceRequest.inputChannels', ENGINE_SLOTS);
  return { ...base, inputChannels: [channel(a, 'DeviceRequest.inputChannels[0]'), channel(b, 'DeviceRequest.inputChannels[1]')] };
}

// ── Session bytes (`engine_snapshot` / `engine_load_session`) ───────────────────────────────────────
//
// `[u32 LE headerLen][headerLen bytes of UTF-8 JSON][f32 LE mono PCM per header track, in header order]`,
// no padding; each block holds `frames` samples. The PCM is in PLAY order (what is heard from loop
// position 0); `reversed` says the lane plays its recording backwards. A snapshot's track carries its
// lane's `mix` as the engine applied it where the snapshot pinned the loops, in session.json's track
// shape; a load's track carries the mix the engine sets on the frame the loops go in (required). A snapshot asked with the master (an export's) whose render succeeded names it in `master` and
// appends its left block, then its right, `master.frames` samples each, frame 0 at loop position 0,
// rendered with those mixes; one whose render failed carries `masterError` instead
// (`src-tauri/src/engine_io/session.rs` owns the layout).
// PCM moves as whole typed-array copies in the platform's byte order: little-endian on every target.

/** One effect of a lane's mix: session.json's FX shape (`FxState`, `src/ui/state/fx-metadata.ts`). */
export interface WireFxState {
  bypassed: boolean;
  params: Record<string, number>;
}

/** A lane's mix as the engine applied it (lf-engine's `LaneMix`): `fx` the five effects in chain order. */
export interface LaneMix {
  volume: number;
  muted: boolean;
  dubFeedback: number;
  /** -1 (hard left) to 1 (hard right). Absent: the centre, 0 (the engine writes it only off the centre,
   * and reads a mix without it as centred). */
  pan?: number;
  fx: WireFxState[];
}

/** One lane in a snapshot: committed lanes only. */
export interface SnapshotTrack {
  index: number;
  frames: number;
  reversed: boolean;
  state: 'Playing' | 'Stopped' | 'Overdubbing';
  /** Its mix on the frame the snapshot pinned the loops (not necessarily when it was asked for). */
  mix: LaneMix;
}

/** `engine_snapshot`'s header. */
export interface SnapshotHeader {
  rate: number;
  masterLengthFrames: number;
  bpm: number;
  tracks: SnapshotTrack[];
  /** The wet master's two blocks follow the tracks' (a snapshot asked with the master). */
  master?: { frames: number };
  /** Why a snapshot asked with the master carries none: the render's error. */
  masterError?: string;
}

/** One lane in a load. */
export interface LoadTrack {
  index: number;
  frames: number;
  reversed: boolean;
  state: 'Playing' | 'Stopped';
  /** The lane's mix from the load's frame: its first loaded sample plays at it. */
  mix: LaneMix;
}

/** `engine_load_session`'s header: into an all-empty engine at the device's rate. */
export interface LoadHeader {
  bpm: number;
  bars: number;
  masterLengthFrames: number;
  tracks: LoadTrack[];
}

/** A stereo master's two channels. */
export interface StereoPcm {
  left: Float32Array;
  right: Float32Array;
}

/** Pack a header and its PCM blocks (one per header track, in order), and a snapshot header's master. */
export function encodeSessionBytes(
  header: LoadHeader | SnapshotHeader,
  pcm: readonly Float32Array[],
  master?: StereoPcm,
): Uint8Array<ArrayBuffer> {
  if (pcm.length !== header.tracks.length) fail('one PCM block per header track', { tracks: header.tracks.length, blocks: pcm.length });
  const masterFrames = 'master' in header && header.master ? header.master.frames : null;
  if ((masterFrames === null) !== (master === undefined)) fail('a master block pair exactly when the header names one', masterFrames);
  const blocks = master ? [...pcm, master.left, master.right] : pcm;
  const json = new TextEncoder().encode(JSON.stringify(header));
  const samples = blocks.reduce((n, block, k) => {
    const want = k < header.tracks.length ? header.tracks[k].frames : masterFrames;
    if (block.length !== want) fail(`session block ${k} holds ${block.length} samples`, want);
    return n + block.length;
  }, 0);
  const bytes = new Uint8Array(4 + json.length + samples * 4);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, json.length, true);
  bytes.set(json, 4);
  let at = 4 + json.length;
  for (const block of blocks) {
    bytes.set(new Uint8Array(block.buffer, block.byteOffset, block.byteLength), at);
    at += block.byteLength;
  }
  return bytes;
}

/** Split session bytes into their JSON header, one PCM block per header track, and the master's two
 * blocks when the header names one (null otherwise). */
export function splitSessionBytes(buffer: ArrayBuffer): { header: unknown; pcm: Float32Array[]; master: StereoPcm | null } {
  const view = new DataView(buffer);
  if (buffer.byteLength < 4) fail('session bytes too short for a header length', buffer.byteLength);
  const headerLen = view.getUint32(0, true);
  if (4 + headerLen > buffer.byteLength) fail('session header runs past the bytes', { headerLen, bytes: buffer.byteLength });
  const header: unknown = JSON.parse(new TextDecoder().decode(new Uint8Array(buffer, 4, headerLen)));
  const o = obj(header, 'session header');
  const tracks = array(o.tracks, 'session header.tracks');
  let at = 4 + headerLen;
  const block = (frames: number) => {
    if (at + frames * 4 > buffer.byteLength) fail('session PCM runs past the bytes', { at, frames, bytes: buffer.byteLength });
    const out = new Float32Array(buffer.slice(at, at + frames * 4));
    at += frames * 4;
    return out;
  };
  const pcm = tracks.map((t) => block(int(obj(t, 'session track').frames, 'session track.frames')));
  let master: StereoPcm | null = null;
  if (o.master !== undefined) {
    const frames = int(obj(o.master, 'session master').frames, 'session master.frames');
    master = { left: block(frames), right: block(frames) };
  }
  if (at !== buffer.byteLength) fail('session bytes carry more than their header lists', { end: at, bytes: buffer.byteLength });
  return { header, pcm, master };
}

/** A lane's mix (a snapshot's or a load's track's, a `Mix` event's): exact fields, `pan` optional (absent
 * is the centre and stays absent), each effect's params by their keys (their ranges: `validateFxStates`,
 * where the UI takes it). */
function decodeLaneMix(raw: unknown, what: string): LaneMix {
  const o = obj(raw, what);
  const extra = Object.keys(o).filter((k) => !['volume', 'muted', 'dubFeedback', 'pan', 'fx'].includes(k));
  if (extra.length > 0) fail(`${what} has unknown fields`, extra);
  const fx = array(o.fx, `${what}.fx`, FX_KINDS.length).map((e, k): WireFxState => {
    const entry = obj(e, `${what}.fx[${k}]`);
    const params = obj(entry.params, `${what}.fx[${k}].params`);
    const keys = FX_KIND_PARAMS[k];
    if (Object.keys(params).length !== keys.length) fail(`${what}.fx[${k}].params must be ${keys.join(', ')}`, params);
    const out: Record<string, number> = {};
    for (const key of keys) out[key] = num(params[key], `${what}.fx[${k}].params.${key}`);
    return { bypassed: bool(entry.bypassed, `${what}.fx[${k}].bypassed`), params: out };
  });
  return {
    volume: num(o.volume, `${what}.volume`),
    muted: bool(o.muted, `${what}.muted`),
    dubFeedback: num(o.dubFeedback, `${what}.dubFeedback`),
    ...(o.pan === undefined ? {} : { pan: num(o.pan, `${what}.pan`) }),
    fx,
  };
}

/** Read `engine_load_session`'s bytes as the engine host does (every track one master long with its
 * mix, at most one track per lane), refusing a field it would ignore. */
export function decodeLoadSession(buffer: ArrayBuffer): { header: LoadHeader; pcm: Float32Array[] } {
  const { header, pcm, master } = splitSessionBytes(buffer);
  if (master) fail('a load carries no master', header);
  const o = obj(header, 'load header');
  const extra = Object.keys(o).filter((k) => !['bpm', 'bars', 'masterLengthFrames', 'tracks'].includes(k));
  if (extra.length > 0) fail('load header has unknown fields', extra);
  const length = int(o.masterLengthFrames, 'load.masterLengthFrames', 1);
  const seen = new Set<number>();
  const tracks = array(o.tracks, 'load.tracks').map((raw): LoadTrack => {
    const t = obj(raw, 'load track');
    const unknown = Object.keys(t).filter((k) => !['index', 'frames', 'reversed', 'state', 'mix'].includes(k));
    if (unknown.length > 0) fail('load track has unknown fields', unknown);
    const track: LoadTrack = {
      index: lane(t.index, 'load track.index'),
      frames: int(t.frames, 'load track.frames'),
      reversed: bool(t.reversed, 'load track.reversed'),
      state: oneOf(t.state, ['Playing', 'Stopped'] as const, 'load track.state'),
      mix: decodeLaneMix(t.mix, 'load track.mix'),
    };
    if (track.frames !== length) fail(`load track ${track.index} is not one master long`, track.frames);
    if (seen.has(track.index)) fail(`load track ${track.index} is listed twice`, track.index);
    seen.add(track.index);
    return track;
  });
  return { header: { bpm: int(o.bpm, 'load.bpm', 1), bars: int(o.bars, 'load.bars', 1), masterLengthFrames: length, tracks }, pcm };
}

/** Read `engine_snapshot`'s bytes: the stems, and the wet master when the snapshot carries one. */
export function decodeSnapshot(buffer: ArrayBuffer): { header: SnapshotHeader; pcm: Float32Array[]; master: StereoPcm | null } {
  const { header, pcm, master } = splitSessionBytes(buffer);
  const o = obj(header, 'snapshot header');
  const length = int(o.masterLengthFrames, 'snapshot.masterLengthFrames');
  const tracks = array(o.tracks, 'snapshot.tracks').map((raw): SnapshotTrack => {
    const t = obj(raw, 'snapshot track');
    const track: SnapshotTrack = {
      index: lane(t.index, 'snapshot track.index'),
      frames: int(t.frames, 'snapshot track.frames'),
      reversed: bool(t.reversed, 'snapshot track.reversed'),
      state: oneOf(t.state, ['Playing', 'Stopped', 'Overdubbing'] as const, 'snapshot track.state'),
      mix: decodeLaneMix(t.mix, 'snapshot track.mix'),
    };
    if (track.frames !== length) fail(`snapshot track ${track.index} is not one master long`, track.frames);
    return track;
  });
  const decoded: SnapshotHeader = { rate: int(o.rate, 'snapshot.rate', 1), masterLengthFrames: length, bpm: int(o.bpm, 'snapshot.bpm', 1), tracks };
  if (master) {
    if (master.left.length !== length) fail('the snapshot master is not one master long', master.left.length);
    decoded.master = { frames: length };
  }
  if (o.masterError !== undefined) {
    if (master) fail('a snapshot carries a master and its error', o.masterError);
    decoded.masterError = str(o.masterError, 'snapshot.masterError');
  }
  return { header: decoded, pcm, master };
}
