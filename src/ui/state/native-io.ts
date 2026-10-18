import { createSignal } from 'solid-js';
import { sendEngine, sendEngineReported } from '../../platform';
import { notifyError } from '../../notify';
import { activeSlot, serializeSlot, slotOff, slotPlugins } from './instrument-slots';

/**
 * OWNS: which slots hear their own input (GO LIVE, the engine's `SetSlotLive`; the capture channel is
 * the slot's pick, `engine-store.ts`). The engine passes a live slot's input through the slot's effect,
 * or dry through a slot without a plugin; both slots may be live at once, and the engine monitors in its
 * own callback. The GO LIVE paths are serialised on the same per-slot chain as load/unload
 * (`instrument-slots.ts`) so they can never interleave with a plugin swap; `disarmInputInternal` is
 * exported for `instrument.ts`'s teardown paths, which are already inside a serialised op and must not
 * re-enter the chain.
 *
 * The engine owns the flags (invariant 3): `inputArmed` is the feed's `live` (`adoptEngineLive`, from
 * `engine-store.ts`), never the UI's own guess. A press sends `SetSlotLive` and the cap follows once the
 * engine applied it; a batch that did not reach the host leaves the flag where the engine has it, and
 * GO LIVE's press says so once (`live.ts`).
 */

// Whether each slot hears its own input, as the engine applied it (the feed's `live`).
const [inputArmed, setInputArmed] = createSignal<[boolean, boolean]>([false, false]);

// Each slot's live flag last sent and not yet on the feed (null: none), and when: the engine applies the
// commands in the order they were sent, so a GO LIVE press before the feed shows the last one toggles
// from it, not from the feed's older value (two quick presses send on, off; not on, on). It lapses after
// `SENT_LIVE_MS`, so a flag the feed never shows cannot hold the toggle's base for good.
const sentLive: [{ on: boolean; at: number } | null, { on: boolean; at: number } | null] = [null, null];
const SENT_LIVE_MS = 2000;

/** The feed's live flags (`engine-store.ts`): the only writer of `inputArmed`. */
export function adoptEngineLive(live: readonly [boolean, boolean]): void {
  for (const s of [0, 1] as const) if (sentLive[s]?.on === live[s]) sentLive[s] = null;
  const [a, b] = inputArmed();
  if (a !== live[0] || b !== live[1]) setInputArmed([live[0], live[1]]);
}

/** What GO LIVE toggles `slot` from: the live flag last sent while the feed does not show it yet, else
 * the feed's. */
export function liveToggleBase(slot: 0 | 1): boolean {
  const sent = sentLive[slot];
  if (sent && performance.now() - sent.at < SENT_LIVE_MS) return sent.on;
  sentLive[slot] = null;
  return inputArmed()[slot];
}

/**
 * Send `slot`'s live flag. Resolves with the batch's submission (`sendEngine`; false: it did not reach
 * the host, and the toggle's base is the feed's again). The slot shows it once the feed does.
 * `ownToast`: the caller tells the player of a lost batch (`sendEngineReported`), so the outbox only
 * logs it.
 */
function setEngineLive(slot: 0 | 1, on: boolean, ownToast = false): Promise<boolean> {
  const sent = { on, at: performance.now() };
  sentLive[slot] = inputArmed()[slot] === on ? null : sent;
  return (ownToast ? sendEngineReported : sendEngine)({ SetSlotLive: [slot, on] }).then((took) => {
    if (!took && sentLive[slot] === sent) sentLive[slot] = null;
    return took;
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
  return liveIntent[slot] ?? (reloading[slot] || liveToggleBase(slot));
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
 * empty, and it says so. Returns whether it asked for live (the flag follows on the feed).
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

/** On a reset frame, before its flags are adopted: tell a new engine the slots the feed last showed live
 * (the settings replay has them too), and the engine a reloaded page finds that none is (a fresh page
 * shows none). */
export function resendEngineLive(): void {
  const [a, b] = inputArmed();
  sendEngine({ SetSlotLive: [0, a] }, { SetSlotLive: [1, b] });
}

/** Throw when the engine did not get `slot`'s live flag. `ownToast`: the caller's refusal path tells the
 * player (GO LIVE's press, `live.ts`), so the outbox does not. */
async function sendLive(slot: 0 | 1, on: boolean, ownToast: boolean): Promise<void> {
  if (!(await setEngineLive(slot, on, ownToast))) throw new Error(`The batch carrying SetSlotLive ${on} did not reach the engine.`);
}

/**
 * Go LIVE on `slot`: the engine's device is already open (Audio Settings picks it, the slot its
 * channel); going live only routes the slot's input, through its plugin or dry while the slot is Off.
 * One op on the slot's chain; rejects when the batch did not reach the host (the slot stays as the feed
 * shows it).
 * `ownToast`: the caller toasts that rejection itself (`sendLive`).
 */
export function goLive(slot: 0 | 1, ownToast = false): Promise<void> {
  return serializeSlot(slot, async () => {
    if (slotPlugins()[slot] || slotOff()[slot]) await sendLive(slot, true, ownToast);
  });
}

/** Stop LIVE on `slot`. One op on the slot's chain; idempotent; rejects as `goLive` does. */
export function stopLive(slot: 0 | 1, ownToast = false): Promise<void> {
  return serializeSlot(slot, () => sendLive(slot, false, ownToast));
}

/**
 * Stop `slot`'s live input from inside an already-serialized slot op (load-swap / clear teardown in
 * `instrument.ts`), so it must NOT re-enter serializeSlot. Resolves with the batch's submission (false:
 * it did not reach the host): an unload waits for it, since a slot left live without its plugin passes
 * the input dry; a source change with no plugin to unload need not.
 */
export function disarmInputInternal(slot: 0 | 1): Promise<boolean> {
  return setEngineLive(slot, false);
}

/** Read-only reactive accessor: per-slot live flags. */
export { inputArmed };
