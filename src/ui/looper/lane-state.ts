import { createMemo, type Accessor } from 'solid-js';
import { clock, looper, type TrackState } from '../state/audio';
import { laneCue } from './gates';

/**
 * OWNS: what a lane shows — its display state, the word it reads as, its well message and its count-in
 * numeral — derived once from the looper's public track signals, so the looper lanes (`Looper.tsx`) and
 * the stage view (`src/ui/stage/StageView.tsx`) can never disagree about a lane. Each surface keeps its
 * own vocabulary for the word (`LaneWord` → text); the derivation is shared.
 *
 * Reactive: `createLaneView` builds memos, so call it during component setup, never from the 60 fps
 * draw loop (invariant 6).
 */

/**
 * Display states add 'ARMED' — a later track that pressed REC but is still waiting for the master
 * loop boundary before its take begins. The engine reports this as RECORDING + `armed`; surfacing it
 * as its own state stops the lane from reading "recording" (red) for up to a full loop while nothing
 * is actually being kept (UX: the single most-bitten gap on every overdub), so it gets its own amber
 * treatment (data-state="armed").
 * LISTENING is the AUTO REC sibling: first-track REC is waiting for input rather than a known grid edge.
 */
type DisplayState = TrackState | 'ARMED' | 'LISTENING';

/** A display state in the lanes' data-state vocab (drives --sc and the per-state treatment in CSS). */
export const DATA_STATE: Readonly<Record<DisplayState, string>> = {
  EMPTY: 'empty',
  RECORDING: 'rec',
  ARMED: 'armed',
  LISTENING: 'listening',
  OVERDUBBING: 'dub',
  PLAYING: 'play',
  STOPPED: 'stop',
};

/** The word a lane reads as: its display state, unless a pending loop-end stop (ENDING), a mute over a
 * take that is not capturing (MUTED) or a rolling RETAKE (TAKE, with `retakePass`) outranks it. */
export type LaneWord = DisplayState | 'ENDING' | 'MUTED' | 'TAKE';

interface LaneView {
  displayState: Accessor<DisplayState>;
  word: Accessor<LaneWord>;
  /** A stop is pending at the loop end. */
  stopping: Accessor<boolean>;
  muted: Accessor<boolean>;
  /** A refused press's reason on this lane (gates.ts `refuseOnLane`), while the cue lasts; else ''. */
  cue: Accessor<string>;
  /** The well's message: the cue, else the pending stop, the armed wait or the AUTO REC listen; else ''. */
  wellMsg: Accessor<string>;
  /** The count-in numeral (4-3-2-1) of an armed FIRST take; 0 otherwise. */
  wellCount: Accessor<number>;
}

/** The shared view of lane `i`. */
export function createLaneView(i: number): LaneView {
  const track = looper.track(i);
  const stopping = () => track().stopAt !== null;
  const muted = () => looper.trackMuted(i);
  // RECORDING-but-armed becomes its own 'ARMED' (waiting-for-downbeat) state. Memoized so unrelated
  // public-track changes cannot re-run what hangs off it (the lane core's glyph is fresh JSX per call).
  const displayState = createMemo((): DisplayState => {
    const t = track();
    if (t.state !== 'RECORDING') return t.state;
    if (t.autoArmed) return 'LISTENING';
    return t.armed ? 'ARMED' : 'RECORDING';
  });
  // The word says what the lane SOUNDS like: a muted take that is playing or stopped reads MUTED (the
  // loop still runs — the playhead keeps moving); a live capture keeps its own word so REC/OVERDUB is
  // never hidden behind a mute. ENDING (stop at loop end) outranks both.
  const word = createMemo((): LaneWord => {
    const d = displayState();
    if (stopping()) return 'ENDING';
    if (muted() && (d === 'PLAYING' || d === 'STOPPED')) return 'MUTED';
    if (track().retakePass > 0) return 'TAKE'; // a rolling RETAKE counts its passes
    return d;
  });
  const cue = createMemo(() => {
    const c = laneCue();
    return c !== null && c.track === i ? c.text : '';
  });
  // Only ARMED/LISTENING and a pending stop carry a well message; EMPTY shows nothing (the lane's record
  // affordance already says "press to record"). A refusal cue outranks every message while it lasts.
  // A FIRST take (no master yet) is armed behind the forced count-in → the well counts it down big
  // (4-3-2-1, the numeral is clock.countLeft); a LATER take waits for the loop boundary → plain text.
  const wellMsg = () => {
    if (cue()) return cue();
    if (stopping()) return 'STOPPING AT LOOP END';
    if (displayState() === 'ARMED') return looper.masterLengthFrames() > 0 ? 'WAITING FOR DOWNBEAT' : 'COUNT-IN';
    if (displayState() === 'LISTENING') return 'WAITING FOR INPUT';
    return '';
  };
  const wellCount = () =>
    !cue() && displayState() === 'ARMED' && looper.masterLengthFrames() === 0 ? clock.countLeft() : 0;
  return { displayState, word, stopping, muted, cue, wellMsg, wellCount };
}
