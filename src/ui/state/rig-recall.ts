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
 * - **Chain** (DEV only until activation, `docs/unit-contract.md` §1, §6): the rig record, its writer, the
 *   restore and the crash marker are the native host's (`engine_io/rig/`, its rules `src-tauri/crates/lf-rig`).
 *   The page hands over the legacy keys' upgrade once (`openRig`, `importRig`; the record's shape and the
 *   upgrade: `rig-record.ts`) and never reads or writes a chain record itself. With no native record
 *   (`platform.rig.available` off: only the browser fake without `__lfRigFake`), a chain-mode page keeps no rig.
 */
import { NATIVE_WAIT_MS, platform, TimedOut, within, type PluginDescriptor, type RigAnswer, type UnitId } from '../../platform';
import { notifyError } from '../../notify';
import { samePluginDescriptor } from './plugin-descriptor';
import { isPluginFormat } from './plugin-format';
import { withAt } from './instrument-slots';
import { serializeRig, upgradeRig, type LegacySnapshot } from './rig-record';

// Where the record and the marker live; null turns rig recall off. A DEV native probe (`VITE_LF_PROBE`)
// starts from empty slots and stores nothing: a run killed mid-probe would hand its next run the
// plugin that wedged it. Only the recall's own probes (`src/debug/recall-restart.ts`,
// `src/debug/tone-recall.ts`) recall, each from keys of its own, so an agent's run never touches the rig
// the owner's dev sessions restore.
const PROBE = import.meta.env.VITE_LF_PROBE as string | undefined;
const RECALL_PROBES = ['recall-restart', 'tone-recall'];
const PREFIX = !PROBE ? 'lf.' : RECALL_PROBES.includes(PROBE) ? `lf.probe.${PROBE}.` : null;
const KEYS = PREFIX === null ? null : { record: `${PREFIX}rigRecall`, marker: `${PREFIX}rigRecallInFlight` };
/** The WebView's own chain record and its crash marker, from before the native record (D12: deleted by the
 * first chain-mode page of this build, never restored natively). */
const DEV_RIG_KEYS = PREFIX === null ? [] : [`${PREFIX}rig`, `${PREFIX}rigInFlight`];
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

function removeMarker(): void {
  if (!KEYS) return;
  try {
    localStorage.removeItem(KEYS.marker);
  } catch {
    /* best-effort; a leftover marker only skips the next recall */
  }
}

/** Whether the legacy recall's settle window runs now. */
let settling = false;
let finished = false;
let finish!: () => void;
const done = new Promise<void>((resolve) => (finish = () => ((finished = true), resolve())));
/** This launch's recall has finished, the settle window included (the probes await it). Settles on every
 * launch whose device opens in chain mode (its handover, `importRig`: native's restore is not awaited); never
 * on one that runs no recall (no device, or legacy mode without a plugin host).
 * @public (the legacy-storage probe awaits it)
 */
export function rigRecallDone(): Promise<void> {
  return done;
}
/** Whether `rigRecallDone` has settled: the synchronous read the DEV recall probes poll. */
export function rigRecallFinished(): boolean {
  return finished;
}

/**
 * The app is closing through its close button (`close-guard.ts`, just before it approves the close).
 * Once this launch's recalled loads have all come back, the host has lived through them: the legacy
 * marker goes now rather than at the end of
 * the settle window, so a close right after a restore brings the rig back at the next launch. A close while
 * a recalled load still runs keeps it, since a load that hangs the host looks just like that.
 */
export function rigRecallOnClose(): void {
  if (settling) removeMarker();
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
    settling = true;
    await new Promise((resolve) => setTimeout(resolve, SETTLE_MS));
    removeMarker();
  } finally {
    settling = false;
    finish();
  }
}

/** `mint`, never answering an id it gave before. */
function freshMint(mint: () => UnitId): () => UnitId {
  const taken = new Set<UnitId>();
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

// ── Chain mode with the native record: the handover (unit contract §1, §6; D12, D13) ────────────────────

/** What the handover needs from the modules that own the fader (`src/app/boot.ts` wires it). */
export interface RigHandoverDeps {
  /** Show these levels on the slots' faders (`instrument.ts` `showRigLevels`): an answer's (null: none, the
   * engine's unity), or the upgrade's own until the import answers. */
  levels(volumes: [number, number] | null, from: 'answer' | 'upgrade'): void;
}

/** What the import needs besides (`src/app/boot.ts` wires them). */
export interface RigImportDeps extends RigHandoverDeps {
  /** This launch's scan of `paths` (the legacy recall's plugins), the full scan when it fails. */
  scan(paths: string[]): Promise<readonly PluginDescriptor[]>;
  /** The old keys through their resolvers, `scan` saying which legacy plugins are found. */
  snapshot(scan: readonly PluginDescriptor[], legacy: SavedRig): Promise<Omit<LegacySnapshot, 'legacyMarker'>>;
  /** A new unit id (`mintUnitId`). */
  mint(): UnitId;
  /** Whether the device runs and the plugin host is initialised (`host_init`, whose epoch the import
   * carries): asked right before the import, after the scan and the bindings wait. */
  ready(): boolean;
}

/** How long the import may take before the handover ends without its answer (D13: a commit that failed or
 * never came leaves no record, and the next launch answers `absent` and imports again). */
const IMPORT_WAIT_MS = 5000;

/** Whether this page showed a rig notice: one toast per page (a reload's page shows it again until `seen`). */
let noticeShown = false;

/** Tell the player what the restore forgot or held back (`answer.notice`), once per page, then confirm it to
 * native (`seen`), which answers it to every open until then. */
function tellNotice(answer: RigAnswer): void {
  const notice = answer.notice;
  if (notice === null || noticeShown) return;
  noticeShown = true;
  if (notice.forgotten.length > 0) {
    const names = notice.forgotten.join(', ');
    console.error(`[rig-recall] the last launch stopped while restoring ${names}; its plugins are forgotten`);
    notifyError('Could not restore the plugins. Add a plugin again to load it.', `The last launch stopped while restoring ${names}.`);
  } else if (notice.pluginsHeld) {
    console.error("[rig-recall] the last restore's crash marker could not be read; no plugin restored this launch");
    notifyError('Could not restore the plugins this time. Restart BleepLoop to bring them back.', 'What the last launch was loading could not be read.');
  }
  platform.rig.seen(notice.id).catch((e: unknown) => {
    console.error('[rig-recall] could not confirm the rig notice; the next page shows it again', e);
  });
}

/**
 * The handover's first half, before the device opens (SEAT-B: on `present` native seeds the slot levels and
 * the send-off before it answers, so the device that opens next replays them): the DEV cutover (D12: `lf.rig`
 * and `lf.rigInFlight` go, a record never restored natively), then `rig.open()`. Its levels go on the faders
 * (but `absent`'s: the upgrade's stay until the import answers), its notice to the player. A rejection, or no
 * answer in NATIVE_WAIT_MS, is one log line and boot goes on without a record this launch, as `unreadable`
 * (kept, never written over) and `unavailable` (another process owns it) are; an answer that comes after
 * NATIVE_WAIT_MS is still taken (its levels at an answer's rank, below a reset frame's and a gesture's, and its
 * notice), though nothing is imported on it. Null when it did not answer in time, or when this page keeps no
 * rig (a DEV native probe's: `KEYS`, so its run never opens the owner's record).
 */
export async function openRig(d: RigHandoverDeps): Promise<RigAnswer | null> {
  if (!KEYS) return null;
  for (const key of DEV_RIG_KEYS) {
    try {
      localStorage.removeItem(key);
    } catch {
      /* no storage: nothing of the old record to drop */
    }
  }
  const call = platform.rig.open();
  const take = (answer: RigAnswer): RigAnswer => {
    if (answer.status === 'unreadable') {
      console.error('[rig-recall] the native rig record cannot be read (or a newer build wrote it); left as it is, nothing restored');
    } else if (answer.status === 'unavailable') {
      console.error('[rig-recall] the native rig record is unavailable (another BleepLoop owns it, or no data folder); nothing restored or kept this launch');
    }
    if (answer.status !== 'absent') d.levels(answer.volumes, 'answer');
    tellNotice(answer);
    return answer;
  };
  try {
    return take(await within(call, NATIVE_WAIT_MS, 'rig_open'));
  } catch (e) {
    console.error('[rig-recall] could not open the native rig record; nothing restored or imported this launch', e);
    d.levels(null, 'answer');
    // a late answer (the device-open gate has let go): what native seeded and its notice still count
    if (e instanceof TimedOut) void call.then(take, () => {});
    return null;
  }
}

/**
 * The handover's second half, once the device runs and the plugin host is up: only when `opened` (`openRig`'s
 * answer) is `absent`, the upgrade of the old keys (§6) from this launch's scan of the legacy recall's plugins,
 * with no reported chains (D12: native adopts what its model holds), and one `rig.import`, which native stores
 * only while it has none, sent only while the device still runs (`ready`, asked right before it). The fader
 * shows the upgrade's levels until the import answers, then the answer's. No device at the import, a refused
 * import (a replaced page: `'stale'`; a failed write or an invalid answer: nothing restored, the next launch
 * imports again) or one with no answer in IMPORT_WAIT_MS is one log line. Settles `rigRecallDone` in every case.
 */
export async function importRig(opened: RigAnswer | null, d: RigImportDeps): Promise<void> {
  try {
    if (!KEYS || opened?.status !== 'absent') return;
    const legacy = readRig();
    const scan = await d.scan([...new Set(legacy.flatMap((p) => (p ? [p.path] : [])))]);
    const base = await d.snapshot(scan, legacy);
    const record = upgradeRig({ ...base, legacyMarker: recallInFlight() }, freshMint(d.mint));
    d.levels([record.slots[0].volume, record.slots[1].volume], 'upgrade');
    if (!d.ready()) {
      // the device stopped (or the host never came up) during the scan or the bindings wait: native would
      // restore into no device, so nothing is imported; native keeps no record, the next launch imports
      console.error('[rig-recall] no audio device runs at the import; nothing imported, the next launch imports it again');
      d.levels(null, 'answer');
      return;
    }
    const call = platform.rig.import(serializeRig(record));
    let answer: RigAnswer;
    try {
      answer = await within(call, IMPORT_WAIT_MS, 'rig_import');
    } catch (e) {
      if (e instanceof TimedOut) {
        // the faders keep the upgrade's levels, which a late commit seeds; a late answer still shows its own
        console.error(`[rig-recall] the rig record's import got no answer in ${IMPORT_WAIT_MS / 1000} s; the handover ends here`, e);
        void call.then(
          (late) => {
            d.levels(late.volumes, 'answer');
            tellNotice(late);
          },
          // Logged only: the faders stay on the upgrade's levels, which no engine took from this import, but
          // which the next launch imports again (D13: a failed commit leaves no record, so it answers `absent`).
          (late: unknown) => console.error('[rig-recall] the rig record\'s import failed after its timeout; nothing restored, the next launch imports it again', late),
        );
        return;
      }
      const stale = e === 'stale' || (e instanceof Error && e.message === 'stale');
      if (stale) console.error('[rig-recall] the native rig record refused this page\'s import: a newer page replaced it');
      else console.error('[rig-recall] could not import the rig record; nothing restored, the next launch imports it again', e);
      d.levels(null, 'answer');
      return;
    }
    d.levels(answer.volumes, 'answer');
    tellNotice(answer);
  } catch (e) {
    console.error("[rig-recall] the rig record's handover failed", e);
  } finally {
    finish();
  }
}
