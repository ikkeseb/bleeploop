import { createSignal } from 'solid-js';
import { sendEngine, sendEngineReported } from '../../platform';
import { notifyError } from '../../notify';
import { activeSlot, serializeLive, slotOff, slotPlugins } from './instrument-slots';

/**
 * OWNS: which slots hear their own input (GO LIVE, the engine's `Toggle::Live` and `SetSlotLive`; the
 * capture channel is the slot's pick, `engine-store.ts`). The engine passes a live slot's input through
 * the slot's effect, or dry through a slot without a plugin; both slots may be live at once, and the
 * engine monitors in its own callback. The GO LIVE paths are serialised on the same per-slot chain as
 * load/unload (`instrument-slots.ts`) so they can never interleave with a plugin swap;
 * `disarmInputInternal` is exported for `instrument.ts`'s teardown paths, which are already inside a
 * serialised op and must not re-enter the chain.
 *
 * The engine owns the flags (invariant 3): `inputArmed` is the feed's `live` (`adoptEngineLive`, from
 * `engine-store.ts`), never the UI's own guess. A press sends the engine's toggle, which flips the value
 * the engine has when it lands, so two quick presses, a pedal and the cap, or a press racing the feed
 * never undo each other; what means a value (an unload's live-off, a reload's resume, an effect pick's
 * auto-start) sends `SetSlotLive`. The cap follows once the engine applied it; a batch that did not
 * reach the host leaves the flag where the engine has it, and GO LIVE's press says so once (`live.ts`).
 */

// Whether each slot hears its own input, as the engine applied it (the feed's `live`).
const [inputArmed, setInputArmed] = createSignal<[boolean, boolean]>([false, false]);

/** The feed's live flags (`engine-store.ts`): the only writer of `inputArmed`. */
export function adoptEngineLive(live: readonly [boolean, boolean]): void {
  const [a, b] = inputArmed();
  if (a !== live[0] || b !== live[1]) setInputArmed([live[0], live[1]]);
}

/**
 * Set `slot`'s live flag. Resolves with the batch's submission (`sendEngine`; false: it did not reach
 * the host). The slot shows it once the feed does. `ownToast`: the caller tells the player of a lost
 * batch (`sendEngineReported`), so the outbox only logs it.
 */
function setEngineLive(slot: 0 | 1, on: boolean, ownToast = false): Promise<boolean> {
  return (ownToast ? sendEngineReported : sendEngine)({ SetSlotLive: [slot, on] });
}

// A tone reload of a live slot, from its unload to its resume: the slot counts as live meanwhile.
const reloading: [boolean, boolean] = [false, false];
// GO LIVE presses on a slot a source op holds, counted until the op settles: an odd count is one toggle
// then (`src/ui/instrument/live.ts`), an even count none, and a reload's resume yields to an odd count.
const heldPresses: [number, number] = [0, 0];

/** A tone reload of live `slot` begins (`on`) or ends (`instrument.ts`). */
export function holdEngineLive(slot: 0 | 1, on: boolean): void {
  reloading[slot] = on;
}

/**
 * Inside `slot`'s serialized op (a tone reload, `instrument.ts`): make `slot` live again once its plugin
 * is back, unless the player pressed GO LIVE an odd number of times meanwhile (the unload left it off, so
 * that is the press's stop). The presses are taken either way. The other slot's live state is its own.
 */
export function resumeEngineLive(slot: 0 | 1): void {
  const stopped = takeHeldPresses(slot) % 2 === 1;
  if (slotPlugins()[slot] && !stopped) void setEngineLive(slot, true);
}

/** Whether GO LIVE is in play on `slot`: live, held live by a reload, or pressed while an op holds it
 * (the named action keeps pressing that slot). */
export function liveInPlay(slot: 0 | 1): boolean {
  return heldPresses[slot] > 0 || reloading[slot] || inputArmed()[slot];
}

/** Count a GO LIVE press on `slot` while a source op holds it. True for the first since the last
 * `takeHeldPresses`: the caller queues one apply behind the op. */
export function holdPress(slot: 0 | 1): boolean {
  heldPresses[slot]++;
  return heldPresses[slot] === 1;
}

/** The presses counted on `slot` since the last take, taken to apply them (0 = none). */
export function takeHeldPresses(slot: 0 | 1): number {
  const n = heldPresses[slot];
  heldPresses[slot] = 0;
  return n;
}

/** The looper facade's MIC: whether the device input runs dry through a live slot without a plugin. */
export function engineInputLive(): boolean {
  return ([0, 1] as const).some((s) => inputArmed()[s] && !slotPlugins()[s]);
}

/**
 * The looper facade's MIC, set outright (the native probes' path; the player sets a slot to Off and goes
 * live instead): the device input dry through a slot without a plugin, heard and recorded. On: an Off
 * slot first, the active one first, else a slot without a plugin. Off: every such slot stops. With a
 * plugin in both slots none is empty, and it says so. Resolves true once the host took every flag it
 * sent (the feed shows them after), false when there was no slot or a batch did not reach the host.
 */
export async function setEngineInput(on: boolean): Promise<boolean> {
  const empty = ([0, 1] as const).filter((s) => !slotPlugins()[s]);
  if (!on) return (await Promise.all(empty.map((s) => setEngineLive(s, false)))).every(Boolean);
  const active = activeSlot();
  const order = [active, active === 0 ? 1 : 0] as const;
  const slot = order.find((s) => !slotPlugins()[s] && slotOff()[s]) ?? order.find((s) => !slotPlugins()[s]);
  if (slot === undefined) {
    notifyError('Could not go live, as both slots hold a plugin. Set a slot to Off and go live on it.', 'An Off slot hears and records the input dry.');
    return false;
  }
  return setEngineLive(slot, true);
}

/** On the page's first reset frame: no slot is live for a fresh page, so a reloaded one disarms what the
 * engine still has live. */
export function disarmEngineLive(): void {
  sendEngine({ SetSlotLive: [0, false] }, { SetSlotLive: [1, false] });
}

/** Throw when the engine did not get `slot`'s live flag. `ownToast`: the caller's refusal path tells the
 * player (GO LIVE's press, `live.ts`), so the outbox does not. */
async function sendLive(slot: 0 | 1, on: boolean, ownToast: boolean): Promise<void> {
  if (!(await setEngineLive(slot, on, ownToast))) throw new Error(`The batch carrying SetSlotLive ${on} did not reach the engine.`);
}

/**
 * GO LIVE's press on `slot`, inside its serialized op (`live.ts`): the engine's toggle when the slot's
 * source takes input (`takesInput`), so the engine flips the value it has when the press lands, else a
 * live-off. Rejects when the batch did not reach the host. `ownToast` as `sendLive`.
 */
export async function pressEngineLive(slot: 0 | 1, takesInput: boolean, ownToast: boolean): Promise<void> {
  if (!takesInput) return sendLive(slot, false, ownToast);
  const took = await (ownToast ? sendEngineReported : sendEngine)({ Action: { Toggle: { Live: slot } } });
  if (!took) throw new Error('The batch carrying GO LIVE did not reach the engine.');
}

/**
 * Go LIVE on `slot`: the engine's device is already open (Audio Settings picks it, the slot its
 * channel); going live only routes the slot's input, through its plugin or dry while the slot is Off.
 * One op on the slot's chain; rejects when the batch did not reach the host (the slot stays as the feed
 * shows it).
 * `ownToast`: the caller toasts that rejection itself (`sendLive`).
 */
export function goLive(slot: 0 | 1, ownToast = false): Promise<void> {
  return serializeLive(slot, async () => {
    if (slotPlugins()[slot] || slotOff()[slot]) await sendLive(slot, true, ownToast);
  });
}

/** Stop LIVE on `slot`. One op on the slot's chain; idempotent; rejects as `goLive` does. */
export function stopLive(slot: 0 | 1, ownToast = false): Promise<void> {
  return serializeLive(slot, () => sendLive(slot, false, ownToast));
}

/**
 * Stop `slot`'s live input from inside an already-serialized slot op (load-swap / clear teardown in
 * `instrument.ts`), so it must NOT re-enter the slot's chain. Resolves with the batch's submission (false:
 * it did not reach the host): an unload waits for it, since a slot left live without its plugin passes
 * the input dry; a source change with no plugin to unload need not.
 */
export function disarmInputInternal(slot: 0 | 1): Promise<boolean> {
  return setEngineLive(slot, false);
}

/** Read-only reactive accessor: per-slot live flags. */
export { inputArmed };
