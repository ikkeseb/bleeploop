//! The master OTT: a three-band upward and downward compressor on the master bus, in the spirit of
//! Ableton's OTT but simpler (one AMOUNT, fixed times). The engine's own design: no Tone or Blink
//! reference, so its tests (`tests/ott.rs`) pin its own properties instead of a render.
//!
//! The split: Linkwitz-Riley 4th-order crossovers (two cascaded 2nd-order Butterworth sections, Q =
//! 1/√2) at 88 Hz and 2500 Hz. Low = LP88², the rest = HP88²; mid = LP2500²(rest), high =
//! HP2500²(rest); the low band also passes a 2nd-order allpass at 2500 Hz (Q = 1/√2), which is what
//! LP2500² + HP2500² sum to. So with every band at unity the three bands sum to AP88 · AP2500 of the
//! input: flat in magnitude, only its phase turned. The coefficients are the bilinear transform
//! prewarped at each crossover (the cookbook forms), so the identity holds for the digital filters too.
//!
//! Each band has a stereo-linked peak follower (the larger of |left| and |right|; attack while the band
//! is above it, release otherwise) and a static curve in dB ([`gain_db`]): 4:1 down above −24 dBFS with
//! a 6 dB soft knee, unity from −36 to −27, 3:1 up below −36 capped at +12 dB, the lift faded out
//! between −48 and −60 dBFS so hiss and room tone between notes are not raised. The followers attack
//! in 1 ms and release slowly: the gain falls fast and rises slowly, so an onset after a quiet passage
//! never rides the lift the quiet left. AMOUNT scales the
//! curve and the makeup together and glides (about 20 ms) so a move never zippers.
//!
//! On and off crossfade dry and processed linearly over 10 ms from the command's frame; a switch inside
//! a fade turns from the weight reached. Once off and settled the bus passes untouched, bit for bit. The
//! processor always runs, so a switch on meets warm filters and envelopes. Every state is per frame and
//! a setting applies on its frame, so any block split renders the same bits. No lookahead: no latency.
//!
//! The crossfade passes a momentary null where dry and AP88 · AP2500 of it are out of phase (near 85 Hz
//! and 2.6 kHz); a longer fade only lengthens it. Its depth is measured in `tests/ott.rs`.

use std::f64::consts::{FRAC_1_SQRT_2, LN_10, PI};

/// The crossovers, Hz (OTT's defaults).
pub const LOW_SPLIT: f64 = 88.0;
pub const HIGH_SPLIT: f64 = 2500.0;
/// The detectors' time constants, seconds: one fast attack for every band, a release per band (low,
/// mid, high). The gain may fall fast and rise slowly: with the 13–48 ms attacks this replaced, a
/// plucked onset passed at the upward gain the quiet before it had left (measured +11.1 dB overshoot
/// before the limiter on a quiet-to-loud pluck at AMOUNT 1, +2.1 dBFS at the device on a five-lane
/// jam); at 1 ms the lift collapses within the onset (−0.1 dB on the same pluck, before the makeup
/// was re-calibrated). A separate fast detector for the upward side alone, the slow ones kept for the
/// downward side, was measured too and rejected: it kept the low-E THD at −57 dB, but the onset met
/// the makeup with no downward gain yet (+3.6 dB on that pluck at AMOUNT 1, over the +3 dB bar).
const ATTACK: f64 = 0.001;
const RELEASE: [f64; 3] = [0.282, 0.282, 0.132];
/// The static curve, dBFS and ratios.
const DOWN_THRESHOLD: f64 = -24.0;
const DOWN_RATIO: f64 = 4.0;
const KNEE: f64 = 6.0;
const UP_THRESHOLD: f64 = -36.0;
const UP_RATIO: f64 = 3.0;
const UP_MAX: f64 = 12.0;
/// The upward lift is scaled from 0 at `FLOOR` to 1 at `FLOOR + FLOOR_RAMP`, on the detector's level.
/// A peak follower on noise reads well above its RMS, so the floor sits high: white hiss at −66 dBFS
/// RMS now leaves raised by the makeup alone (with the floor at −75..−60 and an +18 dB cap it got
/// +15.8 dB at AMOUNT 1). With the +12 dB cap the curve's largest lift is +8 dB, at −48 dBFS: below
/// −54 the cap times the fade, above it the 3:1 lift is under the cap.
const FLOOR: f64 = -60.0;
const FLOOR_RAMP: f64 = 12.0;
/// The makeup, dB, scaled by AMOUNT with the curve. Set by what the master can carry, not by loudness
/// parity: pink noise at −18 dBFS RMS (`tests/ott.rs` `measurements`: fixed-seed pink noise,
/// 20 Hz–20 kHz, both sides identical, RMS over seconds 2–12 at 48 kHz, before the limiter) needs
/// +5.6 dB to leave at AMOUNT 1 at the RMS it came in at, but at +5.6 the five-lane jam's peak reached
/// +0.1 dBFS at the device; the peak moves about 1:1 with the makeup, and the crossovers' phase alone
/// (on at AMOUNT 0) already lifts the jam's pre-limiter peak 2.6 dB. +3.0 keeps the device below
/// −1 dBFS there, at the cost of steady pink leaving about 2.6 dB quieter than it came in.
pub const MAKEUP_DB: f64 = 3.0;
/// AMOUNT's default and its glide's time constant.
pub const DEFAULT_AMOUNT: f32 = 0.5;
const AMOUNT_TAU: f64 = 0.020;
/// The on/off crossfade, seconds.
pub const FADE_SECONDS: f64 = 0.010;
/// The envelope's floor for the level: −180 dBFS.
const LEVEL_FLOOR: f64 = 1e-9;
/// Filter and envelope state below this is flushed to zero: a decay into digital silence would
/// otherwise reach the subnormal range (seconds after the input stops) and slow every frame down.
const TINY: f64 = 1e-30;

/// The static gain curve, dB, of a band whose envelope reads `level` dBFS: before AMOUNT and the makeup.
pub fn gain_db(level: f64) -> f64 {
    let knee_lo = DOWN_THRESHOLD - KNEE / 2.0;
    let knee_hi = DOWN_THRESHOLD + KNEE / 2.0;
    if level >= knee_hi {
        (DOWN_THRESHOLD - level) * (1.0 - 1.0 / DOWN_RATIO)
    } else if level > knee_lo {
        // The quadratic knee: zero slope at its foot, the ratio's slope at its top.
        let x = level - knee_lo;
        -(1.0 - 1.0 / DOWN_RATIO) * x * x / (2.0 * KNEE)
    } else if level >= UP_THRESHOLD {
        0.0
    } else {
        let lift = ((UP_THRESHOLD - level) * (1.0 - 1.0 / UP_RATIO)).min(UP_MAX);
        lift * ((level - FLOOR) / FLOOR_RAMP).clamp(0.0, 1.0)
    }
}

/// A 2nd-order section's coefficients, normalised by a0.
#[derive(Clone, Copy)]
struct Coefs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy)]
enum Kind {
    Lowpass,
    Highpass,
    Allpass,
}

impl Coefs {
    /// The cookbook section at `f0`, Q = 1/√2: the bilinear transform prewarped at `f0`.
    fn new(kind: Kind, f0: f64, sample_rate: f64) -> Coefs {
        let w0 = 2.0 * PI * f0 / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = match kind {
            Kind::Lowpass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            Kind::Highpass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
            Kind::Allpass => (1.0 - alpha, -2.0 * cos, 1.0 + alpha),
        };
        Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0 }
    }

    /// One frame through the section (transposed direct form II), its state flushed near zero.
    #[inline(always)]
    fn tick(&self, s: &mut [f64; 2], x: f64) -> f64 {
        let y = self.b0 * x + s[0];
        s[0] = self.b1 * x - self.a1 * y + s[1];
        s[1] = self.b2 * x - self.a2 * y;
        if s[0].abs() < TINY {
            s[0] = 0.0;
        }
        if s[1].abs() < TINY {
            s[1] = 0.0;
        }
        y
    }

    /// Two identical sections in cascade: the Linkwitz-Riley 4th order.
    #[inline(always)]
    fn twice(&self, s: &mut [[f64; 2]; 2], x: f64) -> f64 {
        let y = self.tick(&mut s[0], x);
        self.tick(&mut s[1], y)
    }
}

/// One side's filter state: two sections each of LP88, HP88, LP2500, HP2500, and the low band's AP2500.
#[derive(Clone, Copy, Default)]
struct Side {
    lp_lo: [[f64; 2]; 2],
    hp_lo: [[f64; 2]; 2],
    lp_hi: [[f64; 2]; 2],
    hp_hi: [[f64; 2]; 2],
    ap_hi: [f64; 2],
}

pub struct Ott {
    lp_lo: Coefs,
    hp_lo: Coefs,
    lp_hi: Coefs,
    hp_hi: Coefs,
    ap_hi: Coefs,
    sides: [Side; 2],
    env: [f64; 3],
    attack: f64,
    release: [f64; 3],
    amount: f64,
    amount_target: f64,
    amount_coef: f64,
    on: bool,
    /// The processed signal's share of the output: 0 is the literal dry bus.
    weight: f64,
    step: f64,
    /// The last frame's bands (low, mid, high; left, right) and the linear gain each got: what a
    /// measurement reads ([`Ott::bands`], [`Ott::gains`]).
    bands: [[f64; 2]; 3],
    gains: [f64; 3],
}

impl Ott {
    /// Off, AMOUNT at its default, every coefficient computed for `sample_rate` here.
    pub fn new(sample_rate: u32) -> Ott {
        let sr = sample_rate as f64;
        let coef = |tau: f64| (-1.0 / (tau * sr)).exp();
        Ott {
            lp_lo: Coefs::new(Kind::Lowpass, LOW_SPLIT, sr),
            hp_lo: Coefs::new(Kind::Highpass, LOW_SPLIT, sr),
            lp_hi: Coefs::new(Kind::Lowpass, HIGH_SPLIT, sr),
            hp_hi: Coefs::new(Kind::Highpass, HIGH_SPLIT, sr),
            ap_hi: Coefs::new(Kind::Allpass, HIGH_SPLIT, sr),
            sides: [Side::default(); 2],
            env: [0.0; 3],
            attack: coef(ATTACK),
            release: RELEASE.map(coef),
            amount: DEFAULT_AMOUNT as f64,
            amount_target: DEFAULT_AMOUNT as f64,
            amount_coef: coef(AMOUNT_TAU),
            on: false,
            weight: 0.0,
            step: 1.0 / (FADE_SECONDS * sr),
            bands: [[0.0; 2]; 3],
            gains: [1.0; 3],
        }
    }

    /// On or off, crossfading from the next frame processed; a switch mid-fade turns from where it is.
    pub fn set_on(&mut self, on: bool) {
        self.on = on;
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// AMOUNT, 0..1 (clamped), glided to from the next frame. A value that is no number is ignored.
    pub fn set_amount(&mut self, amount: f32) {
        if amount.is_finite() {
            self.amount_target = amount.clamp(0.0, 1.0) as f64;
        }
    }

    /// AMOUNT as set (its glide's target).
    pub fn amount(&self) -> f32 {
        self.amount_target as f32
    }

    /// The processed signal's share of the output, 0..1: 0 off and settled, 1 on and settled.
    pub fn weight(&self) -> f64 {
        self.weight
    }

    /// The last frame's bands, low, mid and high, each `[left, right]`, before their gains.
    pub fn bands(&self) -> [[f64; 2]; 3] {
        self.bands
    }

    /// The last frame's linear gain per band (AMOUNT and the makeup applied).
    pub fn gains(&self) -> [f64; 3] {
        self.gains
    }

    /// The bus in place, frame by frame. Off and settled, every sample is left as it was.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let target = if self.on { 1.0 } else { 0.0 };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let dry = [*l as f64, *r as f64];
            let mut band = [[0.0f64; 2]; 3];
            for (c, &x) in dry.iter().enumerate() {
                let s = &mut self.sides[c];
                let low = self.lp_lo.twice(&mut s.lp_lo, x);
                let rest = self.hp_lo.twice(&mut s.hp_lo, x);
                band[0][c] = self.ap_hi.tick(&mut s.ap_hi, low);
                band[1][c] = self.lp_hi.twice(&mut s.lp_hi, rest);
                band[2][c] = self.hp_hi.twice(&mut s.hp_hi, rest);
            }
            self.amount = crate::glide(self.amount, self.amount_target, self.amount_coef);
            let mut wet = [0.0f64; 2];
            for b in 0..3 {
                let x = band[b][0].abs().max(band[b][1].abs());
                let coef = if x > self.env[b] { self.attack } else { self.release[b] };
                let mut env = x + (self.env[b] - x) * coef;
                if env < TINY {
                    env = 0.0;
                }
                self.env[b] = env;
                let level = 20.0 * env.max(LEVEL_FLOOR).log10();
                // dB to linear: exp(dB · ln 10 / 20). AMOUNT 0 gives exp(0), exactly 1.
                let g = (self.amount * (gain_db(level) + MAKEUP_DB) * (LN_10 / 20.0)).exp();
                self.gains[b] = g;
                wet[0] += band[b][0] * g;
                wet[1] += band[b][1] * g;
            }
            self.bands = band;
            self.weight = if self.on { (self.weight + self.step).min(target) } else { (self.weight - self.step).max(target) };
            let w = self.weight;
            if w == 0.0 {
                // Off and settled: the bus as it came, bit for bit.
                continue;
            }
            if w == 1.0 {
                *l = wet[0] as f32;
                *r = wet[1] as f32;
            } else {
                *l = (dry[0] + w * (wet[0] - dry[0])) as f32;
                *r = (dry[1] + w * (wet[1] - dry[1])) as f32;
            }
        }
    }
}
