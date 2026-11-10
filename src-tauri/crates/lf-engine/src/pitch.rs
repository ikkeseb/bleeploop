//! OWNS: the tuner's ear: the pitch of each plugin slot's raw input, read about twenty times a second
//! off the audio thread for the UI's note and cents. The audio thread only copies the slots' input
//! onto a ring; a host thread drains it into an [`Ear`] (one [`Detector`] per slot input, and which
//! one is shown), and the newest [`Reading`] goes to the UI. Everything is allocated in `new`; a push
//! allocates, locks and waits for nothing.
//!
//! A detector low-passes its input (an 8th-order Butterworth at 4 kHz), decimates it to about 12 kHz
//! (11.025 kHz from 44.1), keeps the last 2048 samples (171 ms, 186 ms from 44.1) and reads them every
//! 512 (43 ms, 46 ms). It analyses the newest span that holds one sound: a block 30 dB under the
//! window's loudest, or a 6 dB rise in energy (a pluck on a ringing string), starts the span, so an
//! onset or a reset is read from its own samples alone. On the span it computes McLeod's normalised
//! square difference function (MPM) through an FFT autocorrelation, oversampled four times between
//! lags by padding its spectrum, and picks the first key maximum within 0.9 of the highest among the
//! lags of the reportable pitches. It refines that period on the peaks nearest twice, four times... it,
//! out to the largest multiple within half the span, dividing each lag by its multiple: that is what
//! buys a hundredth of a cent on the high strings. A second refinement runs on the span through a
//! boxcar half a period long, which nulls the even partials and weakens the odd ones as 1/k: a
//! string's partials run sharp of harmonic (inharmonicity), and without it a low string reads up to
//! 5 cents sharp of its first partial (E1, measured), the pitch a player tunes by. A reading has a pitch only above
//! -60 dBFS peak, at a clarity (the NSDF at the period) of at least 0.93, and when the weighted
//! refinement agrees within 3 % and stays periodic (every peak it climbs at least 0.95): a hum or a
//! noise that would pull the reading ends it instead.
//!
//! Measured by `tests/pitch.rs` at 44.1, 48 and 96 kHz: steady harmonic tones from A0 to B6 (and
//! 35 cents either side) read within 0.11 cent, consecutive readings within 0.16 cent of each other,
//! and the same pitch from -50 to -3 dBFS peak within 0.02 cent; a plucked string (B = 1e-4, noise 50
//! dB down) reads within 2.0 cents of its first partial from 150 ms after the pluck, and within 3
//! cents from its first reading. An `Ear` at 48 kHz analyses 193 s of its two inputs per second of CPU
//! (the ignored `cost`, release). Limits, accepted rather than paid for:
//!
//! - **A reading is the window's centre:** a glide is followed 86 to 101 ms late. The first pitch
//!   after an onset needs three periods of span: 43 to 46 ms from A2 to E6 (B6 up to 85), 85 to 93
//!   ms for E2 and A1, up to 139 for E1 and B0, 171 for A0. A string plucked again reads within 3
//!   cents inside 140 ms; a tone that stops reads no pitch 7 to 54 ms later.
//! - **The tail of a note leaves the display early.** A pitch needs the span 0.93 periodic, so a
//!   string whose first partial falls 60 dB in 4 s, over noise 50 dB under its peak, reads for 2.2 to
//!   3.0 s; over mains hum 40 dB under its peak it stops before the hum pulls it a cent (at 1.0 to 1.25
//!   s on E2 and A2, pulled at most 0.81 cent), and reads nothing, not 50 Hz, once only the hum is
//!   left under -60 dBFS.
//! - **Lags under the range's top are never picked,** which is what lets a B6 whose fundamental is 20
//!   dB under its second harmonic read B6; a pitch above the range reads at a multiple of its period
//!   that falls inside it, an octave or more down (not measured).
//! - **Two unrelated pitches at once read nothing** (two equal tones a tritone and 30 cents apart: no
//!   pitched reading at any rate), rather than either.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// The lowest note a detector reads: A0. Readings reach half a semitone under it.
pub const MIN_HZ: f32 = 27.5;
/// The highest pitch a detector reads. Readings reach half a semitone over it.
pub const MAX_HZ: f32 = 2000.0;

/// One reading: the pitch found in the newest window, if any, and how loud and how periodic it was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// The pitch in hertz; 0.0 when there is none.
    pub hz: f32,
    /// The analysis window's RMS, linear: the decimated input after the anti-alias low-pass, its mean
    /// removed, over the samples the window holds since `new` or `reset` (all of it once full).
    pub level: f32,
    /// How periodic the analysed span is at the period picked: the NSDF there, at most 1. A pitch
    /// needs at least `CLARITY`.
    pub clarity: f32,
}

/// The analysis rate the input is decimated to, about: the factor is `round(rate / ANALYSIS_RATE)`.
const ANALYSIS_RATE: u32 = 12_000;
/// The analysis window in decimated samples: 171 ms at 12 kHz, 186 ms at 11.025 kHz.
const WINDOW: usize = 2048;
/// Decimated samples between readings: 23.4 readings a second at 12 kHz, 21.5 at 11.025 kHz.
const HOP: usize = 512;
/// Decimated samples kept before the window, for the weighting's first outputs: half the longest period.
const HISTORY: usize = 256;
const RING: usize = WINDOW + HISTORY;
/// Zero-padded to twice the window, so the circular autocorrelation is the linear one.
const FFT_LEN: usize = 2 * WINDOW;
/// The NSDF's oversampling: lags in steps of `1 / UP` decimated samples. At 5.5 samples a period (2 kHz
/// at 11.025 kHz) whole lags miss a peak by enough to pick its octave.
const UP: usize = 4;
/// The span finder's block: the window is judged in blocks this long.
const BLOCK: usize = 64;
const BLOCKS: usize = WINDOW / BLOCK;
/// A block this far under the window's loudest (energy, -30 dB) is a break: the span starts after it.
const QUIET: f64 = 1e-3;
/// The onset cut: blocks averaged on each side (512 samples, longer than the longest period), the
/// fewest blocks after it, and the rise in mean energy that makes one (6 dB).
const ONSET_SPAN: usize = 8;
const ONSET_AFTER: usize = 4;
const RISE: f64 = 4.0;
/// The shortest span analysed: 21 ms at 12 kHz.
const MIN_SPAN: usize = 256;
/// The anti-alias low-pass before decimation: an 8th-order Butterworth.
const ALIAS_HZ: f64 = 4000.0;
const ALIAS_SECTIONS: usize = 4;
/// The level gate on the analysed span's peak: -60 dBFS.
const GATE_PEAK: f64 = 1e-3;
/// Readings reach half a semitone past either end of the range, so a note at an end reads whole.
const EDGE: f64 = 1.029_302_236_643_492; // 2^(50/1200)
/// MPM's key-maximum threshold: the first key maximum at least this fraction of the highest.
const KEY: f64 = 0.9;
/// The clarity a span needs for a pitch.
const CLARITY: f64 = 0.93;
/// The weighted refinement's guards: every peak it climbs at least this high, and its period within
/// `AGREE` of the broadband one.
const RELIABLE: f64 = 0.95;
const AGREE: f64 = 0.03;
/// A constant far under any signal, added before the filter so its state never goes subnormal (the
/// span's mean is removed after).
const DENORMAL_GUARD: f64 = 1e-18;

/// One biquad low-pass section (RBJ, bilinear), transposed direct form II, in f64.
#[derive(Clone, Copy, Debug, Default)]
struct Section {
    b0: f64,
    b1: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl Section {
    fn lowpass(rate: f64, hz: f64, q: f64) -> Self {
        let (sin, cos) = (std::f64::consts::TAU * hz / rate).sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - cos) / 2.0 / a0;
        Section { b0, b1: 2.0 * b0, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0, s1: 0.0, s2: 0.0 }
    }

    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b0 * x - self.a2 * y;
        y
    }

    fn clear(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }
}

/// The Q of section `k` of an even-order Butterworth.
fn butterworth_q(order: usize, k: usize) -> f64 {
    1.0 / (2.0 * (std::f64::consts::PI * (2 * k + 1) as f64 / (2 * order) as f64).cos())
}

/// A pitch detector for one mono input at the device rate.
pub struct Detector {
    factor: usize,
    phase: usize,
    /// The decimated rate.
    rate: f64,
    alias: [Section; ALIAS_SECTIONS],
    /// The decimated input, `RING` long, written at `head`.
    ring: Vec<f64>,
    head: usize,
    /// Decimated samples since `new` or `reset`, up to `RING`.
    filled: usize,
    since: usize,
    /// The detection's lag range in NSDF steps: the reportable pitches, half a semitone past each end.
    lo: f64,
    hi: usize,
    corr: Correlator,
    lin: Vec<f64>,
    weighted: Vec<f64>,
    nsdf: Vec<f64>,
}

impl Detector {
    /// Allocates everything the detector will ever use.
    pub fn new(sample_rate: u32) -> Self {
        let factor = ((sample_rate as f64 / ANALYSIS_RATE as f64).round() as usize).max(1);
        let full = sample_rate as f64;
        let rate = full / factor as f64;
        let mut alias = [Section::default(); ALIAS_SECTIONS];
        for (k, s) in alias.iter_mut().enumerate() {
            *s = Section::lowpass(full, ALIAS_HZ.min(0.4 * rate), butterworth_q(2 * ALIAS_SECTIONS, k));
        }
        Detector {
            factor,
            phase: 0,
            rate,
            alias,
            ring: vec![0.0; RING],
            head: 0,
            filled: 0,
            since: 0,
            lo: UP as f64 * rate / (MAX_HZ as f64 * EDGE),
            hi: (UP as f64 * rate * EDGE / MIN_HZ as f64).ceil() as usize + UP,
            corr: Correlator::new(),
            lin: vec![0.0; RING],
            weighted: vec![0.0; WINDOW],
            nsdf: vec![0.0; UP * WINDOW + 2],
        }
    }

    /// One sample in; a reading every 512 decimated samples (about 43 ms).
    pub fn push(&mut self, x: f32) -> Option<Reading> {
        // A non-finite sample (raw device input) is taken as silence, so it never reaches the window.
        let x = if x.is_finite() { x } else { 0.0 };
        let mut v = x as f64 + DENORMAL_GUARD;
        for s in &mut self.alias {
            v = s.run(v);
        }
        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;
        self.ring[self.head] = v;
        self.head = (self.head + 1) % RING;
        self.filled = (self.filled + 1).min(RING);
        self.since += 1;
        if self.since < HOP {
            return None;
        }
        self.since = 0;
        Some(self.analyse())
    }

    /// After a break in the input: forget the window and the filter. Readings go on every 512
    /// decimated samples, with no pitch until the fresh samples make a span.
    pub fn reset(&mut self) {
        for s in &mut self.alias {
            s.clear();
        }
        self.ring.fill(0.0);
        self.phase = 0;
        self.head = 0;
        self.filled = 0;
        self.since = 0;
    }

    fn analyse(&mut self) -> Reading {
        let w = WINDOW;
        for (i, v) in self.lin.iter_mut().enumerate() {
            *v = self.ring[(self.head + i) % RING];
        }
        let win = &self.lin[HISTORY..];
        let mean = win.iter().sum::<f64>() / w as f64;
        // The level is the RMS of the fresh samples alone: after `new` or `reset` the window fills from
        // zeros, and those would read a steady tone 6 dB low at the first reading.
        let fresh = &win[w - self.filled.min(w)..];
        let fresh_mean = fresh.iter().sum::<f64>() / fresh.len() as f64;
        let total: f64 = fresh.iter().map(|x| (x - fresh_mean) * (x - fresh_mean)).sum();
        let level = (total / fresh.len() as f64).sqrt() as f32;
        let none = |clarity: f64| Reading { hz: 0.0, level, clarity: clarity as f32 };

        // The span: from the newest break (a quiet block) or onset (a rise), and fresh samples only.
        let mut energy = [0.0f64; BLOCKS];
        for (b, e) in energy.iter_mut().enumerate() {
            *e = win[b * BLOCK..(b + 1) * BLOCK].iter().map(|x| (x - mean) * (x - mean)).sum();
        }
        let loudest = energy.iter().cloned().fold(0.0, f64::max);
        if loudest <= 0.0 || energy[BLOCKS - 1] < loudest * QUIET {
            return none(0.0);
        }
        let mut b = BLOCKS - 1;
        while b > 0 && energy[b - 1] >= loudest * QUIET {
            b -= 1;
        }
        let quiet = b;
        for k in (quiet + 1..=BLOCKS - ONSET_AFTER).rev() {
            let after = &energy[k..(k + ONSET_SPAN).min(BLOCKS)];
            let before = &energy[k.saturating_sub(ONSET_SPAN).max(quiet)..k];
            if after.iter().sum::<f64>() / after.len() as f64 > RISE * before.iter().sum::<f64>() / before.len() as f64 {
                b = k;
                break;
            }
        }
        let mut s = (b * BLOCK).max(w - self.filled.min(w));
        // Into the first block, past the silence before an onset.
        let floor = 0.01 * (loudest / BLOCK as f64).sqrt();
        let first_end = (s / BLOCK + 1) * BLOCK;
        while s < first_end && (win[s] - mean).abs() < floor {
            s += 1;
        }
        let l = w - s;
        if l < MIN_SPAN {
            return none(0.0);
        }
        let s = HISTORY + s;
        let span_mean = self.lin[s..].iter().sum::<f64>() / l as f64;
        for v in &mut self.lin {
            *v -= span_mean;
        }
        if self.lin[s..].iter().fold(0.0f64, |m, x| m.max(x.abs())) < GATE_PEAK {
            return none(0.0);
        }

        // Detection on the broadband NSDF, then its refinement out to half the span.
        let reach = l / 2;
        let dmax = self.hi.min(UP * reach);
        self.corr.nsdf(&self.lin[s..], reach + 1, &mut self.nsdf);
        let Some((j0, clarity)) = pick(&self.nsdf, dmax, self.lo) else {
            return none(0.0);
        };
        if clarity < CLARITY {
            return none(clarity);
        }
        let (p0, _) = parabolic(&self.nsdf, j0);
        if dmax < self.hi && 2.0 * p0 > dmax as f64 {
            // A short span: the octave below was out of reach, so this could be its harmonic.
            return none(clarity);
        }
        let Some((wide, _)) = refine(&self.nsdf, p0, UP * reach) else {
            return none(clarity);
        };

        // The weighted refinement: a boxcar half the period long, which nulls the even partials and
        // weakens the odd ones as 1/k, so an inharmonic string reads near its first partial. Its first
        // outputs use the samples before the span when the span is the whole window, else they start
        // half a period in, so a fresh span has no filter transient.
        let half = ((wide / UP as f64) / 2.0).round().max(1.0) as usize;
        let begin = if s == HISTORY && self.filled >= RING { s + 1 - half } else { s };
        let lw = RING - (begin + half - 1);
        let reach = lw / 2;
        if ((UP * reach) as f64) < 1.5 * wide {
            // Too short yet for a period and a half of lag: wait rather than read the broadband.
            return none(clarity);
        }
        let mut acc: f64 = self.lin[begin..begin + half - 1].iter().sum();
        for (i, y) in self.weighted[..lw].iter_mut().enumerate() {
            acc += self.lin[begin + half - 1 + i];
            *y = acc / half as f64;
            acc -= self.lin[begin + i];
        }
        let m = self.weighted[..lw].iter().sum::<f64>() / lw as f64;
        for v in &mut self.weighted[..lw] {
            *v -= m;
        }
        self.corr.nsdf(&self.weighted[..lw], reach + 1, &mut self.nsdf);
        let Some((period, low)) = refine(&self.nsdf, wide, UP * reach) else {
            return none(clarity);
        };
        if low < RELIABLE || (period / wide - 1.0).abs() > AGREE {
            return none(clarity);
        }
        let hz = UP as f64 * self.rate / period;
        if hz * EDGE < MIN_HZ as f64 || hz > MAX_HZ as f64 * EDGE {
            return none(clarity);
        }
        Reading { hz: hz as f32, level, clarity: clarity as f32 }
    }
}

/// The FFT autocorrelation and its buffers, planned once.
struct Correlator {
    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,
    buf: Vec<Complex<f64>>,
    fine: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    energy: Vec<f64>,
}

impl Correlator {
    fn new() -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let ifft = planner.plan_fft_inverse(UP * FFT_LEN);
        let scratch = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
        Correlator {
            fft,
            ifft,
            buf: vec![Complex::new(0.0, 0.0); FFT_LEN],
            fine: vec![Complex::new(0.0, 0.0); UP * FFT_LEN],
            scratch: vec![Complex::new(0.0, 0.0); scratch],
            energy: vec![0.0; WINDOW + 1],
        }
    }

    /// The normalised square difference function of `x` (at most `WINDOW` long) at lags
    /// `0..=max_lag` in steps of `1 / UP`, into `out`: `n(t) = 2 r(t) / m(t)`, `r` the autocorrelation
    /// (zero-padded to twice the window, so the linear one, and band-limited between whole lags by
    /// padding its spectrum), `m` the energy of both overlapping parts, linear between whole lags.
    fn nsdf(&mut self, x: &[f64], max_lag: usize, out: &mut [f64]) {
        let (l, n, big) = (x.len(), FFT_LEN, UP * FFT_LEN);
        for (b, &v) in self.buf.iter_mut().zip(x) {
            *b = Complex::new(v, 0.0);
        }
        self.buf[l..].fill(Complex::new(0.0, 0.0));
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let power = |c: Complex<f64>| Complex::new(c.norm_sqr(), 0.0);
        self.fine.fill(Complex::new(0.0, 0.0));
        self.fine[0] = power(self.buf[0]);
        for k in 1..n / 2 {
            self.fine[k] = power(self.buf[k]);
            self.fine[big - k] = self.fine[k];
        }
        self.fine[n / 2] = 0.5 * power(self.buf[n / 2]);
        self.fine[big - n / 2] = self.fine[n / 2];
        self.ifft.process_with_scratch(&mut self.fine, &mut self.scratch);
        let max_lag = max_lag.min(l - 1);
        let mut m: f64 = 2.0 * x.iter().map(|v| v * v).sum::<f64>();
        for (t, e) in self.energy.iter_mut().enumerate().take(max_lag + 2) {
            *e = m;
            if t < l {
                m -= x[t] * x[t] + x[l - 1 - t] * x[l - 1 - t];
            }
        }
        for (j, o) in out.iter_mut().enumerate().take(UP * max_lag + 1) {
            let (t, f) = (j / UP, (j % UP) as f64 / UP as f64);
            let m = self.energy[t] + f * (self.energy[t + 1] - self.energy[t]);
            *o = if m > 0.0 { 2.0 * self.fine[j].re / n as f64 / m } else { 0.0 };
        }
    }
}

/// Each key maximum of `n` over the lags `1..=dmax` (`n` holds one more), in order, until `each`
/// says stop: the highest point of each positive lobe after the central one.
fn key_maxima(n: &[f64], dmax: usize, mut each: impl FnMut(usize) -> bool) {
    let mut t = 1;
    while t <= dmax && n[t] > 0.0 {
        t += 1;
    }
    let mut top: Option<usize> = None;
    for i in t..=dmax {
        if n[i] > 0.0 {
            if top.is_none_or(|k| n[i] > n[k]) {
                top = Some(i);
            }
        } else if let Some(k) = top.take() {
            if each(k) {
                return;
            }
        }
    }
    // A lobe still open at `dmax` counts if its top is a peak, not the edge.
    if let Some(k) = top.filter(|&k| k < dmax) {
        each(k);
    }
}

/// McLeod's pick: of the key maxima whose interpolated lag is at least `lo`, the first whose
/// interpolated height is at least `KEY` of the highest. Its index and height.
fn pick(n: &[f64], dmax: usize, lo: f64) -> Option<(usize, f64)> {
    let height = |k: usize| {
        let (lag, v) = parabolic(n, k);
        (lag >= lo).then_some(v)
    };
    let mut highest = f64::MIN;
    key_maxima(n, dmax, |k| {
        if let Some(v) = height(k) {
            highest = highest.max(v);
        }
        false
    });
    let mut found = None;
    key_maxima(n, dmax, |k| match height(k) {
        Some(v) if v >= KEY * highest => {
            found = Some((k, v));
            true
        }
        _ => false,
    });
    found
}

/// From a period `p` (NSDF steps) on `n` (valid to `rmax + 1`): the peak nearest it, then the peaks
/// nearest twice, four times... the period out to the largest multiple within `rmax`, each lag divided
/// by its multiple. Returns the period and the lowest peak climbed.
fn refine(n: &[f64], p: f64, rmax: usize) -> Option<(f64, f64)> {
    let i = climb(n, (p.round() as usize).clamp(2, rmax), rmax);
    let (lag, first) = parabolic(n, i);
    if (lag - p).abs() > 0.25 * p {
        return None;
    }
    let (mut period, mut m, mut low) = (lag, 1usize, first);
    loop {
        let top = ((rmax - 1) as f64 / period).floor() as usize;
        let next = (2 * m).min(top);
        if next <= m {
            break;
        }
        let guess = next as f64 * period;
        let i = climb(n, (guess.round() as usize).clamp(2, rmax), rmax);
        let (lag, val) = parabolic(n, i);
        if val < 0.5 * first || (lag - guess).abs() > 0.25 * period {
            break;
        }
        period = lag / next as f64;
        m = next;
        low = low.min(val);
    }
    Some((period, low))
}

/// From `i`, up the slope to the nearest local maximum within `2..=max`.
fn climb(n: &[f64], mut i: usize, max: usize) -> usize {
    loop {
        if i < max && n[i + 1] > n[i] {
            i += 1;
        } else if i > 2 && n[i - 1] > n[i] {
            i -= 1;
        } else {
            return i;
        }
    }
}

/// The vertex of the parabola through `n[i-1], n[i], n[i+1]`: its lag and value.
fn parabolic(n: &[f64], i: usize) -> (f64, f64) {
    let (a, b, c) = (n[i - 1], n[i], n[i + 1]);
    let den = a - 2.0 * b + c;
    if den >= 0.0 {
        return (i as f64, b);
    }
    let d = 0.5 * (a - c) / den;
    (i as f64 + d, b - 0.25 * (a - c) * d)
}

/// How much louder the other input must be, by `level`, to take the display from a pitched one: 6 dB.
const LOUDER: f32 = 1.995_262_3;

/// The tuner's two ears: one detector per slot input, and which one's reading is shown. The first
/// pitch (after `new` or `reset`) gives the display to the louder pitched input, input 0 on a tie; it
/// changes hands when the shown input has had no pitch for 250 ms while the other has one, or when the
/// other has been pitched and 6 dB louder for 250 ms. With no pitch on either it reads `hz: 0.0` at
/// the larger level. Identical inputs never change hands and read as one detector, bit for bit.
pub struct Ear {
    ears: [Detector; 2],
    /// The input shown; none until one has had a pitch.
    owner: Option<usize>,
    /// Frames pushed: the Ear's clock.
    now: u64,
    /// When the owner last had a pitch.
    owned: u64,
    /// Since when the other input has been pitched and `LOUDER` than the owner, reading after reading.
    louder: Option<u64>,
    /// 250 ms in frames.
    hold: u64,
}

impl Ear {
    pub fn new(sample_rate: u32) -> Self {
        Ear {
            ears: [Detector::new(sample_rate), Detector::new(sample_rate)],
            owner: None,
            now: 0,
            owned: 0,
            louder: None,
            hold: sample_rate as u64 / 4,
        }
    }

    /// Both inputs' sample for one frame; a reading when one is due (both detectors read together).
    pub fn push(&mut self, x: [f32; 2]) -> Option<Reading> {
        let a = self.ears[0].push(x[0]);
        let b = self.ears[1].push(x[1]);
        self.now += 1;
        let r = [a?, b?];
        let pitched = [r[0].hz > 0.0, r[1].hz > 0.0];
        let owner = match self.owner {
            None if pitched[0] || pitched[1] => {
                self.owned = self.now;
                self.louder = None;
                usize::from(pitched[1] && (!pitched[0] || r[1].level > r[0].level))
            }
            None => return Some(Reading { hz: 0.0, ..louder(r, 0) }),
            Some(s) => {
                let o = 1 - s;
                if pitched[s] {
                    self.owned = self.now;
                }
                let loud = pitched[o] && r[o].level >= LOUDER * r[s].level;
                self.louder = if loud { Some(self.louder.unwrap_or(self.now)) } else { None };
                let silent = pitched[o] && !pitched[s] && self.now - self.owned >= self.hold;
                let drowned = self.louder.is_some_and(|t| self.now - t >= self.hold);
                if silent || drowned {
                    self.owned = self.now;
                    self.louder = None;
                    o
                } else {
                    s
                }
            }
        };
        self.owner = Some(owner);
        Some(if pitched[owner] { r[owner] } else { Reading { hz: 0.0, ..louder(r, owner) } })
    }

    /// After a break in the input: forget both windows and the owner.
    pub fn reset(&mut self) {
        self.ears[0].reset();
        self.ears[1].reset();
        self.owner = None;
        self.louder = None;
    }
}

/// The reading with the larger level, `tie`'s on a tie.
fn louder(r: [Reading; 2], tie: usize) -> Reading {
    if r[1 - tie].level > r[tie].level {
        r[1 - tie]
    } else {
        r[tie]
    }
}
