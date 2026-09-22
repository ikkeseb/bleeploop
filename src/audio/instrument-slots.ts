import { createSignal } from 'solid-js';
import type { PluginDescriptor } from '../platform';

/**
 * The two instrument slots' shared state cell + the per-slot operation chain. A LEAF module:
 * `instrument.ts` (synth/plugin selection, editor affinity) and `native-io.ts` (native input/monitor
 * arm, stream faults) both read slot state and serialise on the chain, and neither may import the
 * other — so what they share lives here. UI code reads slot state through `instrument.ts`'s
 * re-exports; the setters exist for those two owner modules only.
 *
 * Slot defaults: 0 = 'lead', 1 = 'bass' (both synths). A slot is in plugin mode while
 * `slotPlugins()[i]` is non-null and reverts to its synth id when a synth is re-selected.
 */

export const DEFAULT_IDS: [string, string] = ['lead', 'bass'];

// Boot opens native selection only after stale slots and persisted audio settings are reconciled.
export const [nativeHostReady, setNativeHostReady] = createSignal(false);

// Reactive state: synth id per slot, the loaded plugin per slot (null = synth mode), active slot.
export const [slotIds, setSlotIds] = createSignal<[string, string]>([...DEFAULT_IDS]);
export const [slotPlugins, setSlotPlugins] = createSignal<
  [PluginDescriptor | null, PluginDescriptor | null]
>([null, null]);
export const [activeSlot, setActiveSlotSignal] = createSignal<0 | 1>(0);

// Number of source operations either running or queued for each slot. Incrementing when an operation
// is enqueued (rather than when it starts) keeps the UI continuously pending between chained swaps.
const [slotPendingCounts, setSlotPendingCounts] = createSignal<[number, number]>([0, 0]);
export { slotPendingCounts };

/** Copy a 2-tuple with index `i` replaced by `v` (signals hold tuples immutably). */
export function withAt<T>(arr: readonly [T, T], i: 0 | 1, v: T): [T, T] {
  const next = [...arr] as [T, T];
  next[i] = v;
  return next;
}

/**
 * Per-slot operation chain. selectPlugin/clearPlugin/goLive/stopLive are async with native-IPC
 * await points; firing two in quick succession (a rapid picker double-click, or picking a synth
 * mid-load) would otherwise interleave — a second call's synchronous prologue reads slot state the
 * first call's continuation later overwrites, leaving slotPlugins / the Rust slot / the bridge /
 * engines[] disagreeing. Chaining each slot's mutations makes call N+1 start only after call N fully
 * settles. Failures don't break the chain (`.then(op, op)` runs the next op regardless; the swallowed
 * tail keeps it from going unhandled).
 */
const slotOpChain: [Promise<unknown>, Promise<unknown>] = [Promise.resolve(), Promise.resolve()];
export function serializeSlot<T>(slot: 0 | 1, op: () => Promise<T>): Promise<T> {
  setSlotPendingCounts((prev) => withAt(prev, slot, prev[slot] + 1));
  const run = slotOpChain[slot].then(op, op).finally(() => {
    setSlotPendingCounts((prev) => withAt(prev, slot, Math.max(0, prev[slot] - 1)));
  });
  slotOpChain[slot] = run.then(
    () => {},
    () => {},
  );
  return run;
}
