// src/session/import.ts
// SESSION-IMPORT coordinator: the read-back mate of export.ts. Takes the bytes of an exported
// BleepLoop .zip, reads and checks it (`archive-read.ts`: session.json, each stem WAV, the slot tones)
// and hands the assembled payload to the source's loadSession (the engine, which re-validates its own
// preconditions and establishes the master grid). The slot tones the session carries come back for the
// caller to store once the loops are in (`restoreSessionTones`, `src/ui/state/slot-tones.ts`). Like
// export.ts this coordinator THROWS and never toasts — the UI catches + notifies. Unlike export.ts, the
// one-download-per-gesture constraint does NOT apply here: import consumes bytes the UI hands in (file
// input / drag-drop), no downloads.
import { TIME_SIGNATURES } from '../platform';
import type { SlotTone } from '../ui/state/slot-tones';
import { readSessionArchive } from './archive-read.ts';
import type { SessionSource } from './session-source';

/**
 * Import a BleepLoop export archive into an all-EMPTY looper: read it (`readSessionArchive`, at the
 * source's rate) and hand the payload to looper.loadSession — which restores PLAYING tracks on one
 * shared grid anchor while preserving STOPPED tracks without starting sources. Throws a descriptive
 * Error on any problem (the UI catches + notifies; nothing is mutated unless every stem validated).
 * `source` is the looper to load into (`session-source.ts`). Resolves with the slot tones the session
 * carries (none for an older export or a recovery), checked against session.json but not yet stored
 * anywhere: the caller hands them to `restoreSessionTones`.
 */
export async function importSession(bytes: Uint8Array | ArrayBuffer, source: SessionSource): Promise<SlotTone[]> {
  const { payload, tones } = readSessionArchive(bytes, source.sampleRate(), TIME_SIGNATURES);
  await source.loadSession(payload);
  return tones;
}
