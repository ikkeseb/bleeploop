import { clock } from '../clock';
import { engine } from '../engine';
import type { FxState } from '../fx/metadata';
import { looper, type PeakView, type TrackState } from '../looper/looper';
import type { LoadSessionPayload } from '../looper/session';
import { master } from '../master';
import type { StemSnapshot } from './stem-archive';

/**
 * OWNS: what export, recovery and import read and write — the committed loops, their mix, the tempo, the
 * rate and the master level — as one interface, so the same coordinators (`export.ts`, `import.ts`,
 * `../autosave.ts`) serve the web looper (`webSession`, here) and engine mode's store
 * (`src/ui/state/engine-store.ts`). The UI hands in the one for its mode (`src/ui/state/audio.ts`).
 */
export interface SessionSource {
  readonly trackCount: number;
  stateOf(i: number): TrackState;
  trackInfo(i: number): { readonly lengthFrames: number };
  peaksInto(i: number, out: PeakView): PeakView;
  trackVolume(i: number): number;
  trackMuted(i: number): boolean;
  fxState(i: number): FxState[];
  masterFramesValue(): number;
  /** Copies of the committed lanes' PCM with their mix, each with the state it had as it was read. */
  exportSnapshot(): Promise<StemSnapshot>;
  /** Load a session into an all-empty looper; throws, changing nothing, otherwise. */
  loadSession(payload: LoadSessionPayload): Promise<void>;
  bpm(): number;
  sampleRate(): number;
  /** The master gain as heard (0 while muted). */
  masterLevel(): number;
}

/** The web looper as a session source, looked up at each call (as the coordinators did, so a DEV probe
 * that wraps a `looper` member still sees every call). Its snapshot and the lane states are read in one
 * tick. */
export const webSession: SessionSource = {
  trackCount: looper.trackCount,
  stateOf: (i) => looper.stateOf(i),
  trackInfo: (i) => looper.trackInfo(i),
  peaksInto: (i, out) => looper.peaksInto(i, out),
  trackVolume: (i) => looper.trackVolume(i),
  trackMuted: (i) => looper.trackMuted(i),
  fxState: (i) => looper.fxState(i),
  masterFramesValue: () => looper.masterFramesValue(),
  exportSnapshot: async () => {
    const snap = looper.exportSnapshot();
    return { ...snap, tracks: snap.tracks.map((t) => ({ ...t, state: looper.stateOf(t.index) })) };
  },
  loadSession: (payload) => looper.loadSession(payload),
  bpm: () => clock.bpm(),
  sampleRate: () => engine.ctx.sampleRate,
  masterLevel: () => (master.muted() ? 0 : master.volume()),
};
