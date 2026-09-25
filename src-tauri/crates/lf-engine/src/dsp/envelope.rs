//! ADSR envelopes: Tone.js 15.1.22's `component/envelope/Envelope.js` ([`Envelope`]),
//! `AmplitudeEnvelope.js` ([`AmplitudeEnvelope`]) and `FrequencyEnvelope.js` ([`FrequencyEnvelope`]).
//!
//! An envelope is a Tone Signal (a constant source) whose offset Tone schedules: the attack from the
//! envelope's current value (a partial attack takes the time left at the full attack's rate), up to
//! the velocity, then the decay toward `velocity * sustain` from the end of the attack; the release
//! from the value at its time down to 0. Linear curves are linear ramps; exponential ones are Tone's
//! `targetRampTo` and `exponentialApproachValueAtTime` (a setTarget finished by a linear ramp over
//! its last 10 %). A retrigger while sounding starts from where the envelope is, which Tone reads from
//! its own model of the curve. An amplitude envelope connects that signal into a gain's param. A
//! frequency envelope maps it through a `Pow` (a WaveShaper of `|x|^exponent`, 8192 points) and a
//! `Scale` from `baseFrequency` to `baseFrequency * 2^octaves`.
//! Deliberately not ported: array curves and the named shapes built from them (cosine, bounce,
//! ripple, sine, step); no production synth sets one. Tone's `asArray` (an offline render) neither.
//!
//! Scheduling and rendering never allocate. Every call takes the frame being rendered, for Blink's
//! context time (`dsp::param`).

use super::fdlibm;
use super::gain::ParamGain;
use super::param::{Units, QUANTUM};
use super::signal::{Scale, Signal, WaveShaper};

const Q: usize = QUANTUM;

/// An envelope segment's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    Linear,
    Exponential,
}

/// Envelope settings (Tone's option names; times in seconds, sustain 0..1).
#[derive(Clone, Copy, Debug)]
pub struct Adsr {
    pub attack: f64,
    pub decay: f64,
    pub sustain: f64,
    pub release: f64,
    pub attack_curve: Curve,
    pub decay_curve: Curve,
    pub release_curve: Curve,
}

impl Adsr {
    /// Tone's Envelope defaults: linear attack, exponential decay and release.
    pub const fn new(attack: f64, decay: f64, sustain: f64, release: f64) -> Self {
        Adsr { attack, decay, sustain, release, attack_curve: Curve::Linear, decay_curve: Curve::Exponential, release_curve: Curve::Exponential }
    }
}

/// Tone's `Envelope`: the scheduled signal.
pub struct Envelope {
    pub adsr: Adsr,
    pub signal: Signal,
    sample_time: f64,
}

impl Envelope {
    pub fn new(adsr: Adsr, sample_rate: f32, frame: u64) -> Self {
        debug_assert!((0.0..=1.0).contains(&adsr.sustain), "sustain in 0..1");
        Envelope { adsr, signal: Signal::new(sample_rate, Units::Number, 0.0, frame), sample_time: 1.0 / sample_rate as f64 }
    }

    pub fn reset(&mut self, frame: u64) {
        self.signal.reset(0.0, frame);
    }

    /// Tone's model of the envelope's value at `time`.
    pub fn value_at_time(&self, time: f64) -> f64 {
        self.signal.param.get_value_at_time(time)
    }

    /// `triggerAttack(time, velocity)`.
    pub fn trigger_attack(&mut self, time: f64, velocity: f64, frame: u64) {
        let Adsr { attack: original_attack, decay, sustain, .. } = self.adsr;
        let mut attack = original_attack;
        let current_value = self.value_at_time(time);
        if current_value > 0.0 {
            // A partial attack: the time left at the full attack's rate.
            let attack_rate = 1.0 / attack;
            let remaining_distance = 1.0 - current_value;
            attack = remaining_distance / attack_rate;
        }
        let sig = &mut self.signal.param;
        if attack < self.sample_time {
            sig.cancel_scheduled_values(time, frame);
            sig.set_value_at_time(velocity, time, frame);
        } else if self.adsr.attack_curve == Curve::Linear {
            sig.linear_ramp_to(velocity, attack, time, frame);
        } else {
            sig.target_ramp_to(velocity, attack, time, frame);
        }
        if decay != 0.0 && sustain < 1.0 {
            let decay_value = velocity * sustain;
            let decay_start = time + attack;
            if self.adsr.decay_curve == Curve::Linear {
                sig.linear_ramp_to_value_at_time(decay_value, decay + decay_start, frame);
            } else {
                sig.exponential_approach_value_at_time(decay_value, decay_start, decay, frame);
            }
        }
    }

    /// `triggerRelease(time)`: nothing when the envelope is already at 0 then.
    pub fn trigger_release(&mut self, time: f64, frame: u64) {
        let current_value = self.value_at_time(time);
        if current_value > 0.0 {
            let release = self.adsr.release;
            let sig = &mut self.signal.param;
            if release < self.sample_time {
                sig.set_value_at_time(0.0, time, frame);
            } else if self.adsr.release_curve == Curve::Linear {
                sig.linear_ramp_to(0.0, release, time, frame);
            } else {
                sig.target_ramp_to(0.0, release, time, frame);
            }
        }
    }

    /// `cancel(after)`.
    pub fn cancel(&mut self, after: f64, frame: u64) {
        self.signal.param.cancel_scheduled_values(after, frame);
    }

    /// Render the envelope's quantum at `q`.
    pub fn process(&mut self, q: u64) -> &[f32; Q] {
        self.signal.process(q, None)
    }
}

/// Tone's `AmplitudeEnvelope`: the envelope drives a gain (0 plus the envelope).
pub struct AmplitudeEnvelope {
    pub envelope: Envelope,
    gain: ParamGain,
}

impl AmplitudeEnvelope {
    pub fn new(adsr: Adsr, sample_rate: f32, frame: u64) -> Self {
        AmplitudeEnvelope { envelope: Envelope::new(adsr, sample_rate, frame), gain: ParamGain::new(sample_rate, 0.0, true, frame) }
    }

    pub fn reset(&mut self, frame: u64) {
        self.envelope.reset(frame);
        self.gain.reset(0.0, true, frame);
    }

    /// Render the quantum at `q`: `input` (`None` when silent) times the envelope. The envelope runs
    /// every quantum.
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>, out: &mut [f32; Q]) -> bool {
        let env = self.envelope.process(q);
        self.gain.process(q, input, Some(env), out)
    }

    /// [`AmplitudeEnvelope::process`] on several channels.
    pub fn process_channels(&mut self, q: u64, input: Option<&[[f32; Q]]>, out: &mut [[f32; Q]]) -> bool {
        let env = self.envelope.process(q);
        self.gain.process_channels(q, input, Some(env), out)
    }
}

/// Tone's `FrequencyEnvelope`: the envelope, raised to `exponent`, scaled to hertz.
pub struct FrequencyEnvelope {
    pub envelope: Envelope,
    pow: WaveShaper,
    scale: Scale,
    shaped: [f32; Q],
}

impl FrequencyEnvelope {
    /// `new FrequencyEnvelope({ ...adsr, baseFrequency, octaves, exponent })`.
    pub fn new(adsr: Adsr, base_frequency: f64, octaves: f64, exponent: f64, sample_rate: f32, frame: u64) -> Self {
        // `toFrequency(hertz)` is `1 / (1 / hertz)` (Tone's FrequencyClass round trip).
        let base = 1.0 / (1.0 / base_frequency);
        FrequencyEnvelope {
            envelope: Envelope::new(adsr, sample_rate, frame),
            pow: WaveShaper::new(8192, |x| fdlibm::pow(x.abs(), exponent)),
            scale: Scale::new(sample_rate, base, base * fdlibm::pow(2.0, octaves), frame),
            shaped: [0.0; Q],
        }
    }

    /// Render the quantum at `q`: the frequency in hertz.
    pub fn process(&mut self, q: u64) -> &[f32; Q] {
        let env = self.envelope.process(q);
        self.pow.process(env, &mut self.shaped);
        self.scale.process(q, Some(&self.shaped))
    }
}
