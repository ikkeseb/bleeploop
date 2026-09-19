/** A built-in synth voice engine. Connects itself to engine.instrumentBus on creation. */
export interface SynthEngine {
  readonly id: string;
  readonly name: string;
  /** velocity is normalized 0..1. `time` is an optional absolute AudioContext time. */
  noteOn(note: number, velocity: number, time?: number): void;
  noteOff(note: number, time?: number): void;
  /** Pitch bend in semitones (±). Optional — voices that can't bend (e.g. drum) omit it. */
  setPitchBend?(semitones: number): void;
  /** Mod-wheel depth 0..1 → vibrato. Optional — omitted where it can't apply (drum). */
  setModulation?(depth: number): void;
  allNotesOff(): void;
  dispose(): void;
}

export interface SynthFactory {
  id: string;
  name: string;
  create: () => SynthEngine;
}
