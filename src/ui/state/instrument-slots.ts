import { createSignal } from 'solid-js';
import type { PluginDescriptor } from '../../platform';

/**
 * The two instrument slots' shared state cell + the per-slot operation chain. A LEAF module:
 * `instrument.ts` (synth/plugin selection, editor affinity) and `native-io.ts` (native input/monitor
 * arm, stream faults) both read slot state and serialise on the chain, and neither may import the
 * other — so what they share lives here. UI code reads slot state through `instrument.ts`'s
 * re-exports; the setters exist for those two owner modules only.
 *
 * Slot defaults: 0 = 'lead', 1 = 'bass' (both synths). A slot is in plugin mode while
 * `slotPlugins()[i]` is non-null; without one it plays its synth id, or nothing while `slotOff()[i]`
 * (an Off slot's notes go nowhere and GO LIVE passes its input dry). A loaded plugin
 * sits over either, and an unload goes back to it.
 */

export const DEFAULT_IDS: [string, string] = ['lead', 'bass'];

// Boot opens native selection only after stale slots and persisted audio settings are reconciled.
export const [nativeHostReady, setNativeHostReady] = createSignal(false);

// Reactive state: synth id per slot, the loaded plugin per slot (null = synth mode), active slot, and
// whether a slot without a plugin is Off.
export const [slotIds, setSlotIds] = createSignal<[string, string]>([...DEFAULT_IDS]);
export const [slotPlugins, setSlotPlugins] = createSignal<
  [PluginDescriptor | null, PluginDescriptor | null]
>([null, null]);
// The native instance each slot's plugin is (`PluginInfo.instance`), set and cleared with `slotPlugins`;
// null while the slot holds no plugin.
export const [slotInstances, setSlotInstances] = createSignal<[number | null, number | null]>([null, null]);

/** The instance a request for `slot`'s plugin names (`PluginHost.openEditor`, ...). 0, while none is
 * known (or a probe's stand-in host answered the load without one), is no instance the host has: its
 * count starts at 1. */
export const slotInstance = (slot: 0 | 1): number => slotInstances()[slot] ?? 0;

/** The host refused a request because it named an instance the slot no longer plays: the plugin it was
 * for is gone, so the request is moot, not failed. */
export const isStaleInstance = (e: unknown): boolean => /stale instance/.test(String(e));

/** The host refused a request for now (`busy:`: a swap or an unload holds the slot), nothing changed: no
 * error state, the player tries again. */
export const isBusyRefusal = (e: unknown): boolean => /^(Error: )?busy:/.test(String(e));

export const [activeSlot, setActiveSlotSignal] = createSignal<0 | 1>(0);
export const [slotOff, setSlotOff] = createSignal<[boolean, boolean]>([false, false]);

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
