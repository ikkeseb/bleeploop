//! The hands-free presses (`src/app/actions.ts`) and the gates they pass (`src/ui/looper/gates.ts`), now in
//! the engine: every refusal, spoken as an event with its reason, and the selection they act on. A press
//! acts on the lane the engine has selected when it lands, never on what a feed frame showed the UI: NEXT
//! TRACK then MUTE, REVERSE, COPY or HALVE in one block act on the new lane, HALVE on the loop as it stands
//! then, and HOLD's release where its press acted, only while that lane still captures. Every looper
//! press disarms a pending pedal CLEAR (a pedal's setting toggle says so with `Press`); a setting alone
//! does not.

mod common;

use common::{code, Rig};
use lf_engine::grid::Frame;
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

#[test]
fn the_gates_let_through_what_they_should_and_lane_events_follow() {
    let mut rig = Rig::new();
    let mark = rig.events.len();
    assert_eq!(refusal(&mut rig, 2, Action::RecDub), None, "an EMPTY lane with no recorder records");
    assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Lane { lane: 2, info, .. } if info.state == LaneState::Recording)));
    rig.set_level(0.5);
    rig.advance(rig.seconds(2.5)); // past the count-in: the take records, unarmed
    assert!(!rig.lane(2).armed);
    assert_eq!(refusal(&mut rig, 0, Action::RecDub), Some(Refusal::OtherRecording), "a take in flight holds the recorder");
    rig.press(Command::RecDub(2));
    rig.set_level(0.0);
    assert_eq!(refusal(&mut rig, 2, Action::RecDub), None, "a forward playing lane overdubs");
    rig.idle(); // the short take's padding runs first
    assert_eq!(rig.state(2), LaneState::Overdubbing);
}

/// Every refusal since event `mark`, with its lane.
fn refusals_since(rig: &Rig, mark: usize) -> Vec<(u8, Refusal)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { lane, reason, .. } => Some((lane, reason)),
            _ => None,
        })
        .collect()
}

/// `commands` at the current frame, in order, then that one frame rendered: one block.
fn at_once(rig: &mut Rig, commands: &[Command]) {
    for &command in commands {
        rig.send_at(rig.frame, command);
    }
    rig.advance(1);
}

/// `bars` bars of the frame code on lane 0, copied to lane 1: two playing lanes.
fn two_lanes(bars: Frame) -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, bars, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.state(1), LaneState::Playing);
    rig
}

#[test]
fn mute_reverse_and_copy_act_on_the_lane_selected_in_the_same_block() {
    let mut rig = two_lanes(1);
    rig.press(Command::SelectTrack(0));
    let mark = rig.events.len();
    at_once(&mut rig, &[Command::Action(Action::NextTrack), Command::Action(Action::Mute)]);
    assert_eq!((rig.engine.looper().volume(0).1, rig.engine.looper().volume(1).1), (false, true), "MUTE on the new lane");
    assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Muted { lane: 1, on: true, .. })), "the feed says so");
    rig.press(Command::Action(Action::Mute));
    assert!(!rig.engine.looper().volume(1).1, "a second press unmutes");
    assert!(rig.events.iter().any(|e| matches!(e, Event::Muted { lane: 1, on: false, .. })));
    at_once(&mut rig, &[Command::Action(Action::PrevTrack), Command::Action(Action::Reverse)]);
    rig.advance_to(rig.next_boundary() + 1);
    assert_eq!((rig.lane(0).reversed, rig.lane(1).reversed), (true, false), "REVERSE on the new lane");
    at_once(&mut rig, &[Command::Action(Action::NextTrack), Command::Action(Action::Copy)]);
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Playing);
    assert!(rig.events.iter().any(|e| matches!(e, Event::Copied { from: 1, to: 2, .. })), "COPY from the new lane");
    // A named lane, whatever the selection.
    rig.press(Command::ActionOn(2, Action::Mute));
    assert!(rig.engine.looper().volume(2).1 && rig.engine.looper().selected() == 1);
}

#[test]
fn mute_reverse_and_copy_say_why_they_are_refused() {
    let mut rig = Rig::new();
    assert_eq!(refusal(&mut rig, 3, Action::Mute), Some(Refusal::NoMute));
    assert_eq!(refusal(&mut rig, 3, Action::Reverse), Some(Refusal::NoReverse));
    assert_eq!(refusal(&mut rig, 3, Action::Copy), Some(Refusal::NoCopy));
    let mut rig = two_lanes(1);
    for _ in 2..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    assert_eq!(refusal(&mut rig, 0, Action::Copy), Some(Refusal::NoFreeLane), "every lane holds a loop");
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert_eq!(refusal(&mut rig, 0, Action::Reverse), Some(Refusal::Stopping));
    assert_eq!(refusal(&mut rig, 0, Action::Mute), None, "a stopping lane mutes");
}

#[test]
fn halve_trims_the_lane_selected_in_the_same_block_to_half_the_loop_as_it_stands() {
    let mut rig = two_lanes(4);
    let fpb = rig.fpb();
    let before = rig.pcm(1);
    rig.press(Command::SelectTrack(0));
    at_once(&mut rig, &[Command::Action(Action::NextTrack), Command::Action(Action::Halve)]);
    rig.idle();
    assert!(rig.lane(1).can_undo && !rig.lane(0).can_undo, "HALVE on the new lane");
    let halved: Vec<f32> = (0..before.len()).map(|k| before[k % (2 * fpb) as usize]).collect();
    assert_eq!(rig.pcm(1), halved, "its first two of four bars, repeated");
    // A 7-bar loop halves to 3, rounded down; a named lane leaves the selection be.
    let mut rig = two_lanes(7);
    let before = rig.pcm(1);
    rig.press(Command::SelectTrack(0));
    rig.press(Command::ActionOn(1, Action::Halve));
    rig.idle();
    assert_eq!(rig.pcm(1), (0..before.len()).map(|k| before[k % (3 * fpb) as usize]).collect::<Vec<f32>>());
    assert_eq!(rig.engine.looper().selected(), 0);
    // A multiply on the press's own frame, not yet on any feed: the new loop's half (three of six bars),
    // not the old one's (one of two), and held for the extension with those bars.
    let mut rig = two_lanes(2);
    let old = rig.pcm(0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(6.0));
    rig.press(Command::SelectTrack(0));
    rig.press(Command::RecDub(2));
    let end = rig.end_frame();
    rig.advance_to(end);
    at_once(&mut rig, &[Command::Action(Action::Halve)]);
    assert_eq!(rig.master(), 6 * fpb, "the take multiplied the loop on the press's frame");
    assert!(rig.engine.holding(), "the halve waits for lane 0's extension");
    rig.idle();
    let grown: Vec<f32> = (0..6 * fpb as usize).map(|k| old[k % old.len()]).collect();
    assert_eq!(rig.pcm(0), (0..grown.len()).map(|k| grown[k % (3 * fpb) as usize]).collect::<Vec<f32>>(), "three of the six new bars");
}

#[test]
fn halve_is_refused_as_trim_is() {
    let mut rig = Rig::new();
    assert_eq!(refusal(&mut rig, 2, Action::Halve), Some(Refusal::NoTrim), "an EMPTY lane");
    let mut rig = two_lanes(1);
    assert_eq!(refusal(&mut rig, 0, Action::Halve), Some(Refusal::NoTrim), "a one-bar loop");
    rig.press(Command::RecDub(0));
    assert_eq!(refusal(&mut rig, 0, Action::Halve), Some(Refusal::Capturing), "an overdubbing lane");
}

#[test]
fn a_hold_release_ends_the_capture_its_press_started_while_that_lane_still_captures() {
    // Before the capture starts: a first take's count-in, and a later take's boundary arm, are cancelled.
    let mut rig = Rig::new();
    rig.press(Command::SelectTrack(2));
    rig.press(Command::Action(Action::Hold));
    assert!(rig.lane(2).armed && rig.locked(), "counting in");
    rig.advance(rig.seconds(0.5));
    rig.press(Command::Action(Action::Release));
    assert_eq!(rig.state(2), LaneState::Empty, "the count-in is cancelled");
    assert!(!rig.locked() && rig.master() == 0);
    let mut rig = two_lanes(1);
    rig.press(Command::SelectTrack(2));
    rig.press(Command::Action(Action::Hold));
    assert!(rig.lane(2).armed, "armed for the boundary");
    rig.press(Command::Action(Action::Release));
    assert_eq!(rig.state(2), LaneState::Empty, "the boundary arm is cancelled");
    // After it starts: the take commits (on to its bar line), an overdub's layer commits.
    rig.set_level(0.25);
    rig.press(Command::Action(Action::Hold));
    let start = rig.start_frame();
    rig.advance_to(start + rig.fpb() / 2);
    rig.press(Command::Action(Action::Release));
    assert_eq!(rig.end_frame(), start + rig.fpb(), "the release ends the take on its bar line");
    rig.advance_to(start + rig.fpb() + 1);
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Playing, "the take commits");
    rig.set_level(0.25);
    rig.press(Command::SelectTrack(0));
    rig.press(Command::Action(Action::Hold));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.advance(1000);
    rig.press(Command::Action(Action::Release));
    rig.set_level(0.0);
    assert_eq!(rig.state(0), LaneState::Playing, "the layer commits");
    rig.idle();
    assert!(rig.lane(0).can_undo);
}

#[test]
fn a_hold_release_after_fixed_closed_the_take_starts_nothing() {
    let mut rig = two_lanes(1);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::SelectTrack(2));
    rig.press(Command::Action(Action::Hold));
    rig.advance_to(rig.end_frame() + 100);
    assert_eq!(rig.state(2), LaneState::Playing, "FIXED closed the take");
    let mark = rig.events.len();
    rig.press(Command::Action(Action::Release));
    rig.advance(1000);
    assert_eq!(rig.state(2), LaneState::Playing, "no overdub starts");
    assert!(rig.window().is_none());
    assert_eq!(refusals_since(&rig, mark), [], "and nothing is refused");
}

#[test]
fn a_hold_release_acts_where_its_press_did_whatever_the_selection_since() {
    let mut rig = two_lanes(1);
    rig.set_level(0.25);
    rig.press(Command::SelectTrack(0));
    at_once(&mut rig, &[Command::Action(Action::NextTrack), Command::Action(Action::Hold)]);
    assert_eq!(rig.state(1), LaneState::Overdubbing, "the press acted on the new lane");
    rig.advance(500);
    rig.press(Command::Action(Action::NextTrack));
    rig.press(Command::Action(Action::Release));
    rig.set_level(0.0);
    assert_eq!((rig.state(1), rig.state(2)), (LaneState::Playing, LaneState::Empty), "the release ended lane 1's layer");
    // A second release has no press to answer.
    rig.press(Command::SelectTrack(1));
    rig.press(Command::Action(Action::Release));
    assert_eq!(rig.state(1), LaneState::Playing);
    // A named HOLD presses REC/DUB there and releases there.
    rig.set_level(0.25);
    rig.press(Command::ActionOn(0, Action::RecDub));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.press(Command::ActionOn(0, Action::Release));
    rig.set_level(0.0);
    assert_eq!(rig.state(0), LaneState::Playing);
}

/// A pedal's CLEAR on lane 0, then `between`, then CLEAR again inside the confirm window: did it clear?
fn clears_past(between: &[Command]) -> bool {
    let mut rig = two_lanes(4);
    rig.press(Command::SelectTrack(0));
    rig.press(Command::Action(Action::Clear));
    for &command in between {
        rig.press(command);
    }
    rig.idle();
    rig.press(Command::Action(Action::Clear));
    rig.state(0) == LaneState::Empty
}

#[test]
fn every_looper_press_disarms_a_pending_clear_and_a_setting_alone_does_not() {
    // A pedal's plain press: the engine's own actions, and a setting toggle announced by `Press`.
    assert!(!clears_past(&[Command::Action(Action::Mute)]), "CLEAR, MUTE, CLEAR asks again");
    assert!(!clears_past(&[Command::ActionOn(1, Action::Reverse)]));
    assert!(!clears_past(&[Command::Action(Action::Halve)]), "CLEAR, HALVE, CLEAR asks again");
    assert!(!clears_past(&[Command::Press, Command::SetMetronome(true)]), "CLEAR, a pedal's CLICK, CLEAR asks again");
    assert!(!clears_past(&[Command::Press, Command::SetInputSend(lf_engine::InputSend::Echo, true)]));
    // The on-screen controls' gestures.
    assert!(!clears_past(&[Command::Trim(0, 2)]), "CLEAR, TRIM, CLEAR asks again");
    assert!(!clears_past(&[Command::Reverse(1)]));
    // A setting alone: a slider, a replayed setting.
    assert!(clears_past(&[Command::SetMetronome(true)]), "a setting is no press");
    assert!(clears_past(&[Command::SetVolume(0, 0.5), Command::SetMute(1, true)]));
    assert!(clears_past(&[]));
}
