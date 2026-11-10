/**
 * OWNS: the tuner's pitch arithmetic, pure: a frequency to its note, octave and cents against A4 = 440 Hz
 * in twelve-tone equal temperament, the in-tune state's hysteresis and the cents as the tuner prints
 * them. No DOM, timer or Solid import: `verify/guards/tuner-pitch.mjs` imports this file in plain Node,
 * and the tuner's draw loop (`tuner-loop.ts`) calls it for each reading.
 */

/** The reference pitch, fixed in v1. */
export const A4_HZ = 440;
const A4_MIDI = 69;

const NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'] as const;
export type NoteName = (typeof NAMES)[number];

/** A pitch as the tuner names it. */
export interface Note {
  /** The 12-TET name, a sharp spelled `#` (C, C#, D, D#, E, F, F#, G, G#, A, A#, B). */
  name: NoteName;
  /** The name carries a sharp. */
  sharp: boolean;
  /** Scientific octave: A4 = 440 Hz, middle C = C4 = MIDI 60, the low E string E2. */
  octave: number;
  /** How far the pitch sits from the note, -50 up to under +50 cents (a thousandth of a cent's
   * resolution): below 0 is flat, above 0 sharp. */
  cents: number;
  /** The note's MIDI number. */
  midi: number;
}

/** A note's spelling from its MIDI number. */
export function spell(midi: number): { name: NoteName; sharp: boolean; octave: number } {
  const name = NAMES[((midi % 12) + 12) % 12];
  return { name, sharp: name.length === 2, octave: Math.floor(midi / 12) - 1 };
}

/** A thousandth: what the arithmetic rounds a cent to, so the last bit of a float never decides a note. */
const round3 = (x: number): number => Math.round(x * 1000) / 1000;

/**
 * The note nearest `hz` and how far it sits from it, or null for a frequency that is not a positive,
 * finite number (no pitch). A pitch exactly halfway between two notes names the upper one, at -50 cents:
 * the cents from A4 are rounded to a thousandth first, so a boundary falls the same way every time.
 */
export function noteOf(hz: number): Note | null {
  if (!Number.isFinite(hz) || hz <= 0) return null;
  const fromA4 = round3(1200 * Math.log2(hz / A4_HZ));
  const steps = Math.round(fromA4 / 100);
  const midi = A4_MIDI + steps;
  return { ...spell(midi), cents: round3(fromA4 - steps * 100), midi };
}

/** In tune from this close to the note (cents, either side) ... */
export const IN_TUNE_ENTER = 2.5;
/** ... and still in tune until a reading passes this far: a reading wavering at the edge never flickers. */
export const IN_TUNE_LEAVE = 3.5;

/** The in-tune state after a reading `cents` off its note, from the state before it (`was`). No pitch
 * (null, or not a number) is never in tune. */
export function inTuneStep(was: boolean, cents: number | null): boolean {
  if (cents === null || !Number.isFinite(cents)) return false;
  return Math.abs(cents) <= (was ? IN_TUNE_LEAVE : IN_TUNE_ENTER);
}

/** The cents as the tuner prints them: whole, signed with a true minus or a plus, `0` unsigned. */
export function centsText(cents: number): string {
  const c = Math.round(cents);
  return c > 0 ? `+${c}` : c < 0 ? `−${-c}` : '0';
}
