/**
 * OWNS: rig recall — what each slot held when the app last ran, restored at launch, and the in-flight
 * marker that keeps a plugin which crashes or hangs the host at load from taking down every later launch.
 * Never an arm is kept, so GO LIVE stays one press. A plugin's tone comes back with it on the engine: the
 * native host keeps each plugin's state in its tone store and restores it inside every load
 * (`src-tauri/src/host/tone.rs`). Two modes, one per page (`platform.chain.available`):
 *
 * - **Legacy** (the release): `lf.rigRecall`, each slot's one plugin, restored through the normal load path
 *   (`recallRig`), marker `lf.rigRecallInFlight`. What the slot plays without its plugin (a synth, or Off)
 *   and the slot levels are kept by `instrument.ts`; each slot's input channel by `audio-settings.ts`. The
 *   record follows the slot: `instrument.ts` remembers a plugin when its load succeeds and forgets it when
 *   the slot's plugin unloads, a load into the slot fails or a synth or Off is picked for it. A WebView
 *   reload keeps the record (`resyncNativeSlots` unloads the stranded plugins without forgetting them), so
 *   the recall brings them back.
 * - **Chain** (DEV only until activation, `docs/unit-contract.md` §1, §6): `lf.rig`, each slot's volume,
 *   its units in order and the old sends' units still to place (`rig-record.ts` holds the shape and its
 *   rules), marker `lf.rigInFlight`, never the legacy one, so a DEV crash never makes legacy mode forget its
 *   rig. `recallChains`: the upgrade of a v0.8.9 profile written whole before the first add, the restore of
 *   each record unit the reset frame's `chains` lacks, the settle, then each slot's `pending` placed once
 *   its head's role is known. One writer (`updateRig`) holds every write after the upgrade: the units from
 *   each `chains` report once the restore finished (`rigChainsReported`), a placement, the slot volume
 *   (`keepRigVolume`, at any time).
 */
import type { ChainState, PluginDescriptor, UnitId, UnitSpec } from '../../platform';
import { notifyError } from '../../notify';
import { samePluginDescriptor } from './plugin-descriptor';
import { isPluginFormat } from './plugin-format';
import { withAt } from './instrument-slots';
import {
  addSpec,
  headRole,
  parseRig,
  placed,
  serializeRig,
  unitsFromReport,
  upgradeRig,
  withPlaced,
  type LegacySnapshot,
  type RigRecord,
  type RigSlot,
  type RigUnit,
} from './rig-record';

// Where the record and the marker live; null turns rig recall off. A DEV native probe (`VITE_LF_PROBE`)
// starts from empty slots and stores nothing: a run killed mid-probe would hand its next run the
// plugin that wedged it. Only the recall's own probes (`src/debug/recall-restart.ts`,
// `src/debug/tone-recall.ts`) recall, each from keys of its own, so an agent's run never touches the rig
// the owner's dev sessions restore.
const PROBE = import.meta.env.VITE_LF_PROBE as string | undefined;
const RECALL_PROBES = ['recall-restart', 'tone-recall'];
const PREFIX = !PROBE ? 'lf.' : RECALL_PROBES.includes(PROBE) ? `lf.probe.${PROBE}.` : null;
const KEYS =
  PREFIX === null
    ? null
    : { record: `${PREFIX}rigRecall`, marker: `${PREFIX}rigRecallInFlight`, rig: `${PREFIX}rig`, chainMarker: `${PREFIX}rigInFlight` };
/** How long the marker outlives the last recalled load: a plugin that takes the host down in its
 * first moments of processing is a crash at load too. */
const SETTLE_MS = 3000;

type SavedPlugin = Pick<PluginDescriptor, 'format' | 'path' | 'id' | 'name'>;
/** The legacy record: each slot's one plugin. */
export type SavedRig = [SavedPlugin | null, SavedPlugin | null];

function isSavedPlugin(v: unknown): v is SavedPlugin {
  if (typeof v !== 'object' || v === null) return false;
  const p = v as Partial<Record<keyof SavedPlugin, unknown>>;
  return (
    isPluginFormat(p.format) &&
    typeof p.path === 'string' &&
    typeof p.id === 'string' &&
    typeof p.name === 'string'
  );
}

function readRig(): SavedRig {
  if (!KEYS) return [null, null];
  try {
    const raw = localStorage.getItem(KEYS.record);
    const parsed: unknown = raw === null ? null : JSON.parse(raw);
    if (Array.isArray(parsed) && parsed.length === 2) {
      return [isSavedPlugin(parsed[0]) ? parsed[0] : null, isSavedPlugin(parsed[1]) ? parsed[1] : null];
    }
  } catch {
    /* unreadable → nothing to restore */
  }
  return [null, null];
}

function writeRig(rig: SavedRig): void {
  if (!KEYS) return;
  try {
    localStorage.setItem(KEYS.record, JSON.stringify(rig));
  } catch {
    /* persistence is best-effort */
  }
}

const label = (p: SavedPlugin) => `${p.name} (${p.format.toUpperCase()})`;

/** The bundle paths of the plugins the rig remembers, for boot's scan of them alone
 * (`PluginHost.scanPluginPaths`) ahead of the full scan. */
export function savedRigPaths(): string[] {
  return [...new Set(readRig().flatMap((p) => (p ? [p.path] : [])))];
}

/** The legacy record as stored (the chain upgrade reads it). */
export const savedRig = (): SavedRig => readRig();

/** Remember `desc` as slot `slot`'s plugin (its load succeeded). */
export function rememberSlotPlugin(slot: 0 | 1, desc: PluginDescriptor): void {
  const { format, path, id, name } = desc;
  writeRig(withAt(readRig(), slot, { format, path, id, name }));
}

/** Forget slot `slot`'s plugin (it unloaded, a load into the slot failed, or a synth or Off was picked for it). */
export function forgetSlotPlugin(slot: 0 | 1): void {
  const rig = readRig();
  if (rig[slot]) writeRig(withAt(rig, slot, null));
}

/** Whether a recall's marker is stored right now (the DEV restart probe watches for it). */
export function recallInFlight(): boolean {
  if (!KEYS) return false;
  try {
    return localStorage.getItem(KEYS.marker) !== null;
  } catch {
    return false;
  }
}

function removeMarker(key = KEYS?.marker): void {
  if (key === undefined) return;
  try {
    localStorage.removeItem(key);
  } catch {
    /* best-effort; a leftover marker only skips the next recall */
  }
}

/** The marker of the settle window running now (legacy's or the chain's), or null. */
let settling: string | null = null;
let finished = false;
let finish!: () => void;
const done = new Promise<void>((resolve) => (finish = () => ((finished = true), resolve())));
/** This launch's recall has finished, the settle window included (the probes await it). Settles on every
 * launch whose device opens in chain mode; never on one that runs no recall (no device, or legacy mode
 * without a plugin host). */
export function rigRecallDone(): Promise<void> {
  return done;
}
/** Whether `rigRecallDone` has settled: the synchronous read the DEV recall probes poll. */
export function rigRecallFinished(): boolean {
  return finished;
}

/**
 * The app is closing through its close button (`close-guard.ts`, just before it approves the close).
 * Once this launch's recalled loads have all come back, the host has lived through them: the marker
 * goes now rather than at the end of the settle window, so a close right after a restore brings the
 * rig back at the next launch. A close while a recalled load still runs keeps it, since a load that
 * hangs the host looks just like that.
 */
export function rigRecallOnClose(): void {
  if (settling !== null) removeMarker(settling);
}

/**
 * Restore each slot's remembered plugin, slot 0 then slot 1, through `load` (`restorePlugin`: the slot
 * chain, the load token and the routing rules all apply, and a slot the player has already chosen a
 * source for keeps that choice), from this launch's scan `available`. Boot runs it once, on the scan of
 * the saved plugins' own paths (`savedRigPaths`), beside the full scan. A slot whose plugin is missing from the scan (file gone, plugin changed) is skipped with one
 * log line and keeps its record for a launch where the plugin is back. A failed load reports through
 * the load path's own toast.
 *
 * The marker is stored before the first load and removed SETTLE_MS after the last, or at a close
 * through the close button once the loads are back (`rigRecallOnClose`). Found at the next launch, it
 * means that launch died or was stopped while restoring: nothing is restored, the whole record is
 * forgotten, and one log line plus one toast say so. A close while a load still runs, or a WebView
 * reload inside the window, counts as such a stop.
 */
export async function recallRig(
  available: readonly PluginDescriptor[],
  load: (slot: 0 | 1, desc: PluginDescriptor) => Promise<void>,
): Promise<void> {
  try {
    if (!KEYS) return;
    let leftover: string | null;
    try {
      leftover = localStorage.getItem(KEYS.marker);
    } catch {
      return; // no storage: nothing was remembered either
    }
    if (leftover !== null) {
      writeRig([null, null]);
      removeMarker();
      console.error(`[rig-recall] the last launch stopped while restoring ${leftover}; nothing restored, the slots are forgotten`);
      notifyError('Could not restore the plugins. Pick a plugin again to load it.', `The last launch stopped while restoring ${leftover}.`);
      return;
    }
    const rig = readRig();
    const plan: [0 | 1, PluginDescriptor][] = [];
    for (const slot of [0, 1] as const) {
      const saved = rig[slot];
      if (!saved) continue;
      const desc = available.find((d) => samePluginDescriptor(d, saved));
      if (desc) plan.push([slot, desc]);
      else {
        console.error(
          `[rig-recall] slot ${slot + 1}: ${label(saved)} at ${saved.path} is not in this launch's plugin scan (file missing or plugin changed); left empty`,
        );
      }
    }
    if (!plan.length) return;
    try {
      localStorage.setItem(KEYS.marker, plan.map(([, d]) => label(d)).join(', '));
    } catch (e) {
      console.error('[rig-recall] could not store the in-flight marker; nothing restored', e);
      return;
    }
    for (const [slot, desc] of plan) await load(slot, desc);
    settling = KEYS.marker;
    await new Promise((resolve) => setTimeout(resolve, SETTLE_MS));
    removeMarker();
  } finally {
    settling = null;
    finish();
  }
}

// ── Chain mode: the rig record `lf.rig` (unit contract §1, §6) ───────────────────────────────────────

/** How long the restore waits for the reset frame's `chains` when none came yet: a frame without them
 * counts as empty chains. */
const REPORT_WAIT_MS = 5000;

/** The rig record as stored; null when there is none or it cannot be read (the upgrade then runs). */
export function readRigRecord(): RigRecord | null {
  if (!KEYS) return null;
  try {
    return parseRig(localStorage.getItem(KEYS.rig));
  } catch {
    return null;
  }
}

/** Each slot's kept volume, for `instrument.ts`' chain gains at load; null without a record. */
export function rigVolumes(): [number, number] | null {
  const r = readRigRecord();
  return r && [r.slots[0].volume, r.slots[1].volume];
}

/** Store `r` whole and read it back: true only when the stored text is `r`'s (review finding 2: a
 * durable step's adds wait for this). */
function storeRig(r: RigRecord): boolean {
  if (!KEYS) return false;
  const text = serializeRig(r);
  try {
    localStorage.setItem(KEYS.rig, text);
    return localStorage.getItem(KEYS.rig) === text;
  } catch {
    return false;
  }
}

const withSlot = (r: RigRecord, slot: 0 | 1, s: RigSlot): RigRecord => ({ version: 1, slots: withAt(r.slots, slot, s) });

/**
 * THE record writer once a record exists: read, change, store, in one step, so a volume change never
 * overwrites newer units or pending. No record (before the upgrade, or it cannot be read): nothing is
 * written, so the upgrade still runs. True when the change is stored (or changed nothing).
 */
function updateRig(change: (r: RigRecord) => RigRecord): boolean {
  const now = readRigRecord();
  if (!now) return false;
  const next = change(now);
  return serializeRig(next) === serializeRig(now) || storeRig(next);
}

/** Keep slot `slot`'s volume (the slot level's setter, at any time, a restore running included). */
export function keepRigVolume(slot: 0 | 1, volume: number): void {
  updateRig((r) => withSlot(r, slot, { ...r.slots[slot], volume }));
}

/** What the chain restore needs from the modules that own it (`src/app/boot.ts` wires them). */
export interface ChainRecallDeps {
  /** This launch's scan of `paths` (the record's or the legacy recall's plugins), the full scan when it fails. */
  scan(paths: string[]): Promise<readonly PluginDescriptor[]>;
  /** The old keys through their resolvers, `scan` saying which legacy plugins are found. */
  snapshot(scan: readonly PluginDescriptor[], legacy: SavedRig): Promise<Omit<LegacySnapshot, 'legacyMarker' | 'reported'>>;
  /** Apply slot `slot`'s volume: the slot level as shown and the engine's `SetSlotGain`. */
  setVolume(slot: 0 | 1, volume: number): void;
  /** The chain host's `add`. */
  add(slot: 0 | 1, unit: UnitId, spec: UnitSpec, at: number): Promise<unknown>;
  /** A new unit id (`mintUnitId`). */
  mint(): UnitId;
}

// The `chains` reports this page has seen: the latest, and every id any of them named (a record unit
// never named is still on its way or was never added; one named before and gone since was removed).
let reported: [ChainState, ChainState] | null = null;
const seen = new Set<UnitId>();
const reportWaiters: (() => void)[] = [];
/** On once this launch's chain restore has finished: each report then writes the record's units. */
let writing = false;
let deps: ChainRecallDeps | null = null;
const placing: [boolean, boolean] = [false, false];
/** A placement whose durable write failed waits for the next launch. */
const placementFailed: [boolean, boolean] = [false, false];

/** The feed's `chains` in chain mode (`chain.ts` `applyChains`): remembered, written once the restore has
 * finished, and each slot's `pending` placed once its head's role is known. */
export function rigChainsReported(next: [ChainState, ChainState]): void {
  reported = next;
  for (const c of next) for (const u of c.units) seen.add(u.id);
  reportWaiters.splice(0).forEach((wake) => wake());
  if (!writing) return;
  writeUnits();
  placePending();
}

function writeUnits(): void {
  const now = reported;
  if (!now) return;
  updateRig((r) => ({
    version: 1,
    slots: [
      { ...r.slots[0], units: unitsFromReport(r.slots[0].units, now[0].units, seen) },
      { ...r.slots[1], units: unitsFromReport(r.slots[1].units, now[1].units, seen) },
    ],
  }));
}

/** The latest `chains`, waiting for the first (a reset frame carries them) at most REPORT_WAIT_MS. */
async function firstChains(): Promise<[ChainState, ChainState]> {
  if (!reported) await new Promise<void>((wake) => {
    reportWaiters.push(wake);
    setTimeout(wake, REPORT_WAIT_MS);
  });
  const empty: ChainState = { units: [], latency: 0, held: false };
  return reported ?? [empty, empty];
}

const unitLabel = (u: RigUnit): string =>
  u.kind === 'plugin' ? `${u.plugin.name ?? u.plugin.path.split(/[\\/]/).pop()} (${u.plugin.format.toUpperCase()})` : u.kind.toUpperCase();

function storedMarker(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** `mint`, never answering an id already `taken` (a reported unit, a record unit) or one it gave before. */
function freshMint(mint: () => UnitId, taken: Set<UnitId>): () => UnitId {
  return () => {
    for (let i = 0; i < 64; i++) {
      const id = mint();
      if (!taken.has(id)) {
        taken.add(id);
        return id;
      }
    }
    throw new Error('no fresh unit id after 64 tries');
  };
}

/**
 * Chain mode's recall, once per launch (`src/app/boot.ts`): no record yet → the upgrade (§6) from this
 * launch's scan of the legacy recall's plugins and the old keys, adopting the reported chains instead when
 * the first reset frame already had units (a DEV chain run before any record); stored whole and read back
 * before anything is added, else nothing is restored and the next launch upgrades again. Each slot's
 * volume applied. A leftover marker with none of the record's plugin units reported means the last launch
 * died restoring: the record forgets its plugin units only (one log line, one toast), and the slots'
 * `pending` then place. With some reported, a WebView reload cut the last restore short: it goes on, and
 * the marker stays until a settle completes. Then each record unit the reported chains lack is added at its
 * index under its id, slot 0 then slot 1, one at a time; a plugin missing from the scan is skipped with one
 * log line and keeps its entry; a failed add's plate is the chain's to show, and the record drops it at the
 * next write (a confirmed failure; a rejection with no plate keeps it for the next launch). The marker is
 * stored before the first plugin add and removed SETTLE_MS after the last (or at a close through the close
 * button meanwhile, `rigRecallOnClose`). Nothing writes the record's units meanwhile; from then on each
 * `chains` report does, and a slot's `pending` is placed by a durable write before its adds.
 */
export async function recallChains(d: ChainRecallDeps): Promise<void> {
  try {
    if (!KEYS) return;
    deps = d;
    const chains = await firstChains();
    const reportedIds = new Set(chains.flatMap((c) => c.units.map((u) => u.id)));
    let record = readRigRecord();
    let scan: readonly PluginDescriptor[] | null = null;
    if (!record) {
      const legacy = readRig();
      scan = await d.scan([...new Set(legacy.flatMap((p) => (p ? [p.path] : [])))]);
      const base = await d.snapshot(scan, legacy);
      const adopted = chains.some((c) => c.units.length > 0)
        ? ([unitsFromReport([], chains[0].units, seen), unitsFromReport([], chains[1].units, seen)] as [RigUnit[], RigUnit[]])
        : null;
      const taken = new Set(reportedIds);
      record = upgradeRig({ ...base, legacyMarker: storedMarker(KEYS.marker) !== null, reported: adopted }, freshMint(d.mint, taken));
      if (!storeRig(record)) {
        console.error('[rig-recall] could not store the rig record; nothing restored, the upgrade runs again at the next launch');
        return;
      }
    }
    for (const slot of [0, 1] as const) d.setVolume(slot, record.slots[slot].volume);

    const leftover = storedMarker(KEYS.chainMarker);
    const plugins = record.slots.flatMap((s) => s.units.filter((u) => u.kind === 'plugin'));
    if (leftover !== null && !plugins.some((u) => reportedIds.has(u.id))) {
      const noPlugins = (s: RigSlot): RigSlot => ({ ...s, units: s.units.filter((u) => u.kind !== 'plugin') });
      const forgotten: RigRecord = { version: 1, slots: [noPlugins(record.slots[0]), noPlugins(record.slots[1])] };
      if (!storeRig(forgotten)) {
        console.error('[rig-recall] could not store the rig record without its plugins; nothing restored');
        return;
      }
      record = forgotten;
      removeMarker(KEYS.chainMarker);
      console.error(`[rig-recall] the last launch stopped while restoring ${leftover}; its plugins are forgotten`);
      notifyError('Could not restore the plugins. Add a plugin again to load it.', `The last launch stopped while restoring ${leftover}.`);
    }

    const wanted = [
      ...new Set(record.slots.flatMap((s) => s.units.flatMap((u) => (u.kind === 'plugin' && !reportedIds.has(u.id) ? [u.plugin.path] : [])))),
    ];
    if (scan === null) scan = wanted.length > 0 ? await d.scan(wanted) : [];
    const found = scan;
    const plan: { slot: 0 | 1; unit: RigUnit; at: number }[] = [];
    for (const slot of [0, 1] as const) {
      let at = 0;
      for (const unit of record.slots[slot].units) {
        if (reportedIds.has(unit.id)) {
          at++;
          continue;
        }
        if (unit.kind === 'plugin' && !found.some((p) => samePluginDescriptor(p, unit.plugin))) {
          console.error(
            `[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} at ${unit.plugin.path} is not in this launch's plugin scan (file missing, plugin changed or no plugin host); kept for a later launch`,
          );
          continue;
        }
        plan.push({ slot, unit, at: at++ });
      }
    }
    const loads = plan.filter((p) => p.unit.kind === 'plugin');
    let marked = leftover !== null && storedMarker(KEYS.chainMarker) !== null;
    if (loads.length > 0) {
      try {
        localStorage.setItem(KEYS.chainMarker, loads.map((p) => unitLabel(p.unit)).join(', '));
        marked = true;
      } catch (e) {
        console.error('[rig-recall] could not store the in-flight marker; nothing restored', e);
        return;
      }
    }
    for (const { slot, unit, at } of plan) {
      try {
        await d.add(slot, unit.id, addSpec(unit), at);
      } catch (e) {
        console.error(`[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} was not restored`, e);
      }
    }
    if (marked) {
      settling = KEYS.chainMarker;
      await new Promise((resolve) => setTimeout(resolve, SETTLE_MS));
      removeMarker(KEYS.chainMarker);
    }
  } catch (e) {
    console.error('[rig-recall] the chain restore failed', e);
  } finally {
    settling = null;
    writing = KEYS !== null;
    if (writing) {
      writeUnits();
      placePending();
    }
    finish();
  }
}

/** Place each slot's `pending` whose head's role the latest report shows (`headRole`), one slot at a time. */
function placePending(): void {
  const now = reported;
  const d = deps;
  if (!now || !d) return;
  for (const slot of [0, 1] as const) {
    if (placing[slot] || placementFailed[slot]) continue;
    const s = readRigRecord()?.slots[slot];
    if (!s?.pending?.length) continue;
    const role = headRole(s.units, now[slot].units);
    if (role === null) continue;
    placing[slot] = true;
    void placeSlot(slot, role, now[slot].units.length, d).finally(() => (placing[slot] = false));
  }
}

/** Slot `slot`'s placement: one durable write moves what its role takes into its units and drops its
 * `pending`; then the adds, at the chain's end (`length`, the reported chain's). A restart between re-adds
 * them from `units` under the same ids. A failed write adds nothing and waits for the next launch. */
async function placeSlot(slot: 0 | 1, role: 'source' | 'input', length: number, d: ChainRecallDeps): Promise<void> {
  let units: RigUnit[] = [];
  const stored = updateRig((r) => {
    units = placed(r.slots[slot], role);
    return withSlot(r, slot, withPlaced(r.slots[slot], role));
  });
  if (!stored) {
    placementFailed[slot] = true;
    console.error(`[rig-recall] slot ${slot + 1}: could not store the placed send units; none added, the next launch places them`);
    return;
  }
  let at = length;
  for (const unit of units) {
    try {
      await d.add(slot, unit.id, addSpec(unit), at);
      at++;
    } catch (e) {
      console.error(`[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} was not placed`, e);
    }
  }
}
