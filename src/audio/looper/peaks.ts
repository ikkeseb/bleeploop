/**
 * OWNS: waveform peak pre-computation — the per-track min/max bins (incremental during capture, full on
 * commit) that the 60 fps renderer down-samples instead of scanning raw PCM.
 */
import { PEAK_FRAMES, engineState, type Track } from './state';

// ── Waveform peaks (for the 60fps renderer) ──────────────────────────────────────────────
/** Compute the min/max of `record[from..to)` into peak bin `binIdx`. Assumes to > from. */
function computeBin(t: Track, binIdx: number, from: number, to: number): void {
  const buf = t.record;
  let mn = buf[from];
  let mx = mn;
  for (let k = from + 1; k < to; k++) {
    const v = buf[k];
    if (v < mn) mn = v;
    else if (v > mx) mx = v;
  }
  t.peakMin[binIdx] = mn;
  t.peakMax[binIdx] = mx;
}

/**
 * Incrementally fold freshly-captured frames into the peak array during RECORDING. Only the
 * newly-completed full bins are computed once; the trailing partial bin (the live growing edge)
 * is recomputed each call (≤ PEAK_FRAMES samples — cheap). O(new frames) total.
 */
export function updateLivePeaks(t: Track): void {
  const wh = t.writeHead;
  let c = t.peakComplete;
  while (c < engineState.maxPeaks && (c + 1) * PEAK_FRAMES <= wh) {
    computeBin(t, c, c * PEAK_FRAMES, (c + 1) * PEAK_FRAMES);
    c++;
  }
  t.peakComplete = c;
  if (c < engineState.maxPeaks && c * PEAK_FRAMES < wh) {
    computeBin(t, c, c * PEAK_FRAMES, wh);
    t.peakCount = c + 1;
  } else {
    t.peakCount = c;
  }
  t.peakVersion++;
}

/** Recompute all peaks from `record[0..frames)` (called once on each commit/swap). */
export function recomputePeaks(t: Track, frames: number): void {
  const bins = Math.min(engineState.maxPeaks, Math.ceil(frames / PEAK_FRAMES));
  for (let b = 0; b < bins; b++) {
    const from = b * PEAK_FRAMES;
    const to = Math.min(frames, (b + 1) * PEAK_FRAMES);
    if (to > from) computeBin(t, b, from, to);
  }
  t.peakCount = bins;
  t.peakComplete = bins;
  t.peakVersion++;
}

/** Reset a track's peaks to empty (on (re)record arm / clear). Bumps version so the canvas clears. */
export function resetPeaks(t: Track): void {
  t.peakCount = 0;
  t.peakComplete = 0;
  t.peakVersion++;
}
