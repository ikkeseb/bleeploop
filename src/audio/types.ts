/** A normalized note event from any input source (on-screen keys, computer kbd, MIDI). */
export interface NoteEvent {
  type: 'on' | 'off';
  /** MIDI note number (0..127). */
  note: number;
  /** MIDI velocity (0..127). Ignored for 'off'. */
  velocity: number;
  source: 'pointer' | 'computer' | 'midi';
  /** Stable physical owner, such as a MIDI port/channel. Defaults to source for legacy callers. */
  owner?: string;
}

/** Equal-tempered MIDI note -> frequency in Hz (A4 = note 69 = 440 Hz). */
export function midiToFreq(note: number): number {
  return 440 * Math.pow(2, (note - 69) / 12);
}

export function clampMidi(v: number): number {
  return v < 0 ? 0 : v > 127 ? 127 : v;
}
