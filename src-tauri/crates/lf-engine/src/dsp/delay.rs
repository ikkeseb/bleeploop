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
//! Tone class sets.
//!
//! Two ways to drive it, the same arithmetic: a quantum at a time ([`DelayNode::process`]), or frame by
//! frame ([`DelayNode::begin_quantum`], then [`DelayNode::process_frame`] for frames 0 to 127 in order)
//! for a delay whose input is rendered frame by frame, such as one inside a feedback cycle. What Blink
//! does to such a cycle belongs to its owner (see `fx::DelayFx`), not to the kernel.
//!
//! Blink stops processing a delay once its input has been silent for longer than the maximum delay
//! (the node's tail); this one keeps writing zeros instead, which reads the same, since everything
//! within reach of the read head is zero by then. Mono; allocation happens only in [`Delay::new`].
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::param::{time_to_sample_frame, AudioParam, Rate, Rounding, ToneParam, Units, QUANTUM};

const Q: usize = QUANTUM;

/// Blink's `Delay` kernel: one channel's circular buffer, a quantum longer than the longest delay.
pub struct Delay {
    buffer: Vec<f32>,
    write_index: usize,
    max_delay_time: f32,
    sample_rate: f32,
}

impl Delay {
    pub fn new(max_delay_time: f64, sample_rate: f32) -> Self {
        assert!(max_delay_time > 0.0 && max_delay_time.is_finite(), "a positive maximum delay");
        // `BufferLengthForDelay`.
        let length = Q + time_to_sample_frame(max_delay_time, sample_rate as f64, Rounding::Up) as usize;
        Delay { buffer: vec![0.0; length], write_index: 0, max_delay_time: max_delay_time as f32, sample_rate }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_index = 0;
    }

    pub fn max_delay_time(&self) -> f32 {
        self.max_delay_time
    }

    /// The circular buffer's length: after this many frames written, nothing older is left in it.
    pub fn buffer_frames(&self) -> usize {
        self.buffer.len()
    }

    fn wrap(&self, i: usize) -> usize {
        if i >= self.buffer.len() {
            i - self.buffer.len()
        } else {
            i
        }
    }

    /// `ProcessARate` on a quantum: `delay_times` in seconds per frame.
    pub fn process_a_rate(&mut self, source: &[f32; Q], delay_times: &[f32; Q], destination: &mut [f32; Q]) {
        // CopyToCircularBuffer.
        let first = Q.min(self.buffer.len() - self.write_index);
        self.buffer[self.write_index..self.write_index + first].copy_from_slice(&source[..first]);
        self.buffer[..Q - first].copy_from_slice(&source[first..]);
        for (k, (d, &t)) in destination.iter_mut().zip(delay_times).enumerate() {
            *d = self.read(k, t);
        }
        self.write_index = self.wrap(self.write_index + Q);
    }

    /// Frame `k` of `ProcessARate`: write `input`, read `delay_time` seconds back; after frame 127 the
    /// quantum ends. Frames go 0 to 127 in order. Reads what [`Delay::process_a_rate`] reads: a read
    /// only reaches past the frame being written when the delay is under a frame, where the later
    /// sample weighs 0.
    pub fn process_frame(&mut self, k: usize, input: f32, delay_time: f32) -> f32 {
        let w = self.wrap(self.write_index + k);
        self.buffer[w] = input;
        let y = self.read(k, delay_time);
        if k == Q - 1 {
            self.write_index = self.wrap(self.write_index + Q);
        }
        y
    }

    /// Frame `k` of a feedback loop, read first: the frame `delay_time` seconds back, before frame `k` is
    /// written ([`Delay::write_frame`], for the same `k`, follows). For a delay of at least one frame
    /// this is what [`Delay::process_frame`] reads (its read never reaches the frame being written), so
    /// an owner can feed the delayed frame back into frame `k` itself, where a Blink cycle feeds it a
    /// quantum late (`fx::DelayFx`).
    pub fn read_frame(&self, k: usize, delay_time: f32) -> f32 {
        self.read(k, delay_time)
    }

    /// Frame `k` of a feedback loop, written after its [`Delay::read_frame`]; after frame 127 the quantum
    /// ends. Frames go 0 to 127 in order.
    pub fn write_frame(&mut self, k: usize, input: f32) {
        let w = self.wrap(self.write_index + k);
        self.buffer[w] = input;
        if k == Q - 1 {
            self.write_index = self.wrap(self.write_index + Q);
        }
    }

    /// `ProcessARateVector`'s lane for frame `k` of the quantum: float read position, truncated to an
    /// index as `_mm_cvttps_epi32` does, then `s1 + f * (s2 - s1)`.
    fn read(&self, k: usize, delay_time: f32) -> f32 {
        // ProcessARate's NaN substitution, then the kernel's max(0, t).
        let delay_time = if delay_time.is_nan() { self.max_delay_time } else { delay_time };
        let delay_time = if delay_time > 0.0 { delay_time } else { 0.0 };
        let length_f = self.buffer.len() as f32;
        let desired_delay_frames = delay_time * self.sample_rate;
        let mut read_position = self.wrap(self.write_index + k) as f32 + (length_f - desired_delay_frames);
        if read_position >= length_f {
            read_position -= length_f;
        }
        let read_index1 = self.wrap(read_position as i32 as usize);
        let read_index2 = self.wrap(read_index1 + 1);
        let interpolation_factor = read_position - read_index1 as f32;
        let (sample1, sample2) = (self.buffer[read_index1], self.buffer[read_index2]);
        sample1 + interpolation_factor * (sample2 - sample1)
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
        self.begin_quantum(q, delay_time_input);
        let source = input.unwrap_or(&[0.0; Q]);
        self.delay.process_a_rate(source, &self.times, &mut self.out);
        &self.out
    }

    /// The delay times of the quantum at `q`, for [`DelayNode::process_frame`].
    pub fn begin_quantum(&mut self, q: u64, delay_time_input: Option<&[f32; Q]>) {
        self.delay_time.native.calculate_sample_accurate_values(q, &mut self.times, delay_time_input);
    }

    /// Frame `k` of the quantum begun: `input` in, the delayed frame out (see [`Delay::process_frame`]).
    pub fn process_frame(&mut self, k: usize, input: f32) -> f32 {
        self.delay.process_frame(k, input, self.times[k])
    }

    /// Frame `k` of the quantum begun, read before it is written (see [`Delay::read_frame`]).
    pub fn read_frame(&self, k: usize) -> f32 {
        self.delay.read_frame(k, self.times[k])
    }

    /// Frame `k` of the quantum begun, written after its read (see [`Delay::write_frame`]).
    pub fn write_frame(&mut self, k: usize, input: f32) {
        self.delay.write_frame(k, input);
    }

    /// The last quantum [`DelayNode::process`] rendered.
    pub fn output(&self) -> &[f32; Q] {
        &self.out
    }
}
