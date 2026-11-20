/**
 * The browser fake's chain per slot (`docs/unit-contract.md` §10): what the engine and its chain host will
 * answer, modelled, never heard. The UI builds the chain (plates, `+`, the OTT face) against it before the
 * native side speaks the chain. `host.web.ts` wires it: its `ChainHost` is the platform's `chain`, its batch
 * commands arrive through the fake's `applyCommand`, its frames go to the fake's feed subscribers, and a
 * probe reaches its seams as `__lf.native.chain`.
 *
 * What it answers, as the contract says the engine does:
 * - structure (§2, §3, §4): a chain per slot of at most `MAX_UNITS`, one call at a time per slot; an id
 *   reserved across both slots from `add` until its removal or a failed add; each change answered by a
 *   `chains` frame, an add through `loading` first; a source lands at the head, replacing a seated source
 *   in one install (into a full chain too: the install shows beside the eight while it loads); an effect
 *   never goes ahead of a source; a failed load leaves a `failed` plate in its place, its id free; an edit
 *   that moves a chain's latency sum or its role waits (`waiting`, `held`) while a take holds the
 *   alignment, the slot's later edits behind it, each judged again when it applies, each call settling on
 *   its outcome; a reload keeps the old seat applied until its replacement lands;
 * - D29 by unit (§1), tones by §8 (today's tone file, read and identity-checked; a parked token bound to its
 *   plugin; an unmatched tone or a removed unit's kept as the slot-and-plugin file a new unit starts from);
 * - the batch (§2, §6): `SetUnitBypass`, `ToggleUnit`, `Toggle.Units` (answered by one `Toggled`) and
 *   `SetUnitParam` (clamped as the unit clamps it) switch the model, answered by `chains`; a command naming
 *   an id that is not seated, or a param of another kind, is dropped and counted (`dropped`);
 * - the meters (§2): one per seated unit on every frame it makes; the OTT face's on the fake's own 60 Hz
 *   timer while an OTT is seated and on, from the accepted mockup's simulated guitar part through the same
 *   curve (`docs/redesign/ott.js`, SIM-BEGIN to SIM-END), following each OTT's four params;
 * - a reset frame carrying `chains` replaces the model and answers every call and held edit begun before it
 *   refused; one without carries the model; either restarts the OTTs' followers.

 * Pure of DOM and Solid: a Node guard can import it (no timer starts at module load).
 */
import type { ChainHost, PluginDescriptor, PluginParamDesc } from './host';
import {
  MAX_UNITS,
  type BandMeter,
  type ChainState,
  type EngineCommand,
  type EngineEvent,
  type FeedFrame,
  type PluginRef,
  type UnitEffectKindId,
  type UnitId,
  type UnitKindId,
  type UnitMeter,
  type UnitParamId,
  type UnitSpec,
  type UnitState,
  isUnitCommand,
} from './engine-wire.ts'; // explicit .ts: Node guards import this file

/** What the fake needs of the engine fake around it (`host.web.ts`). */
interface ChainFakeDeps {
  /** The chain fake is on: a DEV probe forced the engine fake and asked for the chain (`__lfChainFake`). */
  forced(): boolean;
  /** A take holds the alignment: a scripted lane records or overdubs (the engine judges a capture). */
  capturing(): boolean;
  /** Hand a frame the fake made to the feed's subscribers. */
  deliver(frame: FeedFrame): void;
  /** The input meter the scripted frames last carried, for the frames the fake makes. */
  meter(): FeedFrame['meter'];
}

/** A canned plugin for the picker (`webPluginHost.scanPlugins` under `__lfChainFake`) and the chain: its
 * descriptor and the latency it reports once activated. */
export interface CannedPlugin {
  descriptor: PluginDescriptor;
  latency: number;
}

/** The fake's canned plugins (unit contract §10): an effect, an instrument, one with latency. */
export const CANNED_PLUGINS: readonly CannedPlugin[] = [
  { descriptor: { id: 'fake.amp', name: 'Fake Amp', format: 'vst3', path: 'C:\\Fake\\FakeAmp.vst3', isEffect: true }, latency: 0 },
  { descriptor: { id: 'fake.synth', name: 'Fake Synth', format: 'clap', path: 'C:\\Fake\\FakeSynth.clap', isEffect: false }, latency: 0 },
  { descriptor: { id: 'fake.lookahead', name: 'Fake Lookahead', format: 'vst2', path: 'C:\\Fake\\FakeLookahead.dll', isEffect: true }, latency: 64 },
];

/** The fake's chain and what a probe reads and scripts through `__lf.native.chain`. */
export interface ChainFake {
  /** The platform's `chain`. */
  readonly host: ChainHost;
  /** Every `ChainHost` call, in the order made: its method's name and arguments. */
  readonly calls: [string, ...unknown[]][];
  /** Every batch command the model dropped: a unit not seated, a param of another kind. */
  readonly dropped: EngineCommand[];
  readonly plugins: readonly CannedPlugin[];
  /** Probe seam: while set, an add's or a reload's activation waits for it (the plate stays `loading`). */
  holdLoading: Promise<void> | null;
  /** Probe seam: a unit id's next activation (its add or a reload) fails with this reason. */
  readonly failures: Map<UnitId, string>;
  /** Probe seam: while set, a take holds the alignment, as a scripted recording lane does; cleared (and
   * no lane captures), the waiting edits apply in order. */
  capture: boolean;
  /** Probe seam: park a seated unit (`failed` with `reason`, in its place, its latency kept), as a restart
   * whose reinstall failed does. */
  park(unit: UnitId, reason: string): void;
  /** Probe seam: reseat a parked unit, as a restart that succeeds does. */
  reseat(unit: UnitId): void;
  /** The plugin units whose editor window is open (an `openEditor` it took, no close since). */
  readonly editorsOpen: ReadonlySet<UnitId>;
  /** Probe seam: the player closes `unit`'s editor window with its own close (the window event, by unit). */
  closeWindow(unit: UnitId): void;
  /** Probe seam: the meters' own 60 Hz timer (on by default); off, `meterTick` emits one frame. */
  meterTimer: boolean;
  /** Whether the 60 Hz interval is live now: only while the timer is on and an OTT is seated and on. */
  readonly meterTimerRunning: boolean;
  /** Emit one `unitMeters` frame now, one sixtieth of a second of the simulated part later. */
  meterTick(): void;
  /** Probe seam: what `mintUnitId` (`src/platform/index.ts`) answers while set. */
  mint: (() => number) | null;
  /** The model as the last `chains` frame told it. */
  chains(): [ChainState, ChainState];
}

type Slot = 0 | 1;
type BuiltinKind = Exclude<UnitKindId, 'plugin'>;
/** A param's range and default (`lf_engine::api` `InputSendParam::range`); `index`: rounded to an index
 * (`dsp::fx::clamp_index`). */
type Range = { min: number; max: number; def: number; index?: boolean };

/** SYNTH's level ceiling (`SLOT_GAIN_MAX`, `src/ui/state/instrument.ts`, which this layer may not import). */
const SYNTH_GAIN_MAX = 1.5;
const synthGain: Range = { min: 0, max: SYNTH_GAIN_MAX, def: 1 };

/** Each built-in's params, as the engine's ranges have them today (`api.rs` `InputSendParam::range`). */
const PARAMS: Readonly<Record<BuiltinKind, Partial<Record<UnitParamId, Range>>>> = {
  ott: {
    ottDepth: { min: 0, max: 1, def: 0.5 },
    ottTime: { min: 0, max: 1, def: 0.5 },
    ottUpward: { min: 0, max: 1, def: 1 },
    ottDownward: { min: 0, max: 1, def: 1 },
  },
  echo: {
    echoTime: { min: 0, max: 3, def: 1, index: true },
    echoFeedback: { min: 0, max: 0.95, def: 0.4 },
    echoLevel: { min: 0, max: 1, def: 0.5 },
  },
  reverb: { reverbLevel: { min: 0, max: 1, def: 0.5 } },
  ring: { ringFreq: { min: 20, max: 1500, def: 440 }, ringLevel: { min: 0, max: 1, def: 0.5 } },
  // unit contract §2: `instrument` is the index in the UI's `SYNTHS` order; out of range is clamped to it
  // (the contract is silent; the reading closest to `clamp_index`).
  synth: {
    instrument: { min: 0, max: 5, def: 0, index: true },
    gainLead: synthGain,
    gainBass: synthGain,
    gainPad: synthGain,
    gainPiano: synthGain,
    gainOrgan: synthGain,
    gainDrum: synthGain,
  },
};

const NAMES: Readonly<Record<BuiltinKind, string>> = { ott: 'OTT', echo: 'ECHO', reverb: 'REVERB', ring: 'RING MOD', synth: 'SYNTH' };
/** SYNTH sounds a note one render quantum late and reports it as its latency (unit contract §4). */
const SYNTH_LATENCY = 128;

const clampParam = (r: Range, v: number): number => {
  const x = Math.min(r.max, Math.max(r.min, v));
  return r.index ? Math.min(r.max, Math.max(r.min, Math.floor(v + 0.5))) : x;
};

const NO_ENGINE = 'The native engine is unavailable in the browser build.';
const sameRef = (a: PluginRef, b: PluginRef): boolean => a.format === b.format && a.path === b.path && a.id === b.id;
const canned = (ref: PluginRef): CannedPlugin | undefined => CANNED_PLUGINS.find((p) => sameRef(p.descriptor, ref));
const baseName = (path: string): string => path.split(/[\\/]/).pop() ?? path;

// ── The OTT face's signal: the mockup's simulated part (`docs/redesign/ott.js`, SIM-BEGIN to SIM-END) ──

/** One band of the simulated guitar part and of its three-band upward/downward compressor. */
interface SimBand {
  step: number;
  pat: readonly number[];
  ex?: readonly number[];
  A: number;
  acc: number;
  fall: number;
  ft: number;
  slope: number;
  floor: number;
  seed: number;
  tu: number;
  td: number;
  att: number;
  rel: number;
}
const hash = (n: number): number => {
  const x = Math.sin(n * 127.1 + 311.7) * 43758.5453;
  return x - Math.floor(x);
};
const SIM_BANDS: readonly SimBand[] = [
  { step: 2, pat: [1, 0], ex: [1.62], A: -17, acc: 0, fall: 20, ft: 0.3, slope: 3.2, floor: -41, seed: 1, tu: -30, td: -12, att: 0.06, rel: 0.32 },
  { step: 0.25, pat: [1, 1, 0, 1, 1, 0, 1, 1], A: -13, acc: 1.5, fall: 4, ft: 0.06, slope: 6, floor: -40, seed: 2, tu: -32, td: -24, att: 0.045, rel: 0.17 },
  { step: 0.25, pat: [1, 0, 0, 1, 0, 0, 1, 0], A: -20, acc: 1, fall: 22, ft: 0.085, slope: 5, floor: -42, seed: 3, tu: -32, td: -14, att: 0.03, rel: 0.07 },
];
/** A band's level (dBFS) at `t` seconds into the part: the mockup's `level`. */
function simLevel(b: SimBand, t: number): number {
  let L = b.floor + 0.8 * Math.sin(t * 1.3 + b.seed) + 0.5 * Math.sin(t * 3.1 + b.seed * 2);
  const n1 = Math.floor(t / b.step);
  const on: [number, number, number][] = [];
  for (let n = Math.max(0, n1 - 40); n <= n1; n++) if (b.pat[n % b.pat.length]) on.push([n * b.step + (hash(n + b.seed * 97) - 0.5) * 0.014, n, 0]);
  if (b.ex)
    for (let k = Math.max(0, Math.floor(t / (b.step * b.pat.length)) - 3); k * b.step * b.pat.length <= t; k++)
      for (const o of b.ex) on.push([k * b.step * b.pat.length + o, 1000 + k, -11]);
  for (const [th, n, off] of on) {
    if (t < th) continue;
    const d = t - th;
    const A = b.A + off + (n % 8 === 0 ? b.acc : 0) + (hash(n * 3 + b.seed) - 0.5) * 3;
    const onset = d < 0.012 ? 0.5 - 0.5 * Math.cos((Math.PI * d) / 0.012) : 1;
    L = Math.max(L, A - b.fall * (1 - Math.exp(-d / b.ft)) - b.slope * d - (1 - onset) * 40);
  }
  return L;
}
/** The mockup's control rate (samples a second) and the warm-up before the part's first frame. */
const SIM_FS = 400;
const SIM_WARM = 2 * SIM_FS;
const LEVEL_FLOOR = -120;
/** One OTT unit's simulation: the mockup's `run`, stepped a feed frame at a time. */
interface OttSim {
  sample: number;
  frame: number;
  g1: [number, number, number];
  g2: [number, number, number];
}
type OttParams = { depth: number; time: number; up: number; down: number };
/** Advance `sim` by one sample: each band's gain follows the mockup's curve; returns each band's level. */
function simStep(sim: OttSim, p: OttParams, levels: number[]): void {
  const tt = sim.sample / SIM_FS;
  const a = (tau: number) => 1 - Math.exp(-1 / (SIM_FS * tau));
  SIM_BANDS.forEach((b, i) => {
    const L = simLevel(b, tt);
    const rel = b.rel * (0.5 + p.time);
    const k = 2 * p.depth;
    const down = L > b.td ? -(L - b.td) * 0.6 : 0;
    const up = L < b.tu ? Math.min(14, (b.tu - L) * 0.7) * Math.min(1, Math.max(0, (L + 66) / 8)) : 0;
    const target = Math.max(-12, Math.min(12, k * (p.up * up + p.down * down)));
    sim.g1[i] += (target - sim.g1[i]) * a(target < sim.g1[i] ? b.att : rel);
    sim.g2[i] += (sim.g1[i] - sim.g2[i]) * a(sim.g1[i] < sim.g2[i] ? b.att : rel);
    levels[i] = L;
  });
  sim.sample++;
}
function newOttSim(p: OttParams): OttSim {
  const sim: OttSim = { sample: -SIM_WARM, frame: 0, g1: [0, 0, 0], g2: [0, 0, 0] };
  const levels = [0, 0, 0];
  while (sim.sample < 0) simStep(sim, p, levels);
  return sim;
}
/** One feed frame (1/60 s) of `sim`: each band's gain now and its range, its level now and its peak. */
function simFrame(sim: OttSim, p: OttParams, unit: UnitId): UnitMeter {
  sim.frame++;
  const end = Math.floor((sim.frame * SIM_FS) / 60);
  const bands = [0, 1, 2].map((): BandMeter => ({ gain: 0, gainMin: Infinity, gainMax: -Infinity, level: LEVEL_FLOOR, levelPeak: LEVEL_FLOOR }));
  const levels = [0, 0, 0];
  let out = 0;
  while (sim.sample < end) {
    simStep(sim, p, levels);
    let sum = 0;
    bands.forEach((m, i) => {
      const g = sim.g2[i];
      const L = Math.max(LEVEL_FLOOR, levels[i]);
      m.gain = g;
      m.gainMin = Math.min(m.gainMin, g);
      m.gainMax = Math.max(m.gainMax, g);
      m.level = L;
      m.levelPeak = Math.max(m.levelPeak, L);
      sum += 10 ** ((levels[i] + g) / 20);
    });
    out = Math.max(out, sum);
  }
  return { unit, out: [out, out], bands: bands as [BandMeter, BandMeter, BandMeter] };
}

// ── Tones (unit contract §1, §8): today's file, `src-tauri/src/host/tone.rs` `encode`/`decode` ─────────

const TONE_MAGIC = [0x42, 0x4c, 0x54, 0x4e]; // "BLTN"
const TONE_VERSION = 1;
const TONE_FIELD_MAX = 4096;
/** FNV-1a 64, as `tone.rs` sums a tone file. */
function fnv1a64(bytes: Uint8Array): bigint {
  let h = 0xcbf29ce484222325n;
  for (const b of bytes) h = BigInt.asUintN(64, (h ^ BigInt(b)) * 0x100000001b3n);
  return h;
}
/** A tone file's identity, name and state, or why it cannot be read (`tone.rs` `decode`'s refusals). */
function decodeTone(bytes: Uint8Array): { plugin: { format: string; path: string; id: string }; name: string; state: Uint8Array } | { error: string } {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let at = 0;
  const take = (n: number, what: string): Uint8Array => {
    if (at + n > bytes.length) throw new Error(`truncated: ${what} needs ${n} bytes at ${at}`);
    const out = bytes.subarray(at, at + n);
    at += n;
    return out;
  };
  const sized = (max: number, what: string): Uint8Array => {
    const len = view.getUint32(take(4, what).byteOffset - bytes.byteOffset, true);
    if (len > max) throw new Error(`${what} is ${len} bytes, more than ${max}`);
    return take(len, what);
  };
  const text = (what: string): string => new TextDecoder('utf-8', { fatal: true }).decode(sized(TONE_FIELD_MAX, what));
  try {
    if (take(4, 'magic').some((b, i) => b !== TONE_MAGIC[i])) return { error: 'not a BleepLoop tone' };
    const version = view.getUint16(take(2, 'version').byteOffset - bytes.byteOffset, true);
    if (version !== TONE_VERSION) return { error: `tone version ${version}; this build reads ${TONE_VERSION}` };
    const [format, path, id, name] = [text('format'), text('path'), text('id'), text('name')];
    const state = sized(0xffff_ffff, 'state').slice();
    const body = at;
    const sum = view.getBigUint64(take(8, 'checksum').byteOffset - bytes.byteOffset, true);
    if (at !== bytes.length) return { error: `${bytes.length - at} bytes after the checksum` };
    if (sum !== fnv1a64(bytes.subarray(0, body))) return { error: 'checksum mismatch: the file is corrupt' };
    return { plugin: { format, path, id }, name, state };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
}
/** A tone file for `plugin` holding `state` (`tone.rs` `encode`). */
function encodeTone(plugin: PluginRef, name: string, state: Uint8Array): Uint8Array {
  const parts: number[] = [...TONE_MAGIC, TONE_VERSION & 0xff, TONE_VERSION >> 8];
  const put = (b: Uint8Array) => {
    parts.push(b.length & 0xff, (b.length >> 8) & 0xff, (b.length >> 16) & 0xff, (b.length >>> 24) & 0xff, ...b);
  };
  for (const field of [plugin.format, plugin.path, plugin.id, name]) put(new TextEncoder().encode(field));
  put(state);
  const sum = fnv1a64(Uint8Array.from(parts));
  for (let k = 0n; k < 8n; k++) parts.push(Number((sum >> (8n * k)) & 0xffn));
  return Uint8Array.from(parts);
}
const refName = (p: { format: string; path: string; id: string }): string => `${p.format} ${p.path} ${p.id}`;

// ── The model ────────────────────────────────────────────────────────────────────────────────────────

/** A unit as the model holds it: its state on the wire, its slot and the `chains` frame that first told it. */
interface Unit {
  state: UnitState;
  slot: Slot;
  addedSeq: number;
  /** A failed add's plate (unit contract §2 `failed`: no seat): in its place, its id free (§1). */
  noSeat?: true;
  /** A reload in flight: the plate reads `loading` (then `waiting`), the old instance plays on, applied. */
  reloading?: true;
  /** The plugin instance it runs (`inst`), for D29's affinity; the state its tone loaded, if any. */
  inst: number;
  tone: Uint8Array | null;
}
/** An edit held while a take holds the alignment: applied in order once it ends. */
interface Held {
  unit: UnitId;
  kind: 'add' | 'remove' | 'move' | 'reload';
  /** Apply it, revalidated against the chain as it is then (unit contract §7: the engine judges it there). */
  apply(): void;
  /** Settle it refused: the chain it waited in was replaced (a reset frame carrying `chains`). */
  drop(reason: string): void;
  /** A later removal of its unit withdraws it (an add leaves without a trace; a reload is not taken). */
  cancel?(): void;
}
/** A call's answer that settles later than its turn on the slot (a held edit): the slot's next call
 * queues behind it as the engine's port does (§7), its caller waits for the outcome. */
class Later<T> {
  readonly done: Promise<T>;
  constructor(done: Promise<T>) {
    this.done = done;
  }
}
function deferred<T>(): { promise: Promise<T>; resolve: (v: T) => void; reject: (e: Error) => void } {
  let resolve!: (v: T) => void;
  let reject!: (e: Error) => void;
  const promise = new Promise<T>((res, rej) => ((resolve = res), (reject = rej)));
  return { promise, resolve, reject };
}
/** D29's entry for a plugin file (`src/ui/state/instrument.ts` `editorAffinity`, by unit). */
interface Affinity {
  inst: number;
  unit: UnitId;
  state: 'owned' | 'orphaned' | 'unordered';
}

/** The chain fake as `host.web.ts` drives it: the probe seams plus the engine fake's hooks. */
interface WiredChainFake extends ChainFake {
  /** Apply a batch command the chain owns; false when it is not one. */
  command(c: EngineCommand): boolean;
  /** A scripted frame, before the subscribers get it. */
  scripted(f: FeedFrame): void;
  /** Judge the held edits again (a scripted frame may have ended the take). */
  judge(): void;
}

const RESET = 'the chain was reset before this answered';

export function createChainFake(deps: ChainFakeDeps): WiredChainFake {
  const chains: [Unit[], Unit[]] = [[], []];
  /** Every reserved id and its slot: from `add` until its removal or a failed add (unit contract §1). */
  const reserved = new Map<UnitId, Slot>();
  const held: [Held[], Held[]] = [[], []];
  /** One structure call at a time per slot (unit contract §2). */
  const tails: [Promise<unknown>, Promise<unknown>] = [Promise.resolve(), Promise.resolve()];
  const sims = new Map<UnitId, OttSim>();
  /** The last reading of each active OTT: a `chains` frame repeats it, no time passing. */
  const lastBands = new Map<UnitId, UnitMeter>();
  /** Parked session tones by token (unit contract §8), each bound to its plugin and slot. */
  const parked = new Map<number, { plugin: PluginRef; slot: Slot; state: Uint8Array }>();
  /** The slot-and-plugin files (unit contract §1): a removed unit's tone, an import's unmatched one. */
  const slotTones = new Map<string, Uint8Array>();
  const affinity = new Map<string, Affinity>();
  /** The instances that hold a plugin file (the module stays loaded), and those still loading. */
  const holders = new Map<string, Set<number>>();
  const loading = new Set<number>();
  let instances = 0;
  let toneTokens = 0;
  let seq = 0;
  /** Bumped by a reset frame carrying `chains`: a call begun before it answers refused, never seated. */
  let gen = 0;
  let pendingEvents: EngineEvent[] = [];
  let scheduled = false;
  let timer: ReturnType<typeof setInterval> | null = null;
  let captureSeam = false;
  /** The editor windows open, by unit, and who hears one close (`ChainHost.onEditorClosed`). */
  const editorsOpen = new Set<UnitId>();
  const editorClosed = new Set<(unit: UnitId) => void>();
  /** A unit's editor window closed (its own close, its unit withdrawn or removed): told a microtask later. */
  function windowClosed(unit: UnitId): void {
    if (!editorsOpen.delete(unit)) return;
    queueMicrotask(() => editorClosed.forEach((cb) => cb(unit)));
  }

  const all = (): Unit[] => [...chains[0], ...chains[1]];
  const find = (unit: UnitId): Unit | undefined => all().find((u) => u.state.id === unit && !u.noSeat);
  const tombstone = (unit: UnitId): Unit | undefined => all().find((u) => u.state.id === unit && u.noSeat);
  /** In the chain's sum and role: seated, leaving, parked (a parked seat keeps its latency, unit contract
   * §3) or reloading (the old instance plays until its replacement lands, §3). */
  const applied = (u: Unit) =>
    !u.noSeat && (u.reloading === true || u.state.state === 'seated' || u.state.state === 'leaving' || u.state.state === 'failed');
  /** Has a seat that sounds: what the batch reaches and the meters cover (unit contract §2 "seated"). */
  const sounding = (u: Unit) => !u.noSeat && (u.reloading === true || u.state.state === 'seated' || u.state.state === 'leaving');
  const capturing = () => captureSeam || deps.capturing();
  const toneKey = (slot: Slot, p: PluginRef) => `${slot}|${refName(p)}`;

  /** The chain as applied: its units in order, their latency sum (bypassed and parked seats included). */
  function chainState(slot: Slot): ChainState {
    // an edit of a unit that waits behind a take marks its plate (a held removal, a move queued behind one)
    const waits = new Set(held[slot].filter((h) => h.kind === 'remove' || h.kind === 'move').map((h) => h.unit));
    const units = chains[slot].map((u) => {
      const s = structuredClone(u.state);
      if (waits.has(s.id)) s.held = true;
      return s;
    });
    const latency = chains[slot].filter(applied).reduce((s, u) => s + u.state.latency, 0);
    return { units, latency, held: held[slot].length > 0 };
  }
  const snapshot = (): [ChainState, ChainState] => [chainState(0), chainState(1)];
  /** What moves a take's alignment (unit contract §7): the applied units' latency sum and whether a
   * source heads the chain. The fake does not model which chains are live. */
  const alignment = (states: readonly { latency: number; source: boolean }[]): string =>
    `${states.reduce((s, u) => s + u.latency, 0)}:${states[0]?.source === true}`;
  const appliedStates = (units: readonly Unit[]): UnitState[] => units.filter(applied).map((u) => u.state);
  /** The first other entry whose role is known: what an effect may not go ahead of. */
  const headOf = (units: readonly Unit[]): Unit | undefined => units.find(applied);

  function frame(extra: Partial<FeedFrame>): FeedFrame {
    return { seq: 0, reset: false, catchUp: false, events: [], device: [], anchor: null, meter: deps.meter(), peaks: [], scope: null, tuner: null, ...extra };
  }
  /** Answer a change now: one `chains` frame (and the batch's `Toggled`), to the feed's subscribers. */
  function emitChains(): void {
    scheduled = false;
    const events = pendingEvents;
    pendingEvents = [];
    seq++;
    deps.deliver(frame({ seq, events, chains: snapshot(), unitMeters: meters(false) }));
    syncTimer();
  }
  /** Answer the batch's changes a microtask later, in one frame: never inside the call that sent them. */
  function scheduleChains(): void {
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(emitChains);
  }

  function onSlot<T>(slot: Slot, op: () => Promise<T | Later<T>>): Promise<T> {
    const run = tails[slot].then(op);
    tails[slot] = run.catch(() => {});
    return run.then((r) => (r instanceof Later ? r.done : r));
  }
  const refuse = (reason: string): never => {
    throw new Error(reason);
  };
  function guard(): void {
    if (!deps.forced()) refuse(NO_ENGINE);
  }
  function seatedPlugin(unit: UnitId): Unit & { state: { kind: 'plugin'; plugin: PluginRef } } {
    const u = find(unit);
    if (!u || u.state.kind !== 'plugin' || !sounding(u)) return refuse(`unit ${unit} is no seated plugin`);
    return u as Unit & { state: { kind: 'plugin'; plugin: PluginRef } };
  }

  // ── D29 by unit (unit contract §1): a file's editor belongs to the instance loaded first, either slot ──

  /** A plugin instance's load of `path` starts: the first load of a file no instance holds owns its
   * editor; one starting while the owner's load is still in flight cannot be ordered (`unordered`). */
  function claim(path: string): number {
    const inst = ++instances;
    const entry = affinity.get(path);
    const holding = holders.get(path) ?? new Set<number>();
    if (!entry) affinity.set(path, { inst, unit: 0, state: 'owned' });
    else if (entry.state === 'owned' && loading.has(entry.inst)) entry.state = 'unordered';
    holding.add(inst);
    holders.set(path, holding);
    loading.add(inst);
    return inst;
  }
  /** Name the unit an owning instance runs in, once its load answered. */
  function landed(inst: number, path: string, unit: UnitId): void {
    loading.delete(inst);
    const entry = affinity.get(path);
    if (entry?.inst === inst) entry.unit = unit;
  }
  /** An instance let go of `path` (its removal, a failed load, a reload's old instance): the entry dies with
   * the module's last holder; else, its owner gone, the owner thread is gone with the module still bound
   * to it: `orphaned`, refused for every unit (today's rule, per unit). */
  function release(inst: number, path: string): void {
    loading.delete(inst);
    const holding = holders.get(path);
    holding?.delete(inst);
    const entry = affinity.get(path);
    if (!holding || holding.size === 0) {
      holders.delete(path);
      affinity.delete(path);
    } else if (entry?.inst === inst && entry.state === 'owned') entry.state = 'orphaned';
  }

  /** A built-in's params as it applies them at build: every param of its kind, the asked ones clamped. */
  function buildParams(kind: BuiltinKind, asked: Partial<Record<UnitParamId, number>> | undefined): Partial<Record<UnitParamId, number>> {
    const ranges = PARAMS[kind];
    const params: Partial<Record<UnitParamId, number>> = {};
    for (const [k, r] of Object.entries(ranges) as [UnitParamId, Range][]) {
      const v = asked?.[k];
      params[k] = typeof v === 'number' && Number.isFinite(v) ? clampParam(r, v) : r.def;
    }
    return params;
  }

  function newState(unit: UnitId, spec: UnitSpec): UnitState {
    const bypass = spec.bypass === true;
    if (spec.kind === 'plugin') {
      const plugin = { format: spec.plugin.format, path: spec.plugin.path, id: spec.plugin.id };
      const name = canned(plugin)?.descriptor.name ?? baseName(plugin.path);
      return { id: unit, kind: 'plugin', plugin, bypass, source: false, name, latency: 0, state: 'loading' };
    }
    return { id: unit, kind: spec.kind, params: buildParams(spec.kind, spec.params), bypass, source: spec.kind === 'synth', name: NAMES[spec.kind], latency: 0, state: 'loading' };
  }

  /** Activate: the role and latency the unit reports, or why its load failed. */
  function activate(id: UnitId, kind: UnitKindId, plugin: PluginRef | null): { source: boolean; latency: number } | { error: string } {
    const scripted = fake.failures.get(id);
    if (scripted !== undefined) {
      fake.failures.delete(id);
      return { error: scripted };
    }
    if (kind === 'synth') return { source: true, latency: SYNTH_LATENCY };
    if (kind !== 'plugin' || !plugin) return { source: false, latency: 0 };
    const found = canned(plugin);
    if (!found) return { error: `${baseName(plugin.path)} was not found` };
    // The canned role stands for what activation reports (unit contract §The picture: only it settles one).
    return { source: found.descriptor.isEffect === false, latency: found.latency };
  }

  /** A unit leaves the model: its id is free, its instance let go, a plugin's tone kept as its slot-and-plugin
   * file (unit contract §1). */
  function drop(u: Unit): void {
    chains[u.slot] = chains[u.slot].filter((o) => o !== u);
    if (reserved.get(u.state.id) === u.slot && !u.noSeat) reserved.delete(u.state.id);
    sims.delete(u.state.id);
    lastBands.delete(u.state.id);
    if (!u.noSeat) windowClosed(u.state.id);
    if (u.state.kind === 'plugin' && !u.noSeat) {
      if (u.tone) slotTones.set(toneKey(u.slot, u.state.plugin), u.tone);
      release(u.inst, u.state.plugin.path);
    }
  }

  /** Seat an activated unit at its place: a source at the head, replacing a seated source (§3, §4). */
  function place(u: Unit): void {
    const slot = u.slot;
    if (u.state.source) {
      const old = chains[slot].find((o) => o !== u && applied(o) && o.state.source);
      if (old) drop(old);
      chains[slot] = [u, ...chains[slot].filter((o) => o !== u)];
    }
    u.state.state = 'seated';
    delete u.state.error;
  }

  async function holdActivation(): Promise<void> {
    await Promise.resolve();
    const hold = fake.holdLoading;
    if (hold) await hold;
  }

  /** Release the held edits once no take holds the alignment, in order, one frame. */
  function judge(): void {
    if (capturing() || (held[0].length === 0 && held[1].length === 0)) return;
    for (const slot of [0, 1] as const) held[slot].splice(0).forEach((h) => h.apply());
    emitChains();
  }

  /** Hold `h` behind a take when it moves the slot's alignment or the slot already waits. */
  function heldOrNow(slot: Slot, h: Held, moves: boolean): boolean {
    if (capturing() && (moves || held[slot].length > 0)) {
      held[slot].push(h);
      return true;
    }
    return false;
  }

  /** Unit contract §4: a source only heads its chain; an effect never goes ahead of one. `null`: allowed. */
  function placeRefusal(u: Unit, others: readonly Unit[], index: number): string | null {
    if (u.state.source && index !== 0) return 'a source only heads its chain';
    if (!u.state.source && index === 0 && headOf(others)?.state.source) return 'an effect cannot go ahead of the source';
    return null;
  }

  /** A parked tone's token for `plugin`, or why an add or a reload may not take it (unit contract §8). */
  function parkedTone(token: number, plugin: PluginRef | null): Uint8Array {
    const t = parked.get(token);
    if (!t) return refuse(`tone token ${token} is not parked`);
    if (!plugin || !sameRef(t.plugin, plugin)) return refuse(`tone token ${token} belongs to ${refName(t.plugin)}`);
    return t.state;
  }

  const host: ChainHost = {
    get available() {
      return deps.forced();
    },
    async add(slot, unit, spec, at, toneToken) {
      fake.calls.push(['add', slot, unit, structuredClone(spec), at, ...(toneToken === undefined ? [] : [toneToken])]);
      guard();
      if (slot !== 0 && slot !== 1) refuse(`slot ${slot} does not exist`);
      if (!Number.isSafeInteger(unit) || unit < 1 || unit > 0xffff_ffff) refuse(`unit id ${unit} is not a non-zero u32`);
      // Unit contract §1: a reserved id is refused at once, never at seating.
      if (reserved.has(unit)) refuse(`unit ${unit} is reserved`);
      const plugin = spec.kind === 'plugin' ? spec.plugin : null;
      if (toneToken !== undefined) parkedTone(toneToken, plugin);
      // Unit contract §1, §2, §4: a ninth unit is refused at once, unless it may resolve at activation as
      // the source that replaces the seated one (a plugin's role is known only there; SYNTH is a source).
      // A failed add's plate holds its place until removed, so it counts.
      const entries = [...reserved.values()].filter((s) => s === slot).length + chains[slot].filter((u) => u.noSeat && u.state.id !== unit).length;
      const mayReplace = (spec.kind === 'plugin' || spec.kind === 'synth') && headOf(chains[slot])?.state.source === true;
      if (entries > MAX_UNITS || (entries === MAX_UNITS && !mayReplace)) refuse(`the chain holds ${MAX_UNITS} units`);
      reserved.set(unit, slot);
      const g = gen;
      return onSlot(slot, async () => {
        if (g !== gen) return refuse(RESET);
        // The id is free: a failed add's plate under it gives way (unit contract §1).
        const old = tombstone(unit);
        if (old) chains[old.slot] = chains[old.slot].filter((o) => o !== old);
        const u: Unit = { state: newState(unit, spec), slot, addedSeq: seq + 1, inst: 0, tone: null };
        const list = chains[slot];
        list.splice(Math.max(0, Math.min(Math.trunc(at), list.length)), 0, u);
        if (plugin) u.inst = claim(plugin.path);
        emitChains();
        await holdActivation();
        if (g !== gen) {
          if (plugin) release(u.inst, plugin.path);
          return refuse(RESET);
        }
        const others = () => chains[slot].filter((o) => o !== u);
        /** The engine refused the install: the plate goes, the id is free. */
        const refused = (reason: string): never => {
          drop(u);
          emitChains();
          return refuse(reason);
        };
        const answer = activate(unit, u.state.kind, plugin);
        if ('error' in answer) {
          // Unit contract §1, §2: a failed add frees its id; its plate stays `failed` in its place with the
          // reason (no seat) until the player removes it, unless it would be a chain's ninth entry (the
          // source that would have replaced one): then it goes, the add's rejection says why.
          if (others().length >= MAX_UNITS) return refused(answer.error);
          if (plugin) release(u.inst, plugin.path);
          reserved.delete(unit);
          u.noSeat = true;
          u.state.state = 'failed';
          u.state.error = answer.error;
          emitChains();
          return refuse(answer.error);
        }
        if (plugin) landed(u.inst, plugin.path, unit);
        u.state.source = answer.source;
        u.state.latency = answer.latency;
        const why = placeRefusal(u, others(), u.state.source ? 0 : chains[slot].indexOf(u));
        if (why) return refused(why);
        const replaces = u.state.source && others().some((o) => applied(o) && o.state.source);
        if (!replaces && others().length >= MAX_UNITS) return refused(`the chain holds ${MAX_UNITS} units`);
        if (toneToken !== undefined) {
          u.tone = parkedTone(toneToken, plugin);
          parked.delete(toneToken);
        } else if (plugin) u.tone = slotTones.get(toneKey(slot, plugin)) ?? null; // §1: starts from the slot's file
        const before = alignment(appliedStates(chains[slot]));
        const kept = appliedStates(others().filter((o) => !(replaces && o.state.source)));
        const after = alignment(u.state.source ? [u.state, ...kept] : [...kept, u.state]);
        const seat = () => {
          if (!chains[slot].includes(u)) return; // removed while it waited
          place(u);
        };
        const cancel = () => drop(u);
        if (heldOrNow(slot, { unit, kind: 'add', apply: seat, drop: () => {}, cancel }, before !== after)) {
          u.state.state = 'waiting';
          emitChains();
          return 'waiting';
        }
        seat();
        emitChains();
        return 'seated';
      });
    },
    async remove(unit) {
      fake.calls.push(['remove', unit]);
      guard();
      // A failed add's plate: no unit, no engine message; it goes at once (unit contract §2).
      const plate = reserved.has(unit) ? undefined : tombstone(unit);
      if (plate) {
        drop(plate);
        emitChains();
        return;
      }
      const slot = reserved.get(unit);
      if (slot === undefined) return refuse(`unit ${unit} is in no chain`);
      const g = gen;
      return onSlot(slot, async (): Promise<void | Later<void>> => {
        if (g !== gen) return refuse(RESET);
        const u = find(unit);
        if (!u || u.state.state === 'leaving') return;
        // A held edit of this unit: an add that never applied leaves without a trace; a reload is not taken.
        const mine = held[slot].filter((h) => h.unit === unit && h.cancel);
        held[slot] = held[slot].filter((h) => !mine.includes(h));
        mine.forEach((h) => h.cancel?.());
        if (mine.some((h) => h.kind === 'add')) {
          emitChains();
          return;
        }
        const before = alignment(appliedStates(chains[slot]));
        const after = alignment(appliedStates(chains[slot].filter((o) => o !== u)));
        u.state.state = 'leaving';
        // Unit contract §2: `remove` settles on the engine's answer: a held removal once it applied.
        const d = deferred<void>();
        const go = () => {
          drop(u);
          d.resolve();
        };
        if (heldOrNow(slot, { unit, kind: 'remove', apply: go, drop: (r) => d.reject(new Error(r)) }, before !== after)) {
          emitChains();
          return new Later(d.promise);
        }
        emitChains();
        await Promise.resolve();
        if (g !== gen) return refuse(RESET);
        go();
        emitChains();
      });
    },
    async reload(unit, plugin, since, toneToken) {
      fake.calls.push(['reload', unit, structuredClone(plugin), since, toneToken]);
      guard();
      const state = parkedTone(toneToken, plugin);
      const slot = reserved.get(unit);
      if (slot === undefined) return 'moved';
      const g = gen;
      return onSlot(slot, async (): Promise<'reloaded' | 'moved' | 'failed' | Later<'reloaded' | 'moved' | 'failed'>> => {
        if (g !== gen) return refuse(RESET);
        const u = find(unit);
        if (!u || u.state.kind !== 'plugin' || u.addedSeq > since || u.state.state !== 'seated') return 'moved';
        const ref = { format: plugin.format, path: plugin.path, id: plugin.id };
        // Unit contract §3: a reload `replaces` its own id. The old instance stays seated, applied and
        // counted until the new one is activated and lands; the plate reads `loading` meanwhile.
        u.reloading = true;
        u.state.state = 'loading';
        const inst = claim(ref.path);
        emitChains();
        await holdActivation();
        if (g !== gen) {
          release(inst, ref.path);
          return refuse(RESET);
        }
        const answer = activate(unit, 'plugin', ref);
        const back = (): 'failed' => {
          release(inst, ref.path);
          delete u.reloading;
          u.state.state = 'seated';
          emitChains();
          return 'failed';
        };
        // A failed load, or one that came back in another role, leaves the old seated (§3).
        if ('error' in answer || answer.source !== u.state.source) return back();
        landed(inst, ref.path, unit);
        parked.delete(toneToken);
        const commit = () => {
          const old = u.state.kind === 'plugin' ? u.state.plugin : ref;
          release(u.inst, old.path);
          u.inst = inst;
          u.state = { ...u.state, kind: 'plugin', plugin: ref, latency: answer.latency, name: canned(ref)?.descriptor.name ?? baseName(ref.path), state: 'seated' };
          u.tone = state;
          delete u.reloading;
        };
        // §7: a reseat that changes the chain's sum waits for the take, as an install does.
        const before = alignment(appliedStates(chains[slot]));
        const after = alignment(appliedStates(chains[slot]).map((s) => (s === u.state ? { ...s, latency: answer.latency } : s)));
        const d = deferred<'reloaded' | 'moved' | 'failed'>();
        const h: Held = {
          unit,
          kind: 'reload',
          apply: () => {
            if (!chains[slot].includes(u)) return d.resolve('moved');
            commit();
            d.resolve('reloaded');
          },
          drop: (r) => (release(inst, ref.path), d.reject(new Error(r))),
          cancel: () => {
            release(inst, ref.path);
            delete u.reloading;
            u.state.state = 'seated';
            d.resolve('moved');
          },
        };
        if (heldOrNow(slot, h, before !== after)) {
          u.state.state = 'waiting';
          emitChains();
          return new Later(d.promise);
        }
        commit();
        emitChains();
        return 'reloaded';
      });
    },
    async move(unit, to) {
      fake.calls.push(['move', unit, to]);
      guard();
      const slot = reserved.get(unit);
      if (slot === undefined) return refuse(`unit ${unit} is in no chain`);
      const g = gen;
      return onSlot(slot, async (): Promise<void | Later<void>> => {
        if (g !== gen) return refuse(RESET);
        /** The move as the chain is when it applies: refused if the unit left or the place broke §4. */
        const apply = (): void => {
          const u = find(unit);
          if (!u || u.slot !== slot || u.state.state !== 'seated') return refuse(`unit ${unit} is not seated`);
          const rest = chains[slot].filter((o) => o !== u);
          const index = Math.max(0, Math.min(Math.trunc(to), rest.length));
          const why = placeRefusal(u, rest, index);
          if (why) return refuse(why);
          rest.splice(index, 0, u);
          chains[slot] = rest;
        };
        // A move changes no latency and no role: never held itself, only behind the slot's held edits, and
        // then judged again when it applies (unit contract §7).
        const d = deferred<void>();
        const h: Held = {
          unit,
          kind: 'move',
          apply: () => {
            try {
              apply();
              d.resolve();
            } catch (e) {
              d.reject(e instanceof Error ? e : new Error(String(e)));
            }
          },
          drop: (r) => d.reject(new Error(r)),
        };
        if (heldOrNow(slot, h, false)) return new Later(d.promise);
        apply();
        emitChains();
      });
    },
    async openEditor(unit) {
      fake.calls.push(['openEditor', unit]);
      guard();
      const u = seatedPlugin(unit);
      // D29 by unit (unit contract §1): a file's editor belongs to the instance loaded first, in either
      // slot; every other instance is refused it (its parameters work), and an orphaned or unordered entry
      // refuses every one.
      const entry = affinity.get(u.state.plugin.path);
      if (!entry) return;
      if (entry.state === 'orphaned') refuse(`the editor of ${u.state.name} is bound to unit ${entry.unit}'s previous load`);
      if (entry.state === 'unordered') refuse(`the editor of ${u.state.name} belongs to an unknown unit (two loaded it at once)`);
      if (entry.inst !== u.inst) refuse(`the editor of ${u.state.name} belongs to unit ${entry.unit}`);
      editorsOpen.add(unit);
    },
    async closeEditor(unit) {
      fake.calls.push(['closeEditor', unit]);
      guard();
      // a close the UI asked for is no window event of its own
      editorsOpen.delete(unit);
    },
    onEditorClosed(cb) {
      editorClosed.add(cb);
      return () => editorClosed.delete(cb);
    },
    async listParams(unit): Promise<PluginParamDesc[]> {
      fake.calls.push(['listParams', unit]);
      guard();
      seatedPlugin(unit);
      return [];
    },
    async setParameter(unit, paramId, value) {
      fake.calls.push(['setParameter', unit, paramId, value]);
      guard();
      seatedPlugin(unit);
    },
    async takeTone(unit) {
      fake.calls.push(['takeTone', unit]);
      guard();
      const u = seatedPlugin(unit);
      // The canned plugins change no state of their own: a unit's tone is what it loaded, if anything.
      return u.tone ? encodeTone(u.state.plugin, u.state.name, u.tone) : null;
    },
    async importTone(slot, bytes, plugin, unit) {
      fake.calls.push(['importTone', slot, bytes.slice(), structuredClone(plugin), ...(unit === undefined ? [] : [unit])]);
      guard();
      // Identity-checked as today (`tone.rs` `import`): a file that cannot be read or names another plugin.
      const tone = decodeTone(bytes);
      if ('error' in tone) return refuse(`the session's tone file cannot be read: ${tone.error}`);
      if (!sameRef(tone.plugin as PluginRef, plugin)) {
        return refuse(`the session's tone file belongs to ${tone.name} (${refName(tone.plugin)}), not to the plugin session.json names (${refName(plugin)})`);
      }
      // Unit contract §8: the tone's own unit first, wherever it is seated with its identity; then every
      // other unit of its identity in its slot, in chain order. The UI's two passes pick among them.
      const same = (u: Unit) => u.state.kind === 'plugin' && u.state.state === 'seated' && sameRef(u.state.plugin, plugin);
      const own = unit === undefined ? undefined : all().find((u) => u.state.id === unit && same(u));
      const matches = [...(own ? [own] : []), ...chains[slot].filter((u) => u !== own && same(u))].map((u) => u.state.id);
      if (matches.length === 0) {
        // §8: a tone with no match is stored as its slot-and-plugin file (§1); the next unit of that plugin
        // added there starts from it.
        slotTones.set(toneKey(slot, plugin), tone.state);
        return { token: null, name: tone.name, matches };
      }
      const token = ++toneTokens;
      parked.set(token, { plugin: { ...plugin }, slot, state: tone.state });
      return { token, name: tone.name, matches };
    },
    async forgetTone(token) {
      fake.calls.push(['forgetTone', token]);
      guard();
      // Unit contract §8: a token no reload took is forgotten, not stored: only a tone with no match at
      // import becomes the slot's file (the reading closest to "every token no reload took is forgotten").
      parked.delete(token);
    },
  };

  // ── The batch: bypass and params (unit contract §2, §6) ────────────────────────────────────────────

  const seated = (unit: UnitId): Unit | undefined => {
    const u = find(unit);
    return u && sounding(u) ? u : undefined;
  };
  function setBypass(u: Unit, bypass: boolean): void {
    if (u.state.bypass === bypass) return;
    u.state.bypass = bypass;
    scheduleChains();
  }
  function toggleUnits(kind: UnitEffectKindId): void {
    const units = all().filter((u) => u.state.kind === kind && sounding(u));
    if (units.length === 0) return; // none of the kind: nothing, not even a `Toggled`
    const on = !units.some((u) => !u.state.bypass);
    units.forEach((u) => setBypass(u, !on));
    pendingEvents.push({ type: 'Toggled', frame: 0, toggle: { Units: kind }, on });
    scheduleChains();
  }

  /** Apply a batch command the chain owns; false when it is not one. */
  function command(c: EngineCommand): boolean {
    if (!isUnitCommand(c)) return false;
    const action = typeof c === 'object' && 'Action' in c ? c.Action : typeof c === 'object' && 'ActionOn' in c ? c.ActionOn[1] : null;
    if (typeof c === 'object' && 'SetUnitBypass' in c) {
      const u = seated(c.SetUnitBypass[0]);
      if (u) setBypass(u, c.SetUnitBypass[1]);
      else fake.dropped.push(c);
    } else if (typeof c === 'object' && 'SetUnitParam' in c) {
      const [unit, param, value] = c.SetUnitParam;
      const u = seated(unit);
      const range = u && u.state.kind !== 'plugin' ? PARAMS[u.state.kind][param] : undefined;
      if (!u || !range || u.state.kind === 'plugin') fake.dropped.push(c);
      else {
        const params = u.state.params ?? (u.state.params = {});
        const v = clampParam(range, value);
        if (params[param] !== v) {
          params[param] = v;
          scheduleChains();
        }
      }
    } else if (action !== null && typeof action === 'object' && 'ToggleUnit' in action) {
      const u = seated(action.ToggleUnit);
      if (u) setBypass(u, !u.state.bypass);
      else fake.dropped.push(c);
    } else if (action !== null && typeof action === 'object' && 'Toggle' in action && typeof action.Toggle === 'object' && 'Units' in action.Toggle) {
      toggleUnits(action.Toggle.Units);
    }
    return true;
  }

  /** A scripted frame: one carrying `chains` sets the model (a restored rig, a rebuilt engine); a reset
   * frame without it carries the model, as the engine's does. The feed has one sequence: the fake's own
   * frames number on from the largest seq scripted. */
  function scripted(f: FeedFrame): void {
    seq = Math.max(seq, f.seq);
    if (f.chains) {
      // Every call and held edit begun against the old model answers refused, never against the new one.
      gen++;
      for (const slot of [0, 1] as const) held[slot].splice(0).forEach((h) => h.drop(RESET));
      const before = new Map(all().filter((u) => !u.noSeat).map((u) => [u.state.id, u]));
      reserved.clear();
      affinity.clear();
      holders.clear();
      loading.clear();
      f.chains.forEach((c, slot) => {
        chains[slot] = c.units.map((state): Unit => {
          const prev = before.get(state.id);
          // A unit the model held with the same identity keeps the frame that first told it (`reload`'s
          // `since`) and its tone; any other is new as of this frame.
          const same = prev !== undefined && prev.state.kind === state.kind && (state.kind !== 'plugin' || (prev.state.kind === 'plugin' && sameRef(prev.state.plugin, state.plugin)));
          const u: Unit = { state: structuredClone(state), slot: slot as Slot, addedSeq: same ? prev.addedSeq : f.seq, inst: 0, tone: same ? prev.tone : null };
          // The scripted rig's plugin instances load in chain order, slot A first, each landed (rig recall).
          if (state.kind === 'plugin') {
            u.inst = claim(state.plugin.path);
            landed(u.inst, state.plugin.path, state.id);
          }
          return u;
        });
        c.units.forEach((u) => reserved.set(u.id, slot as Slot));
      });
    } else if (f.reset) f.chains = snapshot();
    if (f.reset) {
      // Unit contract §3: a unit entering an engine resets its audio state: the OTT's followers restart.
      sims.clear();
      lastBands.clear();
      f.unitMeters ??= meters(false);
    }
    syncTimer();
  }

  // ── The meters (unit contract §2, §10): one per seated unit; the OTT face's from the simulated part ──

  const active = (u: Unit) => u.state.kind === 'ott' && sounding(u) && !u.state.bypass;
  function ottParams(u: Unit): OttParams {
    const p = u.state.kind === 'ott' ? (u.state.params ?? {}) : {};
    return { depth: p.ottDepth ?? 0.5, time: p.ottTime ?? 0.5, up: p.ottUpward ?? 1, down: p.ottDownward ?? 1 };
  }
  /** At rest: no gain, the follower at its floor (an OTT that is bypassed or has not run yet). */
  const restBands = (): [BandMeter, BandMeter, BandMeter] =>
    [0, 1, 2].map(() => ({ gain: 0, gainMin: 0, gainMax: 0, level: LEVEL_FLOOR, levelPeak: LEVEL_FLOOR })) as [BandMeter, BandMeter, BandMeter];
  /**
   * One meter per seated unit, chain order, slot A first (unit contract §2). The fake has no sound: a unit
   * reads silent, an OTT that is bypassed reads its bands at rest; an active OTT reads the simulated part,
   * one sixtieth of a second further when `advance` (the timer's frame), else its last reading with no time
   * passing (a `chains` frame between two ticks).
   */
  function meters(advance: boolean): UnitMeter[] {
    return all()
      .filter(sounding)
      .map((u): UnitMeter => {
        const unit = u.state.id;
        if (!active(u)) return u.state.kind === 'ott' ? { unit, out: [0, 0], bands: restBands() } : { unit, out: [0, 0] };
        if (!advance) {
          const last = lastBands.get(unit);
          if (!last?.bands) return { unit, out: [0, 0], bands: restBands() };
          const still = last.bands.map((b) => ({ gain: b.gain, gainMin: b.gain, gainMax: b.gain, level: b.level, levelPeak: b.level }));
          return { unit, out: [last.out[0], last.out[1]], bands: still as [BandMeter, BandMeter, BandMeter] };
        }
        const p = ottParams(u);
        let sim = sims.get(unit);
        if (!sim) sims.set(unit, (sim = newOttSim(p)));
        const m = simFrame(sim, p, unit);
        lastBands.set(unit, m);
        return m;
      });
  }
  function tick(): void {
    if (!all().some(active)) return;
    deps.deliver(frame({ unitMeters: meters(true) }));
  }
  /** Run the timer exactly while it is on and an OTT is seated and on (unit contract §10). */
  function syncTimer(): void {
    const want = fake.meterTimer && deps.forced() && all().some(active);
    if (want && timer === null) timer = setInterval(tick, 1000 / 60);
    else if (!want && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  let meterTimer = true;
  const fake: WiredChainFake = {
    host,
    calls: [],
    dropped: [],
    plugins: CANNED_PLUGINS,
    holdLoading: null,
    failures: new Map(),
    get capture() {
      return captureSeam;
    },
    set capture(on: boolean) {
      captureSeam = on;
      judge();
    },
    park(unit, reason) {
      const u = find(unit);
      if (!u || u.state.state !== 'seated') throw new Error(`unit ${unit} is not seated`);
      u.state.state = 'failed';
      u.state.error = reason;
      u.state.parked = true;
      // a withdrawn plugin's window goes with it
      windowClosed(unit);
      emitChains();
    },
    reseat(unit) {
      const u = find(unit);
      if (!u || u.state.state !== 'failed') throw new Error(`unit ${unit} is not parked`);
      u.state.state = 'seated';
      delete u.state.error;
      delete u.state.parked;
      emitChains();
    },
    editorsOpen,
    closeWindow(unit) {
      if (!editorsOpen.has(unit)) throw new Error(`unit ${unit} has no editor window open`);
      windowClosed(unit);
    },
    get meterTimer() {
      return meterTimer;
    },
    set meterTimer(on: boolean) {
      meterTimer = on;
      syncTimer();
    },
    get meterTimerRunning() {
      return timer !== null;
    },
    meterTick: tick,
    mint: null,
    chains: snapshot,
    command,
    scripted,
    judge,
  };
  return fake;
}
