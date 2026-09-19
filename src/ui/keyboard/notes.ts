const NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];

/** MIDI note -> name with octave (MIDI 60 = C4). */
export function noteName(midi: number): string {
  return NAMES[((midi % 12) + 12) % 12] + (Math.floor(midi / 12) - 1);
}

export function isBlackKey(midi: number): boolean {
  const pc = ((midi % 12) + 12) % 12;
  return pc === 1 || pc === 3 || pc === 6 || pc === 8 || pc === 10;
}

/** MIDI note for C of the given octave (octave 4 -> 60). */
export function octaveBase(octave: number): number {
  return (octave + 1) * 12;
}
