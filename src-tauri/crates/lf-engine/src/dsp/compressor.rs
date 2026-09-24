//! Blink's DynamicsCompressorNode, and the master limiter the live line builds from it
//! (`makeMasterLimiter` in `src/audio/engine.ts`: threshold −1 dB, knee 0, ratio 20, attack 3 ms,
//! release 50 ms). Ports `modules/webaudio/dynamics_compressor_handler.cc`,
//! `platform/audio/dynamics_compressor.{h,cc}` and the helpers they call in
//! `platform/audio/audio_utilities.cc` and `third_party/fdlibm/ieee754.cc`, at Chromium 153.0.8010.12.
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.
//!
//! The kernel, as Blink runs it: a stereo-linked peak detector on the undelayed input, a static curve
//! (linear to the threshold, a knee, then the ratio), an envelope that attacks at a rate set by the
//! largest compression step seen so far and releases along an adaptive polynomial, a sine warp on the
//! gain, the "makeup" post-gain Blink always applies, and a fixed 6 ms pre-delay on the audio path
//! (the lookahead). It is also Blink's start-up transient: the detector starts at 0 and the gain at 1,
//! so the first milliseconds dip before the detector settles; the reference renders have it.
//!
//! Control rates, anchored to the absolute frame so any block size renders the same bits: parameters
//! change only at a render-quantum boundary (a multiple of [`QUANTUM`], where Blink's handler reads its
//! k-rate params), and the envelope's target and rate are recomputed at every multiple of [`DIVISION`]
//! (Blink's 32-frame divisions, which start at each quantum's start).
//!
//! Dropped: the reduction meter (nothing reads it) and the mono-input upmix (the engine's master is
//! stereo). Not reproduced: Blink's audio thread runs with the CPU's flush-to-zero on, so every denormal
//! intermediate flushes; here only the state is flushed at each division's end (Blink's fallback
//! `FlushDenormalFloatToZero`). The gain and detector live far above the denormal range in practice.

// Blink's release-polynomial literals stay digit for digit as it writes them.
#![allow(clippy::excessive_precision)]

use super::param;
use crate::grid::Frame;

/// Frames per Web Audio render quantum ([`param::QUANTUM`]): where parameter changes land.
pub const QUANTUM: Frame = param::QUANTUM as Frame;
/// Frames per envelope division (`kNumberOfDivisionFrames`).
pub const DIVISION: Frame = 32;

const MAX_PRE_DELAY: usize = 1024;
const MASK: usize = MAX_PRE_DELAY - 1;
/// The lookahead Blink sets on every quantum (`kPreDelay`, seconds).
const PRE_DELAY: f32 = 0.006;
const SAT_RELEASE_TIME: f32 = 0.0025;
const UNINITIALIZED: f32 = -1.0;
const PI_OVER_TWO: f32 = std::f32::consts::FRAC_PI_2;

// The adaptive release polynomial's coefficients, evaluated in f32 in Blink's order.
const RELEASE_ZONE1: f32 = 0.09;
const RELEASE_ZONE2: f32 = 0.16;
const RELEASE_ZONE3: f32 = 0.42;
const RELEASE_ZONE4: f32 = 0.98;
const A_BASE: f32 = 0.999_999_999_999_999_8 * RELEASE_ZONE1 + 1.843_221_968_432_392_3e-16 * RELEASE_ZONE2
    - 1.937_339_435_167_642_3e-16 * RELEASE_ZONE3
    + 8.824_516_011_816_245e-18 * RELEASE_ZONE4;
const B_BASE: f32 = -1.578_832_035_284_588_8 * RELEASE_ZONE1 + 2.330_583_703_207_428_6 * RELEASE_ZONE2
    - 0.914_119_420_484_042_9 * RELEASE_ZONE3
    + 0.162_367_752_561_203_2 * RELEASE_ZONE4;
const C_BASE: f32 = 0.533_414_286_910_642_4 * RELEASE_ZONE1 - 1.272_736_789_213_631 * RELEASE_ZONE2
    + 0.925_885_604_220_751_2 * RELEASE_ZONE3
    - 0.186_563_101_917_762_26 * RELEASE_ZONE4;
const D_BASE: f32 = 0.087_834_631_382_072_34 * RELEASE_ZONE1 - 0.169_416_296_792_562_2 * RELEASE_ZONE2
    + 0.085_880_579_515_952_72 * RELEASE_ZONE3
    - 0.004_298_914_105_462_83 * RELEASE_ZONE4;
const E_BASE: f32 = -0.042_416_883_008_123_074 * RELEASE_ZONE1 + 0.111_569_382_798_760_2 * RELEASE_ZONE2
    - 0.097_646_763_252_658_72 * RELEASE_ZONE3
    + 0.028_494_263_462_021_576 * RELEASE_ZONE4;

/// The node's AudioParams, as the f32 values Blink reads (dB, dB, ratio, seconds, seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub threshold: f32,
    pub knee: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
}

impl Params {
    /// `makeMasterLimiter`: a hard-knee −1 dB limiter.
    pub const MASTER_LIMITER: Params = Params { threshold: -1.0, knee: 0.0, ratio: 20.0, attack: 0.003, release: 0.05 };
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(0.05 * db)
}

fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.log10()
}

fn ensure_finite(x: f32, default: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        default
    }
}

fn flush_denormal(x: f32) -> f32 {
    if x.abs() < f32::MIN_POSITIVE {
        0.0
    } else {
        x
    }
}

/// fdlibm's float wrappers compute in f64 and round once.
fn fdlibm_powf(x: f32, y: f32) -> f32 {
    (x as f64).powf(y as f64) as f32
}

/// The static curve (`UpdateStaticCurveParameters` and the functions it caches for).
struct Curve {
    ratio: f32,
    slope: f32,
    linear_threshold: f32,
    db_threshold: f32,
    db_knee: f32,
    knee_threshold: f32,
    db_knee_threshold: f32,
    db_yknee_threshold: f32,
    k: f32,
}

impl Curve {
    fn new() -> Self {
        let u = UNINITIALIZED;
        Curve {
            ratio: u,
            slope: u,
            linear_threshold: u,
            db_threshold: u,
            db_knee: u,
            knee_threshold: u,
            db_knee_threshold: u,
            db_yknee_threshold: u,
            k: u,
        }
    }

    fn knee_curve(&self, x: f32, k: f32) -> f32 {
        if x < self.linear_threshold {
            return x;
        }
        self.linear_threshold + (1.0 - ((-k * (x - self.linear_threshold)) as f64).exp() as f32) / k
    }

    fn saturate(&self, x: f32, k: f32) -> f32 {
        if x < self.knee_threshold {
            return self.knee_curve(x, k);
        }
        let db_x = linear_to_db(x);
        let db_y = self.db_yknee_threshold + self.slope * (db_x - self.db_knee_threshold);
        db_to_linear(db_y)
    }

    /// The knee's k whose dB slope at the knee's end is `desired_slope`: 15 bisection steps on a
    /// geometric mean, as Blink does it.
    fn k_at_slope(&self, desired_slope: f32) -> f32 {
        let db_x = self.db_threshold + self.db_knee;
        let x = db_to_linear(db_x);
        // `SlopeAt` is 1 below the threshold (Blink's `x < threshold`, so a NaN `x` takes the other branch).
        let linear = x < self.linear_threshold;
        let (mut x2, mut db_x2) = (1.0f32, 0.0f32);
        if !linear {
            x2 = (x as f64 * 1.001) as f32;
            db_x2 = linear_to_db(x2);
        }
        let (mut min_k, mut max_k, mut k) = (0.1f32, 10000.0f32, 5.0f32);
        let mut slope = 1.0f32;
        for _ in 0..15 {
            if !linear {
                let db_y = linear_to_db(self.knee_curve(x, k));
                let db_y2 = linear_to_db(self.knee_curve(x2, k));
                slope = (db_y2 - db_y) / (db_x2 - db_x);
            }
            if slope < desired_slope {
                max_k = k;
            } else {
                min_k = k;
            }
            k = (min_k * max_k).sqrt();
        }
        k
    }

    fn update(&mut self, db_threshold: f32, db_knee: f32, ratio: f32) -> f32 {
        if db_threshold != self.db_threshold || db_knee != self.db_knee || ratio != self.ratio {
            self.db_threshold = db_threshold;
            self.linear_threshold = db_to_linear(db_threshold);
            self.db_knee = db_knee;
            self.ratio = ratio;
            self.slope = 1.0 / ratio;
            let k = self.k_at_slope(1.0 / ratio);
            self.db_knee_threshold = db_threshold + db_knee;
            self.knee_threshold = db_to_linear(self.db_knee_threshold);
            self.db_yknee_threshold = linear_to_db(self.knee_curve(self.knee_threshold, k));
            self.k = k;
        }
        self.k
    }
}

/// A stereo-linked DynamicsCompressorNode. All memory is allocated in [`Compressor::new`].
pub struct Compressor {
    sample_rate: f32,
    params: Params,
    pending: Option<Params>,
    curve: Curve,
    // Per quantum: derived from the params.
    k: f32,
    linear_post_gain: f32,
    attack_frames: f32,
    sat_release_frames: f32,
    release: [f32; 5],
    // Per division: the envelope's target and rate.
    scaled_desired_gain: f32,
    envelope_rate: f32,
    // Per frame.
    detector_average: f32,
    compressor_gain: f32,
    db_max_attack_compression_diff: f32,
    pre_delay: Box<[[f32; MAX_PRE_DELAY]; 2]>,
    pre_delay_frames: usize,
    read: usize,
    write: usize,
}

impl Compressor {
    /// A compressor in Blink's reset state with `params` in force. The first frame it renders should be
    /// a quantum boundary, as a Web Audio node's is.
    pub fn new(sample_rate: f32, params: Params) -> Self {
        // SetPreDelayTime: the float product, truncated.
        let pre_delay_frames = ((PRE_DELAY * sample_rate) as u32).min(MAX_PRE_DELAY as u32 - 1) as usize;
        let mut c = Compressor {
            sample_rate,
            params,
            pending: None,
            curve: Curve::new(),
            k: 0.0,
            linear_post_gain: 0.0,
            attack_frames: 0.0,
            sat_release_frames: 0.0,
            release: [0.0; 5],
            scaled_desired_gain: 0.0,
            envelope_rate: 0.0,
            detector_average: 0.0,
            compressor_gain: 1.0,
            db_max_attack_compression_diff: -1.0,
            pre_delay: Box::new([[0.0; MAX_PRE_DELAY]; 2]),
            pre_delay_frames,
            read: 0,
            write: pre_delay_frames,
        };
        c.latch(params);
        // A render that starts off a division boundary still has an envelope to follow. Recomputing it
        // at the next boundary from the same state gives the same values.
        c.start_division();
        c
    }

    /// The master limiter at `sample_rate`.
    pub fn master_limiter(sample_rate: f32) -> Self {
        Compressor::new(sample_rate, Params::MASTER_LIMITER)
    }

    /// New params take effect at the next quantum boundary, as a k-rate AudioParam change does.
    pub fn set_params(&mut self, params: Params) {
        self.pending = Some(params);
    }

    pub fn params(&self) -> Params {
        self.params
    }

    /// The pre-delay: the output is the input this many frames late (`LatencyTime` in frames).
    pub fn latency(&self) -> usize {
        self.pre_delay_frames
    }

    /// Compress `left` and `right` in place; `frame` is the absolute frame of their first sample.
    pub fn process(&mut self, frame: Frame, left: &mut [f32], right: &mut [f32]) {
        debug_assert_eq!(left.len(), right.len());
        for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
            let f = frame + i as Frame;
            if f.rem_euclid(DIVISION) == 0 {
                if f.rem_euclid(QUANTUM) == 0 {
                    if let Some(p) = self.pending.take() {
                        self.latch(p);
                    }
                }
                self.start_division();
            }
            (*l, *r) = self.tick(*l, *r);
        }
    }

    /// The per-quantum work in `DynamicsCompressor::Process` before its division loop. Every value is a
    /// function of the params alone, so it runs only when they change.
    fn latch(&mut self, p: Params) {
        self.params = p;
        let sample_rate = self.sample_rate;
        let k = self.curve.update(p.threshold, p.knee, p.ratio);
        self.k = k;
        // Makeup gain with Blink's "empirical/perceptual tuning".
        self.linear_post_gain = fdlibm_powf(1.0 / self.curve.saturate(1.0, k), 0.6);
        self.attack_frames = p.attack.max(0.001) * sample_rate;
        let release_frames = sample_rate * p.release;
        self.sat_release_frames = SAT_RELEASE_TIME * sample_rate;
        self.release = [A_BASE, B_BASE, C_BASE, D_BASE, E_BASE].map(|base| release_frames * base);
    }

    /// The envelope's target and slew rate for the next division, from the detector as it stands.
    fn start_division(&mut self) {
        // The division before ended here: Blink flushes its state as it stores it back.
        self.detector_average = flush_denormal(self.detector_average);
        self.compressor_gain = flush_denormal(self.compressor_gain);

        self.detector_average = ensure_finite(self.detector_average, 1.0);
        let desired_gain = self.detector_average;
        // Pre-warp so the sine warp below lands on desired_gain.
        let scaled_desired_gain = (desired_gain as f64).asin() as f32 / PI_OVER_TWO;
        let is_releasing = scaled_desired_gain > self.compressor_gain;
        let mut db_compression_diff = if scaled_desired_gain == 0.0 {
            if is_releasing {
                -1.0
            } else {
                1.0
            }
        } else {
            linear_to_db(self.compressor_gain / scaled_desired_gain)
        };

        self.envelope_rate = if is_releasing {
            self.db_max_attack_compression_diff = -1.0;
            db_compression_diff = ensure_finite(db_compression_diff, -1.0);
            // Adaptive release: more compression releases faster. -12..0 dB maps to 0..3.
            let x = 0.25 * (db_compression_diff.clamp(-12.0, 0.0) + 12.0);
            let x2 = x * x;
            let x3 = x2 * x;
            let x4 = x2 * x2;
            let [a, b, c, d, e] = self.release;
            let calc_release_frames = a + b * x + c * x2 + d * x3 + e * x4;
            db_to_linear(5.0 / calc_release_frames)
        } else {
            db_compression_diff = ensure_finite(db_compression_diff, 1.0);
            // While attacking, the rate follows the largest step seen so far.
            if self.db_max_attack_compression_diff == -1.0 || self.db_max_attack_compression_diff < db_compression_diff {
                self.db_max_attack_compression_diff = db_compression_diff;
            }
            let db_eff_atten_diff = self.db_max_attack_compression_diff.max(0.5);
            let x = 0.25 / db_eff_atten_diff;
            1.0 - fdlibm_powf(x, 1.0 / self.attack_frames)
        };
        self.scaled_desired_gain = scaled_desired_gain;
    }

    /// One frame of the division loop.
    fn tick(&mut self, left: f32, right: f32) -> (f32, f32) {
        let k = self.k;
        let (w, rd) = (self.write, self.read);
        self.pre_delay[0][w] = left;
        self.pre_delay[1][w] = right;
        // The detector reads the undelayed input: the louder channel's magnitude.
        let mut abs_input = 0.0f32;
        for x in [left, right] {
            let a = if x > 0.0 { x } else { -x };
            if abs_input < a {
                abs_input = a;
            }
        }

        let shaped_input = self.curve.saturate(abs_input, k);
        let attenuation = if abs_input <= 0.0001 { 1.0 } else { shaped_input / abs_input };
        let db_attenuation = (-linear_to_db(attenuation)).max(2.0);
        let db_per_frame = db_attenuation / self.sat_release_frames;
        let sat_release_rate = db_to_linear(db_per_frame) - 1.0;
        let rate = if attenuation > self.detector_average { sat_release_rate } else { 1.0 };
        let mut detector_average = self.detector_average + (attenuation - self.detector_average) * rate;
        detector_average = detector_average.min(1.0);
        self.detector_average = ensure_finite(detector_average, 1.0);

        if self.envelope_rate < 1.0 {
            self.compressor_gain += (self.scaled_desired_gain - self.compressor_gain) * self.envelope_rate;
        } else {
            self.compressor_gain = (self.compressor_gain * self.envelope_rate).min(1.0);
        }

        // The sine warp smooths the exponential's corners; Blink evaluates it in f64.
        let post_warp_compressor_gain = ((PI_OVER_TWO * self.compressor_gain) as f64).sin() as f32;
        let total_gain = self.linear_post_gain * post_warp_compressor_gain;
        let out = (self.pre_delay[0][rd] * total_gain, self.pre_delay[1][rd] * total_gain);
        self.read = (rd + 1) & MASK;
        self.write = (w + 1) & MASK;
        out
    }
}
