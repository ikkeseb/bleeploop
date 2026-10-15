//! `DelayFx`: Tone's `FeedbackDelay` (`effect/FeedbackDelay.js` on `FeedbackEffect.js` and
//! `Effect.js`) with its wet level as the bypass, over the Blink DelayNode it builds
//! ([`DelayNode`](crate::dsp::delay::DelayNode)).
//!
//! What Tone builds: `input` → the dry/wet CrossFade's `a`, and `input` → `effectSend` → DelayNode →
//! `effectReturn` → the CrossFade's `b`, with `effectReturn` → the feedback Gain → `effectSend`. Blink
//! renders that cycle a quantum late (it hands the feedback Gain `effectReturn` as the previous quantum
//! left it), so each repeat after the first lands another 128 frames behind the grid. This node does
//! not: each frame reads the line first ([`DelayNode::read_frame`]) and writes `input + feedback ·
//! return` on the same frame, `send[n] = input[n] + feedback · return[n]`, as the input ECHO does
//! (`input_fx`). Repeat n of an impulse lands exactly n delay times after it and is `feedbackⁿ⁻¹`
//! times the first (the tests below). Every delay time is at least a 1/16 at the fastest tempo, far
//! over the one frame a read before the write needs.
//!
//! Bypassed means wet 0, and a CrossFade at 0 still mixes `b` in at −56 dB (see the crossfade
//! module): the echoes of a bypassed delay are in every chain's output, so this node renders them.
//!
//! The engine's FADE (no Tone counterpart) hands a fading lane's gain to [`DelayFx::process`]: the
//! feedback takes it too, so the echoes the delay held before the fade fall with the lane (at feedback
//! 0.95 they would otherwise ring on at their old level long after it stopped), and what the delay
//! still holds at the lane's stop is the echo of a loop already faded out.

use super::{clamp_index, division_beats, Ctl, FxParam, FxState, FxTiming, DIVISIONS, MAX_FEEDBACK, RAMP};
use crate::dsp::crossfade::CrossFade;
use crate::dsp::delay::DelayNode;
use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

pub struct DelayFx {
    bypassed: bool,
    /// Index into [`DIVISIONS`].
    time: usize,
    feedback: f64,
    mix: f64,
    beat_period: f64,
    timing_set: bool,
    dry_wet: CrossFade,
    /// The feedback Gain's gain.
    feedback_gain: ToneParam,
    delay: DelayNode,
    feedback_values: [f32; QUANTUM],
    sample_rate: f32,
}

impl DelayFx {
    pub fn new(sample_rate: f32, ctl: Ctl) -> Self {
        let rate = sample_rate as f64;
        let frame = ctl.frame;
        let (time, feedback, beat_period, max_delay) = (1, 0.4, 0.5, 2.0);
        let delay_seconds = beat_period * division_beats(time);
        // Effect: the dry/wet CrossFade (Tone's default fade 0.5), then `wet.setValueAtTime(wet, 0)`.
        let mut dry_wet = CrossFade::new(sample_rate, 0.5, frame);
        dry_wet.fade.set_value_at_time(0.0, 0.0, frame);
        // FeedbackEffect: the feedback Gain (normalRange).
        let gain = AudioParam::new(rate, 1.0, f32::MIN, f32::MAX, Rate::A);
        let feedback_gain = ToneParam::new(gain, Units::NormalRange, Some(feedback), frame);
        // FeedbackDelay: `new Delay({ delayTime, maxDelay })`.
        let delay = DelayNode::new(sample_rate, delay_seconds, max_delay, frame);
        DelayFx {
            bypassed: true,
            time,
            feedback,
            mix: 0.3,
            beat_period,
            timing_set: false,
            dry_wet,
            feedback_gain,
            delay,
            feedback_values: [0.0; QUANTUM],
            sample_rate,
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed
    }

    pub fn set_bypass(&mut self, bypassed: bool, ctl: Ctl) {
        self.bypassed = bypassed;
        self.dry_wet.fade.ramp_to(if bypassed { 0.0 } else { self.mix }, RAMP, ctl.now, ctl.frame);
    }

    pub fn get_param(&self, param: FxParam) -> f64 {
        match param {
            FxParam::Time => self.time as f64,
            FxParam::Feedback => self.feedback,
            _ => self.mix,
        }
    }

    pub fn set_param(&mut self, param: FxParam, value: f64, ctl: Ctl) {
        match param {
            FxParam::Time => {
                self.time = clamp_index(value, DIVISIONS.len());
                self.delay.delay_time.ramp_to(self.beat_period * division_beats(self.time), RAMP, ctl.now, ctl.frame);
            }
            FxParam::Feedback => {
                self.feedback = value.clamp(0.0, MAX_FEEDBACK);
                self.feedback_gain.ramp_to(self.feedback, RAMP, ctl.now, ctl.frame);
            }
            _ => {
                self.mix = value.clamp(0.0, 1.0);
                if !self.bypassed {
                    self.dry_wet.fade.ramp_to(self.mix, RAMP, ctl.now, ctl.frame);
                }
            }
        }
    }

    pub fn get_state(&self) -> FxState {
        FxState { bypassed: self.bypassed, params: [self.time as f64, self.feedback, self.mix] }
    }

    /// A new grid's beat replaces the delay time at once, from the context time (Tone's `immediate()`).
    pub fn set_timing(&mut self, timing: FxTiming, ctl: Ctl) {
        if self.timing_set && self.beat_period == timing.beat_period {
            return;
        }
        self.timing_set = true;
        self.beat_period = timing.beat_period;
        let now = ctl.context_time(self.sample_rate);
        self.delay.delay_time.cancel_scheduled_values(now, ctl.frame);
        self.delay.delay_time.set_value_at_time(self.beat_period * division_beats(self.time), now, ctl.frame);
    }

    /// [`super::FxChain::clear_history`] from frame `k` of the current quantum: the delay line, which
    /// the feedback reads too.
    pub(super) fn clear_history(&mut self, k: usize) {
        self.delay.clear(k);
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.dry_wet.begin_quantum(quantum_start);
        self.delay.begin_quantum(quantum_start, None);
        // The feedback GainNode: a-rate values while automated, else its one value.
        let gain = &mut self.feedback_gain.native;
        if gain.has_sample_accurate_values(quantum_start) {
            gain.calculate_sample_accurate_values(quantum_start, &mut self.feedback_values, None);
        } else {
            self.feedback_values.fill(gain.value(quantum_start));
        }
    }

    /// `fade`: a fading lane's gain on these frames, which the feedback takes as well.
    pub(super) fn process(&mut self, at: usize, input: &[f32], fade: Option<&[f32]>, out: &mut [f32]) {
        for (i, &x) in input.iter().enumerate() {
            let k = at + i;
            let y = self.delay.read_frame(k);
            let mut fed = y * self.feedback_values[k];
            if let Some(fade) = fade {
                fed *= fade[i];
            }
            self.delay.write_frame(k, x + fed);
            out[i] = x * self.dry_wet.gain_a(k) + y * self.dry_wet.gain_b(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;
    const ECHOES: usize = 8;

    /// A delay on at full mix: its output for an impulse at `pulse` (mid-quantum, after the 20 ms ramps
    /// to the new values), quantum by quantum, through its `ECHOES`th echo of `delay` frames.
    fn impulse_response(time: usize, beat_period: f64, feedback: f64, pulse: usize, delay: usize) -> Vec<f32> {
        let start = Ctl { now: 0.0, frame: 0 };
        let mut fx = DelayFx::new(RATE, start);
        fx.set_param(FxParam::Time, time as f64, start);
        fx.set_param(FxParam::Feedback, feedback, start);
        fx.set_param(FxParam::Mix, 1.0, start);
        fx.set_bypass(false, start);
        fx.set_timing(FxTiming { anchor: 0.0, beat_period }, start);
        let frames = (pulse + ECHOES * delay + 1).next_multiple_of(QUANTUM);
        let mut out = vec![0.0f32; frames];
        let mut x = [0.0f32; QUANTUM];
        for (q, o) in out.chunks_exact_mut(QUANTUM).enumerate() {
            let at = q * QUANTUM;
            x.fill(0.0);
            if (at..at + QUANTUM).contains(&pulse) {
                x[pulse - at] = 1.0;
            }
            fx.begin_quantum(at as u64);
            fx.process(0, &x, None, o);
        }
        out
    }

    #[test]
    fn echo_n_lands_exactly_n_delay_times_after_the_note_and_decays_by_the_feedback() {
        // (division, beat period, feedback, the division's frames at 48 kHz): times exact in binary, so
        // each echo is one frame, none of them a whole number of quanta.
        let cases = [(1, 0.5, 0.4, 12_000), (3, 0.375, MAX_FEEDBACK, 4_500), (2, 0.75, 0.7, 27_000), (0, 0.3125, 0.5, 15_000)];
        let pulse = 5_037;
        for (time, beat_period, feedback, delay) in cases {
            assert_eq!(beat_period * division_beats(time) * RATE as f64, delay as f64);
            let out = impulse_response(time, beat_period, feedback, pulse, delay);
            let first = out[pulse + delay];
            assert!(first > 0.9, "{delay}: the first echo at full mix ({first})");
            for (k, &y) in out.iter().enumerate() {
                if k == pulse {
                    continue; // the dry path's leak at full mix
                }
                let after = k.wrapping_sub(pulse);
                if k < pulse || after % delay != 0 {
                    assert!(y == 0.0, "{delay}: sound off the grid at frame {k} ({y:e})");
                    continue;
                }
                let n = after / delay;
                let expected = first as f64 * feedback.powi(n as i32 - 1);
                assert!((y as f64 - expected).abs() < 1e-5 * expected, "{delay}: echo {n} is {y}, not {expected}");
            }
        }
    }
}
