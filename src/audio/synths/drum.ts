import { MembraneSynth, MetalSynth, NoiseSynth, now } from 'tone';
import { engine } from '../engine';
import type { SynthEngine } from './synth';

/**
 * A drum voice's static descriptor — the single source of truth for the kit's note set, display
 * label, and PC-keyboard pad key. Consumed by BOTH this synth's note dispatch and the on-screen
 * drum-pad UI (src/ui/keyboard/Keyboard.tsx), so the two can never drift. A MIDI controller plays
 * the same `note` numbers straight through input-router; the `key` is PC-keyboard-only.
 */
export interface DrumVoice {
  readonly note: number; // GM percussion MIDI note
  readonly label: string; // short display name for the pad
  readonly key: string; // PC-keyboard key that fires this pad
}

/**
 * The kit, in pad-grid order (4 columns × 4 rows). The UI renders these left-to-right then
 * top-to-bottom, so the array order IS the visual layout. Key rows: 1234 / qwer / asdf / zxcv.
 * The home row (a/s/d/f) holds the four essentials so they sit under the resting hand.
 *
 * GM-inspired note map (notes are standard General MIDI percussion assignments):
 *   49 Crash · 51 Ride · 56 Cowbell · 54 Tambourine
 *   39 Clap · 40 Electric snare · 44 Pedal hat · 37 Rim/side-stick
 *   36 Kick · 38 Snare · 42 Closed hat · 46 Open hat
 *   35 Kick 2 (acoustic BD) · 45 Low tom · 47 Mid tom · 50 High tom
 */
export const DRUM_KIT: ReadonlyArray<DrumVoice> = [
  // row 1 — cymbals & accents
  { note: 49, label: 'Crash', key: '1' },
  { note: 51, label: 'Ride', key: '2' },
  { note: 56, label: 'Cowbell', key: '3' },
  { note: 54, label: 'Tamb', key: '4' },
  // row 2 — extra snares & hats
  { note: 39, label: 'Clap', key: 'q' },
  { note: 40, label: 'E-Snr', key: 'w' },
  { note: 44, label: 'P-Hat', key: 'e' },
  { note: 37, label: 'Rim', key: 'r' },
  // row 3 — the essentials
  { note: 36, label: 'Kick', key: 'a' },
  { note: 38, label: 'Snare', key: 's' },
  { note: 42, label: 'CH', key: 'd' },
  { note: 46, label: 'OH', key: 'f' },
  // row 4 — second kick & toms
  { note: 35, label: 'Kick 2', key: 'z' },
  { note: 45, label: 'Lo Tom', key: 'x' },
  { note: 47, label: 'Mid Tom', key: 'c' },
  { note: 50, label: 'Hi Tom', key: 'v' },
];

type MetalOpts = ConstructorParameters<typeof MetalSynth>[0];

/**
 * Drum Machine — a 16-voice GM-style kit. noteOff is ignored; every hit is a one-shot. Notes not
 * in DRUM_KIT are silently ignored. Per-voice gain staging trims each element so a full pattern
 * sits under the master sum (the master limiter is only a net); the kick stays the reference.
 *
 * TRIGGER-SIGNATURE TRAP (this once hid both hi-hats for two phases): pitched voices —
 * MembraneSynth and MetalSynth — take `(note, duration, time, velocity)`; NoiseSynth is NOT
 * pitched and takes `(duration, time, velocity)` — duration first, no note. Conflating them reads
 * the duration as a (bogus) note and the voice goes silent. Every call below respects this.
 *
 * NOTE: per-voice volumes here were set by recipe (MetalSynth runs hot — cymbals are trimmed
 * heavily), but never balanced by ear — peak amplitude ≠ perceived loudness for a bright cymbal
 * vs a sub kick. A real ear pass is an open follow-up.
 */
export function createDrum(): SynthEngine {
  // MetalSynth's `frequency` is a Signal, not a constructor option — it must be set as a property
  // after construction. This helper bakes in that pattern plus the bus connection.
  const makeMetal = (volume: number, freq: number, opts: MetalOpts): MetalSynth => {
    const m = new MetalSynth({ volume, ...opts });
    m.frequency.value = freq;
    m.connect(engine.instrumentBus);
    return m;
  };

  // --- membranes (kick + toms): pitched, note-first ---
  const kick = new MembraneSynth({
    volume: -6,
    pitchDecay: 0.05,
    octaves: 6,
    envelope: { attack: 0.001, decay: 0.35, sustain: 0.0, release: 0.12 },
  });
  kick.connect(engine.instrumentBus);

  const kick2 = new MembraneSynth({
    volume: -6,
    pitchDecay: 0.08,
    octaves: 5,
    envelope: { attack: 0.001, decay: 0.5, sustain: 0.0, release: 0.4 },
  });
  kick2.connect(engine.instrumentBus);

  // Toms share one recipe at three pitches; separate instances so a fill can overlap rings.
  const tomEnv = { attack: 0.001, decay: 0.4, sustain: 0.0, release: 0.3 } as const;
  const loTom = new MembraneSynth({ volume: -8, pitchDecay: 0.02, octaves: 4, envelope: { ...tomEnv } });
  const midTom = new MembraneSynth({ volume: -8, pitchDecay: 0.02, octaves: 4, envelope: { ...tomEnv } });
  const hiTom = new MembraneSynth({ volume: -8, pitchDecay: 0.02, octaves: 4, envelope: { ...tomEnv } });
  loTom.connect(engine.instrumentBus);
  midTom.connect(engine.instrumentBus);
  hiTom.connect(engine.instrumentBus);

  // --- noises (snares + clap): NOT pitched, duration-first ---
  const snare = new NoiseSynth({
    volume: -9,
    noise: { type: 'white' },
    envelope: { attack: 0.001, decay: 0.14, sustain: 0.0, release: 0.06 },
  });
  snare.connect(engine.instrumentBus);

  const eSnare = new NoiseSynth({
    volume: -10,
    noise: { type: 'white' },
    envelope: { attack: 0.001, decay: 0.1, sustain: 0.0, release: 0.03 },
  });
  eSnare.connect(engine.instrumentBus);

  const clap = new NoiseSynth({
    volume: -11,
    noise: { type: 'pink' },
    envelope: { attack: 0.001, decay: 0.06, sustain: 0.0, release: 0.04 },
  });
  clap.connect(engine.instrumentBus);

  // --- metals (hats, cymbals, rim, cowbell, tambourine): pitched, note-first ---
  const hatOpts = { harmonicity: 5.1, modulationIndex: 32, resonance: 4000, octaves: 1.5 } as const;
  const closedHat = makeMetal(-14, 400, { ...hatOpts, envelope: { attack: 0.001, decay: 0.06, release: 0.01 } });
  const pedalHat = makeMetal(-16, 400, { ...hatOpts, envelope: { attack: 0.001, decay: 0.04, release: 0.01 } });
  const openHat = makeMetal(-14, 400, { ...hatOpts, envelope: { attack: 0.001, decay: 0.35, release: 0.1 } });

  const rim = makeMetal(-17, 800, {
    harmonicity: 5.1, modulationIndex: 32, resonance: 3000, octaves: 1,
    envelope: { attack: 0.001, decay: 0.05, release: 0.01 },
  });
  const crash = makeMetal(-20, 300, {
    harmonicity: 8, modulationIndex: 40, resonance: 5000, octaves: 2,
    envelope: { attack: 0.001, decay: 1.5, release: 1.0 },
  });
  const ride = makeMetal(-20, 500, {
    harmonicity: 12, modulationIndex: 16, resonance: 6000, octaves: 1.5,
    envelope: { attack: 0.001, decay: 0.6, release: 0.4 },
  });
  // 808 cowbell: a non-integer harmonicity puts the second partial ~an augmented-4th above the
  // 540 Hz base (≈800 Hz), the classic two-tone clang.
  const cowbell = makeMetal(-22, 540, {
    harmonicity: 1.48, modulationIndex: 16, resonance: 2500, octaves: 1,
    envelope: { attack: 0.001, decay: 0.25, release: 0.1 },
  });
  const tamb = makeMetal(-18, 1200, {
    harmonicity: 12, modulationIndex: 24, resonance: 7000, octaves: 1.5,
    envelope: { attack: 0.001, decay: 0.12, release: 0.05 },
  });

  // note -> one-shot trigger. Pitched voices pass the note first; NoiseSynth passes duration first.
  const triggers = new Map<number, (t: number, v: number) => void>([
    [36, (t, v) => kick.triggerAttackRelease('C1', '8n', t, v)],
    [35, (t, v) => kick2.triggerAttackRelease('A0', '4n', t, v)],
    [38, (t, v) => snare.triggerAttackRelease('16n', t, v)],
    [40, (t, v) => eSnare.triggerAttackRelease('16n', t, v)],
    [39, (t, v) => clap.triggerAttackRelease('32n', t, v)],
    [37, (t, v) => rim.triggerAttackRelease(800, '64n', t, v)],
    [42, (t, v) => closedHat.triggerAttackRelease(400, '32n', t, v)],
    [44, (t, v) => pedalHat.triggerAttackRelease(400, '64n', t, v)],
    [46, (t, v) => openHat.triggerAttackRelease(400, '8n', t, v)],
    [49, (t, v) => crash.triggerAttackRelease(300, '2n', t, v)],
    [51, (t, v) => ride.triggerAttackRelease(500, '4n', t, v)],
    [56, (t, v) => cowbell.triggerAttackRelease(540, '16n', t, v)],
    [54, (t, v) => tamb.triggerAttackRelease(1200, '32n', t, v)],
    [45, (t, v) => loTom.triggerAttackRelease('G1', '8n', t, v)],
    [47, (t, v) => midTom.triggerAttackRelease('C2', '8n', t, v)],
    [50, (t, v) => hiTom.triggerAttackRelease('F2', '8n', t, v)],
  ]);

  // Drift guard: every kit descriptor note must have a trigger, or a pad would be silent.
  for (const voice of DRUM_KIT) {
    if (!triggers.has(voice.note)) {
      throw new Error(`createDrum: DRUM_KIT note ${voice.note} (${voice.label}) has no trigger`);
    }
  }

  const all = [
    kick, kick2, loTom, midTom, hiTom, snare, eSnare, clap,
    closedHat, pedalHat, openHat, rim, crash, ride, cowbell, tamb,
  ];

  return {
    id: 'drum',
    name: 'Drum',
    noteOn(note, velocity, time) {
      const t = time ?? now();
      const v = Math.max(0.01, velocity);
      triggers.get(note)?.(t, v);
    },
    noteOff(_note, _time) {
      // One-shot voices: noteOff is intentionally ignored
    },
    allNotesOff() {
      // Nothing sustained to release
    },
    dispose() {
      for (const voice of all) voice.dispose();
    },
  };
}
