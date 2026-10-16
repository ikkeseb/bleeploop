import { createSignal } from 'solid-js';
import { onEngineSend, type EngineCommand } from '../platform';
import { onEngineEvent } from '../ui/state/engine-store';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import { announceLooper, dismissLaneCue, refusalText } from '../ui/looper/gates';

/**
 * OWNS: a lane CLEAR's arm as the UI shows it, one arm whichever hand pressed. CLEAR is the engine's to
 * judge (invariant 3): the lane's CLR, the CLEAR key and a learned pedal all send the engine's CLEAR action
 * (`actions.ts`; a pedal's never passes the UI), the engine arms the lane on the first press (its
 * `ConfirmClear` on the feed) and clears it on a second press on that lane within its confirm window. Any
 * other looper press, a selection included, disarms it (lf-engine `Looper::action`, `engine.rs` `apply`).
 * So the arm is the engine's and this is its mirror: up on `ConfirmClear`, down when the window ends or a
 * looper press goes out. The pill reads SURE? (`Looper.tsx`) and the well "press again to clear"
 * (`lane-state.ts`) from this alone.
 *
 * The mirror errs on the side of showing the arm: it ends when the press that disarms is sent, which the
 * engine applies after, and its window starts when the feed brings the arm, after the engine's did. A
 * press in such a gap arms again; no lane reads unarmed while one press would clear it.
 */

const [armed, setArmed] = createSignal<number | null>(null);
let timer: ReturnType<typeof setTimeout> | undefined;

/** CLEAR is armed on lane `i`: its next CLEAR press clears it. */
export const clearArmed = (i: number): boolean => armed() === i;

/** The engine armed CLEAR on lane `i`: the lane shows it for the confirm window, and the screen reader
 * hears it. It replaces any lane cue up (one message at a time). */
function arm(i: number): void {
  clearTimeout(timer);
  dismissLaneCue();
  setArmed(i);
  announceLooper(`Track ${i + 1}: ${refusalText('ConfirmClear')}`);
  timer = setTimeout(disarmClear, CONFIRM_WINDOW_MS);
}

/** A looper press went out: the engine disarms on it (a CLEAR press too, which arms again or clears). */
export function disarmClear(): void {
  clearTimeout(timer);
  setArmed(null);
}

/** The engine commands that are a looper press to the engine, so disarm its CLEAR: `engine.rs` `apply`'s
 * list, a selection (`Looper::select`) and every action, CLEAR's own included (`Looper::action`). A
 * setting is none. */
const PRESSES: ReadonlySet<string> = new Set([
  'PlayAll',
  'StopAll',
  'ClearAll',
  'Press',
  'RecDub',
  'PlayStop',
  'Stop',
  'Undo',
  'Reverse',
  'Copy',
  'CopyWide',
  'Trim',
  'Clear',
  'SelectTrack',
  'Action',
  'ActionOn',
]);

const isPress = (c: EngineCommand): boolean => PRESSES.has(typeof c === 'string' ? c : Object.keys(c)[0]);

/** Mirror the engine's CLEAR arm: its `ConfirmClear` arms, a looper press the UI sends disarms (a pedal's
 * reaches `disarmClear` through `actions.ts` `nativePressed`). Returns the stop. */
export function startClearArm(): () => void {
  const stopEvents = onEngineEvent((ev) => {
    if (ev.type === 'Refused' && ev.reason === 'ConfirmClear') arm(ev.lane);
  });
  const stopSends = onEngineSend((c) => {
    if (isPress(c)) disarmClear();
  });
  return () => {
    stopEvents();
    stopSends();
    disarmClear();
  };
}
