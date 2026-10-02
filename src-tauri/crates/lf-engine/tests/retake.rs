//! Ports verify/guards/retake.mjs: RETAKE. A take of known length keeps rolling pass after pass; the stop
//! gesture keeps the last complete pass, finishes the pass in flight inside the quarter-beat grace, or
//! is an ordinary stop with nothing kept. The pure stop rule (section A) is tested with the grid
//! (`src/grid.rs`).
//!
//! One rule changes with the engine: an input gap lands on exact frames, so it damages only the passes
//! it overlaps. A point xrun falls in one pass; a jump or a damaged block can span a pass edge and drop
//! both passes it cut. The Web Audio looper sampled losses per drain and could not place one on either
//! side of a pass edge, so it also tainted the next pass; a stop in that tainted pass rejected the take.
//! Here a pass the gap does not reach is clean: it is kept, or an ordinary stop commits it. The product
//! rule stays: a damaged pass drops the older kept pass too.
//!
//! The engine's own: a FIXED first take's click follows the roll, re-anchored on each pass's downbeat,
//! so a pass played to the click commits on its own bar lines however many passes came before (`h_`).
//! What it cannot see: pass 1 still runs on the count-in's exact-tempo grid, so pass 1's bars and the
//! first pass edge's click sit up to 0.41 frames per bar early at 137 BPM (bounded, never accumulating).

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

#[test]
fn f_a_jump_across_a_pass_edge_drops_both_passes_it_cut() {
    let mut r = rolling_first_take(48000, 120, 1.0, 0);
    r.rig.advance_to(r.pass_start(3) - 64);
    r.rig.skip(128); // pass 2 loses its last 64 frames, pass 3 its first 64
    r.rig.advance_to(r.pass_start(4) + r.len / 4);
    let dropped: Vec<u32> = r.rig.events.iter().filter_map(|e| if let Event::PassDropped { pass, .. } = *e { Some(pass) } else { None }).collect();
    assert_eq!(dropped, [2, 3], "both passes the jump cut are dropped");
    // Nothing is kept now, so REC on another lane is ignored.
    r.rig.press(Command::RecDub(1));
    assert!(r.rig.state(1) == LaneState::Empty && r.rig.state(0) == LaneState::Recording);
    r.rig.advance_to(r.pass_start(5) + r.len / 4);
    r.rig.press(Command::RecDub(0));
    assert!(r.rig.master() == r.len && r.rig.rejected() == 0);
    assert_eq!(mismatches(&r.rig.pcm(0), r.pass_start(4)), 0, "the clean pass after the jump is kept");
}

#[test]
fn f_a_jump_over_many_passes_then_another_gap_keeps_none_of_them() {
    let mut r = rolling_first_take(48000, 120, 1.0, 0);
    r.rig.advance_to(r.pass_start(3) - 64);
    r.rig.skip(r.pass_start(12) + 64 - r.rig.frame); // passes 3 to 11 never arrive, nor pass 12's first 64 frames
    r.rig.advance(64);
    r.rig.gap(); // another xrun while the roll still catches up on the passes the jump skipped
    r.rig.advance(r.len / 4);
    let dropped: Vec<u32> = r.rig.events.iter().filter_map(|e| if let Event::PassDropped { pass, .. } = *e { Some(pass) } else { None }).collect();
    assert_eq!(dropped, (2..=11).collect::<Vec<u32>>(), "every pass the jump cut is dropped");
    // Nothing is kept, so REC on another lane is ignored and the roll goes on.
    r.rig.press(Command::RecDub(1));
    assert!(r.rig.state(1) == LaneState::Empty && r.rig.state(0) == LaneState::Recording && r.rig.master() == 0);
    r.rig.advance_to(r.pass_start(14) + r.len / 4);
    r.rig.press(Command::RecDub(0));
    assert!(r.rig.master() == r.len && r.rig.rejected() == 0);
    assert_eq!(mismatches(&r.rig.pcm(0), r.pass_start(13)), 0, "the first clean pass after the jump is kept");
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

#[test]
fn h_a_jump_from_the_handoff_seam_damages_the_take_that_starts_there() {
    let (mut rig, master, s1) = rolling_later_take(false);
    let edge = s1 + 3 * master;
    rig.advance_to(edge - rig.fpb() / 32);
    rig.press(Command::RecDub(2)); // inside the grace: lane 1 finishes pass 3, lane 2 records from its edge
    rig.advance_to(edge);
    rig.skip(128); // pass 3 is whole; lane 2's first 128 frames never arrive
    rig.advance_to(edge + master + master / 2);
    assert!(rig.state(1) == LaneState::Playing && rig.rejected() == 0);
    assert_eq!(mismatches(&rig.pcm(1), s1 + 2 * master), 0, "lane 1 holds the whole pass 3");
    assert!(rig.events.iter().any(|e| matches!(e, Event::PassDropped { lane: 2, pass: 1, .. })), "lane 2's cut first pass is dropped");
}

#[test]
fn a_free_first_take_never_rolls() {
    let mut rig = Rig::new();
    rig.set(Command::SetRetake(true));
    rig.set_level(0.5);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(3.0));
    assert!(rig.state(0) == LaneState::Recording && rig.lane(0).retake_pass == 0, "no known length to roll around");
}

/// FIXED 8 at 137 BPM on lane 0, RETAKE on, the metronome on, `align` frames of input/output alignment,
/// the input an impulse at each frame of `marks`. Every pass is checked to tile on from the last, whole
/// bars long; a stop mid pass `APPROVED + 1` approves pass `APPROVED`. Returns the rig, that pass's
/// window start and every accented click it heard.
const APPROVED: u32 = 4;
const BARS_137: Frame = 8;

fn roll_played_to(align: Frame, marks: Vec<Frame>) -> (Rig, Frame, Vec<Frame>) {
    let mut rig = Rig::with(common::Opts { sr: 48000, start: 48000, align, ..Default::default() });
    rig.set(Command::SetBpm(137.0));
    rig.set(Command::SetMetronome(true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(BARS_137 as f64));
    rig.set(Command::SetRetake(true));
    rig.set_input(move |f| if marks.binary_search(&f).is_ok() { 1.0 } else { 0.0 });
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let len = BARS_137 * rig.fpb();
    let mut edge = rig.start_frame();
    for p in 1..=APPROVED + 1 {
        rig.advance_to(edge + len / 2);
        let pass = (rig.lane(0).retake_pass, rig.start_frame(), rig.end_frame());
        assert_eq!(pass, (p, edge, edge + len), "pass {p} starts where the last ended, whole bars long");
        edge += len;
    }
    rig.press(Command::RecDub(0));
    assert!(rig.state(0) == LaneState::Playing && rig.master() == len, "commits pass {APPROVED}");
    let clicks = rig.clicks_since(mark).into_iter().filter(|c| c.1).map(|c| c.0).collect();
    (rig, edge - 2 * len, clicks)
}

#[test]
fn h_an_approved_pass_commits_on_the_click_however_many_passes_rolled() {
    // 137 BPM at 48 kHz: the click's bar is 84087.59 frames, the window's 84088. A player plays an
    // impulse on every accented click (heard `align` late, as the rig's input arrives) and approves
    // pass 4; its marks must sit on the committed loop's own bar lines. A roll that slides by the
    // rounded bar while the click keeps its exact one puts them 0.41 frames per elapsed bar early.
    for align in [0, 1920] {
        let (_, _, clicks) = roll_played_to(align, Vec::new());
        let marks = clicks.iter().map(|c| c + align).collect();
        let (rig, start, heard) = roll_played_to(align, marks);
        assert_eq!(heard, clicks, "the input moves no click");
        let fpb = rig.fpb();
        assert_eq!((rig.anchor() - (start - align)).rem_euclid(rig.master()), 0, "loop frame 0 on the pass's downbeat");
        let pcm = rig.pcm(0);
        let offsets: Vec<Frame> = (0..pcm.len() as Frame)
            .filter(|&k| pcm[k as usize] > 0.5)
            .map(|k| k - (k + fpb / 2).div_euclid(fpb) * fpb)
            .collect();
        println!("align {align}: each mark's offset from its bar line in the approved pass: {offsets:?}");
        assert_eq!(offsets.len(), BARS_137 as usize, "one mark per bar");
        assert!(offsets.iter().all(|o| o.abs() <= 1), "align {align}: the marks sit off the loop's bar lines: {offsets:?}");
    }
}
