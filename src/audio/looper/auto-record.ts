/**
 * OWNS: the AUTO REC onset detector — when the first take starts on a level trigger instead of a count-in.
 * Pure main-thread: the AudioWorklet stays a zero-allocation producer; this class
 * inspects the PCM that capture.ts already drains from its SAB ring. Every buffer is allocated once in
 * the constructor, and scan() allocates nothing while the lane is listening.
 */

export const AUTO_RECORD_DEFAULT_SENSITIVITY = 50;
export const AUTO_RECORD_MIN_SENSITIVITY = 1;
export const AUTO_RECORD_MAX_SENSITIVITY = 100;
export const AUTO_RECORD_LOOKBACK_MS = 40;
export const AUTO_RECORD_ANALYSIS_MS = 4;

const LEAST_SENSITIVE_DBFS = -12;
const MOST_SENSITIVE_DBFS = -60;
const ONSET_THRESHOLD_RATIO = 0.2;
const ONSET_FLOOR = 10 ** (-72 / 20);

/** Map 1..100 onto -12..-60 dBFS. Higher sensitivity means a lower linear RMS threshold. */
export function autoRecordThreshold(sensitivity: number): number {
  const finite = Number.isFinite(sensitivity) ? sensitivity : AUTO_RECORD_DEFAULT_SENSITIVITY;
  const clamped = Math.max(AUTO_RECORD_MIN_SENSITIVITY, Math.min(AUTO_RECORD_MAX_SENSITIVITY, finite));
  const fraction = (clamped - AUTO_RECORD_MIN_SENSITIVITY) /
    (AUTO_RECORD_MAX_SENSITIVITY - AUTO_RECORD_MIN_SENSITIVITY);
  const dbfs = LEAST_SENSITIVE_DBFS + (MOST_SENSITIVE_DBFS - LEAST_SENSITIVE_DBFS) * fraction;
  return 10 ** (dbfs / 20);
}
export class AutoRecordDetector {
  private readonly history: Float32Array;
  private readonly energyWindow: Float32Array;
  private readonly analysisFrames: number;
  private historyWrite = 0;
  private historyFill = 0;
  private energyWrite = 0;
  private energyFill = 0;
  private energySum = 0;
  private copied = 0;

  constructor(sampleRate: number) {
    const rate = Number.isFinite(sampleRate) && sampleRate > 0 ? sampleRate : 48_000;
    this.analysisFrames = Math.max(1, Math.round(rate * AUTO_RECORD_ANALYSIS_MS / 1000));
    const lookbackFrames = Math.max(this.analysisFrames, Math.round(rate * AUTO_RECORD_LOOKBACK_MS / 1000));
    this.history = new Float32Array(lookbackFrames);
    this.energyWindow = new Float32Array(this.analysisFrames);
  }

  /** Start a fresh listening arm. Older audio and partial RMS state cannot cross this boundary. */
  reset(): void {
    this.historyWrite = 0;
    this.historyFill = 0;
    this.energyWrite = 0;
    this.energyFill = 0;
    this.energySum = 0;
    this.copied = 0;
  }

  /** Number of look-back frames copied by the last successful scan(). */
  copiedFrames(): number {
    return this.copied;
  }

  /**
   * Feed one drain batch. Returns -1 while listening. On trigger it copies the retained onset straight
   * into `output` and returns the first input index not already copied, so capture.ts can append the rest
   * of this same batch once.
   */
  scan(data: Float32Array, count: number, threshold: number, output: Float32Array): number {
    const n = Math.max(0, Math.min(data.length, Math.trunc(count)));
    const useThreshold = Number.isFinite(threshold) && threshold > 0 ? threshold : autoRecordThreshold(AUTO_RECORD_DEFAULT_SENSITIVITY);
    const requiredEnergy = useThreshold * useThreshold * this.analysisFrames;
    this.copied = 0;

    for (let i = 0; i < n; i++) {
      const sample = data[i];
      this.pushHistory(sample);
      this.pushEnergy(sample * sample);
      if (this.energyFill === this.analysisFrames && this.energySum >= requiredEnergy) {
        const start = this.findOnsetStart(useThreshold);
        const frames = this.historyFill - start;
        if (frames > output.length) throw new RangeError('AUTO REC look-back exceeds the record buffer');
        for (let k = 0; k < frames; k++) output[k] = this.historyAt(start + k);
        this.copied = frames;
        return i + 1;
      }
    }
    return -1;
  }

  /**
   * True only when no retained block could become part of a future onset. capture.ts uses this to move
   * its loss baselines forward during a long silent wait without forgiving damage in audio it may keep.
   */
  historyIsQuiet(threshold: number): boolean {
    const useThreshold = Number.isFinite(threshold) && threshold > 0 ? threshold : autoRecordThreshold(AUTO_RECORD_DEFAULT_SENSITIVITY);
    return this.findFirstActiveBlock(useThreshold) < 0;
  }

  private pushHistory(sample: number): void {
    this.history[this.historyWrite] = sample;
    this.historyWrite++;
    if (this.historyWrite === this.history.length) this.historyWrite = 0;
    if (this.historyFill < this.history.length) this.historyFill++;
  }

  private pushEnergy(square: number): void {
    if (this.energyFill === this.analysisFrames) {
      this.energySum -= this.energyWindow[this.energyWrite];
    } else {
      this.energyFill++;
    }
    this.energyWindow[this.energyWrite] = square;
    this.energySum += square;
    this.energyWrite++;
    if (this.energyWrite === this.analysisFrames) this.energyWrite = 0;
  }

  /** Read an index relative to the oldest retained sample. */
  private historyAt(index: number): number {
    const oldest = this.historyWrite - this.historyFill;
    const physical = (oldest + index + this.history.length) % this.history.length;
    return this.history[physical];
  }

  /** Start one analysis block before the first soft activity, never at the full 40 ms history blindly. */
  private findOnsetStart(threshold: number): number {
    const activeBlock = this.findFirstActiveBlock(threshold);
    if (activeBlock < 0) return Math.max(0, this.historyFill - this.analysisFrames);
    return Math.max(0, activeBlock - this.analysisFrames);
  }

  private findFirstActiveBlock(threshold: number): number {
    const onsetThreshold = Math.max(ONSET_FLOOR, threshold * ONSET_THRESHOLD_RATIO);
    const onsetEnergy = onsetThreshold * onsetThreshold;
    for (let start = 0; start < this.historyFill; start += this.analysisFrames) {
      const end = Math.min(this.historyFill, start + this.analysisFrames);
      let sum = 0;
      for (let i = start; i < end; i++) {
        const sample = this.historyAt(i);
        sum += sample * sample;
      }
      if (sum / (end - start) >= onsetEnergy) return start;
    }
    return -1;
  }
}
