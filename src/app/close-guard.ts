import { autosave } from '../audio/autosave';
import { looper } from '../audio/looper/looper';
import { notifyError } from '../notify';
import { confirmNativeClose, onNativeCloseRequested, platform } from '../platform';

/** How long the close waits for the recovery save before asking whether to close without it. */
const FLUSH_DEADLINE_MS = 5000;

/** "Jam in progress" = any lane not EMPTY — RECORDING/OVERDUBBING/PLAYING/STOPPED. */
function jamInProgress(): boolean {
  for (let i = 0; i < looper.trackCount; i++) if (looper.stateOf(i) !== 'EMPTY') return true;
  return false;
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
    // confirm() is a synchronous native WebView2 dialog. Flush the committed loops before approving
    // close. The empty path also flushes so CLEAR ALL followed immediately by close cannot resurrect
    // a stale recovery.
    let closePending = false;
    const closeWithRecovery = async () => {
      if (closePending) return;
      closePending = true;
      try {
        // A flush that never settles would trap the window just like a failed one (closePending stays
        // set, every later close press is swallowed) — so it gets a deadline and lands in the same ask.
        let deadline: ReturnType<typeof setTimeout> | undefined;
        const timedOut = new Promise<never>((_, reject) => {
          deadline = setTimeout(() => reject(new Error(`recovery save did not finish in ${FLUSH_DEADLINE_MS} ms`)), FLUSH_DEADLINE_MS);
        });
        await Promise.race([autosave.flush(), timedOut]).finally(() => clearTimeout(deadline));
      } catch (error) {
        console.error('[app] autosave flush before close failed', error);
        notifyError('Could not update recovery before closing', error);
        // A failed save must never trap the window: the user decides whether to lose the jam.
        if (!window.confirm('The loops could not be saved for recovery. Close anyway? They will be lost.')) {
          closePending = false;
          return;
        }
      }
      try {
        await confirmNativeClose();
      } catch (error) {
        console.error('[app] native close approval failed', error);
        notifyError('BleepLoop could not close', error);
        closePending = false;
      }
    };
    onNativeCloseRequested(() => {
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
