//! Every gain glide lands on its target: a lane's volume and mute, a slot's gain, an instrument's level
//! and the master volume each glide one-pole, `g = target + (g - target) * coef` per frame, and within
//! 1e-6 of the target (-120 dB) take the target itself. Until then the glide is that formula, bit for
//! bit; from then on a glide to 0 renders exact zeros (never a subnormal) and any other target scales
//! by exactly the target. New with the engine: the Web Audio ramps had no rig guard.
//!
//! Each case renders a pair of runs from the same start, one plain and one with the gain set, the
//! glided one at another block size, and checks the glided output frame by frame against the plain
//! output times the gain the formula gives.

mod common;

use common::Rig;
use lf_engine::grid::Frame;
use lf_engine::{Command, Instrument, NoteTarget};

/// Within this of its target a glide lands on it (the engine's `GLIDE_SNAP`, `src/lib.rs`).
const SNAP: f64 = 1e-6;
const SR: u32 = 48_000;

/// Which way a path reads its glide: a lane and the master scale a frame and then step, a slot and an
/// instrument step and then scale.
#[derive(Clone, Copy, PartialEq)]
enum Order {
    ScaleThenStep,
    StepThenScale,
}

/// The gain each of `n` frames is heard at, gliding from `from` to `target`, and the frame it lands
/// on. Asserts that up to there it is the plain formula, which has not landed by itself.
fn heard_gains(from: f64, target: f64, tau_seconds: f64, n: usize, order: Order) -> (Vec<f64>, usize) {
    let coef = (-1.0 / (tau_seconds * SR as f64)).exp();
    let formula = |g: f64| target + (g - target) * coef;
    let snapped = |g: f64| if (formula(g) - target).abs() < SNAP { target } else { formula(g) };
    let (mut g, mut plain) = (from, from);
    let (mut gains, mut formulas) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for _ in 0..n {
        if order == Order::StepThenScale {
            (g, plain) = (snapped(g), formula(plain));
        }
        gains.push(g);
        formulas.push(plain);
        if order == Order::ScaleThenStep {
            (g, plain) = (snapped(g), formula(plain));
        }
    }
    let landed = gains.iter().position(|&g| g == target).expect("the glide lands within the run");
    assert!(landed > 0 && gains[..landed] == formulas[..landed], "the formula, bit for bit, before the landing");
    assert_ne!(formulas[landed], target, "the formula alone has not landed there");
    assert!(gains[landed..].iter().all(|&g| g == target), "it stays landed");
    (gains, landed)
}

/// Render 40 time constants of both runs (`set` pressed at the first frame of the glided one), and check
/// the glided `tap` against the plain one times the glide from 1 to `target`.
fn glide(name: &str, plain: &mut Rig, glided: &mut Rig, set: Command, target: f64, tau_seconds: f64, order: Order, tap: fn(&Rig) -> Vec<f32>) {
    let n = (40.0 * tau_seconds * SR as f64) as usize;
    assert_eq!(plain.frame, glided.frame);
    plain.keep_output();
    plain.advance(n as Frame);
    glided.keep_output();
    glided.press(set);
    glided.advance(n as Frame - 1);
    let (x, y) = (tap(plain), tap(glided));
    assert_eq!((x.len(), y.len()), (n, n));
    let (gains, landed) = heard_gains(1.0, target, tau_seconds, n, order);
    assert!(landed < n / 2, "{name}: lands within 20 time constants ({landed} frames)");
    assert!(x[landed..].iter().filter(|&&s| s != 0.0).count() > n / 4, "{name}: the plain run sounds after the landing");
    for k in 0..n {
        let want = (gains[k] * x[k] as f64) as f32;
        assert!(y[k] == want, "{name}: frame {k}: {} where the glide gives {want} (lands at {landed})", y[k]);
    }
    assert!(y.iter().all(|s| !s.is_subnormal()), "{name}: no subnormal output");
    if target == 0.0 {
        assert!(y[landed..].iter().all(|&s| s == 0.0), "{name}: exactly silent from the landing on");
    } else {
        assert!(y[landed..].iter().zip(&x[landed..]).all(|(&y, &x)| y == (target * x as f64) as f32), "{name}: exactly the target from the landing on");
    }
}

fn heard(rig: &Rig) -> Vec<f32> {
    rig.output.as_ref().unwrap().1.clone()
}

/// Lane 0 plays a loop of a sawtooth; the input is silent from here on.
fn playing(block: usize) -> Rig {
    let mut rig = Rig::new();
    rig.block = block;
    rig.set_input(|f| (f % 997) as f32 / 997.0 - 0.5);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.advance(4800);
    rig
}

#[test]
fn a_lanes_volume_and_mute_land_on_their_target() {
    let lane = 0.01;
    let (mut plain, mut glided) = (playing(128), playing(37));
    glide("lane volume", &mut plain, &mut glided, Command::SetVolume(0, 1.5), 1.5, lane, Order::ScaleThenStep, heard);
    let (mut plain, mut glided) = (playing(128), playing(1));
    glide("lane mute", &mut plain, &mut glided, Command::SetMute(0, true), 0.0, lane, Order::ScaleThenStep, heard);
}

#[test]
fn the_master_volume_and_mute_land_on_their_target() {
    let master = 0.012;
    let (mut plain, mut glided) = (playing(128), playing(37));
    glide("master volume", &mut plain, &mut glided, Command::SetMasterVolume(0.5), 0.5, master, Order::ScaleThenStep, heard);
    let (mut plain, mut glided) = (playing(128), playing(1));
    glide("master mute", &mut plain, &mut glided, Command::SetMasterMute(true), 0.0, master, Order::ScaleThenStep, heard);
}

#[test]
fn a_slots_gain_lands_on_its_target() {
    // Slot 0 is live and empty: it passes the input, at its gain, to the monitor.
    let rig = |block: usize| {
        let mut rig = Rig::new();
        rig.block = block;
        rig.set_input(|f| 0.5 * (f as f32 * 0.01).sin());
        rig.advance(4800);
        rig
    };
    let slot = 0.012;
    let (mut plain, mut glided) = (rig(128), rig(37));
    glide("slot gain", &mut plain, &mut glided, Command::SetSlotGain(0, 0.5), 0.5, slot, Order::StepThenScale, |r| r.monitor.clone());
    let (mut plain, mut glided) = (rig(128), rig(1));
    glide("slot gain to 0", &mut plain, &mut glided, Command::SetSlotGain(0, 0.0), 0.0, slot, Order::StepThenScale, |r| r.monitor.clone());
}

#[test]
fn an_instruments_level_lands_on_its_target() {
    // A held organ note, alone on the bus.
    let rig = |block: usize| {
        let mut rig = Rig::new();
        rig.block = block;
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Organ)));
        rig.press(Command::NoteOn(60, 0.8));
        rig.advance(4800);
        rig
    };
    let level = 0.012;
    let (mut plain, mut glided) = (rig(128), rig(37));
    glide("instrument level", &mut plain, &mut glided, Command::SetInstrumentGain(Instrument::Organ, 0.5), 0.5, level, Order::StepThenScale, |r| r.bus.clone());
    let (mut plain, mut glided) = (rig(128), rig(1));
    glide("instrument level to 0", &mut plain, &mut glided, Command::SetInstrumentGain(Instrument::Organ, 0.0), 0.0, level, Order::StepThenScale, |r| r.bus.clone());
}
