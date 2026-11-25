/**
 * OWNS: the rig record's shape and its pure rules (`docs/unit-contract.md` §1, §6): `lf.rig` as JSON
 * (parse, tolerant; serialize), the upgrade of a v0.8.9 profile into it, the placement of a slot's
 * `pending` once its head's role is known, and the record's units from a `chains` report. Pure: no Solid,
 * no storage, no platform values, explicit `.ts` imports, so `verify/guards/rig-upgrade.mjs` runs it in
 * plain Node. The storage, the restore and the writer around it: `rig-recall.ts`; the values the upgrade
 * reads come through the resolvers that read the old keys today (`instrument.ts` `legacyRigSlot`,
 * `engine-store.ts` `storedInputSends`, `midi.ts` `bindingsKnown`).
 */
import type { PluginRef, UnitEffectKindId, UnitId, UnitKindId, UnitParamId, UnitSpec, UnitState } from '../../platform'; // type-only: erased, so Node runs this file
import { isPluginFormat } from './plugin-format.ts';
import { SYNTHS } from './instruments.ts';

/** A slot's volume range (the lane faders' and the plugin output's): unity at 1. */
export const SLOT_GAIN_MAX = 1.5;

/** A plugin's output level before the player sets one, by its scan kind: an amp-sim/FX is already
 * gain-staged, so its wet wants ~unity; a synth's default patches clip, so it starts conservative, and so
 * does a plugin the scan did not classify or did not find. */
export const defaultPluginLevel = (isEffect: boolean | null): number => (isEffect === true ? 0.9 : 0.1);

/** A plugin as the record keeps it: its identity and its display name (§8's rig carries it), for logs. */
export type RigPlugin = PluginRef & { name?: string };
/** A unit as the record keeps it: what `add` restores, under its id (unit contract §2 `UnitRecord`). */
export type RigUnit =
  | { id: UnitId; kind: 'plugin'; plugin: RigPlugin; bypass?: boolean }
  | { id: UnitId; kind: Exclude<UnitKindId, 'plugin'>; params?: Partial<Record<UnitParamId, number>>; bypass?: boolean };
export interface RigSlot {
  /** The slot volume after the chain, 0..SLOT_GAIN_MAX. */
  volume: number;
  /** The chain in order (§1). */
  units: RigUnit[];
  /** §6: the old sends' units still to place, ids minted, values read; absent: none. */
  pending?: RigUnit[];
}
export interface RigRecord {
  version: 1;
  slots: [RigSlot, RigSlot];
}

// ── parse and serialize ──────────────────────────────────────────────────────────────────────────────

const KINDS: Record<UnitKindId, true> = { plugin: true, ott: true, echo: true, reverb: true, ring: true, synth: true };
const PARAMS: Record<UnitParamId, true> = {
  echoTime: true, echoFeedback: true, echoLevel: true, reverbLevel: true, ringFreq: true, ringLevel: true,
  ottDepth: true, ottTime: true, ottUpward: true, ottDownward: true,
  instrument: true, gainLead: true, gainBass: true, gainPad: true, gainPiano: true, gainOrgan: true, gainDrum: true,
};
const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
const isUnitId = (v: unknown): v is UnitId => Number.isSafeInteger(v) && (v as number) >= 1 && (v as number) <= 0xffff_ffff;

/** One stored unit, or null when it cannot be read; a param it does not know is skipped (§8's reader rule). */
function readUnit(v: unknown): RigUnit | null {
  if (!isObj(v) || !isUnitId(v.id) || typeof v.kind !== 'string' || !Object.hasOwn(KINDS, v.kind)) return null;
  if (v.bypass !== undefined && typeof v.bypass !== 'boolean') return null;
  const bypass = v.bypass === undefined ? {} : { bypass: v.bypass };
  if (v.kind === 'plugin') {
    const p = v.plugin;
    if (!isObj(p) || !isPluginFormat(p.format) || typeof p.path !== 'string' || typeof p.id !== 'string') return null;
    const name = typeof p.name === 'string' ? { name: p.name } : {};
    return { id: v.id, kind: 'plugin', plugin: { format: p.format, path: p.path, id: p.id, ...name }, ...bypass };
  }
  const kind = v.kind as Exclude<UnitKindId, 'plugin'>;
  if (v.params === undefined) return { id: v.id, kind, ...bypass };
  if (!isObj(v.params)) return null;
  const params: Partial<Record<UnitParamId, number>> = {};
  for (const [k, x] of Object.entries(v.params)) {
    if (Object.hasOwn(PARAMS, k) && typeof x === 'number' && Number.isFinite(x)) params[k as UnitParamId] = x;
  }
  return { id: v.id, kind, params, ...bypass };
}

/** A stored unit list: unreadable entries and an id met before are skipped. */
function readUnits(v: unknown, ids: Set<UnitId>): RigUnit[] {
  if (!Array.isArray(v)) return [];
  const out: RigUnit[] = [];
  for (const raw of v) {
    const u = readUnit(raw);
    if (!u || ids.has(u.id)) continue;
    ids.add(u.id);
    out.push(u);
  }
  return out;
}

/** `lf.rig`'s text as a record; null (absent) when there is none or it cannot be read as one. An unreadable
 * unit is skipped, a volume out of range reads unity. */
export function parseRig(raw: string | null): RigRecord | null {
  if (raw === null) return null;
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isObj(v) || v.version !== 1 || !Array.isArray(v.slots) || v.slots.length !== 2) return null;
  if (!v.slots.every(isObj)) return null;
  const ids = new Set<UnitId>();
  const slot = (s: Record<string, unknown>): RigSlot => {
    const volume = typeof s.volume === 'number' && s.volume >= 0 && s.volume <= SLOT_GAIN_MAX ? s.volume : 1;
    const units = readUnits(s.units, ids);
    const pending = readUnits(s.pending, ids);
    return pending.length > 0 ? { volume, units, pending } : { volume, units };
  };
  const [a, b] = v.slots as Record<string, unknown>[];
  return { version: 1, slots: [slot(a), slot(b)] };
}

/** The record as `lf.rig` stores it. */
export function serializeRig(r: RigRecord): string {
  const slot = (s: RigSlot): RigSlot =>
    s.pending && s.pending.length > 0 ? { volume: s.volume, units: s.units, pending: s.pending } : { volume: s.volume, units: s.units };
  return JSON.stringify({ version: 1, slots: [slot(r.slots[0]), slot(r.slots[1])] });
}

// ── the upgrade from a v0.8.9 profile (§6) ───────────────────────────────────────────────────────────

/** A slot's stored source as `instrument.ts` resolves `lf.slotSource.<n>` at load: 'off', 'unset' (no key on
 * a guitar-first install: Off, nobody's pick), a built-in's id, or null for the slot's default synth. */
export function storedSlotSource(saved: string | null, guitarFirst: boolean): 'off' | 'unset' | string | null {
  if (saved === 'off') return 'off';
  if (saved !== null && SYNTHS.some((s) => s.id === saved)) return saved;
  return saved === null && guitarFirst ? 'unset' : null;
}

/** One stored send as its resolvers read it (`engine-store.ts` `storedInputSends`). */
export interface StoredSend {
  on: boolean;
  params: Partial<Record<UnitParamId, number>>;
  defaults: Partial<Record<UnitParamId, number>>;
}

/** One slot's old keys, each read through its resolver (`instrument.ts` `legacyRigSlot`). */
export interface LegacySlot {
  /** `lf.rigRecall`'s plugin for the slot, with what this launch's scan of it says. */
  plugin: {
    ref: RigPlugin;
    /** The scan's kind when it found the plugin; null when it did not, or could not classify it. */
    isEffect: boolean | null;
    /** `lf.pluginGain.<n>.<key>`, when stored and in range. */
    stored: number | null;
  } | null;
  /** `lf.slotSource.<n>` as stored (null: no key). */
  savedSource: string | null;
  /** The slot's synth when nothing is stored ('lead' in A, 'bass' in B). */
  defaultSynth: string;
  /** `lf.slotInputGain.<n>` as its resolver reads it. */
  inputGain: number;
}

/** What the upgrade reads. */
export interface LegacySnapshot {
  slots: [LegacySlot, LegacySlot];
  /** The install opened guitar-first (`guitarFirstInstall`): an unset source is Off. */
  guitarFirst: boolean;
  /** Each instrument's level, `lf.synthGain.<id>` as its resolver reads it, by `SYNTHS` id. */
  synthGains: Readonly<Record<string, number>>;
  sends: Record<UnitEffectKindId, StoredSend>;
  /** Which of the `InFx*` actions a stored binding runs (`boundKinds`). */
  bound: Record<UnitEffectKindId, boolean>;
  /** The legacy recall's crash marker (`lf.rigRecallInFlight`) is stored: v0.8.9 would forget its plugins. */
  legacyMarker: boolean;
  /** The chains the first reset frame reported, when any unit was in them (a DEV chain run before any
   * record): the record adopts them instead of the old plugin and source. */
  reported: [RigUnit[], RigUnit[]] | null;
  /** Each slot's gain the engine applied, as the first reset frame's settings replay it (`SetSlotGain`);
   * null where it holds none. The adopted chains keep it; absent or null: unity. */
  reportedVolumes?: [number | null, number | null];
}

const IN_FX: Record<string, UnitEffectKindId> = { inFxOtt: 'ott', inFxRing: 'ring', inFxEcho: 'echo', inFxReverb: 'reverb' };

/** Which built-in effect kinds a stored binding's action switches, from native MIDI's first `bindings`
 * report; none came (null): every kind counts as bound (seat decision c: a wrong guess costs one silent
 * bypassed unit, never a lost pedal). */
export function boundKinds(bindings: readonly { binding: { action: string } }[] | null): Record<UnitEffectKindId, boolean> {
  const bound = { ott: bindings === null, ring: bindings === null, echo: bindings === null, reverb: bindings === null };
  for (const b of bindings ?? []) {
    const kind = IN_FX[b.binding.action];
    if (kind) bound[kind] = true;
  }
  return bound;
}

/** The order the old sends are placed in, after the chain's plugin (§6): the OTT (dev only), RING MOD,
 * ECHO, REVERB. */
export const SEND_ORDER: readonly UnitEffectKindId[] = ['ott', 'ring', 'echo', 'reverb'];

const atDefaults = (s: StoredSend): boolean =>
  (Object.keys(s.defaults) as UnitParamId[]).every((k) => s.params[k] === s.defaults[k]);

/** A send becomes a unit unless it is off, at its defaults and bound to no pedal (§6). */
const migrates = (kind: UnitEffectKindId, snap: LegacySnapshot): boolean =>
  snap.sends[kind].on || !atDefaults(snap.sends[kind]) || snap.bound[kind];

const SYNTH_GAIN: Record<string, UnitParamId> = {
  lead: 'gainLead', bass: 'gainBass', pad: 'gainPad', piano: 'gainPiano', organ: 'gainOrgan', drum: 'gainDrum',
};

/** A slot's volume as the upgrade derives it from its old keys (no adopted chain): the legacy recall's
 * plugin's saved level, else the scan kind's default (decision f); else unity for a SYNTH, the input level
 * for Off. `instrument.ts` shows it on the slot's fader from the first page of a launch with no record yet. */
export function legacySlotVolume(legacy: LegacySlot, guitarFirst: boolean, legacyMarker: boolean): number {
  if (legacy.plugin && !legacyMarker) return legacy.plugin.stored ?? defaultPluginLevel(legacy.plugin.isEffect);
  const source = storedSlotSource(legacy.savedSource, guitarFirst) ?? legacy.defaultSynth;
  return source === 'off' || source === 'unset' ? legacy.inputGain : 1;
}

/**
 * The record a v0.8.9 profile becomes (§6), written whole before the first load. Per slot: the legacy
 * recall's plugin as a plugin unit (dropped when the legacy marker is stored), volume its saved level, else
 * the scan kind's default (decision f); else its source as `lf.slotSource.<n>` resolves: a SYNTH unit with
 * the instrument's index and every instrument's level, volume unity, or Off: no unit, volume the input
 * level. When the first reset frame reported units, the record adopts those chains as they are instead,
 * at the slot gain the engine reported (unity when it reported none). `pending` takes each send that migrates, in `SEND_ORDER`, its params as stored, bypassed when
 * it was off; a SYNTH-headed slot only the OTT (placement keeps it only if it was on). Ids come from
 * `mint`, slot 0's units then its pending, then slot 1's.
 */
export function upgradeRig(snap: LegacySnapshot, mint: () => UnitId): RigRecord {
  const slot = (n: 0 | 1): RigSlot => {
    const legacy = snap.slots[n];
    let units: RigUnit[];
    let volume: number;
    if (snap.reported) {
      units = snap.reported[n];
      const kept = snap.reportedVolumes?.[n];
      volume = typeof kept === 'number' && kept >= 0 && kept <= SLOT_GAIN_MAX ? kept : 1;
    } else {
      volume = legacySlotVolume(legacy, snap.guitarFirst, snap.legacyMarker);
      if (legacy.plugin && !snap.legacyMarker) {
        units = [{ id: mint(), kind: 'plugin', plugin: { ...legacy.plugin.ref }, bypass: false }];
      } else {
        const source = storedSlotSource(legacy.savedSource, snap.guitarFirst) ?? legacy.defaultSynth;
        if (source === 'off' || source === 'unset') units = [];
        else {
          const params: Partial<Record<UnitParamId, number>> = { instrument: SYNTHS.findIndex((s) => s.id === source) };
          for (const s of SYNTHS) params[SYNTH_GAIN[s.id]] = snap.synthGains[s.id] ?? 1;
          units = [{ id: mint(), kind: 'synth', params, bypass: false }];
        }
      }
    }
    const synthHead = units[0]?.kind === 'synth';
    const pending: RigUnit[] = SEND_ORDER.filter((k) => migrates(k, snap) && (!synthHead || k === 'ott')).map((kind) => ({
      id: mint(),
      kind,
      params: { ...snap.sends[kind].params },
      bypass: !snap.sends[kind].on,
    }));
    return pending.length > 0 ? { volume, units, pending } : { volume, units };
  };
  const a = slot(0);
  return { version: 1, slots: [a, slot(1)] };
}

// ── placement and the writer (§1, §6) ────────────────────────────────────────────────────────────────

/** Whether `u` is applied in its chain: what sets the chain's role (seated, on its way out, parked in its
 * seat, or reloading: its old instance plays on until the new one lands, unit contract §3). A unit still
 * loading or waiting for its add is not, yet. */
export const isApplied = (u: UnitState): boolean =>
  u.state === 'seated' || u.state === 'leaving' || u.reloading === true || (u.state === 'failed' && u.parked === true);

/**
 * Slot's head's role as `reported` (the slot's chain in a `chains` report taken now) shows it: 'input'
 * with no head or a built-in effect at the head, 'source' with a SYNTH; a plugin head's role once it is
 * applied (its `source`). Null while it is unknown: the plugin head is missing from the scan, loading,
 * waiting, failed with no seat (its cause unknown, so the slot is not known to be input-fed), or its add's
 * outcome not reported yet.
 */
export function headRole(units: readonly RigUnit[], reported: readonly UnitState[]): 'source' | 'input' | null {
  const head = units[0];
  if (!head) return 'input';
  if (head.kind === 'synth') return 'source';
  if (head.kind !== 'plugin') return 'input';
  const now = reported.find((u) => u.id === head.id);
  return now && isApplied(now) ? (now.source ? 'source' : 'input') : null;
}

/** The units of `slot.pending` its chain takes for its head's `role` (§6): an input-fed chain all of them,
 * a source-headed one only an OTT that was on. */
export function placed(slot: RigSlot, role: 'source' | 'input'): RigUnit[] {
  const pending = slot.pending ?? [];
  return role === 'input' ? pending : pending.filter((u) => u.kind === 'ott' && u.bypass !== true);
}

/** The slot once its `pending` is placed: the units its role takes appended at the chain's end, `pending`
 * gone. */
export function withPlaced(slot: RigSlot, role: 'source' | 'input'): RigSlot {
  return { volume: slot.volume, units: [...slot.units, ...placed(slot, role)] };
}

/** A reported unit as the record keeps it. */
function recordOf(u: UnitState): RigUnit {
  const bypass = u.bypass === true;
  if (u.kind === 'plugin') return { id: u.id, kind: 'plugin', plugin: { format: u.plugin.format, path: u.plugin.path, id: u.plugin.id, name: u.name }, bypass };
  return { id: u.id, kind: u.kind, ...(u.params ? { params: { ...u.params } } : {}), bypass };
}

/**
 * A slot's units after a `chains` report (§1): every reported unit but one `leaving` (a confirmed removal), in
 * its order, a failed add's plate included: the feed names no failure's cause (a plugin's own load and a host
 * failure, a lost device or an owner thread that would not start, share `failed`), so the plate keeps its id
 * and place, and the next launch adds it again, until the player removes it. And every record unit this
 * page's reports never named (`seen`), at its record place: a plugin missing from the scan, an add still on
 * its way. A unit a report named before and this one does not was removed.
 */
export function unitsFromReport(record: readonly RigUnit[], reported: readonly UnitState[], seen: ReadonlySet<UnitId>): RigUnit[] {
  const out = reported.filter((u) => u.state !== 'leaving').map(recordOf);
  const named = new Set(reported.map((u) => u.id));
  record.forEach((u, i) => {
    if (!named.has(u.id) && !seen.has(u.id)) out.splice(Math.min(i, out.length), 0, u);
  });
  return out;
}

/** The spec `add` takes for a record unit: the plugin's name stays the record's, off the wire. */
export function addSpec(u: RigUnit): UnitSpec {
  const bypass = u.bypass === true;
  if (u.kind === 'plugin') return { kind: 'plugin', plugin: { format: u.plugin.format, path: u.plugin.path, id: u.plugin.id }, bypass };
  return { kind: u.kind, ...(u.params ? { params: { ...u.params } } : {}), bypass };
}
