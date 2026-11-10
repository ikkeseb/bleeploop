//! The tuner's pitch detector (`lf_engine::pitch`), rendered: synthetic guitar-like inputs at 44.1, 48
//! and 96 kHz, every reading held to the pitch it was rendered at. Signals are deterministic (seeded
//! phases and noise; no clock but the ignored cost bar's), and every push runs under `assert_no_alloc`.
//! A reading's time is the frame that completed it. Each test prints what it measured
//! (`-- --nocapture`).

use std::f64::consts::{PI, TAU};

use assert_no_alloc::assert_no_alloc;
use lf_engine::pitch::{Detector, Ear, Reading};

// Every push below runs under assert_no_alloc: the feed thread must not churn. The check runs in debug
// builds; the crate's default `disable_release` makes it a no-op in release.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn violation_count() -> u32 {
    #[cfg(debug_assertions)]
    return assert_no_alloc::violation_count();
    #[cfg(not(debug_assertions))]
    0
}

const RATES: [u32; 3] = [44_100, 48_000, 96_000];

/// The steady-tone notes: the range's ends and the guitar's and bass's registers.
const NOTES: [(&str, f64); 15] = [
    ("A0", 27.50),
    ("B0", 30.87),
    ("E1", 41.20),
    ("A1", 55.00),
    ("E2", 82.41),
    ("A2", 110.00),
    ("D3", 146.83),
    ("G3", 196.00),
    ("B3", 246.94),
    ("E4", 329.63),
    ("A4", 440.00),
    ("E5", 659.26),
    ("B5", 987.77),
    ("E6", 1318.51),
    ("B6", 1975.53),
];
const DETUNE: [f64; 7] = [0.0, -35.0, -7.0, -2.0, 2.0, 7.0, 35.0];

/// The plucked strings: a bass's four, then a guitar's six.
const STRINGS: [(&str, f64); 10] = [
    ("E1", 41.20),
    ("A1", 55.00),
    ("D2", 73.42),
    ("G2", 98.00),
    ("E2", 82.41),
    ("A2", 110.00),
    ("D3", 146.83),
    ("G3", 196.00),
    ("B3", 246.94),
    ("E4", 329.63),
];
/// The strings' inharmonicity: partial k at `f1 k sqrt(1 + B k^2)`.
const B: f64 = 1e-4;

/// A steady tone's length before it stops.
const TONE: f64 = 1.2;

// ---- the detector's geometry, as the tests see it ----

fn factor(rate: u32) -> f64 {
    (rate as f64 / 12_000.0).ceil()
}

/// The analysis window in seconds: 2048 decimated samples.
fn window_secs(rate: u32) -> f64 {
    2048.0 * factor(rate) / rate as f64
}

// ---- signals ----

/// splitmix64: seeded and portable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn phase(&mut self) -> f64 {
        TAU * self.unit()
    }
}

fn cents(hz: f64, truth: f64) -> f64 {
    1200.0 * (hz / truth).log2()
}

fn gain(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
}

fn detune(hz: f64, c: f64) -> f64 {
    hz * 2f64.powf(c / 1200.0)
}

fn samples(rate: u32, secs: f64) -> usize {
    (secs * rate as f64).round() as usize
}

/// Harmonics at 1/k, up to the smaller of 20 or what fits under 5 kHz.
fn sawtooth(hz: f64) -> Vec<f64> {
    let n = ((5000.0 / hz).floor() as usize).clamp(1, 20);
    (1..=n).map(|k| 1.0 / k as f64).collect()
}

/// The sawtooth's harmonics with the fundamental 20 dB under the second.
fn weak_fundamental(hz: f64) -> Vec<f64> {
    let mut a = sawtooth(hz);
    assert!(a.len() >= 2, "{hz} Hz has no second harmonic under 5 kHz");
    a[0] = 0.1 * a[1];
    a
}

/// A harmonic tone: partial k (from 1) at k times `f(t)` Hz, amplitude `amps[k - 1]`, seeded phases.
fn harmonic(rate: u32, secs: f64, f: &dyn Fn(f64) -> f64, amps: &[f64], seed: u64) -> Vec<f64> {
    let n = samples(rate, secs);
    let r = rate as f64;
    let mut rng = Rng(seed);
    let mut z: Vec<(f64, f64)> = amps
        .iter()
        .map(|_| {
            let p = rng.phase();
            (p.cos(), p.sin())
        })
        .collect();
    let mut out = vec![0.0; n];
    for (i, o) in out.iter_mut().enumerate() {
        *o = z.iter().zip(amps).map(|(z, a)| a * z.1).sum();
        let (s, c) = (TAU * f(i as f64 / r) / r).sin_cos();
        let (mut wr, mut wi) = (c, s);
        for zk in z.iter_mut() {
            *zk = (zk.0 * wr - zk.1 * wi, zk.0 * wi + zk.1 * wr);
            (wr, wi) = (wr * c - wi * s, wr * s + wi * c);
        }
        if i % 1024 == 1023 {
            for zk in z.iter_mut() {
                let m = (zk.0 * zk.0 + zk.1 * zk.1).sqrt();
                *zk = (zk.0 / m, zk.1 / m);
            }
        }
    }
    out
}

/// One partial of a string: its frequency, its amplitude at the pluck and its decay rate (1/s).
struct Partial {
    hz: f64,
    amp: f64,
    decay: f64,
}

/// A plucked string's partials up to 8 kHz: stretched by inharmonicity, 1/k under a pluck-position
/// comb that weakens every 5th, the first falling 60 dB in 4 s and the higher ones faster.
fn string_partials(f1: f64, rate: u32) -> Vec<Partial> {
    let top = 8_000f64.min(0.45 * rate as f64);
    let first = 1000f64.ln() / 4.0;
    (1..)
        .map(|k| (k as f64, f1 * k as f64 * (1.0 + B * (k * k) as f64).sqrt()))
        .take_while(|&(_, hz)| hz < top)
        .map(|(k, hz)| Partial {
            hz,
            amp: (0.1 + 0.9 * (PI * k / 5.0).sin().abs()) / k,
            decay: first * (1.0 + (hz / 1000.0).powi(2)),
        })
        .collect()
}

/// The pitch a string should read: its first partial.
fn first_partial(f1: f64) -> f64 {
    f1 * (1.0 + B).sqrt()
}

/// A plucked string from t = 0: each partial a decaying phasor, set exactly every 4096 samples.
fn render_string(rate: u32, secs: f64, partials: &[Partial], seed: u64) -> Vec<f64> {
    let n = samples(rate, secs);
    let r = rate as f64;
    let mut rng = Rng(seed);
    let mut out = vec![0.0; n];
    for p in partials {
        let ph = rng.phase();
        let (s, c) = (TAU * p.hz / r).sin_cos();
        let d = (-p.decay / r).exp();
        let (wr, wi) = (c * d, s * d);
        let mut z = (0.0, 0.0);
        for (i, o) in out.iter_mut().enumerate() {
            if i % 4096 == 0 {
                let t = i as f64 / r;
                let a = p.amp * (-p.decay * t).exp();
                let (s, c) = (TAU * p.hz * t + ph).sin_cos();
                z = (a * c, a * s);
            }
            *o += z.1;
            z = (z.0 * wr - z.1 * wi, z.0 * wi + z.1 * wr);
        }
    }
    out
}

/// A plucked string whose every partial is bent by `bend(t)` cents.
fn render_bent_string(rate: u32, secs: f64, partials: &[Partial], bend: &dyn Fn(f64) -> f64, seed: u64) -> Vec<f64> {
    let n = samples(rate, secs);
    let r = rate as f64;
    let mut rng = Rng(seed);
    let phases: Vec<f64> = partials.iter().map(|_| rng.phase()).collect();
    // The common phase: the integral of the bend's frequency factor.
    let mut phi = vec![0.0; n];
    let mut acc = 0.0;
    for (i, p) in phi.iter_mut().enumerate() {
        *p = acc;
        acc += 2f64.powf(bend(i as f64 / r) / 1200.0) / r;
    }
    let mut out = vec![0.0; n];
    for (p, &ph) in partials.iter().zip(&phases) {
        let d = (-p.decay / r).exp();
        let mut a = p.amp;
        for (o, &t) in out.iter_mut().zip(&phi) {
            *o += a * (TAU * p.hz * t + ph).sin();
            a *= d;
        }
    }
    out
}

/// White noise, uniform, at `rms` linear.
fn white(n: usize, rms: f64, seed: u64) -> Vec<f64> {
    let mut rng = Rng(seed);
    (0..n).map(|_| (2.0 * rng.unit() - 1.0) * rms * 3f64.sqrt()).collect()
}

fn peak(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

/// `x` scaled to a peak of `dbfs`.
fn at_peak(x: &[f64], dbfs: f64) -> Vec<f64> {
    let g = gain(dbfs) / peak(x);
    x.iter().map(|v| v * g).collect()
}

fn to_f32(x: &[f64]) -> Vec<f32> {
    x.iter().map(|&v| v as f32).collect()
}

fn silence(rate: u32, secs: f64) -> Vec<f64> {
    vec![0.0; samples(rate, secs)]
}

fn concat(parts: &[&[f64]]) -> Vec<f64> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

fn mix(a: &[f64], b: &[f64]) -> Vec<f64> {
    a.iter().zip(b).map(|(x, y)| x + y).collect()
}

// ---- listening ----

/// One reading and the frame count that completed it.
#[derive(Clone, Copy, Debug)]
struct At {
    frame: u64,
    r: Reading,
}

impl At {
    fn t(&self, rate: u32) -> f64 {
        self.frame as f64 / rate as f64
    }

    fn pitched(&self) -> bool {
        self.r.hz > 0.0
    }
}

/// Every reading of a fresh detector over `x`, the pushes under `assert_no_alloc`.
fn listen(rate: u32, x: &[f32]) -> Vec<At> {
    let mut det = Detector::new(rate);
    listen_with(&mut det, x, 0)
}

fn listen_with(det: &mut Detector, x: &[f32], start: u64) -> Vec<At> {
    let mut out = Vec::with_capacity(x.len() / 256 + 8);
    let before = violation_count();
    assert_no_alloc(|| {
        for (i, &v) in x.iter().enumerate() {
            if let Some(r) = det.push(v) {
                out.push(At { frame: start + i as u64 + 1, r });
            }
        }
    });
    assert_eq!(violation_count(), before, "the detector allocated after new");
    out
}

/// An `Ear` beside two lone detectors on the same inputs, in lockstep, everything reset at frame
/// `reset_at` if given: `(frame, ear, lone 0, lone 1)` per reading.
fn ear_beside(rate: u32, a: &[f32], b: &[f32], reset_at: Option<usize>) -> Vec<(u64, Reading, Reading, Reading)> {
    assert_eq!(a.len(), b.len());
    let mut ear = Ear::new(rate);
    let mut lone = [Detector::new(rate), Detector::new(rate)];
    let mut out = Vec::with_capacity(a.len() / 256 + 8);
    let before = violation_count();
    assert_no_alloc(|| {
        for i in 0..a.len() {
            if reset_at == Some(i) {
                ear.reset();
                lone[0].reset();
                lone[1].reset();
            }
            let e = ear.push([a[i], b[i]]);
            let r0 = lone[0].push(a[i]);
            let r1 = lone[1].push(b[i]);
            match (e, r0, r1) {
                (Some(e), Some(r0), Some(r1)) => out.push((i as u64 + 1, e, r0, r1)),
                (None, None, None) => {}
                _ => panic!("the Ear and its detectors read on different frames ({i})"),
            }
        }
    });
    assert_eq!(violation_count(), before, "the Ear allocated after new");
    out
}

fn report(fails: &[String]) {
    assert!(fails.is_empty(), "{} failures, the first:\n{}", fails.len(), fails[..fails.len().min(25)].join("\n"));
}

// ---- 1, 2 and 5: steady tones, their stability, acquisition and release ----

fn steady(rate: u32) {
    let mut out = format!("steady tones at {rate} Hz, -12 dBFS peak, both timbres and every detune; error in cents after 250 ms\n");
    let mut fails = Vec::new();
    let (mut all_worst, mut all_spread, mut all_acq, mut all_rel) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (ni, &(name, f)) in NOTES.iter().enumerate() {
        let (mut worst, mut spread, mut acq, mut rel) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (di, &d) in DETUNE.iter().enumerate() {
            let hz = detune(f, d);
            for timbre in 0..2 {
                let amps = if timbre == 0 { sawtooth(hz) } else { weak_fundamental(hz) };
                let tone = harmonic(rate, TONE, &|_| hz, &amps, (ni * 100 + di * 10 + timbre) as u64);
                let x = to_f32(&concat(&[&at_peak(&tone, -12.0), &silence(rate, 0.5)]));
                let readings = listen(rate, &x);
                let case = format!("{rate} {name} {d:+} c timbre {timbre}");
                let (mut lo, mut hi) = (f64::MAX, f64::MIN);
                for a in readings.iter().filter(|a| a.t(rate) > 0.25 && a.t(rate) <= TONE) {
                    if !a.pitched() {
                        fails.push(format!("{case}: no pitch at {:.3} s (clarity {:.3})", a.t(rate), a.r.clarity));
                        continue;
                    }
                    let e = cents(a.r.hz as f64, hz);
                    worst = worst.max(e.abs());
                    lo = lo.min(e);
                    hi = hi.max(e);
                    if e.abs() > 1.0 {
                        fails.push(format!("{case}: {e:+.3} c at {:.3} s", a.t(rate)));
                    }
                }
                if hi >= lo {
                    spread = spread.max(hi - lo);
                    if hi - lo > 0.2 {
                        fails.push(format!("{case}: spread {:.3} c", hi - lo));
                    }
                }
                match readings.iter().find(|a| a.pitched()) {
                    Some(a) => {
                        acq = acq.max(a.t(rate));
                        if a.t(rate) > 0.25 {
                            fails.push(format!("{case}: first pitch at {:.3} s", a.t(rate)));
                        }
                    }
                    None => fails.push(format!("{case}: never pitched")),
                }
                match readings.iter().find(|a| a.t(rate) > TONE && !a.pitched()) {
                    Some(a) => rel = rel.max(a.t(rate) - TONE),
                    None => fails.push(format!("{case}: still pitched after the tone stopped")),
                }
                if readings.iter().any(|a| a.t(rate) > TONE + 0.25 && a.pitched()) {
                    fails.push(format!("{case}: pitched in the silence after it"));
                }
            }
        }
        out += &format!(
            "  {name:>3} {f:8.2} Hz  worst {worst:.3} c  spread {spread:.3} c  acquisition {:3.0} ms  release {:3.0} ms\n",
            acq * 1e3,
            rel * 1e3
        );
        all_worst = all_worst.max(worst);
        all_spread = all_spread.max(spread);
        all_acq = all_acq.max(acq);
        all_rel = all_rel.max(rel);
    }
    out += &format!(
        "  ALL at {rate}: worst {all_worst:.3} c, spread {all_spread:.3} c, acquisition {:.0} ms, release {:.0} ms",
        all_acq * 1e3,
        all_rel * 1e3
    );
    println!("{out}");
    report(&fails);
}

#[test]
fn steady_tones_44100() {
    steady(44_100);
}

#[test]
fn steady_tones_48000() {
    steady(48_000);
}

#[test]
fn steady_tones_96000() {
    steady(96_000);
}

// ---- 3: plucked strings ----

/// A pluck after 200 ms of its noise floor, noise 50 dB under the pluck's peak throughout.
fn pluck_with_noise(rate: u32, f1: f64, secs: f64, seed: u64) -> (Vec<f64>, f64) {
    let lead = 0.2;
    let string = at_peak(&render_string(rate, secs, &string_partials(f1, rate), seed), -12.0);
    let x = concat(&[&silence(rate, lead), &string]);
    let noise = white(x.len(), gain(-12.0 - 50.0), seed + 1);
    (mix(&x, &noise), lead)
}

fn plucks(rate: u32) {
    let mut out = format!("plucked strings at {rate} Hz (B = 1e-4, noise 50 dB under the peak); cents from the first partial\n");
    let mut fails = Vec::new();
    for (si, &(name, f1)) in STRINGS.iter().enumerate() {
        let (x, lead) = pluck_with_noise(rate, f1, 4.0, 1000 + si as u64);
        let readings = listen(rate, &to_f32(&x));
        let truth = first_partial(f1);
        let case = format!("{rate} {name}");
        let (mut early, mut worst, mut late) = (0.0f64, 0.0f64, 0usize);
        let mut first = None;
        let mut stop = None;
        for a in &readings {
            let t = a.t(rate) - lead;
            if a.pitched() && first.is_none() {
                first = Some(t);
            }
            if !a.pitched() {
                if t >= 0.15 && stop.is_none() {
                    stop = Some(t);
                }
                continue;
            }
            let e = cents(a.r.hz as f64, truth);
            if t < 0.15 {
                early = early.max(e.abs());
                continue;
            }
            if stop.is_some() {
                late += 1;
            }
            worst = worst.max(e.abs());
            if e.abs() > 3.0 {
                fails.push(format!("{case}: {e:+.2} c at {t:.3} s after the pluck"));
            }
        }
        let stop = stop.unwrap_or(f64::INFINITY);
        if stop < 1.5 {
            fails.push(format!("{case}: reported only until {stop:.2} s"));
        }
        out += &format!(
            "  {name}: first pitch {:.0} ms, before 150 ms worst {early:.2} c, from 150 ms worst {worst:.2} c, reported until {stop:.2} s, {late} pitched readings after that\n",
            first.unwrap_or(f64::NAN) * 1e3
        );
    }
    println!("{out}");
    report(&fails);
}

#[test]
fn plucked_strings_44100() {
    plucks(44_100);
}

#[test]
fn plucked_strings_48000() {
    plucks(48_000);
}

#[test]
fn plucked_strings_96000() {
    plucks(96_000);
}

// ---- 4: rejection ----

#[test]
fn rejects_silence_noise_and_quiet_tones() {
    let mut fails = Vec::new();
    for rate in RATES {
        let cases: Vec<(String, Vec<f64>)> = vec![
            ("silence".into(), silence(rate, 2.0)),
            ("white noise at -30 dBFS".into(), white(samples(rate, 10.0), gain(-30.0), 7)),
            ("E2 at -70 dBFS".into(), at_peak(&harmonic(rate, 2.0, &|_| 82.41, &sawtooth(82.41), 8), -70.0)),
            ("A4 at -70 dBFS".into(), at_peak(&harmonic(rate, 2.0, &|_| 440.0, &sawtooth(440.0), 9), -70.0)),
        ];
        for (name, x) in cases {
            let readings = listen(rate, &to_f32(&x));
            let pitched: Vec<_> = readings.iter().filter(|a| a.pitched()).collect();
            let top = readings.iter().map(|a| a.r.clarity).fold(0.0f32, f32::max);
            println!("{rate} {name}: {} readings, {} pitched, highest clarity {top:.3}", readings.len(), pitched.len());
            if let Some(a) = pitched.first() {
                fails.push(format!("{rate} {name}: {:.2} Hz at {:.3} s", a.r.hz, a.t(rate)));
            }
        }
        // Two unrelated tones of equal level: either one or nothing, never a third pitch.
        let (f, g) = (82.41, detune(82.41, 630.0));
        let a = at_peak(&harmonic(rate, 3.0, &|_| f, &sawtooth(f), 10), -18.0);
        let b = at_peak(&harmonic(rate, 3.0, &|_| g, &sawtooth(g), 11), -18.0);
        let readings = listen(rate, &to_f32(&mix(&a, &b)));
        let (mut on_f, mut on_g) = (0, 0);
        for r in readings.iter().filter(|a| a.pitched()) {
            let (ef, eg) = (cents(r.r.hz as f64, f), cents(r.r.hz as f64, g));
            if ef.abs() <= 5.0 {
                on_f += 1;
            } else if eg.abs() <= 5.0 {
                on_g += 1;
            } else {
                fails.push(format!("{rate} two tones: a third pitch, {:.2} Hz at {:.3} s", r.r.hz, r.t(rate)));
            }
        }
        println!("{rate} two tones (82.41 and {g:.2} Hz): {} readings, {on_f} on the lower, {on_g} on the upper", readings.len());
    }
    report(&fails);
}

// ---- 5: timing ----

#[test]
fn readings_come_often_and_reset_forgets() {
    let mut fails = Vec::new();
    for rate in RATES {
        let x = to_f32(&white(samples(rate, 10.0), gain(-40.0), 12));
        let n = listen(rate, &x).len();
        println!("{rate}: {:.1} readings a second", n as f64 / 10.0);
        if (n as f64) < 15.0 * 10.0 {
            fails.push(format!("{rate}: only {n} readings in 10 s"));
        }
        // A tone, a reset, another tone: nothing stale after the reset.
        for (ai, &(fa, fb)) in [(82.41, 110.0), (110.0, detune(110.0, 40.0)), (329.63, 246.94), (41.20, 55.0)].iter().enumerate() {
            let a = at_peak(&harmonic(rate, 1.0, &|_| fa, &sawtooth(fa), 20 + ai as u64), -12.0);
            let b = at_peak(&harmonic(rate, 1.0, &|_| fb, &sawtooth(fb), 30 + ai as u64), -12.0);
            let mut det = Detector::new(rate);
            let before = listen_with(&mut det, &to_f32(&a), 0);
            assert!(before.last().is_some_and(|a| a.pitched()));
            det.reset();
            let after = listen_with(&mut det, &to_f32(&b), a.len() as u64);
            let reset_t = a.len() as f64 / rate as f64;
            let case = format!("{rate} {fa} then {fb:.2} Hz");
            let first = after.iter().find(|a| a.pitched());
            match first {
                Some(f) => {
                    let e = cents(f.r.hz as f64, fb);
                    println!("{case}: first pitched reading {:.0} ms after the reset, {e:+.3} c", (f.t(rate) - reset_t) * 1e3);
                    if e.abs() > 1.0 {
                        fails.push(format!("{case}: the first reading after the reset is {e:+.3} c"));
                    }
                }
                None => fails.push(format!("{case}: no pitch after the reset")),
            }
            for r in after.iter().filter(|a| a.pitched()) {
                let e = cents(r.r.hz as f64, fb);
                if e.abs() > 1.0 {
                    fails.push(format!("{case}: {e:+.3} c at {:.3} s after the reset", r.t(rate) - reset_t));
                }
            }
        }
    }
    report(&fails);
}

// ---- 6: a glide ----

#[test]
fn follows_a_string_tuned_up() {
    let mut fails = Vec::new();
    let notes = [("E1", 41.20), ("E2", 82.41), ("A2", 110.0), ("D3", 146.83), ("G3", 196.0), ("B3", 246.94), ("E4", 329.63)];
    for rate in RATES {
        for (ni, &(name, f)) in notes.iter().enumerate() {
            // -30 c for 0.5 s, up to 0 c over 1 s, then held.
            let bend = |t: f64| -30.0 + 30.0 * ((t - 0.5) / 1.0).clamp(0.0, 1.0);
            let x = at_peak(&harmonic(rate, 2.0, &|t| detune(f, bend(t)), &sawtooth(f), 40 + ni as u64), -12.0);
            let readings = listen(rate, &to_f32(&x));
            let case = format!("{rate} {name}");
            let (mut lag_max, mut lag_sum, mut lag_n) = (0.0f64, 0.0, 0);
            for a in readings.iter().filter(|a| a.t(rate) > 0.25) {
                let t = a.t(rate);
                if !a.pitched() {
                    fails.push(format!("{case}: no pitch at {t:.3} s"));
                    continue;
                }
                let c = cents(a.r.hz as f64, f);
                let (lo, hi) = (bend(t - 0.15) - 0.5, bend(t) + 0.5);
                if c < lo || c > hi {
                    fails.push(format!("{case}: {c:+.2} c at {t:.3} s, outside {lo:+.2}..{hi:+.2}"));
                }
                if t > 0.6 && t < 1.5 {
                    // When the glide was where this reading is.
                    let lag = t - (0.5 + (c + 30.0) / 30.0);
                    lag_max = lag_max.max(lag);
                    lag_sum += lag;
                    lag_n += 1;
                }
            }
            println!("{case}: glide lag mean {:.0} ms, max {:.0} ms", lag_sum / lag_n as f64 * 1e3, lag_max * 1e3);
        }
    }
    report(&fails);
}

// ---- the seat's additions: non-finite input, amplitude, clipping ----

#[test]
fn non_finite_input_is_silence() {
    let mut fails = Vec::new();
    for rate in RATES {
        for (ni, &f) in [82.41, 110.0, 329.63].iter().enumerate() {
            let mut x = to_f32(&at_peak(&harmonic(rate, 2.0, &|_| f, &sawtooth(f), 50 + ni as u64), -12.0));
            let (from, to) = (samples(rate, 1.0), samples(rate, 1.01));
            for (i, v) in x[from..to].iter_mut().enumerate() {
                *v = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY][i % 3];
            }
            let readings = listen(rate, &x);
            let case = format!("{rate} {f}");
            let mut back = None;
            for a in &readings {
                let t = a.t(rate);
                if !(a.r.hz.is_finite() && a.r.level.is_finite() && a.r.clarity.is_finite()) {
                    fails.push(format!("{case}: a non-finite reading at {t:.3} s: {:?}", a.r));
                }
                let good = a.pitched() && cents(a.r.hz as f64, f).abs() <= 1.0;
                if t > 1.01 && good && back.is_none() {
                    back = Some(t - 1.01);
                }
                if (t > 0.25 && t <= 1.0 || t >= 1.31) && !good {
                    fails.push(format!("{case}: {:.3} Hz at {t:.3} s", a.r.hz));
                }
            }
            println!("{case}: correct again {:.0} ms after the burst", back.unwrap_or(f64::NAN) * 1e3);
        }
    }
    report(&fails);
}

#[test]
fn level_does_not_move_the_pitch() {
    let mut fails = Vec::new();
    for rate in RATES {
        for (ni, &(name, f)) in [("E2", 82.41), ("E4", 329.63)].iter().enumerate() {
            let tone = harmonic(rate, 1.5, &|_| f, &sawtooth(f), 60 + ni as u64);
            let (mut lo, mut hi) = (f64::MAX, f64::MIN);
            for level in [-50.0, -30.0, -12.0, -3.0] {
                let readings = listen(rate, &to_f32(&at_peak(&tone, level)));
                for a in readings.iter().filter(|a| a.t(rate) > 0.25) {
                    if !a.pitched() {
                        fails.push(format!("{rate} {name} at {level} dBFS: no pitch at {:.3} s", a.t(rate)));
                        continue;
                    }
                    let e = cents(a.r.hz as f64, f);
                    lo = lo.min(e);
                    hi = hi.max(e);
                }
            }
            println!("{rate} {name} from -50 to -3 dBFS: readings span {:.3} c ({lo:+.3} to {hi:+.3})", hi - lo);
            if hi - lo > 0.3 {
                fails.push(format!("{rate} {name}: the levels' readings span {:.3} c", hi - lo));
            }
            // Hard-clipped at half its peak.
            let clipped: Vec<f64> = at_peak(&tone, 0.0).iter().map(|v| v.clamp(-0.5, 0.5)).collect();
            let readings = listen(rate, &to_f32(&clipped));
            let worst = readings.iter().filter(|a| a.t(rate) > 0.25).fold(0.0f64, |w, a| {
                if !a.pitched() {
                    return f64::INFINITY;
                }
                w.max(cents(a.r.hz as f64, f).abs())
            });
            println!("{rate} {name} clipped at half its peak: worst {worst:.3} c");
            if worst > 1.0 {
                fails.push(format!("{rate} {name} clipped: worst {worst:.3} c"));
            }
        }
    }
    report(&fails);
}

/// `level` is the input's own RMS: a full-scale sine reads 0.7071 within 1 % from the first full
/// window on, and its first readings, over the few periods a fresh window holds, within 3 %; none
/// counts the zeros a fresh window starts from (they read the first reading 6 dB low).
#[test]
fn level_is_the_inputs_rms_from_the_first_reading() {
    let mut fails = Vec::new();
    for rate in RATES {
        for f in [41.2, 55.0, 82.41, 110.0, 196.0, 440.0, 659.26, 1318.51] {
            let n = samples(rate, 1.0);
            let x: Vec<f32> = (0..n).map(|i| (TAU * f * i as f64 / rate as f64).sin() as f32).collect();
            let full = window_secs(rate);
            let (mut first, mut steady) = (0.0f64, 0.0f64);
            for a in listen(rate, &x) {
                let e = (a.r.level as f64 / std::f64::consts::FRAC_1_SQRT_2 - 1.0).abs();
                if a.t(rate) >= full {
                    steady = steady.max(e);
                } else {
                    first = first.max(e);
                }
            }
            if steady > 0.01 || first > 0.03 {
                fails.push(format!("{rate} {f} Hz: full windows off by {:.2} %, the first by {:.2} %", steady * 100.0, first * 100.0));
            }
        }
    }
    report(&fails);
}

// ---- the seat's additions: more of a real string ----

#[test]
fn a_string_settling_from_sharp() {
    let mut fails = Vec::new();
    for rate in RATES {
        let half = window_secs(rate) / 2.0;
        for (ni, &(name, f1)) in [("E2", 82.41), ("A2", 110.0), ("E4", 329.63)].iter().enumerate() {
            let bend = |t: f64| 8.0 * (-t / 0.2).exp();
            let x = at_peak(&render_bent_string(rate, 2.5, &string_partials(f1, rate), &bend, 70 + ni as u64), -12.0);
            let readings = listen(rate, &to_f32(&x));
            let mut worst = 0.0f64;
            for a in readings.iter().filter(|a| a.t(rate) >= 0.15 && a.pitched()) {
                let centre = (a.t(rate) - half).max(0.0);
                let e = cents(a.r.hz as f64, first_partial(f1) * 2f64.powf(bend(centre) / 1200.0));
                worst = worst.max(e.abs());
                if e.abs() > 3.0 {
                    fails.push(format!("{rate} {name}: {e:+.2} c at {:.3} s", a.t(rate)));
                }
            }
            println!("{rate} {name} settling from 8 c sharp: worst {worst:.2} c from the instantaneous first partial");
        }
    }
    report(&fails);
}

#[test]
fn mains_hum_under_the_note() {
    let mut fails = Vec::new();
    for rate in RATES {
        for (ni, &(name, f1)) in [("E2", 82.41), ("A2", 110.0)].iter().enumerate() {
            let secs = 8.0;
            let string = at_peak(&render_string(rate, secs, &string_partials(f1, rate), 80 + ni as u64), -22.0);
            let hum = at_peak(&harmonic(rate, secs, &|_| 50.0, &[1.0, 0.5, 0.33], 90 + ni as u64), -62.0);
            let clean = listen(rate, &to_f32(&string));
            let hummed = listen(rate, &to_f32(&mix(&string, &hum)));
            let (mut worst, mut both, mut until) = (0.0f64, 0, 0.0f64);
            for (c, h) in clean.iter().zip(&hummed) {
                let t = c.t(rate);
                if c.pitched() && h.pitched() && t >= 0.15 {
                    let pull = cents(h.r.hz as f64, c.r.hz as f64);
                    worst = worst.max(pull.abs());
                    both += 1;
                    until = t;
                    if pull.abs() > 1.0 {
                        fails.push(format!("{rate} {name}: pulled {pull:+.2} c at {t:.3} s"));
                    }
                }
                if t > 6.0 && h.pitched() {
                    fails.push(format!("{rate} {name}: {:.2} Hz at {t:.3} s from the hum alone", h.r.hz));
                }
            }
            let hum_only = hummed.iter().filter(|a| a.t(rate) > 6.0).count();
            println!("{rate} {name} over hum 40 dB down: {both} readings both pitched (to {until:.2} s), worst pull {worst:.3} c; {hum_only} readings of the hum alone, none pitched expected");
        }
    }
    report(&fails);
}

#[test]
fn a_string_plucked_again() {
    let mut fails = Vec::new();
    for rate in RATES {
        for (ni, &(name, f1)) in [("E1", 41.20), ("E2", 82.41), ("A2", 110.0), ("E4", 329.63)].iter().enumerate() {
            let partials = string_partials(f1, rate);
            let every = 0.7;
            let mut x = Vec::new();
            for k in 0..4 {
                let secs = if k == 3 { 1.5 } else { every };
                // Each pluck takes the string over: its own phases, from its own start.
                x.extend(render_string(rate, secs, &partials, 100 + 10 * ni as u64 + k));
            }
            let x = at_peak(&x, -12.0);
            let readings = listen(rate, &to_f32(&x));
            let truth = first_partial(f1);
            let mut back = [f64::NAN; 4];
            let mut worst_settled = 0.0f64;
            for a in readings.iter().filter(|a| a.pitched()) {
                let t = a.t(rate);
                let k = ((t / every).floor() as usize).min(3);
                let since = t - k as f64 * every;
                let e = cents(a.r.hz as f64, truth);
                if e.abs() > 50.0 {
                    fails.push(format!("{rate} {name}: {e:+.1} c at {t:.3} s, pluck {k}"));
                }
                if since >= 0.2 {
                    worst_settled = worst_settled.max(e.abs());
                    if e.abs() > 3.0 {
                        fails.push(format!("{rate} {name}: {e:+.2} c at {t:.3} s, {since:.3} s after pluck {k}"));
                    }
                }
                if e.abs() <= 3.0 && back[k].is_nan() {
                    back[k] = since;
                }
            }
            for (k, b) in back.iter().enumerate() {
                if b.is_nan() || *b > 0.2 {
                    fails.push(format!("{rate} {name}: not within 3 c inside 200 ms of pluck {k} ({:.0} ms)", b * 1e3));
                }
            }
            println!(
                "{rate} {name} plucked every 700 ms: within 3 c after {:.0}/{:.0}/{:.0}/{:.0} ms, worst from 200 ms {worst_settled:.2} c",
                back[0] * 1e3,
                back[1] * 1e3,
                back[2] * 1e3,
                back[3] * 1e3
            );
        }
    }
    report(&fails);
}

// ---- 7: the Ear ----

fn tone(rate: u32, f: f64, secs: f64, dbfs: f64, seed: u64) -> Vec<f64> {
    at_peak(&harmonic(rate, secs, &|_| f, &sawtooth(f), seed), dbfs)
}

fn bits(r: Reading) -> [u32; 3] {
    [r.hz.to_bits(), r.level.to_bits(), r.clarity.to_bits()]
}

#[test]
fn ear_keeps_the_guitar_over_noise() {
    let mut fails = Vec::new();
    for rate in RATES {
        let (guitar, _) = pluck_with_noise(rate, 82.41, 4.0, 200);
        let noise = white(guitar.len(), gain(-40.0), 201);
        let rows = ear_beside(rate, &to_f32(&guitar), &to_f32(&noise), None);
        let mut shown = 0;
        for &(frame, e, r0, r1) in &rows {
            if r1.hz > 0.0 {
                fails.push(format!("{rate}: the noise read {} Hz at frame {frame}", r1.hz));
            }
            if r0.hz > 0.0 {
                shown += 1;
                if bits(e) != bits(r0) {
                    fails.push(format!("{rate}: frame {frame}: {e:?} is not the guitar's {r0:?}"));
                }
            } else if e.hz != 0.0 {
                fails.push(format!("{rate}: frame {frame}: {e:?} while the guitar has no pitch"));
            }
        }
        println!("{rate}: guitar over noise, {shown} of {} readings the guitar's pitch", rows.len());
    }
    report(&fails);
}

/// The first reading (index) the Ear shows input 1's pitch, and checks that until then it showed
/// input 0's whenever that had one and nothing else, and from then on input 1's.
fn hand_over(rows: &[(u64, Reading, Reading, Reading)], fails: &mut Vec<String>, case: &str) -> Option<usize> {
    let at = rows.iter().position(|&(_, e, _, r1)| r1.hz > 0.0 && bits(e) == bits(r1));
    let split = at.unwrap_or(rows.len());
    for &(frame, e, r0, _) in &rows[..split] {
        if if r0.hz > 0.0 { bits(e) != bits(r0) } else { e.hz != 0.0 } {
            fails.push(format!("{case}: frame {frame}: {e:?}, input 0 read {r0:?}"));
        }
    }
    for &(frame, e, _, r1) in &rows[split..] {
        if r1.hz > 0.0 && bits(e) != bits(r1) {
            fails.push(format!("{case}: frame {frame} after the hand-over: {e:?}, input 1 read {r1:?}"));
        }
    }
    at
}

#[test]
fn ear_hands_over_after_250_ms_without_pitch() {
    let mut fails = Vec::new();
    for rate in RATES {
        let hold = rate as u64 / 4;
        // Input 0: A2 for 1.5 s, then silence. Input 1: D3 from 0.5 s, as loud.
        let a = concat(&[&tone(rate, 110.0, 1.5, -12.0, 300), &silence(rate, 1.5)]);
        let b = concat(&[&silence(rate, 0.5), &tone(rate, 146.83, 2.5, -12.0, 301)]);
        let rows = ear_beside(rate, &to_f32(&a), &to_f32(&b), None);
        let case = format!("{rate} silence hand-over");
        let Some(at) = hand_over(&rows, &mut fails, &case) else {
            fails.push(format!("{case}: never handed over"));
            continue;
        };
        let last0 = rows[..at].iter().rev().find(|r| r.2.hz > 0.0).map(|r| r.0).unwrap_or(0);
        let gap = rows[at].0 - last0;
        // The hand-over is the first reading at least 250 ms after input 0's last pitch.
        let due = rows.iter().position(|r| r.0 > last0 && r.0 - last0 >= hold && r.3.hz > 0.0).unwrap();
        println!("{case}: input 0's last pitch at {:.3} s, handed over at {:.3} s ({:.0} ms)", last0 as f64 / rate as f64, rows[at].0 as f64 / rate as f64, gap as f64 / rate as f64 * 1e3);
        if at != due {
            fails.push(format!("{case}: handed over at reading {at}, due at {due}"));
        }
        // Both pitched and as loud: input 0 keeps it.
        if rows.iter().any(|r| r.0 < (1.4 * rate as f64) as u64 && r.2.hz > 0.0 && bits(r.1) != bits(r.2)) {
            fails.push(format!("{case}: input 0 lost the display while it had a pitch"));
        }
    }
    report(&fails);
}

#[test]
fn ear_hands_over_to_an_input_6_db_louder_for_250_ms() {
    let mut fails = Vec::new();
    for rate in RATES {
        let hold = rate as u64 / 4;
        for (louder_db, expect) in [(18.0, true), (3.0, false)] {
            // Input 0: A2 throughout at -30 dBFS. Input 1: D3 from 1 s, louder.
            let a = tone(rate, 110.0, 3.0, -30.0, 310);
            let b = concat(&[&silence(rate, 1.0), &tone(rate, 146.83, 2.0, -30.0 + louder_db, 311)]);
            let rows = ear_beside(rate, &to_f32(&a), &to_f32(&b), None);
            let case = format!("{rate} {louder_db} dB louder");
            let at = hand_over(&rows, &mut fails, &case);
            match (at, expect) {
                (Some(at), true) => {
                    let since = rows.iter().position(|r| r.3.hz > 0.0 && r.3.level >= 1.995_262_3 * r.2.level).unwrap();
                    let due = rows.iter().position(|r| r.0 >= rows[since].0 + hold).unwrap();
                    println!("{case}: louder from {:.3} s, handed over at {:.3} s", rows[since].0 as f64 / rate as f64, rows[at].0 as f64 / rate as f64);
                    if at != due {
                        fails.push(format!("{case}: handed over at reading {at}, due at {due}"));
                    }
                }
                (None, false) => println!("{case}: kept by input 0, as it should"),
                (at, _) => fails.push(format!("{case}: hand-over at {at:?}, expected {expect}")),
            }
        }
    }
    report(&fails);
}

#[test]
fn ear_starts_with_the_louder_and_reset_forgets_it() {
    let mut fails = Vec::new();
    for rate in RATES {
        // A2 on input 0 and B2 on input 1 from the same frame, input 1 3 dB louder: input 1 takes it.
        let a = tone(rate, 110.0, 1.0, -15.0, 320);
        let b = tone(rate, 123.47, 1.0, -12.0, 321);
        let rows = ear_beside(rate, &to_f32(&a), &to_f32(&b), None);
        let first = rows.iter().find(|r| r.2.hz > 0.0 || r.3.hz > 0.0).unwrap();
        if !(first.2.hz > 0.0 && first.3.hz > 0.0) {
            fails.push(format!("{rate}: the two tones were not first pitched together"));
        } else if bits(first.1) != bits(first.3) {
            fails.push(format!("{rate}: the first reading is {:?}, not the louder input 1's {:?}", first.1, first.3));
        }
        // Then input 0 louder, after a reset: input 0 takes it at once.
        let a2 = concat(&[&a, &tone(rate, 110.0, 1.0, -12.0, 322)]);
        let b2 = concat(&[&b, &tone(rate, 123.47, 1.0, -15.0, 323)]);
        let reset_at = a.len();
        let rows = ear_beside(rate, &to_f32(&a2), &to_f32(&b2), Some(reset_at));
        let first = rows.iter().find(|r| r.0 > reset_at as u64 && (r.2.hz > 0.0 || r.3.hz > 0.0)).unwrap();
        if bits(first.1) != bits(first.2) {
            fails.push(format!("{rate}: after the reset the first reading is {:?}, not the louder input 0's {:?}", first.1, first.2));
        } else {
            println!("{rate}: louder input 1 shown first; after a reset the louder input 0 at once ({:.0} ms after it)", (first.0 - reset_at as u64) as f64 / rate as f64 * 1e3);
        }
    }
    report(&fails);
}

#[test]
fn ear_on_identical_inputs_is_one_detector() {
    for rate in RATES {
        let (guitar, _) = pluck_with_noise(rate, 110.0, 3.0, 330);
        let x = to_f32(&concat(&[&guitar, &silence(rate, 0.5), &tone(rate, 329.63, 1.0, -20.0, 331)]));
        let rows = ear_beside(rate, &x, &x, Some(samples(rate, 2.0)));
        for &(frame, e, r0, r1) in &rows {
            assert_eq!(bits(r0), bits(r1));
            assert_eq!(bits(e), bits(r0), "{rate}: frame {frame}: {e:?} against one detector's {r0:?}");
        }
        println!("{rate}: identical inputs, {} readings, bit-identical to one detector", rows.len());
    }
}

// ---- 7b: every rate a device can report ----

/// The rates a device can open at: the host takes the endpoint's own.
const DEVICE_RATES: [u32; 12] = [8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000];

/// A sawtooth's harmonics band-limited to what `rate` carries (under 0.45 of it), as a device's own
/// converter would deliver it.
fn sawtooth_at(hz: f64, rate: u32) -> Vec<f64> {
    let top = 5_000f64.min(0.45 * rate as f64);
    let n = ((top / hz).floor() as usize).clamp(1, 20);
    (1..=n).map(|k| 1.0 / k as f64).collect()
}

/// The worst error in cents of the pitched readings from `from` s on, and how many readings had no pitch.
fn held(readings: &[At], rate: u32, from: f64, truth: f64) -> (f64, usize) {
    let late: Vec<&At> = readings.iter().filter(|a| a.t(rate) >= from).collect();
    let worst = late.iter().filter(|a| a.pitched()).map(|a| cents(a.r.hz as f64, truth).abs()).fold(0.0, f64::max);
    (worst, late.iter().filter(|a| !a.pitched()).count())
}

/// At 16 kHz the lowest notes' half period would pass the detector's 256 samples of history if the
/// rate were analysed undecimated (it once was, and A0 indexed out of the ring once it was full): read
/// after read past a full ring, they hold their pitch.
#[test]
fn the_lowest_notes_at_16_khz_hold_past_a_full_ring() {
    let rate = 16_000;
    let mut fails = Vec::new();
    for (i, &(name, f)) in [("A0", 27.5), ("B0", 30.87)].iter().enumerate() {
        let x = to_f32(&at_peak(&harmonic(rate, 3.0, &|_| f, &sawtooth_at(f, rate), 900 + i as u64), -12.0));
        let readings = listen(rate, &x);
        let (worst, unpitched) = held(&readings, rate, 0.5, f);
        println!("16000 {name}: {} readings, worst {worst:.3} c from 0.5 s, {unpitched} unpitched", readings.len());
        if worst > 1.0 || unpitched > 0 {
            fails.push(format!("16000 {name}: worst {worst:.3} c, {unpitched} unpitched"));
        }
    }
    report(&fails);
}

/// Every rate a device can report: a steady E2 and A4 and the range's two ends, 2 s each, never panic
/// and read within 1 cent (the measured worst per rate printed).
#[test]
fn every_device_rate_reads_the_range() {
    let mut fails = Vec::new();
    for &rate in &DEVICE_RATES {
        let mut line = format!("{rate:>6}:");
        for (i, &(name, f)) in [("A0", 27.5), ("E2", 82.41), ("A4", 440.0), ("B6", 1975.53)].iter().enumerate() {
            let x = to_f32(&at_peak(&harmonic(rate, 2.0, &|_| f, &sawtooth_at(f, rate), 1_000 + i as u64), -12.0));
            let readings = listen(rate, &x);
            let (worst, unpitched) = held(&readings, rate, 0.5, f);
            line += &format!("  {name} {worst:.3} c ({unpitched} unpitched)");
            if worst > 1.0 || unpitched > 0 {
                fails.push(format!("{rate} {name}: worst {worst:.3} c, {unpitched} unpitched"));
            }
        }
        println!("{line}");
    }
    report(&fails);
}

// ---- 8: the allocation guard this file leans on is live ----

#[cfg(debug_assertions)]
#[test]
fn the_allocation_guard_counts() {
    let before = violation_count();
    assert_no_alloc(|| drop(std::hint::black_box(vec![0u8; 16])));
    assert!(violation_count() > before, "assert_no_alloc saw no allocation: the pushes' checks would be vacuous");
}

// ---- 9: cost ----

#[test]
#[ignore = "a cost bar: run with --release -- --ignored --nocapture"]
fn cost() {
    let rate = 48_000;
    let secs = 60.0;
    let mut a = Vec::new();
    while a.len() < samples(rate, secs) {
        a.extend(at_peak(&render_string(rate, 2.0, &string_partials(82.41, rate), a.len() as u64), -12.0));
    }
    a.truncate(samples(rate, secs));
    let b = white(a.len(), gain(-40.0), 400);
    let (a, b) = (to_f32(&a), to_f32(&b));
    let mut ear = Ear::new(rate);
    let mut n = 0usize;
    let start = std::time::Instant::now();
    for i in 0..a.len() {
        if ear.push([a[i], b[i]]).is_some_and(|r| r.hz > 0.0) {
            n += 1;
        }
    }
    let cpu = start.elapsed().as_secs_f64();
    println!("Ear at 48 kHz: {secs} s of two inputs in {:.3} s, {:.0} s of audio per second ({n} pitched readings)", cpu, secs / cpu);
}

