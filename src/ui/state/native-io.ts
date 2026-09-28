import { createSignal } from 'solid-js';
import { sendEngine } from '../../platform';
import { notifyError } from '../../notify';
import { activeSlot, serializeSlot, slotOff, slotPlugins, withAt } from './instrument-slots';

/**
 * OWNS: which slots hear their own input (GO LIVE, the engine's `SetSlotLive`; the capture channel is
 * the slot's pick, `engine-store.ts`). The engine passes a live slot's input through the slot's effect,
 * or dry through a slot without a plugin; both slots may be live at once, and the engine monitors in its
 * own callback. The GO LIVE paths are serialised on the same per-slot chain as load/unload
 * (`instrument-slots.ts`) so they can never interleave with a plugin swap; `disarmInputInternal` is
 * exported for `instrument.ts`'s teardown paths, which are already inside a serialised op and must not
 * re-enter the chain.
 */

// Whether each slot hears its own input.
const [inputArmed, setInputArmed] = createSignal<[boolean, boolean]>([false, false]);

function setEngineLive(slot: 0 | 1, on: boolean): void {
  if (inputArmed()[slot] === on) return;
  sendEngine({ SetSlotLive: [slot, on] });
  setInputArmed((prev) => withAt(prev, slot, on));
}

// A tone reload of a live slot, from its unload to its resume: the slot counts as live meanwhile.
const reloading: [boolean, boolean] = [false, false];
// The player's latest GO LIVE press on a slot a source op holds (null = none): applied once the op
// settles (`src/ui/instrument/live.ts`); a reload's resume yields to a stop.
const liveIntent: [boolean | null, boolean | null] = [null, null];

/** A tone reload of live `slot` begins (`on`) or ends (`instrument.ts`). */
export function holdEngineLive(slot: 0 | 1, on: boolean): void {
  reloading[slot] = on;
}

/**
 * Inside `slot`'s serialized op (a tone reload, `instrument.ts`): make `slot` live again once its plugin
 * is back, unless the player pressed GO LIVE to stop it meanwhile (the latest press wins). The other
 * slot's live state is its own.
 */
export function resumeEngineLive(slot: 0 | 1): void {
  if (slotPlugins()[slot] && liveIntent[slot] !== false) setEngineLive(slot, true);
}

/** Whether `slot` is live, or will be once the source op holding it settles: the player's latest press
 * on it, else a reload's resume, else its state. */
export function liveIntended(slot: 0 | 1): boolean {
  return liveIntent[slot] ?? (reloading[slot] || inputArmed()[slot]);
}

/** Whether GO LIVE is in play on `slot`: live, held live by a reload, or pressed while an op holds it
 * (the named action keeps pressing that slot). */
export function liveInPlay(slot: 0 | 1): boolean {
  return liveIntent[slot] !== null || reloading[slot] || inputArmed()[slot];
}

/** Record the player's GO LIVE press on `slot` while a source op holds it. True for the first since the
 * last `takeLiveIntent`: the caller queues one apply behind the op. */
export function intendLive(slot: 0 | 1, on: boolean): boolean {
  const first = liveIntent[slot] === null;
  liveIntent[slot] = on;
  return first;
}

/** The recorded press, taken to apply it (null = none). */
export function takeLiveIntent(slot: 0 | 1): boolean | null {
  const on = liveIntent[slot];
  liveIntent[slot] = null;
  return on;
}

/** The looper facade's MIC: whether the device input runs dry through a live slot without a plugin. */
export function engineInputLive(): boolean {
  return ([0, 1] as const).some((s) => inputArmed()[s] && !slotPlugins()[s]);
}

/**
 * The looper facade's MIC toggle (the player sets a slot to Off and goes live instead): the device input
 * dry through a slot without a plugin, heard and recorded. On: an Off slot first, the active one first,
 * else a slot without a plugin. Off: every such live slot stops. With a plugin in both slots none is
 * empty, and it says so. Returns whether the input is live now.
 */
export function toggleEngineInput(): boolean {
  if (engineInputLive()) {
    for (const s of [0, 1] as const) if (!slotPlugins()[s]) setEngineLive(s, false);
    return false;
  }
  const active = activeSlot();
  const order = [active, active === 0 ? 1 : 0] as const;
  const empty = order.find((s) => !slotPlugins()[s] && slotOff()[s]) ?? order.find((s) => !slotPlugins()[s]);
  if (empty === undefined) {
    notifyError('Both slots hold a plugin', 'Set a slot to Off and go live on it to hear and record the input.');
    return false;
  }
  setEngineLive(empty, true);
  return true;
}

/** Tell a new engine (or one the WebView lost track of) which slots are live. */
export function resendEngineLive(): void {
  const [a, b] = inputArmed();
  sendEngine({ SetSlotLive: [0, a] }, { SetSlotLive: [1, b] });
}

/**
 * Go LIVE on `slot`: the engine's device is already open (Audio Settings picks it, the slot its
 * channel); going live only routes the slot's input, through its plugin or dry while the slot is Off.
 * One op on the slot's chain.
 */
export function goLive(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, async () => {
    if (slotPlugins()[slot] || slotOff()[slot]) setEngineLive(slot, true);
  });
}

/** Stop LIVE on `slot`. One op on the slot's chain; idempotent. */
export function stopLive(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, async () => setEngineLive(slot, false));
}

/**
 * Stop `slot`'s live input from inside an already-serialized slot op (load-swap / clear teardown in
 * `instrument.ts`), so it must NOT re-enter serializeSlot.
 */
export async function disarmInputInternal(slot: 0 | 1): Promise<void> {
  setEngineLive(slot, false);
}

/** Read-only reactive accessor: per-slot live flags. */
export { inputArmed };
