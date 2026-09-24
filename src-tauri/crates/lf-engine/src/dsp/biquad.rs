//! Blink's BiquadFilterNode: the lowpass and highpass coefficient formulas and the filter kernel of
//! `platform/audio/biquad.{h,cc}`, and the per-quantum parameter handling of
//! `modules/webaudio/biquad_filter_handler.cc`, at Chromium 153.0.8010.12 (x86 paths).
//!
//! Each quantum the node reads its four params. When any has sample-accurate values (automation, or a
//! signal connected into it, as Tone's `Filter` always does) it computes their a-rate values and, if
//! any of them moves inside the quantum, one coefficient set per frame; otherwise one set for the
//! quantum. With no automation it is the k-rate path: coefficients are recomputed only when a param's
//! final value changes. Coefficients are fixed at the quantum's start, and the kernel runs frame by
//! frame, so the node renders any block split of a quantum identically.
//!
//! Not ported: the other six filter types (no production path builds one), `getFrequencyResponse`,
//! and the tail time. Blink stops rendering a node whose input has been silent for longer than its
//! tail; here the node always renders, so after its input stops its output decays toward zero instead
//! of cutting to silence below 1/32768. Blink's audio thread runs with the CPU's flush-to-zero on: a
//! float output below `FLT_MIN` is flushed to zero here as it would be there.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::fdlibm;
use super::param::{AudioParam, Rate, QUANTUM};

/// The filter types a production path builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterType {
    Lowpass,
    Highpass,
}

/// Blink's `pow10`: 10^x as `fdlibm::expf(x * ln 10)`, which rounds through float twice.
fn pow10(x: f64) -> f64 {
    // Blink writes ln 10 as 2.30258509299404568402, which is LN_10 as a double.
    fdlibm::expf((x * std::f64::consts::LN_10) as f32) as f64
}

/// Blink's `Biquad`: coefficients for up to a quantum of frames and the direct-form I state.
pub struct Biquad {
    b0: [f64; QUANTUM],
    b1: [f64; QUANTUM],
    b2: [f64; QUANTUM],
    a1: [f64; QUANTUM],
    a2: [f64; QUANTUM],
    has_sample_accurate_values: bool,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new()
    }
}

impl Biquad {
    /// A straight wire with clear memory.
    pub fn new() -> Self {
        let mut b = Biquad {
            b0: [0.0; QUANTUM],
            b1: [0.0; QUANTUM],
            b2: [0.0; QUANTUM],
            a1: [0.0; QUANTUM],
            a2: [0.0; QUANTUM],
            has_sample_accurate_values: false,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        b.set_normalized_coefficients(0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        b
    }

    /// Clear the filter memory.
    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    pub fn has_sample_accurate_values(&self) -> bool {
        self.has_sample_accurate_values
    }

    /// Whether [`Biquad::process`] reads a coefficient set per frame or the one at index 0.
    pub fn set_has_sample_accurate_values(&mut self, sample_accurate: bool) {
        self.has_sample_accurate_values = sample_accurate;
    }

    /// `cutoff` is normalized to Nyquist; `resonance` is in dB.
    pub fn set_lowpass_params(&mut self, index: usize, cutoff: f64, resonance: f64) {
        let cutoff = cutoff.clamp(0.0, 1.0);
        if cutoff == 1.0 {
            self.set_normalized_coefficients(index, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        } else if cutoff > 0.0 {
            let resonance = pow10(resonance / 20.0);
            let theta = std::f64::consts::PI * cutoff;
            let alpha = fdlibm::sin(theta) / (2.0 * resonance);
            let cosw = fdlibm::cos(theta);
            let beta = (1.0 - cosw) / 2.0;
            self.set_normalized_coefficients(index, beta, 2.0 * beta, beta, 1.0 + alpha, -2.0 * cosw, 1.0 - alpha);
        } else {
            // Nothing gets through.
            self.set_normalized_coefficients(index, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        }
    }

    /// `cutoff` is normalized to Nyquist; `resonance` is in dB.
    pub fn set_highpass_params(&mut self, index: usize, cutoff: f64, resonance: f64) {
        let cutoff = cutoff.clamp(0.0, 1.0);
        if cutoff == 1.0 {
            self.set_normalized_coefficients(index, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        } else if cutoff > 0.0 {
            let resonance = pow10(resonance / 20.0);
            let theta = std::f64::consts::PI * cutoff;
            let alpha = fdlibm::sin(theta) / (2.0 * resonance);
            let cosw = fdlibm::cos(theta);
            let beta = (1.0 + cosw) / 2.0;
            self.set_normalized_coefficients(index, beta, -2.0 * beta, beta, 1.0 + alpha, -2.0 * cosw, 1.0 - alpha);
        } else {
            // At zero the formulas cancel a quadratic against itself: the transfer function is 1.
            self.set_normalized_coefficients(index, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_normalized_coefficients(&mut self, index: usize, b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) {
        let a0_inverse = 1.0 / a0;
        self.b0[index] = b0 * a0_inverse;
        self.b1[index] = b1 * a0_inverse;
        self.b2[index] = b2 * a0_inverse;
        self.a1[index] = a1 * a0_inverse;
        self.a2[index] = a2 * a0_inverse;
    }

    /// Filter frames `at..at + source.len()` of the current quantum (`at` indexes the per-frame
    /// coefficients). Blink evaluates in double and stores each output as float.
    pub fn process(&mut self, at: usize, source: &[f32], dest: &mut [f32]) {
        debug_assert!(at + source.len() <= QUANTUM && source.len() == dest.len());
        let (mut x1, mut x2, mut y1, mut y2) = (self.x1, self.x2, self.y1, self.y2);
        for (k, (&x, d)) in source.iter().zip(dest.iter_mut()).enumerate() {
            let c = if self.has_sample_accurate_values { at + k } else { 0 };
            let x = x as f64;
            let mut y = (self.b0[c] * x + self.b1[c] * x1 + self.b2[c] * x2 - self.a1[c] * y1 - self.a2[c] * y2) as f32;
            if y.abs() < f32::MIN_POSITIVE {
                y = 0.0f32.copysign(y);
            }
            *d = y;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y as f64;
        }
        (self.x1, self.x2, self.y1, self.y2) = (x1, x2, y1, y2);
    }
}

/// Blink's `HasConstantValues`: every value equals the first.
fn has_constant_values(values: &[f32]) -> bool {
    values.iter().all(|&v| v == values[0])
}

/// The signals summed into a node's params for one quantum (a connected Tone Signal's output).
#[derive(Clone, Copy, Default)]
pub struct ParamInputs<'a> {
    pub frequency: Option<&'a [f32; QUANTUM]>,
    pub q: Option<&'a [f32; QUANTUM]>,
    pub gain: Option<&'a [f32; QUANTUM]>,
    pub detune: Option<&'a [f32; QUANTUM]>,
}

/// Blink's BiquadFilterNode on a mono input: its four params and one [`Biquad`].
pub struct BiquadFilterNode {
    pub frequency: AudioParam,
    pub q: AudioParam,
    pub gain: AudioParam,
    pub detune: AudioParam,
    kind: FilterType,
    nyquist: f64,
    biquad: Biquad,
    has_just_reset: bool,
    previous: [f32; 4],
    frequency_values: [f32; QUANTUM],
    q_values: [f32; QUANTUM],
    gain_values: [f32; QUANTUM],
    detune_values: [f32; QUANTUM],
}

impl BiquadFilterNode {
    /// `createBiquadFilter()` with its default params (350 Hz, Q 1, 0 dB, 0 cents) and `kind`.
    pub fn new(sample_rate: f32, kind: FilterType) -> Self {
        let rate = sample_rate as f64;
        BiquadFilterNode {
            frequency: AudioParam::new(rate, 350.0, 0.0, sample_rate / 2.0, Rate::A),
            q: AudioParam::new(rate, 1.0, f32::MIN, f32::MAX, Rate::A),
            gain: AudioParam::new(rate, 0.0, f32::MIN, 40.0 * f32::MAX.log10(), Rate::A),
            detune: AudioParam::new(rate, 0.0, -1200.0 * f32::MAX.log2(), 1200.0 * f32::MAX.log2(), Rate::A),
            kind,
            nyquist: 0.5 * rate,
            biquad: Biquad::new(),
            has_just_reset: true,
            previous: [f32::NAN; 4],
            frequency_values: [0.0; QUANTUM],
            q_values: [0.0; QUANTUM],
            gain_values: [0.0; QUANTUM],
            detune_values: [0.0; QUANTUM],
        }
    }

    pub fn kind(&self) -> FilterType {
        self.kind
    }

    /// The `type` setter: a new type clears the filter memory and snaps the coefficients.
    pub fn set_kind(&mut self, kind: FilterType) {
        if kind != self.kind {
            self.kind = kind;
            self.biquad.reset();
            self.has_just_reset = true;
        }
    }

    /// Blink's `NormalizeFrequency`: hertz to 0..1 of Nyquist, times 2^(detune/1200). The detune
    /// factor is the platform's float `exp2`, as Blink calls it (no production path detunes a filter).
    fn normalize_frequency(&self, frequency: f32, detune: f32) -> f64 {
        let normalized = frequency as f64 / self.nyquist;
        if detune != 0.0 {
            normalized * (detune / 1200.0).exp2() as f64
        } else {
            normalized
        }
    }

    fn set_params(&mut self, index: usize, frequency: f32, q: f32, detune: f32) {
        let normalized = self.normalize_frequency(frequency, detune);
        match self.kind {
            FilterType::Lowpass => self.biquad.set_lowpass_params(index, normalized, q as f64),
            FilterType::Highpass => self.biquad.set_highpass_params(index, normalized, q as f64),
        }
    }

    /// The parameter half of Blink's `Process` for the quantum at `quantum_start`: read the params
    /// (plus `inputs`), and recompute the coefficients if they changed.
    pub fn begin_quantum(&mut self, quantum_start: u64, inputs: ParamInputs) {
        let q0 = quantum_start;
        let sample_accurate = self.frequency.has_sample_accurate_values(q0)
            || self.q.has_sample_accurate_values(q0)
            || self.gain.has_sample_accurate_values(q0)
            || self.detune.has_sample_accurate_values(q0);
        let mut dirty = false;
        let mut audio_rate = false;
        if sample_accurate {
            dirty = true;
            audio_rate = [&self.frequency, &self.q, &self.gain, &self.detune].iter().any(|p| p.rate() == Rate::A);
        } else if self.has_just_reset {
            self.previous = [f32::NAN; 4];
            dirty = true;
            self.has_just_reset = false;
        } else {
            let finals = [
                self.frequency.final_value(q0, inputs.frequency.map(|v| v[0])),
                self.q.final_value(q0, inputs.q.map(|v| v[0])),
                self.gain.final_value(q0, inputs.gain.map(|v| v[0])),
                self.detune.final_value(q0, inputs.detune.map(|v| v[0])),
            ];
            // NaN never equals itself, so the first read after a reset is always a change.
            if finals.iter().zip(&self.previous).any(|(a, b)| a != b) {
                dirty = true;
                self.previous = finals;
            }
        }
        if !dirty {
            return;
        }
        if sample_accurate && audio_rate {
            self.frequency.calculate_sample_accurate_values(q0, &mut self.frequency_values, inputs.frequency);
            self.q.calculate_sample_accurate_values(q0, &mut self.q_values, inputs.q);
            self.gain.calculate_sample_accurate_values(q0, &mut self.gain_values, inputs.gain);
            self.detune.calculate_sample_accurate_values(q0, &mut self.detune_values, inputs.detune);
            let constant = has_constant_values(&self.frequency_values)
                && has_constant_values(&self.q_values)
                && has_constant_values(&self.gain_values)
                && has_constant_values(&self.detune_values);
            let needed = if constant { 1 } else { QUANTUM };
            self.biquad.set_has_sample_accurate_values(needed > 1);
            for k in 0..needed {
                self.set_params(k, self.frequency_values[k], self.q_values[k], self.detune_values[k]);
            }
        } else {
            // Blink reads the final values again here (the k-rate branch read them once already).
            let frequency = self.frequency.final_value(q0, inputs.frequency.map(|v| v[0]));
            let q = self.q.final_value(q0, inputs.q.map(|v| v[0]));
            self.gain.final_value(q0, inputs.gain.map(|v| v[0]));
            let detune = self.detune.final_value(q0, inputs.detune.map(|v| v[0]));
            self.biquad.set_has_sample_accurate_values(false);
            self.set_params(0, frequency, q, detune);
        }
    }

    /// Filter frames `at..at + source.len()` of the quantum [`BiquadFilterNode::begin_quantum`] set up.
    pub fn process(&mut self, at: usize, source: &[f32], dest: &mut [f32]) {
        self.biquad.process(at, source, dest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    /// The magnitude response of the index-0 coefficients at `f` hertz.
    fn magnitude(b: &Biquad, f: f64) -> f64 {
        let w = std::f64::consts::PI * f / (RATE as f64 / 2.0);
        let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
        let num = (b.b0[0] + b.b1[0] * c1 + b.b2[0] * c2, b.b1[0] * s1 + b.b2[0] * s2);
        let den = (1.0 + b.a1[0] * c1 + b.a2[0] * c2, b.a1[0] * s1 + b.a2[0] * s2);
        (num.0.hypot(num.1)) / (den.0.hypot(den.1))
    }

    #[test]
    fn lowpass_and_highpass_follow_the_audio_eq_cookbook_with_q_in_db() {
        let mut b = Biquad::new();
        b.set_lowpass_params(0, 1000.0 / 24000.0, 0.0);
        assert!((magnitude(&b, 0.0) - 1.0).abs() < 1e-9);
        // Q of 0 dB is a linear Q of 1: the response at the cutoff is exactly Q.
        assert!((magnitude(&b, 1000.0) - 1.0).abs() < 1e-6);
        b.set_lowpass_params(0, 1000.0 / 24000.0, 6.0);
        assert!((magnitude(&b, 1000.0) - 10f64.powf(0.3)).abs() < 1e-5);
        b.set_highpass_params(0, 1000.0 / 24000.0, 0.0);
        assert!((magnitude(&b, 23999.0) - 1.0).abs() < 1e-3 && magnitude(&b, 10.0) < 1e-3);
        // The edges.
        b.set_lowpass_params(0, 0.0, 0.0);
        assert_eq!([b.b0[0], b.a1[0]], [0.0, 0.0]);
        b.set_highpass_params(0, 0.0, 0.0);
        assert_eq!(b.b0[0], 1.0);
        b.set_lowpass_params(0, 1.5, 0.0);
        assert_eq!(b.b0[0], 1.0);
    }

    #[test]
    fn automated_params_give_per_frame_coefficients_and_constant_ones_a_single_set() {
        let mut node = BiquadFilterNode::new(RATE, FilterType::Lowpass);
        let mut ramp = [0.0f32; QUANTUM];
        for (k, v) in ramp.iter_mut().enumerate() {
            *v = 500.0 + k as f32;
        }
        node.frequency.set_connected(true);
        node.begin_quantum(0, ParamInputs { frequency: Some(&ramp), ..Default::default() });
        assert!(node.biquad.has_sample_accurate_values());
        let flat = [800.0f32; QUANTUM];
        node.begin_quantum(128, ParamInputs { frequency: Some(&flat), ..Default::default() });
        assert!(!node.biquad.has_sample_accurate_values());
        // A connected signal sums onto the intrinsic value: nothing was scheduled, so the default
        // 350 Hz stays under the 800 Hz signal.
        let mut want = Biquad::new();
        want.set_lowpass_params(0, (350.0 + 800.0) / 24000.0, 1.0);
        assert_eq!([node.biquad.b0[0], node.biquad.a1[0]], [want.b0[0], want.a1[0]]);
    }

    #[test]
    fn any_split_of_a_quantum_renders_the_same() {
        let input: Vec<f32> = (0..QUANTUM).map(|k| ((k * 37 % 19) as f32 - 9.0) / 9.0).collect();
        let render = |split: usize| {
            let mut node = BiquadFilterNode::new(RATE, FilterType::Highpass);
            node.begin_quantum(0, ParamInputs::default());
            let mut out = vec![0.0f32; QUANTUM];
            node.process(0, &input[..split], &mut out[..split]);
            node.process(split, &input[split..], &mut out[split..]);
            out
        };
        let whole = render(QUANTUM);
        for split in [1, 5, 64, 127] {
            assert_eq!(render(split), whole);
        }
    }
}
