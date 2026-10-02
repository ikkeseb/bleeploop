//! What verify/probes/loop-end-stop.mjs measured on the Web Audio render, on the engine's output: END
//! STOP (PLAY/STOP or STOP ALL with the loop-end stop on) sounds through the frame before the loop
//! boundary and is silent from it, at any block size; a lane cleared and refilled while its stop is
//! pending plays on past the old deadline; two lanes stopped together stop on one frame; STOP ALL over a
//! playing lane and an overdub is one gesture, the dub punching out at once (aligned) while the playing
//! lane waits for the boundary.
//!
//! The probe's blocked main thread and its retiring reversed source have no engine counterpart: a stop
//! is a frame the engine renders, not a timer, and a lane reads one buffer. What the rest of the probe
//! asserted lives beside it: the refusals while a stop is pending in `actions.rs` and
//! `overdub_undo_reverse.rs`, a second STOP stopping at once in `copy.rs`, the silent boundary click in
//! `click_grid.rs`, the pending lane's cue and stop-now control in `verify/probes/end-stop-cue.mjs`.

mod common;

use common::dub::{pos_fn, ramp};
use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneState};

const DUB: f32 = 1.0 / 64.0;

/// Lane 0 PLAYING a one-bar loop of the frame code at 120 bpm (lane 1 its COPY when `two`), the
/// input silent, every job done.
fn looping(block: usize, align: Frame, two: bool) -> Rig {
    let mut rig = Rig::with(Opts { block, align, ..Default::default() });
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    if two {
        rig.press(Command::Copy(0));
        rig.idle();
        assert_eq!(rig.state(1), LaneState::Playing);
    }
    rig
}

/// The kept output over device frames `[a, b)`.
fn heard(rig: &Rig, a: Frame, b: Frame) -> &[f32] {
    let (start, out) = rig.output.as_ref().unwrap();
    &out[(a - start) as usize..(b - start) as usize]
}

/// Every kept frame in `[a, b)` is `lanes`' loops summed at the grid phase, plus the monitor: no other
/// lane sounds (`&[]`: the looper is silent).
fn plays(rig: &Rig, a: Frame, b: Frame, lanes: &[usize]) {
    let (start, out) = rig.output.as_ref().unwrap();
    let pcms: Vec<Vec<f32>> = lanes.iter().map(|&i| rig.pcm(i)).collect();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in a..b {
        let k = (f - start) as usize;
        let pos = (f - anchor).rem_euclid(master) as usize;
        let want = pcms.iter().map(|p| p[pos]).sum::<f32>() + rig.monitor[k];
        assert_eq!(out[k], want, "frame {f} (loop position {pos}): lanes {lanes:?}");
    }
}

/// The frame the feed first reported lane `lane` in `state` at or after `from`.
fn reported(rig: &Rig, lane: u8, state: LaneState, from: Frame) -> Option<Frame> {
    rig.events.iter().find_map(|e| match *e {
        Event::Lane { frame, lane: l, info } if l == lane && info.state == state && frame >= from => Some(frame),
        _ => None,
    })
}

#[test]
fn end_stop_sounds_through_the_frame_before_the_boundary_and_is_silent_from_it() {
    let mut kept = Vec::new();
    for block in [1, 37, 128, 1024] {
        let mut rig = looping(block, 0, false);
        rig.set(Command::SetLoopEndStop(true));
        rig.advance_to(rig.next_boundary() - rig.master() / 3);
        rig.keep_output();
        let press = rig.frame;
        rig.press(Command::PlayStop(0));
        let end = rig.next_boundary();
        assert_eq!(rig.lane(0).stop_at, Some(end), "block {block}: the stop waits for the loop boundary");
        assert_eq!(rig.state(0), LaneState::Playing);
        rig.advance_to(end + 4800);
        plays(&rig, press, end, &[0]);
        assert_ne!(heard(&rig, end - 1, end)[0], 0.0, "block {block}: the frame before the boundary sounds");
        plays(&rig, end, rig.frame, &[]);
        assert_eq!(rig.state(0), LaneState::Stopped);
        assert_eq!(reported(&rig, 0, LaneState::Stopped, press), Some(end), "block {block}: STOPPED on the boundary frame");
        kept.push(heard(&rig, press, end + 4800).to_vec());
    }
    assert!(kept.windows(2).all(|w| w[0] == w[1]), "the same edge at every block size");
}

#[test]
fn a_lane_cleared_and_refilled_while_its_stop_is_pending_plays_on_past_the_old_deadline() {
    // Refilled on the same grid: lane 0 cleared, then a COPY of lane 1 into it, which plays on at once.
    let mut rig = looping(128, 0, true);
    rig.set(Command::SetLoopEndStop(true));
    rig.advance_to(rig.next_boundary() - rig.master() / 3);
    // The probe's sequence: pending, a second STOP (now), a resume, pending again.
    rig.press(Command::PlayStop(0));
    let old = rig.lane(0).stop_at.expect("END STOP pending");
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Stopped, "a second STOP stops now");
    rig.press(Command::PlayStop(0));
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.lane(0).stop_at, Some(old), "pending again, for the same boundary");
    rig.press(Command::Clear(0));
    assert_eq!(rig.state(0), LaneState::Empty);
    rig.press(Command::Copy(1));
    rig.idle();
    assert!(rig.state(0) == LaneState::Playing && rig.lane(0).stop_at.is_none(), "lane 0 refilled, playing");
    assert!(rig.frame < old, "the refill plays before the old deadline");
    rig.keep_output();
    let from = rig.frame;
    rig.advance_to(old + rig.master() + 4800);
    assert!(rig.state(0) == LaneState::Playing && rig.lane(0).stop_at.is_none(), "the old deadline stopped nothing");
    assert_eq!(reported(&rig, 0, LaneState::Stopped, from), None);
    plays(&rig, from, rig.frame, &[0, 1]);

    // Refilled on a fresh grid: the only lane cleared (the session blanks), a new first take recorded
    // across the old deadline, then played past it.
    let mut rig = looping(128, 0, false);
    rig.set(Command::SetLoopEndStop(true));
    rig.advance_to(rig.next_boundary() - rig.master() / 3);
    rig.press(Command::PlayStop(0));
    let old = rig.lane(0).stop_at.expect("END STOP pending");
    let cleared = rig.frame;
    rig.press(Command::Clear(0));
    assert!(rig.state(0) == LaneState::Empty && rig.master() == 0);
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    assert!(master > 0 && rig.state(0) == LaneState::Playing && rig.lane(0).stop_at.is_none());
    assert!(rig.frame > old, "the take ran across the old deadline");
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master + 4800);
    assert_eq!(rig.state(0), LaneState::Playing);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, cleared), None, "never stopped since the CLEAR");
    plays(&rig, from, rig.frame, &[0]);
}

#[test]
fn two_lanes_stopped_together_stop_on_one_rendered_frame() {
    // STOP ALL, as the probe did, and each lane's own PLAY/STOP at a different frame of the same loop.
    for (block, stop_all) in [(37, true), (128, true), (128, false)] {
        let mut rig = looping(block, 0, true);
        rig.set(Command::SetLoopEndStop(true));
        rig.advance_to(rig.next_boundary() - rig.master() / 3);
        rig.keep_output();
        let press = rig.frame;
        if stop_all {
            rig.press(Command::StopAll);
        } else {
            rig.press(Command::PlayStop(0));
            rig.advance(rig.master() / 7);
            rig.press(Command::PlayStop(1));
        }
        let end = rig.next_boundary();
        assert_eq!((rig.lane(0).stop_at, rig.lane(1).stop_at), (Some(end), Some(end)), "block {block}, stop all {stop_all}");
        rig.advance_to(end + 4800);
        // Lane 1 is lane 0's copy: both sound through the frame before the boundary (twice the loop),
        // neither from it.
        plays(&rig, press, end, &[0, 1]);
        plays(&rig, end, rig.frame, &[]);
        for i in 0..2 {
            assert_eq!(reported(&rig, i, LaneState::Stopped, press), Some(end), "lane {i}: block {block}, stop all {stop_all}");
        }
    }
}

#[test]
fn stop_all_over_a_playing_lane_and_an_overdub_is_one_gesture() {
    const ALIGN: Frame = 4800;
    for dub in [0usize, 1] {
        let play = 1 - dub;
        let mut rig = looping(128, ALIGN, true);
        let pre = rig.pcm(dub);
        rig.set(Command::SetLoopEndStop(true));
        rig.advance_to(rig.next_boundary() - rig.master() / 2);
        rig.set_level(DUB);
        rig.press(Command::RecDub(dub as u8));
        let punch = rig.frame - 1;
        assert_eq!(rig.state(dub), LaneState::Overdubbing);
        rig.advance(rig.master() / 4);
        rig.keep_output();
        let press = rig.frame;
        // What was played before the press arrives until ALIGN after it; nothing after.
        let tail = press + ALIGN;
        rig.set_input(move |f| if f < tail { DUB } else { 0.0 });
        rig.press(Command::StopAll);
        let end = rig.next_boundary();
        assert!(rig.state(play) == LaneState::Playing && rig.lane(play).stop_at == Some(end), "the playing lane waits for the loop end");
        assert!(rig.lane(dub).stop_at.is_none(), "the overdub is no pending stop");
        rig.advance_to(end + 4800);
        assert!(rig.state(dub) == LaneState::Stopped && rig.lane(dub).can_undo && rig.window().is_none(), "dub on lane {dub}");
        assert_eq!(reported(&rig, dub as u8, LaneState::Stopped, press), Some(tail), "STOPPED once its aligned tail is in");
        let (anchor, master) = (rig.anchor(), rig.master());
        let mut layer = pre.clone();
        common::dub::dub(&mut layer, (punch + ALIGN, press + ALIGN), ramp(rig.sr), 1.0, pos_fn(anchor, master, ALIGN), |_| DUB);
        assert_eq!(rig.pcm(dub), layer, "dub on lane {dub}: the layer is the loop positions through the press, ramped (D23)");
        // The dub is silent from the press, the playing lane sounds up to the boundary.
        plays(&rig, press, end, &[play]);
        plays(&rig, end, rig.frame, &[]);
        assert_eq!(reported(&rig, play as u8, LaneState::Stopped, press), Some(end));
    }
}
