import { createSignal } from 'solid-js';
import { slotPendingCounts, slotTakesInput } from '../state/instrument';
import { serializeSlot, withAt } from '../state/instrument-slots';
import { goLive, holdPress, inputArmed, pressEngineLive, takeHeldPresses } from '../state/native-io';
import { notifyError } from '../../notify';

/**
 * GO LIVE per slot: the one press path for the slot's cap (`SlotControls.tsx`) and the named action
 * (`src/app/actions.ts`), and a fresh effect pick's auto-start (`PluginControls.tsx`), with their
 * in-flight and error state kept per slot rather than per mounted cap. A press is the engine's toggle
 * (`native-io.ts`): every accepted press reaches the engine in order, and none is dropped while another
 * is in flight (`liveBusy` only shows it).
 */
const [busy, setBusy] = createSignal<[number, number]>([0, 0]);
const [error, setError] = createSignal<[string | null, string | null]>([null, null]);

/** Whether a GO LIVE press or an auto-start on slot `slot` is in flight. */
export const liveBusy = (slot: 0 | 1): boolean => busy()[slot] > 0;

/** The short reason slot `slot`'s last GO LIVE or stop failed, for the chip beside its cap. */
export const liveError = (slot: 0 | 1): string | null => error()[slot];

/** Drop slot `slot`'s failure chip (its cap unmounts with the source it served). */
export function clearLiveError(slot: 0 | 1): void {
  setError((prev) => withAt(prev, slot, null));
}

/** Whether slot `slot` shows a GO LIVE cap: its source takes input. */
export function liveShown(slot: 0 | 1): boolean {
  return slotTakesInput(slot);
}

/** Run `send` as slot `slot`'s GO LIVE op, its failure told unless `quiet` (`wentLive`: the chip's and
 * the toast's words, as the press meant it when it was made). */
async function liveOp(slot: 0 | 1, quiet: boolean, wentLive: boolean, send: (ownToast: boolean) => Promise<void>): Promise<void> {
  setBusy((prev) => withAt(prev, slot, prev[slot] + 1));
  clearLiveError(slot);
  try {
    // The engine's device is open and the slot's channel is the slot's own pick: going live only routes.
    // A lost batch is told once, here, unless `quiet` (then the outbox's toast tells it).
    await send(!quiet);
  } catch (e) {
    // The engine did not get the slot's live flag (its batch was lost), or the slot's op chain failed.
    console.error('[PluginControls] go-live toggle failed', e);
    if (quiet) return;
    setError((prev) => withAt(prev, slot, wentLive ? 'input failed' : 'stop failed'));
    notifyError(wentLive ? 'Could not go live. Press GO LIVE again.' : 'Could not stop the live input. Press INPUT LIVE again.', e);
  } finally {
    setBusy((prev) => withAt(prev, slot, Math.max(0, prev[slot] - 1)));
  }
}

/**
 * Press slot `slot`'s GO LIVE / INPUT LIVE: one op on the slot's chain that sends the engine's toggle,
 * or a live-off once the slot's source takes no input. Every call is a press, even one made while
 * another is in flight.
 */
export function toggleLive(slot: 0 | 1): Promise<void> {
  const wentLive = !inputArmed()[slot];
  return liveOp(slot, false, wentLive, (ownToast) => serializeSlot(slot, () => pressEngineLive(slot, slotTakesInput(slot), ownToast)));
}

/**
 * A fresh effect pick's auto-start: GO LIVE on, outright. Quiet: the scan's effect flag comes from the
 * plugin's category, not its actual input bus, so a refusal is not an error the player caused — no
 * toast, no chip (the console.error stays: it feeds the release log). GO LIVE stays available and
 * reports when pressed.
 */
export function startLive(slot: 0 | 1): Promise<void> {
  return liveOp(slot, true, true, (ownToast) => goLive(slot, ownToast));
}

/**
 * Press slot `slot`'s GO LIVE / INPUT LIVE cap, as a click would (the named action's; a pedal can press
 * while the cap is locked). While an op holds the slot (a load, a session's tone reload, a press still
 * in flight) the press is counted and applied once the op settles: an odd count as one toggle against
 * the engine's value then, an even count not at all; a reload that would resume GO LIVE yields to an
 * odd count. False when there is nothing to press: a source that takes no input, or no host to feed it.
 */
export function pressGoLive(slot: 0 | 1): boolean {
  if (slotPendingCounts()[slot] > 0) {
    if (holdPress(slot)) void applyHeldPresses(slot);
    return true;
  }
  if (!liveShown(slot)) return false;
  void toggleLive(slot);
  return true;
}

/** Apply the presses counted while an op held `slot`, once it (and what queued behind it) ran. */
async function applyHeldPresses(slot: 0 | 1): Promise<void> {
  await serializeSlot(slot, async () => {});
  if (takeHeldPresses(slot) % 2 === 0 || !liveShown(slot)) return;
  await toggleLive(slot);
}
