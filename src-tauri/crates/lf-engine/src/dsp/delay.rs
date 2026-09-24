//! A delay line: Blink's DelayNode ([`DelayNode`]) over its per-channel kernel ([`Delay`]), with Tone's
//! `Delay` (`core/context/Delay.js`: a Tone [`ToneParam`] on `delayTime` in seconds, its range 0 to the
//! maximum delay).
//!
//! Ports `platform/audio/delay.{h,cc}` with its x86 kernel `platform/audio/cpu/x86/delay_sse2.cc` and
//! `modules/webaudio/delay_handler.cc` at Chromium 153.0.8010.12. `delayTime` is a-rate (Blink's
//! default, and what Tone leaves it at), so every quantum takes the sample-accurate path: the quantum
//! is written into a circular buffer of `128 + ceil(maxDelay * rate)` frames first, then each frame
//! reads `delayTime * rate` frames behind its write position (in float, as the SSE kernel does),
//! linearly interpolated. A negative delay reads as 0, NaN as the maximum; a delay under one quantum
//! reads this quantum's own input. Not ported: the k-rate path (`automationRate = "k-rate"`), which no
//! Tone class sets, and what Blink does with a DelayNode inside a cycle (not read yet).
//!
//! Blink stops processing a delay once its input has been silent for longer than the maximum delay
//! (the node's tail); this one keeps writing zeros instead, which reads the same, since everything
//! within reach of the read head is zero by then. Mono, one quantum per call; allocation happens
//! only in [`Delay::new`].
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::oscillator::time_to_sample_frame_up;
use super::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

const Q: usize = QUANTUM;

/// Blink's `Delay` kernel: one channel's circular buffer.
pub struct Delay {
    buffer: Vec<f32>,
    write_index: usize,
    max_delay_time: f32,
    sample_rate: f32,
}

impl Delay {
    pub fn new(max_delay_time: f64, sample_rate: f32) -> Self {
        assert!(max_delay_time > 0.0 && max_delay_time.is_finite(), "a positive maximum delay");
        let length = Q + time_to_sample_frame_up(max_delay_time, sample_rate as f64) as usize;
        Delay { buffer: vec![0.0; length], write_index: 0, max_delay_time: max_delay_time as f32, sample_rate }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_index = 0;
    }

    pub fn max_delay_time(&self) -> f32 {
        self.max_delay_time
    }

    /// `ProcessARate`: `delay_times` in seconds per frame (NaN becomes the maximum in place).
    pub fn process_a_rate(&mut self, source: &[f32; Q], delay_times: &mut [f32; Q], destination: &mut [f32; Q]) {
        let length = self.buffer.len();
        for t in delay_times.iter_mut() {
            if t.is_nan() {
                *t = self.max_delay_time;
            }
        }
        // CopyToCircularBuffer.
        let first = Q.min(length - self.write_index);
        self.buffer[self.write_index..self.write_index + first].copy_from_slice(&source[..first]);
        self.buffer[..Q - first].copy_from_slice(&source[first..]);

        // ProcessARateVector: four frames at a time, float read positions.
        let length_f = length as f32;
        let wrap_index = |i: usize| if i >= length { i - length } else { i };
        let mut write = [0, 1, 2, 3].map(|m| wrap_index(self.write_index + m));
        for k in (0..Q).step_by(4) {
            for m in 0..4 {
                let delay_time = if delay_times[k + m] > 0.0 { delay_times[k + m] } else { 0.0 };
                let desired_delay_frames = delay_time * self.sample_rate;
                let mut read_position = write[m] as f32 + (length_f - desired_delay_frames);
                if read_position >= length_f {
                    read_position -= length_f;
                }
                let read_index1 = wrap_index(read_position as i32 as usize);
                let read_index2 = wrap_index(read_index1 + 1);
                let interpolation_factor = read_position - read_index1 as f32;
                let (sample1, sample2) = (self.buffer[read_index1], self.buffer[read_index2]);
                destination[k + m] = sample1 + interpolation_factor * (sample2 - sample1);
                write[m] = wrap_index(write[m] + 4);
            }
        }
        self.write_index = wrap_index(self.write_index + Q);
    }
}

/// Blink's DelayNode (mono) with Tone's `delayTime` param.
pub struct DelayNode {
    pub delay: Delay,
    /// Seconds, a-rate, 0 to the maximum delay.
    pub delay_time: ToneParam,
    times: [f32; Q],
    out: [f32; Q],
}

impl DelayNode {
    /// Tone's `new Delay({ delayTime, maxDelay })`.
    pub fn new(sample_rate: f32, delay_time: f64, max_delay: f64, frame: u64) -> Self {
        // Tone's range check takes the larger of the two; the native node is built with maxDelay.
        let native = AudioParam::new(sample_rate as f64, 0.0, 0.0, max_delay as f32, Rate::A);
        let param = ToneParam::new(native, Units::Time, Some(delay_time), frame).with_range(Some(0.0), Some(max_delay.max(delay_time)));
        DelayNode { delay: Delay::new(max_delay, sample_rate), delay_time: param, times: [0.0; Q], out: [0.0; Q] }
    }

    /// Render the quantum at `q`: `input` (`None` when silent) delayed by the param plus the signal
    /// connected to it, if any.
    pub fn process(&mut self, q: u64, input: Option<&[f32; Q]>, delay_time_input: Option<&[f32; Q]>) -> &[f32; Q] {
        self.delay_time.native.calculate_sample_accurate_values(q, &mut self.times, delay_time_input);
        let source = input.unwrap_or(&[0.0; Q]);
        self.delay.process_a_rate(source, &mut self.times, &mut self.out);
        &self.out
    }

    /// The last rendered quantum.
    pub fn output(&self) -> &[f32; Q] {
        &self.out
    }
}
