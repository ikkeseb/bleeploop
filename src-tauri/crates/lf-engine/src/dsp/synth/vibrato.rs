//! Tone.js 15.1.22's `source/oscillator/LFO.js` ([`Lfo`]) and `effect/Vibrato.js` ([`Vibrato`]).
//!
//! The LFO is a Tone Oscillator (a custom wave when its phase is not 0), times `amplitude`, mapped from
//! -1..1 to 0..1 (`AudioToGain`), then scaled to `min..max` (`Scale`: a Multiply by `max - min`, an Add
//! of `min`). Its stopped-value signal and zeros only add +0 once it is started, so they are left out:
//! before its start an LFO outputs `min + (max - min) / 2` here, where Tone adds the wave's value at
//! its phase (only an LFO connected before its start renders that: Tone's PitchShift, when an offline
//! replay schedules its enable ahead of the render). Tone builds a phase-shifted wave from
//! `Math.sin`/`Math.cos` (V8's fdlibm) of the phase times each partial's number
//! (`Oscillator._getRealImaginary`: `real[n] = -b·sin(phase·n)`, `imag[n] = b·cos(phase·n)`).
//!
//! The Vibrato delays its input by the LFO: a Delay of `maxDelay` whose `delayTime` is the LFO scaled
//! to 0..maxDelay, so at depth 0 it rests at half the maximum. Its Effect wrapper is a CrossFade at
//! wet 1: a StereoPanner panned hard right splits a constant 1 into the dry gain (cos(pi/2), Blink's
//! 6.1e-17, kept) and the wet gain (sin(pi/2) = 1).

use std::sync::Arc;

use crate::dsp::crossfade::pan_gains;
use crate::dsp::delay::DelayNode;
use crate::dsp::fdlibm;
use crate::dsp::gain::GainNode;
use crate::dsp::oscillator::{OscillatorType, PeriodicWave, ToneOscillator};
use crate::dsp::param::{Units, QUANTUM};
use crate::dsp::signal::{Scale, Signal, WaveShaper};

const Q: usize = QUANTUM;

/// Tone's LFO with a sine wave, started when built.
pub struct Lfo {
    pub frequency: Signal,
    detune: Signal,
    oscillator: ToneOscillator,
    /// Tone's `amplitude` (normalRange).
    pub amplitude: GainNode,
    a2g: WaveShaper,
    scale: Scale,
    osc_out: [f32; Q],
    amplified: [[f32; Q]; 1],
    mapped: [f32; Q],
}

impl Lfo {
    /// The custom wave of a sine LFO at `phase` degrees, not 0 ([`Lfo::wave`]). Tone caches custom
    /// waves module-wide by type and phase (`Oscillator._periodicWaveCache`), so every later context
    /// reuses the wave the first one built, tables, rate scale and all: `sample_rate` is that first
    /// context's rate. An oscillator in a context at another rate then runs at `frequency * its rate /
    /// sample_rate`.
    pub fn sine_wave(phase_degrees: f64, sample_rate: f32) -> PeriodicWave {
        Self::wave(OscillatorType::Sine, phase_degrees, sample_rate)
    }

    /// The wave Tone's Oscillator plays for a basic `kind` at `phase` degrees (its `type` setter): the
    /// native wave at phase 0, else a custom wave of 2048 coefficients from `_getRealImaginary`.
    pub fn wave(kind: OscillatorType, phase_degrees: f64, sample_rate: f32) -> PeriodicWave {
        let phase = (phase_degrees * std::f64::consts::PI) / 180.0;
        if phase == 0.0 {
            return PeriodicWave::basic(kind, sample_rate);
        }
        const SIZE: usize = 2048;
        let mut real = vec![0.0f32; SIZE];
        let mut imag = vec![0.0f32; SIZE];
        for n in 1..SIZE {
            let pi_factor = 2.0 / (n as f64 * std::f64::consts::PI);
            let odd = n & 1 == 1;
            let b = match kind {
                OscillatorType::Sine => f64::from(u8::from(n == 1)),
                OscillatorType::Square => 2.0 * pi_factor * f64::from(u8::from(odd)),
                OscillatorType::Sawtooth => pi_factor * if odd { 1.0 } else { -1.0 },
                OscillatorType::Triangle if odd => 2.0 * (pi_factor * pi_factor) * if ((n - 1) >> 1) & 1 == 1 { -1.0 } else { 1.0 },
                OscillatorType::Triangle => 0.0,
            };
            if b != 0.0 {
                real[n] = (-b * fdlibm::sin(phase * n as f64)) as f32;
                imag[n] = (b * fdlibm::cos(phase * n as f64)) as f32;
            }
        }
        PeriodicWave::custom(&real, &imag, sample_rate)
    }

    /// `new LFO({ frequency, min, max, amplitude })` with `wave`, then `start(now)`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(wave: Arc<PeriodicWave>, frequency: f64, min: f64, max: f64, amplitude: f64, now: f64, sample_rate: f32, frame: u64) -> Self {
        let mut lfo = Self::stopped(wave, frequency, min, max, amplitude, sample_rate, frame);
        lfo.start(now, frame);
        lfo
    }

    /// `new LFO({ frequency, min, max, amplitude })` with `wave`, not started yet.
    pub fn stopped(wave: Arc<PeriodicWave>, frequency: f64, min: f64, max: f64, amplitude: f64, sample_rate: f32, frame: u64) -> Self {
        let rate = sample_rate as f64;
        Lfo {
            frequency: Signal::new(sample_rate, Units::Frequency, frequency, frame),
            detune: Signal::new(sample_rate, Units::Cents, 0.0, frame),
            oscillator: ToneOscillator::new(wave, sample_rate, frame),
            amplitude: GainNode::new(rate, amplitude, Units::NormalRange, frame),
            a2g: WaveShaper::audio_to_gain(),
            scale: Scale::new(sample_rate, min, max, frame),
            osc_out: [0.0; Q],
            amplified: [[0.0; Q]],
            mapped: [0.0; Q],
        }
    }

    /// `start(time)`.
    pub fn start(&mut self, time: f64, frame: u64) {
        self.oscillator.start(time, frame);
    }

    /// The `min` and `max` setters at Tone's `now` (Scale's `_setRange`: the Add's value, then the
    /// Multiply's). The LFO converts them to its destination's units first, a no-op for the numbers in
    /// seconds, hertz or normal range that reach it.
    pub fn set_range(&mut self, min: f64, max: f64, now: f64, frame: u64) {
        self.scale.set_range(min, max, now, frame);
    }

    /// Render the quantum at `q`.
    pub fn process(&mut self, q: u64) -> &[f32; Q] {
        self.process_driven(q, None)
    }

    /// Render the quantum at `q` with `frequency_input` summed into the frequency signal: a signal
    /// connected to it (connect with `frequency.param.connect_signal(true, ..)`, which zeroes the
    /// signal's own value).
    pub fn process_driven(&mut self, q: u64, frequency_input: Option<&[f32; Q]>) -> &[f32; Q] {
        let f = self.frequency.process(q, frequency_input);
        let d = self.detune.process(q, None);
        let silent = self.oscillator.process(q, f, d, &mut self.osc_out);
        self.amplitude.process(q, (!silent).then_some(std::slice::from_ref(&self.osc_out)), &mut self.amplified);
        // The shaper's input is never silent (the stopped signal feeds it too): zeros map as well.
        self.a2g.process(&self.amplified[0], &mut self.mapped);
        self.scale.process(q, Some(&self.mapped))
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
        let (dry_gain, wet_gain) = pan_gains(1.0);
        Vibrato { lfo, delay, dry_gain, wet_gain, out: [0.0; Q] }
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
