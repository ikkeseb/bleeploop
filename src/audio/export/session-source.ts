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
  /** Changes whenever lane `i`'s committed loop may have changed: recovery's dirty check, which waits
   * for it to hold still. While the lane overdubs it moves only when a layer commits (the snapshot holds
   * the loop without the layer in flight), so saves land during a dub. The engine's holds still while a
   * take records too; the web looper's moves with the take's waveform, so no save lands during one. */
  revision(i: number): number;
  /**
   * The player's clear that emptied the looper (CLEAR ALL, or the CLEAR of the last loop), while it still
   * stands: only it lets an empty looper delete the recovery (`../autosave.ts`), once (`spendClear`).
   * Null after a partial clear, after any commit since, and once the looper was replaced (engine mode:
   * another sample rate, a fault, a WebView reload), whose empty lanes keep the jam.
   */
  clearToken(): ClearToken | null;
  /** The recovery deleted the jam `token` let it delete. */
  spendClear(token: ClearToken): void;
  trackVolume(i: number): number;
  trackMuted(i: number): boolean;
  /** DUB FEEDBACK (0..1): engine mode's lane setting; the web looper only sums (1). */
  trackDubFeedback(i: number): number;
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

/** A player's clear that emptied the looper, as `SessionSource.clearToken` hands it out: compared by
 * identity. */
export type ClearToken = object;

const peakView: PeakView = { min: null, max: null, count: 0, version: -1 };
/** The web looper's one token: it has no replacement path, so every clear that empties it is the player's. */
const webClear: ClearToken = {};

/** The web looper as a session source, looked up at each call (as the coordinators did, so a DEV probe
 * that wraps a `looper` member still sees every call). Its snapshot and the lane states are read in one
 * tick. Its revision is the waveform's: an overdub commits its layer at each loop boundary (`record`
 * holds the last one), so its waveform moves only there. It has no replacement path: it empties only
 * by a player's clear. */
export const webSession: SessionSource = {
  trackCount: looper.trackCount,
  stateOf: (i) => looper.stateOf(i),
  trackInfo: (i) => looper.trackInfo(i),
  revision: (i) => looper.peaksInto(i, peakView).version,
  clearToken: () => webClear,
  spendClear: () => {},
  trackVolume: (i) => looper.trackVolume(i),
  trackMuted: (i) => looper.trackMuted(i),
  trackDubFeedback: () => 1,
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
