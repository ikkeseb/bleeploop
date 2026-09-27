import { createSignal } from 'solid-js';
import { clock, looper } from '../state/audio';
import { engineMode, type Refusal } from '../../platform';

/**
 * OWNS: the looper UI's refusal gates, its screen-reader announcement line and the sighted lane cue.
 * One predicate per lane gesture (and per command-bar toggle a pedal reaches), returning WHY a press is
 * refused, so the control's disabled state, its title, its ARIA label and a key's or pedal's refusal
 * (`src/app/actions.ts`) all speak one vocabulary. The engine
 * keeps its own self-protection (machine.ts recDub/playStop); these mirror it for the UI, they do not
 * replace it. In engine mode a hands-free press is gated by the engine, which names its reason by the
 * same vocabulary (`refusalText`, `lf_engine::Refusal::text`).
 *
 * Reactive: the gates read the looper's public track signals, so call them from JSX, memos or an event
 * handler — never from the 60 fps draw loop (invariant 6).
 */
export type Gate = { ok: true } | { ok: false; reason: string };

/** Shared frozen results, one object per outcome, so a per-lane memo over a gate sees an unchanged
 * outcome as `===`-equal and settles instead of re-firing its disabled/title/aria subscribers. */
const OK: Gate = Object.freeze({ ok: true });
const refuse = (reason: string) => Object.freeze({ ok: false as const, reason });

/** The refusal vocabulary: short phrases a title, an aria-label and an announcement can all carry. */
const REFUSAL = {
  stopping: refuse('stopping at loop end, wait or stop now'),
  playFirst: refuse('play first to overdub'),
  reversed: refuse('overdub unavailable while reversed, switch to forward first'),
  otherRecording: refuse('another track is recording, stop it first'),
  empty: refuse('nothing to play, record first'),
  noUndo: refuse('nothing to undo, overdub first'),
  noClear: refuse('nothing to clear'),
  noMute: refuse('nothing to mute, record first'),
  noReverse: refuse('nothing to reverse, record first'),
  noCopy: refuse('nothing to copy, record first'),
  noFreeLane: refuse('no empty track to copy to'),
  tempoLocked: refuse('tempo locked to the loop, clear all to retap'),
  fixedCapturing: refuse('a take is recording, FIXED changes after it'),
  fixedRetake: refuse('RETAKE is on, so FIXED is ignored'),
  noInputFx: refuse('input effects need the native engine'),
} as const;

/** CLEAR's first press, by key or pedal: the second one clears (`src/app/actions.ts`). */
export const CONFIRM_CLEAR_TEXT = 'press again to clear';

/** The words for an engine refusal: the vocabulary above. */
export function refusalText(reason: Refusal): string {
  switch (reason) {
    case 'Stopping':
      return REFUSAL.stopping.reason;
    case 'PlayFirst':
      return REFUSAL.playFirst.reason;
    case 'Reversed':
      return REFUSAL.reversed.reason;
    case 'OtherRecording':
      return REFUSAL.otherRecording.reason;
    case 'Empty':
      return REFUSAL.empty.reason;
    case 'NoUndo':
      return REFUSAL.noUndo.reason;
    case 'NoClear':
      return REFUSAL.noClear.reason;
    case 'ConfirmClear':
      return CONFIRM_CLEAR_TEXT;
  }
}

/** Any lane capturing (RECORDING incl. armed/listening, or OVERDUBBING) other than `except`. */
function otherCapturing(except = -1): boolean {
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

/** May UNDO (the lane's ↶ DUB) act on lane `i`? Only with an overdub to swap, and not while ending. */
export function undoGate(i: number): Gate {
  const t = looper.track(i)();
  if (!t.canUndo) return REFUSAL.noUndo;
  if (t.stopAt !== null) return REFUSAL.stopping;
  return OK;
}

/** May CLEAR act on lane `i`? Only an EMPTY lane has nothing to clear; the confirm is the caller's. */
export function clearGate(i: number): Gate {
  return looper.track(i)().state === 'EMPTY' ? REFUSAL.noClear : OK;
}

/** May MUTE (the lane's MUTE cap) act on lane `i`? An EMPTY lane's cap is disabled. */
export function muteGate(i: number): Gate {
  return looper.track(i)().state === 'EMPTY' ? REFUSAL.noMute : OK;
}

/** May REV act on lane `i`? The cap shows once the lane has a loop and is disabled while it ends. */
export function reverseGate(i: number): Gate {
  const t = looper.track(i)();
  if (!t.canReverse) return REFUSAL.noReverse;
  if (t.stopAt !== null) return REFUSAL.stopping;
  return OK;
}

/** May COPY act on lane `i`? The cap shows once the lane has a loop and another lane is EMPTY. */
export function copyGate(i: number): Gate {
  if (!looper.track(i)().canReverse) return REFUSAL.noCopy;
  for (let j = 0; j < looper.trackCount; j++) if (looper.track(j)().state === 'EMPTY') return OK;
  return REFUSAL.noFreeLane;
}

/** May TAP set the tempo? Not once a loop has fixed it. */
export function tapGate(): Gate {
  return clock.bpmLocked() ? REFUSAL.tempoLocked : OK;
}

/** May FIXED be switched? Not while a capture reads it, nor while RETAKE (whose passes roll at the
 * loop's length) overrides it over a loop. */
export function fixedGate(): Gate {
  if (otherCapturing()) return REFUSAL.fixedCapturing;
  if (looper.retakeEnabled() && looper.masterLengthFrames() > 0) return REFUSAL.fixedRetake;
  return OK;
}

/** May an IN FX send be switched? Only the native engine has input sends. */
export function inputFxGate(): Gate {
  return engineMode() ? OK : REFUSAL.noInputFx;
}

// The looper's polite live-region text (rendered by Looper.tsx). `equals: false` so the same refusal
// pressed twice is written again rather than deduped away.
const [liveMsg, setLiveMsg] = createSignal('', { equals: false });
export { liveMsg };

/** Put `msg` on the looper's screen-reader status line. */
export function announceLooper(msg: string): void {
  setLiveMsg(msg);
}

/** The lane a keyboard press was refused on, and why. Looper.tsx shows `text` in that lane's well. */
type LaneCue = { track: number; text: string };

// The sighted twin of the announcement. One cue at a time: a newer one replaces it. Written by the key
// press and by this UI timer only, never by an audio-path timer (invariant 6).
const [laneCue, setLaneCue] = createSignal<LaneCue | null>(null);
export { laneCue };
let cueTimer: ReturnType<typeof setTimeout> | undefined;

/** How long a refusal stays on the lane. */
const CUE_MS = 1600;

/** Refuse a transport press on lane `i`: say why on the lane for `ms` and on the screen-reader line. */
export function refuseOnLane(i: number, reason: string, ms = CUE_MS): void {
  announceLooper(`Track ${i + 1}: ${reason}`);
  clearTimeout(cueTimer);
  setLaneCue({ track: i, text: reason });
  cueTimer = setTimeout(() => setLaneCue(null), ms);
}

/** Take the lane cue down early: an accepted press makes the old reason stale. */
export function dismissLaneCue(): void {
  clearTimeout(cueTimer);
  setLaneCue(null);
}
