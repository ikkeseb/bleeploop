//! The Stage 2 cost bar: five lanes (one overdubbing) plus the click at 48 kHz in 64-frame blocks
//! should take under 10 % of the block's real time, measured offline. Ignored by default (timing on a
//! shared CI runner is noise); run it on the PC:
//!
//!   cargo test -p lf-engine --test perf -- --ignored --nocapture
//!
//! lf-engine builds at opt-level 3 in the dev profile, so this measures optimized engine code. The
//! limiter is not in the engine yet (plan Stage 3), so it is not in this number.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Dry, LaneState, ProcessContext};
use std::time::Instant;

#[test]
#[ignore]
fn five_lanes_one_overdubbing_and_the_click_cost_under_a_tenth_of_the_block() {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set(Command::SetMetronome(true));
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 2400);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::RecDub(0));
    assert!((1..5).all(|i| rig.state(i) == LaneState::Playing) && rig.state(0) == LaneState::Overdubbing);

    let block = 64usize;
    let blocks = 48000 * 60 / block;
    let input: Vec<f32> = (0..block).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
    let mut frame = rig.frame;
    let mut worst = 0.0f64;
    let mut times = Vec::with_capacity(blocks);
    let started = Instant::now();
    for _ in 0..blocks {
        let t = Instant::now();
        let ctx = ProcessContext { frame, xrun: false, align_frames: 0 };
        rig.engine.process(&ctx, &input, &mut left, &mut right, &mut Dry);
        let dt = t.elapsed().as_secs_f64();
        worst = worst.max(dt);
        times.push(dt);
        frame += block as Frame;
    }
    let total = started.elapsed().as_secs_f64();
    let period = block as f64 / 48000.0;
    let mean = total / blocks as f64 / period;
    times.sort_by(f64::total_cmp);
    let p999 = times[blocks * 999 / 1000] / period;
    println!(
        "master {master} frames; {blocks} blocks: mean {:.2} %, p99.9 {:.2} %, worst {:.1} % of the block",
        mean * 100.0,
        p999 * 100.0,
        worst / period * 100.0
    );
    assert!(mean < 0.10, "mean block time {:.2} % of the block", mean * 100.0);
}
