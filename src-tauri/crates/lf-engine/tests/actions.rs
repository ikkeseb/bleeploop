//! The hands-free presses (`src/app/actions.ts`) and the gates they pass (`src/ui/looper/gates.ts`), now in
//! the engine: every refusal, spoken as an event with its reason, and the selection they act on.

mod common;

use common::{code, Rig};
use lf_engine::{Action, Command, Event, LaneState, Refusal};

fn refusal(rig: &mut Rig, lane: u8, action: Action) -> Option<Refusal> {
    rig.press(Command::SelectTrack(lane));
    let mark = rig.events.len();
    rig.press(Command::Action(action));
    rig.events[mark..].iter().find_map(|e| match *e {
        Event::Refused { lane: l, reason, .. } if l == lane => Some(reason),
        _ => None,
    })
}

fn looping() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig
}

#[test]
fn every_refusal_says_why() {
    let mut rig = Rig::new();
    assert_eq!(refusal(&mut rig, 3, Action::PlayStop), Some(Refusal::Empty));
    assert_eq!(refusal(&mut rig, 3, Action::Undo), Some(Refusal::NoUndo));
    assert_eq!(refusal(&mut rig, 3, Action::Clear), Some(Refusal::NoClear));

    let mut rig = looping();
    rig.press(Command::PlayStop(0));
    assert_eq!(refusal(&mut rig, 0, Action::RecDub), Some(Refusal::PlayFirst));
    rig.press(Command::PlayStop(0));
    rig.press(Command::Reverse(0));
    rig.advance_to(rig.next_boundary() + 1);
    assert_eq!(refusal(&mut rig, 0, Action::RecDub), Some(Refusal::Reversed));
    rig.press(Command::Reverse(0));
    rig.press(Command::RecDub(1)); // lane 2 arms: the recorder is taken
    assert_eq!(refusal(&mut rig, 2, Action::RecDub), Some(Refusal::OtherRecording));
    rig.press(Command::Stop(1));
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert_eq!(refusal(&mut rig, 0, Action::RecDub), Some(Refusal::Stopping));
    assert_eq!(refusal(&mut rig, 0, Action::Undo), Some(Refusal::NoUndo));
    assert_eq!(rig.lane(0).state, LaneState::Playing);
}

#[test]
fn undo_is_refused_while_the_lane_is_stopping() {
    let mut rig = looping();
    rig.set_level(0.25);
    rig.press(Command::RecDub(0));
    rig.advance(1000);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.lane(0).can_undo);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert_eq!(refusal(&mut rig, 0, Action::Undo), Some(Refusal::Stopping));
}

#[test]
fn an_accepted_press_acts_on_the_selected_lane() {
    let mut rig = looping();
    assert_eq!(refusal(&mut rig, 0, Action::PlayStop), None);
    assert_eq!(rig.state(0), LaneState::Stopped);
    rig.press(Command::SelectTrack(9));
    assert_eq!(rig.engine.looper().selected(), 4, "an out-of-range selection clamps to the last lane");
    assert!(rig.events.iter().any(|e| matches!(e, Event::Selected { lane: 4, .. })));
}

#[test]
fn the_clear_confirm_window_is_2_5_seconds_exclusive() {
    for (after, clears) in [(119_999, true), (120_000, false)] {
        let mut rig = looping();
        rig.press(Command::SelectTrack(0));
        let first = rig.frame;
        rig.press(Command::Action(Action::Clear));
        rig.advance_to(first + after);
        rig.press(Command::Action(Action::Clear));
        assert_eq!(rig.state(0) == LaneState::Empty, clears, "second press {after} frames later");
    }
}
