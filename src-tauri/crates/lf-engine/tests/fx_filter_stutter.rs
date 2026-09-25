//! The FX chain (`dsp::fx`) with its filter and stutter against the Tone references: the fx-filter-*
//! and fx-stutter-* scenarios replayed from the manifest through the whole chain, pitch, delay and
//! reverb bypassed as the probe leaves them. Also: the same bits at any block size, both with the
//! events scheduled ahead as Tone's offline clock does and with them landing live mid-block, and no
//! allocation while rendering or taking a control.
//!
//! Decided while porting: a live control lands on its frame with Tone's `now` = that frame and no
//! lookahead; the stutter restarts its gate at the control's context time, without fx.ts's 256-frame
//! live lead (the lead covers main-thread jitter the engine does not have).

mod common;

use common::fx::{bits, render, Schedule, BLOCKS};
use common::refs::{self, Class};
use lf_engine::dsp::fx::{Ctl, FxChain, FxKind, FxParam, FxTiming};
use lf_engine::dsp::param::QUANTUM;

/// Each scenario with the tightest class it passes.
const SCENARIOS: [(&str, Class); 6] = [
    ("fx-filter-dark-48000", Class::N),
    ("fx-filter-reso-48000", Class::N),
    ("fx-filter-reso-44100", Class::N),
    ("fx-filter-bypass-48000", Class::N),
    ("fx-stutter-8th-48000", Class::N),
    ("fx-stutter-137-48000", Class::N),
];

#[test]
fn filter_and_stutter_scenarios_pass_their_classes() {
    for (id, class) in SCENARIOS {
        let render = render(id, 128, Schedule::Ahead);
        refs::assert_class(id, std::slice::from_ref(&render), class);
        // Beyond N: the chain renders Blink's bits.
        assert!(bits(&render) == bits(&refs::reference(id)[0]), "{id}: not bit-exact");
    }
}

#[test]
fn renders_the_same_bits_at_any_block_size() {
    for (id, _) in SCENARIOS {
        for schedule in [Schedule::Ahead, Schedule::Live(37)] {
            let reference = bits(&render(id, 128, schedule));
            for block in BLOCKS {
                assert!(bits(&render(id, block, schedule)) == reference, "{id}: block {block} differs");
            }
        }
    }
}

#[test]
fn live_param_and_bypass_changes_on_their_ticks_render_the_reference() {
    // Live, Tone's `now` is the tick's frame over the rate rather than Tone's summed clock, which
    // moves a ramp's start by a rounding: a few samples differ in their last bits.
    refs::assert_class("fx-filter-bypass-48000", &[render("fx-filter-bypass-48000", 128, Schedule::Live(0))], Class::N);
    // Not so the stutter's rate change: the reference's Tone clock ran it before rendering began,
    // when Blink's currentTime was still 0, so its new gate plays from frame 0 and the 1/16 gate
    // never sounds. Live, the change lands on its tick; `stutter::tests` checks it lands on the grid.
    let id = "fx-stutter-137-48000";
    let first = refs::scenario(id).events[0].frame as usize;
    assert!(render(id, 128, Schedule::Live(0))[..first] != refs::reference(id)[0][..first]);
}

/// One chain with the filter and the stutter on, per 128 frames at 48 kHz: steady, and with the cutoff
/// ramping all the time (per-frame coefficients in both biquads). Ignored by default (timing on CI is
/// noise):
///   cargo test -p lf-engine --test fx_filter_stutter -- --ignored --nocapture
#[test]
#[ignore]
fn chain_cost_per_quantum() {
    let rate = 48000.0f32;
    let start = Ctl { now: 0.0, frame: 0 };
    let mut states = lf_engine::dsp::fx::default_fx_states();
    states[FxKind::Filter.index()].bypassed = false;
    states[FxKind::Stutter.index()].bypassed = false;
    let x: Vec<f32> = (0..QUANTUM).map(|k| ((k * 37 % 101) as f32 / 50.0) - 1.0).collect();
    let (mut out, mut send) = (vec![0.0f32; QUANTUM], vec![0.0f32; QUANTUM]);
    let quanta = 48000 * 60 / QUANTUM;
    for ramping in [false, true] {
        let mut chain = FxChain::new(rate, Some(&states), start);
        chain.set_timing(FxTiming { anchor: 0.0, beat_period: 0.5 }, start).expect("a valid timing");
        let started = std::time::Instant::now();
        for q in 0..quanta {
            let frame = (q * QUANTUM) as u64;
            if ramping && q % 7 == 0 {
                // A 20 ms ramp spans 7.5 quanta: a new one every 7 keeps the cutoff moving.
                chain.set_param(FxParam::Cutoff, if q % 14 == 0 { 300.0 } else { 6000.0 }, Ctl::at(frame, rate));
            }
            chain.process(frame, &x, &mut out, &mut send);
        }
        let per = started.elapsed().as_secs_f64() / quanta as f64;
        let what = if ramping { "cutoff ramping" } else { "steady" };
        println!("fx chain, filter + stutter on, {what}: {:.2} µs per 128 frames ({:.2} % of the quantum at 48 k)", per * 1e6, per / (128.0 / 48000.0) * 100.0);
        std::hint::black_box((&out, &send));
    }
}
