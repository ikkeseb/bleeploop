/**
 * OWNS: rig recall — which plugin each slot held when the app last ran, restoring it at launch
 * through the normal load path, and the in-flight marker that keeps a plugin which crashes or hangs
 * the host at load from taking down every later launch. Only the plugin's identity is kept: not its
 * tone state (VST3 state recall is not built) and never an arm, so GO LIVE stays one press. The
 * guitar input channel is not here: it is the one global Audio Settings choice, already persisted by
 * `audio-settings.ts`.
 *
 * The record follows the slot: `instrument.ts` remembers a plugin when its load succeeds and forgets
 * it when the slot's plugin unloads, a load into the slot fails or a synth is picked for it. A WebView
 * reload keeps the record (`resyncNativeSlots` unloads the stranded plugins without forgetting them),
 * so the recall brings them back.
 */
import type { PluginDescriptor } from '../platform';
import { notifyError } from '../notify';
import { samePluginDescriptor } from './plugin-descriptor';
import { withAt } from './instrument-slots';

// Where the record and the marker live; null turns rig recall off. A DEV native probe (`VITE_LF_PROBE`)
// starts from empty slots and stores nothing: a run killed mid-probe would hand its next run the
// plugin that wedged it. Only the recall's own probe (`src/debug/recall-restart.ts`) recalls, from keys
// of its own, so an agent's run never touches the rig the owner's dev sessions restore.
const PROBE = import.meta.env.VITE_LF_PROBE as string | undefined;
const PREFIX = !PROBE ? 'lf.' : PROBE === 'recall-restart' ? `lf.probe.${PROBE}.` : null;
const KEYS = PREFIX === null ? null : { record: `${PREFIX}rigRecall`, marker: `${PREFIX}rigRecallInFlight` };
/** How long the marker outlives the last recalled load: a plugin that takes the host down in its
 * first moments of processing is a crash at load too. */
const SETTLE_MS = 3000;

type SavedPlugin = Pick<PluginDescriptor, 'format' | 'path' | 'id' | 'name'>;
type SavedRig = [SavedPlugin | null, SavedPlugin | null];

function isSavedPlugin(v: unknown): v is SavedPlugin {
  if (typeof v !== 'object' || v === null) return false;
  const p = v as Partial<Record<keyof SavedPlugin, unknown>>;
  return (
    (p.format === 'clap' || p.format === 'vst3') &&
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

const label = (p: SavedPlugin) => `${p.name} (${p.format})`;

/** Remember `desc` as slot `slot`'s plugin (its load succeeded). */
export function rememberSlotPlugin(slot: 0 | 1, desc: PluginDescriptor): void {
  const { format, path, id, name } = desc;
  writeRig(withAt(readRig(), slot, { format, path, id, name }));
}

/** Forget slot `slot`'s plugin (it unloaded, a load into the slot failed, or a synth was picked for it). */
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

let settling = false;
let done = false;
/** Whether this launch's recall has finished, the settle window included (the probes wait on it). */
export function rigRecallDone(): boolean {
  return done;
}

/**
 * The app is closing through its close button (`close-guard.ts`, just before it approves the close).
 * Once this launch's recalled loads have all come back, the host has lived through them: the marker
 * goes now rather than at the end of the settle window, so a close right after a restore brings the
 * rig back at the next launch. A close while a recalled load still runs keeps it, since a load that
 * hangs the host looks just like that.
 */
export function rigRecallOnClose(): void {
  if (settling) removeMarker();
}

/**
 * Restore each slot's remembered plugin, slot 0 then slot 1, through `load` (`restorePlugin`: the slot
 * chain, the load token and the routing rules all apply, and a slot the player has already chosen a
 * source for keeps that choice), from this launch's scan `available`. Boot runs it once, after the
 * scan. A slot whose plugin is missing from the scan (file gone, plugin changed) is skipped with one
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
      notifyError('Plugins not restored', `The last launch stopped while restoring ${leftover}. Pick a plugin again to load it.`);
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
    done = true;
  }
}
