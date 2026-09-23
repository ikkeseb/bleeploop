import { createSignal } from 'solid-js';
import { looper } from '../../audio/looper/looper';

/**
 * OWNS: the looper UI's refusal gates and its screen-reader announcement line. One predicate per lane
 * gesture, returning WHY a press is refused, so the lane button's disabled state, its title, its ARIA
 * label and the keyboard transport's refusal all speak one vocabulary. The engine keeps its own
 * self-protection (machine.ts recDub/playStop); these mirror it for the UI, they do not replace it.
 *
 * Reactive: the gates read the looper's public track signals, so call them from JSX, memos or an event
 * handler — never from the 60 fps draw loop (invariant 6).
 */
export type Gate = { ok: true } | { ok: false; reason: string };

/** Shared frozen results, one object per outcome, so a per-lane memo over a gate sees an unchanged
 * outcome as `===`-equal and settles instead of re-firing its disabled/title/aria subscribers. */
const OK: Gate = Object.freeze({ ok: true });
const refuse = (reason: string): Gate => Object.freeze({ ok: false, reason });

/** The refusal vocabulary: short phrases a title, an aria-label and an announcement can all carry. */
const REFUSAL = {
  stopping: refuse('stopping at loop end, wait or stop now'),
  playFirst: refuse('play first to overdub'),
  reversed: refuse('overdub unavailable while reversed, switch to forward first'),
  otherRecording: refuse('another track is recording, stop it first'),
  empty: refuse('nothing to play, record first'),
} as const;

/** Any lane capturing (RECORDING incl. armed/listening, or OVERDUBBING) other than `except`. */
function otherCapturing(except: number): boolean {
  for (let j = 0; j < looper.trackCount; j++) {
    if (j === except) continue;
    const s = looper.track(j)().state;
    if (s === 'RECORDING' || s === 'OVERDUBBING') return true;
  }
  return false;
}

/** A rolling RETAKE opens the EMPTY lanes' REC as its approve-and-record-next gesture. */
function retakeRolling(): boolean {
  for (let j = 0; j < looper.trackCount; j++) if (looper.track(j)().retakePass > 0) return true;
  return false;
}

/** May the REC/DUB core of lane `i` act now? A live capture on the lane itself can always be ended. */
export function recDubGate(i: number): Gate {
  const t = looper.track(i)();
  if (t.state === 'RECORDING' || t.state === 'OVERDUBBING') return OK;
  if (t.stopAt !== null) return REFUSAL.stopping;
  if (t.state === 'STOPPED') return REFUSAL.playFirst;
  if (t.state === 'PLAYING' && t.reversed) return REFUSAL.reversed;
  if (otherCapturing(i) && !(t.state === 'EMPTY' && retakeRolling())) {
    return REFUSAL.otherRecording;
  }
  return OK;
}

/** May the PLAY/STOP cap of lane `i` act now? Only an EMPTY lane has nothing to start or stop. */
export function playStopGate(i: number): Gate {
  return looper.track(i)().state === 'EMPTY' ? REFUSAL.empty : OK;
}

// The looper's polite live-region text (rendered by Looper.tsx). `equals: false` so the same refusal
// pressed twice is written again rather than deduped away.
const [liveMsg, setLiveMsg] = createSignal('', { equals: false });
export { liveMsg };

/** Put `msg` on the looper's screen-reader status line. */
export function announceLooper(msg: string): void {
  setLiveMsg(msg);
}
