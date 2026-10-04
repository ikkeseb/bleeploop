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
//! [`Delay::clear`] (no Blink counterpart: the engine's session boundary, `fx::FxChain::clear_history`)
//! silences everything written before it in O(1): until the buffer has been written over once, a read
//! of a frame older than the clear reads 0. Zeroing the buffers instead writes them whole in one
//! callback: a 2 s line is 96 128 floats at 48 kHz (384 128 at 192 kHz), and a five-lane load or CLEAR
//! ALL clears twenty lines (each lane's delay and the PitchShift's three).
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
    /// Frames written since the last [`Delay::clear`] before the current quantum's first frame, negative
    /// for a clear inside the quantum, up to the buffer's length: from there no frame in it is older.
    fresh: i64,
}

impl Delay {
    pub fn new(max_delay_time: f64, sample_rate: f32) -> Self {
        assert!(max_delay_time > 0.0 && max_delay_time.is_finite(), "a positive maximum delay");
        // `BufferLengthForDelay`.
        let length = Q + time_to_sample_frame(max_delay_time, sample_rate as f64, Rounding::Up) as usize;
        Delay { buffer: vec![0.0; length], write_index: 0, max_delay_time: max_delay_time as f32, sample_rate, fresh: length as i64 }
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_index = 0;
        self.fresh = self.buffer.len() as i64;
    }

    /// From frame `k` of the current quantum on (frames 0 to `k - 1` written), every frame written
    /// before reads as 0. O(1): no frame of the buffer is touched.
    pub fn clear(&mut self, k: usize) {
        self.fresh = -(k as i64);
    }

    /// The quantum's last frame is written: the write position moves on a quantum.
    fn end_quantum(&mut self) {
        self.write_index = self.wrap(self.write_index + Q);
        self.fresh = (self.fresh + Q as i64).min(self.buffer.len() as i64);
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
        self.end_quantum();
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
            self.end_quantum();
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
            self.end_quantum();
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
        let (mut sample1, mut sample2) = (self.buffer[read_index1], self.buffer[read_index2]);
        let length = self.buffer.len();
        if self.fresh < length as i64 {
            // Since the clear: frames 0 to `fresh + k` back from frame `k`'s write position.
            let w = self.wrap(self.write_index + k);
            let back = |i: usize| (if w >= i { w - i } else { w + length - i }) as i64;
            if back(read_index1) > self.fresh + k as i64 {
                sample1 = 0.0;
            }
            if back(read_index2) > self.fresh + k as i64 {
                sample2 = 0.0;
            }
        }
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

    /// From frame `k` of the current quantum on, what was written before reads as 0 ([`Delay::clear`]).
    pub fn clear(&mut self, k: usize) {
        self.delay.clear(k);
    }

    /// The last quantum [`DelayNode::process`] rendered.
    pub fn output(&self) -> &[f32; Q] {
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;
    const CLEAR: usize = 30_037;

    /// A 1 s line, frame by frame at `time(f)` seconds: noise until `clear` when `heard`, then silence
    /// with an impulse at each of `pulses`; `clear` clears it there ([`Delay::clear`]). The output and
    /// the line.
    fn run(heard: bool, clear: Option<usize>, pulses: &[usize], time: impl Fn(usize) -> f32, frames: usize) -> (Vec<f32>, Delay) {
        let mut d = Delay::new(1.0, RATE);
        let until = clear.unwrap_or(frames);
        let out = (0..frames)
            .map(|f| {
                let k = f % Q;
                if clear == Some(f) {
                    d.clear(k);
                }
                let x = match f {
                    f if pulses.contains(&f) => 1.0,
                    f if heard && f < until => ((f * 37 % 101) as f32 / 50.0 - 1.0) * 0.5,
                    _ => 0.0,
                };
                d.process_frame(k, x, time(f))
            })
            .collect();
        (out, d)
    }

    #[test]
    fn a_clear_stops_masking_once_the_line_is_written_over() {
        // A pulse while the clear still masks, one after the line has been written over whole.
        let (length, frames) = (Q + 48_000, 150_000);
        let late = CLEAR + length + 1000;
        let pulses = [CLEAR + 10_000, late];
        let time = |_| 0.25;
        let (stale, _) = run(true, None, &pulses, time, frames);
        assert!(stale[CLEAR..CLEAR + 12_000].iter().any(|&x| x != 0.0), "uncleared, the noise echoes on");
        let (fresh, _) = run(false, None, &pulses, time, frames);
        let (cleared, line) = run(true, Some(CLEAR), &pulses, time, frames);
        assert_eq!(line.fresh, length as i64, "the counter saturates at the line's length");
        assert!(cleared[CLEAR..] == fresh[CLEAR..], "from the clear on, a line that heard only silence");
        assert_eq!(cleared[late + 12_000], 1.0, "the late pulse echoes unmasked");
        assert_eq!(cleared[CLEAR + 22_000], 1.0, "the early pulse echoes under the mask");
    }

    #[test]
    fn a_delay_time_changed_after_a_clear_reads_nothing_from_before_it() {
        // 62.5 ms (3000 frames), then from 4000 frames on (after the pulse's echo) a ramp to 0.9 s over
        // 1000 frames: the read sweeps back across the clear's frame into the noise before it, at
        // fractional positions.
        let time = |f: usize| 0.0625 + 0.8375 * ((f.saturating_sub(CLEAR + 4000)) as f32 / 1000.0).min(1.0);
        let pulses = [CLEAR + 100];
        let frames = CLEAR + 60_000;
        let (stale, _) = run(true, None, &pulses, time, frames);
        assert!(stale[CLEAR + 4000..CLEAR + 40_000].iter().any(|&x| x != 0.0), "uncleared, the long read finds the noise");
        let (fresh, _) = run(false, None, &pulses, time, frames);
        let (cleared, _) = run(true, Some(CLEAR), &pulses, time, frames);
        assert!(cleared[CLEAR..] == fresh[CLEAR..], "from the clear on, a line that heard only silence");
        assert_eq!(cleared[CLEAR + 100 + 3000], 1.0, "the pulse after the clear echoes at 62.5 ms");
    }
}
