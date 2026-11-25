/**
 * OWNS: the rig record's shape and the upgrade of a v0.8.9 profile into it (`docs/unit-contract.md` §1, §6),
 * the one record the page makes: `importRig` (`rig-recall.ts`) hands it to the native record once
 * (`src-tauri/crates/lf-rig` holds the record's rules from then on). Pure: no Solid, no storage, no platform
 * values, explicit `.ts` imports, so `verify/guards/rig-upgrade.mjs` runs it in plain Node. The values the
 * upgrade reads come through the resolvers that read the old keys today (`instrument.ts` `legacyRigSnapshot`,
 * `engine-store.ts` `storedInputSends`, `midi.ts` `bindingsKnown`).
 */
import type { PluginRef, UnitEffectKindId, UnitId, UnitKindId, UnitParamId } from '../../platform'; // type-only: erased, so Node runs this file
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

// ── the record's text ────────────────────────────────────────────────────────────────────────────────

/** The record as the import hands it over (`lf.rig` v1's text, D12). */
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

/** One slot's old keys, each read through its resolver (`instrument.ts` `legacyRigSnapshot`). */
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

/** A slot's volume as the upgrade derives it from its old keys: the legacy recall's
 * plugin's saved level, else the scan kind's default (decision f); else unity for a SYNTH, the input level
 * for Off. `instrument.ts` shows it on the slot's fader until the native record answers. */
export function legacySlotVolume(legacy: LegacySlot, guitarFirst: boolean, legacyMarker: boolean): number {
  if (legacy.plugin && !legacyMarker) return legacy.plugin.stored ?? defaultPluginLevel(legacy.plugin.isEffect);
  const source = storedSlotSource(legacy.savedSource, guitarFirst) ?? legacy.defaultSynth;
  return source === 'off' || source === 'unset' ? legacy.inputGain : 1;
}

/**
 * The record a v0.8.9 profile becomes (§6), imported whole (`importRig`; native restores it). Per slot: the legacy
 * recall's plugin as a plugin unit (dropped when the legacy marker is stored), volume its saved level, else
 * the scan kind's default (decision f); else its source as `lf.slotSource.<n>` resolves: a SYNTH unit with
 * the instrument's index and every instrument's level, volume unity, or Off: no unit, volume the input
 * level (what a DEV chain already holds is native's to adopt, D12). `pending` takes each send that migrates,
 * in `SEND_ORDER`, its params as stored, bypassed when it was off; a SYNTH-headed slot only the OTT (native's
 * placement keeps it only if it was on). Ids come from
 * `mint`, slot 0's units then its pending, then slot 1's.
 */
export function upgradeRig(snap: LegacySnapshot, mint: () => UnitId): RigRecord {
  const slot = (n: 0 | 1): RigSlot => {
    const legacy = snap.slots[n];
    const volume = legacySlotVolume(legacy, snap.guitarFirst, snap.legacyMarker);
    let units: RigUnit[];
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
