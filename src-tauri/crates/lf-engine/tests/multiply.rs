//! F14 multiply (a tester report): a later FIXED take longer than the master grows the loop to it in
//! whole loops, and the shorter tracks repeat across it (an RC-505-style multiply). The take commits as
//! the new master; the grid re-anchors on its boundary, a whole number of old loops after the old anchor,
//! so every beat, accent and old lane's phase carries on untouched; every other loop and its undo target
//! tile out to the new length in block jobs, read through the old loop until they are done. No web guard
//! precedes this: the Web Audio looper never multiplied.
//!
//! E10 (an owner decision): FIXED off, a later take runs until the press, as the first take does, and a
//! stop past its first loop pass keeps the NEAREST whole number of loops (recording on to it when it lies
//! ahead), so a free take grows the loop the same way; a FIXED multiply window stopped early follows the
//! same rule (`e_…`, `j_…`, `k_…`).
//!
//! The beat grid's origin, which the lanes' rhythmic FX follow, stays put across a multiply: a dotted
//! STUTTER keeps its phase through the commit (`l_…`).

mod common;

use common::{code, Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::{Frame, Grid};
use lf_engine::looper::{job_frames, JOB_RATE};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Command, Event, LaneState, SessionJob, Snapshot};

/// `bars` bars of `signal` committed on lane 0 as the master, then the input silent. Returns the master.
fn master_of(rig: &mut Rig, bars: Frame, signal: impl Fn(Frame) -> f32 + 'static) -> Frame {
    rig.set_input(signal);
    let master = rig.record_first_take(0, bars, 2400);
    rig.set_level(0.0);
    rig.idle();
    master
}

/// FIXED `bars` on, then REC on `lane`: the take's window.
fn arm_fixed(rig: &mut Rig, lane: u8, bars: f64) -> (Frame, Frame) {
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.press(Command::RecDub(lane));
    (rig.start_frame(), rig.end_frame())
}

/// `loop_` repeated out to `len` samples.
fn tiled(loop_: &[f32], len: Frame) -> Vec<f32> {
    (0..len as usize).map(|k| loop_[k % loop_.len()]).collect()
}

/// Every bin of lane `lane`'s buffer, up to the frames the lane holds, is its min and max (`tests/peaks.rs`).
fn peaks_describe(rig: &Rig, lane: usize, tag: &str) {
    let looper = rig.engine.looper();
    let view = looper.overview().lane(lane);
    let pcm = &looper.live_buffer(lane)[..view.frames as usize];
    for (bin, frames) in pcm.chunks(PEAK_FRAMES).enumerate() {
        let want = frames.iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        assert_eq!(looper.overview().bin(view.buf, bin), want, "{tag}: lane {lane} bin {bin}");
    }
}

fn steps_stay_small(rig: &Rig) {
    let max = rig.engine.looper().job_step_max();
    assert!(max <= JOB_RATE * rig.block as Frame, "a job step moved {max} positions at block {}", rig.block);
}

#[test]
fn a_a_fixed_take_past_a_one_bar_master_multiplies_it() {
    for align in [0, 480] {
        let mut rig = Rig::with(Opts { align, ..Default::default() });
        let master = master_of(&mut rig, 1, code);
        assert_eq!(master, rig.fpb());
        let old = rig.pcm(0);
        let old_anchor = rig.anchor();
        rig.set_input(|f| -code(f));
        let mark = rig.events.len();
        let (start, end) = arm_fixed(&mut rig, 1, 4.0);
        assert_eq!(end - start, 4 * master, "the window is four loops");
        let boundary = start - align;
        assert_eq!((boundary - old_anchor).rem_euclid(master), 0, "it starts on a boundary of the old loop");
        rig.advance_to(end);
        assert_eq!((rig.master(), rig.state(1)), (master, LaneState::Recording), "nothing moves before the window closes");
        rig.advance(1);
        assert_eq!(rig.master(), 4 * master, "align={align}: the take is the new master");
        assert_eq!(rig.anchor(), boundary, "the grid re-anchors on the take's boundary");
        assert!((0..2).all(|i| rig.state(i) == LaneState::Playing && rig.lane(i).length == 4 * master), "both lanes play the new length");
        rig.set_level(0.0);
        rig.idle();
        let take = rig.pcm(1);
        assert_eq!(take.len() as Frame, 4 * master);
        assert!(take.iter().enumerate().all(|(k, &x)| x == -code(start + k as Frame)), "take frame k sits at loop position k");
        assert_eq!(rig.pcm(0), tiled(&old, 4 * master), "the old loop tiled four times, bit-exact");
        // The feed says so: the transport's master and both lanes' length, and the overview draws it.
        assert!(rig.events[mark..].iter().any(|e| matches!(*e, Event::Transport { master: m, .. } if m == 4 * master)));
        for lane in [0u8, 1] {
            assert!(rig.events[mark..].iter().any(|e| matches!(*e, Event::Lane { lane: l, info, .. } if l == lane && info.length == 4 * master)));
            assert_eq!(rig.engine.looper().overview().lane(lane as usize).frames, 4 * master);
            peaks_describe(&rig, lane as usize, "multiplied");
        }
        // The old lane is one loop of the new length now: a layer over all of it plays back whole (no
        // read-through into the old loop is left).
        rig.set_input(|f| 0.125 * code(f + 4321));
        rig.press(Command::RecDub(0));
        rig.advance(4 * master);
        rig.press(Command::RecDub(0));
        rig.set_level(0.0);
        rig.advance(align + 1);
        rig.idle();
        assert_ne!(rig.pcm(0), tiled(&old, 4 * master));
        rig.keep_output();
        let from = rig.frame;
        rig.advance(4 * master);
        let (pcm0, pcm1) = (rig.pcm(0), rig.pcm(1));
        let out = &rig.output.as_ref().unwrap().1;
        for f in from..rig.frame {
            let pos = (f - boundary).rem_euclid(4 * master) as usize;
            assert_eq!(out[(f - from) as usize], 0.0 + pcm0[pos] + pcm1[pos], "align={align}: frame {f}, the dubbed loop");
        }
        steps_stay_small(&rig);
    }
}

#[test]
fn b_an_old_lane_plays_on_unbroken_across_the_take_the_commit_and_the_new_loop() {
    for reversed in [false, true] {
        for align in [0, 480] {
            let mut rig = Rig::with(Opts { align, ..Default::default() });
            let master = master_of(&mut rig, 1, code);
            if reversed {
                rig.press(Command::Reverse(0));
                rig.advance_to(rig.next_boundary() + 1); // the reversal swaps in on the loop boundary
                assert!(rig.lane(0).reversed);
            }
            // The loop as it plays, from the old anchor: what every later frame must continue.
            let played = rig.pcm(0);
            let old_anchor = rig.anchor();
            rig.keep_output();
            let from = rig.frame;
            let (_, end) = arm_fixed(&mut rig, 1, 4.0); // a silent take: the tap is lane 0 alone
            rig.advance_to(end + 4 * master + master / 3);
            assert_eq!(rig.master(), 4 * master);
            let out = &rig.output.as_ref().unwrap().1;
            for f in from..rig.frame {
                let want = played[(f - old_anchor).rem_euclid(master) as usize];
                assert_eq!(out[(f - from) as usize], want, "reversed={reversed} align={align}: frame {f} ({} after the commit)", f - end);
            }
            rig.idle();
            assert_eq!(rig.pcm(0), tiled(&played, 4 * master), "reversed={reversed}: the loop as it plays, tiled");
        }
    }
}

/// A jam through a multiply: a forward lane with an undo target, a reversed copy, a FIXED 4 take, a COPY
/// still running at its commit, UNDO after it. Its output before the limiter and as heard.
fn jam(sr: u32, block: usize) -> [Vec<f32>; 2] {
    let mut rig = Rig::with(Opts { sr, start: sr as Frame, loop_seconds: 20.0, block, align: 37 });
    rig.keep_output();
    rig.set(Command::SetMetronome(true));
    let master = master_of(&mut rig, 1, code);
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::Reverse(1));
    rig.set_input(|f| 0.125 * code(f + 12_345));
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.press(Command::RecDub(0));
    rig.set_input(|f| -code(f));
    rig.advance(100);
    let (_, end) = arm_fixed(&mut rig, 2, 4.0);
    rig.advance_to(end - job_frames(master) / 2);
    rig.press(Command::Copy(0));
    rig.advance_to(end + 1);
    assert!(rig.master() == 4 * master && rig.engine.looper().busy(), "sr={sr} block={block}: committed, extending");
    rig.set_level(0.0);
    rig.advance(4 * master + master / 5);
    rig.press(Command::Undo(0));
    rig.advance(4 * master + master / 3);
    let states: Vec<_> = (0..5).map(|i| rig.state(i)).collect();
    assert_eq!(states, [LaneState::Playing, LaneState::Playing, LaneState::Playing, LaneState::Playing, LaneState::Empty]);
    steps_stay_small(&rig);
    [rig.output.take().unwrap().1, std::mem::take(&mut rig.heard)]
}

#[test]
fn b_a_multiply_renders_bit_identically_at_any_block_size() {
    let sr = 8000;
    let reference = jam(sr, 128);
    for block in [1, 32, 64, 127, 480, 1024] {
        for (tap, (out, reference)) in ["pre-limiter", "heard"].iter().zip(jam(sr, block).iter().zip(&reference)) {
            assert_eq!(out.len(), reference.len(), "block={block} {tap}");
            let first = out.iter().zip(reference).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "block={block}: the {tap} output differs from block 128");
        }
    }
}

#[test]
fn c_the_beats_and_the_click_carry_on_across_the_commit() {
    // The same session with and without a multiply take: a silent loop, so the tap is the click alone.
    let run = |multiply: bool| {
        let mut rig = Rig::new();
        rig.set(Command::SetMetronome(true));
        master_of(&mut rig, 1, |_| 0.0);
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(4.0));
        let mark = rig.events.len();
        rig.keep_output();
        let master = rig.master();
        if multiply {
            rig.press(Command::RecDub(1));
        } else {
            rig.advance(1);
        }
        rig.advance(10 * master);
        (rig, mark)
    };
    let (with, mark) = run(true);
    let (without, mark_without) = run(false);
    let master = without.master();
    assert_eq!(with.master(), 4 * master, "the multiply committed inside the window");
    let beats = with.beats_since(mark);
    assert_eq!(beats, without.beats_since(mark_without), "the same beats: frame, bar position, click");
    // One beat per grid step of the old loop, none doubled or missing, the accent on each bar's one.
    let grid = Grid::master(without.anchor(), master, 1);
    let first = grid.first_beat_at_or_after(beats[0].0);
    assert!(beats.len() >= 40, "{} beats", beats.len());
    for (k, &(frame, in_bar, count_left, clicked)) in beats.iter().enumerate() {
        let n = first + k as u64;
        assert_eq!((frame, in_bar as u64, count_left, clicked), (grid.beat_frame(n), n % 4, 0, true), "beat {k}");
    }
    assert!(with.output.as_ref().unwrap().1 == without.output.as_ref().unwrap().1, "the click renders bit-identically");
    assert!(with.heard == without.heard);
}

#[test]
fn d_undo_after_a_multiply_gives_the_loop_from_before_the_dub_tiled() {
    let mut rig = Rig::new();
    let master = master_of(&mut rig, 1, code);
    let pre = rig.pcm(0);
    rig.set_input(|f| 0.25 * code(f + 777));
    rig.press(Command::RecDub(0));
    rig.advance(master);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(100);
    rig.idle();
    let dubbed = rig.pcm(0);
    assert_ne!(dubbed, pre);
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(pre.clone()));
    let (_, end) = arm_fixed(&mut rig, 1, 4.0);
    rig.advance_to(end + 1);
    assert_eq!(rig.master(), 4 * master);
    assert!(rig.lane(0).can_undo, "the undo target survives the multiply");
    rig.press(Command::Undo(0));
    assert!(rig.engine.holding(), "UNDO waits for the lane's extension");
    rig.idle();
    assert_eq!(rig.pcm(0), tiled(&pre, 4 * master), "UNDO: the pre-dub loop, tiled to the new master");
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(tiled(&dubbed, 4 * master)), "and the dubbed loop to redo, tiled");
    peaks_describe(&rig, 0, "undone");
    rig.press(Command::Undo(0));
    rig.idle();
    assert_eq!(rig.pcm(0), tiled(&dubbed, 4 * master));
    peaks_describe(&rig, 0, "redone");
}

/// A two-bar master of the frame code and a take on lane 1 of the negated code (FIXED at `fixed` bars;
/// `None`: FIXED off, a free take), ended by `stop` (REC/DUB, or the device stopping) `bars` bars after
/// the take's musical start (`None`: at its window end). Returns the rig (settled), the old loop and the
/// take's first frame.
fn early_stop(align: Frame, fixed: Option<f64>, stop: Option<(f64, bool)>) -> (Rig, Vec<f32>, Frame) {
    let mut rig = Rig::with(Opts { align, ..Default::default() });
    master_of(&mut rig, 2, code);
    let old = rig.pcm(0);
    rig.set_input(|f| -code(f));
    let (start, end) = match fixed {
        Some(bars) => arm_fixed(&mut rig, 1, bars),
        None => {
            rig.press(Command::RecDub(1));
            (rig.start_frame(), rig.end_frame())
        }
    };
    match stop {
        Some((bars, punch_out)) => {
            rig.advance_to(start - align + (bars * rig.fpb() as f64) as Frame);
            if punch_out {
                rig.punch_out();
            } else {
                rig.press(Command::RecDub(1));
            }
        }
        None => rig.advance_to(end + 1),
    }
    // A stop that records on waits for its bar line or loop boundary.
    if let Some(end) = rig.window().and_then(|w| w.2) {
        rig.advance_to(end + 1);
    }
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.window().is_none() && rig.state(1) == LaneState::Playing, "{fixed:?} {stop:?}: committed");
    (rig, old, start)
}

/// The first `bars` bars of the take that began at `start`, as it was played.
fn take_bars(rig: &Rig, start: Frame, bars: Frame) -> Vec<f32> {
    (0..bars * rig.fpb()).map(|k| -code(start + k)).collect()
}

#[test]
fn e_an_early_stop_keeps_the_nearest_whole_loops_past_the_master_and_whole_bars_below_it() {
    // Both gestures, one rule (E10): a FIXED 8 multiply window and a free take, over a 2-bar master.
    for align in [0, 480] {
        for fixed in [Some(8.0), None] {
            let fpb = Rig::new().fpb();
            for punch_out in [false, true] {
                // 1.5 bars, inside the first loop pass: an ordinary one-bar take, tiled across the master.
                let (rig, old, start) = early_stop(align, fixed, Some((1.5, punch_out)));
                assert_eq!(rig.master(), 2 * fpb, "{fixed:?}: 1.5 bars do not multiply");
                assert_eq!(rig.pcm(1), tiled(&take_bars(&rig, start, 1), 2 * fpb), "one bar tiled across the master");
                assert_eq!(rig.pcm(0), old, "the master is untouched");
                // 2.6 bars, 1.3 loops: the nearest is one loop, which the take already holds.
                let (rig, old, start) = early_stop(align, fixed, Some((2.6, punch_out)));
                assert_eq!(rig.master(), 2 * fpb, "{fixed:?} punch_out={punch_out}: 1.3 loops keep one");
                assert!(rig.pcm(1) == take_bars(&rig, start, 2) && rig.pcm(0) == old);
            }
            // 5.3 bars, 2.65 loops: REC records on to the nearest whole loop, three (six bars). The device
            // stopping cannot record on: it keeps the two loops it completed.
            let (rig, old, start) = early_stop(align, fixed, Some((5.3, false)));
            assert_eq!(rig.master(), 6 * fpb, "align={align} {fixed:?}: 5.3 bars record on to 6");
            assert_eq!(rig.pcm(1), take_bars(&rig, start, 6), "the first six bars of the take");
            assert_eq!(rig.pcm(0), tiled(&old, 6 * fpb));
            let (rig, old, start) = early_stop(align, fixed, Some((5.3, true)));
            assert_eq!(rig.master(), 4 * fpb, "a punch-out keeps the completed loops");
            assert!(rig.pcm(1) == take_bars(&rig, start, 4) && rig.pcm(0) == tiled(&old, 4 * fpb));
            // 3.5 bars, 1.75 loops: two loops, recording on to the boundary. 3 bars is the tie: up.
            for bars in [3.5, 3.0] {
                let (rig, old, start) = early_stop(align, fixed, Some((bars, false)));
                assert_eq!(rig.master(), 4 * fpb, "{fixed:?}: {bars} bars keep two loops");
                assert!(rig.pcm(1) == take_bars(&rig, start, 4) && rig.pcm(0) == tiled(&old, 4 * fpb));
            }
            // A stop in the grace before the fourth bar line keeps the 4 bars, waiting for the tail.
            let (rig, _, _) = early_stop(align, fixed, Some((4.0 - 1.0 / 32.0, false)));
            assert_eq!(rig.master(), 4 * fpb);
            // A stop inside the first bar records on to its bar line: one bar, tiled.
            let (rig, _, start) = early_stop(align, fixed, Some((0.4, false)));
            assert!(rig.master() == 2 * fpb && rig.pcm(1) == tiled(&take_bars(&rig, start, 1), 2 * fpb));
        }
        // FIXED 3 over a 2-bar master is a 2-bar take: whole loops only.
        let fpb = Rig::new().fpb();
        let mut rig = Rig::with(Opts { align, ..Default::default() });
        master_of(&mut rig, 2, code);
        let (start, end) = arm_fixed(&mut rig, 1, 3.0);
        assert_eq!(end - start, 2 * fpb, "FIXED 3 records the master's two bars");
        rig.press(Command::Stop(1));
        let (rig, old, start) = early_stop(align, Some(3.0), None);
        assert_eq!(rig.master(), 2 * fpb);
        assert!(rig.pcm(1) == take_bars(&rig, start, 2) && rig.pcm(0) == old);
    }
}

#[test]
fn f_the_next_take_may_reach_the_longest_multiply_the_buffer_holds() {
    // (rate, bpm, lane buffer seconds, the bound over a 1-, 2-, 3- and 4-bar master)
    for (sr, bpm, seconds, want) in [
        (48000, 120.0, 60.0, [30, 30, 30, 28]),
        (48000, 120.0, 20.0, [10, 10, 9, 8]),
        // 137 bpm at 44.1 kHz fits 34 bars in 60 s: the FIXED bound of 32 caps it.
        (44100, 137.0, 60.0, [32, 32, 30, 32]),
    ] {
        for (k, bars) in [1, 2, 3, 4].into_iter().enumerate() {
            let mut rig = Rig::with(Opts { sr, start: sr as Frame, loop_seconds: seconds, ..Default::default() });
            rig.set(Command::SetBpm(bpm));
            assert_eq!(rig.engine.looper().next_take_max_bars(rig.bpm()), 32, "no master: 32");
            master_of(&mut rig, bars, |_| 0.25);
            let max = rig.engine.looper().next_take_max_bars(rig.bpm());
            assert_eq!(max, want[k], "sr={sr} bpm={bpm} {seconds} s, a {bars}-bar master");
            // FIXED at 32 records exactly that.
            let (start, end) = arm_fixed(&mut rig, 1, 32.0);
            assert_eq!(end - start, max * rig.fpb());
        }
    }
}

#[test]
fn g_a_copy_still_running_at_the_commit_ends_extended() {
    let mut rig = Rig::new();
    let master = master_of(&mut rig, 1, code);
    let old = rig.pcm(0);
    rig.set_input(|f| -code(f));
    let (_, end) = arm_fixed(&mut rig, 1, 4.0);
    rig.advance_to(end - job_frames(master) / 2);
    rig.press(Command::Copy(0));
    assert_eq!(rig.state(2), LaneState::Stopped, "lane 2 takes the copy");
    rig.advance_to(end + 1);
    assert_eq!(rig.master(), 4 * master);
    assert!(rig.state(2) == LaneState::Stopped && rig.lane(2).length == 4 * master, "still copying, and already the new length");
    rig.set_level(0.0);
    rig.keep_output();
    let from = rig.frame;
    // COPY from a lane still extending waits for it, then copies the whole new loop.
    rig.press(Command::Copy(0));
    assert!(rig.engine.holding());
    rig.idle();
    rig.advance(4 * master + master / 3);
    let want = tiled(&old, 4 * master);
    for i in [0, 2, 3] {
        assert_eq!((rig.state(i), rig.pcm(i) == want), (LaneState::Playing, true), "lane {i}: the loop, extended");
        peaks_describe(&rig, i, "copied");
    }
    let resumed = |to: u8| rig.events.iter().find_map(|e| match *e { Event::Copied { frame, from: 0, to: t, .. } if t == to => Some(frame), _ => None }).unwrap();
    let (resumed2, resumed3) = (resumed(2), resumed(3));
    assert!(resumed2 > end && resumed3 > resumed2, "the first copy finished after the commit, the second after the extension");
    // From the commit on, every lane at the new grid phase (a lane mid-extension reads through its loop),
    // each copy fading in over 5 ms from its resume into the running loop (D23).
    let (anchor, len, n) = (rig.anchor(), rig.master(), common::dub::ramp(rig.sr));
    let pcms: Vec<Vec<f32>> = (0..4).map(|i| rig.pcm(i)).collect();
    let out = &rig.output.as_ref().unwrap().1;
    for f in from..rig.frame {
        let pos = (f - anchor).rem_euclid(len) as usize;
        let mut want = 0.0f32;
        for (i, p) in pcms.iter().enumerate() {
            let resumed = [None, None, Some(resumed2), Some(resumed3)][i];
            want += match resumed {
                None => p[pos],
                Some(r) if f >= r => common::edges::sample(1.0, common::edges::join(f - r, n), p[pos], 0.0),
                Some(_) => 0.0,
            };
        }
        assert_eq!(out[(f - from) as usize], want, "frame {f}");
    }
    steps_stay_small(&rig);
}

/// Hand a snapshot into `samples` to the engine and render until it comes back (`tests/session.rs`).
fn snapshot(rig: &mut Rig, samples: usize) -> Snapshot {
    let mut pcm = Vec::with_capacity(samples);
    pcm.resize(samples, 0.0f32);
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(pcm)))).is_ok());
    for _ in 0..100_000 {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            match *job {
                SessionJob::Snapshot(s) => return s,
                SessionJob::Load(_) => unreachable!(),
            }
        }
    }
    panic!("the snapshot never came back");
}

#[test]
fn h_a_snapshot_after_a_multiply_holds_the_new_master_on_every_track() {
    let mut rig = Rig::new();
    let master = master_of(&mut rig, 1, code);
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::Reverse(1));
    rig.set_input(|f| -code(f));
    let (_, end) = arm_fixed(&mut rig, 2, 4.0);
    rig.advance_to(end + 1);
    rig.set_level(0.0);
    let s = snapshot(&mut rig, 3 * 4 * master as usize);
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!((s.master, s.count), (4 * master, 3));
    for k in 0..3 {
        let t = s.tracks[k].unwrap();
        let pcm = &s.pcm[k * 4 * master as usize..(k + 1) * 4 * master as usize];
        assert_eq!(pcm, &rig.pcm(t.index as usize)[..], "track {}: its loop in play order, the new master long", t.index);
    }
    assert!(s.tracks[1].unwrap().reversed);
}

#[test]
fn i_the_most_jobs_a_multiply_starts_fit_and_every_step_stays_small() {
    for block in [1, 128, 1024] {
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        let master = master_of(&mut rig, 1, code);
        rig.press(Command::Copy(0));
        rig.idle();
        // Both lanes get an undo target: each extends two buffers.
        for lane in [0u8, 1] {
            rig.set_input(move |f| 0.125 * code(f + 999 * lane as Frame));
            rig.press(Command::RecDub(lane));
            rig.advance(master / 3);
            rig.press(Command::RecDub(lane));
            rig.set_level(0.0);
            rig.advance(100);
            rig.idle();
        }
        let before: Vec<Vec<f32>> = (0..2).map(|i| rig.pcm(i)).collect();
        let undo: Vec<Vec<f32>> = (0..2).map(|i| rig.engine.looper().undo_pcm(i).unwrap()).collect();
        rig.set_input(|f| -code(f));
        let (_, end) = arm_fixed(&mut rig, 4, 4.0);
        // Two COPYs still running into lanes 2 and 3 at the commit.
        rig.advance_to(end - job_frames(master) / 2);
        rig.press(Command::Copy(0));
        rig.press(Command::Copy(1));
        rig.advance_to(end + 1);
        rig.set_level(0.0);
        assert_eq!(rig.master(), 4 * master, "block={block}");
        rig.idle();
        for i in 0..2 {
            assert_eq!(rig.pcm(i), tiled(&before[i], 4 * master), "block={block} lane {i}");
            assert_eq!(rig.engine.looper().undo_pcm(i), Some(tiled(&undo[i], 4 * master)), "block={block} lane {i}'s undo");
            assert_eq!(rig.pcm(i + 2), tiled(&before[i], 4 * master), "block={block}: lane {}'s copy", i + 2);
        }
        steps_stay_small(&rig);
    }
}

#[test]
fn j_a_free_later_take_runs_to_the_longest_multiply_and_closes_there_by_itself() {
    for align in [0, 480] {
        let mut rig = Rig::with(Opts { align, ..Default::default() });
        let master = master_of(&mut rig, 2, code);
        let old = rig.pcm(0);
        rig.set_input(|f| -code(f));
        rig.press(Command::RecDub(1));
        let (start, end) = (rig.start_frame(), rig.end_frame());
        let max = rig.engine.looper().next_take_max_bars(rig.bpm());
        assert_eq!((max, end - start), (10, 10 * rig.fpb()), "the 20 s buffer holds five loops of two bars");
        rig.advance_to(end);
        assert_eq!((rig.state(1), rig.master()), (LaneState::Recording, master), "align={align}: it runs on past the loop");
        rig.advance(1);
        assert_eq!(rig.master(), 5 * master, "closed by itself: a multiply of five loops");
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(rig.pcm(1), take_bars(&rig, start, 10), "take frame k at loop position k");
        assert_eq!(rig.pcm(0), tiled(&old, 5 * master), "the old loop, five times, bit-exact");
        steps_stay_small(&rig);
    }
    // RETAKE is unchanged: it rolls at the loop's length.
    let mut rig = Rig::new();
    let master = master_of(&mut rig, 2, code);
    rig.set(Command::SetRetake(true));
    rig.press(Command::RecDub(1));
    assert_eq!(rig.end_frame() - rig.start_frame(), master, "a RETAKE pass is one loop");
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!((rig.state(1), rig.lane(1).retake_pass, rig.master()), (LaneState::Recording, 2, master));
}

#[test]
fn k_a_free_later_take_pressed_past_the_loop_keeps_the_nearest_whole_loops() {
    for align in [0, 480] {
        // 1.3 loops: one loop, committed at the press, the master as it was.
        let mut rig = Rig::with(Opts { align, ..Default::default() });
        let master = master_of(&mut rig, 2, code);
        let old = rig.pcm(0);
        rig.set_input(|f| -code(f));
        rig.press(Command::RecDub(1));
        let start = rig.start_frame();
        rig.advance_to(start - align + master * 13 / 10);
        rig.press(Command::RecDub(1));
        assert_eq!((rig.state(1), rig.master()), (LaneState::Playing, master), "align={align}: committed at the press");
        rig.set_level(0.0);
        rig.idle();
        assert!(rig.pcm(1) == take_bars(&rig, start, 2) && rig.pcm(0) == old);
        steps_stay_small(&rig);
    }
    // 1.6 loops: it records on to two loops and commits as a multiply; the old lane (forward or reversed)
    // plays on unbroken across the take, the commit and the grown loop (a silent take: the tap is lane 0).
    for reversed in [false, true] {
        for align in [0, 480] {
            let mut rig = Rig::with(Opts { align, ..Default::default() });
            let master = master_of(&mut rig, 2, code);
            if reversed {
                rig.press(Command::Reverse(0));
                rig.advance_to(rig.next_boundary() + 1);
            }
            let played = rig.pcm(0);
            let old_anchor = rig.anchor();
            rig.keep_output();
            let from = rig.frame;
            rig.press(Command::RecDub(1));
            let start = rig.start_frame();
            rig.advance_to(start - align + master * 16 / 10);
            rig.press(Command::RecDub(1));
            assert_eq!((rig.state(1), rig.end_frame()), (LaneState::Recording, start + 2 * master), "records on to two loops");
            rig.advance_to(start + 4 * master + master / 3);
            assert_eq!(rig.master(), 2 * master, "reversed={reversed} align={align}: a multiply to two loops");
            let out = &rig.output.as_ref().unwrap().1;
            for f in from..rig.frame {
                let want = played[(f - old_anchor).rem_euclid(master) as usize];
                assert_eq!(out[(f - from) as usize], want, "reversed={reversed} align={align}: frame {f}");
            }
            rig.idle();
            assert_eq!(rig.pcm(0), tiled(&played, 2 * master), "the loop as it plays, twice");
            steps_stay_small(&rig);
        }
    }
}

#[test]
fn l_a_dotted_stutter_keeps_its_phase_across_the_commit() {
    // Lane 0's loop plays through a 1/8. STUTTER, with and without a silent FIXED take on lane 1 that
    // multiplies it. The multiply re-anchors the loop a whole number of bars later, which is no whole
    // number of dotted eighths (four beats over three quarters of one): the FX follow the beat grid's
    // origin, which the multiply leaves where it was, so the bus cannot tell the two sessions apart.
    let run = |multiply: bool| {
        let mut rig = Rig::new();
        let master = master_of(&mut rig, 1, code);
        rig.set(Command::SetFxParam(0, FxParam::Rate, 2.0));
        rig.set(Command::SetFxBypass(0, FxKind::Stutter, false));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(4.0));
        // A take whose boundary lies a whole number of bars from the old anchor that three does not divide:
        // there the old code's gate would jump phase.
        while ((rig.next_boundary() - rig.anchor()) / master) % 3 == 0 {
            rig.advance(master);
        }
        rig.keep_output();
        if multiply {
            rig.press(Command::RecDub(1));
        } else {
            rig.advance(1);
        }
        rig.advance(10 * master);
        rig
    };
    let (with, without) = (run(true), run(false));
    assert_eq!(with.master(), 4 * without.master(), "the multiply committed inside the render");
    assert!(with.anchor() != without.anchor(), "and re-anchored the loop");
    for (tap, a, b) in [("left", &with.bus, &without.bus), ("right", &with.bus_right, &without.bus_right)] {
        assert_eq!(a.len(), b.len());
        let first = a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits());
        assert_eq!(first, None, "the {tap} bus differs from the session without a multiply");
    }
    assert!(with.bus.iter().any(|&x| x != 0.0), "the lane sounds through its gate");
}
