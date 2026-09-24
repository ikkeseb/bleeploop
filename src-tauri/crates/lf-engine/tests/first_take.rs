//! Ports verify/guards/free-stop.mjs, free-record-cap.mjs, fixed-length.mjs and short-take.mjs: how a
//! first take ends, and how a later FIXED take is sized. short-take.mjs's pure tiling and stop-plan
//! checks live with the grid (`src/grid.rs` tests); a short later take tiling on the real looper is in
//! `later_arm.rs`.
//!
//! The compensation C of the Web Audio looper is the engine's `align_frames`: a take starts that many
//! frames after its downbeat and its tail arrives that much after the press. The drain regimes become
//! block sizes. Two checks change meaning: an aborted take leaves no loop (`length` 0) but its buffer is
//! not zeroed, since the next take writes over it and a commit pads what it did not reach.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::{Command, LaneState};

fn committed(rig: &Rig) -> bool {
    rig.state(0) == LaneState::Playing && rig.master() > 0
}

/// Boot at `bpm`/`sr` with `align`, press REC; the tap carries the frame code. Returns the counted
/// downbeat and the take's first frame.
fn start_take(bpm: u32, sr: u32, align: Frame) -> (Rig, Frame, Frame) {
    let mut rig = Rig::with(Opts { sr, start: 50 * sr as Frame, align, loop_seconds: 60.0, ..Default::default() });
    rig.set(Command::SetBpm(bpm as f64));
    rig.set_input(code);
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + (240.0 * sr as f64 / bpm as f64).round() as Frame;
    let start = rig.start_frame();
    assert_eq!(start, downbeat + align);
    (rig, downbeat, start)
}

fn until_committed(rig: &mut Rig, limit: Frame) -> Frame {
    let end = rig.frame + limit;
    while !committed(rig) && rig.frame < end {
        rig.advance(1);
    }
    rig.frame
}

#[test]
fn free_a_a_stop_after_a_bar_line_waits_for_the_aligned_tail() {
    for (bpm, sr) in [(120u32, 44100), (120, 48000), (90, 44100), (137, 48000)] {
        let align = sr as Frame / 10;
        let (mut rig, downbeat, start) = start_take(bpm, sr, align);
        let fpb = rig.fpb();
        rig.advance_to(downbeat + 8 * fpb + rig.seconds(0.05));
        let press = rig.frame;
        rig.press(Command::RecDub(0));
        assert_eq!(rig.end_frame(), start + 8 * fpb, "bpm={bpm} sr={sr}");
        assert!(!committed(&rig), "the tail is still in flight at the press");
        let at = until_committed(&mut rig, sr as Frame);
        assert_eq!(rig.master(), 8 * fpb);
        assert_eq!(at, start + 8 * fpb + 1, "commits on the window end");
        assert!(at - press <= align);
        let pcm = rig.pcm(0);
        assert_eq!((pcm[0], pcm[(8 * fpb - 1) as usize]), (code(start), code(start + 8 * fpb - 1)));
    }
}

#[test]
fn free_b_a_mid_bar_stop_floors_and_commits_at_once() {
    for (bpm, sr) in [(120u32, 44100), (90, 48000)] {
        for block in [1, 128, 2048] {
            let (mut rig, downbeat, _) = start_take(bpm, sr, 0);
            rig.block = block;
            let fpb = rig.fpb();
            rig.advance_to(downbeat + 7 * fpb + fpb / 2);
            rig.press(Command::RecDub(0));
            assert!(committed(&rig));
            assert_eq!(rig.master(), 7 * fpb);
        }
    }
}

#[test]
fn free_c_a_press_a_quarter_beat_early_keeps_the_bar() {
    for (bpm, sr) in [(120u32, 44100), (200, 48000)] {
        let (mut rig, downbeat, start) = start_take(bpm, sr, 0);
        let fpb = rig.fpb();
        rig.advance_to(downbeat + 4 * fpb - (0.6 * fpb as f64 / 16.0) as Frame);
        rig.press(Command::RecDub(0));
        assert!(!committed(&rig) && rig.end_frame() == start + 4 * fpb);
        until_committed(&mut rig, sr as Frame);
        assert_eq!(rig.master(), 4 * fpb);
        let (mut rig, downbeat, _) = start_take(bpm, sr, 0);
        rig.advance_to(downbeat + 4 * fpb - fpb / 8);
        rig.press(Command::RecDub(0));
        until_committed(&mut rig, sr as Frame);
        assert_eq!(rig.master(), 3 * fpb, "half a beat early drops the bar");
    }
}

#[test]
fn free_d_a_sub_bar_stop_keeps_its_tail_then_pads() {
    let align = 2646; // 60 ms at 44.1 kHz
    let (mut rig, downbeat, start) = start_take(120, 44100, align);
    let fpb = rig.fpb();
    rig.advance_to(downbeat + fpb * 4 / 10);
    let press = rig.frame;
    rig.press(Command::RecDub(0));
    assert_eq!(rig.end_frame(), press + align);
    assert!(!committed(&rig));
    rig.advance(rig.seconds(0.02));
    rig.press(Command::RecDub(0)); // a later press would end later
    assert!(committed(&rig) || rig.end_frame() == press + align);
    until_committed(&mut rig, 3 * 44100);
    let kept = (press + align - start) as usize;
    let pcm = rig.pcm(0);
    assert_eq!(rig.master(), fpb);
    assert_eq!((pcm[kept - 1], pcm[kept], pcm[fpb as usize - 1]), (code(start + kept as Frame - 1), 0.0, 0.0));
}

#[test]
fn free_g_alignment_moves_both_edges_never_the_length() {
    let (_, d0, plain) = start_take(120, 48000, 0);
    let (mut rig, d1, comp) = start_take(120, 48000, 7200);
    assert_eq!((plain - d0, comp - d1), (0, 7200));
    rig.advance_to(d1 + 2 * rig.fpb() + rig.seconds(0.02));
    rig.press(Command::RecDub(0));
    assert_eq!(rig.end_frame() - comp, 2 * rig.fpb());
    until_committed(&mut rig, 48000);
    assert_eq!(rig.master(), 2 * rig.fpb());
}

#[test]
fn cap_a_free_take_held_past_the_buffer_commits_at_its_capacity() {
    for (sr, bpm) in [(48000u32, 120u32), (44100, 100), (48000, 137), (44100, 42)] {
        let mut rig = Rig::with(Opts { sr, start: sr as Frame, loop_seconds: 60.0, block: 1024, ..Default::default() });
        rig.set(Command::SetBpm(bpm as f64));
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        let (start, end) = (rig.start_frame(), rig.end_frame());
        let cap = rig.engine.looper().capacity();
        assert_eq!((cap, end - start), ((60 * sr) as Frame, cap), "sr={sr}");
        rig.advance_to(end);
        assert_eq!(rig.state(0), LaneState::Recording, "not before the capacity");
        rig.advance(1);
        let fpb = frames_per_bar(bpm as f64, sr);
        assert!(committed(&rig) && rig.window().is_none(), "commits on the capacity frame");
        assert_eq!(rig.master(), cap / fpb * fpb);
        if sr == 48000 && bpm == 120 {
            let pcm = rig.pcm(0);
            assert_eq!((pcm[0], pcm[rig.master() as usize - 1]), (code(start), code(start + rig.master() - 1)));
        }
    }
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(12.0));
    assert!(rig.state(0) == LaneState::Recording && rig.master() == 0, "a 10 s free take keeps recording");
}

/// Enable FIXED `bars`, press REC. The tap is MARKER until the armed start, then the frame code.
fn fixed_take(sr: u32, bpm: u32, bars: f64, align: Frame, loop_seconds: f64) -> (Rig, Frame, Frame) {
    let mut rig = Rig::with(Opts { sr, start: sr as Frame, align, loop_seconds, ..Default::default() });
    rig.set(Command::SetBpm(bpm as f64));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.set_level(MARKER);
    rig.press(Command::RecDub(0));
    let (start, end) = (rig.start_frame(), rig.end_frame());
    rig.set_input(move |f| if f < start { MARKER } else { code(f) });
    (rig, start, end)
}

const MARKER: f32 = 0.75;

#[test]
fn fixed_a_the_window_is_whole_bars_that_fit() {
    for sr in [48000u32, 44100] {
        for bpm in [120u32, 90, 137, 100, 73] {
            for bars in [1, 2, 4, 8, 16] {
                let (rig, start, end) = fixed_take(sr, bpm, bars as f64, 0, 20.0);
                let fpb = frames_per_bar(bpm as f64, sr);
                let fit = (20 * sr as Frame / fpb).min(bars);
                assert_eq!(end - start, fit * fpb, "bpm={bpm} sr={sr} bars={bars}");
                assert!(rig.locked());
            }
        }
    }
    let (rig, start, end) = fixed_take(48000, 40, 32.0, 0, 60.0);
    let fpb = rig.fpb();
    assert_eq!(end - start, rig.engine.looper().capacity() / fpb * fpb, "an over-long request clamps to whole bars");
    let mut rig = Rig::new();
    for (n, want) in [(0.0, 1), (-3.0, 1), (99.0, 32), (4.6, 5)] {
        rig.set(Command::SetFixedBars(n));
        assert_eq!(rig.engine.looper().fixed_bars(), want);
    }
}

#[test]
fn fixed_b_it_commits_exactly_its_window_at_any_block_size() {
    for (sr, bpm, bars) in [(48000u32, 120u32, 4.0), (44100, 100, 2.0), (48000, 90, 2.0), (44100, 137, 1.0)] {
        for block in [1, 128, 4096] {
            let (mut rig, start, end) = fixed_take(sr, bpm, bars, 0, 20.0);
            rig.block = block;
            let target = end - start;
            rig.advance_to(end + rig.seconds(0.3));
            assert!(committed(&rig), "block={block}");
            assert_eq!(rig.master(), target);
            let pcm = rig.pcm(0);
            assert_eq!((pcm[0], pcm[target as usize - 1]), (code(start), code(start + target - 1)));
            assert_eq!(rig.engine.looper().live_buffer(0)[target as usize], 0.0, "nothing past the window");
            assert!(pcm.iter().all(|&x| x != MARKER));
            assert!(rig.window().is_none() && rig.locked() && rig.bpm() == bpm);
        }
    }
}

#[test]
fn fixed_d_e_an_abort_in_the_count_or_the_take_leaves_nothing() {
    for when in [1.0, 3.5] {
        let (mut rig, _, _) = fixed_take(48000, 120, 4.0, 0, 20.0);
        rig.advance(rig.seconds(when));
        rig.press(Command::Stop(0));
        rig.advance(rig.seconds(0.5));
        assert!(rig.state(0) == LaneState::Empty && rig.master() == 0 && rig.window().is_none() && !rig.locked());
        assert!(rig.pcm(0).is_empty() && rig.engine.looper().written(0) == 0);
    }
}

#[test]
fn fixed_f_i_a_manual_stop_commits_completed_bars_and_waits_for_the_tail() {
    let (mut rig, start, _) = fixed_take(48000, 120, 4.0, 0, 20.0);
    let fpb = rig.fpb();
    rig.advance_to(start + fpb * 12 / 5);
    rig.press(Command::RecDub(0));
    until_committed(&mut rig, 48000);
    assert!(rig.master() == 2 * fpb && rig.locked());

    let (mut rig, start, _) = fixed_take(48000, 120, 4.0, 4800, 20.0);
    let downbeat = start - 4800;
    rig.advance_to(downbeat + 2 * fpb + 100);
    rig.press(Command::RecDub(0));
    assert_eq!(rig.end_frame(), start + 2 * fpb, "the manual stop replaces the longer end");
    assert!(!committed(&rig) && rig.window().unwrap().0 == 0 && rig.locked());
    rig.advance_to(downbeat + 2 * fpb + 1500);
    if !committed(&rig) {
        rig.press(Command::RecDub(0));
    }
    assert!(committed(&rig) || rig.end_frame() == start + 2 * fpb);
    until_committed(&mut rig, 48000);
    assert_eq!(rig.master(), 2 * fpb);
    assert_eq!(rig.pcm(0)[(2 * fpb - 1) as usize], code(start + 2 * fpb - 1));
    assert_eq!(rig.engine.looper().live_buffer(0)[(2 * fpb) as usize], 0.0);
}

#[test]
fn fixed_h_42_bpm_32_bars_clamp_and_a_later_take_fills_the_master() {
    let (mut rig, start, end) = fixed_take(44100, 42, 32.0, 0, 60.0);
    let cap = rig.engine.looper().capacity();
    let fpb = rig.fpb();
    assert!(cap % fpb != 0 && 32 * fpb > cap);
    assert_eq!(end - start, cap / fpb * fpb);
    rig.advance_to(end + 4410);
    let master = rig.master();
    assert_eq!(master, end - start);
    rig.set(Command::SetFixedLength(false));
    rig.press(Command::RecDub(1));
    rig.advance(2 * master + 8820);
    assert!(rig.state(1) == LaneState::Playing && rig.lane(1).length == master);
}

#[test]
fn fixed_j_a_later_fixed_take_selects_its_bars_retake_keeps_the_master() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 8, 2400);
    let fpb = rig.fpb();
    assert_eq!(master, 8 * fpb);
    let mut window = |fixed: bool, bars: f64, retake: bool| {
        rig.set(Command::SetFixedLength(fixed));
        rig.set(Command::SetFixedBars(bars));
        rig.set(Command::SetRetake(retake));
        rig.press(Command::RecDub(1));
        let w = rig.end_frame() - rig.start_frame();
        rig.press(Command::Stop(1));
        w
    };
    assert_eq!(window(true, 3.0, false), 3 * fpb);
    assert_eq!(window(true, 12.0, false), master);
    assert_eq!(window(false, 3.0, false), master);
    assert_eq!(window(true, 3.0, true), master);
}
