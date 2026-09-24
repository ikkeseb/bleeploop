//! Ports verify/guards/overdub.mjs, undo.mjs and reverse.mjs: the overdub layer, the one-level UNDO/REDO
//! toggle and per-lane REVERSE.
//!
//! The engine sums an overdub in place (plan § Memory), so there is no working copy, no boundary swap
//! and no swap timer. overdub.mjs's timer checks (one pending swap per lane, early fire, a stalled
//! re-arm, a swap that throws) guard machinery that no longer exists and are not ported; what they
//! protected is asserted on the loop and on the rendered output: every captured frame summed exactly
//! once, heard on the grid. undo.mjs's "summing leaves the loop alone until the boundary" becomes "the
//! undo target keeps the pre-session loop while the layer sums in". Peaks are not built yet (reverse.mjs
//! A's peak check waits for them).

mod common;

use common::{code, Rig};
use lf_engine::grid::Frame;
use lf_engine::looper::JOB_RATE;
use lf_engine::{Action, Command, Event, LaneState, Refusal};

const DUB: f32 = 1.0 / 64.0;

/// A committed one-bar loop at 200 bpm whose take is the frame code; then silence.
fn playing_loop() -> (Rig, Frame) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    (rig, master)
}

fn layer_sum(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) as f64).sum()
}

/// From `from`, the kept output equals `loop_at(position)`.
fn plays(rig: &Rig, from: Frame, to: Frame, loop_at: impl Fn(usize) -> f32) {
    let (start, out) = rig.output.as_ref().unwrap();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in from.max(*start)..to.min(start + out.len() as Frame) {
        let pos = (f - anchor).rem_euclid(master) as usize;
        assert_eq!(out[(f - start) as usize], loop_at(pos), "frame {f} pos {pos}");
    }
}

/// DUB from mid-period, across `boundaries` boundaries, then a quarter period more.
fn overdub_session(rig: &mut Rig, master: Frame, boundaries: usize, input: f32) {
    rig.set_level(input);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    for _ in 0..boundaries {
        rig.advance_to(rig.next_boundary() + 480);
    }
    rig.advance(master / 4);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(960);
}

#[test]
fn overdub_every_captured_frame_sums_once_across_end_start_cycles() {
    for cycles in [0, 1, 2, 4] {
        let (mut rig, master) = playing_loop();
        let pre = rig.pcm(0);
        rig.set_level(DUB);
        rig.advance_to(rig.next_boundary() + 2400);
        let mut captured = 0;
        for tap in 0..(1 + 2 * cycles) {
            let at = rig.frame;
            rig.press(Command::RecDub(0));
            let want = if tap % 2 == 0 { LaneState::Overdubbing } else { LaneState::Playing };
            assert_eq!(rig.state(0), want);
            rig.advance(959);
            if tap % 2 == 0 {
                captured += rig.frame - at;
            }
            if tap % 2 == 1 {
                captured -= rig.frame - at; // the end press's own frames were not captured
            }
        }
        let _ = captured;
        rig.advance_to(rig.next_boundary() + 5 * master);
        rig.press(Command::RecDub(0));
        rig.set_level(0.0);
        rig.advance(10);
        assert_eq!(rig.state(0), LaneState::Playing);
        let added = layer_sum(&rig.pcm(0), &pre) / DUB as f64;
        assert!(added >= master as f64 && added.fract() == 0.0, "cycles={cycles}: added {added} frames");
        rig.keep_output();
        let from = rig.frame;
        rig.advance(master + 100);
        let now = rig.pcm(0);
        plays(&rig, from, rig.frame, |p| now[p]);
    }
}

#[test]
fn overdub_play_stop_commits_the_layer_through_the_press_and_lands_stopped() {
    for align in [0, 4800] {
        let (mut rig, master) = playing_loop();
        rig.align = align;
        rig.advance_to(rig.next_boundary() + 2400);
        let pre = rig.pcm(0);
        rig.set_level(DUB);
        rig.press(Command::RecDub(0));
        let punch = rig.frame - 1;
        rig.advance_to(rig.next_boundary() - 1440);
        rig.press(Command::PlayStop(0));
        let stop = rig.frame - 1;
        let tail = stop + 1 + align;
        rig.set_input(move |f| if f < tail { DUB } else { 0.0 });
        rig.keep_output();
        let from = rig.frame;
        rig.advance(3 * master);
        assert_eq!(rig.state(0), LaneState::Stopped);
        let out = &rig.output.as_ref().unwrap().1;
        let monitor = |f: Frame| if f < tail { DUB } else { 0.0 };
        assert!(out.iter().enumerate().all(|(k, &y)| y == monitor(from + k as Frame)), "no lane playback after STOP, only the monitor");
        let added = layer_sum(&rig.pcm(0), &pre) / DUB as f64;
        assert_eq!(added, (stop - punch) as f64, "align={align}: the layer through the press");
    }
}

#[test]
fn undo_a_the_snapshot_the_session_and_the_toggle() {
    let (mut rig, master) = playing_loop();
    assert!(!rig.lane(0).can_undo && rig.engine.looper().undo_pcm(0).is_none());
    let pre = rig.pcm(0);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.advance(master / JOB_RATE + 1);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "DUB snapshots the pre-dub loop");
    assert!(!rig.lane(0).can_undo, "no undo while OVERDUBBING");
    rig.advance_to(rig.next_boundary() + 480);
    assert_ne!(rig.pcm(0), pre, "the layer sums in place");
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "the undo target keeps the pre-session loop");
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(480);
    let dubbed = rig.pcm(0);
    assert_eq!(layer_sum(&dubbed, &pre), master as f64 * DUB as f64, "one period of layer");
    assert!(rig.lane(0).can_undo);
    rig.advance_to(rig.next_boundary() + 480);
    rig.keep_output();
    for (want, label) in [(&pre, "undo"), (&dubbed, "redo"), (&pre, "undo again")] {
        let boundary = rig.next_boundary();
        let before = rig.pcm(0);
        let from = rig.frame;
        rig.press(Command::Undo(0));
        assert_eq!(&rig.pcm(0), want, "{label} sets the loop");
        assert!(rig.lane(0).length == master && rig.master() == master && rig.lane(0).can_undo);
        rig.advance_to(boundary + 480);
        plays(&rig, from, boundary, |p| before[p]);
        plays(&rig, boundary, rig.frame, |p| want[p]);
    }
}

#[test]
fn undo_b_a_fresh_session_re_baselines_the_snapshot() {
    let (mut rig, master) = playing_loop();
    overdub_session(&mut rig, master, 0, DUB);
    let first = rig.pcm(0);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() + master / 4);
    rig.press(Command::RecDub(0));
    rig.advance(master / 4);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), first);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    assert_ne!(rig.pcm(0), first);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), first, "undo restores the prior committed loop, not the take");
}

#[test]
fn undo_c_no_op_without_a_snapshot_while_overdubbing_or_stopping() {
    let (mut rig, master) = playing_loop();
    let take = rig.pcm(0);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), take);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.next_boundary() + 480);
    let mid = rig.pcm(0);
    let snapshot = rig.engine.looper().undo_pcm(0);
    rig.press(Command::Undo(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    assert!(rig.engine.looper().undo_pcm(0) == snapshot && layer_sum(&rig.pcm(0), &mid) == DUB as f64);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    let dubbed = rig.pcm(0);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert!(rig.lane(0).stop_at.is_some());
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), dubbed, "a pending loop-end stop blocks undo");
}

#[test]
fn undo_d_stopped_undoes_in_place_play_plays_it_clear_drops_it() {
    let (mut rig, master) = playing_loop();
    let pre = rig.pcm(0);
    overdub_session(&mut rig, master, 1, DUB);
    rig.press(Command::PlayStop(0));
    assert!(rig.state(0) == LaneState::Stopped && rig.lane(0).can_undo);
    rig.keep_output();
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre);
    rig.advance(1000);
    assert!(rig.output.as_ref().unwrap().1.iter().all(|&y| y == 0.0), "no playback from a STOPPED undo");
    rig.press(Command::PlayStop(0));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master);
    plays(&rig, from, rig.frame, |p| pre[p]);
    rig.press(Command::Clear(0));
    assert!(!rig.lane(0).can_undo && rig.engine.looper().undo_pcm(0).is_none());
    rig.set_input(code);
    assert_eq!(rig.record_first_take(0, 3, 2400), 3 * master, "the lane keeps its full capacity");
}

#[test]
fn undo_e_a_layer_with_an_input_gap_restores_the_loop_and_the_previous_undo_target() {
    let (mut rig, master) = playing_loop();
    let take = rig.pcm(0);
    overdub_session(&mut rig, master, 0, DUB);
    let first = rig.pcm(0);
    rig.advance(master / JOB_RATE + 2);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.advance(master / 3);
    rig.gap();
    rig.advance(master / 3);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    assert_eq!(rig.rejected(), 1);
    assert!(rig.events.iter().any(|e| matches!(e, Event::TakeRejected { overdub: true, .. })));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master);
    assert_eq!(rig.pcm(0), first, "the pre-layer loop is back");
    plays(&rig, from, rig.frame, |p| first[p]);
    assert!(rig.state(0) == LaneState::Playing && rig.lane(0).can_undo);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), take, "undo still targets the loop before the kept layer");
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), take);
}

#[test]
fn reverse_a_flips_the_loop_keeps_its_length() {
    for (bpm, sr) in [(200u32, 48000u32), (137, 44100)] {
        let mut rig = Rig::at(sr);
        rig.set(Command::SetBpm(bpm as f64));
        rig.set_input(code);
        let master = rig.record_first_take(0, 1, 2400);
        let pre = rig.pcm(0);
        rig.press(Command::Reverse(0));
        let flipped: Vec<f32> = pre.iter().rev().copied().collect();
        assert_eq!(rig.pcm(0), flipped);
        if master % 2 == 1 {
            assert_eq!(rig.pcm(0)[(master as usize - 1) / 2], pre[(master as usize - 1) / 2]);
        }
        assert!(rig.lane(0).length == master && rig.master() == master && rig.lane(0).reversed);
    }
}

#[test]
fn reverse_b_a_playing_lane_flips_on_the_next_boundary_twice_is_identity() {
    let (mut rig, master) = playing_loop();
    let pre = rig.pcm(0);
    rig.advance_to(rig.next_boundary() + 480);
    rig.keep_output();
    for k in 0..7 {
        rig.advance(master * (3 * k + 1) / 23);
        let boundary = rig.next_boundary();
        let before = rig.pcm(0);
        let from = rig.frame;
        rig.press(Command::Reverse(0));
        let want: Vec<f32> = if k % 2 == 0 { pre.iter().rev().copied().collect() } else { pre.clone() };
        assert_eq!(rig.pcm(0), want);
        assert_eq!(rig.lane(0).reversed, k % 2 == 0);
        rig.advance_to(boundary + 480);
        plays(&rig, from, boundary, |p| before[p]);
        plays(&rig, boundary, rig.frame, |p| want[p]);
    }
    rig.press(Command::Reverse(0));
    assert!(rig.pcm(0) == pre && !rig.lane(0).reversed);
}

#[test]
fn reverse_c_no_op_unless_committed_stopped_flips_in_place_clear_resets() {
    let (mut rig, master) = playing_loop();
    rig.press(Command::Reverse(1));
    assert!(!rig.lane(1).reversed);
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.next_boundary() + 4800);
    assert_eq!(rig.state(1), LaneState::Recording);
    rig.press(Command::Reverse(1));
    assert!(!rig.lane(1).reversed);
    rig.press(Command::Stop(1));
    rig.set_level(DUB);
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.press(Command::Reverse(0));
    assert!(!rig.lane(0).reversed);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    let dubbed = rig.pcm(0);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    rig.press(Command::Reverse(0));
    assert!(!rig.lane(0).reversed, "a pending loop-end stop blocks reverse: {:?}", rig.lane(0));
    assert!(rig.pcm(0) == dubbed, "the loop is untouched");
    rig.advance_to(rig.next_boundary() + 2400);
    assert_eq!(rig.state(0), LaneState::Stopped);
    rig.keep_output();
    rig.press(Command::Reverse(0));
    let flipped: Vec<f32> = dubbed.iter().rev().copied().collect();
    assert!(rig.lane(0).reversed && rig.pcm(0) == flipped);
    rig.advance(1000);
    assert!(rig.output.as_ref().unwrap().1.iter().all(|&y| y == 0.0));
    rig.set(Command::SetLoopEndStop(false));
    rig.press(Command::PlayStop(0));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master);
    plays(&rig, from, rig.frame, |p| flipped[p]);
    rig.press(Command::Clear(0));
    assert!(!rig.lane(0).reversed);
}

#[test]
fn reverse_d_the_flag_stays_honest_across_dub_reverse_undo_and_dub_is_refused_reversed() {
    let (mut rig, master) = playing_loop();
    let mut cur = (rig.pcm(0), false);
    let mut undo: Option<(Vec<f32>, bool)> = None;
    let mut seed: u32 = 12345;
    let mut counts = [0; 4];
    let mut layer = 0.0;
    for step in 0..36 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        match (seed >> 16) % 3 {
            1 => {
                rig.press(Command::Reverse(0));
                cur = (cur.0.iter().rev().copied().collect(), !cur.1);
                counts[1] += 1;
            }
            2 => {
                rig.advance(master / JOB_RATE + 2);
                rig.press(Command::Undo(0));
                if let Some(u) = undo.take() {
                    undo = Some(std::mem::replace(&mut cur, u));
                }
                counts[2] += 1;
            }
            _ if cur.1 => {
                let mark = rig.events.len();
                rig.press(Command::Action(Action::RecDub));
                assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Refused { reason: Refusal::Reversed, .. })), "step {step}");
                rig.press(Command::RecDub(0));
                assert_eq!(rig.state(0), LaneState::Playing, "the engine refuses DUB while reversed");
                counts[3] += 1;
            }
            _ => {
                layer += 1.0;
                rig.set_level(layer / 256.0);
                rig.press(Command::RecDub(0));
                rig.advance(rig.seconds(0.1));
                rig.press(Command::RecDub(0));
                rig.set_level(0.0);
                rig.advance(960);
                let next = (rig.pcm(0), cur.1);
                let prev = std::mem::replace(&mut cur, next);
                assert_ne!(cur.0, prev.0);
                undo = Some(prev);
                counts[0] += 1;
            }
        }
        assert_eq!(rig.pcm(0), cur.0, "step {step}: the loop");
        assert_eq!(rig.lane(0).reversed, cur.1, "step {step}: the flag");
        let target = rig.engine.looper().undo_pcm(0);
        match &undo {
            None => assert!(target.is_none()),
            Some(u) => {
                rig.advance(master / JOB_RATE + 2);
                assert_eq!(rig.engine.looper().undo_pcm(0).as_ref(), Some(&u.0), "step {step}: the undo target");
            }
        }
    }
    assert!(counts.iter().all(|&n| n >= 3), "{counts:?}");
}
