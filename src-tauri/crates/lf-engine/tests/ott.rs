//! The master OTT (`dsp::ott`, `Command::SetMasterOtt`, `Command::SetMasterOttAmount`): the engine's own
//! design, so these pin its properties rather than a reference render. With every band at unity the
//! three bands null against an AP88 · AP2500 cascade (magnitude and phase) and are flat 20 Hz–20 kHz,
//! at 44.1 and 48 kHz; the static gain curve's points and the ceiling's shape; on, an AMOUNT change and
//! an on/off fade reversed mid-fade, all on frames off the 128-frame quanta, render bit-identical at
//! block sizes 1, 64, 127 and 480, and an AMOUNT change is heard from its own frame; the fade lands
//! exactly on its last frame; finite output for silence, a full-scale square and near-denormal input,
//! and exact silence once a decay has settled; a hard square onset (a plugin instrument) and a
//! five-lane jam leave the device and the Share under −0.3 dBFS; a loud onset after quiet overshoots
//! under 3 dB and −66 dBFS hiss is lifted under 6 dB at AMOUNT 1; off (the default, or switched off and
//! settled) leaves the device output and the Share mix bit for bit as they were, and the click never
//! passes it; a non-finite AMOUNT is dropped and one past 1 clamps.
//!
//! `measurements` (ignored) prints the numbers the design was accepted on: run it with
//! `cargo test -p lf-engine --release --test ott -- --ignored --nocapture measurements`.

mod common;
// Its riffs serve the listening sheet; this file plays a few of them.
#[allow(dead_code)]
#[path = "listen/pluck.rs"]
mod pluck;

use std::f64::consts::{FRAC_1_SQRT_2, PI};
use std::rc::Rc;

use common::{Opts, Rig};
use lf_engine::dsp::compressor::Compressor;
use lf_engine::dsp::ott::{ceiling, gain_db, Ott, CEILING, CEILING_KNEE, DEFAULT_AMOUNT, FADE_SECONDS, MAKEUP_DB};
use lf_engine::grid::Frame;
use lf_engine::{Command, Instrument, LaneState, NoteTarget, SlotEvent, SlotKind, SlotProcessor};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

// ── Signals and helpers ─────────────────────────────────────────────────────────────────────────────

/// A cookbook 2nd-order section in f64 (transposed direct form II): the reference cascade and the
/// pink noise's band limits.
#[derive(Clone, Copy)]
struct Section {
    b: [f64; 3],
    a: [f64; 2],
    s: [f64; 2],
}

#[derive(Clone, Copy)]
enum Kind {
    Lowpass,
    Highpass,
    Allpass,
}

impl Section {
    fn new(kind: Kind, f0: f64, sr: f64) -> Section {
        let w0 = 2.0 * PI * f0 / sr;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = match kind {
            Kind::Lowpass => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            Kind::Highpass => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            Kind::Allpass => [1.0 - alpha, -2.0 * cos, 1.0 + alpha],
        };
        Section { b: b.map(|x| x / a0), a: [-2.0 * cos / a0, (1.0 - alpha) / a0], s: [0.0; 2] }
    }

    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.s[0];
        self.s[0] = self.b[1] * x - self.a[0] * y + self.s[1];
        self.s[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// xorshift64*, seeded: uniform in [-1, 1).
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Fixed-seed pink noise: xorshift64* white through Paul Kellett's refined pink filter, then band-limited
/// by 2nd-order Butterworth sections at 20 Hz (high-pass) and 20 kHz (low-pass, where the rate allows),
/// scaled to `rms_db` dBFS RMS over the whole buffer.
fn pink(seed: u64, frames: usize, sr: u32, rms_db: f64) -> Vec<f32> {
    let mut noise = Noise(seed);
    let mut b = [0.0f64; 7];
    let mut hp = Section::new(Kind::Highpass, 20.0, sr as f64);
    let mut lp = Section::new(Kind::Lowpass, 20000.0f64.min(0.45 * sr as f64), sr as f64);
    let mut out: Vec<f64> = (0..frames)
        .map(|_| {
            let w = noise.next();
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.1538520;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let p = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
            b[6] = w * 0.115926;
            lp.tick(hp.tick(p))
        })
        .collect();
    let rms = (out.iter().map(|x| x * x).sum::<f64>() / frames as f64).sqrt();
    let k = 10f64.powf(rms_db / 20.0) / rms;
    out.iter_mut().for_each(|x| *x *= k);
    out.into_iter().map(|x| x as f32).collect()
}

fn sine(freq: f64, db: f64, frames: usize, sr: u32) -> Vec<f32> {
    let a = 10f64.powf(db / 20.0);
    (0..frames).map(|k| (a * (2.0 * PI * freq * k as f64 / sr as f64).sin()) as f32).collect()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// An OTT at `sr` with AMOUNT `amount`, on or off, settled (its AMOUNT glide and on/off fade done)
/// over a second of silence.
fn settled(sr: u32, amount: f32, on: bool) -> Ott {
    let mut ott = Ott::new(sr);
    ott.set_amount(amount);
    ott.set_on(on);
    let (mut l, mut r) = (vec![0.0f32; sr as usize], vec![0.0f32; sr as usize]);
    ott.process(&mut l, &mut r);
    ott
}

/// `left`/`right` through `ott` in place, 128 frames a call.
fn run(ott: &mut Ott, left: &mut [f32], right: &mut [f32]) {
    for (l, r) in left.chunks_mut(128).zip(right.chunks_mut(128)) {
        ott.process(l, r);
    }
}

/// The first frame where two renders differ, by bits (the OTT never makes a -0.0 the bus did not have).
fn first_difference(a: &[f32], b: &[f32]) -> Option<usize> {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits())
}

// ── The split ──────────────────────────────────────────────────────────────────────────────────────

/// Unity bands against the reference AP88 → AP2500 cascade, on white noise with an impulse in it: the
/// largest residual, dB against the input's peak.
fn null_depth(sr: u32) -> f64 {
    let n = 2 * sr as usize;
    let mut noise = Noise(7);
    let mut x: Vec<f32> = (0..n).map(|_| (0.25 * noise.next()) as f32).collect();
    x[n / 3] = 0.9;
    let mut ott = settled(sr, 0.0, true);
    let (mut l, mut r) = (x.clone(), x.clone());
    run(&mut ott, &mut l, &mut r);
    // `max` folds a NaN away: an output that is no number must fail here, not pass as a deep null.
    assert!(l.iter().chain(&r).all(|v| v.is_finite()), "sr={sr}: a sample that is no number");
    let (mut ap_lo, mut ap_hi) = (Section::new(Kind::Allpass, 88.0, sr as f64), Section::new(Kind::Allpass, 2500.0, sr as f64));
    let mut worst = 0.0f64;
    for (k, &v) in x.iter().enumerate() {
        let want = ap_hi.tick(ap_lo.tick(v as f64)) as f32;
        worst = worst.max((l[k] - want).abs() as f64).max((r[k] - want).abs() as f64);
    }
    db(worst / peak(&x) as f64)
}

/// Unity bands' magnitude response from their impulse response, 20 Hz to 20 kHz (or 0.45 · the rate):
/// the largest deviation from 0 dB.
fn flatness(sr: u32) -> f64 {
    let n = 1 << 17;
    let mut ott = settled(sr, 0.0, true);
    let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
    l[0] = 0.5;
    r[0] = 0.5;
    run(&mut ott, &mut l, &mut r);
    assert!(l.iter().all(|v| v.is_finite()), "sr={sr}: a sample that is no number");
    let mut spectrum: Vec<Complex<f64>> = l.iter().map(|&v| Complex::new(v as f64 / 0.5, 0.0)).collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut spectrum);
    let bin = |f: f64| (f * n as f64 / sr as f64).round() as usize;
    (bin(20.0)..=bin(20000.0f64.min(0.45 * sr as f64))).map(|k| db(spectrum[k].norm()).abs()).fold(0.0, f64::max)
}

#[test]
fn the_unity_bands_null_against_the_allpass_cascade_and_are_flat() {
    for sr in [44100, 48000] {
        let depth = null_depth(sr);
        assert!(depth < -130.0, "sr={sr}: the bands sum to AP88 · AP2500 of the input only to {depth:.1} dB");
        let flat = flatness(sr);
        assert!(flat < 0.05, "sr={sr}: the unity bands deviate {flat:.4} dB from flat");
    }
}

#[test]
fn the_ceiling_is_the_identity_below_its_knee_smooth_through_it_and_never_reaches_its_top() {
    for x in [0.0, 1e-9, 0.25, CEILING_KNEE] {
        assert_eq!(ceiling(x), x);
        assert_eq!(ceiling(-x), -x);
    }
    // Slope 1 into the knee from above, then monotonic, odd and under the ceiling, out to +40 dBFS.
    let h = 1e-7;
    assert!(((ceiling(CEILING_KNEE + h) - CEILING_KNEE) / h - 1.0).abs() < 1e-6);
    // The bend itself, which a hard clip at the ceiling would not match: one room past the knee it is
    // at tanh(1) of the room, and half a room past it its slope is 1/cosh²(0.5).
    let room = CEILING - CEILING_KNEE;
    assert!((ceiling(CEILING_KNEE + room) - (CEILING_KNEE + room * 1f64.tanh())).abs() < 1e-12);
    let mid = CEILING_KNEE + room / 2.0;
    let slope = (ceiling(mid + h) - ceiling(mid - h)) / (2.0 * h);
    assert!((slope - 1.0 / 0.5f64.cosh().powi(2)).abs() < 1e-6, "slope {slope} half a room past the knee");
    let mut last = CEILING_KNEE;
    for k in 1..=100_000 {
        let x = CEILING_KNEE + k as f64 * 1e-3;
        let y = ceiling(x);
        assert!(y >= last, "folds back at {x}");
        assert!(y <= CEILING && ceiling(-x) == -y, "at {x}: {y}");
        last = y;
    }
}

#[test]
fn the_gain_curve_holds_its_static_points() {
    let points = [
        (-90.0, 0.0),
        (-60.0, 0.0),
        // Capped at +12 dB below -54, faded in from -60: the cap times the fade.
        (-57.0, 12.0 * 0.25),
        (-54.0, 12.0 * 0.5),
        // Above -54 the 3:1 lift is under the cap; the fade is full at -48.
        (-51.0, 10.0 * 0.75),
        (-48.0, 8.0),
        (-45.0, 6.0),
        (-36.0, 0.0),
        (-30.0, 0.0),
        (-27.0, 0.0),
        (-24.0, -0.75 * 9.0 / 12.0),
        (-21.0, -2.25),
        (-12.0, -9.0),
        (0.0, -18.0),
    ];
    for (level, want) in points {
        assert!((gain_db(level) - want).abs() < 1e-12, "at {level} dBFS: {} dB, want {want}", gain_db(level));
    }
    // Continuous, and never an inversion: a louder band always leaves louder.
    let mut last = f64::NEG_INFINITY;
    for k in 0..=12000 {
        let level = -110.0 + k as f64 * 0.01;
        let out = level + gain_db(level);
        assert!(out > last, "the curve folds back at {level:.2} dBFS");
        last = out;
    }
}

// ── The engine ─────────────────────────────────────────────────────────────────────────────────────

/// Commands to send at frames past the jam's start.
type Script = Vec<(Frame, Command)>;

/// A short jam on the bus, in blocks of `block`: a held lead chord and a looping take of a pluck riff,
/// `script` sent at its frames past the start, two seconds rendered.
fn jam(block: usize, script: &Script, click: bool) -> Rig {
    let mut rig = Rig::with(Opts { block, loop_seconds: 10.0, ..Opts::default() });
    let riff = pluck::periodic(&pluck::RIFF_A, 12000.0, 48000, 8.0, 0.3);
    rig.set_input(move |f| riff[f as usize % riff.len()]);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    rig.set(Command::SetMetronome(click));
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    let t0 = rig.frame;
    for (at, command) in script {
        rig.send_at(t0 + at, *command);
    }
    rig.keep_output();
    rig.send_at(t0 + 300, Command::NoteOn(64, 0.8));
    rig.send_at(t0 + 301, Command::NoteOn(71, 0.8));
    rig.send_at(t0 + 40_000, Command::NoteOff(64));
    rig.advance(96_000);
    rig
}

/// A plugin instrument that plays a square at ±`amp`, `half` frames a half period, from device frame
/// `start` (silent before): a hard synth onset on the bus before the OTT, with no click and no monitor.
struct Square {
    start: Frame,
    half: Frame,
    amp: f32,
}

impl SlotProcessor for Square {
    fn kind(&self) -> SlotKind {
        SlotKind::Instrument
    }

    fn latency(&self) -> Frame {
        0
    }

    fn process(&mut self, frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (k, y) in out.iter_mut().enumerate() {
            let f = frame + k as Frame;
            *y = if f < self.start {
                0.0
            } else if ((f - self.start) / self.half) % 2 == 0 {
                self.amp
            } else {
                -self.amp
            };
        }
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A square at ±`amp` and `freq` Hz out of a second of silence through the engine at 48 kHz, the OTT on
/// at `amount` (None: off) and settled before it: the device's and the Share's peaks after their
/// limiters and the bus's before them, linear, over the onset and two seconds after it.
fn square_onset(freq: f64, amp: f32, amount: Option<f32>) -> (f64, f64, f64) {
    let mut rig = Rig::new();
    rig.set(Command::SetMetronome(false));
    if let Some(a) = amount {
        rig.set(Command::SetMasterOttAmount(a));
        rig.set(Command::SetMasterOtt(true));
    }
    let start = rig.frame + rig.seconds(1.0);
    rig.install(1, Box::new(Square { start, half: (48000.0 / (2.0 * freq)).round() as Frame, amp }));
    rig.advance(rig.seconds(0.5));
    rig.keep_output();
    rig.advance(rig.seconds(2.5));
    let both = |a: &[f32], b: &[f32]| {
        assert!(a.iter().chain(b).all(|v| v.is_finite()), "a sample that is no number");
        peak(a).max(peak(b)) as f64
    };
    (both(&rig.heard, &rig.heard_right), both(&rig.share, &rig.share_right), both(&rig.bus, &rig.bus_right))
}

#[test]
fn on_an_amount_change_and_a_reversed_fade_render_the_same_at_any_block_size() {
    let fade = (FADE_SECONDS * 48000.0) as Frame;
    let script: Script = vec![
        (1_001, Command::SetMasterOtt(true)),
        (9_333, Command::SetMasterOttAmount(1.0)),
        (30_077, Command::SetMasterOtt(false)),
        // Mid-fade: it turns back from the weight reached.
        (30_077 + fade / 3, Command::SetMasterOtt(true)),
        (50_005, Command::SetMasterOttAmount(0.2)),
        (70_011, Command::SetMasterOtt(false)),
    ];
    let reference = jam(128, &script, true);
    let off = jam(128, &Script::new(), true);
    assert!(first_difference(&reference.bus, &off.bus).is_some_and(|k| k >= 1_001), "the OTT changed the bus from its frame on");
    for block in [1, 64, 127, 480] {
        let rig = jam(block, &script, true);
        for (name, a, b) in [
            ("bus left", &rig.bus, &reference.bus),
            ("bus right", &rig.bus_right, &reference.bus_right),
            ("device left", &rig.heard, &reference.heard),
            ("device right", &rig.heard_right, &reference.heard_right),
            ("share left", &rig.share, &reference.share),
            ("share right", &rig.share_right, &reference.share_right),
        ] {
            assert_eq!(first_difference(a, b), None, "block {block}: the {name} differs from block 128");
        }
    }
}

#[test]
fn an_amount_change_lands_on_its_frame() {
    // The same jam, on from frame 1 001, with and without an AMOUNT change at 30 077 (off the quanta):
    // bit for bit the same before it, different from it on. An engine that dropped AMOUNT commands, or
    // applied one at a block or quantum start, fails here.
    let at = 30_077;
    let on: Script = vec![(1_001, Command::SetMasterOtt(true))];
    let mut moved = on.clone();
    moved.push((at, Command::SetMasterOttAmount(1.0)));
    let (a, b) = (jam(128, &on, false), jam(128, &moved, false));
    for (name, x, y) in [("left", &a.bus, &b.bus), ("right", &a.bus_right, &b.bus_right)] {
        assert_eq!(first_difference(x, y), Some(at as usize), "the {name} bus");
        let late = at as usize + 4_800..at as usize + 9_600;
        assert!(first_difference(&x[late.clone()], &y[late]).is_some(), "the {name} bus 100 ms on");
    }
}

#[test]
fn a_switch_inside_a_fade_turns_from_the_weight_reached() {
    let mut ott = settled(48000, DEFAULT_AMOUNT, false);
    let step = 1.0 / (FADE_SECONDS * 48000.0);
    let (mut l, mut r) = (vec![0.1f32; 200], vec![0.1f32; 200]);
    ott.set_on(true);
    ott.process(&mut l[..160], &mut r[..160]);
    assert!((ott.weight() - 160.0 * step).abs() < 1e-12);
    ott.set_on(false);
    ott.process(&mut l[160..], &mut r[160..]);
    assert!((ott.weight() - 120.0 * step).abs() < 1e-12, "weight {}", ott.weight());
}

#[test]
fn the_fade_lands_exactly_on_its_last_frame() {
    // Counted, not accumulated: 480 frames at 48 kHz reach exactly 1 and exactly 0 (summing 1/480 left
    // 2.9e-15 on the off fade's last frame and settled one frame later).
    let fade = (FADE_SECONDS * 48000.0) as usize;
    let mut ott = settled(48000, DEFAULT_AMOUNT, false);
    let (mut l, mut r) = (vec![0.1f32; fade], vec![0.1f32; fade]);
    ott.set_on(true);
    ott.process(&mut l[..fade - 1], &mut r[..fade - 1]);
    assert!(ott.weight() < 1.0);
    ott.process(&mut l[fade - 1..], &mut r[fade - 1..]);
    assert_eq!(ott.weight(), 1.0);
    ott.set_on(false);
    ott.process(&mut l[..fade - 1], &mut r[..fade - 1]);
    assert!(ott.weight() > 0.0);
    ott.process(&mut l[fade - 1..], &mut r[fade - 1..]);
    assert_eq!(ott.weight(), 0.0);
}

#[test]
fn off_leaves_the_device_output_and_the_share_mix_untouched() {
    let off = jam(128, &Script::new(), true);
    // Settings moved while off: nothing changes.
    let moved = jam(128, &vec![(5, Command::SetMasterOttAmount(1.0)), (6_007, Command::SetMasterOtt(false)), (9_000, Command::SetMasterOttAmount(0.0))], true);
    for (name, a, b) in [
        ("device left", &moved.heard, &off.heard),
        ("device right", &moved.heard_right, &off.heard_right),
        ("share left", &moved.share, &off.share),
        ("share right", &moved.share_right, &off.share_right),
    ] {
        assert_eq!(first_difference(a, b), None, "the {name} moved with the OTT off");
    }
    // On, then off: from the fade's end the bus is the dry bus again, bit for bit (the limiters after it
    // carry their own memory of what it did).
    let fade = (FADE_SECONDS * 48000.0) as usize;
    let back = jam(128, &vec![(1_000, Command::SetMasterOtt(true)), (20_000, Command::SetMasterOtt(false))], true);
    assert_eq!(first_difference(&back.bus[1_000..], &off.bus[1_000..]), Some(0), "it was on from its frame");
    // The fade's last frame (its 480th, from the command's) is already the dry bus.
    let dry = 20_000 + fade - 1;
    assert_eq!(first_difference(&back.bus[dry..], &off.bus[dry..]), None, "off and settled: the dry bus");
    assert_eq!(first_difference(&back.bus_right[dry..], &off.bus_right[dry..]), None);
}

#[test]
fn the_click_never_passes_the_ott() {
    // A count-in over an empty looper: the bus carries the click alone, and with the OTT on it is the
    // same click to the bit.
    let count_in = |on: bool| {
        let mut rig = Rig::new();
        rig.set(Command::SetMasterOttAmount(1.0));
        rig.set(Command::SetMasterOtt(on));
        rig.keep_output();
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(1.5));
        rig
    };
    let (off, on) = (count_in(false), count_in(true));
    assert!(peak(&off.heard) > 0.05, "the count-in clicks");
    assert_eq!(first_difference(&on.heard, &off.heard), None);
}

#[test]
fn a_non_finite_amount_is_dropped_and_one_past_one_clamps() {
    let set = vec![(10, Command::SetMasterOttAmount(0.8)), (11, Command::SetMasterOtt(true))];
    let mut with = set.clone();
    with.extend([(2_003, Command::SetMasterOttAmount(f32::NAN)), (2_004, Command::SetMasterOttAmount(f32::INFINITY)), (2_005, Command::SetMasterOttAmount(f32::NEG_INFINITY))]);
    let (a, b) = (jam(128, &set, false), jam(128, &with, false));
    assert_eq!(first_difference(&a.bus, &b.bus), None, "a value that is no number changed the AMOUNT");
    let one = jam(128, &vec![(10, Command::SetMasterOttAmount(1.0)), (11, Command::SetMasterOtt(true))], false);
    let over = jam(128, &vec![(10, Command::SetMasterOttAmount(3.0)), (11, Command::SetMasterOtt(true))], false);
    assert_eq!(first_difference(&one.bus, &over.bus), None, "3 is 1");
    let mut ott = Ott::new(48000);
    ott.set_amount(0.8);
    ott.set_amount(f32::NAN);
    assert_eq!(ott.amount(), 0.8);
}

#[test]
fn silence_a_full_scale_square_and_near_denormal_input_stay_finite() {
    let sr = 48000;
    let mut ott = settled(sr, 1.0, true);
    let square: Vec<f32> = (0..2 * sr as usize).map(|k| if (k / 240) % 2 == 0 { 1.0 } else { -1.0 }).collect();
    let tiny: Vec<f32> = (0..sr as usize).map(|k| if k % 2 == 0 { 1e-38 } else { -1e-40 }).collect();
    let silence = vec![0.0f32; 20 * sr as usize];
    for (name, x) in [("silence", &silence[..sr as usize]), ("a full-scale square", &square[..]), ("near-denormal input", &tiny[..]), ("a decay into silence", &silence[..])] {
        let (mut l, mut r) = (x.to_vec(), x.to_vec());
        run(&mut ott, &mut l, &mut r);
        assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{name}: a sample that is no number");
        // The crossovers' phase turns a square's edges into spikes (the cascade alone peaks at +5.2
        // dBFS) and the makeup rides on them (+2.8 dBFS for this square at AMOUNT 1 without the
        // ceiling): the ceiling holds them under −1.5 dBFS.
        assert!(peak(&l).max(peak(&r)) as f64 <= CEILING, "{name}: peak {}", peak(&l));
    }
    // Twenty seconds after the input stopped every state has been flushed: exact silence.
    let (mut l, mut r) = (vec![0.0f32; 128], vec![0.0f32; 128]);
    ott.process(&mut l, &mut r);
    assert!(l.iter().chain(&r).all(|&v| v == 0.0));
}

/// −0.3 dBFS: what the ceiling guarantees at the device and in the Share mix, after the limiter's
/// +0.57 dB makeup (measured −0.93 dBFS for every hard square onset below).
const DEVICE_CEILING_DB: f64 = -0.3;

#[test]
fn a_hard_onset_leaves_the_device_and_the_share_under_full_scale() {
    // A square out of silence, a plugin instrument on the bus: the OTT's makeup on the crossovers' phase
    // spikes took the device to +1.05 dBFS at AMOUNT 1 (±0.8, 200 Hz) before the ceiling.
    for (amp, freq) in [(0.8f32, 100.0), (0.8, 200.0), (1.0, 200.0)] {
        for amount in [0.0f32, 0.5, 1.0] {
            let (device, share, _) = square_onset(freq, amp, Some(amount));
            for (name, x) in [("device", device), ("Share", share)] {
                assert!(db(x) <= DEVICE_CEILING_DB, "±{amp} {freq} Hz at AMOUNT {amount}: the {name} at {:+.2} dBFS", db(x));
            }
        }
    }
}

#[test]
fn a_five_lane_jam_leaves_the_device_under_full_scale() {
    let mut rig = five_lanes();
    rig.set(Command::SetMasterOttAmount(1.0));
    rig.set(Command::SetMasterOtt(true));
    rig.advance(rig.seconds(0.5));
    rig.keep_output();
    rig.advance(rig.master());
    let device = peak(&rig.heard).max(peak(&rig.heard_right)) as f64;
    assert!(rig.heard.iter().chain(&rig.heard_right).all(|v| v.is_finite()));
    assert!(db(device) <= DEVICE_CEILING_DB, "the device at {:+.2} dBFS", db(device));
}

// ── Measurements ───────────────────────────────────────────────────────────────────────────────────

/// The input's and output's RMS per band over `window`, frame by frame through `ott` (one frame a
/// call, so its band taps can be read); returns `(band in, band out)` RMS, dBFS, and the output.
fn by_band(ott: &mut Ott, l: &[f32], r: &[f32], window: std::ops::Range<usize>) -> ([f64; 3], [f64; 3], Vec<f32>, Vec<f32>) {
    let (mut pin, mut pout) = ([0.0f64; 3], [0.0f64; 3]);
    let (mut ol, mut or) = (l.to_vec(), r.to_vec());
    for k in 0..l.len() {
        ott.process(&mut ol[k..k + 1], &mut or[k..k + 1]);
        if window.contains(&k) {
            let (bands, gains) = (ott.bands(), ott.gains());
            for b in 0..3 {
                let p = 0.5 * (bands[b][0] * bands[b][0] + bands[b][1] * bands[b][1]);
                pin[b] += p;
                pout[b] += p * gains[b] * gains[b];
            }
        }
    }
    let n = window.len() as f64;
    (pin.map(|p| db((p / n).sqrt())), pout.map(|p| db((p / n).sqrt())), ol, or)
}

/// The instantaneous amplitude of `x` (its analytic signal's magnitude, through an FFT).
fn envelope(x: &[f32]) -> Vec<f64> {
    let n = x.len();
    let mut s: Vec<Complex<f64>> = x.iter().map(|&v| Complex::new(v as f64, 0.0)).collect();
    let mut planner = FftPlanner::new();
    planner.plan_fft_forward(n).process(&mut s);
    for (k, v) in s.iter_mut().enumerate() {
        if k == 0 || (n % 2 == 0 && k == n / 2) {
            continue;
        }
        *v *= if k < n.div_ceil(2) { 2.0 } else { 0.0 };
    }
    planner.plan_fft_inverse(n).process(&mut s);
    s.iter().map(|v| v.norm() / n as f64).collect()
}

/// A single-frequency DFT over whole periods' worth of `x` from `from`: the complex amplitude at `freq`.
fn phasor(x: &[f32], from: usize, len: usize, freq: f64, sr: u32) -> Complex<f64> {
    let mut acc = Complex::new(0.0, 0.0);
    for k in from..from + len {
        let w = 2.0 * PI * freq * k as f64 / sr as f64;
        acc += Complex::new(w.cos(), -w.sin()) * x[k] as f64;
    }
    acc * (2.0 / len as f64)
}

/// The on/off crossfade's dip on a sustained sine at `freq`: on at 1 s, off at 2 s. Returns the deepest
/// momentary drop in each fade from the analytic envelope, and the phasor model's, dB against the dry
/// level.
fn crossfade_dip(freq: f64, level: f64, amount: f32) -> (f64, f64, f64) {
    let sr = 48000;
    let n = 3 * sr as usize;
    let x = sine(freq, level, n, sr);
    let mut ott = settled(sr, amount, false);
    let (mut l, mut r) = (x.clone(), x.clone());
    let s = sr as usize;
    run(&mut ott, &mut l[..s], &mut r[..s]);
    ott.set_on(true);
    run(&mut ott, &mut l[s..2 * s], &mut r[s..2 * s]);
    ott.set_on(false);
    run(&mut ott, &mut l[2 * s..], &mut r[2 * s..]);
    let env = envelope(&l);
    let dry = 10f64.powf(level / 20.0);
    let fade = (FADE_SECONDS * sr as f64) as usize;
    let deepest = |at: usize| db(env[at..at + fade + 1].iter().fold(f64::INFINITY, |m, &v| m.min(v)) / dry);
    // The model: (1 - w) · dry + w · wet, the wet's phasor measured where it is settled.
    let period = (sr as f64 / freq).round() as usize;
    let len = (0.5 * sr as f64 / period as f64).floor() as usize * period;
    let wet = phasor(&l, s + s / 3, len, freq, sr) / phasor(&x, s + s / 3, len, freq, sr);
    let model = (0..=1000).map(|k| k as f64 / 1000.0).map(|w| (Complex::new(1.0 - w, 0.0) + wet * w).norm()).fold(f64::INFINITY, f64::min);
    (deepest(s), deepest(2 * s), db(model))
}

/// Harmonic distortion of a sustained sine through `ott`: the harmonics 2 to 10 against the
/// fundamental, dB, from a Blackman-Harris-windowed FFT of 2^16 settled frames.
fn thd(ott: &mut Ott, freq: f64, level: f64, sr: u32) -> f64 {
    let n = 1 << 16;
    let settle = 3 * sr as usize;
    let x = sine(freq, level, settle + n, sr);
    let (mut l, mut r) = (x.clone(), x.clone());
    run(ott, &mut l, &mut r);
    let mut s: Vec<Complex<f64>> = (0..n)
        .map(|k| {
            let t = 2.0 * PI * k as f64 / n as f64;
            let w = 0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos();
            Complex::new(l[settle + k] as f64 * w, 0.0)
        })
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut s);
    let power = |h: usize| {
        let c = (h as f64 * freq * n as f64 / sr as f64).round() as usize;
        (c - 6..=c + 6).map(|k| s[k].norm_sqr()).sum::<f64>()
    };
    let fundamental = power(1);
    let harmonics: f64 = (2..=10).map(power).sum();
    10.0 * (harmonics / fundamental).log10()
}

/// Five lanes, each its own riff (a pluck phrase at 0.25 peak), a two-bar loop at 120 BPM.
fn five_lanes() -> Rig {
    let mut rig = Rig::with(Opts { loop_seconds: 20.0, ..Opts::default() });
    let unit = 12000.0;
    let riffs: [Rc<Vec<f32>>; 5] = [
        pluck::periodic(&pluck::RIFF_A, unit, 48000, 16.0, 0.25),
        pluck::periodic(&pluck::RIFF_B, unit, 48000, 16.0, 0.25),
        pluck::periodic(&pluck::RIFF_C, unit, 48000, 16.0, 0.25),
        pluck::periodic(&pluck::LAYER, unit, 48000, 16.0, 0.25),
        pluck::periodic(&pluck::STABS, unit, 48000, 16.0, 0.25),
    ];
    let first = riffs[0].clone();
    rig.set_input(move |f| first[f as usize % first.len()]);
    let master = rig.record_first_take(0, 2, 2400);
    rig.set_level(0.0);
    rig.idle();
    for (i, p) in riffs.iter().enumerate().skip(1) {
        rig.press(Command::RecDub(i as u8));
        let start = rig.start_frame();
        let p = p.clone();
        rig.set_input(move |f| if f >= start { p[(f - start) as usize % p.len()] } else { 0.0 });
        rig.advance_to(start + master + 2400);
        rig.press(Command::RecDub(i as u8));
        rig.advance(rig.seconds(0.25));
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(rig.state(i), LaneState::Playing, "lane {i}");
    }
    rig
}

/// White hiss at −66 dBFS RMS, the same on both sides, `n` frames at 48 kHz.
fn hiss(n: usize) -> Vec<f32> {
    let mut noise = Noise(5);
    let a = 10f64.powf(-66.0 / 20.0) * 3f64.sqrt();
    (0..n).map(|_| (a * noise.next()) as f32).collect()
}

/// What the OTT at `amount` adds to −66 dBFS RMS hiss, dB of RMS over seconds 2–4 (makeup included).
fn hiss_lift(amount: f32) -> f64 {
    let x = hiss(4 * 48000);
    let mut ott = settled(48000, amount, true);
    let (mut l, mut r) = (x.clone(), x.clone());
    run(&mut ott, &mut l, &mut r);
    db(rms(&l[2 * 48000..])) - db(rms(&x[2 * 48000..]))
}

/// Plucks at −40 dBFS peak for 2 s, then one at −3 dBFS peak (E2), 48 kHz: the signal and the loud
/// note's first frame.
fn quiet_to_loud() -> (Vec<f32>, usize) {
    let sr = 48000;
    let quiet = pluck::once(&[pluck::n(0.0, 52, 0.9), pluck::n(1.0, 55, 0.9), pluck::n(2.0, 57, 0.9), pluck::n(3.0, 59, 0.9)], 24000.0, sr, 2 * sr as usize, 0.01);
    let loud = pluck::once(&[pluck::accent(0.0, 40, 2.0)], 24000.0, sr, sr as usize, 0.708);
    (quiet.iter().chain(loud.iter()).copied().collect(), quiet.len())
}

/// The input's and the output's peak over the first 50 ms from `onset`.
fn onset_peaks(x: &[f32], out: &[f32], onset: usize) -> (f64, f64) {
    let w = onset..onset + 48000 / 20;
    (peak(&x[w.clone()]) as f64, peak(&out[w]) as f64)
}

#[test]
fn an_onset_after_quiet_and_steady_hiss_ride_no_lift() {
    // The gain falls fast: a loud pluck after quiet ones leaves no more than 3 dB over its input's peak
    // before the limiter, the makeup and the crossovers' phase included (11.1 dB with the slow attacks
    // this replaced).
    let (x, onset) = quiet_to_loud();
    let mut ott = settled(48000, 1.0, true);
    let (mut l, mut r) = (x.clone(), x.clone());
    run(&mut ott, &mut l, &mut r);
    let (pi, po) = onset_peaks(&x, &l, onset);
    assert!(db(po) - db(pi) < 3.0, "the onset overshoots {:+.1} dB", db(po) - db(pi));
    // The noise floor: hiss at −66 dBFS RMS gets no more than 6 dB, the makeup included (15.8 dB before).
    let lift = hiss_lift(1.0);
    assert!(lift < 6.0, "hiss lifted {lift:+.1} dB");
}

#[test]
#[ignore = "a measurement: prints the numbers the OTT was accepted on"]
fn measurements() {
    println!("makeup M = {MAKEUP_DB:+.2} dB, default AMOUNT {DEFAULT_AMOUNT}");

    println!("\n## Split (unity bands, AMOUNT 0, on)");
    for sr in [44100, 48000] {
        println!("sr={sr}: null against AP88 · AP2500 {:.1} dB re peak; flatness 20 Hz-20 kHz: max |dev| {:.6} dB", null_depth(sr), flatness(sr));
    }

    println!("\n## A full-scale 100 Hz square out of a second of silence, AMOUNT 1, 48 kHz, before the limiter");
    {
        let mut ott = settled(48000, 1.0, true);
        let square: Vec<f32> = (0..2 * 48000).map(|k| if (k / 240) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let (mut l, mut r) = (square.clone(), square.clone());
        run(&mut ott, &mut l, &mut r);
        let mut off = settled(48000, 0.0, true);
        let (mut ol, mut or) = (square.clone(), square);
        run(&mut off, &mut ol, &mut or);
        // Without the ceiling: +2.8 dBFS, and +5.2 at AMOUNT 0 (the crossovers' phase alone).
        println!("  peak {:+.1} dBFS (AMOUNT 0: {:+.1} dBFS), the ceiling's", db(peak(&l) as f64), db(peak(&ol) as f64));
    }

    println!("\n## A square out of a second of silence through the engine (a plugin instrument on the bus, no click, no monitor), 48 kHz: peak dBFS");
    for (amp, freq) in [(0.8f32, 100.0), (0.8, 200.0), (1.0, 100.0), (1.0, 200.0)] {
        for amount in [None, Some(0.0f32), Some(0.5), Some(1.0)] {
            let (device, share, bus) = square_onset(freq, amp, amount);
            let name = amount.map_or("OTT off".to_string(), |a| format!("AMOUNT {a}"));
            println!("  ±{amp} {freq} Hz, {name:<10}: device {:+.2}, Share {:+.2}; bus before the limiters {:+.2}", db(device), db(share), db(bus));
        }
    }

    println!("\n## Calibration: pink noise at -18 dBFS RMS, AMOUNT 1, RMS over seconds 2-12, 48 kHz, before the limiter");
    let sr = 48000;
    let n = 12 * sr as usize;
    let settled_from = 2 * sr as usize;
    for (name, seed_r) in [("both sides identical (correlation 1)", None), ("independent sides (correlation ~0)", Some(99u64))] {
        let l = pink(1, n, sr, -18.0);
        let r = seed_r.map_or_else(|| l.clone(), |s| pink(s, n, sr, -18.0));
        let mut ott = settled(sr, 1.0, true);
        let (mut ol, mut or) = (l.clone(), r.clone());
        run(&mut ott, &mut ol, &mut or);
        let w = settled_from..n;
        let (i, o) = (0.5 * (db(rms(&l[w.clone()])) + db(rms(&r[w.clone()]))), 0.5 * (db(rms(&ol[w.clone()])) + db(rms(&or[w.clone()]))));
        println!("{name}: in {i:.2} dBFS, out {o:.2} dBFS, out - in {:+.2} dB (M that zeroes it: {:+.2})", o - i, MAKEUP_DB - (o - i));
    }

    println!("\n## Per band, RMS dBFS in -> out (low / mid / high), total RMS and peak, 48 kHz, seconds 2-6");
    let n = 6 * sr as usize;
    let window = 2 * sr as usize..n;
    let mut inputs: Vec<(String, Vec<f32>)> = Vec::new();
    for level in [-18.0, -30.0, -45.0] {
        inputs.push((format!("pink {level} dBFS RMS"), pink(1, n, sr, level)));
    }
    for level in [-1.0, -50.0] {
        for f in [100.0, 1000.0, 6000.0] {
            inputs.push((format!("sine {f} Hz {level} dBFS"), sine(f, level, n, sr)));
        }
    }
    for amount in [0.5f32, 1.0] {
        println!("AMOUNT {amount}:");
        for (name, x) in &inputs {
            let mut ott = settled(sr, amount, true);
            let (bi, bo, ol, _) = by_band(&mut ott, x, x, window.clone());
            let finite = ol.iter().all(|v| v.is_finite());
            println!(
                "  {name:<26} low {:6.1} -> {:6.1}  mid {:6.1} -> {:6.1}  high {:6.1} -> {:6.1}  | RMS {:6.1} -> {:6.1}, peak {:6.1} -> {:6.1}{}",
                bi[0], bo[0], bi[1], bo[1], bi[2], bo[2],
                db(rms(&x[window.clone()])), db(rms(&ol[window.clone()])),
                db(peak(&x[window.clone()]) as f64), db(peak(&ol[window.clone()]) as f64),
                if finite { "" } else { "  NON-FINITE" }
            );
        }
    }

    println!("\n## On/off crossfade dip (10 ms linear), sustained sine at -12 dBFS, 48 kHz: deepest momentary drop vs the dry level");
    for amount in [0.0f32, DEFAULT_AMOUNT, 1.0] {
        for f in [85.0, 2580.0] {
            let (on, off, model) = crossfade_dip(f, -12.0, amount);
            println!("  AMOUNT {amount}, {f} Hz: switching on {on:.1} dB, switching off {off:.1} dB (phasor model {model:.1} dB)");
        }
    }

    println!("\n## A decaying pluck into steady hiss at -66 dBFS RMS (white, both sides), 48 kHz");
    let n = 10 * sr as usize;
    let note = pluck::once(&[pluck::held(0.0, 52, 16.0, 4.0)], 24000.0, sr, n, 0.5);
    let hiss = hiss(n);
    let x: Vec<f32> = note.iter().zip(&hiss).map(|(a, b)| a + b).collect();
    for amount in [DEFAULT_AMOUNT, 1.0] {
        let mut ott = settled(sr, amount, true);
        let (mut ol, mut or) = (x.clone(), x.clone());
        run(&mut ott, &mut ol, &mut or);
        let win = sr as usize / 10;
        let mut rows = Vec::new();
        let (mut swell, mut worst_rise) = (f64::NEG_INFINITY, 0.0f64);
        let mut last_out = f64::INFINITY;
        for k in (0..n / win).map(|k| k * win) {
            let (i, o) = (db(rms(&x[k..k + win])), db(rms(&ol[k..k + win])));
            swell = swell.max(o - i);
            if k > sr as usize / 2 && o > last_out {
                worst_rise = worst_rise.max(o - last_out);
            }
            last_out = o;
            if k % (sr as usize) == 0 {
                rows.push(format!("{:.0}s {:.1}->{:.1}", k as f64 / sr as f64, i, o));
            }
        }
        let tail = 8 * sr as usize..n;
        let hiss_alone = hiss_lift(amount);
        println!(
            "  AMOUNT {amount}: per second (RMS in->out over 100 ms) {}; largest lift over the tail {swell:+.1} dB; largest rise of the output between 100 ms windows after 0.5 s {worst_rise:+.2} dB; lift at the hiss (seconds 8-10) {:+.1} dB; hiss alone {hiss_alone:+.1} dB",
            rows.join(", "),
            db(rms(&ol[tail.clone()])) - db(rms(&x[tail]))
        );
    }

    println!("\n## Quiet-to-loud: plucks at -40 dBFS peak for 2 s, then one at -3 dBFS peak (48 kHz)");
    let (x, onset) = quiet_to_loud();
    for amount in [DEFAULT_AMOUNT, 1.0] {
        let mut ott = settled(sr, amount, true);
        let (mut ol, mut or) = (x.clone(), x.clone());
        run(&mut ott, &mut ol, &mut or);
        let (pi, po) = onset_peaks(&x, &ol, onset);
        let mut limiter = Compressor::master_limiter(sr as f32);
        let (mut ll, mut lr) = (ol.clone(), or.clone());
        for (k, (a, b)) in ll.chunks_mut(128).zip(lr.chunks_mut(128)).enumerate() {
            limiter.process((k * 128) as Frame, a, b);
        }
        let after = peak(&ll[onset..]) as f64;
        let settled_out = peak(&ol[onset + sr as usize / 2..]) as f64 / peak(&x[onset + sr as usize / 2..]) as f64;
        println!(
            "  AMOUNT {amount}: first 50 ms of the loud note: in peak {:.1} dBFS, out peak {:.1} dBFS before the limiter (overshoot {:+.1} dB; half a second later the gain is {:+.1} dB); after the limiter {:.1} dBFS",
            db(pi), db(po), db(po) - db(pi), db(settled_out), db(after)
        );
    }

    println!("\n## THD, low E (82.4 Hz) at -12 dBFS, 48 kHz (harmonics 2-10 against the fundamental)");
    let mut dry = settled(sr, 0.0, false);
    println!("  off (the method's floor): {:.1} dB", thd(&mut dry, 82.4, -12.0, sr));
    for amount in [DEFAULT_AMOUNT, 1.0] {
        let mut ott = settled(sr, amount, true);
        println!("  AMOUNT {amount}: {:.1} dB", thd(&mut ott, 82.4, -12.0, sr));
    }

    println!("\n## Five-lane jam (five pluck riffs at 0.25 peak, a two-bar loop at 120 BPM), two loops each, 48 kHz");
    let mut rig = five_lanes();
    let m = rig.master();
    for (name, setting) in [("OTT off", None), ("OTT on, AMOUNT 0", Some(0.0f32)), ("OTT on, AMOUNT 0.5", Some(0.5)), ("OTT on, AMOUNT 1", Some(1.0))] {
        if let Some(a) = setting {
            rig.set(Command::SetMasterOttAmount(a));
            rig.set(Command::SetMasterOtt(true));
        }
        rig.advance(rig.seconds(0.5));
        rig.keep_output();
        rig.advance(2 * m);
        let bus = peak(&rig.bus).max(peak(&rig.bus_right)) as f64;
        let device = peak(&rig.heard).max(peak(&rig.heard_right)) as f64;
        println!(
            "  {name:<20} before the limiter: peak {:.2} dBFS, RMS {:.2} dBFS; device output: peak {:.2} dBFS, RMS {:.2} dBFS",
            db(bus), db(rms(&rig.bus)), db(device), db(rms(&rig.heard))
        );
    }
}

#[test]
fn an_amount_set_while_off_and_settled_is_taken_at_once() {
    // As a new engine's replay sends it: the amount, then the switch. It must come on at the amount, as
    // an OTT whose glide had all the time it wanted.
    let signal = sine(220.0, -12.0, 4_800, 48_000);
    let mut at_once = Ott::new(48_000);
    at_once.set_amount(1.0);
    at_once.set_on(true);
    let (mut l1, mut r1) = (signal.clone(), signal.clone());
    at_once.process(&mut l1, &mut r1);

    let mut glided = Ott::new(48_000);
    glided.set_amount(1.0);
    let mut silence = vec![0.0f32; 48_000];
    let mut silence_r = silence.clone();
    glided.process(&mut silence, &mut silence_r);
    glided.set_on(true);
    let (mut l2, mut r2) = (signal.clone(), signal);
    glided.process(&mut l2, &mut r2);
    assert_eq!((l1, r1), (l2, r2), "the amount set while off is in force from the first frame on");
}
