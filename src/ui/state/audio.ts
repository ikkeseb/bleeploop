import { engineClock, engineLooper, engineMaster, engineSampleRate, engineSession } from './engine-store';

/**
 * OWNS: the names the UI reads the audio side by: `looper`, `clock`, `master` and `sampleRate` are the
 * engine store's (`engine-store.ts`), and `session` is what export, import and recovery read and write.
 */

export type { PeakView, TrackState } from './looper-types';
export { PEAK_FRAMES } from './looper-types';

export const looper = engineLooper;
export const clock = engineClock;
export const master = engineMaster;
export const session = engineSession;

/** The audio clock's rate: the engine's device. */
export function sampleRate(): number {
  return engineSampleRate();
}
