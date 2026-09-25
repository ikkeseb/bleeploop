//! Tone.js 15.1.22's monophonic voices: `instrument/Synth.js` ([`Synth`]: an oscillator through an
//! amplitude envelope), and `ModulationSynth.js` with `AMSynth.js` and `FMSynth.js`
//! ([`ModulationSynth`]: a carrier Synth and a modulator Synth, both at -10 dB), with the note
//! handling of `Monophonic.js` and the output `Volume` of `Instrument.js`.
//!
//! The graph, per quantum and in Blink's float arithmetic:
//! - Synth: `frequency` and `detune` signals into the oscillator, times the envelope, times the volume.
//! - FMSynth: the voice's `frequency` signal drives the carrier's; times `harmonicity` it drives the
//!   modulator's; times `modulationIndex` it feeds a gain whose gain is the modulator's output, which
//!   sums into the carrier's frequency. `detune` drives both.
//! - AMSynth: frequency as above without the index; the modulator's output, mapped from -1..1 to 0..1
//!   by a WaveShaper, is the gain on the carrier's output.
//!
//! The Source and Oscillator volumes inside each oscillator are 0 dB with nothing scheduled (a
//! copy), so they are left out, as are the zero-valued signals a `connectSignal` leaves behind.
//! `portamento` is 0 in every production voice and not ported.

use std::sync::Arc;

use crate::dsp::envelope::{Adsr, AmplitudeEnvelope};
use crate::dsp::gain::{GainNode, ParamGain};
use crate::dsp::oscillator::{PeriodicWave, ToneOscillator};
use crate::dsp::param::{Units, QUANTUM};
use crate::dsp::signal::{Signal, WaveShaper};

const Q: usize = QUANTUM;

/// Tone's `Synth`.
pub struct Synth {
    /// The OmniOscillator's frequency signal (Hz).
    pub frequency: Signal,
    /// The OmniOscillator's detune signal (cents).
    pub detune: Signal,
    pub oscillator: ToneOscillator,
    pub envelope: AmplitudeEnvelope,
    /// The Instrument's output `Volume` (dB).
    pub volume: GainNode,
    osc_out: [f32; Q],
    env_out: [f32; Q],
    out: [[f32; Q]; 1],
}

impl Synth {
    /// `new Synth({ oscillator: { type }, envelope, volume })`.
    pub fn new(wave: Arc<PeriodicWave>, envelope: Adsr, volume_db: f64, sample_rate: f32, frame: u64) -> Self {
        Synth {
            frequency: Signal::new(sample_rate, Units::Frequency, 440.0, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            oscillator: ToneOscillator::new(wave, sample_rate, frame),
            envelope: AmplitudeEnvelope::new(envelope, sample_rate, frame),
            volume: GainNode::new(sample_rate as f64, volume_db, Units::Decibels, frame),
            osc_out: [0.0; Q],
            env_out: [0.0; Q],
            out: [[0.0; Q]],
        }
    }

    /// Another signal drives this voice's frequency and detune (`connectSignal` into overridden
    /// signals: their schedules become 0 and the input sums in).
    fn override_signals(&mut self, frame: u64) {
        self.frequency.param.connect_signal(true, frame);
        self.detune.param.connect_signal(true, frame);
    }

    /// `triggerAttack(frequency, time, velocity)`.
    pub fn trigger_attack(&mut self, frequency: f64, time: f64, velocity: f64, frame: u64) {
        self.trigger_envelope_attack(time, velocity, frame);
        // setNote with no portamento.
        self.frequency.param.set_value_at_time(frequency, time, frame);
    }

    /// `triggerRelease(time)`.
    pub fn trigger_release(&mut self, time: f64, frame: u64) {
        self.trigger_envelope_release(time, frame);
    }

    pub(crate) fn trigger_envelope_attack(&mut self, time: f64, velocity: f64, frame: u64) {
        self.envelope.envelope.trigger_attack(time, velocity, frame);
        self.oscillator.start(time, frame);
        let adsr = self.envelope.envelope.adsr;
        if adsr.sustain == 0.0 {
            self.oscillator.stop(time + adsr.attack + adsr.decay, frame);
        }
    }

    pub(crate) fn trigger_envelope_release(&mut self, time: f64, frame: u64) {
        self.envelope.envelope.trigger_release(time, frame);
        self.oscillator.stop(time + self.envelope.envelope.adsr.release, frame);
    }

    /// Render the quantum at `q`, with the signals connected to frequency and detune, if any. `None`
    /// when the output is silent.
    pub fn process(&mut self, q: u64, frequency: Option<&[f32; Q]>, detune: Option<&[f32; Q]>) -> Option<&[f32; Q]> {
        let f = self.frequency.process(q, frequency);
        let d = self.detune.process(q, detune);
        let silent = self.oscillator.process(q, f, d, &mut self.osc_out);
        let silent = self.envelope.process(q, (!silent).then_some(&self.osc_out), &mut self.env_out);
        let silent = self.volume.process(q, (!silent).then_some(std::slice::from_ref(&self.env_out)), &mut self.out);
        (!silent).then_some(&self.out[0])
    }
}

/// What the modulator does to the carrier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Modulation {
    /// AMSynth: the modulator, mapped to 0..1, scales the carrier's amplitude.
    Am,
    /// FMSynth: the modulator, times frequency × `modulation_index`, adds to the carrier's frequency.
    Fm { modulation_index: f64 },
}

/// Tone's `ModulationSynth` as `AMSynth` or `FMSynth` build it.
pub struct ModulationSynth {
    modulation: Modulation,
    /// The voice's frequency signal (Hz), 0 until the first note.
    pub frequency: Signal,
    pub detune: Signal,
    /// A Multiply: frequency × harmonicity drives the modulator.
    harmonicity: GainNode,
    /// FMSynth's Multiply: frequency × modulationIndex feeds the modulation gain.
    modulation_index: Option<GainNode>,
    /// The gain the modulator (AM: mapped by `audio_to_gain`) drives.
    modulation_node: ParamGain,
    audio_to_gain: Option<WaveShaper>,
    pub carrier: Synth,
    pub modulator: Synth,
    /// The Instrument's output `Volume` (dB).
    pub volume: GainNode,
    harmonic: [[f32; Q]; 1],
    indexed: [[f32; Q]; 1],
    modulator_out: [f32; Q],
    shaped: [f32; Q],
    carrier_frequency: [f32; Q],
    carrier_out: [f32; Q],
    modulated: [f32; Q],
    out: [[f32; Q]; 1],
}

/// The carrier's and modulator's shapes, envelopes and the voice's modulation settings.
#[derive(Clone, Copy, Debug)]
pub struct ModulationOptions {
    pub modulation: Modulation,
    pub harmonicity: f64,
    pub envelope: Adsr,
    pub modulation_envelope: Adsr,
}

impl ModulationSynth {
    pub fn new(carrier: Arc<PeriodicWave>, modulator: Arc<PeriodicWave>, options: ModulationOptions, volume_db: f64, sample_rate: f32, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let multiply = |value: f64| {
            let mut g = GainNode::new(rate, 1.0, Units::Gain, frame);
            g.gain.set_value_at_time(value, 0.0, frame);
            g
        };
        let mut s = ModulationSynth {
            modulation: options.modulation,
            frequency: Signal::new(sample_rate, Units::Frequency, 0.0, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            harmonicity: multiply(options.harmonicity),
            modulation_index: match options.modulation {
                Modulation::Fm { modulation_index } => Some(multiply(modulation_index)),
                Modulation::Am => None,
            },
            // AM connects through a SignalOperator (connectSignal), FM with a plain connect; the gain's
            // schedule is 0 at 0 either way.
            modulation_node: ParamGain::new(sample_rate, 0.0, options.modulation == Modulation::Am, frame),
            audio_to_gain: (options.modulation == Modulation::Am).then(WaveShaper::audio_to_gain),
            carrier: Synth::new(carrier, options.envelope, -10.0, sample_rate, frame),
            modulator: Synth::new(modulator, options.modulation_envelope, -10.0, sample_rate, frame),
            volume: GainNode::new(rate, volume_db, Units::Decibels, frame),
            harmonic: [[0.0; Q]],
            indexed: [[0.0; Q]],
            modulator_out: [0.0; Q],
            shaped: [0.0; Q],
            carrier_frequency: [0.0; Q],
            carrier_out: [0.0; Q],
            modulated: [0.0; Q],
            out: [[0.0; Q]],
        };
        s.carrier.override_signals(frame);
        s.modulator.override_signals(frame);
        s
    }

    /// `triggerAttack(frequency, time, velocity)`: both envelopes and oscillators, then the note.
    pub fn trigger_attack(&mut self, frequency: f64, time: f64, velocity: f64, frame: u64) {
        self.carrier.trigger_envelope_attack(time, velocity, frame);
        self.modulator.trigger_envelope_attack(time, velocity, frame);
        self.frequency.param.set_value_at_time(frequency, time, frame);
    }

    /// `triggerRelease(time)`.
    pub fn trigger_release(&mut self, time: f64, frame: u64) {
        self.carrier.trigger_envelope_release(time, frame);
        self.modulator.trigger_envelope_release(time, frame);
    }

    /// Render the quantum at `q`; `None` when silent.
    pub fn process(&mut self, q: u64) -> Option<&[f32; Q]> {
        let f = self.frequency.process(q, None);
        let d = self.detune.process(q, None);
        self.harmonicity.process(q, Some(std::slice::from_ref(f)), &mut self.harmonic);
        let modulator = self.modulator.process(q, Some(&self.harmonic[0]), Some(d));
        match modulator {
            Some(m) => self.modulator_out.copy_from_slice(m),
            None => self.modulator_out.fill(0.0),
        }
        let modulator_silent = modulator.is_none();
        let carrier_silent = match self.modulation {
            Modulation::Fm { .. } => {
                let index = self.modulation_index.as_mut().expect("FM has an index");
                index.process(q, Some(std::slice::from_ref(f)), &mut self.indexed);
                let gain_input = (!modulator_silent).then_some(&self.modulator_out);
                self.modulation_node.process(q, Some(&self.indexed[0]), gain_input, &mut self.modulated);
                // The carrier's frequency signal sums the voice's frequency and the modulation.
                for ((c, &f), &m) in self.carrier_frequency.iter_mut().zip(f).zip(&self.modulated) {
                    *c = f + m;
                }
                match self.carrier.process(q, Some(&self.carrier_frequency), Some(d)) {
                    Some(c) => {
                        self.carrier_out.copy_from_slice(c);
                        false
                    }
                    None => true,
                }
            }
            Modulation::Am => {
                let shaper = self.audio_to_gain.as_ref().expect("AM has a shaper");
                shaper.process(&self.modulator_out, &mut self.shaped);
                let carrier = self.carrier.process(q, Some(f), Some(d));
                self.modulation_node.process(q, carrier, Some(&self.shaped), &mut self.carrier_out)
            }
        };
        let silent = self.volume.process(q, (!carrier_silent).then_some(std::slice::from_ref(&self.carrier_out)), &mut self.out);
        (!silent).then_some(&self.out[0])
    }
}

/// One voice of a poly synth.
pub enum Voice {
    Synth(Box<Synth>),
    Modulation(Box<ModulationSynth>),
}

impl Voice {
    pub fn trigger_attack(&mut self, frequency: f64, time: f64, velocity: f64, frame: u64) {
        match self {
            Voice::Synth(s) => s.trigger_attack(frequency, time, velocity, frame),
            Voice::Modulation(s) => s.trigger_attack(frequency, time, velocity, frame),
        }
    }

    pub fn trigger_release(&mut self, time: f64, frame: u64) {
        match self {
            Voice::Synth(s) => s.trigger_release(time, frame),
            Voice::Modulation(s) => s.trigger_release(time, frame),
        }
    }

    /// The voice's `detune` signal (Tone's `voice.detune`).
    pub fn detune(&mut self) -> &mut Signal {
        match self {
            Voice::Synth(s) => &mut s.detune,
            Voice::Modulation(s) => &mut s.detune,
        }
    }

    /// The voice's output `Volume` (Tone's `voice.volume`).
    pub fn volume(&mut self) -> &mut GainNode {
        match self {
            Voice::Synth(s) => &mut s.volume,
            Voice::Modulation(s) => &mut s.volume,
        }
    }

    pub fn process(&mut self, q: u64) -> Option<&[f32; Q]> {
        match self {
            Voice::Synth(s) => s.process(q, None, None),
            Voice::Modulation(s) => s.process(q),
        }
    }
}
