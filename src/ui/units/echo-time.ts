/**
 * ECHO's time, in one place. The engine's `echoTime` is an index into the lane delay's divisions in the
 * engine's order (`dsp::fx::DIVISIONS`: 1/4, 1/8, 1/8., 1/16; `fx-metadata.ts` `DIVISION_LABELS`); the
 * face orders them by length (1/16, 1/8, 1/8., 1/4). `ECHO_STEPS` is the face's order, each step naming the
 * engine's index it sends. Pure: the face (Solid) and its draw loop (`echo-loop.ts`, invariant 6) share it.
 */

/** One of TIME's positions: its label, its length in quarter notes, the engine's index for it. */
interface EchoStep {
  label: string;
  quarters: number;
  index: number;
}

/** TIME's positions in the face's order, by length. */
export const ECHO_STEPS: readonly EchoStep[] = [
  { label: '1/16', quarters: 0.25, index: 3 },
  { label: '1/8', quarters: 0.5, index: 1 },
  { label: '1/8.', quarters: 0.75, index: 2 },
  { label: '1/4', quarters: 1, index: 0 },
];

/** The face's position for the engine's index (rounded and clamped as the engine's `clamp_index`). */
export function stepOfIndex(index: number): number {
  const i = Math.min(3, Math.max(0, Math.round(index)));
  return ECHO_STEPS.findIndex((s) => s.index === i);
}

/** The tempo as the echo reads it: the counted beat and the bar's quarter notes (the engine's
 * `follow_tempo`: the echo's beat is the bar's quarter note, a compound meter's bar at the dotted quarter). */
export interface EchoTempo {
  bpm: number;
  /** The meter: counted beats a bar, and its numerator and denominator. */
  beats: number;
  num: number;
  den: number;
}

/** Seconds of one counted beat. */
export const beatSeconds = (t: EchoTempo): number => 60 / t.bpm;
/** Seconds of one bar. */
export const barSeconds = (t: EchoTempo): number => beatSeconds(t) * t.beats;
/** Seconds of one quarter note, the echo's beat. */
const quarterSeconds = (t: EchoTempo): number => barSeconds(t) / ((t.num * 4) / t.den);
/** Seconds between repeats at the face's position `step`. */
export const stepSeconds = (t: EchoTempo, step: number): number => quarterSeconds(t) * ECHO_STEPS[step].quarters;

/** FEEDBACK's ceiling (the engine's `MAX_FEEDBACK`): the face's knob runs 0 to 95 %. */
export const ECHO_FEEDBACK_MAX = 0.95;

/** Repeat `k`'s gain (k from 1): LEVEL, compensated for the feedback, times the feedback per pass
 * (`input_fx.rs`: the heard level is LEVEL · sqrt(1 - FB²), each pass FB). */
export const repeatGain = (level: number, feedback: number, k: number): number => level * Math.sqrt(1 - feedback * feedback) * feedback ** (k - 1);
