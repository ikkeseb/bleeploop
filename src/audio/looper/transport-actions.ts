import { createSignal } from 'solid-js';
import { TRACK_COUNT } from './state';
import { playStop, recDub } from './machine';

/**
 * OWNS: track selection + the selected-track REC/DUB and PLAY/STOP adapters. The single action-routing
 * layer that BOTH the
 * keyboard shortcut handler (step 1) and the future MIDI control-map (step 2) dispatch into — so a
 * key, a footswitch keystroke, and a MIDI CC all drive the identical engine paths the on-screen
 * Looper buttons use (`recDub`/`playStop` in machine.ts). No parallel state machine lives here: these
 * are thin, selected-track-aware adapters. The engine self-protects the edge cases (single-recorder
 * lockout, STOPPED no-op, reverse-blocks-overdub) inside startRecording/startOverdub, so these do not
 * re-implement the UI's `disabled` predicates.
 */

// Which track keyboard/MIDI transport acts on. Defaults to track 1 (index 0). UI-navigation state,
// deliberately kept beside its actions rather than in the engine-state module.
const [selectedTrack, setSelectedTrack] = createSignal(0);
export { selectedTrack };

/** Select track `i` (0-based, clamped to a valid track). */
export function selectTrack(i: number): void {
  setSelectedTrack(Math.max(0, Math.min(TRACK_COUNT - 1, Math.trunc(i))));
}

/** REC/DUB toggle on the selected track — same path as the lane's core button. */
export function recDubSelected(): void {
  void recDub(selectedTrack());
}

/** PLAY/STOP on the selected track — same path as the lane's PLAY/STOP cap. */
export function playStopSelected(): void {
  playStop(selectedTrack());
}
