import { autosave } from '../session/autosave';
import { rigRecallOnClose } from '../ui/state/rig-recall';
import { notifyError } from '../notify';
import { confirmNativeClose, onNativeCloseRequested, platform } from '../platform';
import { looper } from '../ui/state/audio';

/** How long the close waits for the recovery save before asking whether to close without it. */
const FLUSH_DEADLINE_MS = 5000;

/** "Jam in progress" = any lane not EMPTY — RECORDING/OVERDUBBING/PLAYING/STOPPED. */
export function jamInProgress(): boolean {
  for (let i = 0; i < looper.trackCount; i++) if (looper.stateOf(i) !== 'EMPTY') return true;
  return false;
}

/** A quit is running its sequence (a close's or an update's): the other waits it out by not starting. */
let quitting = false;

/**
 * The one quit sequence a close and an update share: save the loops for recovery from the engine's
 * own snapshot (`autosave.quitSave`, which never skips a take the feed does not show yet), bounded at
 * `FLUSH_DEADLINE_MS`, then `quit` (a close: the native approval; an update: the installer). The empty
 * looper saves too, so CLEAR ALL followed at once by a quit cannot resurrect a stale recovery. Only one
 * runs at a time: a second asked meanwhile does nothing.
 *
 * A failed or timed-out save asks a close whether to close anyway (a failed save must never trap the
 * window); an update waits instead, says so and the jam plays on, and its installer never starts
 * before the save's transaction completed. False: nothing quit (another quit ran, or the save failed
 * and the player stayed, or the update waits). `quit`'s own failure rejects, the sequence ended.
 */
export async function quitWithSave(kind: 'close' | 'update', quit: () => Promise<void>): Promise<boolean> {
  if (quitting) return false;
  quitting = true;
  try {
    try {
      // A save that never settles would trap the window just like a failed one, so it gets a deadline
      // and lands in the same branch.
      let deadline: ReturnType<typeof setTimeout> | undefined;
      const timedOut = new Promise<never>((_, reject) => {
        deadline = setTimeout(() => reject(new Error(`recovery save did not finish in ${FLUSH_DEADLINE_MS} ms`)), FLUSH_DEADLINE_MS);
      });
      await Promise.race([autosave.quitSave(), timedOut]).finally(() => clearTimeout(deadline));
    } catch (error) {
      if (kind === 'update') {
        console.error('[app] autosave flush before the update failed', error);
        notifyError('Could not save the loops, so the update waits. Press Update to try again.', error);
        return false;
      }
      console.error('[app] autosave flush before close failed', error);
      notifyError('Could not save the loops for recovery before closing. Export them to keep them.', error);
      if (!window.confirm('The loops could not be saved for recovery. Close anyway? They will be lost.')) return false;
    }
    // A clean quit: a rig restored this launch comes back at the next one (`rig-recall.ts`).
    rigRecallOnClose();
    await quit();
    return true;
  } finally {
    quitting = false;
  }
}

/**
 * Close guard + recovery: a window close (or tab close/reload on the web tier) must not silently
 * discard a jam. Native tier: Rust vetoes CloseRequested and forwards it here; the frontend confirms,
 * flushes the committed loops, then approves the close. Web tier: the standard beforeunload veto (the
 * browser shows its own generic dialog) — NOT registered under Tauri, where the Rust guard owns close
 * and a beforeunload veto could fight the approved window.close().
 *
 * Returns a dispose fn (web tier removes its listener; the native subscription is app-lifetime).
 */
export function installCloseGuard(): () => void {
  if (platform.kind === 'tauri') {
    // confirm() is a synchronous native WebView2 dialog. Save the loops before approving the close
    // (`quitWithSave`).
    const closeWithRecovery = async () => {
      try {
        await quitWithSave('close', confirmNativeClose);
      } catch (error) {
        console.error('[app] native close approval failed', error);
        notifyError('Could not close BleepLoop. Close it again.', error);
      }
    };
    onNativeCloseRequested(() => {
      if (quitting) return;
      if (!jamInProgress()) {
        void closeWithRecovery();
        return;
      }
      if (
        window.confirm(
          'Close BleepLoop? The latest committed loops will be saved locally. Audio still being recorded is not included.',
        )
      ) {
        void closeWithRecovery();
      }
    });
    return () => {};
  }
  const onBeforeUnload = (e: BeforeUnloadEvent) => {
    if (!jamInProgress()) return;
    e.preventDefault();
    // Legacy engines require returnValue to actually show the dialog.
    e.returnValue = '';
  };
  window.addEventListener('beforeunload', onBeforeUnload);
  return () => window.removeEventListener('beforeunload', onBeforeUnload);
}
