//! The input OTT: a three-band upward and downward compressor on what the player plays, in the spirit of
//! Ableton's OTT but simpler (DEPTH, TIME, UP and DOWN). It sits on each source's own sound, before that
//! sound splits into what is heard and what is recorded, so the monitor and the loop carry the same
//! processed signal, only shifted in time: one mono instance on each plugin slot's output after its gain
//! (`slots`: an amp-sim, a plugin synth, an empty live slot's dry input), one stereo instance on the
//! built-in instruments' sum after their levels (`instruments`). One setting drives every instance from
//! the same frame (`Command::SetInputSend` with `InputSend::Ott`, the `Ott*` `InputSendParam`s). The
//! loops, the click and the reverb bus never pass it. The engine's own design: no Tone or Blink
//! reference, so its tests (`tests/ott.rs`) pin its own properties instead of a render.
//!
//! The split: Linkwitz-Riley 4th-order crossovers (two cascaded 2nd-order Butterworth sections, Q =
//! 1/√2) at 88 Hz and 2500 Hz. Low = LP88², the rest = HP88²; mid = LP2500²(rest), high =
//! HP2500²(rest); the low band also passes a 2nd-order allpass at 2500 Hz (Q = 1/√2), which is what
//! LP2500² + HP2500² sum to. So with every band at unity the three bands sum to AP88 · AP2500 of the
//! input: flat in magnitude, only its phase turned. The coefficients are the bilinear transform
//! prewarped at each crossover (the cookbook forms), so the identity holds for the digital filters too.
//!
//! Each band has a peak follower linked across the instance's channels (the larger of |left| and |right|
//! on the stereo one; attack while the band is above it, release otherwise) and a static curve in dB
//! ([`gain_db`]): 4:1 down above −24 dBFS with a 6 dB soft knee, unity from −36 to −27, 3:1 up below −36
//! capped at +12 dB, the lift faded out between −48 and −60 dBFS so hiss and room tone between notes are
//! not raised. The followers attack in 1 ms and release slowly: the gain falls fast and rises slowly, so
//! an onset after a quiet passage never rides the lift the quiet left. TIME scales every band's release
//! by 4^(2·TIME − 1) (a quarter to four times, exactly 1 at its default 0.5); the attack stays. A band's
//! gain is DEPTH · (UP · the curve's lift + DOWN · its cut + the makeup), in dB: UP and DOWN scale the
//! curve's upward and downward parts, DEPTH all of it with the makeup, and DEPTH 0 is unity exactly.
//! DEPTH, UP and DOWN glide (about 20 ms) so a move never zippers; TIME moves an envelope's coefficient,
//! not the signal, and is taken on its frame.
//!
//! The processed signal then passes a soft ceiling ([`ceiling`]): untouched up to −3 dBFS, never
//! past −1.5 dBFS, so the makeup riding the crossovers' phase spikes on a hard onset can drive neither
//! the master limiter (which lets a fast spike through) past full scale nor the monitor, which has no
//! limiter. It is a function of the sample alone: no state, no lookahead.
//!
//! On and off crossfade dry and processed linearly over 10 ms from the command's frame; a switch inside
//! a fade turns from the weight reached. Once off and settled the signal passes untouched, bit for bit.
//! The processor always runs, so a switch on meets warm filters and envelopes. Every state is per frame
//! and a setting applies on its frame, so any block split renders the same bits. No lookahead: no added
//! latency samples; the crossovers' phase is a group delay of their own, on the lows above all (about
//! 4.6 ms at 100 Hz, 0.24 ms at 1 kHz, 0.06 ms at 5 kHz at 48 kHz: `tests/ott.rs`).
//!
//! The constants' calibrations below were measured where the OTT first sat, on the master bus, with
//! one control, AMOUNT (now DEPTH, with UP, DOWN and TIME at their defaults): the five-lane jam's
//! numbers come from there. At its place on the input, the hard square onset's peak at the device is
//! pinned again (`tests/ott.rs`).
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
/// before the limiter on a quiet-to-loud pluck at DEPTH 1, +2.1 dBFS at the device on a five-lane
/// jam); at 1 ms the lift collapses within the onset (−0.1 dB on the same pluck, before the makeup
/// was re-calibrated). A separate fast detector for the upward side alone, the slow ones kept for the
/// downward side, was measured too and rejected: it kept the low-E THD at −57 dB, but the onset met
/// the makeup with no downward gain yet (+3.6 dB on that pluck at DEPTH 1, over the +3 dB bar).
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
/// +15.8 dB at DEPTH 1). With the +12 dB cap the curve's largest lift is +8 dB, at −48 dBFS: below
/// −54 the cap times the fade, above it the 3:1 lift is under the cap.
const FLOOR: f64 = -60.0;
const FLOOR_RAMP: f64 = 12.0;
/// The makeup, dB, scaled by DEPTH with the curve. Set by what the master could carry, not by loudness
/// parity: pink noise at −18 dBFS RMS (`tests/ott.rs` `measurements`: fixed-seed pink noise,
/// 20 Hz–20 kHz, both sides identical, RMS over seconds 2–12 at 48 kHz, before the limiter) needs
/// +5.6 dB to leave at DEPTH 1 at the RMS it came in at, but at +5.6 the five-lane jam's peak reached
/// +0.1 dBFS at the device; the peak moves about 1:1 with the makeup, and the crossovers' phase alone
/// (on at DEPTH 0) already lifts the jam's pre-limiter peak 2.6 dB. +3.0 keeps the device below
/// −1 dBFS there, at the cost of steady pink leaving about 2.6 dB quieter than it came in.
pub const MAKEUP_DB: f64 = 3.0;
/// The controls' defaults (the UI's too, `InputSendParam::range`), and the glide's time constant.
pub const DEFAULT_DEPTH: f64 = 0.5;
pub const DEFAULT_TIME: f64 = 0.5;
pub const DEFAULT_UPWARD: f64 = 1.0;
pub const DEFAULT_DOWNWARD: f64 = 1.0;
const GLIDE_TAU: f64 = 0.020;
/// The on/off crossfade, seconds.
pub const FADE_SECONDS: f64 = 0.010;
/// The wet path's soft ceiling, linear: identity up to `CEILING_KNEE`, then a tanh curve that meets it
/// with slope 1 and no curvature and approaches `CEILING` without reaching it ([`ceiling`]). The
/// master limiter after it is a k-rate compressor with a 6 ms pre-delay, not a sample-peak brickwall,
/// and its own makeup lifts what it does not reduce by +0.57 dB: without the ceiling a square at ±0.8
/// out of silence (a plugin instrument, `tests/ott.rs` `measurements`) left the device at +1.05 dBFS
/// at DEPTH 1 (200 Hz; +0.45 at 100 Hz), the crossovers' phase spikes at its edges riding the makeup
/// past the limiter. With the bus held under −1.5 dBFS the device stays at or under −0.93 in every
/// such case, which is under the same square with the OTT off plus 0.5 dB (−1.37 + 0.5); a ceiling at
/// −1 dBFS left it at −0.43, 0.94 dB over that. The knee sits at the top of what music reaches: the
/// five-lane jam at DEPTH 1 peaks at −2.45 dBFS before the limiter, its device peak (−1.88 dBFS) and
/// RMS (−27.27) moved under a tenth of a dB; a low E at −12 dBFS (THD −53.9 dB) and −66 dBFS hiss
/// (+3.0 dB) never reach it. A sustained tone hotter than the knee is shaped only at a low DEPTH: the
/// downward curve takes DEPTH × 17 dB off one at −1 dBFS, enough to pass under the knee from DEPTH
/// ~0.15 up; below that it leaves soft-clipped (a 200 Hz sine at −1.1 dBFS, DEPTH 0: ~2.6 % THD).
pub const CEILING: f64 = 0.841_395_141_645_195_1; // −1.5 dBFS
pub const CEILING_KNEE: f64 = 0.707_945_784_384_137_9; // −3 dBFS
/// The envelope's floor for the level: −180 dBFS.
const LEVEL_FLOOR: f64 = 1e-9;
/// Filter and envelope state below this is flushed to zero: a decay into digital silence would
/// otherwise reach the subnormal range (seconds after the input stops) and slow every frame down.
const TINY: f64 = 1e-30;

/// The static gain curve, dB, of a band whose envelope reads `level` dBFS: before DEPTH and the makeup.
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

/// The wet path's soft ceiling on one sample: `x` up to `CEILING_KNEE` in magnitude, then
/// `knee + (CEILING − knee) · tanh((|x| − knee) / (CEILING − knee))` with `x`'s sign. Stateless, so it
/// changes nothing about block-size identity.
pub fn ceiling(x: f64) -> f64 {
    let a = x.abs();
    if a <= CEILING_KNEE {
        return x;
    }
    let room = CEILING - CEILING_KNEE;
    (CEILING_KNEE + room * ((a - CEILING_KNEE) / room).tanh()).copysign(x)
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

/// One channel's filter state: two sections each of LP88, HP88, LP2500, HP2500, and the low band's AP2500.
#[derive(Clone, Copy, Default)]
struct Side {
    lp_lo: [[f64; 2]; 2],
    hp_lo: [[f64; 2]; 2],
    lp_hi: [[f64; 2]; 2],
    hp_hi: [[f64; 2]; 2],
    ap_hi: [f64; 2],
}

/// One change of the OTT's one setting, which the engine hands every instance on the same frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Setting {
    On(bool),
    /// DEPTH, 0..1: the curve and the makeup together (the old DEPTH).
    Depth(f64),
    /// TIME, 0..1: every band's release times [`release_scale`].
    Time(f64),
    /// UP, 0..1: the curve's upward part.
    Upward(f64),
    /// DOWN, 0..1: the curve's downward part.
    Downward(f64),
}

/// The release multiplier at TIME `time`: 4^(2·time − 1), a quarter at 0, 1 at 0.5 (exactly), 4 at 1.
pub fn release_scale(time: f64) -> f64 {
    4f64.powf(2.0 * time - 1.0)
}

/// A glided control: its value now and the target it heads for.
#[derive(Clone, Copy)]
struct Glided {
    value: f64,
    target: f64,
}

impl Glided {
    fn new(v: f64) -> Glided {
        Glided { value: v, target: v }
    }
}

/// The OTT over `C` channels, their detectors linked: [`MonoOtt`] on a plugin slot, [`StereoOtt`] on the
/// built-in instruments. One implementation; the channel count is the only difference.
pub struct Ott<const C: usize> {
    lp_lo: Coefs,
    hp_lo: Coefs,
    lp_hi: Coefs,
    hp_hi: Coefs,
    ap_hi: Coefs,
    sides: [Side; C],
    env: [f64; 3],
    sample_rate: f64,
    attack: f64,
    release: [f64; 3],
    time: f64,
    depth: Glided,
    up: Glided,
    down: Glided,
    glide_coef: f64,
    on: bool,
    /// The on/off fade's position, frames: 0 is the literal dry signal, `fade` the processed one alone.
    /// Counted, not accumulated, so the fade's last frame lands exactly on 0 or 1.
    pos: u32,
    fade: u32,
    /// The last frame's bands (low, mid, high; per channel) and the linear gain each got: what a
    /// measurement reads ([`Ott::bands`], [`Ott::gains`]).
    bands: [[f64; C]; 3],
    gains: [f64; 3],
}

pub type MonoOtt = Ott<1>;
pub type StereoOtt = Ott<2>;

impl<const C: usize> Ott<C> {
    /// Off, every control at its default, every coefficient computed for `sample_rate` here.
    pub fn new(sample_rate: u32) -> Ott<C> {
        Ott::build(sample_rate, false)
    }

    /// On and settled at every control's default (for [`crate::units::OttUnit`]).
    pub(crate) fn new_on(sample_rate: u32) -> Ott<C> {
        Ott::build(sample_rate, true)
    }

    fn build(sample_rate: u32, on: bool) -> Ott<C> {
        let sr = sample_rate as f64;
        let coef = |tau: f64| (-1.0 / (tau * sr)).exp();
        let fade = ((FADE_SECONDS * sr).round() as u32).max(1);
        let mut ott = Ott {
            lp_lo: Coefs::new(Kind::Lowpass, LOW_SPLIT, sr),
            hp_lo: Coefs::new(Kind::Highpass, LOW_SPLIT, sr),
            lp_hi: Coefs::new(Kind::Lowpass, HIGH_SPLIT, sr),
            hp_hi: Coefs::new(Kind::Highpass, HIGH_SPLIT, sr),
            ap_hi: Coefs::new(Kind::Allpass, HIGH_SPLIT, sr),
            sides: [Side::default(); C],
            env: [0.0; 3],
            sample_rate: sr,
            attack: coef(ATTACK),
            release: [0.0; 3],
            time: DEFAULT_TIME,
            depth: Glided::new(DEFAULT_DEPTH),
            up: Glided::new(DEFAULT_UPWARD),
            down: Glided::new(DEFAULT_DOWNWARD),
            glide_coef: coef(GLIDE_TAU),
            on,
            pos: if on { fade } else { 0 },
            fade,
            bands: [[0.0; C]; 3],
            gains: [1.0; 3],
        };
        ott.retime();
        ott
    }

    /// Set the starting on/off state settled, before any frame has rendered (an install that arrives
    /// bypassed).
    pub(crate) fn snap_on(&mut self, on: bool) {
        self.on = on;
        self.pos = if on { self.fade } else { 0 };
    }

    /// The release coefficients at the TIME set.
    fn retime(&mut self) {
        let scale = release_scale(self.time);
        for (r, tau) in self.release.iter_mut().zip(RELEASE) {
            *r = (-1.0 / (tau * scale * self.sample_rate)).exp();
        }
    }

    /// Apply `setting` from the next frame processed. On or off crossfades from there, a switch mid-fade
    /// turning from where it is. A control is clamped to 0..1, and one that is no number is ignored.
    /// DEPTH, UP and DOWN glide to their value; off and settled, where nothing hears the glide, they take
    /// it at once, so an OTT replayed into a new engine (its controls before its switch) comes on at
    /// them. TIME recomputes the release coefficients at once.
    pub fn set(&mut self, setting: Setting) {
        let settled_off = !self.on && self.pos == 0;
        let glided = |g: &mut Glided, v: f64| {
            if v.is_finite() {
                g.target = v.clamp(0.0, 1.0);
                if settled_off {
                    g.value = g.target;
                }
            }
        };
        match setting {
            Setting::On(on) => self.on = on,
            Setting::Depth(v) => glided(&mut self.depth, v),
            Setting::Upward(v) => glided(&mut self.up, v),
            Setting::Downward(v) => glided(&mut self.down, v),
            Setting::Time(v) => {
                if v.is_finite() {
                    self.time = v.clamp(0.0, 1.0);
                    self.retime();
                }
            }
        }
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// DEPTH, TIME, UP and DOWN as set (a glide's target).
    pub fn controls(&self) -> [f64; 4] {
        [self.depth.target, self.time, self.up.target, self.down.target]
    }

    /// The processed signal's share of the output, 0..1: 0 off and settled, 1 on and settled.
    pub fn weight(&self) -> f64 {
        self.pos as f64 / self.fade as f64
    }

    /// The last frame's bands, low, mid and high, each per channel, before their gains.
    pub fn bands(&self) -> [[f64; C]; 3] {
        self.bands
    }

    /// The last frame's linear gain per band (the controls and the makeup applied).
    pub fn gains(&self) -> [f64; 3] {
        self.gains
    }

    /// The last frame's linked peak follower per band, linear amplitude.
    pub fn env(&self) -> [f64; 3] {
        self.env
    }

    /// One frame of every channel. Off and settled, it comes back as it went in, bit for bit.
    #[inline]
    pub fn frame(&mut self, x: [f32; C]) -> [f32; C] {
        let dry = x.map(|v| v as f64);
        let mut band = [[0.0f64; C]; 3];
        for (c, &x) in dry.iter().enumerate() {
            let s = &mut self.sides[c];
            let low = self.lp_lo.twice(&mut s.lp_lo, x);
            let rest = self.hp_lo.twice(&mut s.hp_lo, x);
            band[0][c] = self.ap_hi.tick(&mut s.ap_hi, low);
            band[1][c] = self.lp_hi.twice(&mut s.lp_hi, rest);
            band[2][c] = self.hp_hi.twice(&mut s.hp_hi, rest);
        }
        for g in [&mut self.depth, &mut self.up, &mut self.down] {
            g.value = crate::glide(g.value, g.target, self.glide_coef);
        }
        let (depth, up, down) = (self.depth.value, self.up.value, self.down.value);
        let mut wet = [0.0f64; C];
        for (b, band) in band.iter().enumerate() {
            let x = band.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            let coef = if x > self.env[b] { self.attack } else { self.release[b] };
            let mut env = x + (self.env[b] - x) * coef;
            if env < TINY {
                env = 0.0;
            }
            self.env[b] = env;
            let curve = gain_db(20.0 * env.max(LEVEL_FLOOR).log10());
            // At UP and DOWN 1 the shaped curve is the curve itself, bit for bit.
            let shaped = up * curve.max(0.0) + down * curve.min(0.0);
            // dB to linear: exp(dB · ln 10 / 20). DEPTH 0 gives exp(0), exactly 1.
            let g = (depth * (shaped + MAKEUP_DB) * (LN_10 / 20.0)).exp();
            self.gains[b] = g;
            for (w, &v) in wet.iter_mut().zip(band) {
                *w += v * g;
            }
        }
        self.bands = band;
        self.pos = if self.on { (self.pos + 1).min(self.fade) } else { self.pos.saturating_sub(1) };
        if self.pos == 0 {
            // Off and settled: the signal as it came, bit for bit.
            return x;
        }
        let wet = wet.map(ceiling);
        if self.pos == self.fade {
            wet.map(|v| v as f32)
        } else {
            let w = self.weight();
            std::array::from_fn(|c| (dry[c] + w * (wet[c] - dry[c])) as f32)
        }
    }
}

impl Ott<1> {
    /// A mono signal in place, frame by frame.
    pub fn process(&mut self, x: &mut [f32]) {
        for v in x.iter_mut() {
            [*v] = self.frame([*v]);
        }
    }
}

impl Ott<2> {
    /// A stereo signal in place, frame by frame.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            [*l, *r] = self.frame([*l, *r]);
        }
    }
}
