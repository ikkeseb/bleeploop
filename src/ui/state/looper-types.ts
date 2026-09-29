import type { FxState } from './fx-metadata';
import { framesPerBar } from './quantize';

/**
 * OWNS: the looper's shapes the engine store, the UI and the session code share — the lane count and
 * the record cap, a lane's state, the waveform peak view and its bin size, the export snapshot and the
 * session-load payload — plus the first take's drawn span. The engine owns the looper itself; these are
 * what the feed and a session are read into.
 */

export const TRACK_COUNT = 5;
export const MAX_LOOP_SECONDS = 60;
/**
 * Waveform peak resolution: one min/max pair per this many frames (~21ms @48kHz). The engine feed's
 * bins are these 1024 frames (`PeakUpdate` in `src/platform/engine-wire.ts`) but carry no size, so this
 * one constant sizes them; the 60fps draw loop only down-samples this coarse array to the canvas width.
 */
export const PEAK_FRAMES = 1024;

export type TrackState = 'EMPTY' | 'RECORDING' | 'OVERDUBBING' | 'PLAYING' | 'STOPPED';

/**
 * A non-reactive view of a track's waveform peaks, filled into a caller-owned object by
 * `peaksInto()` so the rAF draw loop can read peaks with zero allocation and no Solid
 * subscription. `min`/`max` are refs into the store's pre-allocated arrays; valid indices are
 * `[0, count)`. `version` bumps whenever the peaks change (the renderer's dirty flag).
 */
export interface PeakView {
  min: Float32Array | null;
  max: Float32Array | null;
  count: number;
  version: number;
}

/** One committed lane as export and recovery read it (`SessionSource.exportSnapshot`). */
export interface ExportTrack {
  /** 0-based engine track index. */
  index: number;
  /** Copy of the committed loop region [0, lengthFrames), MONO. */
  pcm: Float32Array;
  volume: number;
  muted: boolean;
  reversed: boolean;
  /** Deep copy of the five per-track FX states (chain order) — drives the v1 offline master render
   *  and lands in session.json. Plain JSON-serializable data. */
  fx: FxState[];
}
export interface ExportSnapshot {
  sampleRate: number;
  masterLengthFrames: number;
  tracks: ExportTrack[];
}

/**
 * The span a first take (no loop yet) is drawn across: 4 bars at the tempo when it starts, or 8 s with
 * no tempo. Taken once per take, never per drawn frame (invariant 6).
 */
export function openingSpan(bpm: number, sampleRate: number): number {
  const frames = bpm > 0 && Number.isFinite(bpm) ? 4 * framesPerBar(bpm, sampleRate) : 8 * sampleRate;
  return Math.max(1, frames);
}

/**
 * A first take's span `elapsed` frames in: the opening span, doubled each time the take reaches it. Its
 * peaks are placed by frame over this span, so they move only when it doubles, never as bins arrive.
 */
export function firstTakeSpan(opening: number, elapsed: number): number {
  let span = opening;
  for (let k = 0; k < 32 && elapsed >= span; k++) span *= 2;
  return span;
}

/** One imported track (`SessionSource.loadSession`): 0-based engine index + EXACTLY masterLengthFrames
 * of mono PCM. */
export interface LoadSessionTrack {
  index: number;
  pcm: Float32Array;
  volume: number;
  muted: boolean;
  reversed: boolean;
  /** Missing stays compatible with older direct fixtures and normalizes to PLAYING. */
  state?: 'PLAYING' | 'STOPPED';
  fx: FxState[];
  /** DUB FEEDBACK (0..1). Missing reads as 1. */
  dubFeedback?: number;
}
export interface LoadSessionPayload {
  bpm: number;
  bars: number;
  masterLengthFrames: number;
  tracks: LoadSessionTrack[];
}
