/**
 * Pure timing helpers — no Tone or browser deps, fully unit-testable.
 * Used by the looper for master-loop frame math and quantized start/stop.
 */

/** Seconds per beat given BPM. */
export function secondsPerBeat(bpmValue: number): number {
  return 60 / bpmValue;
}

/**
 * Number of audio frames per bar.
 * @param bpmValue   Beats per minute
 * @param sampleRate Audio sample rate in Hz
 * @param beatsPerBar Number of beats per bar (default 4)
 */
export function framesPerBar(bpmValue: number, sampleRate: number, beatsPerBar = 4): number {
  return Math.round(secondsPerBeat(bpmValue) * beatsPerBar * sampleRate);
}

/**
 * Largest number of WHOLE bars that fit a record buffer (>= 1). The buffer-fit bound every
 * bar-count derivation must respect: a master length > record.length would zero-pad the first loop's
 * tail and RangeError a later track's consume() (its configured window cannot exceed `master` frames in the
 * same-sized buffer). The single source of truth for this clamp — do not hand-copy it at commit/arm
 * sites.
 */
export function maxWholeBars(bufferFrames: number, fpb: number): number {
  return Math.max(1, Math.floor(bufferFrames / fpb));
}

/** Clamp a requested/derived bar count to [1, maxBars] (whole bars that fit the buffer). */
export function clampBars(bars: number, maxBars: number): number {
  return Math.min(maxBars, Math.max(1, bars));
}

/**
 * Mean interval between consecutive timestamps (ms in, ms out). The shared core of tap tempo and
 * MIDI-clock BPM smoothing in clock.ts (both average a rolling timestamp window, then convert to
 * BPM). Requires >= 2 timestamps; callers guard.
 */
export function averageInterval(times: readonly number[]): number {
  let sum = 0;
  for (let i = 1; i < times.length; i++) sum += times[i] - times[i - 1];
  return sum / (times.length - 1);
}
