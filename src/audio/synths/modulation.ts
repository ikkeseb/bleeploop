import { Gain, Vibrato } from 'tone';
import { engine } from '../engine';

/**
 * MIDI mod wheel maps to vibrato depth. Full wheel = a musical (not seasick) vibrato, so we cap
 * well below Vibrato's 1.0 maximum.
 */
const MOD_WHEEL_MAX_DEPTH = 0.35;
/** Vibrato rate — a natural hand-vibrato sits around 5–6 Hz. */
const VIBRATO_RATE_HZ = 5.5;
/**
 * Crossfade span for swapping the vibrato delay line in/out. Longer than the delay difference it
 * masks (~2.5 ms), so a mid-note wheel move from/to 0 ramps the waveform step instead of clicking.
 */
const BYPASS_CROSSFADE_S = 0.01;

export interface Modulation {
  /** The node the synth source connects into; it feeds engine.instrumentBus. */
  readonly input: Gain;
  /** Pitch bend in semitones (±). */
  setPitchBend(semitones: number): void;
  /** Mod-wheel depth 0..1. */
  setModulation(depth: number): void;
  dispose(): void;
}

/**
 * Shared pitch-bend + mod-wheel plumbing for the pitched built-in synths. The source connects into
 * `input`, which fans out to two pre-built paths to instrumentBus: a dry pass-through and the
 * Vibrato. Mod wheel drives the vibrato depth; pitch bend is applied to the source's own detune via
 * the caller-supplied `applyDetuneCents` (PolySynth.set for the poly voices, mono.detune.value for
 * the bass).
 *
 * The bypass exists because Tone's Vibrato carries a constant ~2.5 ms delay-line latency even at
 * depth 0 (default maxDelay 5 ms, centre ~2.5 ms). Mod-wheel default is 0, so an always-spliced
 * Vibrato would tax every note with latency no one is using. So at depth 0 the signal takes the dry
 * path; the Vibrato is faded in only while depth > 0. Both paths stay pre-built — only the two
 * output gains ramp, never a per-note or per-tick node build, and only ONE path is audible in
 * steady state (a standing dry+wet mix would comb-filter on the 2.5 ms offset).
 */
export function createModulation(applyDetuneCents: (cents: number) => void): Modulation {
  const input = new Gain();
  const dryGain = new Gain(1); // depth 0 is the default → dry path is live at construction.
  const wetGain = new Gain(0);
  const vibrato = new Vibrato({ frequency: VIBRATO_RATE_HZ, depth: 0 });

  input.connect(dryGain);
  input.connect(vibrato);
  dryGain.connect(engine.instrumentBus);
  vibrato.connect(wetGain);
  wetGain.connect(engine.instrumentBus);

  let wet = false; // tracks which path is live so we only ramp on a 0 ↔ >0 transition.

  return {
    input,
    setPitchBend(semitones) {
      applyDetuneCents(semitones * 100);
    },
    setModulation(depth) {
      const d = depth < 0 ? 0 : depth > 1 ? 1 : depth;
      vibrato.depth.value = d * MOD_WHEEL_MAX_DEPTH;
      const wantWet = d > 0;
      if (wantWet !== wet) {
        wet = wantWet;
        wetGain.gain.rampTo(wantWet ? 1 : 0, BYPASS_CROSSFADE_S);
        dryGain.gain.rampTo(wantWet ? 0 : 1, BYPASS_CROSSFADE_S);
      }
    },
    dispose() {
      input.dispose();
      dryGain.dispose();
      wetGain.dispose();
      vibrato.dispose();
    },
  };
}
