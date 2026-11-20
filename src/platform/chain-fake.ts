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
 *   in one install; an effect never goes ahead of a source; an edit that moves a chain's latency sum or its
 *   role waits (`waiting`, `held`) while a take holds the alignment, the slot's later edits behind it;
 * - the batch (§2, §6): `SetUnitBypass`, `ToggleUnit`, `Toggle.Units` (answered by one `Toggled`) and
 *   `SetUnitParam` (clamped as the unit clamps it) switch the model, answered by `chains`; a command naming
 *   an id that is not seated, or a param of another kind, is dropped and counted (`dropped`);
 * - the OTT face's feed (§2): `unitMeters` on the fake's own 60 Hz timer while an OTT is seated and on,
 *   from the accepted mockup's simulated guitar part through the same curve (`docs/redesign/ott.js`,
 *   SIM-BEGIN to SIM-END), following each OTT's four params.
 *
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
} from './engine-wire.ts'; // explicit .ts: Node guards import this file

/** What the fake needs of the engine fake around it (`host.web.ts`). */
interface ChainFakeDeps {
  /** The engine fake is on (a DEV probe forced it). */
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
  /** Probe seam: the meters' own 60 Hz timer (on by default); off, `meterTick` emits one frame. */
  meterTimer: boolean;
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

// ── The model ────────────────────────────────────────────────────────────────────────────────────────

/** A unit as the model holds it: its state on the wire, its slot and the `chains` frame that first told it. */
interface Unit {
  state: UnitState;
  slot: Slot;
  addedSeq: number;
}
/** An edit held while a take holds the alignment: applied in order once it ends. */
interface Held {
  unit: UnitId;
  apply(): void;
  /** A held removal or add that a later removal cancels. */
  cancel?(): void;
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

export function createChainFake(deps: ChainFakeDeps): WiredChainFake {
  const chains: [Unit[], Unit[]] = [[], []];
  /** Every reserved id and its slot: from `add` until its removal or a failed add (unit contract §1). */
  const reserved = new Map<UnitId, Slot>();
  const held: [Held[], Held[]] = [[], []];
  /** One structure call at a time per slot (unit contract §2). */
  const tails: [Promise<unknown>, Promise<unknown>] = [Promise.resolve(), Promise.resolve()];
  const sims = new Map<UnitId, OttSim>();
  const parkedTones = new Set<number>();
  let toneTokens = 0;
  let seq = 0;
  let pendingEvents: EngineEvent[] = [];
  let scheduled = false;
  let timer: ReturnType<typeof setInterval> | null = null;
  let captureSeam = false;

  const find = (unit: UnitId): Unit | undefined => chains[reserved.get(unit) ?? 0].find((u) => u.state.id === unit);
  const applied = (u: Unit) => u.state.state === 'seated' || u.state.state === 'leaving' || u.state.state === 'failed';
  const capturing = () => captureSeam || deps.capturing();

  /** The chain as applied: its units in order, their latency sum (bypassed and parked seats included:
   * a parked seat keeps its latency, unit contract §3), held while an edit waits. */
  function chainState(slot: Slot): ChainState {
    const units = chains[slot].map((u) => structuredClone(u.state));
    const latency = chains[slot].filter(applied).reduce((s, u) => s + u.state.latency, 0);
    return { units, latency, held: held[slot].length > 0 };
  }
  const snapshot = (): [ChainState, ChainState] => [chainState(0), chainState(1)];
  /** What moves a take's alignment (unit contract §7): the applied units' latency sum and whether a
   * source heads the chain. The fake does not model which chains are live. */
  const alignment = (states: readonly UnitState[]): string => `${states.reduce((s, u) => s + u.latency, 0)}:${states[0]?.source === true}`;
  const appliedStates = (units: readonly Unit[]): UnitState[] => units.filter(applied).map((u) => u.state);

  function frame(extra: Partial<FeedFrame>): FeedFrame {
    return { seq: 0, reset: false, catchUp: false, events: [], device: [], anchor: null, meter: deps.meter(), peaks: [], scope: null, tuner: null, ...extra };
  }
  /** Answer a change now: one `chains` frame (and the batch's `Toggled`), to the feed's subscribers. */
  function emitChains(): void {
    scheduled = false;
    const events = pendingEvents;
    pendingEvents = [];
    seq++;
    deps.deliver(frame({ seq, events, chains: snapshot() }));
    syncTimer();
  }
  /** Answer the batch's changes a microtask later, in one frame: never inside the call that sent them. */
  function scheduleChains(): void {
    if (scheduled) return;
    scheduled = true;
    queueMicrotask(emitChains);
  }
  /** The `chains` frame seq that tells the unit's newest state. */
  const nextSeq = () => seq + 1;

  function onSlot<T>(slot: Slot, op: () => Promise<T>): Promise<T> {
    const run = tails[slot].then(op);
    tails[slot] = run.catch(() => {});
    return run;
  }
  const refuse = (reason: string): never => {
    throw new Error(reason);
  };
  function guard(): void {
    if (!deps.forced()) refuse(NO_ENGINE);
  }
  function seatedPlugin(unit: UnitId): Unit {
    const u = find(unit);
    if (!u || u.state.kind !== 'plugin' || u.state.state !== 'seated') return refuse(`unit ${unit} is no seated plugin`);
    return u;
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
  function activate(state: UnitState): { source: boolean; latency: number } | { error: string } {
    const scripted = fake.failures.get(state.id);
    if (scripted !== undefined) {
      fake.failures.delete(state.id);
      return { error: scripted };
    }
    if (state.kind === 'synth') return { source: true, latency: SYNTH_LATENCY };
    if (state.kind !== 'plugin') return { source: false, latency: 0 };
    const found = canned(state.plugin);
    if (!found) return { error: `${state.name} was not found` };
    // The canned role stands for what activation reports (unit contract §The picture: only it settles one).
    return { source: found.descriptor.isEffect === false, latency: found.latency };
  }

  function drop(slot: Slot, unit: UnitId): void {
    chains[slot] = chains[slot].filter((u) => u.state.id !== unit);
    reserved.delete(unit);
    sims.delete(unit);
  }

  /** Seat an activated unit at its place: a source at the head, replacing a seated source (§3, §4). */
  function place(slot: Slot, u: Unit): void {
    const list = chains[slot];
    if (u.state.source) {
      const old = list.find((o) => o !== u && o.state.source);
      if (old) drop(slot, old.state.id);
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

  /** Hold `apply` behind a take when it moves the slot's alignment or the slot already waits; else run it. */
  function heldOrNow(slot: Slot, h: Held, moves: boolean): boolean {
    if (capturing() && (moves || held[slot].length > 0)) {
      held[slot].push(h);
      return true;
    }
    return false;
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
      // Unit contract §2, §4: a ninth unit is refused. Only SYNTH is known as a source before activation and
      // may replace one in a full chain; a plugin's role waits for activation, so here it counts as a ninth.
      const count = [...reserved.values()].filter((s) => s === slot).length;
      if (count >= MAX_UNITS && spec.kind !== 'synth') refuse(`the chain holds ${MAX_UNITS} units`);
      reserved.set(unit, slot);
      return onSlot(slot, async () => {
        const u: Unit = { state: newState(unit, spec), slot, addedSeq: nextSeq() };
        const list = chains[slot];
        list.splice(Math.max(0, Math.min(Math.trunc(at), list.length)), 0, u);
        emitChains();
        await holdActivation();
        const fail = (reason: string): never => {
          drop(slot, unit);
          emitChains();
          return refuse(reason);
        };
        const answer = activate(u.state);
        if ('error' in answer) return fail(answer.error);
        u.state.source = answer.source;
        u.state.latency = answer.latency;
        const others = chains[slot].filter((o) => o !== u);
        // Unit contract §4: an effect never goes ahead of a source.
        if (!u.state.source && chains[slot].indexOf(u) === 0 && others[0]?.state.source) return fail('an effect cannot go ahead of the source');
        const replaces = u.state.source && others.some((o) => o.state.source);
        if (others.length - (replaces ? 1 : 0) >= MAX_UNITS) return fail(`the chain holds ${MAX_UNITS} units`);
        if (toneToken !== undefined) parkedTones.delete(toneToken);
        const before = alignment(appliedStates(chains[slot]));
        const kept = appliedStates(others.filter((o) => !(replaces && o.state.source)));
        const after = alignment(u.state.source ? [u.state, ...kept] : [...kept, u.state]);
        const seat = () => place(slot, u);
        if (heldOrNow(slot, { unit, apply: seat, cancel: () => drop(slot, unit) }, before !== after)) {
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
      const slot = reserved.get(unit);
      if (slot === undefined) return refuse(`unit ${unit} is in no chain`);
      return onSlot(slot, async () => {
        const u = find(unit);
        if (!u) return;
        // A held add that never applied leaves without a trace.
        const waiting = held[slot].findIndex((h) => h.unit === unit && h.cancel);
        if (waiting >= 0 && u.state.state === 'waiting') {
          held[slot].splice(waiting, 1)[0].cancel?.();
          emitChains();
          return;
        }
        u.state.state = 'leaving';
        const before = alignment(appliedStates(chains[slot]));
        const after = alignment(appliedStates(chains[slot].filter((o) => o !== u)));
        if (heldOrNow(slot, { unit, apply: () => drop(slot, unit) }, before !== after)) {
          emitChains();
          return;
        }
        emitChains();
        await Promise.resolve();
        drop(slot, unit);
        emitChains();
      });
    },
    async reload(unit, plugin, since, toneToken) {
      fake.calls.push(['reload', unit, structuredClone(plugin), since, toneToken]);
      guard();
      const slot = reserved.get(unit);
      if (slot === undefined) return 'moved';
      return onSlot(slot, async () => {
        const u = find(unit);
        if (!u || u.state.kind !== 'plugin' || u.addedSeq > since || u.state.state !== 'seated') return 'moved';
        parkedTones.delete(toneToken);
        u.state.state = 'loading';
        emitChains();
        await holdActivation();
        const previous = u.state.plugin;
        u.state.plugin = { format: plugin.format, path: plugin.path, id: plugin.id };
        const answer = activate(u.state);
        if ('error' in answer || answer.source !== u.state.source) {
          // Unit contract §3: a failed load leaves the old seated.
          u.state.plugin = previous;
          u.state.state = 'seated';
          emitChains();
          return 'failed';
        }
        u.state.latency = answer.latency;
        u.state.name = canned(u.state.plugin)?.descriptor.name ?? baseName(u.state.plugin.path);
        u.state.state = 'seated';
        emitChains();
        return 'reloaded';
      });
    },
    async move(unit, to) {
      fake.calls.push(['move', unit, to]);
      guard();
      const slot = reserved.get(unit);
      if (slot === undefined) return refuse(`unit ${unit} is in no chain`);
      return onSlot(slot, async () => {
        const u = find(unit);
        if (!u || u.state.state !== 'seated') return refuse(`unit ${unit} is not seated`);
        const others = chains[slot].filter((o) => o !== u);
        const index = Math.max(0, Math.min(Math.trunc(to), others.length));
        // Unit contract §4: a source only heads its chain; an effect never goes ahead of one.
        if (u.state.source && index !== 0) refuse('a source only heads its chain');
        if (!u.state.source && index === 0 && others[0]?.state.source) refuse('an effect cannot go ahead of the source');
        const apply = () => {
          const rest = chains[slot].filter((o) => o !== u);
          rest.splice(index, 0, u);
          chains[slot] = rest;
        };
        // A move changes no latency and no role: never held itself, only behind the slot's held edits.
        let applied: () => void = () => {};
        const done = new Promise<void>((resolve) => (applied = resolve));
        if (heldOrNow(slot, { unit, apply: () => (apply(), applied()) }, false)) return done;
        apply();
        emitChains();
      });
    },
    async openEditor(unit) {
      fake.calls.push(['openEditor', unit]);
      guard();
      const u = seatedPlugin(unit);
      // D29 by unit (unit contract §1): a file's editor belongs to the unit loaded first; a second instance
      // in the same chain is refused its editor.
      const first = chains[u.slot].find((o) => o.state.kind === 'plugin' && o.state.plugin.path === (u.state as { plugin: PluginRef }).plugin.path);
      if (first && first !== u) refuse(`the editor of ${u.state.name} belongs to unit ${first.state.id}`);
    },
    async closeEditor(unit) {
      fake.calls.push(['closeEditor', unit]);
      guard();
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
      seatedPlugin(unit);
      return null; // the canned plugins keep no state
    },
    async importTone(slot, bytes, plugin, unit) {
      fake.calls.push(['importTone', slot, bytes.length, structuredClone(plugin), ...(unit === undefined ? [] : [unit])]);
      guard();
      // Unit contract §8: the tone's own unit first, if seated with its identity; then every other unit of
      // its identity in its slot, in chain order. The UI's two passes pick among them.
      const same = chains[slot].filter((u) => u.state.state === 'seated' && u.state.kind === 'plugin' && sameRef(u.state.plugin, plugin));
      const own = same.find((u) => u.state.id === unit);
      const matches = [...(own ? [own] : []), ...same.filter((u) => u !== own)].map((u) => u.state.id);
      const token = matches.length > 0 ? ++toneTokens : null;
      if (token !== null) parkedTones.add(token);
      return { token, name: canned(plugin)?.descriptor.name ?? baseName(plugin.path), matches };
    },
    async forgetTone(token) {
      fake.calls.push(['forgetTone', token]);
      guard();
      parkedTones.delete(token);
    },
  };

  // ── The batch: bypass and params (unit contract §2, §6) ────────────────────────────────────────────

  const seated = (unit: UnitId): Unit | undefined => {
    const u = find(unit);
    return u && u.state.state === 'seated' ? u : undefined;
  };
  function setBypass(u: Unit, bypass: boolean): void {
    if (u.state.bypass === bypass) return;
    u.state.bypass = bypass;
    scheduleChains();
  }
  function toggleUnits(kind: UnitEffectKindId): void {
    const units = [...chains[0], ...chains[1]].filter((u) => u.state.kind === kind && u.state.state === 'seated');
    if (units.length === 0) return; // none of the kind: nothing, not even a `Toggled`
    const on = !units.some((u) => !u.state.bypass);
    units.forEach((u) => setBypass(u, !on));
    pendingEvents.push({ type: 'Toggled', frame: 0, toggle: { Units: kind }, on });
    scheduleChains();
  }

  /** Apply a batch command the chain owns; false when it is not one. */
  function command(c: EngineCommand): boolean {
    if (typeof c !== 'object') return false;
    const action = 'Action' in c ? c.Action : 'ActionOn' in c ? c.ActionOn[1] : null;
    if ('SetUnitBypass' in c) {
      const u = seated(c.SetUnitBypass[0]);
      if (u) setBypass(u, c.SetUnitBypass[1]);
      else fake.dropped.push(c);
    } else if ('SetUnitParam' in c) {
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
    } else return false;
    return true;
  }

  /** A scripted frame: one carrying `chains` sets the model (a restored rig, a rebuilt engine); a reset
   * frame without it carries the model, as the engine's does; then the held edits are judged again. */
  function scripted(f: FeedFrame): void {
    if (f.chains) {
      reserved.clear();
      sims.clear();
      held[0] = [];
      held[1] = [];
      f.chains.forEach((c, slot) => {
        chains[slot] = c.units.map((state) => ({ state: structuredClone(state), slot: slot as Slot, addedSeq: f.seq }));
        c.units.forEach((u) => reserved.set(u.id, slot as Slot));
      });
      seq = Math.max(seq, f.seq);
      syncTimer();
    } else if (f.reset) f.chains = snapshot();
  }

  // ── The OTT face's meters (unit contract §2, §10) ──────────────────────────────────────────────────

  const metered = (): Unit[] => [...chains[0], ...chains[1]].filter((u) => u.state.kind === 'ott' && u.state.state === 'seated' && !u.state.bypass);
  function ottParams(u: Unit): OttParams {
    const p = u.state.kind === 'ott' ? (u.state.params ?? {}) : {};
    return { depth: p.ottDepth ?? 0.5, time: p.ottTime ?? 0.5, up: p.ottUpward ?? 1, down: p.ottDownward ?? 1 };
  }
  function tick(): void {
    const units = metered();
    if (units.length === 0) return;
    const unitMeters = units.map((u) => {
      const p = ottParams(u);
      let sim = sims.get(u.state.id);
      if (!sim) sims.set(u.state.id, (sim = newOttSim(p)));
      return simFrame(sim, p, u.state.id);
    });
    deps.deliver(frame({ unitMeters }));
  }
  /** Run the timer exactly while it is on and an OTT is seated and on. */
  function syncTimer(): void {
    const want = fake.meterTimer && deps.forced() && metered().length > 0;
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
      const u = seated(unit);
      if (!u) throw new Error(`unit ${unit} is not seated`);
      u.state.state = 'failed';
      u.state.error = reason;
      emitChains();
    },
    reseat(unit) {
      const u = find(unit);
      if (!u || u.state.state !== 'failed') throw new Error(`unit ${unit} is not parked`);
      u.state.state = 'seated';
      delete u.state.error;
      emitChains();
    },
    get meterTimer() {
      return meterTimer;
    },
    set meterTimer(on: boolean) {
      meterTimer = on;
      syncTimer();
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
