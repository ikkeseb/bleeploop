/**
 * OWNS: the looper's pure grid arithmetic — the decisions that turn a raw take into a master loop, choose
 * and tile a later take's whole-bar window, anchor it to the count grid, and split a captured batch at an
 * arm boundary.
 *
 * PURE ON PURPOSE: no engine, clock, Tone or engineState import, so this module runs under Node and the
 * `verify/` guards IMPORT it instead of hand-mirroring it (each import here deletes a MIRRORS drift tag —
 * see verify/README.md). Keep it that way: every function takes its clock/state as arguments.
 */
import { clampBars, framesPerBar, maxWholeBars } from '../quantize.ts'; // explicit .ts: Node runs this file

/** Small safety margin (s) when scheduling boundaries — the worklet-arm lead. */
export const HEARTBEAT_INTERNAL_LATENCY = 0.02;
/**
 * First-track count-in: one bar (4 beats) of forced-audible click before the take begins, so the loop
 * downbeat is a counted "1" (not wherever the button was pressed) and there's no recorded dead air at
 * the head. Always on for first-track record (the take arms frame-exact to the beat after the count).
 */
export const COUNT_IN_BEATS = 4;

/** Phase of `t` within a `period`-long grid anchored at `anchor`, wrapped into [0, period). */
export function phaseOffset(t: number, anchor: number, period: number): number {
  return (((t - anchor) % period) + period) % period;
}

export interface CommitPlan {
  fpb: number; // integer frames per bar at `bpm`
  bars: number; // whole COMPLETED bars the take floors to (≥ 1, ≤ what fits `recordLength`)
  master: number; // bars * fpb — the master loop length, integer frames
  derivedBpm: number; // the bpm the integer frame count implies (display guard; the period never uses it)
  period: number; // master / sr (s)
  beatPeriod: number; // the exact quarter note from the integer frame count
}

/**
 * The first-track commit: FLOOR the raw take to the largest number of COMPLETED whole bars (not
 * round-to-nearest — a free-record stop lands a hair AFTER the targeted downbeat, so keep the bars you
 * finished and drop the overshoot; this is also what kills the commit grid-hop), clamped to whole bars that
 * FIT the record buffer, and derive the exact per-beat period from the integer frame count.
 */
export function planCommit(rawFrames: number, bpm: number, sr: number, recordLength: number): CommitPlan {
  const fpb = framesPerBar(bpm, sr);
  const bars = clampBars(Math.floor(rawFrames / fpb), maxWholeBars(recordLength, fpb));
  const master = bars * fpb;
  return {
    fpb,
    bars,
    master,
    derivedBpm: (bars * 4 * 60 * sr) / master, // master = bars*4*(60/bpm)*sr ⇒ bpm = bars*4*60*sr/master
    period: master / sr,
    beatPeriod: master / sr / (4 * bars),
  };
}

export interface CommitAnchor {
  period: number;
  gridAnchor: number; // the counted downbeat at/just before playAt — the loop grid's anchor (never a hop)
  startOffset: number; // buffer offset playback starts from (raw ≥ master: phase-correct mid-loop start)
  playWhen: number; // when the buffer actually starts (delayed to the next downbeat for the pad case)
}

/**
 * PHASE-LOCKED COMMIT anchor. Playback starts at `playAt` (the earliest gapless moment), but the grid is
 * ALWAYS anchored to the counted come-in downbeat, an integer number of loop periods back — never to the
 * commit instant. raw ≥ master (floored take / fixed-length): start now from the phase-correct buffer
 * offset. raw < master (a sub-1-bar take padded UP to the 1-bar minimum): [raw..master) is silence, so hold
 * playback to the NEXT counted downbeat and start from frame 0. No counted downbeat (0) ⇒ anchor at playAt.
 */
export function commitAnchor(
  downbeatCtx: number,
  master: number,
  sr: number,
  playAt: number,
  rawFrames: number,
): CommitAnchor {
  const period = master / sr;
  let gridAnchor = playAt;
  let startOffset = 0;
  let playWhen = playAt;
  if (downbeatCtx > 0 && period > 0) {
    const off = phaseOffset(playAt, downbeatCtx, period); // how far past the counted downbeat playAt sits
    gridAnchor = playAt - off;
    if (rawFrames >= master) startOffset = off;
    else playWhen = gridAnchor + period; // next counted downbeat (> playAt since off ∈ [0, period))
  }
  return { period, gridAnchor, startOffset, playWhen };
}

export interface CountInArm {
  beatPeriod: number; // count grid = the tempo at press
  anchor: number; // the count's beat 0 — the snappiest gapless start (now + lead)
  recordStart: number; // the come-in downbeat: the take's frame 0
  pendingFrames: number; // frames to discard from `now` before the take begins (before compensation C)
}

/** First-track count-in: anchor one bar of count at now + lead; the take arms frame-exact after it. */
export function countInArm(now: number, bpm: number, sr: number): CountInArm {
  const beatPeriod = 60 / bpm;
  const anchor = now + HEARTBEAT_INTERNAL_LATENCY;
  const recordStart = anchor + COUNT_IN_BEATS * beatPeriod;
  return { beatPeriod, anchor, recordStart, pendingFrames: Math.round((recordStart - now) * sr) };
}

export interface FreeStopPlan {
  fpb: number;
  bars: number; // whole bars the WALL CLOCK says were played (with the quarter-beat grace)
  target: number; // bars*fpb clamped to whole bars that fit the buffer — the frame count to commit
}

/**
 * Free-record stop: the intended length in BARS comes from the WALL CLOCK (time since the counted come-in
 * downbeat), NOT from the drained frame count — the capture pipeline lags real time by the compensation C
 * plus the drain cadence, so a stop pressed right ON the target downbeat still shows the drained frames a
 * hair short, and a drained-frames floor would cut the take a whole bar short. A quarter-beat GRACE forgives
 * a slightly anticipated press. The capture window decides when enough audio has arrived to commit.
 */
export function planFreeStop(
  elapsedSec: number,
  bpm: number,
  sr: number,
  recordLength: number,
): FreeStopPlan {
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const grace = barSec / 16; // a quarter of a beat
  const bars = barSec > 0 ? Math.floor((elapsedSec + grace) / barSec) : 0;
  const target = clampBars(bars, maxWholeBars(recordLength, fpb)) * fpb;
  return { fpb, bars, target };
}

export interface LaterStopPlan {
  fpb: number;
  bars: number;
  target: number;
}

/**
 * Later-take stop: choose the completed whole bars at the press, with the same quarter-beat grace as a
 * free first take. `captureStartFrame` includes compensation C, so subtract C before measuring musical
 * elapsed time. The result is always one through `masterBars`; a first-bar press therefore records on to
 * that bar line, while a press after a completed bar may put the deadline in the past.
 */
export function planLaterStop(
  pressTimeSec: number,
  captureStartFrame: number,
  compensationFrames: number,
  bpm: number,
  sr: number,
  masterBars: number,
): LaterStopPlan {
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const grace = barSec / 16;
  const musicalStartSec = (captureStartFrame - compensationFrames) / sr;
  const elapsedSec = pressTimeSec - musicalStartSec;
  const elapsedBars = barSec > 0 ? Math.floor((elapsedSec + grace) / barSec) : 0;
  const bars = clampBars(elapsedBars, masterBars);
  return { fpb, bars, target: bars * fpb };
}

/** Repeat the chosen later-take window across the master region, cutting the final copy at the edge. */
export function tileTake(buf: Float32Array, takeFrames: number, master: number): void {
  if (takeFrames <= 0 || takeFrames >= master || master > buf.length) return;
  for (let i = takeFrames; i < master; i++) buf[i] = buf[i % takeFrames];
}

/**
 * Commit a later take: floor the captured window to whole bars (at least one, at most the master), blank
 * whatever a window cut short of its first bar line left unwritten, and tile the result across the master.
 */
export function commitLaterTake(buf: Float32Array, rawFrames: number, fpb: number, master: number): void {
  const takeBars = clampBars(Math.floor(rawFrames / fpb), maxWholeBars(master, fpb));
  if (rawFrames < fpb) buf.fill(0, Math.max(0, rawFrames), fpb); // never tile stale frames
  tileTake(buf, takeBars * fpb, master);
}

export type RetakeStop = 'finish-pass' | 'keep-last' | 'stop-now';

/**
 * The stop gesture on a rolling RETAKE. A press within `planFreeStop`'s quarter-beat grace BEFORE the
 * pass ends means "this pass": let it run to its end and commit it. Otherwise the last complete pass is
 * kept (the pass in flight is dropped); with no complete pass yet the press is an ordinary stop.
 */
export function planRetakeStop(
  pressFrame: number,
  passEndFrame: number,
  fpb: number,
  hasKeptPass: boolean,
): RetakeStop {
  if (passEndFrame - pressFrame <= fpb / 16) return 'finish-pass';
  return hasKeptPass ? 'keep-last' : 'stop-now';
}

/** Absolute time of the next master-loop boundary at/after `now` (or `now` itself with no master). */
export function nextBoundaryTime(masterStartTime: number, period: number, now: number): number {
  if (period <= 0) return now;
  const n = Math.ceil((now - masterStartTime) / period);
  return masterStartTime + n * period;
}

/** Frames from `now` to the next boundary — the later-track arm count (before compensation C). */
export function framesToBoundary(masterStartTime: number, period: number, now: number, sr: number): number {
  return Math.round((nextBoundaryTime(masterStartTime, period, now) - now) * sr);
}

export interface ArmSplit {
  offset: number; // -1: the whole batch is pre-roll, discard it; else the index where the take begins
  pending: number; // frames still to discard after this batch
}

/**
 * The frame-exact arm split shared by the count-in and the later-track boundary arm: a batch of `count`
 * captured frames against `pending` frames still to discard. Either the whole batch is pre-roll, or it
 * straddles the target and the take begins at `offset` within it.
 */
export function armSplit(pending: number, count: number): ArmSplit {
  if (count <= pending) return { offset: -1, pending: pending - count };
  return { offset: pending, pending: 0 };
}

/** Split a timestamped capture batch at an absolute render-frame deadline. */
export function armSplitAt(startFrame: number, firstFrame: number, count: number): ArmSplit {
  return armSplit(Math.max(0, startFrame - firstFrame), count);
}

/** Map a wet sample's absolute render frame back to its compensated position on the master grid. */
export function compensatedLoopFrame(frame: number, compensation: number, gridFrame: number, master: number): number {
  return ((frame - compensation - gridFrame) % master + master) % master;
}
