// src/session/archive-limits.ts
// OWNS: the largest session archive an import takes (`docs/unit-contract.md` §8: a full format v3 archive)
// and the most entries it may hold. PURE, explicit .ts imports: `verify/guards/import.mjs` imports it under
// Node and round-trips an archive of exactly that size.
import { MAX_LOOP_SECONDS, TRACK_COUNT } from '../ui/state/looper-types.ts';

const ZIP_OVERHEAD_BYTES = 1 << 20;
/** The plugin slots, each a chain of units (the engine's `SLOT_COUNT`). */
const SLOT_COUNT = 2;
/** The units one slot's chain seats at most, each of which may carry a tone: lf-engine's `MAX_UNITS`
 * (`src-tauri/crates/lf-engine/src/api.rs`). Change them together (`verify/guards/import.mjs` holds them
 * equal). */
export const MAX_UNITS = 8;
/** The largest tone file: `MAX_STATE_BYTES` in `src-tauri/src/host/tone.rs` plus its small header.
 * Change them together. */
export const MAX_TONE_BYTES = (16 << 20) + (1 << 16);
/** The most entries an archive holds: five stems, the master, session.json and a tone per unit. */
export const MAX_ARCHIVE_ENTRIES = TRACK_COUNT + 2 + SLOT_COUNT * MAX_UNITS;

/** Five stereo Float32 stems at the longest loop, a PCM16 stereo master, a tone of the largest size per
 * unit, and the zip's own bytes. */
export function maxImportArchiveBytes(sampleRate: number): number {
  return (
    (TRACK_COUNT * 2 * Float32Array.BYTES_PER_ELEMENT + 2 * Int16Array.BYTES_PER_ELEMENT) * MAX_LOOP_SECONDS * sampleRate +
    SLOT_COUNT * MAX_UNITS * MAX_TONE_BYTES +
    ZIP_OVERHEAD_BYTES
  );
}
