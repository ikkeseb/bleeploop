/**
 * OWNS: the plain feed the stage's draw loop reads (invariant 6). Solid effects in `StageView.tsx` write
 * it when a signal changes; the 60 fps loop (`stage-loop.ts` and the view modules) only reads it, beside
 * the looper's non-reactive getters. No Solid import may enter this module or any module the loop
 * reaches: `verify/guards/stage-draw.mjs` checks that.
 */

export const LANES = 5;

/** A lane's display state, in the lanes' data-state vocabulary (`looper/lane-state.ts` `DATA_STATE`). */
export type LaneKind = 'empty' | 'stop' | 'play' | 'armed' | 'listening' | 'rec' | 'dub';

const each = <T>(value: T): T[] => Array.from({ length: LANES }, () => value);

export const feed = {
  kind: each<LaneKind>('empty'),
  /** The lane fades out, or stops at the loop end (`stopping` covers both). */
  fading: each(false),
  stopping: each(false),
  muted: each(false),
  volume: each(1),
  /** When the lane's refusal cue began, on `performance.now()`'s clock; 0 before the first. */
  cueAt: each(0),
  selected: 0,
  /** The beat the engine last made heard (0..3), and a count that steps with every one of them. */
  beat: 0,
  beatSeq: 0,
  /** A count-in numeral shows: the drawing dims under it. */
  counting: false,
  /**
   * The loop moves: some lane plays, overdubs or records, or a lane waits armed over a master loop
   * while no count runs. While it does not, the views show the loop cued at its start: the engine's
   * phase runs on with every lane stopped, but the next PLAY starts from the top.
   */
  moving: false,
  /** A device runs. */
  running: false,
  /** Beats in the master loop (bars x 4); 0 with no loop. */
  beatsPerLoop: 0,
  /** `prefers-reduced-motion`: contours, playhead and selection only. */
  reduced: false,
};
