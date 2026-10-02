import type { FxState } from '../ui/state/fx-metadata';
import type { LoadSessionPayload, TrackState } from '../ui/state/looper-types';
import type { StemSnapshot } from './stem-archive';

/**
 * OWNS: what export, recovery and import read and write — the committed loops, their mix, the tempo, the
 * rate and the master level — as one interface the coordinators (`export.ts`, `import.ts`,
 * `autosave.ts`) take. The engine store implements it (`engineSession`, `src/ui/state/engine-store.ts`);
 * the UI hands it in as `session` (`src/ui/state/audio.ts`).
 */
export interface SessionSource {
  readonly trackCount: number;
  stateOf(i: number): TrackState;
  trackInfo(i: number): { readonly lengthFrames: number };
  /** Changes whenever lane `i`'s committed loop may have changed: recovery's dirty check, which waits
   * for it to hold still. While the lane overdubs it moves only when a layer commits (the snapshot holds
   * the loop without the layer in flight), so saves land during a dub; it holds still while a take
   * records too. */
  revision(i: number): number;
  /**
   * The player's clear that emptied the looper (CLEAR ALL, or the CLEAR of the last loop), while it still
   * stands: only it lets an empty looper delete the recovery (`autosave.ts`), once (`spendClear`).
   * Null after a partial clear, after any commit since, and once the looper was replaced (another sample
   * rate, a fault, a WebView reload), whose empty lanes keep the jam.
   */
  clearToken(): ClearToken | null;
  /** The recovery deleted the jam `token` let it delete. */
  spendClear(token: ClearToken): void;
  trackVolume(i: number): number;
  trackMuted(i: number): boolean;
  /** DUB FEEDBACK (0..1): the lane's setting. */
  trackDubFeedback(i: number): number;
  fxState(i: number): FxState[];
  masterFramesValue(): number;
  /** Copies of the committed lanes' PCM with their mix, each with the state it had as it was read, and
   * the grid they were read on; with `master` (an export's, never a recovery's), the engine's wet master
   * of them too, or its error. */
  exportSnapshot(options?: { master?: boolean }): Promise<StemSnapshot>;
  /** Load a session into an all-empty looper; throws, changing nothing, otherwise. */
  loadSession(payload: LoadSessionPayload): Promise<void>;
  sampleRate(): number;
  /** The master gain as heard (0 while muted). */
  masterLevel(): number;
}

/** A player's clear that emptied the looper, as `SessionSource.clearToken` hands it out: compared by
 * identity. `rate`: the rate of the engine it emptied, whose jam it deletes. */
export interface ClearToken {
  readonly rate: number;
}
