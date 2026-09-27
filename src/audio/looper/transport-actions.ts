import { createSignal } from 'solid-js';
import { TRACK_COUNT } from './state';

/**
 * OWNS: track selection, exposed through the looper facade (`looper.ts`): the track the transport keys
 * and MIDI learn act on when a press names none. The named-action table (`src/app/actions.ts`) reads it
 * and runs the lane's own path through the facade (`recDub`/`playStop` in machine.ts), so a key, a
 * footswitch keystroke and a learned MIDI message drive the identical paths the on-screen Looper buttons
 * use. UI-navigation state, deliberately kept apart from the engine-state module.
 */

// Defaults to track 1 (index 0).
const [selectedTrack, setSelectedTrack] = createSignal(0);
export { selectedTrack };

/** Select track `i` (0-based, clamped to a valid track). */
export function selectTrack(i: number): void {
  setSelectedTrack(Math.max(0, Math.min(TRACK_COUNT - 1, Math.trunc(i))));
}
