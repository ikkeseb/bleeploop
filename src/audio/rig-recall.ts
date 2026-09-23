/**
 * OWNS: rig recall — which plugin each slot held when the app last ran, restoring it at launch
 * through the normal load path, and the in-flight marker that keeps a plugin which crashes or hangs
 * the host at load from taking down every later launch. Only the plugin's identity is kept: not its
 * tone state (VST3 state recall is not built) and never an arm, so GO LIVE stays one press. The
 * guitar input channel is not here: it is the one global Audio Settings choice, already persisted by
 * `audio-settings.ts`.
 *
 * The record follows the slot: `instrument.ts` remembers a plugin when its load succeeds and forgets
 * it when the slot's plugin unloads or a load into the slot fails. A WebView reload keeps the record
 * (`resyncNativeSlots` unloads the stranded plugins without forgetting them), so the recall brings
 * them back.
 */
import type { PluginDescriptor } from '../platform';
import { notifyError } from '../notify';
import { samePluginDescriptor } from './plugin-descriptor';
import { slotPendingCounts, slotPlugins, withAt } from './instrument-slots';

// A DEV native probe (`VITE_LF_PROBE`) keeps its own record, so an agent's probe run never rewrites
// the rig the owner's dev sessions restore, and a probe never starts on the owner's plugins.
const PREFIX = import.meta.env.VITE_LF_PROBE ? `lf.probe.${import.meta.env.VITE_LF_PROBE}.` : 'lf.';
const RECORD_KEY = `${PREFIX}rigRecall`;
const MARKER_KEY = `${PREFIX}rigRecallInFlight`;
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
  try {
    const raw = localStorage.getItem(RECORD_KEY);
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
  try {
    localStorage.setItem(RECORD_KEY, JSON.stringify(rig));
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

/** Forget slot `slot`'s plugin (it unloaded, or a load into the slot failed). */
export function forgetSlotPlugin(slot: 0 | 1): void {
  const rig = readRig();
  if (rig[slot]) writeRig(withAt(rig, slot, null));
}

/** Whether a recall's marker is stored right now (the DEV restart probe watches for it). */
export function recallInFlight(): boolean {
  try {
    return localStorage.getItem(MARKER_KEY) !== null;
  } catch {
    return false;
  }
}

let done = false;
/** Whether this launch's recall has finished, the settle window included (the probes wait on it). */
export function rigRecallDone(): boolean {
  return done;
}

/**
 * Restore each slot's remembered plugin, slot 0 then slot 1, through `load` (`selectPlugin`, so the
 * slot chain, the load token and the routing rules all apply), from this launch's scan `available`.
 * Boot runs it once, after the scan. A slot whose plugin is missing from the scan (file gone, plugin
 * changed) is skipped with one log line and keeps its record for a launch where the plugin is back; a
 * slot already picked for keeps that pick. A failed load reports through the load path's own toast.
 *
 * The marker is stored before the first load and removed SETTLE_MS after the last. Found at the next
 * launch, it means that launch died or was stopped while restoring: nothing is restored, the whole
 * record is forgotten, and one log line plus one toast say so. Closing the app inside that window
 * counts as such a stop.
 */
export async function recallRig(
  available: readonly PluginDescriptor[],
  load: (slot: 0 | 1, desc: PluginDescriptor) => Promise<void>,
): Promise<void> {
  try {
    let leftover: string | null;
    try {
      leftover = localStorage.getItem(MARKER_KEY);
    } catch {
      return; // no storage: nothing was remembered either
    }
    if (leftover !== null) {
      writeRig([null, null]);
      try {
        localStorage.removeItem(MARKER_KEY);
      } catch {
        /* best-effort */
      }
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
      localStorage.setItem(MARKER_KEY, plan.map(([, d]) => label(d)).join(', '));
    } catch (e) {
      console.error('[rig-recall] could not store the in-flight marker; nothing restored', e);
      return;
    }
    for (const [slot, desc] of plan) {
      // Checked and enqueued in one synchronous step: a pick made during slot 0's load wins slot 1.
      if (slotPlugins()[slot] || slotPendingCounts()[slot] > 0) continue;
      await load(slot, desc);
    }
    await new Promise((resolve) => setTimeout(resolve, SETTLE_MS));
    try {
      localStorage.removeItem(MARKER_KEY);
    } catch {
      /* best-effort; a leftover marker only skips the next recall */
    }
  } finally {
    done = true;
  }
}
