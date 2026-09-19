/**
 * Best-effort localStorage number persistence — ONE home for the read-validate-default /
 * write-and-swallow pattern master.ts and clock.ts each hand-rolled. localStorage can throw
 * (private mode / disabled storage), so both directions swallow and fall back; a stored value
 * outside [min, max] (or non-numeric garbage) falls back to the default rather than poisoning
 * the audio path with an out-of-range gain.
 */

export function readStoredNumber(key: string, def: number, min: number, max: number): number {
  try {
    const raw = localStorage.getItem(key);
    if (raw === null) return def;
    const v = Number(raw);
    return Number.isFinite(v) && v >= min && v <= max ? v : def;
  } catch {
    return def;
  }
}

export function writeStoredNumber(key: string, v: number): void {
  try {
    localStorage.setItem(key, String(v));
  } catch {
    /* persistence is best-effort */
  }
}
