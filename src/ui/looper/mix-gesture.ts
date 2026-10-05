import { onCleanup } from 'solid-js';
import { looper } from '../state/audio';
import type { MixKey } from '../state/engine-store';

/**
 * A lane mix control's side of the overlay rule (`src/ui/state/engine-store.ts`, the lane mix section):
 * the control shows its gesture's value until the engine has it, and its disposal ends that.
 */

/** Lane `lane`'s control `key` drops its overlay when it is disposed (an FX param unmounted with its
 * bypassed effect, the drawer closed). Call it in the control's component. */
export function mixDisposal(lane: number, key: MixKey): void {
  onCleanup(() => looper.dropMix(lane, key));
}

/**
 * The handlers of a continuous control's gesture (a slider), for lane `lane`'s control `key`: a pointer
 * down or a key held starts it; the pointer's release anywhere (the window's), its cancel, a lost capture,
 * the key's release, the control's blur and the window's blur end it. While it runs, no `Mix` ends the
 * overlay. Registers the disposal too. Call it in the control's component.
 */
export function mixGesture(lane: number, key: MixKey) {
  const end = (): void => {
    window.removeEventListener('pointerup', end, true);
    window.removeEventListener('pointercancel', end, true);
    window.removeEventListener('blur', end);
    looper.holdMix(lane, key, false);
  };
  /** A hold starts: the window losing focus ends it too (a release it will never see). */
  const start = (pointer: boolean): void => {
    looper.holdMix(lane, key, true);
    if (pointer) {
      window.addEventListener('pointerup', end, true);
      window.addEventListener('pointercancel', end, true);
    }
    window.addEventListener('blur', end);
  };
  onCleanup(end);
  mixDisposal(lane, key);
  return {
    onPointerDown: (): void => start(true),
    onLostPointerCapture: end,
    onKeyDown: (): void => start(false),
    onKeyUp: end,
    onBlur: end,
  };
}
