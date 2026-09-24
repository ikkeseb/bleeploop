//! Tone.js 15.1.22's `source/oscillator/LFO.js` ([`Lfo`]) and `effect/Vibrato.js` ([`Vibrato`]).
//!
//! The LFO is a Tone Oscillator (a custom wave when its phase is not 0), times `amplitude`, mapped from
//! -1..1 to 0..1 (`AudioToGain`), then scaled to `min..max` (`Scale`: a Multiply by `max - min`, an Add
//! of `min`). Its stopped-value signal and zeros only add +0 once it is started, and it is started when
//! built, so they are left out. Tone builds the phase-shifted wave from `Math.sin`/`Math.cos` of the
//! phase for the first partial: `real[1] = -sin(phase)`, `imag[1] = cos(phase)`.
//!
//! The Vibrato delays its input by the LFO: a Delay of `maxDelay` whose `delayTime` is the LFO scaled
//! to 0..maxDelay, so at depth 0 it rests at half the maximum. Its Effect wrapper is a CrossFade at
//! wet 1: a StereoPanner panned hard right splits a constant 1 into the dry gain (cos(pi/2), Blink's
//! 6.1e-17, kept) and the wet gain (sin(pi/2) = 1).

use std::sync::Arc;

use super::signal::{Signal, WaveShaper};
use crate::dsp::delay::DelayNode;
use crate::dsp::gain::GainNode;
use crate::dsp::oscillator::{PeriodicWave, ToneOscillator};
use crate::dsp::param::{Units, QUANTUM};

const Q: usize = QUANTUM;

/// Tone's LFO with a sine wave, started when built.
pub struct Lfo {
    pub frequency: Signal,
    detune: Signal,
    oscillator: ToneOscillator,
    /// Tone's `amplitude` (normalRange).
    pub amplitude: GainNode,
    a2g: WaveShaper,
    /// Scale's Multiply (`max - min`) and its Add (`min`).
    scale: GainNode,
    add: Signal,
    osc_out: [f32; Q],
    amplified: [[f32; Q]; 1],
    mapped: [f32; Q],
    scaled: [[f32; Q]; 1],
    out: [f32; Q],
}

impl Lfo {
    /// The custom wave of a sine LFO at `phase` degrees (Oscillator `_getRealImaginary`, 2048
    /// coefficients, one partial). Tone caches custom waves module-wide by type and phase
    /// (`Oscillator._periodicWaveCache`), so every later context reuses the wave the first one built,
    /// tables, rate scale and all: `sample_rate` is that first context's rate. An oscillator in a
    /// context at another rate then runs at `frequency * its rate / sample_rate`.
    pub fn sine_wave(phase_degrees: f64, sample_rate: f32) -> PeriodicWave {
        let phase = (phase_degrees * std::f64::consts::PI) / 180.0;
        let mut real = vec![0.0f32; 2048];
        let mut imag = vec![0.0f32; 2048];
        real[1] = (-phase.sin()) as f32;
        imag[1] = phase.cos() as f32;
        PeriodicWave::custom(&real, &imag, sample_rate)
    }

    /// `new LFO({ frequency, min, max, amplitude })` with `wave`, then `start(now)`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(wave: Arc<PeriodicWave>, frequency: f64, min: f64, max: f64, amplitude: f64, now: f64, sample_rate: f32, frame: u64) -> Self {
        let rate = sample_rate as f64;
        let mut scale = GainNode::new(rate, 1.0, Units::Gain, frame);
        scale.gain.set_value_at_time(max - min, 0.0, frame);
        let mut lfo = Lfo {
            frequency: Signal::new(sample_rate, Units::Frequency, frequency, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            oscillator: ToneOscillator::new(wave, sample_rate, frame),
            amplitude: GainNode::new(rate, amplitude, Units::NormalRange, frame),
            a2g: WaveShaper::audio_to_gain(),
            scale,
            add: Signal::new(sample_rate, Units::Number, min, frame),
            osc_out: [0.0; Q],
            amplified: [[0.0; Q]],
            mapped: [0.0; Q],
            scaled: [[0.0; Q]],
            out: [0.0; Q],
        };
        lfo.oscillator.start(now, frame);
        lfo
    }

    /// Render the quantum at `q`.
    pub fn process(&mut self, q: u64) -> &[f32; Q] {
        let f = self.frequency.process(q, None);
        let d = self.detune.process(q, None);
        let silent = self.oscillator.process(q, f, d, &mut self.osc_out);
        self.amplitude.process(q, (!silent).then_some(std::slice::from_ref(&self.osc_out)), &mut self.amplified);
        // The shaper's input is never silent (the stopped signal feeds it too): zeros map as well.
        self.a2g.process(&self.amplified[0], &mut self.mapped);
        self.scale.process(q, Some(std::slice::from_ref(&self.mapped)), &mut self.scaled);
        let add = self.add.process(q, None);
        for ((o, &x), &a) in self.out.iter_mut().zip(&self.scaled[0]).zip(add) {
            *o = x + a;
        }
        &self.out
    }
}

/// Tone's `Vibrato` (sine, wet 1).
pub struct Vibrato {
    pub lfo: Lfo,
    delay: DelayNode,
    /// CrossFade's gains at wet 1: dry cos(pi/2), wet sin(pi/2).
    dry_gain: f32,
    wet_gain: f32,
    out: [f32; Q],
}

impl Vibrato {
    /// `new Vibrato({ frequency, depth, maxDelay })`, built at Tone time `now`. `wave` is the LFO's
    /// [`Lfo::sine_wave`] at -90 degrees (see [`Lfo::sine_wave`] for the rate it is built at).
    pub fn new(wave: Arc<PeriodicWave>, frequency: f64, depth: f64, max_delay: f64, now: f64, sample_rate: f32, frame: u64) -> Self {
        // LFO amplitude 1, then `depth.value = depth`.
        let mut lfo = Lfo::new(wave, frequency, 0.0, max_delay, 1.0, now, sample_rate, frame);
        lfo.amplitude.gain.set_value(depth, now, frame);
        let mut delay = DelayNode::new(sample_rate, 0.0, max_delay, frame);
        delay.delay_time.connect_signal(false, frame);
        // StereoPanner, mono input, pan 1: (float)(1 * cos(pi/2)), (float)(1 * sin(pi/2)).
        let pan_radian = std::f64::consts::FRAC_PI_2;
        Vibrato { lfo, delay, dry_gain: pan_radian.cos() as f32, wet_gain: pan_radian.sin() as f32, out: [0.0; Q] }
    }

    /// The LFO's `amplitude` (Tone's `depth`).
    pub fn depth(&mut self) -> &mut GainNode {
        &mut self.lfo.amplitude
    }

    /// Render the quantum at `q` from `input` (`None` when silent).
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>) -> &[f32; Q] {
        let delay_time = self.lfo.process(q);
        let wet = self.delay.process(q, input, Some(delay_time));
        match input {
            Some(dry) => {
                for ((o, &x), &w) in self.out.iter_mut().zip(dry).zip(wet) {
                    *o = x * self.dry_gain + w * self.wet_gain;
                }
            }
            None => {
                for (o, &w) in self.out.iter_mut().zip(wet) {
                    *o = w * self.wet_gain;
                }
            }
        }
        &self.out
    }
}
