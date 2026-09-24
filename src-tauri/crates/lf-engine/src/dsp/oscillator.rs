//! Oscillators: Blink's band-limited wave tables ([`PeriodicWave`]), Blink's OscillatorNode
//! ([`Oscillator`]) and Tone's oscillator source on top of it ([`ToneOscillator`]).
//!
//! [`PeriodicWave`] ports `modules/webaudio/periodic_wave.cc` and [`Oscillator`] ports
//! `modules/webaudio/oscillator_handler.cc` with its x86 kernels
//! (`modules/webaudio/cpu/x86/oscillator_kernel_sse2.cc`) and the scheduling of
//! `audio_scheduled_source_handler.cc`, at Chromium 153.0.8010.12. A wave is one table per third of an
//! octave (36 at 44.1 and 48 kHz), each with the partials that stay below Nyquist for its pitch range,
//! all normalized by the first table's peak. The oscillator reads two neighbouring tables per frame
//! (picked by the frequency, one `log2f` each) and blends them; frequency and detune are a-rate,
//! detune scales by `exp2f(cents / 1200)`, the result is clamped to ±Nyquist. Blink's float and double
//! rounding is kept step for step (the SSE kernels' float read index, the double one of the a-rate
//! kernel, the Lagrange interpolation below 0.3 table samples per frame), including Blink's first
//! quantum quirk: a source starting mid-quantum reads the phase increments from the quantum's start.
//! The tables are built with an f64 FFT instead of Blink's f32 one (rustfft or PFFFT, a runtime flag
//! decides): they agree to about one float ulp. Deliberately not ported: custom waves with
//! `disableNormalization` (Tone never asks for one).
//!
//! [`ToneOscillator`] ports Tone.js 15.1.22's `source/oscillator/OmniOscillator.js` over
//! `Oscillator.js` for the basic types, with the start/stop state of `source/Source.js` and the native
//! node wrapper `ToneOscillatorNode.js` with its `source/OneShotSource.js` gain. Tone builds a new
//! native OscillatorNode for every start after a stop (so its phase restarts at 0) and keeps the
//! running one on a start before the stop time (a restart: the stop is cancelled, the phase runs on).
//! The Omni wrapper and the Oscillator inside it keep identical state timelines, so one is kept. Their
//! frequency and detune are Tone Signals the owner renders and passes in: the Oscillator's own
//! signals between them are overridden to zero and only add +0, so they are left out.
//!
//! Tone stops a native node from a timeout its clock fires on the first tick past the stop time; here
//! the native stop is the stop time itself. Both land where the OneShotSource gain is already zero,
//! so the output is the same. Tone builds nodes per note; this module keeps [`NODE_POOL`] per
//! oscillator and reuses finished ones, so starting and rendering never allocate.
//!
//! Every control call takes `frame`, the frame being rendered when it is made: Blink's context time
//! (and Tone's `currentTime`, which a live Tone context reads from Blink) is then the next quantum not
//! yet computed, [`param::context_frame`]. A start or event earlier than that lands on it.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use std::sync::Arc;

use super::gain::GainNode;
use super::param::{self, time_to_sample_frame, AudioParam, Rate, Rounding, Timeline, Units, QUANTUM};
use super::signal::connect_signal;

const Q: usize = QUANTUM;

/// Below this many table samples per frame the interpolation is 3-point Lagrange (Blink's
/// `kInterpolate2Point`); above it, linear.
const INTERPOLATE_2_POINT: f32 = 0.3;
/// Below this, 5-point Lagrange (`kInterpolate3Point`).
const INTERPOLATE_3_POINT: f32 = 0.16;
/// Pitch ranges per octave (`kNumberOfOctaveBands`).
const NUMBER_OF_OCTAVE_BANDS: u32 = 3;
/// `kCentsPerRange`.
const CENTS_PER_RANGE: f32 = 1200.0 / NUMBER_OF_OCTAVE_BANDS as f32;

/// Native oscillator nodes one [`ToneOscillator`] keeps: a node plays until its stop, and a start
/// after the stop may be scheduled while it still renders.
pub const NODE_POOL: usize = 4;

/// The built-in wave shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscillatorType {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

/// Blink's `PeriodicWaveImpl`: band-limited tables of one waveform at one sample rate.
pub struct PeriodicWave {
    size: usize,
    number_of_ranges: usize,
    lowest_fundamental_frequency: f32,
    rate_scale: f32,
    tables: Vec<Vec<f32>>,
}

impl PeriodicWave {
    /// A built-in waveform (`GenerateBasicWaveform`): sine coefficients in float, as Blink computes
    /// them.
    pub fn basic(kind: OscillatorType, sample_rate: f32) -> Self {
        let mut wave = PeriodicWave::empty(sample_rate);
        let half = wave.size / 2;
        let real = vec![0.0f32; half];
        let mut imag = vec![0.0f32; half];
        for (n, b) in imag.iter_mut().enumerate().skip(1) {
            let pi_factor = 2.0 / (n as f32 * std::f32::consts::PI);
            *b = match kind {
                OscillatorType::Sine => {
                    if n == 1 {
                        1.0
                    } else {
                        0.0
                    }
                }
                OscillatorType::Square => {
                    if n & 1 == 1 {
                        2.0 * pi_factor
                    } else {
                        0.0
                    }
                }
                OscillatorType::Sawtooth => pi_factor * if n & 1 == 1 { 1.0 } else { -1.0 },
                OscillatorType::Triangle => {
                    if n & 1 == 1 {
                        2.0 * (pi_factor * pi_factor) * if ((n - 1) >> 1) & 1 == 1 { -1.0 } else { 1.0 }
                    } else {
                        0.0
                    }
                }
            };
        }
        wave.create_band_limited_tables(&real, &imag);
        wave
    }

    /// `createPeriodicWave(real, imag)` with normalization: the cosine and sine coefficients of each
    /// harmonic (index 0 is DC and ignored).
    pub fn custom(real: &[f32], imag: &[f32], sample_rate: f32) -> Self {
        assert_eq!(real.len(), imag.len(), "real and imag of equal length");
        assert!(real.len() >= 2, "at least two coefficients");
        let mut wave = PeriodicWave::empty(sample_rate);
        wave.create_band_limited_tables(real, imag);
        wave
    }

    fn empty(sample_rate: f32) -> Self {
        let size = if sample_rate <= 24000.0 {
            2048
        } else if sample_rate <= 88200.0 {
            4096
        } else {
            16384
        };
        let nyquist = (0.5 * sample_rate as f64) as f32;
        let lowest_fundamental_frequency = nyquist / (size / 2) as f32;
        let rate_scale = size as f32 / sample_rate;
        // `0.5 + bands * log2f(size)`: a double, truncated.
        let number_of_ranges = (0.5 + (NUMBER_OF_OCTAVE_BANDS as f32 * (size as f32).log2()) as f64) as usize;
        PeriodicWave { size, number_of_ranges, lowest_fundamental_frequency, rate_scale, tables: Vec::new() }
    }

    /// Table samples per second of 1 Hz (`RateScale`).
    pub fn rate_scale(&self) -> f32 {
        self.rate_scale
    }

    fn number_of_partials_for_range(&self, range_index: usize) -> usize {
        let cents_to_cull = range_index as f32 * CENTS_PER_RANGE;
        // C++ `pow(2, float)` is the double pow; the product is float, truncated.
        let culling_scale = super::fdlibm::pow(2.0, (-cents_to_cull / 1200.0) as f64) as f32;
        (culling_scale * (self.size / 2) as f32) as usize
    }

    fn create_band_limited_tables(&mut self, real: &[f32], imag: &[f32]) {
        let n = self.size;
        let half = n / 2;
        let components = real.len().min(half);
        let fft = Fft::new(n);
        let mut re = vec![0.0f64; n];
        let mut im = vec![0.0f64; n];
        let mut normalization = 1.0f64;
        self.tables = Vec::with_capacity(self.number_of_ranges);
        for range in 0..self.number_of_ranges {
            // The partials this range keeps: 1 ..= number_of_partials, below the components given.
            let kept = components.min(self.number_of_partials_for_range(range) + 1);
            re.fill(0.0);
            im.fill(0.0);
            // x[n] = sum a_k cos(2 pi k n / N) + b_k sin(2 pi k n / N): the real part of the inverse
            // transform of a_k - i b_k.
            for k in 1..kept {
                re[k] = real[k] as f64;
                im[k] = -(imag[k] as f64);
            }
            fft.inverse(&mut re, &mut im);
            if range == 0 {
                let peak = re.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
                if peak > 0.0 {
                    normalization = 1.0 / peak;
                }
            }
            self.tables.push(re.iter().map(|&v| (v * normalization) as f32).collect());
        }
    }

    /// `WaveDataForFundamentalFrequency` (the scalar and SSE versions compute the same floats): the
    /// table with fewer partials, the one with more, and the blend factor between them.
    fn wave_data(&self, fundamental_frequency: f32) -> (&[f32], &[f32], f32) {
        let frequency = fundamental_frequency.abs();
        let ratio = if frequency > 0.0 { frequency / self.lowest_fundamental_frequency } else { 0.5 };
        let cents_above_lowest_frequency = ratio.log2() * 1200.0;
        let mut pitch_range = 1.0 + cents_above_lowest_frequency / CENTS_PER_RANGE;
        let top = (self.number_of_ranges - 1) as f32;
        pitch_range = if pitch_range > 0.0 { pitch_range } else { 0.0 };
        pitch_range = if pitch_range < top { pitch_range } else { top };
        let range_index1 = pitch_range as usize;
        let range_index2 = if range_index1 < self.number_of_ranges - 1 { range_index1 + 1 } else { range_index1 };
        (&self.tables[range_index2], &self.tables[range_index1], pitch_range - range_index1 as f32)
    }
}

/// An iterative radix-2 complex FFT in f64, used once per table when a wave is built.
struct Fft {
    n: usize,
    cos: Vec<f64>,
    sin: Vec<f64>,
}

impl Fft {
    fn new(n: usize) -> Self {
        assert!(n.is_power_of_two());
        let step = 2.0 * std::f64::consts::PI / n as f64;
        Fft { n, cos: (0..n / 2).map(|j| (step * j as f64).cos()).collect(), sin: (0..n / 2).map(|j| (step * j as f64).sin()).collect() }
    }

    /// In place, unnormalized, with `e^{+i}` twiddles.
    fn inverse(&self, re: &mut [f64], im: &mut [f64]) {
        let n = self.n;
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let stride = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (wr, wi) = (self.cos[k * stride], self.sin[k * stride]);
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Unscheduled,
    Scheduled,
    Playing,
    Finished,
}

/// The start/stop scheduling of Blink's `AudioScheduledSourceHandler`.
struct Schedule {
    sample_rate: f32,
    state: PlaybackState,
    start_time: f64,
    end_time: Option<f64>,
}

impl Schedule {
    /// `UpdateSchedulingInfo`: the frame offset the source starts at in this quantum, the frames it
    /// renders, and the sub-frame start offset on its first quantum. Zeroes the frames of `out` it
    /// does not render.
    fn update(&mut self, quantum_start: u64, out: &mut [f32; Q]) -> (usize, usize, f64) {
        let sample_rate = self.sample_rate as f64;
        let quantum_end = quantum_start + Q as u64;
        let start_frame = time_to_sample_frame(self.start_time, sample_rate, Rounding::Up);
        let end_frame = self.end_time.map(|t| time_to_sample_frame(t, sample_rate, Rounding::Up));
        if end_frame.is_some_and(|e| e <= quantum_start) {
            self.state = PlaybackState::Finished;
        }
        if matches!(self.state, PlaybackState::Unscheduled | PlaybackState::Finished) || start_frame >= quantum_end {
            return (0, 0, 0.0);
        }
        let mut start_frame_offset = 0.0;
        if self.state == PlaybackState::Scheduled {
            self.state = PlaybackState::Playing;
            start_frame_offset = self.start_time * sample_rate - start_frame as f64;
        }
        let offset = (start_frame.saturating_sub(quantum_start) as usize).min(Q);
        let mut frames = Q - offset;
        if frames == 0 {
            return (offset, 0, start_frame_offset);
        }
        out[..offset].fill(0.0);
        if let Some(end_frame) = end_frame {
            if end_frame >= quantum_start && end_frame <= quantum_end {
                if end_frame < quantum_end {
                    let zero_start = (end_frame - quantum_start) as usize;
                    frames = frames.saturating_sub(Q - zero_start);
                    out[zero_start..].fill(0.0);
                }
                self.state = PlaybackState::Finished;
            }
        }
        (offset, frames, start_frame_offset)
    }
}

/// Blink's OscillatorNode. One call to [`Oscillator::process`] renders one quantum.
pub struct Oscillator {
    wave: Arc<PeriodicWave>,
    nyquist: f32,
    /// a-rate, 440 Hz, ±Nyquist.
    pub frequency: AudioParam,
    /// a-rate, 0 cents, ±1200·log2(FLT_MAX).
    pub detune: AudioParam,
    schedule: Schedule,
    virtual_read_index: f64,
    phase_increments: [f32; Q],
    detune_values: [f32; Q],
    out: [f32; Q],
    silent: bool,
}

impl Oscillator {
    pub fn new(wave: Arc<PeriodicWave>, sample_rate: f32) -> Self {
        let nyquist = sample_rate / 2.0;
        // 1200 * log2f(FLT_MAX), in float.
        let detune_limit = 1200.0 * f32::MAX.log2();
        Oscillator {
            wave,
            nyquist,
            frequency: AudioParam::new(sample_rate as f64, 440.0, -nyquist, nyquist, Rate::A),
            detune: AudioParam::new(sample_rate as f64, 0.0, -detune_limit, detune_limit, Rate::A),
            schedule: Schedule { sample_rate, state: PlaybackState::Unscheduled, start_time: 0.0, end_time: None },
            virtual_read_index: 0.0,
            phase_increments: [0.0; Q],
            detune_values: [0.0; Q],
            out: [0.0; Q],
            silent: true,
        }
    }

    /// Back to a freshly built node playing `wave`.
    pub fn reset(&mut self, wave: Arc<PeriodicWave>) {
        self.wave = wave;
        self.frequency.reset();
        self.detune.reset();
        self.schedule.state = PlaybackState::Unscheduled;
        self.schedule.start_time = 0.0;
        self.schedule.end_time = None;
        self.virtual_read_index = 0.0;
        self.silent = true;
    }

    pub fn state(&self) -> PlaybackState {
        self.schedule.state
    }

    /// `start(when)`, once.
    pub fn start(&mut self, when: f64, frame: u64) {
        if self.schedule.state != PlaybackState::Unscheduled || when < 0.0 {
            debug_assert!(false, "start once, at a time >= 0");
            return;
        }
        self.schedule.start_time = when.max(param::context_time(frame, self.schedule.sample_rate as f64));
        self.schedule.state = PlaybackState::Scheduled;
    }

    /// `stop(when)`: the last call wins until the source has stopped.
    pub fn stop(&mut self, when: f64) {
        if self.schedule.state == PlaybackState::Unscheduled || when < 0.0 {
            debug_assert!(false, "stop after start, at a time >= 0");
            return;
        }
        self.schedule.end_time = Some(when.max(0.0));
    }

    /// Withdraw a stop that has not been reached yet (Tone's cancelled stop timeout).
    fn cancel_stop(&mut self) {
        if self.schedule.state != PlaybackState::Finished {
            self.schedule.end_time = None;
        }
    }

    /// The last rendered quantum.
    pub fn output(&self) -> &[f32; Q] {
        &self.out
    }

    /// Whether the last quantum is silent (Blink's bus flag).
    pub fn silent(&self) -> bool {
        self.silent
    }

    /// Render the quantum at `quantum_start`; `frequency` and `detune` are the signals connected to
    /// the params, if any. An unscheduled or finished oscillator propagates silence.
    pub fn process(&mut self, quantum_start: u64, frequency: Option<&[f32; Q]>, detune: Option<&[f32; Q]>) {
        if matches!(self.schedule.state, PlaybackState::Unscheduled | PlaybackState::Finished) {
            self.silence();
            return;
        }
        let (offset, frames, start_frame_offset) = self.schedule.update(quantum_start, &mut self.out);
        if frames == 0 {
            self.silence();
            return;
        }
        let mut virtual_read_index = self.virtual_read_index;
        let rate_scale = self.wave.rate_scale;
        let has_sample_accurate_values = self.calculate_sample_accurate_phase_increments(quantum_start, frequency, detune);
        let mut k_rate_frequency = 0.0f32;
        if !has_sample_accurate_values {
            k_rate_frequency = self.k_rate_frequency(quantum_start, frequency, detune);
        }
        let mut destination = offset;
        let mut n = frames;
        if start_frame_offset > 0.0 {
            destination += 1;
            n -= 1;
            virtual_read_index += (1.0 - start_frame_offset) * k_rate_frequency as f64 * rate_scale as f64;
        } else if start_frame_offset < 0.0 {
            virtual_read_index = -start_frame_offset * k_rate_frequency as f64 * rate_scale as f64;
        }
        let wave = Arc::clone(&self.wave);
        let out = &mut self.out[destination..destination + n];
        virtual_read_index = if has_sample_accurate_values {
            process_a_rate(&wave, out, virtual_read_index, &self.phase_increments)
        } else {
            process_k_rate(&wave, out, virtual_read_index, k_rate_frequency)
        };
        self.virtual_read_index = virtual_read_index;
        self.silent = false;
    }

    fn silence(&mut self) {
        self.out.fill(0.0);
        self.silent = true;
    }

    /// `CalculateSampleAccuratePhaseIncrements`: table samples per frame into `phase_increments`.
    fn calculate_sample_accurate_phase_increments(&mut self, q: u64, frequency: Option<&[f32; Q]>, detune: Option<&[f32; Q]>) -> bool {
        let mut has_sample_accurate_values = false;
        let mut has_frequency_changes = false;
        let mut final_scale = self.wave.rate_scale;
        if self.frequency.has_sample_accurate_values(q) && self.frequency.rate() == Rate::A {
            has_sample_accurate_values = true;
            has_frequency_changes = true;
            self.frequency.calculate_sample_accurate_values(q, &mut self.phase_increments, frequency);
        } else {
            final_scale *= self.frequency.final_value(q, frequency.map(|x| x[0]));
        }
        if self.detune.has_sample_accurate_values(q) && self.detune.rate() == Rate::A {
            has_sample_accurate_values = true;
            let values = if has_frequency_changes { &mut self.detune_values } else { &mut self.phase_increments };
            self.detune.calculate_sample_accurate_values(q, values, detune);
            let k = (1.0f64 / 1200.0) as f32;
            for v in values.iter_mut() {
                *v = (*v * k).exp2();
            }
            if has_frequency_changes {
                for (p, &d) in self.phase_increments.iter_mut().zip(&self.detune_values) {
                    *p *= d;
                }
            }
        } else {
            final_scale *= detune_to_frequency_multiplier(self.detune.final_value(q, detune.map(|x| x[0])));
        }
        if has_sample_accurate_values {
            for p in self.phase_increments.iter_mut() {
                *p = clamp_frequency(*p, self.nyquist) * final_scale;
            }
        }
        has_sample_accurate_values
    }

    /// The k-rate frequency Blink recomputes in `Process` and `ProcessKRate`.
    fn k_rate_frequency(&mut self, q: u64, frequency: Option<&[f32; Q]>, detune: Option<&[f32; Q]>) -> f32 {
        let f = self.frequency.final_value(q, frequency.map(|x| x[0]));
        let d = self.detune.final_value(q, detune.map(|x| x[0]));
        clamp_frequency(f * detune_to_frequency_multiplier(d), self.nyquist)
    }
}

/// `DetuneToFrequencyMultiplier`: `exp2(cents / 1200)` in float.
fn detune_to_frequency_multiplier(detune: f32) -> f32 {
    (detune / 1200.0).exp2()
}

/// `ClampFrequency`: NaN goes to +Nyquist.
fn clamp_frequency(f: f32, nyquist: f32) -> f32 {
    if f.is_nan() {
        nyquist
    } else {
        f.clamp(-nyquist, nyquist)
    }
}

/// Wrap a read index into `0..size` (`x - floor(x / size) * size`, with the reciprocal multiplied).
fn wrap(x: f64, size: f64, inv_size: f64) -> f64 {
    x - (x * inv_size).floor() * size
}

/// `DoInterpolation`: linear at 0.3 table samples per frame and above, else 3- or 5-point Lagrange,
/// in double, then the blend of the two tables.
fn do_interpolation(virtual_read_index: f64, incr: f32, mask: u32, table_factor: f32, lower: &[f32], higher: &[f32]) -> f32 {
    let mut sample_lower = 0.0f64;
    let mut sample_higher = 0.0f64;
    let read_index_0 = virtual_read_index as u32;
    if incr >= INTERPOLATE_2_POINT {
        let read_index2 = read_index_0.wrapping_add(1) & mask;
        let read_index_0 = read_index_0 & mask;
        let (s1_lower, s2_lower) = (lower[read_index_0 as usize], lower[read_index2 as usize]);
        let (s1_higher, s2_higher) = (higher[read_index_0 as usize], higher[read_index2 as usize]);
        let f = ((virtual_read_index as f32) - read_index_0 as f32) as f64;
        sample_higher = (1.0 - f) * s1_higher as f64 + f * s2_higher as f64;
        sample_lower = (1.0 - f) * s1_lower as f64 + f * s2_lower as f64;
    } else if incr >= INTERPOLATE_3_POINT {
        let t = virtual_read_index - read_index_0 as f64;
        let a = [0.5 * t * (t - 1.0), 1.0 - t * t, 0.5 * t * (t + 1.0)];
        for (k, a) in a.iter().enumerate() {
            let i = (read_index_0.wrapping_add(k as u32).wrapping_sub(1) & mask) as usize;
            sample_lower += a * lower[i] as f64;
            sample_higher += a * higher[i] as f64;
        }
    } else {
        let t = virtual_read_index - read_index_0 as f64;
        let t2 = t * t;
        let a = [
            t * (t2 - 1.0) * (t - 2.0) / 24.0,
            -t * (t - 1.0) * (t2 - 4.0) / 6.0,
            (t2 - 1.0) * (t2 - 4.0) / 4.0,
            -t * (t + 1.0) * (t2 - 4.0) / 6.0,
            t * (t2 - 1.0) * (t + 2.0) / 24.0,
        ];
        for (k, a) in a.iter().enumerate() {
            let i = (read_index_0.wrapping_add(k as u32).wrapping_sub(2) & mask) as usize;
            sample_lower += a * lower[i] as f64;
            sample_higher += a * higher[i] as f64;
        }
    }
    ((1.0 - table_factor) as f64 * sample_higher + table_factor as f64 * sample_lower) as f32
}

/// `ProcessARate`: the SSE2 vector loop over whole groups of four, then the scalar remainder. The
/// phase increments are read from the quantum's start whatever frame the output starts at.
fn process_a_rate(wave: &PeriodicWave, out: &mut [f32], mut virtual_read_index: f64, phase_increments: &[f32; Q]) -> f64 {
    let n = out.len();
    let rate_scale = wave.rate_scale;
    let inv_rate_scale = 1.0 / rate_scale;
    let size = wave.size as f64;
    let inv_size = 1.0 / size;
    let mask = (wave.size - 1) as u32;
    let mut k = 0;
    while k + 4 <= n {
        let incr = [phase_increments[k], phase_increments[k + 1], phase_increments[k + 2], phase_increments[k + 3]];
        let is_big_increment = incr.iter().all(|i| i.abs() >= INTERPOLATE_2_POINT);
        let data = incr.map(|i| wave.wave_data(inv_rate_scale * i));
        if is_big_increment {
            // ProcessARateVectorKernel: four read indices from double partial sums of the increments.
            let s0 = incr[0] as f64;
            let s1 = s0 + incr[1] as f64;
            let s2 = s1 + incr[2] as f64;
            let s3 = s2 + incr[3] as f64;
            let index = [virtual_read_index, virtual_read_index + s0, virtual_read_index + s1, virtual_read_index + s2]
                .map(|x| wrap_pd(x, size, inv_size));
            for m in 0..4 {
                let read0 = (index[m] as i32 as u32) & mask;
                let read1 = (read0 + 1) & mask;
                let (lower, higher, table_factor) = data[m];
                let (s1_lower, s2_lower) = (lower[read0 as usize], lower[read1 as usize]);
                let (s1_higher, s2_higher) = (higher[read0 as usize], higher[read1 as usize]);
                let factor = index[m] as f32 - read0 as i32 as f32;
                let sample_higher = s1_higher + factor * (s2_higher - s1_higher);
                let sample_lower = s1_lower + factor * (s2_lower - s1_lower);
                out[k + m] = sample_higher + table_factor * (sample_lower - sample_higher);
            }
            virtual_read_index = wrap(virtual_read_index + s3, size, inv_size);
        } else {
            for m in 0..4 {
                let (lower, higher, table_factor) = data[m];
                out[k + m] = do_interpolation(virtual_read_index, incr[m].abs(), mask, table_factor, lower, higher);
                virtual_read_index = wrap(virtual_read_index + incr[m] as f64, size, inv_size);
            }
        }
        k += 4;
    }
    // ProcessARateScalar.
    for m in k..n {
        let incr = phase_increments[m];
        let (lower, higher, table_factor) = wave.wave_data(inv_rate_scale * incr);
        out[m] = do_interpolation(virtual_read_index, incr.abs(), mask, table_factor, lower, higher);
        virtual_read_index = wrap(virtual_read_index + incr as f64, size, inv_size);
    }
    virtual_read_index
}

/// `WrapVirtualIndexVectorPd`: the floor from a truncation to int32, corrected for negatives.
fn wrap_pd(x: f64, size: f64, inv_size: f64) -> f64 {
    let r = x * inv_size;
    let mut f = r as i32;
    if r < f as f64 {
        f -= 1;
    }
    x - f as f64 * size
}

/// `WrapVirtualIndexVector`, the float version of [`wrap_pd`].
fn wrap_ps(x: f32, size: f32, inv_size: f32) -> f32 {
    let r = x * inv_size;
    let mut f = r as i32;
    if r < f as f32 {
        f -= 1;
    }
    x - f as f32 * size
}

/// `ProcessKRate`: one frequency for the quantum. At 0.3 table samples per frame and above, the SSE2
/// loop with a float read index, then the scalar remainder; the returned index is recomputed from
/// the start in double.
fn process_k_rate(wave: &PeriodicWave, out: &mut [f32], virtual_read_index: f64, frequency: f32) -> f64 {
    let n = out.len();
    let size = wave.size as f64;
    let inv_size = 1.0 / size;
    let mask = (wave.size - 1) as u32;
    let (lower, higher, table_factor) = wave.wave_data(frequency);
    let incr = frequency * wave.rate_scale;
    if incr >= INTERPOLATE_2_POINT {
        let size_f = wave.size as f32;
        let inv_size_f = 1.0 / size_f;
        let mut v = [0.0f32, 1.0, 2.0, 3.0].map(|m| wrap_ps((virtual_read_index + (m * incr) as f64) as f32, size_f, inv_size_f));
        let v_incr = 4.0 * incr;
        let mut k = 0;
        while k + 4 <= n {
            for m in 0..4 {
                let read0 = (v[m] as i32 as u32) & mask;
                let read1 = (read0 + 1) & mask;
                let factor = v[m] - read0 as i32 as f32;
                let sample_higher = higher[read0 as usize] + factor * (higher[read1 as usize] - higher[read0 as usize]);
                let sample_lower = lower[read0 as usize] + factor * (lower[read1 as usize] - lower[read0 as usize]);
                out[k + m] = sample_higher + table_factor * (sample_lower - sample_higher);
                v[m] = wrap_ps(v[m] + v_incr, size_f, inv_size_f);
            }
            k += 4;
        }
        let mut index = wrap(virtual_read_index + (k as f32 * incr) as f64, size, inv_size);
        // ProcessKRateScalar.
        for o in out[k..].iter_mut() {
            let read0 = (index as u32) & mask;
            let read1 = (read0 + 1) & mask;
            let factor = index as f32 - read0 as f32;
            let sample_higher = higher[read0 as usize] + factor * (higher[read1 as usize] - higher[read0 as usize]);
            let sample_lower = lower[read0 as usize] + factor * (lower[read1 as usize] - lower[read0 as usize]);
            *o = sample_higher + table_factor * (sample_lower - sample_higher);
            index = wrap(index + incr as f64, size, inv_size);
        }
        wrap(virtual_read_index + (n as f32 * incr) as f64, size, inv_size)
    } else {
        let mut index = virtual_read_index;
        for o in out.iter_mut() {
            *o = do_interpolation(index, incr.abs(), mask, table_factor, lower, higher);
            index = wrap(index + incr as f64, size, inv_size);
        }
        index
    }
}

// ── Tone's layer ────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Started,
    Stopped,
}

#[derive(Clone, Copy, Debug)]
struct StateEvent {
    state: State,
    time: f64,
}

/// Tone's `StateTimeline` (`core/util/StateTimeline.js`): "stopped" at 0, then the scheduled states.
struct StateTimeline {
    events: Timeline<StateEvent>,
}

impl StateTimeline {
    fn new() -> Self {
        let mut events = Timeline::new(100, |e: &StateEvent| e.time);
        events.add(StateEvent { state: State::Stopped, time: 0.0 });
        StateTimeline { events }
    }

    fn reset(&mut self) {
        self.events.clear();
        self.events.add(StateEvent { state: State::Stopped, time: 0.0 });
    }

    fn value_at(&self, time: f64) -> State {
        self.events.get(time).map_or(State::Stopped, |e| e.state)
    }

    fn set(&mut self, state: State, time: f64) {
        self.events.add(StateEvent { state, time });
    }

    fn next_state(&self, state: State, time: f64) -> bool {
        let index = self.events.search(time);
        index >= 0 && self.events.events()[index as usize..].iter().any(|e| e.state == state)
    }
}

/// Tone's `ToneOscillatorNode`: a native oscillator into its OneShotSource gain.
struct OneShotOscillator {
    osc: Oscillator,
    gain: GainNode,
    /// OneShotSource's `_startTime` (-1 until started).
    start_time: f64,
    out: [[f32; Q]; 1],
}

impl OneShotOscillator {
    fn new(wave: Arc<PeriodicWave>, sample_rate: f32, frame: u64) -> Self {
        let mut node = OneShotOscillator {
            osc: Oscillator::new(wave, sample_rate),
            gain: GainNode::new(sample_rate as f64, 0.0, Units::Gain, frame),
            start_time: -1.0,
            out: [[0.0; Q]],
        };
        node.connect_signals(frame);
        node
    }

    fn reset(&mut self, wave: Arc<PeriodicWave>, frame: u64) {
        self.osc.reset(wave);
        self.gain.reset(0.0, frame);
        self.start_time = -1.0;
        self.connect_signals(frame);
    }

    /// The owner's frequency and detune signals connect in.
    fn connect_signals(&mut self, frame: u64) {
        connect_signal(&mut self.osc.frequency, frame);
        connect_signal(&mut self.osc.detune, frame);
    }

    fn free(&self) -> bool {
        matches!(self.osc.state(), PlaybackState::Unscheduled | PlaybackState::Finished)
    }

    /// `start(time)`: `_startGain` then the native start.
    fn start(&mut self, time: f64, current_time: f64, frame: u64) {
        debug_assert!(self.start_time == -1.0, "Source cannot be started more than once");
        self.start_time = time.max(current_time);
        self.gain.gain.set_value_at_time(1.0, time, frame);
        self.osc.start(time, frame);
    }

    /// `cancelStop`: drop the stop envelope; the stop timeout has not fired yet.
    fn cancel_stop(&mut self, sample_time: f64, frame: u64) {
        debug_assert!(self.start_time != -1.0, "Source is not started");
        self.gain.gain.cancel_scheduled_values(self.start_time + sample_time, frame);
        self.osc.cancel_stop();
    }

    /// `stop(time)` = `_stopGain` with no fade out: hold, then zero, at `time`.
    fn stop(&mut self, time: f64, sample_time: f64, frame: u64) {
        self.cancel_stop(sample_time, frame);
        self.gain.gain.cancel_and_hold_at_time(time, frame);
        self.gain.gain.set_value_at_time(0.0, time, frame);
        self.osc.stop(time);
    }

    fn process(&mut self, q: u64, frequency: &[f32; Q], detune: &[f32; Q]) -> Option<&[f32; Q]> {
        if self.free() {
            return None;
        }
        self.osc.process(q, Some(frequency), Some(detune));
        if self.osc.silent() {
            return None;
        }
        let silent = self.gain.process(q, Some(std::slice::from_ref(self.osc.output())), &mut self.out);
        (!silent).then_some(&self.out[0])
    }
}

/// Tone's OmniOscillator (basic types) over its Oscillator source: start, stop and restart semantics
/// with a pool of native nodes. Frequency and detune come from the owner's signals each quantum.
pub struct ToneOscillator {
    sample_rate: f32,
    sample_time: f64,
    wave: Arc<PeriodicWave>,
    state: StateTimeline,
    nodes: Vec<OneShotOscillator>,
    current: Option<usize>,
    /// Pool order of the node starts, to reuse the oldest.
    serial: [u64; NODE_POOL],
    next_serial: u64,
}

impl ToneOscillator {
    pub fn new(wave: Arc<PeriodicWave>, sample_rate: f32, frame: u64) -> Self {
        ToneOscillator {
            sample_rate,
            sample_time: 1.0 / sample_rate as f64,
            nodes: (0..NODE_POOL).map(|_| OneShotOscillator::new(Arc::clone(&wave), sample_rate, frame)).collect(),
            wave,
            state: StateTimeline::new(),
            current: None,
            serial: [0; NODE_POOL],
            next_serial: 0,
        }
    }

    /// Back to a freshly built oscillator.
    pub fn reset(&mut self, frame: u64) {
        for node in self.nodes.iter_mut() {
            node.reset(Arc::clone(&self.wave), frame);
        }
        self.state.reset();
        self.current = None;
    }

    fn current_time(&self, frame: u64) -> f64 {
        param::context_time(frame, self.sample_rate as f64)
    }

    /// `Source.start(time)`: a restart while started (the running node keeps its phase and loses its
    /// stop), else a new native node from `time`.
    pub fn start(&mut self, time: f64, frame: u64) {
        let current_time = self.current_time(frame);
        let time = time.max(current_time);
        if self.state.value_at(time) == State::Started {
            debug_assert!(self.state.events.get(time).is_some_and(|e| param::gt(time, e.time)), "Start time must be strictly greater than previous start time");
            // Source.start cancels from `time` and sets "started" there; restart cancels it again.
            self.state.events.cancel(time);
            if let Some(i) = self.current {
                self.nodes[i].cancel_stop(self.sample_time, frame);
            }
        } else {
            self.state.set(State::Started, time);
            let i = self.take_node(frame);
            self.nodes[i].start(time, current_time, frame);
            self.current = Some(i);
        }
    }

    /// `Source.stop(time)`.
    pub fn stop(&mut self, time: f64, frame: u64) {
        let time = time.max(self.current_time(frame));
        if self.state.value_at(time) == State::Started || self.state.next_state(State::Started, time) {
            if let Some(i) = self.current {
                self.nodes[i].stop(time, self.sample_time, frame);
            }
            self.state.events.cancel(time);
            self.state.set(State::Stopped, time);
        }
    }

    /// A finished node, else the oldest (a pool this full only happens with starts scheduled far ahead).
    fn take_node(&mut self, frame: u64) -> usize {
        let i = (0..NODE_POOL)
            .filter(|&i| self.nodes[i].free())
            .min_by_key(|&i| self.serial[i])
            .unwrap_or_else(|| {
                debug_assert!(false, "oscillator node pool exhausted");
                (0..NODE_POOL).min_by_key(|&i| self.serial[i]).expect("a pool")
            });
        self.nodes[i].reset(Arc::clone(&self.wave), frame);
        self.next_serial += 1;
        self.serial[i] = self.next_serial;
        i
    }

    /// Render the quantum at `q` into `out`; returns whether it is silent.
    pub fn process(&mut self, q: u64, frequency: &[f32; Q], detune: &[f32; Q], out: &mut [f32; Q]) -> bool {
        let mut silent = true;
        for node in self.nodes.iter_mut() {
            if let Some(x) = node.process(q, frequency, detune) {
                if silent {
                    out.copy_from_slice(x);
                    silent = false;
                } else {
                    for (o, &v) in out.iter_mut().zip(x) {
                        *o += v;
                    }
                }
            }
        }
        if silent {
            out.fill(0.0);
        }
        silent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(osc: &mut Oscillator, frequency: f32, quanta: u64) -> Vec<f32> {
        let f = [frequency; Q];
        let d = [0.0f32; Q];
        let mut out = Vec::new();
        for q in 0..quanta {
            osc.process(q * Q as u64, Some(&f), Some(&d));
            out.extend_from_slice(osc.output());
        }
        out
    }

    #[test]
    fn a_basic_sine_is_a_sine() {
        let rate = 48000.0f32;
        let wave = Arc::new(PeriodicWave::basic(OscillatorType::Sine, rate));
        let mut osc = Oscillator::new(wave, rate);
        osc.frequency.set_connected(true);
        osc.detune.set_connected(true);
        osc.frequency.set_value_at_time(0.0, 0.0, 0);
        osc.start(0.0, 0);
        let v = render(&mut osc, 440.0, 8);
        for (n, &x) in v.iter().enumerate() {
            let want = (2.0 * std::f64::consts::PI * 440.0 * n as f64 / rate as f64).sin();
            assert!((x as f64 - want).abs() < 1e-4, "frame {n}: {x} vs {want}");
        }
    }

    #[test]
    fn tables_are_normalized_and_band_limited() {
        let wave = PeriodicWave::basic(OscillatorType::Sawtooth, 48000.0);
        assert_eq!(wave.tables.len(), 36);
        let peak = wave.tables[0].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 1e-6);
        // The top range culls every partial.
        assert!(wave.tables[35].iter().all(|&v| v == 0.0));
        // A rising ramp at the start: the sawtooth's positive slope at phase 0.
        assert!(wave.tables[0][1] > 0.0 && wave.tables[0][100] > wave.tables[0][1]);
    }

    #[test]
    fn a_stop_lands_on_its_frame_and_finishes() {
        let rate = 48000.0f32;
        let wave = Arc::new(PeriodicWave::basic(OscillatorType::Square, rate));
        let mut osc = Oscillator::new(wave, rate);
        osc.start(10.0 / rate as f64, 0);
        osc.stop(200.0 / rate as f64);
        let v = render(&mut osc, 1000.0, 3);
        assert!(v[..10].iter().all(|&x| x == 0.0));
        assert!(v[10..200].iter().any(|&x| x != 0.0));
        assert!(v[200..].iter().all(|&x| x == 0.0));
        assert_eq!(osc.state(), PlaybackState::Finished);
    }
}
