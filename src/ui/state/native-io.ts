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
 *
 * The engine echoes no live flag (`SetSlotLive` in `src-tauri/crates/lf-engine/src/engine.rs` sends no
 * event), so the UI's flag is the one it sent: shown at once, and taken back when the batch carrying it
 * did not reach the host (`setEngineLive`).
 */

// Whether each slot hears its own input.
const [inputArmed, setInputArmed] = createSignal<[boolean, boolean]>([false, false]);

// Each slot's latest `SetSlotLive` send: a lost one takes its flip back only while no later one followed.
const liveSends: [number, number] = [0, 0];

/**
 * Send `slot`'s live flag and show it. Resolves with the batch's submission (`sendEngine`): false takes
 * the flip back, unless a later send for the slot followed. False can also mean native MIDI dropped a
 * press that rode in the same batch while this setting reached the engine; the slot then reads not live
 * while it is, and the next GO LIVE press sends live again.
 */
function setEngineLive(slot: 0 | 1, on: boolean): Promise<boolean> {
  if (inputArmed()[slot] === on) return Promise.resolve(true);
  const send = ++liveSends[slot];
  const took = sendEngine({ SetSlotLive: [slot, on] });
  setInputArmed((prev) => withAt(prev, slot, on));
  return took.then((ok) => {
    // The outbox logged and toasted the lost batch; GO LIVE's press reports it too (`live.ts`).
    if (!ok && liveSends[slot] === send) setInputArmed((prev) => withAt(prev, slot, !on));
    return ok;
  });
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
  if (slotPlugins()[slot] && liveIntent[slot] !== false) void setEngineLive(slot, true);
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
    for (const s of [0, 1] as const) if (!slotPlugins()[s]) void setEngineLive(s, false);
    return false;
  }
  const active = activeSlot();
  const order = [active, active === 0 ? 1 : 0] as const;
  const empty = order.find((s) => !slotPlugins()[s] && slotOff()[s]) ?? order.find((s) => !slotPlugins()[s]);
  if (empty === undefined) {
    notifyError('Could not go live, as both slots hold a plugin. Set a slot to Off and go live on it.', 'An Off slot hears and records the input dry.');
    return false;
  }
  void setEngineLive(empty, true);
  return true;
}

/** Tell a new engine (or one the WebView lost track of) which slots are live. */
export function resendEngineLive(): void {
  const [a, b] = inputArmed();
  sendEngine({ SetSlotLive: [0, a] }, { SetSlotLive: [1, b] });
}

/** Throw when the engine did not get `slot`'s live flag: the caller's refusal path reports it. */
async function sendLive(slot: 0 | 1, on: boolean): Promise<void> {
  if (!(await setEngineLive(slot, on))) throw new Error(`The batch carrying SetSlotLive ${on} did not reach the engine.`);
}

/**
 * Go LIVE on `slot`: the engine's device is already open (Audio Settings picks it, the slot its
 * channel); going live only routes the slot's input, through its plugin or dry while the slot is Off.
 * One op on the slot's chain; rejects when the engine did not get it (the slot reads not live again).
 */
export function goLive(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, async () => {
    if (slotPlugins()[slot] || slotOff()[slot]) await sendLive(slot, true);
  });
}

/** Stop LIVE on `slot`. One op on the slot's chain; idempotent; rejects as `goLive` does. */
export function stopLive(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, () => sendLive(slot, false));
}

/**
 * Stop `slot`'s live input from inside an already-serialized slot op (load-swap / clear teardown in
 * `instrument.ts`), so it must NOT re-enter serializeSlot. It does not wait for the submission.
 */
export async function disarmInputInternal(slot: 0 | 1): Promise<void> {
  void setEngineLive(slot, false);
}

/** Read-only reactive accessor: per-slot live flags. */
export { inputArmed };
