//! `PitchFx`: a CrossFade whose dry input is always connected and whose wet input is Tone's
//! `PitchShift` (`effect/PitchShift.js` on `FeedbackEffect.js` and `Effect.js`, Tone 15.1.22), which
//! fx.ts builds on the first enable (`pitch`, `windowSize: 0.1`, `wet: 1`) and keeps from then on.
//!
//! What the PitchShift builds: two Delays of up to 1 s, each with a sawtooth LFO on its `delayTime`
//! (phase 0 and 180, sweeping `0..windowSize`, falling for a pitch up and rising for a pitch down),
//! an equal-power CrossFade between them driven by a triangle LFO (phase 90, `0..1`), then a Delay
//! at 0 s into the effect return. One `_frequency` Signal drives all three LFOs:
//! `(2^((pitch − 1) / 12) + 1) · 1.2 / windowSize` below 0, `(2^(pitch / 12) − 1) · 1.2 / windowSize`
//! from 0 up. Each delay line reads behind its sweeping read head, so the wet signal lags the input by
//! 0 to `windowSize` (up to 100 ms): that latency is what fx.ts ships, and it is reproduced as heard,
//! not compensated. The Effect wrapper mixes input and return in its own CrossFade at wet 1 (the
//! input still leaks in at Blink's cos(π/2) ≈ 6e−17, kept).
//!
//! Left out because they add exact zeros: FeedbackEffect's feedback Gain (gain 0, so the send is the
//! input plus 0) and the unity Gains around the effect (input, send, return).
//!
//! Tone builds the PitchShift at the enable; this port builds it with the chain (its delay lines and
//! wave tables allocate) and starts it at the enable, which renders the same: every event Tone
//! schedules while building sits at time 0, which Blink holds until the first quantum it renders, and
//! the node is connected, so it renders, from the enable's context frame ([`param::context_frame`]).
//! Its LFOs start at the enable's `now`. The custom LFO waves are built at the chain's rate, where
//! Tone reuses the first context's (`Oscillator._periodicWaveCache`; the app has one live context).

use std::sync::Arc;

use super::{Ctl, FxState, RAMP};
use crate::dsp::crossfade::CrossFade;
use crate::dsp::delay::DelayNode;
use crate::dsp::fdlibm;
use crate::dsp::oscillator::OscillatorType;
use crate::dsp::param::{self, Units};
use crate::dsp::signal::Signal;
use crate::dsp::synth::vibrato::Lfo;

/// fx.ts's `windowSize`, in seconds.
const WINDOW_SIZE: f64 = 0.1;

/// Tone's PitchShift, built but not started.
struct PitchShift {
    /// `_frequency`: every LFO's rate.
    frequency: Signal,
    lfo_a: Lfo,
    lfo_b: Lfo,
    fade_lfo: Lfo,
    delay_a: DelayNode,
    delay_b: DelayNode,
    cross_fade: CrossFade,
    /// `_feedbackDelay`: 0 s, so it hands on this quantum's own input.
    feedback_delay: DelayNode,
    /// The Effect's `_dryWet` at wet 1.
    dry_wet: CrossFade,
}

impl PitchShift {
    fn new(sample_rate: f32, frame: u64) -> Self {
        let wave = |kind, phase| Arc::new(Lfo::wave(kind, phase, sample_rate));
        // Each LFO's frequency Signal is overridden by `_frequency.fan(...)` (connectSignal), so its
        // default rate ("4n", 2 Hz at Tone's 120 bpm) never sounds.
        let lfo = |wave, min, max| {
            let mut lfo = Lfo::stopped(wave, 2.0, min, max, 1.0, sample_rate, frame);
            lfo.frequency.param.connect_signal(true, frame);
            lfo
        };
        // `new Delay({ maxDelay: 1 })` with an LFO connected to `delayTime` (connectSignal).
        let swept = || {
            let mut delay = DelayNode::new(sample_rate, 0.0, 1.0, frame);
            delay.delay_time.connect_signal(false, frame);
            delay
        };
        let mut cross_fade = CrossFade::new(sample_rate, 0.5, frame);
        cross_fade.fade.param.connect_signal(true, frame);
        // Effect: `new CrossFade()` (fade 0.5), then `wet.setValueAtTime(wet, 0)`.
        let mut dry_wet = CrossFade::new(sample_rate, 0.5, frame);
        dry_wet.fade.set_value_at_time(1.0, 0.0, frame);
        PitchShift {
            frequency: Signal::new(sample_rate, Units::Number, 0.0, frame),
            lfo_a: lfo(wave(OscillatorType::Sawtooth, 0.0), 0.0, WINDOW_SIZE),
            lfo_b: lfo(wave(OscillatorType::Sawtooth, 180.0), 0.0, WINDOW_SIZE),
            fade_lfo: lfo(wave(OscillatorType::Triangle, 90.0), 0.0, 1.0),
            delay_a: swept(),
            delay_b: swept(),
            cross_fade,
            feedback_delay: DelayNode::new(sample_rate, 0.0, 1.0, frame),
            dry_wet,
        }
    }

    /// The rest of Tone's constructor, at its `now`: start the three LFOs, then the `windowSize`
    /// setter, which sets the pitch.
    fn start(&mut self, pitch: f64, now: f64, frame: u64) {
        self.lfo_a.start(now, frame);
        self.lfo_b.start(now, frame);
        self.fade_lfo.start(now, frame);
        self.set_pitch(pitch, now, frame);
    }

    /// The `pitch` setter: the sweep's direction and the LFOs' rate.
    fn set_pitch(&mut self, interval: f64, now: f64, frame: u64) {
        let ratio = |interval: f64| fdlibm::pow(2.0, interval / 12.0);
        let (min, max, factor) =
            if interval < 0.0 { (0.0, WINDOW_SIZE, ratio(interval - 1.0) + 1.0) } else { (WINDOW_SIZE, 0.0, ratio(interval) - 1.0) };
        self.lfo_a.set_range(min, max, now, frame);
        self.lfo_b.set_range(min, max, now, frame);
        self.frequency.param.set_value(factor * (1.2 / WINDOW_SIZE), now, frame);
    }

    /// The control values of the quantum at `q`: the LFOs, the delay times and the fades.
    fn begin_quantum(&mut self, q: u64) {
        let PitchShift { frequency, lfo_a, lfo_b, fade_lfo, delay_a, delay_b, cross_fade, feedback_delay, dry_wet } = self;
        let f = frequency.process(q, None);
        delay_a.begin_quantum(q, Some(lfo_a.process_driven(q, Some(f))));
        delay_b.begin_quantum(q, Some(lfo_b.process_driven(q, Some(f))));
        cross_fade.begin_quantum_driven(q, Some(fade_lfo.process_driven(q, Some(f))));
        feedback_delay.begin_quantum(q, None);
        dry_wet.begin_quantum(q);
    }

    /// Frame `k` of the quantum begun: `x` in, the effect's output out.
    fn frame(&mut self, k: usize, x: f32) -> f32 {
        let a = self.delay_a.process_frame(k, x);
        let b = self.delay_b.process_frame(k, x);
        let faded = a * self.cross_fade.gain_a(k) + b * self.cross_fade.gain_b(k);
        let returned = self.feedback_delay.process_frame(k, faded);
        x * self.dry_wet.gain_a(k) + returned * self.dry_wet.gain_b(k)
    }
}

pub struct PitchFx {
    bypassed: bool,
    semitones: f64,
    xfade: CrossFade,
    shift: Box<PitchShift>,
    /// The context frame of the first enable, from which the PitchShift is connected.
    connected_at: Option<u64>,
    /// Whether the current quantum renders the PitchShift.
    running: bool,
}

impl PitchFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        PitchFx {
            bypassed: true,
            semitones: 0.0,
            xfade: CrossFade::new(sample_rate, 0.0, ctl.frame),
            shift: Box::new(PitchShift::new(sample_rate, ctl.frame)),
            connected_at: None,
            running: false,
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    /// The first enable builds the PitchShift (here: starts it); every call ramps the fade.
    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        if !bypassed && self.connected_at.is_none() {
            self.shift.start(self.semitones, ctl.now, ctl.frame);
            self.connected_at = Some(param::context_frame(ctl.frame));
        }
        self.bypassed = bypassed;
        self.xfade.fade.ramp_to(if bypassed { 0.0 } else { 1.0 }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self) -> f64 {
        self.semitones
    }

    /// Stored always; it reaches the PitchShift once that is built.
    pub fn set_param(&mut self, value: f64, ctl: Ctl) {
        self.semitones = value;
        if self.connected_at.is_some() {
            self.shift.set_pitch(value, ctl.now, ctl.frame);
        }
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.semitones, 0.0, 0.0] }
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.xfade.begin_quantum(quantum_start);
        self.running = self.connected_at.is_some_and(|at| quantum_start >= at);
        if self.running {
            self.shift.begin_quantum(quantum_start);
        }
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) {
        if !self.running {
            // Nothing connected to `b` yet: its GainNode is silent.
            self.xfade.process(at, Some(input), None, out);
            return;
        }
        for (i, (&x, o)) in input.iter().zip(out.iter_mut()).enumerate() {
            let k = at + i;
            let wet = self.shift.frame(k, x);
            *o = x * self.xfade.gain_a(k) + wet * self.xfade.gain_b(k);
        }
    }
}
