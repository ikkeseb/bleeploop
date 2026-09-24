//! Ports verify/guards/count-in.mjs, count-grid.mjs and bpm-lock.mjs: the first-track COUNT-IN, its
//! grid, and the tempo lock it takes.
//!
//! The Web Audio anchor sat one scheduling lead (20 ms) after the press; the engine counts from the
//! press frame itself, so "count 1 at now + HBL" becomes "count 1 on the press frame". The drain
//! regimes (steady, ragged stalls, one batch spanning count and take) become block sizes: the engine
//! has no drain, and a window edge is a frame whatever the block.

mod common;

use common::{code, Rig};
use lf_engine::grid::{frames_per_bar, Frame, Grid, COUNT_IN_BEATS};
use lf_engine::{Command, LaneState};

const MARKER: f32 = 0.5;
const TAKE: f32 = 0.7;

fn count_frames(bpm: u32, sr: u32) -> Frame {
    Grid::tempo(0, 0, bpm, sr).beat_frame(COUNT_IN_BEATS)
}

/// Press REC on lane 0 with MARKER in the tap, then TAKE from the armed start frame.
fn press_with_marker(rig: &mut Rig) -> Frame {
    rig.set_level(MARKER);
    rig.press(Command::RecDub(0));
    let start = rig.start_frame();
    rig.set_input(move |f| if f < start { MARKER } else { TAKE });
    start
}

#[test]
fn a_the_press_arms_one_bar_ahead_and_counts_from_the_press() {
    for sr in [48000, 44100] {
        for bpm in [120u32, 137, 73, 200] {
            let mut rig = Rig::at(sr);
            rig.set(Command::SetBpm(bpm as f64));
            let press = rig.frame;
            let mark = rig.events.len();
            rig.press(Command::RecDub(0));
            let start = rig.start_frame();
            assert_eq!(start, press + count_frames(bpm, sr), "bpm={bpm} sr={sr}");
            assert_eq!(start - press, (240.0 * sr as f64 / bpm as f64).round() as Frame);
            let one = rig.beats_since(mark)[0];
            assert_eq!((one.0, one.1, one.2, one.3), (press, 0, 4, true), "count 1 on the press frame");
            assert!(rig.lane(0).armed && rig.locked());
        }
    }
}

#[test]
fn b_the_count_is_discarded_and_the_take_starts_at_frame_0() {
    for block in [1, 128, 1024, 4096] {
        let mut rig = Rig::new();
        rig.block = block;
        let start = press_with_marker(&mut rig);
        rig.advance(rig.seconds(2.0 + 4.0 + 0.05));
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(0.3));
        let master = rig.master();
        let fpb = frames_per_bar(120.0, rig.sr);
        assert_eq!(master, 2 * fpb, "block={block}");
        let pcm = rig.pcm(0);
        assert!(!rig.lane(0).armed && rig.window().is_none());
        assert_eq!((pcm[0], pcm[master as usize - 1]), (TAKE, TAKE));
        assert!(pcm.iter().all(|&x| x != MARKER), "count leaked, block={block}");
        assert_eq!((start - rig.anchor()).rem_euclid(master), 0, "grid anchored on the armed downbeat");
    }
}

#[test]
fn b2_a_take_shorter_than_a_bar_keeps_its_content_from_frame_0() {
    let mut rig = Rig::new();
    press_with_marker(&mut rig);
    let fpb = rig.fpb();
    rig.advance(rig.seconds(2.0 + 0.5));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(2.5));
    let pcm = rig.pcm(0);
    assert_eq!(rig.master(), fpb);
    assert_eq!(pcm[0], TAKE);
    let played = pcm.iter().position(|&x| x == 0.0).unwrap() as Frame;
    assert!((played - fpb / 4).abs() <= 1, "content ends at {played}");
    assert!(pcm.iter().all(|&x| x != MARKER));
}

#[test]
fn c_count_clicks_are_forced_and_the_bar_one_is_accented() {
    for metronome in [false, true] {
        let mut rig = Rig::new();
        rig.set(Command::SetMetronome(metronome));
        rig.set_level(TAKE);
        let mark = rig.events.len();
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(2.0 + 2.2));
        let beats = rig.beats_since(mark);
        let anchor = beats[0].0;
        let at = |n: Frame| anchor + n * rig.seconds(0.5);
        for n in 0..4 {
            let b = beats[n as usize];
            assert_eq!((b.0, b.3, b.1 == 0), (at(n), true, n == 0), "count beat {n} metronome={metronome}");
        }
        assert_eq!((beats[4].0, beats[4].1, beats[4].2), (at(4), 0, 0), "come-in beat");
        if metronome {
            assert!(beats[4].3, "the come-in 1 clicks");
            assert!((5..=8).all(|n| beats[n].3 && (beats[n].1 == 0) == (n % 4 == 0)));
        } else {
            assert!(!beats[4].3 && !beats[5].3, "metronome off: silent past the count");
        }
    }
}

#[test]
fn d_stop_during_the_count_aborts_to_empty() {
    let mut rig = Rig::new();
    press_with_marker(&mut rig);
    rig.advance(rig.seconds(1.0));
    assert!(rig.lane(0).armed && rig.engine.looper().written(0) == 0);
    let mark = rig.events.len();
    rig.press(Command::Stop(0));
    assert_eq!(rig.state(0), LaneState::Empty);
    assert_eq!((rig.lane(0).length, rig.master()), (0, 0));
    assert!(rig.window().is_none() && !rig.locked());
    rig.advance(rig.seconds(2.0));
    let after = rig.beats_since(mark);
    assert!(after.len() >= 3 && after.iter().all(|b| b.2 == 0 && !b.3), "no count numeral or click after the abort");
    assert!(after.windows(2).all(|w| w[1].0 - w[0].0 == rig.seconds(0.5)), "free-run cadence");
}

#[test]
fn e_a_later_arm_abort_leaves_the_master_pulse_alone() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 2, 2400);
    let anchor = rig.anchor();
    let beat = master / 8;
    rig.press(Command::RecDub(1));
    assert!(rig.lane(1).armed && rig.state(1) == LaneState::Recording);
    let mark = rig.events.len();
    rig.press(Command::Stop(1));
    rig.advance(rig.seconds(2.0));
    assert_eq!(rig.state(1), LaneState::Empty);
    let after = rig.beats_since(mark);
    assert!(after.len() >= 3 && after.iter().all(|b| (b.0 - anchor) % beat == 0));
}

#[test]
fn f_a_fast_abort_and_re_record_never_swallows_the_new_count_one() {
    let mut rig = Rig::new();
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.01));
    rig.press(Command::RecDub(0)); // abort 10 ms later
    rig.advance(rig.seconds(0.02));
    let mark2 = rig.events.len();
    rig.press(Command::RecDub(0));
    let one = rig.count_one(mark2);
    rig.advance(rig.seconds(2.1));
    let clicks = rig.clicks_since(mark);
    assert!(one - clicks[0].0 < rig.seconds(0.12), "precondition: within the anti-flam window");
    assert!(clicks.contains(&(one, true)), "the re-record count 1 sounds");
    assert!((0..4).all(|n| clicks.iter().any(|c| c.0 == one + n * rig.seconds(0.5))));
}

#[test]
fn count_grid_the_take_starts_on_the_frame_at_the_come_in_beat() {
    for bpm in [120u32, 90, 200] {
        for sr in [48000, 44100] {
            let mut ones = Vec::new();
            for metronome in [true, false] {
                for block in [128, 1024] {
                    let mut rig = Rig::at(sr);
                    rig.block = block;
                    rig.set(Command::SetBpm(bpm as f64));
                    rig.set(Command::SetMetronome(metronome));
                    rig.advance(rig.seconds(0.0371));
                    rig.set_input(code);
                    let press = rig.frame;
                    let mark = rig.events.len();
                    rig.press(Command::RecDub(0));
                    let grid = Grid::tempo(press, 0, bpm, sr);
                    let start = grid.beat_frame(COUNT_IN_BEATS);
                    assert_eq!(rig.start_frame(), start);
                    rig.advance_to(start + rig.seconds(4.0 * 60.0 / bpm as f64 + 0.05));
                    let count: Vec<_> = rig.beats_since(mark).into_iter().filter(|b| b.2 > 0).collect();
                    assert_eq!(count.len(), 4);
                    assert!(count.iter().enumerate().all(|(n, b)| b.0 == grid.beat_frame(n as u64) && b.3 && (b.1 == 0) == (n == 0)));
                    ones.push(count[0].0 - press);
                    rig.press(Command::RecDub(0));
                    rig.advance(rig.seconds(0.3));
                    let pcm = rig.pcm(0);
                    let master = rig.master() as usize;
                    assert_eq!(rig.state(0), LaneState::Playing);
                    assert_eq!(pcm[0], code(start), "frame 0 is the frame at the come-in");
                    assert_eq!(pcm[master - 1], code(start + master as Frame - 1));
                }
            }
            assert!(ones.iter().all(|&d| d == 0), "the count starts on the press, metronome on or off");
        }
    }
}

#[test]
fn bpm_lock_a_the_press_locks_and_the_count_runs_at_the_press_tempo() {
    let mut rig = Rig::new();
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(1.2));
    assert!(rig.locked());
    let count: Vec<_> = rig.beats_since(mark).into_iter().filter(|b| b.2 > 0).collect();
    assert!(count.len() >= 3 && count.windows(2).all(|w| w[1].0 - w[0].0 == rig.seconds(0.5)));
}

#[test]
fn bpm_lock_b_a_mid_take_retune_is_a_no_op() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(3.0));
    rig.set(Command::SetBpm(100.0));
    assert_eq!(rig.bpm(), 120);
    rig.advance(rig.seconds(3.05));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.3));
    assert_eq!(rig.master(), 2 * frames_per_bar(120.0, rig.sr));
}

#[test]
fn bpm_lock_c_every_abort_path_unlocks() {
    type Abort = (&'static str, fn(&mut Rig));
    let aborts: [Abort; 5] = [
        ("REC again", |r| r.press(Command::RecDub(0))),
        ("PLAY/STOP", |r| r.press(Command::PlayStop(0))),
        ("STOP", |r| r.press(Command::Stop(0))),
        ("CLEAR", |r| r.press(Command::Clear(0))),
        ("STOP ALL", |r| r.press(Command::StopAll)),
    ];
    for fixed in [false, true] {
        for (label, abort) in aborts {
            let mut rig = Rig::new();
            rig.set(Command::SetFixedLength(fixed));
            rig.set(Command::SetFixedBars(2.0));
            rig.press(Command::RecDub(0));
            rig.advance(rig.seconds(0.8));
            assert!(rig.locked(), "{label}");
            abort(&mut rig);
            rig.advance(rig.seconds(0.1));
            assert!(!rig.locked() && rig.window().is_none() && rig.state(0) == LaneState::Empty, "{label} fixed={fixed}");
            rig.set(Command::SetBpm(90.0));
            assert_eq!(rig.bpm(), 90, "tempo editable again after {label}");
        }
    }
    for clear in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(4.0));
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(3.5));
        assert!(rig.state(0) == LaneState::Recording && !rig.lane(0).armed);
        rig.press(if clear { Command::Clear(0) } else { Command::Stop(0) });
        assert!(!rig.locked() && rig.window().is_none());
    }
}

#[test]
fn bpm_lock_d_e_a_committed_loop_keeps_the_lock_and_a_later_abort_does_not_unlock() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    assert!(rig.record_first_take(0, 2, 2400) > 0);
    assert!(rig.locked());
    rig.set(Command::SetBpm(140.0));
    assert_eq!(rig.bpm(), 120);
    for abort in [Command::RecDub(1), Command::Stop(1), Command::Clear(1)] {
        rig.press(Command::RecDub(1));
        assert!(rig.lane(1).armed);
        rig.press(abort);
        assert!(rig.locked(), "{abort:?}");
        assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Empty);
    }
}

#[test]
fn bpm_lock_f_only_the_recording_lane_releases_its_window() {
    let mut rig = Rig::new();
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    rig.press(Command::RecDub(2));
    rig.advance(rig.seconds(0.5));
    let before = rig.window();
    rig.press(Command::Clear(1));
    rig.press(Command::Stop(3));
    rig.press(Command::PlayStop(4));
    assert_eq!(rig.window(), before);
    assert!(rig.locked());
    rig.press(Command::Stop(2));
    assert!(rig.window().is_none() && !rig.locked());
}
