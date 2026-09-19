import { MonoSynth } from 'tone';
import { midiToFreq } from '../types';
import { createModulation } from './modulation';
import type { SynthEngine } from './synth';

/**
 * Bass — punchy low monosynth.
 * MonoSynth with a square oscillator, low-pass filter, and a sub-y punch envelope.
 * Capped at 1 voice (MonoSynth is inherently monophonic).
 */
export function createBass(): SynthEngine {
  const mono = new MonoSynth({
    volume: -7, // square+lowpass runs hot (single note peaked ~0.7); trim for headroom under the master sum
    oscillator: { type: 'square' },
    envelope: { attack: 0.006, decay: 0.22, sustain: 0.65, release: 0.4 },
    filter: { frequency: 800, type: 'lowpass', rolloff: -24 },
    filterEnvelope: {
      attack: 0.01,
      decay: 0.18,
      sustain: 0.3,
      release: 0.4,
      baseFrequency: 120,
      octaves: 3.5,
    },
  });
  // Splice the shared vibrato into the chain; pitch bend re-tunes the mono voice via its detune signal.
  const mod = createModulation((cents) => {
    mono.detune.value = cents;
  });
  mono.connect(mod.input);

  const held = new Map<number, number>();
  let sounding: number | null = null;
  let attackTime = -Infinity;
  const attack = (note: number, velocity: number, time?: number) => {
    attackTime = Math.max(time ?? mono.context.immediate() + 0.005, attackTime + 1 / mono.context.sampleRate);
    mono.triggerAttack(midiToFreq(note), attackTime, velocity);
  };

  return {
    id: 'bass',
    name: 'Bass',
    noteOn(note, velocity, time) {
      held.delete(note);
      held.set(note, velocity);
      sounding = note;
      attack(note, velocity, time);
    },
    noteOff(note, time) {
      if (!held.delete(note) || sounding !== note) return;
      const previous = [...held.entries()].at(-1);
      sounding = previous?.[0] ?? null;
      if (previous) attack(previous[0], previous[1], time);
      else mono.triggerRelease(Math.max(time ?? mono.context.immediate() + 0.005, attackTime));
    },
    setPitchBend(semitones) {
      mod.setPitchBend(semitones);
    },
    setModulation(depth) {
      mod.setModulation(depth);
    },
    allNotesOff() {
      held.clear();
      sounding = null;
      mono.triggerRelease(Math.max(mono.context.immediate() + 0.005, attackTime));
    },
    dispose() {
      mono.dispose();
      mod.dispose();
    },
  };
}
