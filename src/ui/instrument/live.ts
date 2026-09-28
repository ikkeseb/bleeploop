import { createSignal } from 'solid-js';
import { slotPendingCounts, slotTakesInput } from '../../audio/instrument';
import { serializeSlot, withAt } from '../../audio/instrument-slots';
import { goLive, inputArmed, intendLive, liveIntended, stopLive, takeLiveIntent } from '../../audio/native-io';
import { readAudioDeviceSettings } from '../../audio/audio-settings';
import { usingAsio } from '../../audio/audio-devices';
import { engineMode, platform } from '../../platform';
import { notifyError } from '../../notify';

/**
 * GO LIVE per slot: the one press path for the slot's cap (`SlotControls.tsx`), a fresh effect pick's
 * auto-start (`PluginControls.tsx`) and the named action (`src/app/actions.ts`), with its in-flight and
 * error state kept per slot rather than per mounted cap.
 */
const [busy, setBusy] = createSignal<[boolean, boolean]>([false, false]);
const [error, setError] = createSignal<[string | null, string | null]>([null, null]);

/** Whether slot `slot`'s arm or stop is in flight. */
export const liveBusy = (slot: 0 | 1): boolean => busy()[slot];

/** The short reason slot `slot`'s last GO LIVE or stop failed, for the chip beside its cap. */
export const liveError = (slot: 0 | 1): string | null => error()[slot];

/** Drop slot `slot`'s failure chip (its cap unmounts with the source it served). */
export function clearLiveError(slot: 0 | 1): void {
  setError((prev) => withAt(prev, slot, null));
}

/** Whether slot `slot` shows a GO LIVE cap: its source takes input, and something can feed it (the
 * engine, or the web path's native plugin host). */
export function liveShown(slot: 0 | 1): boolean {
  return slotTakesInput(slot) && (engineMode() || platform.pluginHost.available);
}

/**
 * Toggle slot `slot`'s live input. `quiet` = the auto-start after a picker pick: the scan's effect flag
 * comes from the plugin's category, not its actual input bus, so a refusal is not an error the player
 * caused — no toast, no chip (the console.error stays: it feeds the release log). GO LIVE stays
 * available and reports when pressed.
 */
export async function toggleLive(slot: 0 | 1, quiet = false): Promise<void> {
  if (busy()[slot]) return; // single-flight: ignore re-presses while an arm/disarm is pending
  const wantLive = !inputArmed()[slot];
  setBusy((prev) => withAt(prev, slot, true));
  clearLiveError(slot);
  try {
    if (wantLive) {
      // The web path reads the capture device/channel + monitor output device chosen in Audio Settings
      // (persisted) at arm time; goLive arms input THEN monitor as one unit and mutes the web path.
      // ASIO uses the cached driver; the persisted Windows device IDs apply only to WASAPI. Engine mode
      // ignores all three: its device is open, and the slot's channel is the slot's own pick.
      const s = readAudioDeviceSettings();
      const ch = s.inputChannel === '' ? null : Number(s.inputChannel);
      await goLive(slot, usingAsio() ? null : s.inputDeviceId || null, ch, usingAsio() ? null : s.outputDeviceId || null);
    } else await stopLive(slot);
  } catch (e) {
    // The worth-surfacing failure is going live on a plugin with no audio-input bus (a pure synth in
    // the slot) — goLive rejects at the input-arm step. A cpal monitor-open failure also lands here
    // (goLive rolled the input back). A stop failure is rare; report generically.
    console.error('[PluginControls] go-live toggle failed', e);
    if (quiet) return;
    setError((prev) => withAt(prev, slot, wantLive ? 'input failed' : 'stop failed'));
    notifyError(wantLive ? "Couldn't start live input" : 'Live input stop failed', e);
  } finally {
    setBusy((prev) => withAt(prev, slot, false));
  }
}

/**
 * Press slot `slot`'s GO LIVE / INPUT LIVE cap, as a click would (the named action's; a pedal can press
 * while the cap is locked). While a source op holds the slot (a load, a session's tone reload) the press
 * is the player's latest intent, toggled from what the slot will be once the op settles and applied
 * then; a reload that would resume GO LIVE yields to a stop. False when there is nothing to press: a
 * source that takes no input, no host to feed it, or an arm or stop in flight.
 */
export function pressGoLive(slot: 0 | 1): boolean {
  if (busy()[slot]) return false;
  if (slotPendingCounts()[slot] > 0) {
    if (intendLive(slot, !liveIntended(slot))) void applyLiveIntent(slot);
    return true;
  }
  if (!liveShown(slot)) return false;
  void toggleLive(slot);
  return true;
}

/** Apply the press recorded while a source op held `slot`, once it (and what queued behind it) ran. */
async function applyLiveIntent(slot: 0 | 1): Promise<void> {
  await serializeSlot(slot, async () => {});
  const on = takeLiveIntent(slot);
  if (on === null || on === inputArmed()[slot] || !liveShown(slot)) return;
  await toggleLive(slot);
}
