//! Blink's ConvolverNode at Chromium 153.0.8010.12, for the one shape the product builds: a mono input
//! into a stereo impulse response ([`Convolver`]).
//!
//! Ports `modules/webaudio/convolver_handler.cc` (the node, its tail and the silence rule of
//! `audio_handler.cc`), `platform/audio/reverb.cc` (the normalization, and the 1 → 2 → 2 case: one
//! convolver per response channel, both fed the mono input), `reverb_convolver.cc`,
//! `reverb_convolver_stage.cc`, `reverb_accumulation_buffer.cc`, `fft_convolver.cc`,
//! `direct_convolver.cc` and `fft_frame.cc`, taking the x86 AVX paths of `cpu/x86/vector_math_*` where
//! they order a sum (the direct convolution, the normalization's sum of squares).
//!
//! # Structure
//!
//! Chromium 153 convolves on the audio thread in one partitioned pass (its background-thread stages
//! are gone). The response is cut into stages: the first 64 frames by direct convolution, then FFT
//! stages whose FFT size doubles from 128 to 8192 and stays there, each stage half its FFT long. A
//! pre-delay before a stage and a post-delay in the shared accumulation buffer cancel its offset and
//! its FFT's half-block latency, so the convolver has none. The pre-delays follow each stage's
//! "render phase" (a quantum per stage index, and one more for the right channel), so the 8192-point
//! stages FFT on different quanta: a stage transforms once per half FFT, when its input half fills.
//!
//! **The worst block** (the bus's IR, 125 760 frames at 48 k): stages 1–7 (FFT 128 … 8192) have no
//! pre-delay, so they transform together every 32nd quantum, joined there by one staggered 8192 stage;
//! every other quantum runs at most one staggered 8192 stage. Per channel, no quantum does more than
//! two 8192-point FFT pairs plus one each of 128 … 4096 (the 128 twice). One quantum's work stops
//! growing with the response's length until the response passes 32 × 4096 frames (2.7 s at 48 k).
//!
//! # FFT
//!
//! Chromium 153's FFTFrame runs RustFFT (the `WebAudioRustFft` feature is stable): `rustfft_ffi.rs`
//! transforms a real frame through a half-size complex FFT with its own packing, on the plans of
//! `FftPlanner::new()`. [`FftPlan`] ports that wrapper over the same crate at the version Chromium
//! vendors (6.4.1), so the transforms are Blink's on the same CPU (the planner picks AVX, SSE or
//! scalar code at run time in both).
//!
//! # Blocks
//!
//! Blink renders 128 frames at a time. This port takes any block within one quantum and renders the
//! same bits: the direct stage keeps Blink's per-output summation order, the FFT stages stream (Blink's
//! FFTConvolver already does), and the pre-delays wrap on quantum boundaries. A cell of the
//! accumulation buffer gets its contributions in the same order at any block size: stages with equal
//! post-delays write in one call in stage order, and unequal ones are ≥ 2 944 frames apart.
//!
//! Construction allocates (plans, spectra, buffers): build a convolver off the audio thread.
//! [`Convolver::process`] never allocates.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::fdlibm;
use super::param::QUANTUM;

/// ConvolverHandler's `kMaxFftSize`.
const MAX_FFT_SIZE: usize = 8192;
/// ReverbConvolver's `kMinFFTSize`: the first stage's FFT size.
const MIN_FFT_SIZE: usize = 128;

/// Reverb's `kGainCalibration` (dB) and the rate it was calibrated at.
const GAIN_CALIBRATION: f32 = -58.0;
const GAIN_CALIBRATION_SAMPLE_RATE: f32 = 44100.0;
/// Reverb's `kMinPower`: the floor for a silent response.
const MIN_POWER: f32 = 0.000125;

/// Reverb's `CalculateNormalizationScale`: the response's RMS over all channels, inverted, calibrated
/// by −58 dB and scaled by 44.1 k over the rate. Only 1- and 2-channel responses are built, so its
/// 4-channel ("true stereo") halving is left out.
pub fn normalization_scale(response: &[&[f32]], sample_rate: f32) -> f32 {
    let length = response[0].len();
    let mut power = 0.0f32;
    for channel in response {
        power += sum_of_squares(channel);
    }
    power = (power / (response.len() * length) as f32).sqrt();
    if !power.is_finite() || power < MIN_POWER {
        power = MIN_POWER;
    }
    let mut scale = 1.0 / power;
    scale *= fdlibm::powf(10.0, GAIN_CALIBRATION * 0.05);
    if sample_rate != 0.0 {
        scale *= GAIN_CALIBRATION_SAMPLE_RATE / sample_rate;
    }
    scale
}

/// `vector_math::Vsvesq` on x86 with AVX: eight lanes over the AVX-sized head, four over the SSE-sized
/// rest, then one by one, each lane set folded into the sum in lane order. Assumes 32-byte aligned
/// channel data, as Blink's large AudioBuffer allocations are (an unaligned start would move up to
/// seven frames into a scalar head and could change the scale's last bit).
fn sum_of_squares(x: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    let avx = x.len() & !7;
    if avx > 0 {
        let mut lanes = [0.0f32; 8];
        for chunk in x[..avx].chunks_exact(8) {
            for (lane, &v) in lanes.iter_mut().zip(chunk) {
                *lane += v * v;
            }
        }
        for lane in lanes {
            sum += lane;
        }
    }
    let sse = (x.len() - avx) & !3;
    if sse > 0 {
        let mut lanes = [0.0f32; 4];
        for chunk in x[avx..avx + sse].chunks_exact(4) {
            for (lane, &v) in lanes.iter_mut().zip(chunk) {
                *lane += v * v;
            }
        }
        for lane in lanes {
            sum += lane;
        }
    }
    for &v in &x[avx + sse..] {
        sum += v * v;
    }
    sum
}

/// One FFT size: `rustfft_ffi.rs`'s even strategy, a half-size complex FFT and its packing twiddles.
struct FftPlan {
    size: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    twiddles: Vec<Complex<f32>>,
    /// The inverse's `1 / half_size`: forward then inverse is the identity.
    scale: f32,
}

impl FftPlan {
    fn new(size: usize) -> Self {
        let half = size / 2;
        // Blink plans each size once, with a fresh planner.
        let mut planner = FftPlanner::new();
        let forward = planner.plan_fft_forward(half);
        let inverse = planner.plan_fft_inverse(half);
        let twiddles = (0..half)
            .map(|k| {
                // std's sin_cos, as Blink's Rust calls it (not fdlibm).
                let angle = -2.0 * std::f64::consts::PI * (k as f64) / (size as f64);
                let (sin, cos) = angle.sin_cos();
                Complex::new((0.5 * cos) as f32, (0.5 * sin) as f32)
            })
            .collect();
        FftPlan { size, forward, inverse, twiddles, scale: 1.0 / (half as f32) }
    }

    fn half(&self) -> usize {
        self.size / 2
    }

    fn limit(&self) -> usize {
        (self.half() - 1) / 2
    }

    fn middle(&self) -> Option<usize> {
        let half = self.half();
        (half % 2 == 0).then_some(half / 2)
    }

    /// `FFTFrame::DoFFT` of `data` zero-padded to the FFT size, into `spectrum` (packed: DC in
    /// `real[0]`, Nyquist in `imag[0]`).
    fn forward(&self, data: &[f32], complex: &mut [Complex<f32>], scratch: &mut [Complex<f32>], spectrum: &mut Spectrum) {
        let half = self.half();
        let z = &mut complex[..half];
        z.fill(Complex::new(0.0, 0.0));
        for (c, pair) in z.iter_mut().zip(data.chunks(2)) {
            *c = Complex::new(pair[0], pair.get(1).copied().unwrap_or(0.0));
        }
        self.forward.process_with_scratch(z, scratch);
        // `reconstruct_real_fft_even`.
        let (real, imag) = (&mut spectrum.real, &mut spectrum.imag);
        real[0] = z[0].re + z[0].im;
        imag[0] = z[0].re - z[0].im;
        for k in 1..=self.limit() {
            let z_k = z[k];
            let z_mk = z[half - k];
            let f_even = Complex::new(0.5 * (z_k.re + z_mk.re), 0.5 * (z_k.im - z_mk.im));
            let f_odd_unscaled = Complex::new(z_k.im + z_mk.im, -(z_k.re - z_mk.re));
            let w_f_odd = self.twiddles[k] * f_odd_unscaled;
            let x_k = f_even + w_f_odd;
            let x_mk_conj = f_even - w_f_odd;
            real[k] = x_k.re;
            imag[k] = x_k.im;
            real[half - k] = x_mk_conj.re;
            imag[half - k] = -x_mk_conj.im;
        }
        if let Some(k) = self.middle() {
            real[k] = z[k].re;
            imag[k] = -z[k].im;
        }
    }

    /// `FFTFrame::DoInverseFFT` of `spectrum` into `time` (the FFT size).
    fn inverse(&self, spectrum: &Spectrum, complex: &mut [Complex<f32>], scratch: &mut [Complex<f32>], time: &mut [f32]) {
        let half = self.half();
        let (real, imag) = (&spectrum.real, &spectrum.imag);
        let z = &mut complex[..half];
        // `prepare_inverse_fft_even`.
        z[0] = Complex::new(0.5 * (real[0] + imag[0]), 0.5 * (real[0] - imag[0]));
        for k in 1..=self.limit() {
            let x_k = Complex::new(real[k], imag[k]);
            let x_mk_conj = Complex::new(real[half - k], -imag[half - k]);
            let f_even = (x_k + x_mk_conj) * 0.5;
            let x_diff = x_k - x_mk_conj;
            let f_odd = self.twiddles[k].conj() * x_diff;
            let i_f_odd = Complex::new(-f_odd.im, f_odd.re);
            z[k] = f_even + i_f_odd;
            z[half - k] = (f_even - i_f_odd).conj();
        }
        if let Some(k) = self.middle() {
            z[k] = Complex::new(real[k], -imag[k]);
        }
        self.inverse.process_with_scratch(z, scratch);
        for (pair, c) in time.chunks_exact_mut(2).zip(z.iter()) {
            pair[0] = c.re * self.scale;
            pair[1] = c.im * self.scale;
        }
    }
}

/// A packed real spectrum: `half` bins, the Nyquist bin's real part in `imag[0]`.
struct Spectrum {
    real: Vec<f32>,
    imag: Vec<f32>,
}

impl Spectrum {
    fn new(half: usize) -> Self {
        Spectrum { real: vec![0.0; half], imag: vec![0.0; half] }
    }

    /// `FFTFrame::Multiply`: bin by bin (`Zvmul`), then the packed DC and Nyquist on their own.
    fn multiply(&mut self, kernel: &Spectrum) {
        let (real0, imag0) = (self.real[0], self.imag[0]);
        let bins = self.real.iter_mut().zip(self.imag.iter_mut()).zip(kernel.real.iter().zip(&kernel.imag));
        for ((r1, i1), (&r2, &i2)) in bins {
            let (a, b) = (*r1, *i1);
            *r1 = a * r2 - b * i2;
            *i1 = a * i2 + b * r2;
        }
        self.real[0] = real0 * kernel.real[0];
        self.imag[0] = imag0 * kernel.imag[0];
    }

    /// `FFTFrame::ScaleFFT`.
    fn scale(&mut self, factor: f32) {
        for v in self.real.iter_mut().chain(self.imag.iter_mut()) {
            *v *= factor;
        }
    }
}

/// What one FFT needs while it runs. Blink gives every FFTFrame its own; stages never transform at
/// once, so the stages of a convolver share one per FFT size (same arithmetic, less memory).
struct FftWork {
    complex: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    frame: Spectrum,
    time: Vec<f32>,
}

impl FftWork {
    fn new(plan: &FftPlan) -> Self {
        let half = plan.half();
        let scratch = plan.forward.get_inplace_scratch_len().max(plan.inverse.get_inplace_scratch_len());
        FftWork {
            complex: vec![Complex::new(0.0, 0.0); half],
            scratch: vec![Complex::new(0.0, 0.0); scratch],
            frame: Spectrum::new(half),
            time: vec![0.0; plan.size],
        }
    }
}

/// An FFT size's plan and working buffers.
struct FftSize {
    plan: FftPlan,
    work: FftWork,
}

/// `FFTConvolver`: overlap-add over half-FFT input blocks, one FFT pair when a block fills. Its output
/// runs half an FFT late; the stage's delays account for that.
struct FftConvolver {
    /// Index into the convolver's FFT sizes.
    size: usize,
    kernel: Spectrum,
    /// The input half (Blink's input buffer is the FFT size with its second half always zero).
    input: Vec<f32>,
    /// The output half being read out, and the last block's second half, saved for the next.
    output: Vec<f32>,
    overlap: Vec<f32>,
    index: usize,
}

impl FftConvolver {
    fn process(&mut self, sizes: &mut [FftSize], mut source: &[f32], mut dest: &mut [f32]) {
        let FftSize { plan, work } = &mut sizes[self.size];
        let half = plan.half();
        while !source.is_empty() {
            let n = source.len().min(half - self.index);
            let span = self.index..self.index + n;
            self.input[span.clone()].copy_from_slice(&source[..n]);
            dest[..n].copy_from_slice(&self.output[span]);
            source = &source[n..];
            dest = &mut dest[n..];
            self.index += n;
            if self.index == half {
                let FftWork { complex, scratch, frame, time } = work;
                plan.forward(&self.input, complex, scratch, frame);
                frame.multiply(&self.kernel);
                plan.inverse(frame, complex, scratch, time);
                // Overlap-add the first half onto the previous block's second half; keep this one's.
                for ((o, &t), &l) in self.output.iter_mut().zip(&time[..half]).zip(&self.overlap) {
                    *o = t + l;
                }
                self.overlap.copy_from_slice(&time[half..]);
                self.index = 0;
            }
        }
    }
}

/// `DirectConvolver`, the first stage: a 64-tap FIR over `[history | block]`. Each output sums its
/// products from the oldest tap to the newest, starting from +0, as the AVX `Conv` does in each lane;
/// the loops run tap by tap across the block, so the sums vectorize over outputs.
struct DirectConvolver {
    /// The kernel reversed: `reversed[i]` weighs the input `taps - 1 - i` frames back.
    reversed: Vec<f32>,
    /// `taps - 1` frames of history, then room for one quantum.
    buffer: Vec<f32>,
}

impl DirectConvolver {
    fn new(kernel: Vec<f32>) -> Self {
        let taps = kernel.len();
        let reversed = kernel.into_iter().rev().collect();
        DirectConvolver { reversed, buffer: vec![0.0; taps - 1 + QUANTUM] }
    }

    fn process(&mut self, source: &[f32], dest: &mut [f32]) {
        let n = source.len();
        let history = self.reversed.len() - 1;
        self.buffer[history..history + n].copy_from_slice(source);
        dest.fill(0.0);
        for (i, &h) in self.reversed.iter().enumerate() {
            for (s, &x) in dest.iter_mut().zip(&self.buffer[i..i + n]) {
                *s += h * x;
            }
        }
        self.buffer.copy_within(n..n + history, 0);
    }
}

enum Kernel {
    Direct(DirectConvolver),
    Fft(FftConvolver),
}

/// `ReverbConvolverStage`: one slice of the response, pre-delayed so its FFT lands on its render
/// phase and post-delayed (in the accumulation buffer) to its offset.
struct Stage {
    kernel: Kernel,
    /// The pre-delay ring: whole quanta, empty for none.
    pre_delay: Vec<f32>,
    pre_index: usize,
    post_delay: usize,
    frames_processed: u64,
}

/// Where a stage sits in the response and when it transforms.
struct StageLayout {
    offset: usize,
    length: usize,
    fft_size: usize,
    render_phase: usize,
}

impl Stage {
    fn new(response: &[f32], at: StageLayout, scale: f32, sizes: &mut Vec<FftSize>) -> Self {
        let StageLayout { offset, length, fft_size, render_phase } = at;
        let half = fft_size / 2;
        let direct = offset == 0;
        let segment = &response[offset..offset + length];
        let kernel = if direct {
            let mut taps = vec![0.0f32; half];
            taps[..length].copy_from_slice(segment);
            if scale != 1.0 {
                for t in &mut taps[..length] {
                    *t *= scale;
                }
            }
            Kernel::Direct(DirectConvolver::new(taps))
        } else {
            let size = match sizes.iter().position(|s| s.plan.size == fft_size) {
                Some(i) => i,
                None => {
                    let plan = FftPlan::new(fft_size);
                    let work = FftWork::new(&plan);
                    sizes.push(FftSize { plan, work });
                    sizes.len() - 1
                }
            };
            // `DoPaddedFFT`, then the normalization on the spectrum (Blink scales it there: linear).
            let mut kernel = Spectrum::new(half);
            let FftSize { plan, work } = &mut sizes[size];
            plan.forward(segment, &mut work.complex, &mut work.scratch, &mut kernel);
            if scale != 1.0 {
                kernel.scale(scale);
            }
            Kernel::Fft(FftConvolver { size, kernel, input: vec![0.0; half], output: vec![0.0; half], overlap: vec![0.0; half], index: 0 })
        };
        // The stage sits `offset` frames into the response, and an FFT stage is already half an FFT late.
        let mut total_delay = offset;
        if !direct && total_delay >= half {
            total_delay -= half;
        }
        let max_pre_delay = half.min(total_delay);
        let mut pre_delay = if total_delay > 0 { render_phase % max_pre_delay } else { 0 };
        pre_delay = pre_delay / QUANTUM * QUANTUM;
        if pre_delay > total_delay {
            pre_delay = 0;
        }
        Stage { kernel, pre_delay: vec![0.0; pre_delay], pre_index: 0, post_delay: total_delay - pre_delay, frames_processed: 0 }
    }

    fn process(&mut self, source: &[f32], sizes: &mut [FftSize], temp: &mut [f32], accumulation: &mut Accumulation) {
        let n = source.len();
        let delayed = !self.pre_delay.is_empty();
        let span = self.pre_index..self.pre_index + n;
        // The convolver starts once the pre-delay has filled; until then only time passes.
        if self.frames_processed >= self.pre_delay.len() as u64 {
            let input = if delayed { &self.pre_delay[span.clone()] } else { source };
            match &mut self.kernel {
                Kernel::Direct(d) => d.process(input, temp),
                Kernel::Fft(f) => f.process(sizes, input, temp),
            }
            accumulation.accumulate(temp, self.post_delay);
        }
        if delayed {
            self.pre_delay[span].copy_from_slice(source);
            self.pre_index += n;
            if self.pre_index >= self.pre_delay.len() {
                self.pre_index = 0;
            }
        }
        self.frames_processed += n as u64;
    }
}

/// `ReverbAccumulationBuffer`: the stages sum into it at their post-delays; the convolver reads and
/// clears. Each stage's read index in Blink moves in step with the buffer's, so one index serves all.
struct Accumulation {
    buffer: Vec<f32>,
    read: usize,
}

impl Accumulation {
    fn accumulate(&mut self, source: &[f32], delay: usize) {
        let len = self.buffer.len();
        let write = (self.read + delay) % len;
        let first = source.len().min(len - write);
        for (d, &s) in self.buffer[write..write + first].iter_mut().zip(source) {
            *d += s;
        }
        for (d, &s) in self.buffer.iter_mut().zip(&source[first..]) {
            *d += s;
        }
    }

    fn read_and_clear(&mut self, dest: &mut [f32]) {
        let len = self.buffer.len();
        let first = dest.len().min(len - self.read);
        let rest = dest.len() - first;
        let span = self.read..self.read + first;
        dest[..first].copy_from_slice(&self.buffer[span.clone()]);
        self.buffer[span].fill(0.0);
        dest[first..].copy_from_slice(&self.buffer[..rest]);
        self.buffer[..rest].fill(0.0);
        self.read = (self.read + dest.len()) % len;
    }
}

/// `ReverbConvolver`: one response channel's stages and their accumulation buffer.
struct ReverbConvolver {
    stages: Vec<Stage>,
    accumulation: Accumulation,
}

impl ReverbConvolver {
    fn new(response: &[f32], convolver_render_phase: usize, scale: f32, sizes: &mut Vec<FftSize>) -> Self {
        let total = response.len();
        let mut stages = Vec::new();
        let (mut offset, mut i, mut fft_size) = (0, 0, MIN_FFT_SIZE);
        while offset < total {
            let length = (fft_size / 2).min(total - offset);
            let render_phase = convolver_render_phase + i * QUANTUM;
            stages.push(Stage::new(response, StageLayout { offset, length, fft_size, render_phase }, scale, sizes));
            // The direct stage takes the first 64 frames and leaves the first FFT stage at 128.
            if offset != 0 {
                fft_size = (fft_size * 2).min(MAX_FFT_SIZE);
            }
            offset += length;
            i += 1;
        }
        ReverbConvolver { stages, accumulation: Accumulation { buffer: vec![0.0; total + QUANTUM], read: 0 } }
    }

    fn process(&mut self, source: &[f32], dest: &mut [f32], sizes: &mut [FftSize], temp: &mut [f32]) {
        for stage in &mut self.stages {
            stage.process(source, sizes, temp, &mut self.accumulation);
        }
        self.accumulation.read_and_clear(dest);
    }
}

/// Blink's ConvolverNode with a stereo response and a mono input.
pub struct Convolver {
    channels: [ReverbConvolver; 2],
    sizes: Vec<FftSize>,
    temp: [f32; QUANTUM],
    zeros: [f32; QUANTUM],
    sample_rate: f64,
    /// The response's length in seconds: how long the node renders after its input goes silent.
    tail: f64,
    /// Blink's `last_non_silent_time_`: the end of the last quantum with a non-silent input.
    last_non_silent_time: f64,
    quantum: Option<u64>,
    /// The convolver runs this quantum (else the node outputs silence and its state waits).
    active: bool,
}

impl Convolver {
    /// `convolver.buffer = response`, `normalize` as given (allocates: build it off the audio thread).
    pub fn new(response: [&[f32]; 2], sample_rate: f32, normalize: bool) -> Self {
        assert_eq!(response[0].len(), response[1].len(), "the response's channels differ in length");
        assert!(!response[0].is_empty(), "an empty response");
        let scale = if normalize { normalization_scale(&response, sample_rate) } else { 1.0 };
        let mut sizes = Vec::new();
        // Reverb starts each channel's convolver a quantum further along its render phase.
        let left = ReverbConvolver::new(response[0], 0, scale, &mut sizes);
        let right = ReverbConvolver::new(response[1], QUANTUM, scale, &mut sizes);
        Convolver {
            channels: [left, right],
            sizes,
            temp: [0.0; QUANTUM],
            zeros: [0.0; QUANTUM],
            sample_rate: sample_rate as f64,
            tail: response[0].len() as f64 / sample_rate as f64,
            last_non_silent_time: 0.0,
            quantum: None,
            active: false,
        }
    }

    /// Render frames `frame..frame + left.len()`: blocks follow each other, none crosses a quantum.
    /// `input` is `None` when the input is silent this quantum (the same for every block of a
    /// quantum). Returns whether the output is silent: the input has been silent for longer than the
    /// response (Blink's tail), so the node skips the quantum, its state untouched.
    pub fn process(&mut self, frame: u64, input: Option<&[f32]>, left: &mut [f32], right: &mut [f32]) -> bool {
        let q = QUANTUM as u64;
        let quantum_start = frame - frame % q;
        debug_assert!(frame + left.len() as u64 <= quantum_start + q, "a block crosses a quantum");
        debug_assert_eq!(left.len(), right.len());
        if self.quantum != Some(quantum_start) {
            self.quantum = Some(quantum_start);
            // AudioHandler::ProcessIfNecessary: silent inputs past latency (0) + tail propagate silence.
            let current_time = quantum_start as f64 / self.sample_rate;
            self.active = input.is_some() || self.last_non_silent_time + self.tail >= current_time;
            if input.is_some() {
                self.last_non_silent_time = (quantum_start + q) as f64 / self.sample_rate;
            }
        }
        if !self.active {
            left.fill(0.0);
            right.fill(0.0);
            return true;
        }
        let source = input.unwrap_or(&self.zeros[..left.len()]);
        let temp = &mut self.temp[..left.len()];
        self.channels[0].process(source, left, &mut self.sizes, temp);
        self.channels[1].process(source, right, &mut self.sizes, temp);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_cover_the_response_with_zero_latency() {
        let response = vec![0.5f32; 125_760];
        let mut sizes = Vec::new();
        let c = ReverbConvolver::new(&response, 0, 1.0, &mut sizes);
        assert_eq!(c.stages.len(), 37, "a direct stage, FFT 128 … 4096, then 30 stages of 8192");
        assert_eq!(sizes.iter().map(|s| s.plan.size).collect::<Vec<_>>(), [128, 256, 512, 1024, 2048, 4096, 8192]);
        let mut offset = 0;
        for (i, s) in c.stages.iter().enumerate() {
            let (latency, length) = match &s.kernel {
                Kernel::Direct(d) => (0, d.reversed.len()),
                Kernel::Fft(f) => (f.input.len(), f.input.len().min(response.len() - offset)),
            };
            assert_eq!(latency + s.pre_delay.len() + s.post_delay, offset, "stage {i}");
            offset += length;
        }
        assert_eq!(offset, response.len());
        // Block-size independence (module doc): distinct post-delays never meet within one quantum.
        for phase in [0, QUANTUM] {
            let c = ReverbConvolver::new(&response, phase, 1.0, &mut sizes);
            let mut posts: Vec<usize> = c.stages.iter().map(|s| s.post_delay).collect();
            posts.sort_unstable();
            posts.dedup();
            assert!(posts.windows(2).all(|w| w[1] - w[0] >= 2944), "phase {phase}: {posts:?}");
        }
    }

    #[test]
    fn an_impulse_renders_the_response() {
        let response: Vec<f32> = (0..20_000).map(|k| ((k * 7919 % 1000) as f32 / 500.0 - 1.0) * 0.9998f32.powi(k)).collect();
        let reversed: Vec<f32> = response.iter().rev().copied().collect();
        let mut c = Convolver::new([&response, &reversed], 48000.0, false);
        let frames = response.len() + 256;
        let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        let mut impulse = [0.0f32; QUANTUM];
        impulse[0] = 1.0;
        let silence = [0.0f32; QUANTUM];
        for q in 0..frames / QUANTUM {
            let span = q * QUANTUM..(q + 1) * QUANTUM;
            let input = if q == 0 { &impulse } else { &silence };
            c.process((q * QUANTUM) as u64, Some(input), &mut l[span.clone()], &mut r[span]);
        }
        for (got, want) in [(&l, &response), (&r, &reversed)] {
            let err = got.iter().zip(want.iter()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(err < 1e-5, "max error {err}");
            assert!(got[response.len()..].iter().all(|v| v.abs() < 1e-5), "a tail past the response");
        }
    }

    #[test]
    fn a_silent_input_renders_the_tail_then_the_node_goes_silent() {
        let response = vec![0.1f32; 1000];
        let mut c = Convolver::new([&response, &response], 48000.0, true);
        let (mut l, mut r) = ([0.0f32; QUANTUM], [0.0f32; QUANTUM]);
        assert!(!c.process(0, Some(&[1.0; QUANTUM]), &mut l, &mut r));
        // The input's last sound ends at frame 128; the tail runs 1000 frames past it.
        for q in 1..=8u64 {
            assert!(!c.process(q * 128, None, &mut l, &mut r), "quantum {q} is within the tail");
        }
        assert!(c.process(9 * 128, None, &mut l, &mut r));
        assert!(l.iter().chain(&r).all(|&v| v == 0.0));
        assert!(!c.process(10 * 128, Some(&[0.0; QUANTUM]), &mut l, &mut r), "a sounding input wakes it");
    }

    #[test]
    fn the_scale_is_the_rms_inverted_and_calibrated() {
        let ones = vec![1.0f32; 1000];
        assert_eq!(normalization_scale(&[&ones, &ones], 44100.0), fdlibm::powf(10.0, -58.0 * 0.05));
        let silent = vec![0.0f32; 10];
        assert_eq!(normalization_scale(&[&silent, &silent], 44100.0), (1.0 / MIN_POWER) * fdlibm::powf(10.0, -58.0 * 0.05));
    }
}
