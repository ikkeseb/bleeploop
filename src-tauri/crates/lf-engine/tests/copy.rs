//! COPY (`machine.ts` copy): the whole lane into the first EMPTY lane, never the recorder's nor the one a
//! RETAKE handoff records to, as a block job; the copy plays when its source played on, else it lands
//! STOPPED, as it does when a STOP or a STOP ALL reaches it before it is done. The golden jam copies a
//! playing lane; these are the guards around it. WIDE COPY (engine only) is COPY with the copy's loop 20 ms behind its source's, circularly,
//! and its pan the source's mirror.

mod common;

use common::{code, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Action, Command, Event, LaneState, Refusal};

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

#[test]
fn a_stop_while_a_copy_runs_keeps_the_copy_stopped() {
    for (gesture, end_stop) in [("stop", false), ("stop all", false), ("stop all", true)] {
        let tag = format!("{gesture}, END STOP {end_stop}");
        let mut rig = looping();
        rig.set(Command::SetLoopEndStop(end_stop));
        rig.press(Command::Copy(0));
        assert!(rig.engine.looper().busy() && rig.state(1) == LaneState::Stopped, "{tag}: the copy still runs");
        rig.press(if gesture == "stop" { Command::Stop(1) } else { Command::StopAll });
        rig.idle();
        assert_eq!(copies(&rig), 1, "{tag}");
        assert_eq!(rig.pcm(1), rig.pcm(0), "{tag}: the copy is whole");
        assert_eq!(rig.state(1), LaneState::Stopped, "{tag}: the stopped copy is not played on");
        if gesture == "stop" {
            continue;
        }
        rig.advance_to(rig.next_boundary() + 1);
        assert!((0..5).all(|i| rig.state(i) != LaneState::Playing), "{tag}: every lane stopped");
        rig.keep_output();
        rig.advance(rig.master());
        assert!(rig.output.as_ref().unwrap().1.iter().all(|&y| y == 0.0), "{tag}: silent");
    }
}

/// Lane 0's and lane 1's pan targets, and where lane 1's pan is.
fn pans(rig: &Rig) -> (f32, f32, f64) {
    let fx = rig.engine.fx();
    (fx.pan(0), fx.pan(1), fx.pan_position(1))
}

#[test]
fn a_wide_copy_is_its_source_20_ms_behind_on_the_other_side() {
    // (reversed, the source's pan, the source's and the copy's pans after): a centred source (|pan| under
    // 0.1) goes half left and the copy half right; another takes the mirror of its pan.
    for (reversed, pan, want) in [(false, 0.0, (-0.5, 0.5)), (false, 0.6, (0.6, -0.6)), (true, -0.05, (-0.5, 0.5)), (true, -0.3, (-0.3, 0.3))] {
        let tag = format!("reversed {reversed}, pan {pan}");
        let mut rig = looping();
        if reversed {
            rig.press(Command::Reverse(0));
            rig.advance_to(rig.next_boundary() + 1);
        }
        rig.press(Command::SetPan(0, pan));
        rig.advance(rig.seconds(0.3));
        rig.press(Command::CopyWide(0));
        assert_eq!((pans(&rig).1, pans(&rig).2), (want.1, want.1 as f64), "{tag}: the copy sits on its side from the press");
        rig.idle();
        rig.advance(rig.seconds(0.3));
        assert_eq!(copies(&rig), 1, "{tag}: answered as COPY is");
        assert_eq!((rig.state(1), rig.lane(1).reversed), (LaneState::Playing, reversed), "{tag}");
        assert_eq!(pans(&rig).0, want.0, "{tag}: the source's pan");
        assert_eq!(pans(&rig).1, want.1, "{tag}: the copy's pan");
        // Heard order (the rig's `pcm` reads a reversed lane backwards): the copy at loop position h plays
        // the source's h - 20 ms, around the loop edge.
        let (src, copy) = (rig.pcm(0), rig.pcm(1));
        let (m, d) = (src.len(), 960);
        assert_eq!(rig.sr, 48_000);
        assert_eq!(copy.len(), m, "{tag}");
        assert!(src.windows(2).all(|w| w[0] != w[1]), "{tag}: every sample of the source tells its place");
        assert_eq!((copy[d - 1], copy[d], copy[m - 1]), (src[m - 1], src[0], src[m - 1 - d]), "{tag}: frame-exact at the loop edge");
        let shifted: Vec<f32> = (0..m).map(|h| src[(h + m - d) % m]).collect();
        assert!(copy == shifted, "{tag}: the copy is its source shifted by 20 ms");
    }
}

#[test]
fn a_plain_copy_is_its_source_at_its_pan() {
    let mut rig = looping();
    rig.press(Command::SetPan(0, 0.6));
    rig.advance(rig.seconds(0.3));
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.pcm(1), rig.pcm(0));
    assert_eq!(pans(&rig), (0.6, 0.6, 0.6f32 as f64));
}

#[test]
fn a_wide_copy_is_gated_and_refused_as_copy_is() {
    let mut rig = Rig::new();
    rig.press(Command::Action(Action::CopyWide));
    assert!(rig.events.iter().any(|e| matches!(e, Event::Refused { lane: 0, reason: Refusal::NoCopy, .. })), "nothing to copy");
    let mut rig = looping();
    rig.press(Command::ActionOn(0, Action::CopyWide));
    rig.idle();
    assert_eq!((copies(&rig), rig.state(1)), (1, LaneState::Playing), "a named lane");
    assert_ne!(rig.pcm(1), rig.pcm(0), "the action copies wide");
}

/// A reversed one-bar loop on lane 0 at 120 BPM, `fill` copies of it, then RETAKE rolling on `roller` past
/// its first pass, REC on `next` approving it inside the grace (it finishes its second pass and `next`
/// records from that edge), and `presses` 100 frames before the edge. Frame code as the input, so the
/// frames a lane holds name themselves. The rig, the edge and the master.
fn copy_in_retakes_grace(fill: u8, roller: u8, next: u8, presses: &[Command]) -> (Rig, Frame, Frame) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.press(Command::Reverse(0));
    rig.advance(master);
    rig.idle();
    assert!(rig.lane(0).reversed);
    for _ in 0..fill {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.set(Command::SetRetake(true));
    rig.press(Command::RecDub(roller));
    let edge = rig.start_frame() + 2 * master;
    rig.advance_to(edge - 3000);
    rig.press(Command::RecDub(next));
    rig.advance_to(edge - 100);
    for &command in presses {
        rig.press(command);
    }
    rig.advance_to(edge + 2 * master + master / 2);
    // Still rolling: the stop keeps its last whole pass, the one from a master past the edge.
    rig.press(Command::RecDub(next));
    rig.idle();
    (rig, edge, master)
}

/// `lane` holds what the input played over `[from, from + master)`, forwards.
fn holds_the_input(rig: &Rig, lane: usize, from: Frame, master: Frame) {
    assert!(!rig.lane(lane).reversed, "lane {lane} takes nothing of the copy's REVERSE");
    assert_eq!(rig.state(lane), LaneState::Playing);
    let want: Vec<f32> = (from..from + master).map(code).collect();
    assert!(rig.pcm(lane) == want, "lane {lane} holds the input over its window");
}

#[test]
fn a_copy_in_retakes_grace_skips_the_lane_the_handoff_records_to() {
    let (rig, edge, master) = copy_in_retakes_grace(0, 1, 2, &[Command::Copy(0)]);
    holds_the_input(&rig, 2, edge + master, master);
    assert_eq!(copies(&rig), 1);
    assert!(rig.lane(3).reversed && rig.state(3) == LaneState::Playing && rig.pcm(3) == rig.pcm(0), "the copy took the next EMPTY lane");
}

#[test]
fn a_copy_in_retakes_grace_with_only_the_handoffs_lane_empty_does_nothing_and_says_nothing() {
    // `Looper::copy` finds no lane; `Action::Copy`'s NoFreeLane counts the handoff's lane as free, as it
    // does a lane armed for its boundary, so neither way answers.
    let (rig, edge, master) = copy_in_retakes_grace(2, 3, 4, &[Command::Copy(0), Command::ActionOn(0, Action::Copy)]);
    holds_the_input(&rig, 4, edge + master, master);
    assert_eq!(copies(&rig), 2, "only the two before the roll");
    assert!(!rig.events.iter().any(|e| matches!(e, Event::Refused { .. })), "{:?}", rig.events);
}
