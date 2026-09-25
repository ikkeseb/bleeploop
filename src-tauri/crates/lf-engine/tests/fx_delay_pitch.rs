//! The FX chain (`dsp::fx`) with its feedback delay and its pitch shift against the Tone references:
//! the fx-delay-* and fx-pitch-* scenarios replayed from the manifest through the whole chain, the other
//! effects bypassed as the probe leaves them. Also: the same bits at any block size, a pitch enabled
//! live mid-block (the PitchShift connects on the enable's context frame and stays), and no allocation
//! while rendering or taking a control.
//!
//! How close each lands: fx-delay-default renders Blink's bits. fx-delay-hot differs only where
//! Blink's audio thread flushes denormals to zero (its 0.95 feedback decays the bypassed filter's
//! leak into them; ±1.2e-38 at most here). The pitch scenarios sit near −70 dB: the oscillator tables
//! are built with an f64 FFT where Blink uses an f32 one (about one float ulp apart, see
//! `dsp::oscillator`), which moves the LFO-driven delay times by an ulp, and Blink's float read
//! position (0.004 frames apart in a 1 s line) turns that into a step of up to 6e-3 on a noise burst.

mod common;

use assert_no_alloc::assert_no_alloc;
use common::fx::{bits, render, Schedule, BLOCKS};
use common::refs::{self, Class};
use common::violation_count;
use lf_engine::dsp::fx::{default_fx_states, Ctl, FxChain, FxKind, FxParam, FxTiming};
use lf_engine::dsp::param::QUANTUM;

/// Each scenario with the tightest class it passes (null residual when ported, in dB).
const SCENARIOS: [(&str, Class); 4] = [
    ("fx-delay-default-48000", Class::N), // bit-exact
    ("fx-delay-hot-48000", Class::N),     // −776.5
    ("fx-pitch-down12-48000", Class::N),  // −71.8
    ("fx-pitch-up7-48000", Class::N),     // −69.4
];

#[test]
fn delay_and_pitch_scenarios_pass_their_classes() {
    for (id, class) in SCENARIOS {
        let render = render(id, 128, Schedule::Ahead);
        refs::assert_class(id, std::slice::from_ref(&render), class);
    }
}

#[test]
fn the_delay_renders_blinks_bits_down_to_the_denormals() {
    let id = "fx-delay-default-48000";
    assert!(bits(&render(id, 128, Schedule::Ahead)) == bits(&refs::reference(id)[0]), "{id}: not bit-exact");
    let id = "fx-delay-hot-48000";
    let (render, reference) = (render(id, 128, Schedule::Ahead), &refs::reference(id)[0]);
    // Blink's zeros are subnormals here, and what the tail adds them to moves by as little.
    for (k, (&a, &b)) in render.iter().zip(reference).enumerate() {
        assert!(a.to_bits() == b.to_bits() || (a - b).abs() < 1e-37, "{id}: frame {k}: {a:e} vs {b:e}");
    }
}

#[test]
fn renders_the_same_bits_at_any_block_size() {
    for (id, _) in SCENARIOS {
        let reference = bits(&render(id, 128, Schedule::Ahead));
        for block in BLOCKS {
            assert!(bits(&render(id, block, Schedule::Ahead)) == reference, "{id}: block {block} differs");
        }
    }
}

/// A chain over the pitch scenarios' input with the pitch at `semitones`, enabled live at `enable`,
/// bypassed again at `disable`, `block` frames per call.
fn render_live_pitch(block: usize, semitones: f64, enable: Option<usize>, disable: Option<usize>) -> Vec<f32> {
    let s = refs::scenario("fx-pitch-up7-48000");
    let x = refs::fx_input(s);
    let rate = s.rate as f32;
    let start = Ctl { now: 0.0, frame: 0 };
    let mut chain = FxChain::new(rate, Some(&default_fx_states()), start);
    chain.set_timing(FxTiming { anchor: 0.0, beat_period: 0.5 }, start).expect("a valid timing");
    chain.set_param(FxParam::Semitones, semitones, start);
    let (mut out, mut send) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let controls: Vec<(usize, bool)> = [(enable, false), (disable, true)].into_iter().filter_map(|(f, b)| f.map(|f| (f, b))).collect();
    let violations = violation_count();
    let mut at = 0;
    while at < s.frames {
        let mut end = (at + block).min(s.frames);
        if let Some(&(f, _)) = controls.iter().find(|(f, _)| *f > at) {
            end = end.min(f);
        }
        let (o, sd) = (&mut out[at..end], &mut send[at..end]);
        assert_no_alloc(|| chain.process(at as u64, &x[at..end], o, sd));
        at = end;
        for &(f, bypassed) in &controls {
            if f == at {
                let ctl = Ctl::at(f as u64, rate);
                assert_no_alloc(|| chain.set_bypass(FxKind::Pitch, bypassed, ctl));
            }
        }
    }
    assert_eq!(violation_count(), violations, "the chain allocated while rendering or taking a control");
    out
}

#[test]
fn a_live_enable_connects_the_pitch_shift_on_its_context_frame_and_keeps_it() {
    let (enable, disable) = (10_037, 30_011);
    let never = render_live_pitch(128, -5.0, None, None);
    let live = render_live_pitch(128, -5.0, Some(enable), Some(disable));
    // Until the quantum after the enable, nothing is connected: the chain renders as if never enabled.
    let connected = enable.next_multiple_of(QUANTUM);
    assert!(bits(&live[..connected]) == bits(&never[..connected]));
    assert!(live[connected..connected + 4800] != never[connected..connected + 4800]);
    // Bypassed again, the PitchShift keeps rendering: its wet path leaks in at −56 dB (a gain of
    // 0.0015 on a wet signal peaking near 1.4).
    let settled = disable + 4800;
    let leak = live[settled..].iter().zip(&never[settled..]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(leak > 0.0 && leak < 2.5e-3, "{leak}");
    for block in BLOCKS {
        assert!(bits(&render_live_pitch(block, -5.0, Some(enable), Some(disable))) == bits(&live), "block {block} differs");
    }
}

/// One chain with the pitch on, per 128 frames at 48 kHz (the other effects bypassed, as every chain
/// renders them). Ignored by default (timing on CI is noise):
///   cargo test -p lf-engine --release --test fx_delay_pitch -- --ignored --nocapture
#[test]
#[ignore]
fn chain_cost_per_quantum_with_pitch_on() {
    let rate = 48000.0f32;
    let start = Ctl { now: 0.0, frame: 0 };
    let x: Vec<f32> = (0..QUANTUM).map(|k| ((k * 37 % 101) as f32 / 50.0) - 1.0).collect();
    let (mut out, mut send) = (vec![0.0f32; QUANTUM], vec![0.0f32; QUANTUM]);
    let quanta = 48000 * 60 / QUANTUM;
    for pitch_on in [false, true] {
        let mut states = default_fx_states();
        states[FxKind::Pitch.index()].bypassed = !pitch_on;
        states[FxKind::Pitch.index()].params[0] = 7.0;
        let mut chain = FxChain::new(rate, Some(&states), start);
        chain.set_timing(FxTiming { anchor: 0.0, beat_period: 0.5 }, start).expect("a valid timing");
        let started = std::time::Instant::now();
        for q in 0..quanta {
            chain.process((q * QUANTUM) as u64, &x, &mut out, &mut send);
        }
        let per = started.elapsed().as_secs_f64() / quanta as f64;
        let what = if pitch_on { "pitch on" } else { "all bypassed" };
        println!("fx chain, {what}: {:.2} µs per 128 frames ({:.2} % of the quantum at 48 k)", per * 1e6, per / (128.0 / 48000.0) * 100.0);
        std::hint::black_box((&out, &send));
    }
}

