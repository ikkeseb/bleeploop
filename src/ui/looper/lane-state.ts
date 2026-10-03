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
 * Display states add 'ARMED' — a track that pressed REC but is still waiting for its downbeat before
 * its take begins: the end of a count-in (a first take's, or a later take's armed while the loops are
 * stopped), or the master loop boundary (a later take armed beside playing loops). The engine reports
 * this as RECORDING + `armed`; surfacing it as its own state stops the lane from reading "recording"
 * (red) for up to a full loop while nothing is actually being kept (UX: the single most-bitten gap on
 * every overdub), so it gets its own amber treatment (data-state="armed").
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

/** The word a lane reads as: its display state, unless a fade (FADING), a pending loop-end
 * stop (ENDING), a mute over a take that is not capturing (MUTED) or a rolling RETAKE (TAKE, with
 * `retakePass`) outranks it. */
export type LaneWord = DisplayState | 'FADING' | 'ENDING' | 'MUTED' | 'TAKE';

interface LaneView {
  displayState: Accessor<DisplayState>;
  word: Accessor<LaneWord>;
  /** A stop is pending: at the loop end, or where a fade ends. */
  stopping: Accessor<boolean>;
  /** The lane fades out (FADE) and stops where the fade ends. */
  fading: Accessor<boolean>;
  muted: Accessor<boolean>;
  /** A refused press's reason on this lane (gates.ts `refuseOnLane`), while the cue lasts; else ''. */
  cue: Accessor<string>;
  /** The well's message: the cue, else the pending stop, the armed wait or the AUTO REC listen; else ''. */
  wellMsg: Accessor<string>;
  /** The count-in numeral (4-3-2-1) of an armed take that is counted in; 0 otherwise. */
  wellCount: Accessor<number>;
}

/** The shared view of lane `i`. */
export function createLaneView(i: number): LaneView {
  const track = looper.track(i);
  const stopping = () => track().stopAt !== null;
  const fading = () => track().fading === true;
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
  // never hidden behind a mute. FADING, and ENDING (stop at loop end), outrank both.
  const word = createMemo((): LaneWord => {
    const d = displayState();
    if (fading()) return 'FADING';
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
  // A take behind the forced count-in → the well counts it down big (4-3-2-1, the numeral is
  // clock.countLeft); a take waiting for the loop boundary → plain text. Which one is the engine's to
  // say: a FIRST take (no master yet) is always counted in, and a LATER take is counted in when the
  // engine counts (armed while every loop is stopped), which the engine store's reducer notes per lane
  // as the count's beats arrive (`looper.trackCounted`) and holds until the lane leaves its wait (the
  // numeral reads 0 before the first count beat is heard, and between the last one and the take). Never
  // guessed from which lanes play.
  const countedIn = () => displayState() === 'ARMED' && (looper.masterLengthFrames() === 0 || looper.trackCounted(i));
  const wellMsg = () => {
    if (cue()) return cue();
    if (fading()) return 'FADING OUT';
    if (stopping()) return 'STOPPING AT LOOP END';
    if (displayState() === 'ARMED') return countedIn() ? 'COUNT-IN' : 'WAITING FOR DOWNBEAT';
    if (displayState() === 'LISTENING') return 'WAITING FOR INPUT';
    return '';
  };
  const wellCount = () => (!cue() && countedIn() ? clock.countLeft() : 0);
  return { displayState, word, stopping, fading, muted, cue, wellMsg, wellCount };
}
