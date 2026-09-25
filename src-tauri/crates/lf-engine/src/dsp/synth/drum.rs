//! The drum kit: `src/audio/synths/drum.ts` on Tone.js 15.1.22's `instrument/MembraneSynth.js`,
//! `NoiseSynth.js` and `MetalSynth.js` (with `source/oscillator/FMOscillator.js`), each with the
//! output `Volume` of `Instrument.js` and the note handling of `Monophonic.js` and
//! `triggerAttackRelease`.
//!
//! - [`MembraneSynth`] (kick, kick 2, toms) is Tone's [`Synth`] on a sine whose frequency jumps to
//!   `note * octaves` and ramps exponentially down to the note over `pitchDecay`, with an exponential
//!   attack.
//! - [`NoiseSynth`] (snare, e-snare, clap) is a looped stereo noise table ([`Noise`], started from a
//!   random offset) through an amplitude envelope.
//! - [`MetalSynth`] (hats, rim, crash, ride, cowbell, tambourine) is six FM oscillators (square
//!   carrier and modulator) at inharmonic ratios of its frequency, summed into a highpass [`Filter`]
//!   (one biquad, Q 0) whose cutoff is its envelope scaled from `resonance` to `resonance *
//!   2^octaves`, times the same envelope. Inside each FMOscillator: `frequency × harmonicity` drives
//!   the modulator, `frequency × modulationIndex` times the modulator's output adds to the carrier's
//!   frequency. The FMOscillator's own frequency Signal, overridden by that connection and rendered
//!   from the first quantum on, passes its input through plus 0 and is left out (the carrier's and
//!   modulator's Signals are [`ToneOscillator`]'s to model); the detune chain carries MetalSynth's
//!   detune (0, the app never sets it) plus zeros, so one detune Signal feeds all twelve oscillators.
//!   The Oscillator and Source volumes are 0 dB copies, left out as in [`super::voice`].
//!
//! Every hit is `triggerAttackRelease(note, duration, time, velocity)`: with sustain 0 each voice's
//! source stops at `attack + decay`, and a release scheduled after that stop is a no-op for the source
//! (the envelope still releases if it has not reached 0). Durations are Tone notation at the
//! Transport's default 120 bpm (`'8n'` = 0.25 s; Tone's bpm round trip through ticks returns exactly
//! 120) and note names are Tone's `mtof` of their MIDI number, read back through `toFrequency`
//! (`1 / (1 / hertz)`). Velocity has a floor of 0.01; note-off does nothing.
//!
//! The kit sums its voices into a stereo pair: the noise voices are stereo, the rest mono (up-mixed to
//! both sides, as Blink's bus does). Each noise start draws its table offset from the kit's random
//! source, one draw per start in call order (Tone's `Math.random`).
//!
//! Deliberate deviation: a second hit on one voice at the same Tone time as its previous hit is
//! dropped. Tone asserts a restart is strictly later (`Source.start`) and throws midway through the
//! trigger, after the envelope's attack; a live context maps two hits inside one render quantum to one
//! time. Time and frame rules are [`super::poly`]'s.

use std::sync::Arc;

use super::poly::midi_to_freq;
use super::voice::Synth;
use crate::dsp::biquad::FilterType;
use crate::dsp::buffer_source::{AudioBuffer, Noise, MAX_CHANNELS};
use crate::dsp::envelope::{Adsr, AmplitudeEnvelope, Curve, Envelope};
use crate::dsp::fdlibm;
use crate::dsp::filter::{Filter, Rolloff};
use crate::dsp::gain::{GainNode, ParamGain};
use crate::dsp::oscillator::{OscillatorType, PeriodicWave, ToneOscillator};
use crate::dsp::param::{self, Units, QUANTUM};
use crate::dsp::rng::Mulberry32;
use crate::dsp::signal::{Scale, Signal};

const Q: usize = QUANTUM;

/// Tone notation at the Transport's default tempo: `'<n>n'` is `(60 / bpm) * (4 / n)` seconds.
fn note_value(n: f64) -> f64 {
    const BPM: f64 = 120.0;
    (60.0 / BPM) * (4.0 / n)
}

/// A note name's frequency as `toFrequency` returns it (`1 / (1 / mtof(midi))`).
fn note_frequency(midi: u8) -> f64 {
    1.0 / (1.0 / midi_to_freq(midi))
}

/// Tone's `MembraneSynth`.
pub struct MembraneSynth {
    pub synth: Synth,
    pitch_decay: f64,
    octaves: f64,
}

impl MembraneSynth {
    /// `new MembraneSynth({ volume, pitchDecay, octaves, envelope })` (the envelope's attack curve is
    /// MembraneSynth's exponential default).
    pub fn new(wave: Arc<PeriodicWave>, volume_db: f64, pitch_decay: f64, octaves: f64, envelope: Adsr, sample_rate: f32, frame: u64) -> Self {
        let envelope = Adsr { attack_curve: Curve::Exponential, ..envelope };
        MembraneSynth { synth: Synth::new(wave, envelope, volume_db, sample_rate, frame), pitch_decay, octaves }
    }

    /// `triggerAttackRelease(note, duration, time, velocity)` with `hertz` from `toFrequency(note)`.
    pub fn trigger_attack_release(&mut self, hertz: f64, duration: f64, time: f64, velocity: f64, frame: u64) {
        self.synth.trigger_envelope_attack(time, velocity, frame);
        // setNote: from `hertz * octaves` down to the note.
        let frequency = &mut self.synth.frequency.param;
        frequency.set_value_at_time(hertz * self.octaves, time, frame);
        frequency.exponential_ramp_to_value_at_time(hertz, time + self.pitch_decay, frame);
        self.synth.trigger_envelope_release(time + duration, frame);
    }

    pub fn process(&mut self, q: u64) -> Option<&[f32; Q]> {
        self.synth.process(q, None, None)
    }
}

/// Tone's `NoiseSynth`.
pub struct NoiseSynth {
    pub noise: Noise,
    pub envelope: AmplitudeEnvelope,
    /// The Instrument's output `Volume` (dB).
    pub volume: GainNode,
    channels: usize,
    env_out: [[f32; Q]; MAX_CHANNELS],
    out: [[f32; Q]; MAX_CHANNELS],
}

impl NoiseSynth {
    /// `new NoiseSynth({ volume, noise: { type }, envelope })`; `table` is the type's noise table.
    pub fn new(table: Arc<AudioBuffer>, volume_db: f64, envelope: Adsr, sample_rate: f32, frame: u64) -> Self {
        NoiseSynth {
            channels: table.number_of_channels(),
            noise: Noise::new(sample_rate, table, frame),
            envelope: AmplitudeEnvelope::new(envelope, sample_rate, frame),
            volume: GainNode::new(sample_rate as f64, volume_db, Units::Decibels, frame),
            env_out: [[0.0; Q]; MAX_CHANNELS],
            out: [[0.0; Q]; MAX_CHANNELS],
        }
    }

    /// `triggerAttackRelease(duration, time, velocity)`; `random` is the start's `Math.random` draw
    /// and `now` Tone's current time.
    pub fn trigger_attack_release(&mut self, duration: f64, time: f64, velocity: f64, random: f64, now: f64, frame: u64) {
        let adsr = self.envelope.envelope.adsr;
        self.envelope.envelope.trigger_attack(time, velocity, frame);
        self.noise.start(time, random, now, frame);
        if adsr.sustain == 0.0 {
            self.noise.stop(time + adsr.attack + adsr.decay, now, frame);
        }
        let release = time + duration;
        self.envelope.envelope.trigger_release(release, frame);
        self.noise.stop(release + adsr.release, now, frame);
    }

    /// Render the quantum at `q`; `None` when silent, else one slot per table channel.
    pub fn process(&mut self, q: u64) -> Option<&[[f32; Q]]> {
        let c = self.channels;
        self.noise.process(q);
        let input = (!self.noise.silent()).then(|| self.noise.output());
        let silent = self.envelope.process_channels(q, input, &mut self.env_out[..c]);
        let silent = self.volume.process(q, (!silent).then_some(&self.env_out[..c]), &mut self.out[..c]);
        (!silent).then_some(&self.out[..c])
    }
}

/// MetalSynth's partials (`inharmRatios`).
const INHARMONIC_RATIOS: [f64; 6] = [1.0, 1.483, 1.932, 2.546, 2.63, 3.897];

/// Tone's `FMOscillator` with square carrier and modulator, as MetalSynth builds it.
struct FmOscillator {
    carrier: ToneOscillator,
    modulator: ToneOscillator,
    /// Multiplies: frequency × harmonicity (the modulator's frequency), × modulationIndex.
    harmonicity: GainNode,
    modulation_index: GainNode,
    /// A Gain at 0 whose gain the modulator drives.
    modulation_node: ParamGain,
    harmonic: [[f32; Q]; 1],
    indexed: [[f32; Q]; 1],
    modulator_out: [f32; Q],
    modulated: [f32; Q],
    carrier_frequency: [f32; Q],
    out: [f32; Q],
}

impl FmOscillator {
    fn new(square: &Arc<PeriodicWave>, harmonicity: f64, modulation_index: f64, sample_rate: f32, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let multiply = |value: f64| {
            let mut g = GainNode::new(rate, 1.0, Units::Gain, frame);
            g.gain.set_value_at_time(value, 0.0, frame);
            g
        };
        FmOscillator {
            carrier: ToneOscillator::new(Arc::clone(square), sample_rate, frame),
            modulator: ToneOscillator::new(Arc::clone(square), sample_rate, frame),
            harmonicity: multiply(harmonicity),
            modulation_index: multiply(modulation_index),
            // A plain connect: the gain's schedule stays 0 at 0 and the modulator sums in.
            modulation_node: ParamGain::new(sample_rate, 0.0, false, frame),
            harmonic: [[0.0; Q]],
            indexed: [[0.0; Q]],
            modulator_out: [0.0; Q],
            modulated: [0.0; Q],
            carrier_frequency: [0.0; Q],
            out: [0.0; Q],
        }
    }

    fn start(&mut self, time: f64, frame: u64) {
        self.modulator.start(time, frame);
        self.carrier.start(time, frame);
    }

    fn stop(&mut self, time: f64, frame: u64) {
        self.modulator.stop(time, frame);
        self.carrier.stop(time, frame);
    }

    /// Render the quantum at `q` at `frequency`; `None` when silent.
    fn process(&mut self, q: u64, frequency: &[f32; Q], detune: &[f32; Q]) -> Option<&[f32; Q]> {
        let f = std::slice::from_ref(frequency);
        self.harmonicity.process(q, Some(f), &mut self.harmonic);
        self.modulation_index.process(q, Some(f), &mut self.indexed);
        let modulator_silent = self.modulator.process(q, &self.harmonic[0], detune, &mut self.modulator_out);
        let gain_input = (!modulator_silent).then_some(&self.modulator_out);
        self.modulation_node.process(q, Some(&self.indexed[0]), gain_input, &mut self.modulated);
        for ((c, &f), &m) in self.carrier_frequency.iter_mut().zip(frequency).zip(&self.modulated) {
            *c = f + m;
        }
        let silent = self.carrier.process(q, &self.carrier_frequency, detune, &mut self.out);
        (!silent).then_some(&self.out)
    }
}

/// MetalSynth's options.
#[derive(Clone, Copy, Debug)]
pub struct MetalOptions {
    pub volume_db: f64,
    pub harmonicity: f64,
    pub modulation_index: f64,
    pub resonance: f64,
    pub octaves: f64,
    pub attack: f64,
    pub decay: f64,
    pub release: f64,
}

/// Tone's `MetalSynth`.
pub struct MetalSynth {
    /// Hz; `frequency.value` is set after construction.
    pub frequency: Signal,
    detune: Signal,
    multipliers: [GainNode; 6],
    oscillators: [FmOscillator; 6],
    highpass: Filter,
    /// `_filterFreqScaler`: the envelope to the highpass cutoff.
    scale: Scale,
    pub envelope: Envelope,
    /// `_amplitude`: a Gain at 0 driven by the envelope.
    amplitude: ParamGain,
    /// The Instrument's output `Volume` (dB).
    pub volume: GainNode,
    partial: [[f32; Q]; 1],
    sum: [f32; Q],
    filtered: [f32; Q],
    amp_out: [f32; Q],
    out: [[f32; Q]; 1],
}

impl MetalSynth {
    /// `new MetalSynth({ volume, ...options })` then `frequency.value = frequency`, at Tone time `now`.
    pub fn new(square: &Arc<PeriodicWave>, options: MetalOptions, frequency: f64, sample_rate: f32, now: f64, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let mut highpass = Filter::new(sample_rate, FilterType::Highpass, 350.0, 0.0, Rolloff::Db12, frame);
        // `envelope.chain(_filterFreqScaler, _highpass.frequency)`.
        highpass.frequency.param.connect_signal(true, frame);
        let resonance = 1.0 / (1.0 / options.resonance);
        let mut scale = Scale::new(sample_rate, resonance, 7000.0, frame);
        // The octaves setter: max = min * 2^octaves.
        scale.set_range(resonance, resonance * fdlibm::pow(2.0, options.octaves), now, frame);
        let mut synth = MetalSynth {
            frequency: Signal::new(sample_rate, Units::Frequency, 0.0, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            multipliers: INHARMONIC_RATIOS.map(|ratio| {
                let mut g = GainNode::new(rate, 1.0, Units::Gain, frame);
                g.gain.set_value_at_time(ratio, 0.0, frame);
                g
            }),
            oscillators: std::array::from_fn(|_| FmOscillator::new(square, options.harmonicity, options.modulation_index, sample_rate, frame)),
            highpass,
            scale,
            envelope: Envelope::new(Adsr::new(options.attack, options.decay, 0.0, options.release), sample_rate, frame),
            // `envelope.connect(_amplitude.gain)`: connectSignal into a Param.
            amplitude: ParamGain::new(sample_rate, 0.0, true, frame),
            volume: GainNode::new(rate, options.volume_db, Units::Decibels, frame),
            partial: [[0.0; Q]],
            sum: [0.0; Q],
            filtered: [0.0; Q],
            amp_out: [0.0; Q],
            out: [[0.0; Q]],
        };
        synth.frequency.param.set_value(frequency, now, frame);
        synth
    }

    /// `triggerAttackRelease(hertz, duration, time, velocity)`.
    pub fn trigger_attack_release(&mut self, hertz: f64, duration: f64, time: f64, velocity: f64, frame: u64) {
        let adsr = self.envelope.adsr;
        self.envelope.trigger_attack(time, velocity, frame);
        for osc in self.oscillators.iter_mut() {
            osc.start(time, frame);
        }
        for osc in self.oscillators.iter_mut() {
            osc.stop(time + adsr.attack + adsr.decay, frame);
        }
        self.frequency.param.set_value_at_time(hertz, time, frame);
        let release = time + duration;
        self.envelope.trigger_release(release, frame);
        for osc in self.oscillators.iter_mut() {
            osc.stop(release + adsr.release, frame);
        }
    }

    pub fn process(&mut self, q: u64) -> Option<&[f32; Q]> {
        let f = self.frequency.process(q, None);
        let d = self.detune.process(q, None);
        self.sum.fill(0.0);
        for (mult, osc) in self.multipliers.iter_mut().zip(self.oscillators.iter_mut()) {
            mult.process(q, Some(std::slice::from_ref(f)), &mut self.partial);
            if let Some(x) = osc.process(q, &self.partial[0], d) {
                for (s, &v) in self.sum.iter_mut().zip(x) {
                    *s += v;
                }
            }
        }
        let env = self.envelope.process(q);
        let cutoff = self.scale.process(q, Some(env));
        self.highpass.begin_quantum_driven(q, Some(cutoff));
        self.highpass.process(0, &self.sum, &mut self.filtered);
        let silent = self.amplitude.process(q, Some(&self.filtered), Some(env), &mut self.amp_out);
        let silent = self.volume.process(q, (!silent).then_some(std::slice::from_ref(&self.amp_out)), &mut self.out);
        (!silent).then_some(&self.out[0])
    }
}

/// One pad of the kit.
enum Voice {
    Membrane(Box<MembraneSynth>),
    Noise(Box<NoiseSynth>),
    Metal(Box<MetalSynth>),
}

/// How a pad fires: `triggerAttackRelease`'s note (MIDI, or hertz for a MetalSynth) and duration.
#[derive(Clone, Copy)]
enum Pitch {
    Note(u8),
    Hertz(f64),
    Unpitched,
}

struct Pad {
    note: u8,
    pitch: Pitch,
    /// Notation: `'<n>n'`.
    duration: f64,
    voice: Voice,
    last_hit: Option<f64>,
}

/// `createDrum()`: sixteen one-shot voices on GM notes. Stereo output.
pub struct DrumKit {
    sample_rate: f32,
    pads: Vec<Pad>,
    rng: Mulberry32,
    left: [f32; Q],
    right: [f32; Q],
    /// The quantum `left`/`right` hold.
    rendered: Option<u64>,
}

impl DrumKit {
    /// Build the kit (allocates). `white` and `pink` are Tone's noise tables (`dsp::noise`), `rng`
    /// the source of the noise start offsets; `now` is Tone's time and `frame` the frame being
    /// rendered.
    pub fn new(sample_rate: f32, white: &Arc<AudioBuffer>, pink: &Arc<AudioBuffer>, rng: Mulberry32, now: f64, frame: u64) -> Self {
        let sine = Arc::new(PeriodicWave::basic(OscillatorType::Sine, sample_rate));
        let square = Arc::new(PeriodicWave::basic(OscillatorType::Square, sample_rate));
        let sr = sample_rate;
        let membrane = |volume_db, pitch_decay, octaves, attack, decay, release| {
            let adsr = Adsr::new(attack, decay, 0.0, release);
            Voice::Membrane(Box::new(MembraneSynth::new(Arc::clone(&sine), volume_db, pitch_decay, octaves, adsr, sr, frame)))
        };
        let noise = |table: &Arc<AudioBuffer>, volume_db, attack, decay, release| {
            Voice::Noise(Box::new(NoiseSynth::new(Arc::clone(table), volume_db, Adsr::new(attack, decay, 0.0, release), sr, frame)))
        };
        #[allow(clippy::too_many_arguments)]
        let metal = |volume_db, hertz, harmonicity, modulation_index, resonance, octaves, decay, release| {
            let options = MetalOptions { volume_db, harmonicity, modulation_index, resonance, octaves, attack: 0.001, decay, release };
            Voice::Metal(Box::new(MetalSynth::new(&square, options, hertz, sr, now, frame)))
        };
        let pad = |note, pitch, n: f64, voice| Pad { note, pitch, duration: note_value(n), voice, last_hit: None };
        // drum.ts's voices, in its `triggers` order.
        let pads = vec![
            pad(36, Pitch::Note(24), 8.0, membrane(-6.0, 0.05, 6.0, 0.001, 0.35, 0.12)), // kick, 'C1'
            pad(35, Pitch::Note(21), 4.0, membrane(-6.0, 0.08, 5.0, 0.001, 0.5, 0.4)),   // kick 2, 'A0'
            pad(38, Pitch::Unpitched, 16.0, noise(white, -9.0, 0.001, 0.14, 0.06)),      // snare
            pad(40, Pitch::Unpitched, 16.0, noise(white, -10.0, 0.001, 0.1, 0.03)),      // e-snare
            pad(39, Pitch::Unpitched, 32.0, noise(pink, -11.0, 0.001, 0.06, 0.04)),      // clap
            pad(37, Pitch::Hertz(800.0), 64.0, metal(-17.0, 800.0, 5.1, 32.0, 3000.0, 1.0, 0.05, 0.01)), // rim
            pad(42, Pitch::Hertz(400.0), 32.0, metal(-14.0, 400.0, 5.1, 32.0, 4000.0, 1.5, 0.06, 0.01)), // closed hat
            pad(44, Pitch::Hertz(400.0), 64.0, metal(-16.0, 400.0, 5.1, 32.0, 4000.0, 1.5, 0.04, 0.01)), // pedal hat
            pad(46, Pitch::Hertz(400.0), 8.0, metal(-14.0, 400.0, 5.1, 32.0, 4000.0, 1.5, 0.35, 0.1)),   // open hat
            pad(49, Pitch::Hertz(300.0), 2.0, metal(-20.0, 300.0, 8.0, 40.0, 5000.0, 2.0, 1.5, 1.0)),    // crash
            pad(51, Pitch::Hertz(500.0), 4.0, metal(-20.0, 500.0, 12.0, 16.0, 6000.0, 1.5, 0.6, 0.4)),   // ride
            pad(56, Pitch::Hertz(540.0), 16.0, metal(-22.0, 540.0, 1.48, 16.0, 2500.0, 1.0, 0.25, 0.1)), // cowbell
            pad(54, Pitch::Hertz(1200.0), 32.0, metal(-18.0, 1200.0, 12.0, 24.0, 7000.0, 1.5, 0.12, 0.05)), // tambourine
            pad(45, Pitch::Note(31), 8.0, membrane(-8.0, 0.02, 4.0, 0.001, 0.4, 0.3)),   // low tom, 'G1'
            pad(47, Pitch::Note(36), 8.0, membrane(-8.0, 0.02, 4.0, 0.001, 0.4, 0.3)),   // mid tom, 'C2'
            pad(50, Pitch::Note(41), 8.0, membrane(-8.0, 0.02, 4.0, 0.001, 0.4, 0.3)),   // high tom, 'F2'
        ];
        DrumKit { sample_rate, pads, rng, left: [0.0; Q], right: [0.0; Q], rendered: None }
    }

    /// The kit's GM notes.
    pub fn notes(&self) -> impl Iterator<Item = u8> + '_ {
        self.pads.iter().map(|p| p.note)
    }

    /// `noteOn(note, velocity, time)`; velocity 0..1. A note outside the kit does nothing.
    pub fn note_on(&mut self, note: u8, velocity: f64, time: f64, frame: u64) {
        let Some(pad) = self.pads.iter_mut().find(|p| p.note == note) else { return };
        let now = param::context_time(frame, self.sample_rate as f64);
        // Source.start clamps to the current time before it compares with the last start.
        let at = time.max(now);
        if pad.last_hit.is_some_and(|last| param::eq(at, last)) {
            return;
        }
        pad.last_hit = Some(at);
        let v = velocity.max(0.01);
        match (&mut pad.voice, pad.pitch) {
            (Voice::Membrane(m), Pitch::Note(midi)) => m.trigger_attack_release(note_frequency(midi), pad.duration, time, v, frame),
            (Voice::Noise(n), _) => n.trigger_attack_release(pad.duration, time, v, self.rng.next_f64(), now, frame),
            (Voice::Metal(m), Pitch::Hertz(hertz)) => m.trigger_attack_release(hertz, pad.duration, time, v, frame),
            _ => unreachable!("a pad's pitch matches its voice"),
        }
    }

    /// `noteOff`: one-shot voices ignore it.
    pub fn note_off(&mut self, _note: u8, _time: f64, _frame: u64) {}

    fn process(&mut self, q: u64) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        for pad in self.pads.iter_mut() {
            match &mut pad.voice {
                Voice::Membrane(m) => add_mono(m.process(q), &mut self.left, &mut self.right),
                Voice::Metal(m) => add_mono(m.process(q), &mut self.left, &mut self.right),
                Voice::Noise(n) => {
                    if let Some(x) = n.process(q) {
                        let r = if x.len() > 1 { 1 } else { 0 };
                        for (o, &v) in self.left.iter_mut().zip(&x[0]) {
                            *o += v;
                        }
                        for (o, &v) in self.right.iter_mut().zip(&x[r]) {
                            *o += v;
                        }
                    }
                }
            }
        }
    }

    /// Render frames `frame..frame + left.len()` into both channels. Blocks must follow each other.
    pub fn render(&mut self, frame: u64, left: &mut [f32], right: &mut [f32]) {
        debug_assert_eq!(left.len(), right.len());
        let mut done = 0;
        while done < left.len() {
            let f = frame + done as u64;
            let q = f - f % Q as u64;
            if self.rendered != Some(q) {
                self.process(q);
                self.rendered = Some(q);
            }
            let at = (f - q) as usize;
            let n = (Q - at).min(left.len() - done);
            left[done..done + n].copy_from_slice(&self.left[at..at + n]);
            right[done..done + n].copy_from_slice(&self.right[at..at + n]);
            done += n;
        }
    }
}

/// A mono voice up-mixed to both sides.
fn add_mono(x: Option<&[f32; Q]>, left: &mut [f32; Q], right: &mut [f32; Q]) {
    if let Some(x) = x {
        for ((l, r), &v) in left.iter_mut().zip(right.iter_mut()).zip(x) {
            *l += v;
            *r += v;
        }
    }
}
