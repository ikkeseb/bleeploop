//! COPY (`machine.ts` copy): the whole lane into the first EMPTY lane, never the recorder's, as a block
//! job; the copy plays when its source played on, else it lands STOPPED. The golden jam copies a
//! playing lane; these are the guards around it.

mod common;

use common::{code, Rig};
use lf_engine::{Command, Event, LaneState};

fn copies(rig: &Rig) -> usize {
    rig.events.iter().filter(|e| matches!(e, Event::Copied { .. })).count()
}

fn looping() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig
}

#[test]
fn nothing_to_copy_is_a_no_op() {
    let mut rig = looping();
    rig.press(Command::Copy(1)); // EMPTY
    rig.set_level(0.25);
    rig.press(Command::RecDub(0));
    rig.press(Command::Copy(0)); // OVERDUBBING
    rig.idle();
    assert!(copies(&rig) == 0 && (1..5).all(|i| rig.state(i) == LaneState::Empty));
}

#[test]
fn a_stopped_or_stopping_source_gives_a_stopped_copy() {
    let mut rig = looping();
    rig.press(Command::PlayStop(0));
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!((rig.state(1), rig.pcm(1)), (LaneState::Stopped, rig.pcm(0)));
    rig.press(Command::PlayStop(0));
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert!(rig.lane(0).stop_at.is_some());
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Stopped, "a lane on its way out is not played on");
}

#[test]
fn a_copy_skips_the_recorders_lane() {
    let mut rig = looping();
    rig.press(Command::RecDub(1)); // lane 1 arms for the boundary: it holds the recorder
    rig.press(Command::Copy(0));
    rig.idle();
    assert!(rig.state(1) == LaneState::Recording && rig.state(2) == LaneState::Playing);
}

#[test]
fn a_lane_cleared_while_muted_fades_back_in_when_refilled() {
    let mut rig = looping();
    rig.set_level(0.0);
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::SetMute(1, true));
    rig.advance(4800); // the muted gain is at 0
    rig.press(Command::Clear(1));
    rig.press(Command::Copy(0)); // refills lane 1, which plays on at once
    rig.idle();
    rig.keep_output();
    rig.advance(1);
    let (anchor, master) = (rig.anchor(), rig.master());
    let pos = ((rig.frame - 1 - anchor).rem_euclid(master)) as usize;
    let lane0 = rig.pcm(0)[pos];
    let heard = rig.output.as_ref().unwrap().1[0];
    assert!((heard - lane0).abs() < 0.5 * lane0.abs(), "lane 1 glides up from silence, no pop: {heard} vs {lane0}");
}

#[test]
fn stop_all_with_the_loop_end_stop_on_stops_at_the_loop_end() {
    let mut rig = looping();
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::StopAll);
    let at = rig.lane(0).stop_at.expect("a loop-end stop");
    assert_eq!(rig.state(0), LaneState::Playing);
    rig.advance_to(at + 1);
    assert_eq!(rig.state(0), LaneState::Stopped);
    rig.press(Command::PlayAll);
    rig.press(Command::StopAll);
    assert!(rig.lane(0).stop_at.is_some());
    rig.press(Command::StopAll);
    assert_eq!(rig.state(0), LaneState::Stopped, "a second STOP ALL stops now");
}
