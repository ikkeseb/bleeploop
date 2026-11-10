import { createSignal } from 'solid-js';
import type { PluginDescriptor } from '../../platform';

/**
 * The two instrument slots' shared state cell + the per-slot operation chain. A LEAF module:
 * `instrument.ts` (synth/plugin selection, editor affinity) and `native-io.ts` (native input/monitor
 * arm, stream faults) both read slot state and serialise on the chain, and neither may import the
 * other — so what they share lives here. UI code reads slot state through `instrument.ts`'s
 * re-exports; the setters exist for those two owner modules only.
 *
 * Slot defaults: 0 = 'lead', 1 = 'bass' (both synths), except on a guitar-first install
 * (`guitarFirstInstall`), where a slot nobody picked a source for starts Off (`instrument.ts`). A slot
 * is in plugin mode while `slotPlugins()[i]` is non-null; without one it plays its synth id, or nothing
 * while `slotOff()[i]` (an Off slot's notes go nowhere and GO LIVE passes its input dry). A loaded
 * plugin sits over either, and an unload goes back to it.
 */

export const DEFAULT_IDS: [string, string] = ['lead', 'bass'];

const GUITAR_FIRST_KEY = 'lf.guitarFirst';

/**
 * Whether this install opened guitar-first: nothing of BleepLoop's (no `lf.` key) was stored when it
 * first ran, so it keeps the guitar-first defaults for everything its player has not set: both slots
 * Off (slot A asking for the amp-sim, slot B folded), the keyboard hidden until the first built-in
 * synth is picked (`layout-store.ts`) and the empty looper's start line (`lane-state.ts`). An install
 * that stored anything before this rule existed keeps today's defaults to the pixel. The first call on
 * a fresh install stores the marker, so every later call and launch agrees; its callers read it while
 * the modules load, before the app stores anything. Unreadable storage counts as an earlier install.
 */
export function guitarFirstInstall(): boolean {
  try {
    if (localStorage.getItem(GUITAR_FIRST_KEY) !== null) return true;
    for (let i = 0; i < localStorage.length; i++) if (localStorage.key(i)?.startsWith('lf.')) return false;
    localStorage.setItem(GUITAR_FIRST_KEY, '1');
    return true;
  } catch {
    return false;
  }
}

// Boot opens native selection only after stale slots and persisted audio settings are reconciled.
export const [nativeHostReady, setNativeHostReady] = createSignal(false);

// Reactive state: synth id per slot, the loaded plugin per slot (null = synth mode), active slot, and
// whether a slot without a plugin is Off.
export const [slotIds, setSlotIds] = createSignal<[string, string]>([...DEFAULT_IDS]);
export const [slotPlugins, setSlotPlugins] = createSignal<
  [PluginDescriptor | null, PluginDescriptor | null]
>([null, null]);
export const [activeSlot, setActiveSlotSignal] = createSignal<0 | 1>(0);
export const [slotOff, setSlotOff] = createSignal<[boolean, boolean]>([false, false]);
// A slot still on the guitar-first install's Off that has never held a source: no source stored for it
// and none picked or loaded since (`instrument.ts`). Slot A's picker then asks for the amp-sim and slot
// B folds (`InstrumentSlot.tsx`).
export const [slotUnset, setSlotUnset] = createSignal<[boolean, boolean]>([false, false]);

// Number of source operations either running or queued for each slot. Incrementing when an operation
// is enqueued (rather than when it starts) keeps the UI continuously pending between chained swaps. A
// GO LIVE op (`serializeLive`) is no source op and is not counted.
const [slotPendingCounts, setSlotPendingCounts] = createSignal<[number, number]>([0, 0]);
export { slotPendingCounts };

/** Copy a 2-tuple with index `i` replaced by `v` (signals hold tuples immutably). */
export function withAt<T>(arr: readonly [T, T], i: 0 | 1, v: T): [T, T] {
  const next = [...arr] as [T, T];
  next[i] = v;
  return next;
}

/**
 * Per-slot operation chain. selectPlugin/clearPlugin (source ops) and goLive/stopLive/GO LIVE's press
 * (live ops, `serializeLive`) are async with native-IPC await points; firing two in quick succession (a rapid picker double-click, or picking a synth
 * mid-load) would otherwise interleave — a second call's synchronous prologue reads slot state the
 * first call's continuation later overwrites, leaving slotPlugins / the Rust slot / the bridge /
 * engines[] disagreeing. Chaining each slot's mutations makes call N+1 start only after call N fully
 * settles. Failures don't break the chain (`.then(op, op)` runs the next op regardless; the swallowed
 * tail keeps it from going unhandled).
 */
const slotOpChain: [Promise<unknown>, Promise<unknown>] = [Promise.resolve(), Promise.resolve()];
function chain<T>(slot: 0 | 1, op: () => Promise<T>): Promise<T> {
  const run = slotOpChain[slot].then(op, op);
  slotOpChain[slot] = run.then(
    () => {},
    () => {},
  );
  return run;
}

/** A source op on `slot`'s chain: the slot shows it pending (`slotPendingCounts`) until it settles. */
export function serializeSlot<T>(slot: 0 | 1, op: () => Promise<T>): Promise<T> {
  setSlotPendingCounts((prev) => withAt(prev, slot, prev[slot] + 1));
  return chain(slot, op).finally(() => {
    setSlotPendingCounts((prev) => withAt(prev, slot, Math.max(0, prev[slot] - 1)));
  });
}

/** A GO LIVE op on `slot`'s chain, in order with its source ops but none itself: the slot shows no
 * pending source, and a press made meanwhile queues behind it (`src/ui/instrument/live.ts`). */
export function serializeLive<T>(slot: 0 | 1, op: () => Promise<T>): Promise<T> {
  return chain(slot, op);
}
