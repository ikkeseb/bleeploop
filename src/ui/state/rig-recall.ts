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
 *   each record unit the reset frame's `chains` lacks, the settle once each plugin add's outcome is in the
 *   feed, then each slot's `pending` placed once its head's role is known. A crash is told from a WebView
 *   reload by native MIDI's epoch (`midi.ts` `firstPageOfLaunch`), never by empty chains alone. One writer
 *   (`updateRig`) holds every write once the record is known (read, or upgraded and stored): the units from
 *   each `chains` report once the restore finished (`rigChainsReported`), a placement, the slot volume
 *   (`keepRigVolume`, at any time; before the record is known, the record takes it then).
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
 * launch whose device opens in chain mode (unless a recalled add never answers); never on one that runs no
 * recall (no device, or legacy mode without a plugin host). */
export function rigRecallDone(): Promise<void> {
  return done;
}
/** Whether `rigRecallDone` has settled: the synchronous read the DEV recall probes poll. */
export function rigRecallFinished(): boolean {
  return finished;
}

/**
 * The app is closing through its close button (`close-guard.ts`, just before it approves the close).
 * Once this launch's recalled loads have all come back (in chain mode: each plugin add's outcome is in the
 * feed, seated or failed), the host has lived through them: the marker goes now rather than at the end of
 * the settle window, so a close right after a restore brings the rig back at the next launch. A close while
 * a recalled load still runs keeps it, since a load that hangs the host looks just like that.
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

/** How long the restore waits for the first reset frame's `chains` (natively every reset frame carries
 * them): none by then, nothing is restored or written this launch. */
const REPORT_WAIT_MS = 5000;
/** How long a rejected add waits for its plate in a report (the feed and the IPC answer race): named, it
 * holds its place; not, the add's outcome is unknown. */
const PLATE_WAIT_MS = 500;

/** `lf.rig` as stored: none (the upgrade runs), unreadable (an unsupported version, bad text, no storage:
 * left as it is), or the record. */
type StoredRig = { kind: 'absent' } | { kind: 'unreadable' } | { kind: 'record'; record: RigRecord };

function readStoredRig(): StoredRig {
  if (!KEYS) return { kind: 'unreadable' };
  let raw: string | null;
  try {
    raw = localStorage.getItem(KEYS.rig);
  } catch {
    return { kind: 'unreadable' };
  }
  if (raw === null) return { kind: 'absent' };
  const record = parseRig(raw);
  return record ? { kind: 'record', record } : { kind: 'unreadable' };
}

/** The rig record as stored; null when there is none or it cannot be read. */
export function readRigRecord(): RigRecord | null {
  const s = readStoredRig();
  return s.kind === 'record' ? s.record : null;
}

/** Each slot's kept volume, for `instrument.ts`' chain gains at load; null without a readable record. */
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

/** On once this launch's restore has the record (read, or upgraded and stored): the writer writes only
 * then, so a record that failed its first write or cannot be read is never written over. */
let ready = false;
/** The slot levels the player set before `ready`: the record takes them when it is known. */
const levelsMeanwhile: [number | null, number | null] = [null, null];

const withLevels = (r: RigRecord): RigRecord => ({
  version: 1,
  slots: [0, 1].map((n) => ({ ...r.slots[n], volume: levelsMeanwhile[n] ?? r.slots[n].volume })) as [RigSlot, RigSlot],
});

/**
 * THE record writer once this launch's restore has the record (`ready`): read, change, store, in one step,
 * so a volume change never overwrites newer units or pending. Before that, or with no record: nothing is
 * written. True when the change is stored (or changed nothing).
 */
function updateRig(change: (r: RigRecord) => RigRecord): boolean {
  if (!ready) return false;
  const now = readRigRecord();
  if (!now) return false;
  const next = change(now);
  return serializeRig(next) === serializeRig(now) || storeRig(next);
}

/** Keep slot `slot`'s volume (the slot level's setter, at any time, a restore running included): before the
 * record is known, the record takes it then (the upgrade's first write included). */
export function keepRigVolume(slot: 0 | 1, volume: number): void {
  if (!ready) {
    levelsMeanwhile[slot] = volume;
    return;
  }
  if (!updateRig((r) => withSlot(r, slot, { ...r.slots[slot], volume }))) {
    console.error(`[rig-recall] slot ${slot + 1}: could not store its level in the rig record; the next launch starts at the level stored before`);
  }
}

/** What the chain restore needs from the modules that own it (`src/app/boot.ts` wires them). */
export interface ChainRecallDeps {
  /** This launch's scan of `paths` (the record's or the legacy recall's plugins), the full scan when it fails. */
  scan(paths: string[]): Promise<readonly PluginDescriptor[]>;
  /** The old keys through their resolvers, `scan` saying which legacy plugins are found. */
  snapshot(scan: readonly PluginDescriptor[], legacy: SavedRig): Promise<Omit<LegacySnapshot, 'legacyMarker' | 'reported'>>;
  /** Apply slot `slot`'s volume (the slot level as shown and the engine's `SetSlotGain`); settles once the
   * engine took it. */
  setVolume(slot: 0 | 1, volume: number): Promise<void>;
  /** Settles once the engine took the reset frame's send-off of the legacy sends (`legacySendsOff`). */
  sendsOff(): Promise<void>;
  /** Whether this page is the first of its native process: true a new launch, false a WebView reload, null
   * unknown (`midi.ts` `firstPageOfLaunch`). */
  firstPage(): Promise<boolean | null>;
  /** The chain host's `add`. */
  add(slot: 0 | 1, unit: UnitId, spec: UnitSpec, at: number): Promise<unknown>;
  /** A new unit id (`mintUnitId`). */
  mint(): UnitId;
}

// The `chains` reports this page has seen: the latest, whether a reset frame's came, and every id any of
// them named (a record unit never named is still on its way or was never added; one named before and gone
// since was removed).
let reported: [ChainState, ChainState] | null = null;
let resetReported = false;
const seen = new Set<UnitId>();
const reportWaiters: (() => void)[] = [];
/** On once this launch's chain restore has finished with the record known: each report then writes the
 * record's units, and each slot's `pending` is placed. */
let writing = false;
let deps: ChainRecallDeps | null = null;
const placing: [boolean, boolean] = [false, false];
/** A placement whose durable write failed, or whose add's outcome is unknown, waits for the next launch. */
const placementStopped: [boolean, boolean] = [false, false];
/** The units this page's adds put in (their add answered): a placement waits until the feed names each, so
 * its index counts them. */
const addedHere = new Set<UnitId>();

/** The feed's `chains` in chain mode (`chain.ts` `applyChains`; `reset`: a reset frame's): remembered, written
 * once the restore has finished, and each slot's `pending` placed once its head's role is known. */
export function rigChainsReported(next: [ChainState, ChainState], reset = false): void {
  reported = next;
  if (reset) resetReported = true;
  for (const c of next) for (const u of c.units) seen.add(u.id);
  reportWaiters.splice(0).forEach((wake) => wake());
  if (!writing) return;
  writeUnits();
  placePending();
}

/** Wait until `test()` holds, checked now and at each report; false when `ms` ran out first (none: no bound). */
async function untilReported(test: () => boolean, ms?: number): Promise<boolean> {
  const deadline = ms === undefined ? Infinity : Date.now() + ms;
  for (;;) {
    if (test()) return true;
    const left = deadline - Date.now();
    if (left <= 0) return false;
    await new Promise<void>((wake) => {
      reportWaiters.push(wake);
      if (left !== Infinity) setTimeout(wake, left);
    });
  }
}

const reportedUnit = (id: UnitId) => reported?.flatMap((c) => c.units).find((u) => u.id === id);

/** Whether a rejected add's unit has a plate in the chain (a failed add's, or a reservation whose answer was
 * lost), waiting PLATE_WAIT_MS for the report that names it. */
const plated = (id: UnitId): Promise<boolean> => untilReported(() => reportedUnit(id) !== undefined, PLATE_WAIT_MS);

/** A plugin unit's add has its outcome in the feed: seated, failed (parked or not), or gone again (removed);
 * not while it loads or waits for a take. */
function outcomeKnown(id: UnitId): boolean {
  const u = reportedUnit(id);
  if (!u) return seen.has(id);
  return u.reloading !== true && (u.state === 'seated' || u.state === 'failed');
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
 * Chain mode's recall, once per launch (`src/app/boot.ts`), on the first reset frame's `chains` (none within
 * REPORT_WAIT_MS: nothing restored or written). An unreadable `lf.rig` (bad text, an unsupported version) is
 * left as it is: one log line, nothing restored, nothing written this launch. None yet → the upgrade (§6)
 * from this launch's scan of the legacy recall's plugins and the old keys, adopting the reported chains
 * instead when the first reset frame already had units (a DEV chain run before any record, at the slot gains
 * the engine kept), a slot level the player set meanwhile included; stored whole and read back before
 * anything is added, else nothing is restored or written and the next launch upgrades again. Each slot's
 * volume applied and the legacy sends' send-off taken by the engine before the first add.
 *
 * A leftover marker means the last restore did not finish: in a new process (`firstPage`) with none of the
 * record's plugin units reported, that launch died restoring, and the record forgets its plugin units only
 * (one log line, one toast), the slots' `pending` then placing. A WebView reload (the same process; or some
 * of its plugin units reported) cut it short: it goes on, and the marker stays until a settle completes.
 * Unknown which: its plugin units are kept and not added this launch (the marker stays).
 *
 * Then each record unit the reported chains lack is added under its id, slot 0 then slot 1, one at a time,
 * each after the units before it (a rejected add's plate holds its place; with no plate its outcome is
 * unknown, and the slot's later units wait for the next launch); a plugin missing from the scan is skipped
 * with one log line and keeps its entry; a failed plate keeps its entry too (the feed names no cause). The
 * marker is stored before the first plugin add; the settle starts once every plugin add in flight has its
 * outcome in the feed (seated or failed) and removes the marker SETTLE_MS later (a close through the close
 * button meanwhile removes it then, `rigRecallOnClose`); with none in flight (each add rejected with no
 * plate) the marker goes at once. Nothing writes the record's units meanwhile; from
 * then on each `chains` report does, and a slot's `pending` is placed by a durable write before its adds.
 */
export async function recallChains(d: ChainRecallDeps): Promise<void> {
  let known = false;
  try {
    if (!KEYS) return;
    deps = d;
    if (!(await untilReported(() => resetReported, REPORT_WAIT_MS)) || !reported) {
      console.error('[rig-recall] no chains report from the engine; nothing restored, the rig record untouched');
      return;
    }
    const chains: [ChainState, ChainState] = reported;
    const reportedIds = new Set(chains.flatMap((c) => c.units.map((u) => u.id)));
    const stored = readStoredRig();
    if (stored.kind === 'unreadable') {
      console.error('[rig-recall] the rig record cannot be read (or is of a version this build does not know); left as it is, nothing restored');
      return;
    }
    let record: RigRecord;
    let scan: readonly PluginDescriptor[] | null = null;
    if (stored.kind === 'absent') {
      const legacy = readRig();
      scan = await d.scan([...new Set(legacy.flatMap((p) => (p ? [p.path] : [])))]);
      const base = await d.snapshot(scan, legacy);
      const adopted = chains.some((c) => c.units.length > 0)
        ? ([unitsFromReport([], chains[0].units, seen), unitsFromReport([], chains[1].units, seen)] as [RigUnit[], RigUnit[]])
        : null;
      const taken = new Set(reportedIds);
      record = withLevels(upgradeRig({ ...base, legacyMarker: storedMarker(KEYS.marker) !== null, reported: adopted }, freshMint(d.mint, taken)));
      if (!storeRig(record)) {
        console.error('[rig-recall] could not store the rig record; nothing restored, the upgrade runs again at the next launch');
        return;
      }
    } else {
      record = stored.record;
      if (levelsMeanwhile.some((v) => v !== null)) {
        record = withLevels(record);
        if (!storeRig(record)) console.error('[rig-recall] could not store the slot levels set during the restore');
      }
    }
    ready = true;
    levelsMeanwhile.fill(null);
    // The engine has each slot's level and the legacy sends off before any unit goes in (review finding 6).
    await Promise.all([d.setVolume(0, record.slots[0].volume), d.setVolume(1, record.slots[1].volume), d.sendsOff()]);
    known = true;

    const plugins = record.slots.flatMap((s) => s.units.filter((u) => u.kind === 'plugin'));
    const leftover = storedMarker(KEYS.chainMarker);
    let keepPlugins = false;
    if (leftover !== null && !plugins.some((u) => reportedIds.has(u.id))) {
      const first = await d.firstPage();
      if (first === true) {
        const noPlugins = (s: RigSlot): RigSlot => ({ ...s, units: s.units.filter((u) => u.kind !== 'plugin') });
        const forgotten: RigRecord = { version: 1, slots: [noPlugins(record.slots[0]), noPlugins(record.slots[1])] };
        if (!storeRig(forgotten)) {
          console.error('[rig-recall] could not store the rig record without its plugins; nothing restored');
          known = false;
          return;
        }
        record = forgotten;
        removeMarker(KEYS.chainMarker);
        console.error(`[rig-recall] the last launch stopped while restoring ${leftover}; its plugins are forgotten`);
        notifyError('Could not restore the plugins. Add a plugin again to load it.', `The last launch stopped while restoring ${leftover}.`);
      } else if (first === null) {
        keepPlugins = true;
        console.error(`[rig-recall] the last restore of ${leftover} did not finish, and whether this is a new launch or a reload is unknown; its plugins are kept, not restored now`);
      }
    }

    const wanted = [
      ...new Set(record.slots.flatMap((s) => s.units.flatMap((u) => (u.kind === 'plugin' && !reportedIds.has(u.id) ? [u.plugin.path] : [])))),
    ];
    if (scan === null) scan = wanted.length > 0 && !keepPlugins ? await d.scan(wanted) : [];
    const found = scan;
    const addable = (u: RigUnit): boolean => u.kind !== 'plugin' || (!keepPlugins && found.some((p) => samePluginDescriptor(p, u.plugin)));
    const loads = record.slots.flatMap((s) => s.units.filter((u) => u.kind === 'plugin' && !reportedIds.has(u.id) && addable(u)));
    // In flight: the plugins this launch adds, and, when the last restore did not finish, the reported ones.
    const inFlight = new Set(loads.map((u) => u.id));
    const carried = leftover !== null && storedMarker(KEYS.chainMarker) !== null;
    if (carried) for (const u of plugins) if (reportedIds.has(u.id)) inFlight.add(u.id);
    if (loads.length > 0) {
      try {
        localStorage.setItem(KEYS.chainMarker, loads.map(unitLabel).join(', '));
      } catch (e) {
        console.error('[rig-recall] could not store the in-flight marker; nothing restored', e);
        return;
      }
    }
    for (const slot of [0, 1] as const) {
      let at = 0;
      for (const unit of record.slots[slot].units) {
        if (reportedIds.has(unit.id)) {
          at++;
          continue;
        }
        if (unit.kind === 'plugin' && !addable(unit)) {
          if (!keepPlugins) {
            console.error(
              `[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} at ${unit.plugin.path} is not in this launch's plugin scan (file missing, plugin changed or no plugin host); kept for a later launch`,
            );
          }
          continue;
        }
        try {
          await d.add(slot, unit.id, addSpec(unit), at);
          addedHere.add(unit.id);
          at++;
        } catch (e) {
          console.error(`[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} was not restored`, e);
          if (await plated(unit.id)) at++;
          else {
            // Reserved or not is unknown: a later unit's index would be a guess. They wait for the next launch.
            inFlight.delete(unit.id);
            break;
          }
        }
      }
    }
    if (inFlight.size > 0) {
      await untilReported(() => [...inFlight].every(outcomeKnown));
      settling = KEYS.chainMarker;
      await new Promise((resolve) => setTimeout(resolve, SETTLE_MS));
      removeMarker(KEYS.chainMarker);
    } else if (loads.length > 0) {
      // This launch's marker, and none of its plugin adds reached the host (each rejected with no plate): no
      // load is in flight, and an IPC failure is no crash (the next launch retries them).
      removeMarker(KEYS.chainMarker);
    }
  } catch (e) {
    console.error('[rig-recall] the chain restore failed', e);
    known = false;
  } finally {
    settling = null;
    writing = known;
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
    if (placing[slot] || placementStopped[slot]) continue;
    const s = readRigRecord()?.slots[slot];
    if (!s?.pending?.length) continue;
    const role = headRole(s.units, now[slot].units);
    if (role === null) continue;
    // an add this page made that the feed does not name yet would put the placement's index short
    const named = new Set(now[slot].units.map((u) => u.id));
    if (s.units.some((u) => addedHere.has(u.id) && !named.has(u.id) && !seen.has(u.id))) continue;
    placing[slot] = true;
    void placeSlot(slot, role, d).finally(() => (placing[slot] = false));
  }
}

/** Slot `slot`'s placement: one durable write moves what its role takes into its units and drops its
 * `pending`; then the adds, appended in record order after the chain as reported, each after the one before
 * (a rejected add's plate holds its place; with none, its outcome is unknown and the rest wait for the next
 * launch's restore, under their ids at their record places). A restart between re-adds them from `units`.
 * A failed write adds nothing and waits for the next launch. */
async function placeSlot(slot: 0 | 1, role: 'source' | 'input', d: ChainRecallDeps): Promise<void> {
  let units: RigUnit[] = [];
  const stored = updateRig((r) => {
    units = placed(r.slots[slot], role);
    return withSlot(r, slot, withPlaced(r.slots[slot], role));
  });
  if (!stored) {
    placementStopped[slot] = true;
    console.error(`[rig-recall] slot ${slot + 1}: could not store the placed send units; none added, the next launch places them`);
    return;
  }
  await d.sendsOff();
  let at = reported?.[slot].units.length ?? 0;
  for (const unit of units) {
    try {
      await d.add(slot, unit.id, addSpec(unit), at);
      addedHere.add(unit.id);
      at++;
    } catch (e) {
      console.error(`[rig-recall] slot ${slot + 1}: ${unitLabel(unit)} was not placed`, e);
      if (await plated(unit.id)) at++;
      else {
        placementStopped[slot] = true;
        return;
      }
    }
  }
}
