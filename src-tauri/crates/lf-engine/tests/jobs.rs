//! Block jobs (plan § Memory: no loop-sized work in one callback): they are spread over frames, they run
//! ahead of every head that reads or writes what they touch (checked on the rendered output from the
//! very frame they start), and only the commands that would collide with a job wait for it.

mod common;

use common::{code, Rig};
use lf_engine::grid::Frame;
use lf_engine::looper::JOB_RATE;
use lf_engine::{Command, Event, LaneState};

/// A two-bar master of the frame code on lane 0, then silence.
fn looping(bars: Frame) -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, bars, 2400);
    rig.set_level(0.0);
    rig
}

/// From `from` to now, the output is the lanes at the grid phase (the input is silent).
fn plays(rig: &Rig, from: Frame, lanes: &[usize]) {
    let (start, out) = rig.output.as_ref().unwrap();
    let pcms: Vec<Vec<f32>> = lanes.iter().map(|&i| rig.pcm(i)).collect();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in from..rig.frame {
        let pos = (f - anchor).rem_euclid(master) as usize;
        let mut want = 0.0f32;
        for p in &pcms {
            want += p[pos];
        }
        assert_eq!(out[(f - start) as usize], want, "frame {f}, loop position {pos}");
    }
}

#[test]
fn no_job_moves_more_than_its_rate_times_the_block_in_one_step() {
    let mut rig = looping(4); // 384000 frames: every job here spans far more than one block's worth
    rig.press(Command::Copy(0));
    rig.set_level(0.25);
    rig.press(Command::RecDub(0));
    rig.advance(20_000);
    rig.press(Command::RecDub(0));
    rig.idle();
    let max = rig.engine.looper().job_step_max();
    assert!(max > 0 && max <= JOB_RATE * rig.block as Frame, "a job step moved {max} positions");
}

#[test]
fn a_short_later_take_is_tiled_ahead_of_playback_from_its_commit_frame() {
    for bars in [2, 4] {
        tiled_ahead(bars);
    }
}

fn tiled_ahead(bars: Frame) {
    let mut rig = looping(bars);
    let fpb = rig.fpb();
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.start_frame() + fpb * 6 / 5); // 1.2 bars: commits one bar at the press
    rig.set_level(0.0);
    rig.keep_output();
    let from = rig.frame;
    rig.press(Command::RecDub(1));
    assert_eq!(rig.state(1), LaneState::Playing);
    rig.advance(fpb);
    plays(&rig, from, &[0, 1]);
}

#[test]
fn a_discarded_layer_is_restored_ahead_of_playback() {
    let mut rig = looping(1);
    let master = rig.master();
    let pre = rig.pcm(0);
    rig.set_level(0.125);
    rig.press(Command::RecDub(0));
    rig.advance(master / 3);
    rig.gap();
    rig.advance(master + master / 2); // the layer covers every position: playback is inside it
    rig.set_level(0.0);
    rig.keep_output();
    let from = rig.frame;
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    assert_eq!(rig.pcm(0), pre);
    plays(&rig, from, &[0]);
}

#[test]
fn a_layer_rejected_before_its_undo_copy_finished_restores_exactly() {
    let mut rig = looping(4); // the undo copy takes 375 frames
    let pre = rig.pcm(0);
    rig.set_level(0.125);
    rig.press(Command::RecDub(0));
    rig.advance(40);
    rig.gap();
    rig.advance(40);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.pcm(0), pre);
}

#[test]
fn another_lanes_copy_survives_a_rejected_layer() {
    let mut rig = looping(1);
    rig.press(Command::Copy(0)); // lane 1
    rig.idle();
    let master = rig.master();
    rig.set_level(0.125);
    rig.press(Command::RecDub(0));
    rig.advance(master / 4);
    rig.gap();
    rig.advance(100);
    rig.press(Command::Copy(1)); // lane 2, still copying when the layer is rejected
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.events.iter().any(|e| matches!(e, Event::Copied { from: 1, to: 2, .. })));
    assert!(rig.state(2) == LaneState::Playing && rig.pcm(2) == rig.pcm(1));
}

#[test]
fn only_colliding_commands_wait() {
    // A later take's tiling runs on lane 1: REC on lane 2 arms at once, a stop on lane 1 lands at once.
    let mut rig = looping(2);
    let fpb = rig.fpb();
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.start_frame() + fpb * 6 / 5);
    rig.press(Command::RecDub(1));
    assert!(rig.engine.looper().busy());
    rig.press(Command::RecDub(2));
    assert!(rig.window().is_some_and(|w| w.0 == 2), "REC elsewhere is not held");
    rig.press(Command::PlayStop(1));
    assert_eq!(rig.state(1), LaneState::Stopped, "a stop is never held");
    assert!(rig.engine.looper().busy(), "and all of it before the tiling is done");
}
