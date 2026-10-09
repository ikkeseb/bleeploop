import { engineClock, engineLooper, engineMaster, engineSampleRate, engineSession } from './engine-store';

/**
 * OWNS: the names the UI reads the audio side by: `looper`, `clock`, `master` and `sampleRate` are the
 * engine store's (`engine-store.ts`), and `session` is what export, import and recovery read and write.
 */

export type { PeakView, TrackState } from './looper-types';
export { PEAK_FRAMES } from './looper-types';
/** The live scope taps' view (`looper.scopeInto`) and the two non-lane sources a batch carries, for
 * the stage look that draws them (`src/ui/stage/scope.ts`); the lanes are 0..4 of the same order. */
export type { ScopeView } from './engine-store';
export { SCOPE_MASTER, SCOPE_MONITOR } from '../../platform';

export const looper = engineLooper;
export const clock = engineClock;
export const master = engineMaster;
export const session = engineSession;

/** The audio clock's rate: the engine's device. */
export function sampleRate(): number {
  return engineSampleRate();
}
