//! STATUS E3, a take recording when the audio device drops (`Engine::punch_out`, `Looper::punch_out`):
//! punch out at the last frame and keep it. The device owner calls it once the streams are down; the
//! next block continues the frame counter where the last one ended. New with the engine: the Web Audio
//! looper had no device loss of its own to guard.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneInfo, LaneState, TRACK_COUNT};

const DUB: f32 = 1.0 / 64.0;

/// The last Lane event for `lane` since `mark`, and the Transport one.
fn lane_event(rig: &Rig, mark: usize, lane: u8) -> Option<(Frame, LaneInfo)> {
    rig.events[mark..].iter().rev().find_map(|e| match *e {
        Event::Lane { frame, lane: l, info } if l == lane => Some((frame, info)),
        _ => None,
    })
}

fn transport_event(rig: &Rig, mark: usize) -> Option<(Frame, Frame)> {
    rig.events[mark..].iter().rev().find_map(|e| match *e {
        Event::Transport { frame, master, .. } => Some((frame, master)),
        _ => None,
    })
}

fn mismatches(pcm: &[f32], from: Frame) -> usize {
    pcm.iter().enumerate().filter(|&(k, &x)| x != code(from + k as Frame)).count()
}

/// Rendering resumes at the next frame: with the input silent and the click off, the output is the
/// playing lanes, each at its loop position.
fn plays_on_in_place(rig: &mut Rig) {
    let loops: Vec<Option<Vec<f32>>> = (0..TRACK_COUNT).map(|i| (rig.state(i) == LaneState::Playing).then(|| rig.pcm(i))).collect();
    rig.set_level(0.0);
    rig.keep_output();
    rig.advance(rig.master() + 1000);
    let (start, out) = rig.output.as_ref().unwrap();
    let (anchor, master) = (rig.anchor(), rig.master());
    for (k, &y) in out.iter().enumerate() {
        let pos = (start + k as Frame - anchor).rem_euclid(master) as usize;
        let want = loops.iter().fold(0.0f32, |sum, pcm| sum + pcm.as_ref().map_or(0.0, |pcm| pcm[pos]));
        assert_eq!(y, want, "frame {} plays loop position {pos}", start + k as Frame);
    }
}

/// REC on lane 0 with the frame code as input: the counted downbeat (the take's first frame, align 0).
fn first_take_rolling(rig: &mut Rig) -> Frame {
    rig.set_input(code);
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    rig.count_one(mark) + 4 * 24_000
}

#[test]
fn before_the_first_block_a_punch_out_does_nothing() {
    let mut rig = Rig::new();
    rig.send_at(rig.frame, Command::RecDub(0));
    rig.punch_out();
    assert!(rig.events.is_empty());
    rig.advance(128);
    assert!(rig.lane(0).armed, "the command still applies on the first block");
}

#[test]
fn a_count_in_is_cancelled_as_stop_cancels_it() {
    let run = |punch: bool| {
        let mut rig = Rig::new();
        rig.press(Command::RecDub(0));
        rig.advance(30_000);
        let (now, mark) = (rig.frame, rig.events.len());
        if punch {
            rig.punch_out();
        } else {
            rig.send_at(now, Command::Stop(0));
            rig.advance(1);
        }
        assert_eq!(lane_event(&rig, mark, 0).map(|(f, i)| (f, i.state)), Some((now, LaneState::Empty)));
        assert!(rig.window().is_none() && !rig.locked() && rig.master() == 0);
        rig.advance(96_000);
        rig.clicks_since(mark)
    };
    assert_eq!(run(true), vec![], "no count-in beat clicks after the punch-out");
    assert_eq!(run(false), vec![]);
}

#[test]
fn a_boundary_armed_take_and_auto_listening_are_cancelled() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 1, 2400);
    rig.press(Command::RecDub(1));
    assert!(rig.lane(1).armed);
    rig.punch_out();
    assert!(rig.state(1) == LaneState::Empty && rig.window().is_none());
    assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, master), "the loop is untouched");

    let mut rig = Rig::new();
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    rig.advance(4800);
    assert!(rig.lane(0).auto_armed);
    rig.punch_out();
    assert!(rig.state(0) == LaneState::Empty && rig.window().is_none() && !rig.locked());
}

#[test]
fn a_rolling_first_take_ends_at_the_punch_out_and_commits_to_whole_bars() {
    let mut rig = Rig::new();
    let downbeat = first_take_rolling(&mut rig);
    let fpb = rig.fpb();
    rig.advance_to(downbeat + 2 * fpb + fpb / 2);
    let (now, mark) = (rig.frame, rig.events.len());
    rig.punch_out();
    assert_eq!(rig.state(0), LaneState::Playing);
    assert_eq!(rig.master(), 2 * fpb, "plan_commit floors to the completed bars");
    assert_eq!(mismatches(&rig.pcm(0), downbeat), 0, "the take from its downbeat");
    assert_eq!((rig.anchor() - downbeat) % rig.master(), 0, "on the count-in's grid");
    assert_eq!(rig.rejected(), 0);
    let (frame, info) = lane_event(&rig, mark, 0).expect("a Lane event");
    assert_eq!((frame, info.state, info.length), (now, LaneState::Playing, 2 * fpb));
    assert_eq!(transport_event(&rig, mark), Some((now, 2 * fpb)));
    plays_on_in_place(&mut rig);
}

#[test]
fn a_first_take_under_a_bar_pads_to_one() {
    let mut rig = Rig::new();
    let downbeat = first_take_rolling(&mut rig);
    let fpb = rig.fpb();
    rig.advance_to(downbeat + fpb / 3);
    rig.punch_out();
    assert_eq!(rig.master(), fpb);
    rig.idle(); // the padding is a block job: it runs once rendering resumes
    let pcm = rig.pcm(0);
    assert_eq!(mismatches(&pcm[..(fpb / 3) as usize], downbeat), 0);
    assert!(pcm[(fpb / 3) as usize..].iter().all(|&x| x == 0.0));
}

#[test]
fn a_later_take_ends_at_the_punch_out_and_tiles_its_whole_bars() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.record_first_take(0, 2, 2400);
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    let start = rig.start_frame();
    let fpb = rig.fpb();
    rig.advance_to(start + fpb + fpb / 2);
    rig.punch_out();
    assert_eq!(rig.state(1), LaneState::Playing);
    assert!(rig.engine.looper().busy(), "the tiling runs as a block job");
    rig.idle();
    let pcm = rig.pcm(1);
    assert!(pcm.iter().enumerate().all(|(p, &x)| x == code(start + p as Frame % fpb)), "bar 1 tiled over the 2-bar master");
    plays_on_in_place(&mut rig);
}

#[test]
fn a_take_damaged_by_an_earlier_gap_is_still_rejected() {
    let mut rig = Rig::new();
    let downbeat = first_take_rolling(&mut rig);
    rig.advance_to(downbeat + 10_000);
    rig.gap();
    rig.advance(rig.fpb());
    rig.punch_out();
    assert_eq!(rig.rejected(), 1);
    assert!(rig.state(0) == LaneState::Empty && rig.master() == 0);
}

/// FIXED one bar + RETAKE on lane 0: the pass length and pass 1's first frame.
fn rolling_retake(rig: &mut Rig) -> (Frame, Frame) {
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetRetake(true));
    (rig.fpb(), first_take_rolling(rig))
}

#[test]
fn a_retake_roll_keeps_its_last_complete_pass() {
    let mut rig = Rig::new();
    let (len, start) = rolling_retake(&mut rig);
    rig.advance_to(start + 2 * len + len / 2);
    rig.punch_out();
    assert!(rig.state(0) == LaneState::Playing && rig.window().is_none());
    assert_eq!(rig.master(), len);
    assert_eq!(mismatches(&rig.pcm(0), start + len), 0, "pass 2, the last complete one");
    plays_on_in_place(&mut rig);
}

/// Inside the grace a stop gesture would let pass 2 finish; the device cannot, so pass 1, complete and
/// kept, is the take (never pass 2 cut short, which a many-bar take would floor a bar shorter).
#[test]
fn a_retake_roll_in_its_grace_keeps_the_last_complete_pass() {
    let mut rig = Rig::new();
    let (len, start) = rolling_retake(&mut rig);
    rig.advance_to(start + 2 * len - len / 30);
    rig.punch_out();
    assert!(rig.state(0) == LaneState::Playing && rig.window().is_none());
    assert_eq!(rig.master(), len);
    assert_eq!(mismatches(&rig.pcm(0), start), 0, "pass 1, complete and kept");
    plays_on_in_place(&mut rig);
}

#[test]
fn a_retake_roll_with_nothing_kept_ends_at_the_punch_out() {
    for into in [2, 30] {
        let mut rig = Rig::new();
        let (len, start) = rolling_retake(&mut rig);
        let pass = start;
        // Pass 1 halfway, or inside the grace before its edge: nothing is kept yet either way.
        let now = pass + len - len / into;
        rig.advance_to(now);
        rig.punch_out();
        assert!(rig.state(0) == LaneState::Playing && rig.window().is_none(), "the window never ends after the punch-out");
        assert_eq!(rig.master(), len);
        rig.idle();
        let pcm = rig.pcm(0);
        let raw = (now - pass) as usize;
        assert_eq!(mismatches(&pcm[..raw], pass), 0, "the pass in flight, up to the punch-out");
        assert!(pcm[raw..].iter().all(|&x| x == 0.0), "padded");
    }
}

/// REC on another lane approves the roll and would record there from the pass edge; the device stops
/// first: the pass in flight ends at the punch-out and the approved lane stays empty.
#[test]
fn a_retake_approval_waiting_for_its_pass_edge_arms_nothing() {
    let mut rig = Rig::new();
    let (len, start) = rolling_retake(&mut rig);
    rig.advance_to(start + 2 * len - len / 32); // pass 2, inside the grace
    rig.press(Command::RecDub(1));
    assert_eq!(rig.end_frame(), start + 2 * len, "the pass in flight finishes first");
    rig.advance(100);
    rig.punch_out();
    assert_eq!((rig.state(0), rig.state(1)), (LaneState::Playing, LaneState::Empty));
    assert!(rig.window().is_none());
}

#[test]
fn an_overdub_layer_ends_at_the_punch_out_and_is_kept() {
    let mut rig = Rig::new();
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    let before = rig.pcm(0);
    rig.press(Command::RecDub(0));
    let first = rig.frame - 1;
    rig.advance(master / 4);
    let now = rig.frame;
    rig.punch_out();
    assert_eq!(rig.state(0), LaneState::Playing);
    assert!(rig.window().is_none() && rig.lane(0).can_undo);
    rig.idle();
    let (anchor, pcm) = (rig.anchor(), rig.pcm(0));
    for (p, (&x, &was)) in pcm.iter().zip(&before).enumerate() {
        let dubbed = (p as Frame - (first - anchor)).rem_euclid(master) < now - first;
        assert_eq!(x, if dubbed { was + DUB } else { was }, "position {p}");
    }
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(before), "the undo target is the loop before it");
    plays_on_in_place(&mut rig);
}

#[test]
fn an_overdub_stopped_with_play_stop_lands_stopped() {
    let mut rig = Rig::with(Opts { align: 1920, ..Default::default() });
    rig.set_level(0.5);
    rig.record_first_take(0, 1, 2400);
    rig.press(Command::RecDub(0));
    rig.advance(4800);
    rig.press(Command::PlayStop(0)); // the window closes `align` frames later
    rig.advance(100);
    rig.punch_out();
    assert!(rig.state(0) == LaneState::Stopped && rig.window().is_none());
}

#[test]
fn a_loop_end_stop_and_the_block_jobs_carry_on_when_rendering_resumes() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.record_first_take(0, 1, 2400);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    let stop_at = rig.lane(0).stop_at.expect("a loop-end stop");
    rig.press(Command::Copy(0));
    let mark = rig.events.len();
    rig.punch_out();
    assert!(rig.events[mark..].is_empty(), "nothing records: nothing changes");
    assert_eq!(rig.lane(0).stop_at, Some(stop_at));
    assert!(rig.engine.looper().busy());
    rig.idle();
    assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Copied { from: 0, to: 1, .. })));
    rig.advance_to(stop_at + 1);
    assert_eq!(rig.state(0), LaneState::Stopped);
}
