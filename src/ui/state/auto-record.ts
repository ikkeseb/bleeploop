/**
 * OWNS: the AUTO REC sensitivity scale the UI shows — its range, its default and the level threshold a
 * sensitivity maps to (the command-bar meter's mark). The engine detects the onset (lf-engine
 * `autorec.rs`).
 */

export const AUTO_RECORD_DEFAULT_SENSITIVITY = 50;
export const AUTO_RECORD_MIN_SENSITIVITY = 1;
export const AUTO_RECORD_MAX_SENSITIVITY = 100;

const LEAST_SENSITIVE_DBFS = -12;
const MOST_SENSITIVE_DBFS = -60;

/** Map 1..100 onto -12..-60 dBFS. Higher sensitivity means a lower linear RMS threshold. */
export function autoRecordThreshold(sensitivity: number): number {
  const finite = Number.isFinite(sensitivity) ? sensitivity : AUTO_RECORD_DEFAULT_SENSITIVITY;
  const clamped = Math.max(AUTO_RECORD_MIN_SENSITIVITY, Math.min(AUTO_RECORD_MAX_SENSITIVITY, finite));
  const fraction = (clamped - AUTO_RECORD_MIN_SENSITIVITY) /
    (AUTO_RECORD_MAX_SENSITIVITY - AUTO_RECORD_MIN_SENSITIVITY);
  const dbfs = LEAST_SENSITIVE_DBFS + (MOST_SENSITIVE_DBFS - LEAST_SENSITIVE_DBFS) * fraction;
  return 10 ** (dbfs / 20);
}
