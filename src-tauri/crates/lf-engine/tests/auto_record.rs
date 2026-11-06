//! AUTO REC on the real looper: the detector unit (verify/guards/auto-record.mjs) is ported beside it in
//! `src/autorec.rs`; these are the golden jam's AUTO checks (its assertion 8) with frames in hand. While
//! listening the lane keeps nothing, clicks nothing and leaves the tempo free; the same gestures cancel;
//! an onset starts the take with its soft attack retained, and the grid anchors where the player played
//! it (`align` frames before it reached the tap). An input gap while listening drops the history before
//! it, so a retained onset is always one contiguous run.

mod common;

use common::{code, Rig};
use lf_engine::grid::{Frame, Grid, Meter};
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
        assert_eq!(take.len() as Frame, rig.frame - start, "everything from the retained start");
        let input = onset_at(at);
        assert!(take.iter().enumerate().all(|(k, &x)| x == input(start + k as Frame)), "one contiguous run from the start");
        // The pulse runs at the current tempo from where the player played the onset.
        let mark = rig.events.len();
        rig.advance(rig.seconds(1.1));
        let grid = Grid::tempo(start - align, 0, 120, rig.sr, Meter::FOUR_FOUR);
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

#[test]
fn a_silent_block_drops_the_history_where_it_leaves_a_live_plugins_tap() {
    // The live slot delays its input 1024 frames to the record tap: the silent block [d, d + 256)
    // reaches it over [d + 1024, d + 1280). The onset 100 frames after the block reaches it on
    // d + 1380, and a history kept from before d + 1280 would retain the silent frames.
    const LATENCY: Frame = 1024;
    let mut rig = Rig::with(common::Opts { block: 256, ..Default::default() });
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetAutoRecord(true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).auto_armed);
    let d = rig.frame + 9600;
    let at = d + 356;
    rig.set_input(onset_at(at));
    rig.advance_to(d);
    rig.damage();
    rig.advance(256);
    rig.advance_to(at + LATENCY + 600);
    let start = rig.start_frame();
    assert!(start >= d + 256 + LATENCY, "nothing from before the silent block left the tap: start {start}");
    rig.advance_to(start + rig.fpb() + 1);
    assert_eq!(rig.rejected(), 0);
    assert!(rig.state(0) == LaneState::Playing && rig.master() == rig.fpb());
    let input = onset_at(at);
    assert!(rig.pcm(0)[..1024].iter().enumerate().all(|(k, &x)| x == input(start + k as Frame - LATENCY)), "one contiguous run from the start");
}

#[test]
fn a_trigger_on_the_frames_before_a_gap_leaves_the_tap_keeps_it_and_the_gap_rejects_the_take() {
    // The point gap at d leaves the tap on d + 1000, late in the render chunk the trigger lands in: the
    // onset played just before d reaches it first, so the take holds the gap.
    const LATENCY: Frame = 1000;
    let mut rig = Rig::with(common::Opts { block: 256, ..Default::default() });
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetAutoRecord(true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    let d = rig.frame + 9600;
    rig.set_input(move |f| if f < d - 60 { 0.0 } else { 0.2 });
    rig.advance_to(d);
    rig.gap();
    rig.advance(1256);
    let start = rig.start_frame();
    assert!(start < d + LATENCY, "the take began before the gap: start {start}");
    rig.advance_to(start + rig.fpb() + 1);
    assert_eq!(rig.rejected(), 1);
    assert_eq!(rig.state(0), LaneState::Empty);
}

#[test]
fn a_trigger_after_a_gap_leaves_the_tap_in_the_same_quantum_starts_the_take_there() {
    // At 8 kHz the detector's RMS window is 32 frames, so a trigger fits in the 128-frame quantum after
    // the frame the last gap leaves the tap (at 44.1 and 48 kHz the window, 176 and 192 frames, never
    // does). The live slot delays its input 320 frames: the point gap at d, on a quantum start, leaves
    // the tap on d + 320, 64 frames into its quantum, and the onset reaches it 5 frames later. The
    // history starts there, so the take does, and triggers 32 frames in, at d + 352.
    const LATENCY: Frame = 320;
    for block in [1, 128] {
        let mut rig = Rig::with(common::Opts { sr: 8000, start: 8000, block, ..Default::default() });
        rig.install(0, Box::new(common::Delay::new(LATENCY)));
        rig.set(Command::SetAutoRecord(true));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.press(Command::RecDub(0));
        assert!(rig.lane(0).auto_armed);
        let d = 8000 + 40 * 128;
        let input = move |f: Frame| if f < d + 5 { 0.0 } else { 0.2 + code(f) / 16.0 };
        rig.set_input(input);
        rig.advance_to(d);
        rig.gap();
        rig.advance(LATENCY + 64);
        assert!(!rig.lane(0).auto_armed, "block {block}: triggered");
        let start = rig.start_frame();
        assert_eq!(start, d + LATENCY, "block {block}: the take starts where the gap left the tap");
        let take = rig.engine.looper().take_pcm(0);
        assert_eq!(take.len() as Frame, rig.frame - start, "block {block}: everything from the start");
        assert!(take.iter().enumerate().all(|(k, &x)| x == input(start + k as Frame - LATENCY)), "block {block}: one contiguous run from the start");
        rig.advance_to(start + rig.fpb() + 1);
        assert_eq!(rig.rejected(), 0, "block {block}: no gap inside the take");
        assert!(rig.state(0) == LaneState::Playing && rig.master() == rig.fpb());
    }
}

#[test]
fn the_sensitivity_sets_what_level_triggers() {
    for (sensitivity, triggers) in [(1.0, false), (100.0, true)] {
        let mut rig = listening(0);
        rig.set(Command::SetAutoSensitivity(sensitivity));
        rig.set_level(0.05); // -26 dBFS: under the -12 dBFS of 1, over the -60 dBFS of 100
        rig.advance(4800);
        assert_eq!(!rig.lane(0).auto_armed, triggers, "sensitivity {sensitivity}");
    }
}
