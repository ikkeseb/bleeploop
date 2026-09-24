//! AUTO REC on the real looper: the detector unit (verify/guards/auto-record.mjs) is ported beside it in
//! `src/autorec.rs`; these are the golden jam's AUTO checks (its assertion 8) with frames in hand. While
//! listening the lane keeps nothing, clicks nothing and leaves the tempo free; the same gestures cancel;
//! an onset starts the take with its soft attack retained, and the grid anchors where the player played
//! it (`align` frames before it reached the tap). An input gap while listening drops the history before
//! it, so a retained onset is always one contiguous run.

mod common;

use common::{code, Rig};
use lf_engine::grid::{Frame, Grid};
use lf_engine::{Command, LaneState};

/// Silence, then a soft 0.01 attack for 300 frames, then 0.2: the onset at `at`.
fn onset_at(at: Frame) -> impl Fn(Frame) -> f32 {
    move |f| {
        if f < at {
            0.0
        } else if f < at + 300 {
            0.01
        } else {
            0.2 + code(f) / 16.0
        }
    }
}

fn listening(align: Frame) -> Rig {
    let mut rig = Rig::with(common::Opts { align, ..Default::default() });
    rig.set(Command::SetAutoRecord(true));
    rig.set(Command::SetMetronome(true));
    rig.press(Command::RecDub(0));
    let info = rig.lane(0);
    assert!(info.state == LaneState::Recording && info.auto_armed && !info.armed);
    assert!(!rig.locked() && rig.window().is_some_and(|w| w.1.is_none()));
    rig
}

#[test]
fn listening_keeps_nothing_clicks_nothing_and_cancels_cleanly() {
    for cancel in [Command::RecDub(0), Command::PlayStop(0), Command::Stop(0)] {
        let mut rig = listening(0);
        let mark = rig.events.len();
        rig.set_level(0.001);
        rig.advance(rig.seconds(3.0));
        assert!(rig.lane(0).auto_armed && rig.engine.looper().written(0) == 0);
        assert!(rig.clicks_since(mark).is_empty() && !rig.locked());
        rig.press(cancel);
        assert!(rig.state(0) == LaneState::Empty && !rig.lane(0).auto_armed && rig.window().is_none() && !rig.locked(), "{cancel:?}");
    }
}

#[test]
fn an_onset_starts_the_take_with_its_attack_and_anchors_the_grid() {
    for align in [0, 2400] {
        let mut rig = listening(align);
        let at = rig.frame + rig.seconds(1.3);
        rig.set_input(onset_at(at));
        rig.advance_to(at + 600);
        let info = rig.lane(0);
        assert!(!info.auto_armed && info.state == LaneState::Recording && rig.locked());
        let start = rig.start_frame();
        assert!(start <= at && at - start <= 2 * 192, "the soft attack is retained: start {start}, onset {at}");
        let take = rig.engine.looper().take_pcm(0);
        let input = onset_at(at);
        assert!(take.iter().enumerate().all(|(k, &x)| x == input(start + k as Frame)), "one contiguous run from the start");
        // The pulse runs at the current tempo from where the player played the onset.
        let mark = rig.events.len();
        rig.advance(rig.seconds(1.1));
        let grid = Grid::tempo(start - align, 0, 120, rig.sr);
        let beats = rig.beats_since(mark);
        assert!(!beats.is_empty() && beats.iter().all(|b| grid.beat_frame(grid.first_beat_at_or_after(b.0)) == b.0 && b.3));
        // A stop after two bars commits two bars on that grid.
        rig.advance_to(start - align + 2 * rig.fpb() + 480);
        rig.press(Command::RecDub(0));
        rig.advance(align + 10);
        assert!(rig.state(0) == LaneState::Playing && rig.master() == 2 * rig.fpb());
        assert_eq!((rig.anchor() - (start - align)) % rig.master(), 0);
        assert_eq!(rig.pcm(0)[0], input(start));
    }
}

#[test]
fn a_fixed_auto_take_commits_its_bars() {
    let mut rig = listening(0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let at = rig.frame + 4800;
    rig.set_input(onset_at(at));
    rig.advance_to(at + 600);
    let start = rig.start_frame();
    assert_eq!(rig.end_frame(), start + rig.fpb());
    rig.advance_to(start + rig.fpb() + 1);
    assert!(rig.state(0) == LaneState::Playing && rig.master() == rig.fpb());
}

#[test]
fn a_gap_while_listening_drops_the_history_before_it() {
    let mut rig = listening(0);
    let at = rig.frame + 9600;
    rig.set_input(onset_at(at));
    rig.advance_to(at + 100); // inside the soft attack
    rig.gap();
    rig.advance(600);
    let start = rig.start_frame();
    assert!(start >= at + 100, "nothing from before the gap: start {start}");
}
