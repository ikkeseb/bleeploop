//! `DelayFx`: Tone's `FeedbackDelay` (`effect/FeedbackDelay.js` on `FeedbackEffect.js` and
//! `Effect.js`) with its wet level as the bypass, and the Blink DelayNode it builds
//! ([`BlinkDelay`]: `platform/audio/delay.cc` with the SSE2 a-rate loop of
//! `platform/audio/cpu/x86/delay_sse2.cc`, driven as `modules/webaudio/delay_handler.cc` does, at
//! Chromium 153.0.8010.12).
//!
//! What Tone builds: `input` → the dry/wet CrossFade's `a`, and `input` → `effectSend` → DelayNode →
//! `effectReturn` → the CrossFade's `b`, with `effectReturn` → the feedback Gain → `effectSend`. Blink
//! renders the cycle by pulling `effectReturn` first: when the pull comes back around to it through
//! the feedback Gain, it hands out its bus as the previous quantum left it. So the feedback reaches
//! the delay one quantum late: `send[n] = input[n] + feedback · return[n − 128]`.
//!
//! Bypassed means wet 0, and a CrossFade at 0 still mixes `b` in at −56 dB (see the crossfade
//! module): the echoes of a bypassed delay are in every chain's output, so this node renders them.
//! The delay-* reference scenarios that judge the effect itself belong to a later wave.

use super::{clamp_index, division_beats, Ctl, FxParam, FxState, FxTiming, DIVISIONS, MAX_FEEDBACK, RAMP};
use crate::dsp::crossfade::CrossFade;
use crate::dsp::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

/// Blink's `Delay` kernel on one channel: a circular buffer one quantum longer than the longest delay.
pub struct BlinkDelay {
    buffer: Vec<f32>,
    write_index: usize,
    max_delay_time: f64,
    sample_rate: f32,
}

impl BlinkDelay {
    pub fn new(max_delay_time: f64, sample_rate: f32) -> Self {
        // `BufferLengthForDelay`: a quantum plus the longest delay in frames, rounded up.
        let frames = (max_delay_time * sample_rate as f64 * 1024.0).round() / 1024.0;
        let length = QUANTUM + frames.ceil() as usize;
        BlinkDelay { buffer: vec![0.0; length], write_index: 0, max_delay_time, sample_rate }
    }

    /// One frame of `ProcessARate`: write `input` at the quantum's frame `k`, read `delay_time`
    /// seconds back. Blink copies the whole quantum in before reading; a read never reaches past the
    /// frame being written unless the delay is under a frame, where the later sample weighs 0.
    fn process_frame(&mut self, k: usize, input: f32, delay_time: f32) -> f32 {
        let length = self.buffer.len();
        let mut w = self.write_index + k;
        if w >= length {
            w -= length;
        }
        self.buffer[w] = input;
        // The SSE2 lanes, in float.
        let delay_time = if delay_time.is_nan() { self.max_delay_time as f32 } else { delay_time.max(0.0) };
        let desired = delay_time * self.sample_rate;
        let length_f = length as f32;
        let mut position = w as f32 + (length_f - desired);
        if position >= length_f {
            position -= length_f;
        }
        let mut index1 = position as usize;
        if index1 >= length {
            index1 -= length;
        }
        let mut index2 = index1 + 1;
        if index2 >= length {
            index2 -= length;
        }
        let f = position - index1 as f32;
        let (s1, s2) = (self.buffer[index1], self.buffer[index2]);
        s1 + f * (s2 - s1)
    }

    /// Advance the write index past a rendered quantum.
    fn end_quantum(&mut self) {
        self.write_index += QUANTUM;
        if self.write_index >= self.buffer.len() {
            self.write_index -= self.buffer.len();
        }
    }
}

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
    delay_time: ToneParam,
    delay: BlinkDelay,
    delay_times: [f32; QUANTUM],
    feedback_values: [f32; QUANTUM],
    /// `effectReturn`'s output: this quantum's, and the previous one the feedback reads.
    returned: [f32; QUANTUM],
    previous: [f32; QUANTUM],
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
        // FeedbackDelay: `new Delay({ delayTime, maxDelay })`, its delayTime a time Param over the
        // DelayNode's [0, maxDelay] param.
        let max = f64::max(max_delay, delay_seconds);
        let native = AudioParam::new(rate, 0.0, 0.0, max_delay as f32, Rate::A);
        let delay_time = ToneParam::new(native, Units::Time, Some(delay_seconds), frame).with_range(Some(0.0), Some(max));
        DelayFx {
            bypassed: true,
            time,
            feedback,
            mix: 0.3,
            beat_period,
            timing_set: false,
            dry_wet,
            feedback_gain,
            delay_time,
            delay: BlinkDelay::new(max_delay, sample_rate),
            delay_times: [0.0; QUANTUM],
            feedback_values: [0.0; QUANTUM],
            returned: [0.0; QUANTUM],
            previous: [0.0; QUANTUM],
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
                self.delay_time.ramp_to(self.beat_period * division_beats(self.time), RAMP, ctl.now, ctl.frame);
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
        self.delay_time.cancel_scheduled_values(now, ctl.frame);
        self.delay_time.set_value_at_time(self.beat_period * division_beats(self.time), now, ctl.frame);
    }

    pub(super) fn begin_quantum(&mut self, quantum_start: u64) {
        self.dry_wet.begin_quantum(quantum_start);
        self.delay_time.native.calculate_sample_accurate_values(quantum_start, &mut self.delay_times, None);
        // The feedback GainNode: a-rate values while automated, else its one value.
        let gain = &mut self.feedback_gain.native;
        if gain.has_sample_accurate_values(quantum_start) {
            gain.calculate_sample_accurate_values(quantum_start, &mut self.feedback_values, None);
        } else {
            self.feedback_values.fill(gain.value(quantum_start));
        }
        self.previous = self.returned;
    }

    pub(super) fn process(&mut self, at: usize, input: &[f32], out: &mut [f32]) {
        for (i, &x) in input.iter().enumerate() {
            let k = at + i;
            let send = x + self.previous[k] * self.feedback_values[k];
            let y = self.delay.process_frame(k, send, self.delay_times[k]);
            self.returned[k] = y;
            out[i] = x * self.dry_wet.gain_a(k) + y * self.dry_wet.gain_b(k);
        }
        if at + input.len() == QUANTUM {
            self.delay.end_quantum();
        }
    }
}
