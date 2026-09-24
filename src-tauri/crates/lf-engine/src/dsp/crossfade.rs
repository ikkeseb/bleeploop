//! Tone's `CrossFade` (`component/channel/CrossFade.js`, Tone 15.1.22) as it builds on Blink: an
//! equal-power fade from input `a` to input `b` driven by the `fade` Signal (0 = all `a`).
//!
//! What Tone builds: `fade` → GainToAudio (a WaveShaperNode with a 1024-point curve of `|x|·2 − 1`)
//! → a StereoPannerNode's `pan`; the panner pans a constant 1 (Tone's looped 128-frame buffer of ones,
//! channelCount 1) and a ChannelSplitter sends its left gain into `a`'s GainNode gain and its right
//! gain into `b`'s. Both gains are Tone Gains built at 0, so each reads 0 plus the panner's output. The
//! ports: Blink's `WaveShaperCurveValues` (the x86 path, in float) from
//! `modules/webaudio/wave_shaper_handler.cc`, the mono branch of
//! `StereoPanner::PanWithSampleAccurateValues` from `platform/audio/stereo_panner.cc`, and the
//! GainNodes' sample-accurate product from `gain_handler.cc`, at Chromium 153.0.8010.12.
//! standardized-audio-context connects a looped two-frame buffer of zeros into a WaveShaper whose
//! curve is not zero at 0 (the DC fix); it adds nothing.
//!
//! The curve is sampled, not the ideal `|x|`: at fade 0 the shaper reads midway between curve points
//! 511 and 512, both `−1021/1023`, so the pan is −0.998 rather than −1 and `b` keeps a gain of
//! sin(0.000977·π/2) ≈ 0.00153 (−56 dB), with `a` at 0.9999988. At fade 1 the pan is 1 and `a` keeps
//! cos(π/2) ≈ 6e−17. So a bypassed Tone effect still mixes in its wet path at −56 dB; the port keeps
//! that, and the FX chain renders every wet path it leaks.
//!
//! Gains are computed once per quantum at its start; the mix runs frame by frame, so any block split
//! renders the same.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::fdlibm;
use super::filter::{connect_signal, Signal};
use super::param::{AudioParam, Rate, ToneParam, Units, QUANTUM};

/// GainToAudio's curve length (Tone's WaveShaper default).
const CURVE: usize = 1024;

/// Tone's CrossFade on mono inputs.
pub struct CrossFade {
    pub fade: Signal,
    curve: [f32; CURVE],
    pan: AudioParam,
    a: ToneParam,
    b: ToneParam,
    shaped: [f32; QUANTUM],
    pan_values: [f32; QUANTUM],
    left: [f32; QUANTUM],
    right: [f32; QUANTUM],
    gain_a: [f32; QUANTUM],
    gain_b: [f32; QUANTUM],
}

impl CrossFade {
    /// `new CrossFade({ fade })`, built while `frame` renders.
    pub fn new(sample_rate: f32, fade: f64, frame: u64) -> Self {
        let rate = sample_rate as f64;
        // GainToAudio's mapping, evaluated in double and stored as float (Tone's `setMap`).
        let curve = std::array::from_fn(|i| {
            let normalized = (i as f64 / (CURVE - 1) as f64) * 2.0 - 1.0;
            (normalized.abs() * 2.0 - 1.0) as f32
        });
        let gain = |frame| {
            let native = AudioParam::new(rate, 1.0, f32::MIN, f32::MAX, Rate::A);
            let mut p = ToneParam::new(native, Units::Gain, Some(0.0), frame);
            // The splitter connects with Tone's plain `connect`: the Gain's schedule stays.
            p.native.set_connected(true);
            p
        };
        let a = gain(frame);
        let b = gain(frame);
        let fade = Signal::new(rate, Units::NormalRange, true, fade, frame);
        let mut pan = AudioParam::new(rate, 0.0, -1.0, 1.0, Rate::A);
        connect_signal(&mut pan, frame);
        CrossFade {
            fade,
            curve,
            pan,
            a,
            b,
            shaped: [0.0; QUANTUM],
            pan_values: [0.0; QUANTUM],
            left: [0.0; QUANTUM],
            right: [0.0; QUANTUM],
            gain_a: [0.0; QUANTUM],
            gain_b: [0.0; QUANTUM],
        }
    }

    /// Blink's `WaveShaperCurveValues` on one value, in float as the x86 path computes it.
    fn shape(curve: &[f32; CURVE], input: f32) -> f32 {
        let max_index = (CURVE - 1) as i32;
        let virtual_index = ((input + 1.0) * (0.5 * (CURVE - 1) as f64) as f32).clamp(0.0, max_index as f32);
        let index1 = virtual_index as i32;
        let v1 = curve[index1.clamp(0, max_index) as usize];
        let v2 = curve[(index1 + 1).clamp(0, max_index) as usize];
        let f = virtual_index - index1 as f32;
        f * (v2 - v1) + v1
    }

    /// The panner's gains for one pan value: a constant 1 panned equal-power, left and right.
    fn pan_gains(pan: f32) -> (f32, f32) {
        let pan = (pan as f64).clamp(-1.0, 1.0);
        let radian = (pan * 0.5 + 0.5) * std::f64::consts::FRAC_PI_2;
        (fdlibm::cos(radian) as f32, fdlibm::sin(radian) as f32)
    }

    /// The gains of `a` and `b` for the quantum at `quantum_start`.
    pub fn begin_quantum(&mut self, quantum_start: u64) {
        let fade = *self.fade.render(quantum_start);
        if fade.iter().all(|&v| v == fade[0]) {
            // A fade holding still (every quantum but a ramp's) shapes and pans one value.
            self.shaped.fill(Self::shape(&self.curve, fade[0]));
        } else {
            for (s, &v) in self.shaped.iter_mut().zip(&fade) {
                *s = Self::shape(&self.curve, v);
            }
        }
        self.pan.calculate_sample_accurate_values(quantum_start, &mut self.pan_values, Some(&self.shaped));
        if self.pan_values.iter().all(|&v| v == self.pan_values[0]) {
            let (l, r) = Self::pan_gains(self.pan_values[0]);
            self.left.fill(l);
            self.right.fill(r);
        } else {
            for ((l, r), &p) in self.left.iter_mut().zip(self.right.iter_mut()).zip(&self.pan_values) {
                (*l, *r) = Self::pan_gains(p);
            }
        }
        self.a.native.calculate_sample_accurate_values(quantum_start, &mut self.gain_a, Some(&self.left));
        self.b.native.calculate_sample_accurate_values(quantum_start, &mut self.gain_b, Some(&self.right));
    }

    /// Mix frames `at..at + out.len()` of the current quantum: `a·gain_a + b·gain_b`. A missing input
    /// is a silent one (its GainNode outputs silence and the sum skips it).
    pub fn process(&self, at: usize, a: Option<&[f32]>, b: Option<&[f32]>, out: &mut [f32]) {
        let n = out.len();
        let (ga, gb) = (&self.gain_a[at..at + n], &self.gain_b[at..at + n]);
        match (a, b) {
            (Some(a), Some(b)) => {
                for i in 0..n {
                    out[i] = a[i] * ga[i] + b[i] * gb[i];
                }
            }
            (Some(a), None) => {
                for i in 0..n {
                    out[i] = a[i] * ga[i];
                }
            }
            (None, Some(b)) => {
                for i in 0..n {
                    out[i] = b[i] * gb[i];
                }
            }
            (None, None) => out.fill(0.0),
        }
    }

    /// `a`'s gain at frame `k` of the current quantum.
    pub fn gain_a(&self, k: usize) -> f32 {
        self.gain_a[k]
    }

    /// `b`'s gain at frame `k` of the current quantum.
    pub fn gain_b(&self, k: usize) -> f32 {
        self.gain_b[k]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resting_fade_leaks_the_other_input_through_the_sampled_curve() {
        let mut x = CrossFade::new(48000.0, 0.0, 0);
        x.begin_quantum(0);
        let pan = (-1021.0f64 / 1023.0) as f32 as f64;
        let radian = (pan * 0.5 + 0.5) * std::f64::consts::FRAC_PI_2;
        assert_eq!(x.gain_a(0), fdlibm::cos(radian) as f32);
        assert_eq!(x.gain_b(0), fdlibm::sin(radian) as f32);
        assert!((x.gain_b(0) - 0.0015355).abs() < 1e-6);
        let mut x = CrossFade::new(48000.0, 1.0, 0);
        x.begin_quantum(0);
        assert_eq!(x.gain_b(0), 1.0);
        assert!(x.gain_a(0) < 1e-16);
    }

    #[test]
    fn a_ramped_fade_is_equal_power_at_its_midpoint() {
        let mut x = CrossFade::new(48000.0, 0.0, 0);
        x.fade.ramp_to(1.0, 256.0 / 48000.0, 0.0, 0);
        x.begin_quantum(0);
        x.begin_quantum(128);
        // Frame 128 of a 256-frame ramp: fade 0.5, pan 0 on the curve's exact midpoint value.
        let (a, b) = (x.gain_a(0), x.gain_b(0));
        assert!((a * a + b * b - 1.0).abs() < 1e-6);
        assert!((a - b).abs() < 2e-3, "{a} {b}");
    }
}
