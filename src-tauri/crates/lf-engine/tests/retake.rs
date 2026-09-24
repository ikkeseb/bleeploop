//! Ports verify/guards/retake.mjs: RETAKE. A take of known length keeps rolling pass after pass; the stop
//! gesture keeps the last complete pass, finishes the pass in flight inside the quarter-beat grace, or
//! is an ordinary stop with nothing kept. The pure stop rule (section A) is tested with the grid
//! (`src/grid.rs`).
//!
//! One rule changes with the engine: an input gap lands on an exact frame, so it damages only the pass
//! it falls in. The Web Audio looper sampled losses per drain and could not place one on either side
//! of a pass edge, so it also tainted the next pass; a stop in that tainted pass rejected the take.
//! Here that next pass is clean: it is kept, or an ordinary stop commits it. The product rule stays: a
//! damaged pass drops the older kept pass too.

mod common;

use common::{code, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, LaneState};

const MARKER: f32 = 0.75;

fn mismatches(pcm: &[f32], from: Frame) -> usize {
    pcm.iter().enumerate().filter(|&(k, &x)| x != code(from + k as Frame)).count()
}

struct Roll {
    rig: Rig,
    len: Frame,
    start: Frame,
    downbeat: Frame,
}

impl Roll {
    fn pass_start(&self, p: Frame) -> Frame {
        self.start + (p - 1) * self.len
    }
}

/// FIXED `bars` + RETAKE on lane 0. Pass p captures [start + (p-1)L, start + pL).
fn rolling_first_take(sr: u32, bpm: u32, bars: f64, align: Frame) -> Roll {
    let mut rig = Rig::with(common::Opts { sr, start: sr as Frame, align, ..Default::default() });
    rig.set(Command::SetBpm(bpm as f64));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.set(Command::SetRetake(true));
    rig.set_level(MARKER);
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + (240.0 * sr as f64 / bpm as f64).round() as Frame;
    let start = downbeat + align;
    rig.set_input(move |f| if f < start { MARKER } else { code(f) });
    let len = bars as Frame * rig.fpb();
    Roll { rig, len, start, downbeat }
}

#[test]
fn b_a_rolling_take_slides_one_pass_per_edge_and_commits_nothing() {
    for (sr, bpm) in [(48000u32, 120u32), (44100, 97), (48000, 174)] {
        let mut r = rolling_first_take(sr, bpm, 1.0, 0);
        assert_eq!(r.rig.window().map(|w| (w.1, w.2)), Some((Some(r.start), Some(r.start + r.len))));
        for p in 1..=4 {
            r.rig.advance_to(r.pass_start(p) + r.len / 2);
            assert_eq!(r.rig.window().map(|w| (w.1, w.2)), Some((Some(r.pass_start(p)), Some(r.pass_start(p + 1)))));
            let info = r.rig.lane(0);
            assert!(info.retake_pass == p as u32 && info.state == LaneState::Recording && r.rig.master() == 0);
            let take = r.rig.engine.looper().take_pcm(0);
            assert_eq!(take.len() as Frame, r.len / 2, "pass {p}: half a pass captured");
            assert_eq!(mismatches(&take, r.pass_start(p)), 0, "pass {p} writes from its own downbeat");
        }
    }
}

#[test]
fn c_a_mid_pass_stop_commits_the_last_complete_pass_on_the_count_grid() {
    for (sr, bpm, bars, passes) in [(48000u32, 120u32, 1.0, 3), (44100, 97, 2.0, 2), (48000, 137, 1.0, 7)] {
        let mut r = rolling_first_take(sr, bpm, bars, 0);
        r.rig.advance_to(r.pass_start(passes) + r.len * 45 / 100);
        r.rig.press(Command::RecDub(0));
        assert!(r.rig.state(0) == LaneState::Playing && r.rig.window().is_none(), "commits at once");
        assert_eq!(r.rig.master(), r.len);
        assert_eq!(mismatches(&r.rig.pcm(0), r.pass_start(passes - 1)), 0, "holds the last complete pass");
        assert_eq!((r.rig.anchor() - r.downbeat) % r.len, 0, "loop frame 0 on the count-in grid");
    }
}

#[test]
fn d_a_stop_inside_the_grace_finishes_the_pass_in_flight() {
    for align in [0, 1920] {
        let mut r = rolling_first_take(48000, 120, 1.0, align);
        let grace = r.rig.fpb() / 16;
        // Just inside the grace in CAPTURED frames; an uncompensated press would sit outside it.
        r.rig.advance_to(r.pass_start(3) - grace + 256 - align);
        let captured = r.rig.frame + align;
        assert!(r.pass_start(3) - captured <= grace && r.pass_start(3) > captured);
        if align > 0 {
            assert!(r.pass_start(3) - (captured - align) > grace);
        }
        r.rig.press(Command::RecDub(0));
        assert!(r.rig.state(0) == LaneState::Recording && r.rig.master() == 0, "keeps recording to its edge");
        r.rig.advance_to(r.pass_start(3) + 100);
        assert!(r.rig.state(0) == LaneState::Playing && r.rig.master() == r.len);
        assert_eq!(mismatches(&r.rig.pcm(0), r.pass_start(2)), 0, "the pass in flight (2), not pass 1");
        assert!(r.rig.window().is_none());
    }
}

#[test]
fn e_a_stop_in_pass_1_is_an_ordinary_free_stop() {
    let mut r = rolling_first_take(48000, 120, 4.0, 0);
    let fpb = r.rig.fpb();
    r.rig.advance_to(r.start + fpb * 5 / 2);
    r.rig.press(Command::RecDub(0));
    r.rig.advance(4800);
    assert_eq!(r.rig.master(), 2 * fpb);
    assert_eq!(mismatches(&r.rig.pcm(0), r.start), 0);
}

#[test]
fn f_a_damaged_pass_is_dropped_with_the_kept_pass_and_only_that_pass() {
    let mut r = rolling_first_take(48000, 120, 1.0, 0);
    r.rig.advance_to(r.pass_start(2) + r.len / 2);
    r.rig.gap();
    r.rig.advance_to(r.pass_start(3) + r.len / 4);
    assert!(r.rig.events.iter().any(|e| matches!(e, Event::PassDropped { pass: 2, .. })));
    // Pass 1 was kept until pass 2 dropped it: nothing is kept now, so REC on another lane is ignored.
    r.rig.press(Command::RecDub(1));
    assert!(r.rig.state(1) == LaneState::Empty && r.rig.state(0) == LaneState::Recording);
    r.rig.advance_to(r.pass_start(4) + r.len / 4);
    r.rig.press(Command::RecDub(0));
    assert_eq!(r.rig.master(), r.len);
    assert_eq!(mismatches(&r.rig.pcm(0), r.pass_start(3)), 0, "the clean pass after the gap is kept");

    // A stop mid pass 3 with nothing kept is an ordinary stop of that clean pass.
    let mut r = rolling_first_take(48000, 120, 1.0, 0);
    r.rig.advance_to(r.pass_start(2) + r.len / 2);
    r.rig.gap();
    r.rig.advance_to(r.pass_start(3) + r.len / 2);
    r.rig.press(Command::RecDub(0));
    r.rig.advance(4800);
    assert!(r.rig.state(0) == LaneState::Playing && r.rig.rejected() == 0);
    let pcm = r.rig.pcm(0);
    let kept = (r.len / 2) as usize; // the press frame ends the window, exclusive
    assert_eq!(mismatches(&pcm[..kept], r.pass_start(3)), 0);
    assert!(pcm[kept..].iter().all(|&x| x == 0.0), "padded to one bar");
}

/// A two-bar first take on lane 0, then a RETAKE rolling on lane 1 in pass 3.
fn rolling_later_take(fixed: bool) -> (Rig, Frame, Frame) {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 2, 2400);
    rig.set(Command::SetFixedLength(fixed));
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetRetake(true));
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    assert_eq!(rig.lane(1).retake_pass, 0, "no pass while armed for the boundary");
    let s1 = rig.start_frame();
    assert_eq!((s1 - rig.anchor()) % master, 0, "arms on a master boundary");
    assert_eq!(rig.end_frame() - s1, master, "its pass is the master (fixed={fixed})");
    rig.advance_to(s1 + 2 * master + master / 2);
    assert!(rig.lane(1).retake_pass == 3 && rig.start_frame() == s1 + 2 * master);
    assert!([0, 2, 3, 4].iter().all(|&i| rig.lane(i).retake_pass == 0), "only the rolling lane shows a pass");
    (rig, master, s1)
}

#[test]
fn g_rec_on_another_lane_approves_a_later_roll_and_records_next() {
    for fixed in [true, false] {
        let (mut rig, master, s1) = rolling_later_take(fixed);
        rig.press(Command::RecDub(2));
        assert_eq!(rig.state(1), LaneState::Playing);
        assert_eq!(mismatches(&rig.pcm(1), s1 + master), 0, "the approved lane holds pass 2");
        assert!(rig.window().is_some_and(|w| w.0 == 2) && rig.state(2) == LaneState::Recording);
        assert_eq!(rig.start_frame(), s1 + 3 * master, "the approving lane records from the pass edge");
        rig.advance_to(s1 + 3 * master + master / 2);
        assert_eq!(rig.lane(2).retake_pass, 1, "RETAKE still on: the approving lane rolls in turn");
        let take = rig.engine.looper().take_pcm(2);
        assert_eq!(take.len() as Frame, master / 2);
        assert_eq!(mismatches(&take, s1 + 3 * master), 0, "seamless: no frame lost at the handoff");
    }
    let (mut rig, master, s1) = rolling_later_take(false);
    let fpb = rig.fpb();
    rig.advance_to(s1 + 3 * master - fpb / 32);
    rig.press(Command::RecDub(1));
    assert_eq!(rig.state(1), LaneState::Recording, "inside the grace it keeps recording");
    rig.advance_to(s1 + 3 * master + fpb / 4);
    assert!(rig.state(1) == LaneState::Playing && rig.window().is_none());
    assert_eq!(mismatches(&rig.pcm(1), s1 + 2 * master), 0, "commits the pass in flight (3)");
}
