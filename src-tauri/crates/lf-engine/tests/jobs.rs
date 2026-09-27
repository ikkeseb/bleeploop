//! Block jobs (plan § Memory: no loop-sized work in one callback): they are spread over frames, they run
//! ahead of every head that reads or writes what they touch (checked on the rendered output from the
//! very frame they start), only the commands that would collide with a job wait for it, and a jump in the
//! device frame counter moves their schedule instead of making the work it skipped fall due at once (an
//! overdub the jump's gap damaged stops writing, so its delayed undo copy still restores it exactly).

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::looper::{job_frames, JOB_RATE};
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
fn a_jump_while_a_long_loops_undo_copy_runs_rejects_the_layer_and_restores_the_loop_bit_for_bit() {
    // A 16-bar loop at 8 kHz: 256000 positions, an undo copy of 250 frames. The jump carries the write
    // head far past the copy, which the jump delays (`Looper::skip`); the dub goes on for a whole loop.
    let mut rig = Rig::with(Opts { sr: 8000, start: 8000, loop_seconds: 40.0, ..Default::default() });
    rig.set_input(code);
    let master = rig.record_first_take(0, 16, 240);
    rig.set_level(0.0);
    rig.idle();
    let pre = rig.pcm(0);
    assert!(job_frames(master) > 200, "the undo copy outlasts the frames before the jump");
    let rejected = rig.rejected();
    rig.set_input(|f| code(f) / 4.0);
    rig.press(Command::RecDub(0));
    rig.advance(20);
    assert!(rig.engine.looper().busy(), "the undo copy still runs");
    rig.skip(master / 2);
    rig.advance(master + 1000);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.rejected(), rejected + 1, "the jump's gap rejects the layer");
    assert_eq!(rig.state(0), LaneState::Playing);
    let got = rig.pcm(0);
    let wrong: Vec<usize> = (0..pre.len()).filter(|&p| got[p].to_bits() != pre[p].to_bits()).collect();
    assert!(wrong.is_empty(), "{} positions kept the rejected layer, from {:?}", wrong.len(), wrong.first());
    assert!(!rig.lane(0).can_undo, "no undo target: the loop never had one");
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

#[test]
fn a_device_jump_moves_the_jobs_instead_of_owing_their_work_to_one_callback() {
    // A multiply's eight extension jobs (four lanes, each with an undo target), pending when the device
    // frame counter jumps past all of them (the WASAPI join adding the frames it lost).
    let mut rig = Rig::with(Opts { sr: 8000, start: 8000, ..Default::default() });
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 240);
    rig.set_level(0.0);
    rig.idle();
    for _ in 1..4 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    for lane in 0..4u8 {
        rig.set_input(move |f| code(f + 1000 * lane as Frame) / 8.0);
        rig.press(Command::RecDub(lane));
        rig.advance(master / 3);
        rig.press(Command::RecDub(lane));
        rig.set_level(0.0);
        rig.advance(100);
        rig.idle();
    }
    assert!((0..4).all(|i| rig.lane(i).can_undo));
    let loops: Vec<(Vec<f32>, Vec<f32>)> = (0..4).map(|i| (rig.pcm(i), rig.engine.looper().undo_pcm(i).unwrap())).collect();
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(8.0));
    rig.press(Command::RecDub(4));
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.master(), 8 * master, "the multiply committed");
    let work = job_frames(7 * master);
    assert!(rig.engine.looper().busy(), "the extensions run");
    // The jump: past the last job's done frame, several times over.
    rig.skip(8 * work * 4);
    let mut blocks = 0;
    while rig.engine.looper().busy() {
        let before = rig.engine.looper().job_work();
        rig.advance(rig.block as Frame);
        let moved = rig.engine.looper().job_work() - before;
        assert!(moved <= JOB_RATE * rig.block as Frame, "block {blocks} after the jump moved {moved} positions");
        blocks += 1;
    }
    assert!(blocks as Frame >= 8 * work / rig.block as Frame, "the jobs ran on for {blocks} blocks, paced by rendered frames");
    let max = rig.engine.looper().job_step_max();
    assert!(max <= JOB_RATE * rig.block as Frame, "a job step moved {max} positions");
    let tiled = |pcm: &[f32]| (0..8 * master as usize).map(|k| pcm[k % pcm.len()]).collect::<Vec<f32>>();
    for (i, (live, undo)) in loops.iter().enumerate() {
        assert_eq!(rig.pcm(i), tiled(live), "lane {i}: its loop tiled, bit-exact");
        assert_eq!(rig.engine.looper().undo_pcm(i), Some(tiled(undo)), "lane {i}: its undo target tiled, bit-exact");
    }
}
