//! Ports verify/guards/later-arm.mjs and looper-arm.mjs: a later take lands frame-exact on the master
//! grid; an armed lane aborts cleanly; the recorder slot belongs to its lane.
//!
//! later-arm's main-thread stall (the capture ring filling while the drain is blocked) has no engine
//! counterpart; what it guarded maps onto an input gap: before the window it costs nothing, inside it
//! the take is rejected, never committed shifted. looper-arm's "STOP keeps the committed PCM" on an
//! overdubbing lane: the engine's Stop discards the whole layer (the Web Audio looper dropped only the
//! pass since its last boundary swap), so the committed loop is the pre-dub loop.

mod common;

use common::{code, frame_of, Rig};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::{Command, Event, LaneState};

#[test]
fn a_later_takes_land_frame_exact_on_the_master_grid() {
    for sr in [44100u32, 48000] {
        for bpm in [60u32, 97, 137, 220] {
            for bars in [1, 2] {
                let mut rig = Rig::with(common::Opts { sr, start: sr as Frame / 3, ..Default::default() });
                rig.set(Command::SetBpm(bpm as f64));
                rig.set_level(0.5);
                let master = rig.record_first_take(0, bars, 1000);
                assert_eq!(master, bars * frames_per_bar(bpm as f64, sr));
                let track1_frame0 = rig.anchor();
                rig.set_input(code);
                for phase in [0.013, 0.37, 0.81] {
                    let next = rig.next_boundary();
                    rig.advance_to(next + (phase * master as f64) as Frame);
                    rig.press(Command::RecDub(1));
                    let boundary = rig.start_frame();
                    assert_eq!(boundary, rig.next_boundary(), "arms on the next boundary");
                    rig.advance(master * 21 / 10);
                    let pcm = rig.pcm(1);
                    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize);
                    let first = frame_of(pcm[0], boundary);
                    assert_eq!(first, boundary, "sr={sr} bpm={bpm} bars={bars} phase={phase}");
                    assert_eq!((first - track1_frame0) % master, 0);
                    assert!(pcm.iter().enumerate().all(|(k, &x)| frame_of(x, first + k as Frame) == first + k as Frame));
                    rig.press(Command::Clear(1));
                }
            }
        }
    }
}

#[test]
fn b_a_gap_before_the_window_costs_nothing_inside_it_rejects_the_take() {
    for inside in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        let master = rig.record_first_take(0, 2, 2400);
        rig.set_input(code);
        rig.advance(rig.seconds(0.5));
        rig.press(Command::RecDub(1));
        let boundary = rig.start_frame();
        if inside {
            rig.advance_to(boundary + rig.seconds(0.3));
        }
        rig.gap();
        rig.advance(master + rig.seconds(4.5));
        if inside {
            assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0, "a damaged take is never committed");
            assert_eq!(rig.rejected(), 1, "reported once");
            assert!(rig.state(0) == LaneState::Playing && rig.master() == master);
        } else {
            let pcm = rig.pcm(1);
            assert_eq!(rig.state(1), LaneState::Playing);
            assert_eq!((frame_of(pcm[0], boundary), rig.rejected()), (boundary, 0));
        }
    }
}

fn with_master() -> (Rig, Frame) {
    let mut rig = Rig::new();
    rig.set_level(0.4);
    let master = rig.record_first_take(0, 4, 2400);
    (rig, master)
}

#[test]
fn arm_1_stopping_an_armed_later_lane_aborts_to_empty() {
    for abort in [Command::RecDub(1), Command::PlayStop(1), Command::Stop(1)] {
        let (mut rig, master) = with_master();
        rig.set_level(0.5);
        rig.advance(rig.seconds(0.9));
        rig.press(Command::RecDub(1));
        rig.advance(rig.seconds(0.2));
        assert!(rig.lane(1).armed && rig.engine.looper().written(1) == 0);
        rig.press(abort);
        assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0 && rig.window().is_none(), "{abort:?}");
        assert!(rig.master() == master && rig.state(0) == LaneState::Playing && rig.locked());
        rig.advance(master + 4800);
        assert_eq!(rig.state(1), LaneState::Empty, "nothing records at the boundary afterwards");
    }
}

#[test]
fn arm_2_3_a_later_take_records_from_frame_0_and_a_short_one_tiles() {
    let (mut rig, master) = with_master();
    rig.set_level(0.7);
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    rig.advance(2 * master);
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize && !rig.lane(1).armed);
    assert!(pcm[0] == 0.7 && pcm[master as usize - 1] == 0.7);

    let (mut rig, master) = with_master();
    let fpb = rig.fpb() as usize;
    rig.set_input(|f| ((f % 997) + 1) as f32 / 1024.0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    rig.advance(2 * master);
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize);
    assert!((fpb..master as usize).all(|k| pcm[k] == pcm[k % fpb]) && pcm[master as usize - 1] != 0.0);
}

#[test]
fn arm_4_stop_keeps_the_committed_loop_and_cancels_a_loop_end_stop() {
    let (mut rig, master) = with_master();
    let before = rig.pcm(0);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.3));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.press(Command::Stop(0));
    rig.advance(rig.seconds(0.1));
    assert_eq!(rig.state(0), LaneState::Stopped);
    assert_eq!(rig.pcm(0), before, "the pre-dub loop survives");
    assert!(rig.master() == master && rig.locked());

    let (mut rig, _) = with_master();
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert!(rig.lane(0).stop_at.is_some() && rig.state(0) == LaneState::Playing);
    rig.press(Command::Stop(0));
    assert!(rig.lane(0).stop_at.is_none() && rig.state(0) == LaneState::Stopped);
}

#[test]
fn arm_5_only_the_recording_lane_releases_its_window() {
    let (mut rig, master) = with_master();
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    let window = rig.window();
    rig.press(Command::Clear(3));
    rig.press(Command::Stop(4));
    assert_eq!(rig.window(), window);
    rig.press(Command::Clear(0)); // the only committed lane
    assert!(rig.master() == master && rig.locked(), "an in-flight lane keeps the grid");
    rig.press(Command::Stop(1));
    assert!(rig.window().is_none() && rig.master() == 0 && !rig.locked(), "its own cancel resets the blank session");
    assert!(rig.events.iter().any(|e| matches!(e, Event::Transport { master: 0, locked: false, .. })));
}

/// Every beat since `mark` sits exactly on the master grid (4 * bars beats per master).
fn beats_on_master_grid(rig: &Rig, mark: usize, bars: Frame) -> usize {
    let grid = lf_engine::grid::Grid::master(rig.anchor(), rig.master(), bars);
    let beats = rig.beats_since(mark);
    for b in &beats {
        assert_eq!(grid.beat_frame(grid.first_beat_at_or_after(b.0)), b.0, "beat at {} is off the master grid", b.0);
    }
    beats.len()
}

#[test]
fn an_aborted_or_rejected_later_take_keeps_the_master_pulse() {
    // 137 bpm at 44.1 kHz: a master beat (19313.75 frames) and a tempo beat (19313.87) part within a few beats.
    for reject in [false, true] {
        let mut rig = Rig::at(44100);
        rig.set(Command::SetBpm(137.0));
        rig.set_level(0.5);
        rig.record_first_take(0, 2, 1000);
        rig.press(Command::RecDub(1));
        if reject {
            rig.advance_to(rig.start_frame() + 1000);
            rig.gap();
            rig.advance(10);
            rig.press(Command::PlayStop(1));
            rig.advance_to(rig.end_frame() + 1);
            assert_eq!(rig.state(1), LaneState::Empty);
        } else {
            rig.press(Command::Stop(1));
        }
        let mark = rig.events.len();
        rig.advance(rig.seconds(15.0));
        assert!(beats_on_master_grid(&rig, mark, 2) >= 30);
    }
}

#[test]
fn a_gap_on_a_window_edge_damages_nothing() {
    for at_end in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        let master = rig.record_first_take(0, 2, 2400);
        rig.set_input(code);
        rig.press(Command::RecDub(1));
        let (start, end) = (rig.start_frame(), rig.end_frame());
        rig.advance_to(if at_end { end } else { start });
        rig.gap(); // the block starting on the edge follows the gap
        rig.advance(master + 4800);
        assert!(rig.state(1) == LaneState::Playing && rig.rejected() == 0, "a gap on the {} edge", if at_end { "end" } else { "start" });
    }
}
