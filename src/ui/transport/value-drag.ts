import { createSignal, onCleanup, type Accessor } from 'solid-js';

/**
 * The command bar's one value gesture (the app.css header's KEYS: a key carries its value, changed by a
 * drag or a scroll on the key; no value fields or steppers beside it). Used by the value keys
 * (`ValueKey.tsx`), the tempo on the display and the master knob (`Transport.tsx`):
 * - a vertical drag: past a movement threshold the pointer is captured and every `pxPerStep` up or down
 *   is one step; a press that never passed the threshold stays a click (the key's own press);
 * - the wheel over it: one step a notch (a trackpad's small deltas add up to one);
 * - the arrows on the focused control: Up/Right a step up, Down/Left a step down; PageUp/PageDown `big`
 *   steps where the range is long; Home/End to its ends where the control has them (`edge`).
 * A press is tracked from pointerdown, before any capture, so a release, a cancel or a lost capture
 * anywhere ends it: a pointer let go off the key and brought back over it never drags (a hover with no
 * button held once stepped the value). Only the click a real drag ends is swallowed.
 * Each step calls `step`, which sends what today's control sent (the store's call); the feed shows the
 * outcome (invariant 3). `adjusting` is true while a gesture runs and a moment after it, so the value line
 * lights and the scale window shows. Its timer is a UI timer, never the audio path (invariant 6).
 */
export interface ValueDragOptions {
  /** Is the value live now? A dead value takes no gesture, and a press is the control's own. */
  enabled: Accessor<boolean>;
  /** One step up (1) or down (-1); `big` for PageUp/PageDown. */
  step: (dir: 1 | -1, big: boolean) => void;
  /** Pixels of vertical travel per step. */
  pxPerStep: number;
  /** Home (-1) and End (1): the value to its lowest or highest. Without it the two keys pass through. */
  edge?: (end: 1 | -1) => void;
  /** Keep the pointer's default (focus on press). A control that must not take focus from the pointer
   * (a knob `div`, which the transport keys' click blur does not reach) sets false. */
  focusOnPointer?: boolean;
}

export interface ValueDrag {
  adjusting: Accessor<boolean>;
  /** Spread on the element. */
  handlers: {
    onPointerDown: (e: PointerEvent) => void;
    onPointerMove: (e: PointerEvent) => void;
    onPointerUp: (e: PointerEvent) => void;
    onPointerCancel: (e: PointerEvent) => void;
    onLostPointerCapture: (e: PointerEvent) => void;
    onKeyDown: (e: KeyboardEvent) => void;
  };
  /** The wheel needs a non-passive listener: hand the element here (a `ref`). */
  ref: (el: HTMLElement) => void;
  /** True once for the click that ends a drag: the press it would be is swallowed. */
  takeDragClick: () => boolean;
}

/** Travel before a press becomes a drag (px). */
const DRAG_THRESHOLD = 4;
/** How long the value stays lit after a wheel or key step (ms). */
const LINGER_MS = 700;
/** Wheel travel per step: a mouse notch (100 px, or 3 lines) is one. */
const WHEEL_STEP = 50;

export function createValueDrag(opts: ValueDragOptions): ValueDrag {
  const [adjusting, setAdjusting] = createSignal(false);
  let linger: ReturnType<typeof setTimeout> | undefined;
  let drag: { id: number; y: number; moved: boolean } | null = null;
  let swallowClick = false;
  let swallowTimer: ReturnType<typeof setTimeout> | undefined;
  let wheelAcc = 0;

  const touch = (): void => {
    clearTimeout(linger);
    setAdjusting(true);
    linger = setTimeout(() => setAdjusting(false), LINGER_MS);
  };

  const stopTracking = (): void => {
    drag = null;
    window.removeEventListener('pointerup', onWindowRelease, true);
    window.removeEventListener('pointercancel', onWindowRelease, true);
  };
  // A press not yet captured gets its release wherever it lands: the window hears it (capture phase, so
  // nothing under the pointer can stop it) and the tracking ends.
  function onWindowRelease(e: PointerEvent): void {
    if (drag && e.pointerId === drag.id && !drag.moved) stopTracking();
  }
  onCleanup(() => {
    clearTimeout(linger);
    clearTimeout(swallowTimer);
    stopTracking();
  });

  const onPointerDown = (e: PointerEvent): void => {
    swallowClick = false;
    if (e.button !== 0 || !opts.enabled()) return;
    if (opts.focusOnPointer === false) e.preventDefault();
    stopTracking();
    drag = { id: e.pointerId, y: e.clientY, moved: false };
    window.addEventListener('pointerup', onWindowRelease, true);
    window.addEventListener('pointercancel', onWindowRelease, true);
  };
  const onPointerMove = (e: PointerEvent): void => {
    if (!drag || e.pointerId !== drag.id) return;
    // No button held: the release went unheard (a window that lost focus meanwhile). Not a drag.
    if ((e.buttons & 1) === 0 && !drag.moved) {
      stopTracking();
      return;
    }
    const el = e.currentTarget as HTMLElement;
    const dy = drag.y - e.clientY;
    if (!drag.moved) {
      if (Math.abs(dy) < DRAG_THRESHOLD) return;
      drag.moved = true;
      drag.y = e.clientY;
      el.setPointerCapture(e.pointerId);
      clearTimeout(linger);
      setAdjusting(true);
      return;
    }
    const steps = Math.trunc(dy / opts.pxPerStep);
    if (steps === 0 || !opts.enabled()) return;
    drag.y -= steps * opts.pxPerStep;
    const dir = steps > 0 ? 1 : -1;
    for (let k = 0; k < Math.abs(steps); k++) opts.step(dir, false);
  };
  const end = (e: PointerEvent): void => {
    if (!drag || e.pointerId !== drag.id) return;
    const el = e.currentTarget as HTMLElement;
    const moved = drag.moved;
    stopTracking();
    if (moved) {
      // The click this release makes is dispatched with it, in the same task: swallow that one only.
      if (e.type === 'pointerup') {
        swallowClick = true;
        clearTimeout(swallowTimer);
        swallowTimer = setTimeout(() => (swallowClick = false), 0);
      }
      if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
      touch();
    }
  };
  const onKeyDown = (e: KeyboardEvent): void => {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    if (opts.edge && (e.key === 'Home' || e.key === 'End')) {
      e.preventDefault();
      e.stopPropagation();
      if (!opts.enabled()) return;
      opts.edge(e.key === 'End' ? 1 : -1);
      touch();
      return;
    }
    const up = e.key === 'ArrowUp' || e.key === 'ArrowRight' || e.key === 'PageUp';
    const down = e.key === 'ArrowDown' || e.key === 'ArrowLeft' || e.key === 'PageDown';
    if (!up && !down) return;
    // The arrows are the transport's lane keys elsewhere (`transport-keys.ts`): a focused value control
    // keeps them, live or not, so a dead one never moves the lane selection under the player.
    e.preventDefault();
    e.stopPropagation();
    if (!opts.enabled()) return;
    opts.step(up ? 1 : -1, e.key === 'PageUp' || e.key === 'PageDown');
    touch();
  };
  const onWheel = (e: WheelEvent): void => {
    if (!opts.enabled()) return;
    e.preventDefault();
    wheelAcc += e.deltaMode === 1 ? e.deltaY * 33 : e.deltaY;
    if (Math.abs(wheelAcc) < WHEEL_STEP) return;
    opts.step(wheelAcc < 0 ? 1 : -1, false);
    wheelAcc = 0;
    touch();
  };

  return {
    adjusting,
    handlers: { onPointerDown, onPointerMove, onPointerUp: end, onPointerCancel: end, onLostPointerCapture: end, onKeyDown },
    ref: (el) => {
      el.addEventListener('wheel', onWheel, { passive: false });
      onCleanup(() => el.removeEventListener('wheel', onWheel));
    },
    takeDragClick: () => {
      const s = swallowClick;
      swallowClick = false;
      return s;
    },
  };
}
