import { AMSynth, FMSynth, Synth } from 'tone';
import { midiToFreq } from '../types';
import { createBass } from './bass';
import { createDrum } from './drum';
import { createModulation } from './modulation';
import type { SynthEngine, SynthFactory } from './synth';

/**
 * A built-in polyphonic voice: the SynthEngine wrapper (note dispatch + dispose) is identical across
 * them, so only the per-voice data lives here — a new poly voice is one row. `make` builds one raw
 * voice (oscillator + envelope); `volumeDb` is the gain-staging trim (omitted = Tone's 0 dB default).
 * Bass (MonoSynth) and Drum (GM kit) have their own note semantics and stay separate factories.
 */
interface PolyVoiceSpec {
  readonly id: string;
  readonly name: string;
  readonly maxPolyphony: number;
  readonly volumeDb?: number;
  readonly make: () => Synth | FMSynth | AMSynth;
}

const POLY_VOICES: readonly PolyVoiceSpec[] = [
  {
    id: 'lead',
    name: 'Lead',
    maxPolyphony: 8,
    volumeDb: -9, // a single sawtooth note hits ~0 dBFS; a chord clipped (~1.8–2.6). Trim for headroom.
    make: () =>
      new Synth({
        oscillator: { type: 'sawtooth' },
        envelope: { attack: 0.008, decay: 0.18, sustain: 0.55, release: 0.35 },
      }),
  },
  {
    id: 'pad',
    name: 'Pad',
    maxPolyphony: 12,
    // untrimmed this FMSynth stack ran hot — a 4-note chord measured 1.15, a 6-note 1.60 at vel 127
    // (pre-limiter), the only built-in to clip a normal chord. -4 dB brings a 4-note to ~0.73 (in line
    // with lead/organ/piano) and a 6-note to ~1.0, so the master limiter nets only the rare dense chord.
    volumeDb: -4,
    make: () =>
      new FMSynth({
        oscillator: { type: 'sine' },
        envelope: { attack: 0.8, decay: 0.4, sustain: 0.9, release: 2.0 },
        modulation: { type: 'sine' },
        modulationEnvelope: { attack: 0.6, decay: 0.3, sustain: 0.7, release: 1.8 },
        modulationIndex: 2,
        harmonicity: 1.5,
      }),
  },
  {
    id: 'piano',
    name: 'Piano',
    maxPolyphony: 12,
    volumeDb: -10, // pure-synth piano: triangle osc, percussive envelope. A 4-note chord peaked ~2.15 (clip).
    make: () =>
      new Synth({
        oscillator: { type: 'triangle' },
        envelope: { attack: 0.004, decay: 0.6, sustain: 0.12, release: 0.8 },
      }),
  },
  {
    id: 'organ',
    name: 'Organ',
    maxPolyphony: 8,
    // drawbar-style additive: AMSynth harmonic stack, full sustain, no attack transient. 0 dB (untrimmed).
    make: () =>
      new AMSynth({
        oscillator: { type: 'sine' },
        envelope: { attack: 0.01, decay: 0.0, sustain: 1.0, release: 0.06 },
        modulation: { type: 'square' },
        modulationEnvelope: { attack: 0.01, decay: 0.0, sustain: 1.0, release: 0.06 },
        harmonicity: 1,
      }),
  },
];

/** Build a SynthEngine from a poly-voice spec. The bounded pool and note dispatch are shared by every poly voice. */
function makePolySynth(spec: PolyVoiceSpec): SynthEngine {
  // Reuse a bounded voice pool. Release tails occupy real voices, so a deferred-note count cannot
  // prevent Tone.PolySynth from dropping attacks. Prefer a released voice, then the oldest held voice.
  const voices = Array.from({ length: spec.maxPolyphony }, () => {
    const voice = spec.make();
    if (spec.volumeDb !== undefined) voice.volume.value = spec.volumeDb;
    return { voice, note: null as number | null, order: 0, attackTime: -Infinity };
  });
  let serial = 0;
  const mod = createModulation((cents) => {
    for (const { voice } of voices) voice.detune.value = cents;
  });
  for (const { voice } of voices) voice.connect(mod.input);
  return {
    id: spec.id,
    name: spec.name,
    noteOn(note, velocity, time) {
      const same = voices.find((entry) => entry.note === note);
      const available = voices.filter((entry) => entry.note === null);
      const entry = same ?? (available.length ? available : voices).reduce((a, b) => a.order <= b.order ? a : b);
      entry.note = note;
      entry.order = ++serial;
      // Multiple MIDI attacks can share one ctx timestamp. Tone forbids restarting a source at its
      // previous start time; advance only a reused voice by one sample, retaining near-now scheduling.
      const when = Math.max(time ?? entry.voice.context.immediate() + 0.005, entry.attackTime + 1 / entry.voice.context.sampleRate);
      entry.attackTime = when;
      entry.voice.triggerAttack(midiToFreq(note), when, velocity);
    },
    noteOff(note, time) {
      const entry = voices.find((candidate) => candidate.note === note);
      if (!entry) return; // A stale release must not stop the note that stole this voice.
      entry.note = null;
      entry.voice.triggerRelease(Math.max(time ?? entry.voice.context.immediate() + 0.005, entry.attackTime));
    },
    setPitchBend(semitones) { mod.setPitchBend(semitones); },
    setModulation(depth) { mod.setModulation(depth); },
    allNotesOff() {
      for (const entry of voices) {
        entry.note = null;
        entry.voice.triggerRelease(Math.max(entry.voice.context.immediate() + 0.005, entry.attackTime));
      }
    },
    dispose() {
      for (const { voice } of voices) voice.dispose();
      mod.dispose();
    },
  };
}

/** Registry entry for a poly voice, looked up by id so the SYNTHS order below stays explicit (= UI order). */
function polyFactory(id: string): SynthFactory {
  const spec = POLY_VOICES.find((v) => v.id === id)!;
  return { id: spec.id, name: spec.name, create: () => makePolySynth(spec) };
}

/** Registry of built-in synth engines. Array order = the UI's voice order. */
export const SYNTHS: SynthFactory[] = [
  polyFactory('lead'),
  { id: 'bass', name: 'Bass', create: createBass },
  polyFactory('pad'),
  polyFactory('piano'),
  polyFactory('organ'),
  { id: 'drum', name: 'Drum', create: createDrum },
];

export type { SynthFactory } from './synth';
