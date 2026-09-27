//! F16 TRIM (a tester report; STATUS E11, approved): a lane keeps its first N bars as heard and repeats
//! them across the loop, cut at the loop end (3 over 8 plays 3+3+2), the map a short later take commits
//! with; the loop keeps its length, the lane its orientation. The loop before the trim is the lane's
//! one-level undo target, so UNDO toggles it as it does an overdub. A PLAYING lane hears the trimmed loop
//! from the next loop boundary; a block job builds it, in heard order, so it is ready there even for a
//! press a frame before the boundary. A TRIM pressed while the loop playing until that boundary is the
//! spare it would write (an UNDO or a TRIM pressed in the same loop) waits for the boundary, and is heard
//! from there. No web guard precedes this: the Web Audio looper never trimmed.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::looper::{job_frames, JOB_RATE};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{Command, Event, LaneInfo, LaneState, Refusal};

/// 8 kHz keeps the loops short; the frame code stays unique.
const SR: u32 = 8000;

fn rig_with(block: usize) -> Rig {
    Rig::with(Opts { sr: SR, start: SR as Frame, block, ..Default::default() })
}

fn rig() -> Rig {
    rig_with(128)
}

/// `bars` bars of the frame code committed on lane 0 (lane 0 PLAYING), then the input silent. Returns the
/// master.
fn loop_of(rig: &mut Rig, bars: Frame) -> Frame {
    rig.set_input(code);
    let master = rig.record_first_take(0, bars, 240);
    rig.set_level(0.0);
    rig.idle();
    master
}

/// Reverse lane 0 and let the reversal land on its boundary.
fn reverse_lane_0(rig: &mut Rig) {
    rig.press(Command::Reverse(0));
    rig.advance_to(rig.next_boundary() + 1);
    assert!(rig.lane(0).reversed);
}

/// The first `frames` of `pcm` repeated out to its length.
fn first_tiled(pcm: &[f32], frames: Frame) -> Vec<f32> {
    (0..pcm.len()).map(|k| pcm[k % frames as usize]).collect()
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

/// From `from`, the kept output equals `want(frame, loop position)`.
fn plays(rig: &Rig, from: Frame, want: impl Fn(Frame, usize) -> f32, tag: &str) {
    let (start, out) = rig.output.as_ref().unwrap();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in from.max(*start)..start + out.len() as Frame {
        let pos = (f - anchor).rem_euclid(master) as usize;
        assert_eq!(out[(f - start) as usize], want(f, pos), "{tag}: frame {f} pos {pos}");
    }
}

/// Press TRIM on `lane` and return the refusals it drew.
fn trim(rig: &mut Rig, lane: u8, bars: u32) -> Vec<Refusal> {
    let mark = rig.events.len();
    rig.press(Command::Trim(lane, bars));
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { lane: l, reason, .. } if l == lane => Some(reason),
            _ => None,
        })
        .collect()
}

#[test]
fn a_a_trim_keeps_the_first_bars_as_heard_tiled_and_undo_toggles_it() {
    for reversed in [false, true] {
        for bars in [1, 3, 4, 7] {
            let tag = format!("reversed={reversed} bars={bars}");
            let mut rig = rig();
            let master = loop_of(&mut rig, 8);
            let fpb = rig.fpb();
            if reversed {
                reverse_lane_0(&mut rig);
            }
            let before = rig.pcm(0);
            assert_eq!(trim(&mut rig, 0, bars), [], "{tag}: accepted");
            rig.idle();
            let trimmed = first_tiled(&before, bars as Frame * fpb);
            assert_eq!(rig.pcm(0), trimmed, "{tag}: the first bars as heard, repeated and cut at the loop end");
            let t = rig.lane(0);
            assert_eq!((t.reversed, t.length, rig.master(), t.can_undo), (reversed, master, master, true), "{tag}: length and orientation stay");
            assert_eq!(rig.engine.looper().undo_pcm(0), Some(before.clone()), "{tag}: the loop before the trim is the undo target");
            peaks_describe(&rig, 0, &tag);
            rig.press(Command::Undo(0));
            rig.idle();
            assert_eq!(rig.pcm(0), before, "{tag}: UNDO gives back the exact loop");
            assert_eq!(rig.engine.looper().undo_pcm(0), Some(trimmed.clone()));
            peaks_describe(&rig, 0, &tag);
            rig.press(Command::Undo(0));
            rig.idle();
            assert_eq!(rig.pcm(0), trimmed, "{tag}: a second UNDO trims again");
            steps_stay_small(&rig);
        }
    }
    // The trim replaces an overdub's undo target: UNDO gives the dubbed loop back, not the one before it.
    let mut rig = rig();
    let master = loop_of(&mut rig, 4);
    rig.set_level(1.0 / 64.0);
    rig.press(Command::RecDub(0));
    rig.advance(master / 3);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(100);
    rig.idle();
    let dubbed = rig.pcm(0);
    assert_eq!(trim(&mut rig, 0, 2), []);
    rig.idle();
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(dubbed.clone()));
    rig.press(Command::Undo(0));
    rig.idle();
    assert_eq!(rig.pcm(0), dubbed);
}

#[test]
fn b_a_trim_while_playing_is_heard_from_the_next_boundary_and_not_before() {
    for reversed in [false, true] {
        let mut probe = rig();
        let master = loop_of(&mut probe, 4);
        // Pressed a third of a loop before the boundary, inside the job's run, and a frame before it.
        for early in [master / 3, job_frames(master) / 2, 1] {
            let tag = format!("reversed={reversed} early={early}");
            let mut rig = rig();
            loop_of(&mut rig, 4);
            if reversed {
                reverse_lane_0(&mut rig);
            }
            let before = rig.pcm(0);
            let trimmed = first_tiled(&before, rig.fpb());
            rig.advance_to(rig.next_boundary() - early);
            let boundary = rig.next_boundary();
            rig.keep_output();
            let from = rig.frame;
            // One frame a block from the press to past the boundary: a chunk no longer than a frame, so
            // the job cannot finish ahead of the reader in one chunk and must stay ahead in heard order.
            rig.block = 1;
            assert_eq!(trim(&mut rig, 0, 1), [], "{tag}");
            rig.advance_to(boundary + 4096);
            rig.block = 128;
            rig.advance_to(boundary + master + master / 3);
            plays(&rig, from, |f, pos| if f < boundary { before[pos] } else { trimmed[pos] }, &tag);
            steps_stay_small(&rig);
        }
    }
}

#[test]
fn c_a_trim_during_a_multiply_extension_waits_for_it() {
    let mut rig = rig();
    let master = loop_of(&mut rig, 2);
    let old = rig.pcm(0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.end_frame() + 1);
    assert!(rig.master() == 2 * master && rig.engine.looper().busy(), "committed, extending lane 0");
    assert_eq!(trim(&mut rig, 0, 1), []);
    assert!(rig.engine.holding(), "the trim waits for lane 0's extension");
    rig.idle();
    let grown = tiled(&old, 2 * master);
    assert_eq!(rig.pcm(0), first_tiled(&grown, rig.fpb()), "the grown loop's first bar, repeated");
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(grown), "the undo target is the grown loop");
    steps_stay_small(&rig);
}

#[test]
fn c_the_jobs_a_multiply_starts_beside_trims_in_flight_fit() {
    for block in [1, 128] {
        let mut rig = rig_with(block);
        let master = loop_of(&mut rig, 2);
        for _ in 1..4 {
            rig.press(Command::Copy(0));
            rig.idle();
        }
        // Every lane with an undo target: each extends two buffers.
        for lane in 0..4u8 {
            rig.set_level(1.0 / 64.0);
            rig.press(Command::RecDub(lane));
            rig.advance(master / 3);
            rig.press(Command::RecDub(lane));
            rig.set_level(0.0);
            rig.advance(100);
            rig.idle();
        }
        let before: Vec<Vec<f32>> = (0..4).map(|i| rig.pcm(i)).collect();
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(4.0));
        rig.press(Command::RecDub(4));
        let end = rig.end_frame();
        // A trim on every other lane, still running at the commit.
        rig.advance_to(end - 1);
        for lane in 0..4u8 {
            rig.send_at(rig.frame, Command::Trim(lane, 1));
        }
        rig.advance_to(end + 1);
        assert_eq!(rig.master(), 2 * master, "block={block}");
        rig.idle();
        let fpb = rig.fpb();
        for (i, before) in before.iter().enumerate() {
            assert_eq!(rig.pcm(i), first_tiled(&tiled(before, 2 * master), fpb), "block={block} lane {i}");
            assert_eq!(rig.engine.looper().undo_pcm(i), Some(tiled(before, 2 * master)), "block={block} lane {i}'s undo");
        }
        steps_stay_small(&rig);
    }
}

#[test]
fn d_a_trim_is_refused_with_a_reason_and_changes_nothing() {
    let mut rig = rig();
    assert_eq!(trim(&mut rig, 0, 1), [Refusal::NoTrim], "an EMPTY lane");
    loop_of(&mut rig, 4);
    let unchanged = |rig: &Rig, info: LaneInfo, pcm: &[f32], tag: &str| {
        assert!(!rig.engine.looper().busy(), "{tag}: no job");
        assert_eq!((rig.lane(0), rig.pcm(0).as_slice()), (info, pcm), "{tag}: the lane as it was");
    };
    let (info, pcm) = (rig.lane(0), rig.pcm(0));
    for bars in [0, 4, 5, 99] {
        assert_eq!(trim(&mut rig, 0, bars), [Refusal::NoTrim], "{bars} bars of a 4-bar loop");
        unchanged(&rig, info, &pcm, &format!("{bars} bars"));
    }
    // Its own overdub, and a take waiting on another lane: both capture.
    rig.set_level(1.0 / 64.0);
    rig.press(Command::RecDub(0));
    assert_eq!(trim(&mut rig, 0, 2), [Refusal::Capturing], "an overdubbing lane");
    rig.press(Command::Stop(0)); // the layer is discarded, the lane stopped
    rig.set_level(0.0);
    rig.idle();
    rig.press(Command::PlayStop(0));
    unchanged(&rig, info, &pcm, "the discarded layer");
    rig.press(Command::RecDub(1));
    assert_eq!(trim(&mut rig, 1, 1), [Refusal::Capturing], "an armed take");
    rig.press(Command::Stop(1));
    // A pending loop-end stop.
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    let stopping = rig.lane(0);
    assert!(stopping.stop_at.is_some());
    assert_eq!(trim(&mut rig, 0, 2), [Refusal::Stopping]);
    unchanged(&rig, stopping, &pcm, "stopping");
    // A one-bar loop has nothing to keep a part of.
    let mut rig = self::rig();
    loop_of(&mut rig, 1);
    assert_eq!(trim(&mut rig, 0, 1), [Refusal::NoTrim], "a one-bar loop");
}

#[test]
fn e_a_stopped_lane_trims_at_once_and_plays_it_once_the_job_is_done() {
    let mut rig = rig();
    let master = loop_of(&mut rig, 4);
    let before = rig.pcm(0);
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Stopped);
    assert_eq!(trim(&mut rig, 0, 3), []);
    rig.press(Command::PlayStop(0)); // a resume waits for the job
    assert!(rig.engine.holding() && rig.state(0) == LaneState::Stopped);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    let trimmed = first_tiled(&before, 3 * rig.fpb());
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master + master / 2);
    plays(&rig, from, |_, pos| trimmed[pos], "resumed");
}

#[test]
fn f_a_trim_during_an_undo_swap_waits_for_it_and_is_heard_from_the_boundary() {
    let mut rig = rig();
    let master = loop_of(&mut rig, 4);
    let pre = rig.pcm(0);
    rig.set_level(1.0 / 64.0);
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(100);
    rig.idle();
    let dubbed = rig.pcm(0);
    rig.advance_to(rig.next_boundary() + master / 5);
    rig.press(Command::Undo(0)); // the dubbed loop plays on to the boundary
    rig.advance(master / 5);
    let boundary = rig.next_boundary();
    rig.keep_output();
    let from = rig.frame;
    assert_eq!(trim(&mut rig, 0, 1), []);
    assert!(rig.engine.holding(), "the trim waits for the undo's swap: it would write the loop playing now");
    rig.advance_to(boundary + master / 2);
    let trimmed = first_tiled(&pre, rig.fpb());
    plays(&rig, from, |f, pos| if f < boundary { dubbed[pos] } else { trimmed[pos] }, "undo, then trim");
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(pre.clone()), "the loop the undo gave back is the undo target");
    assert_ne!(dubbed, pre);
}

#[test]
fn h_a_second_trim_before_the_boundary_is_heard_there_instead_of_the_first() {
    for reversed in [false, true] {
        for (first, second) in [(4, 3), (2, 5)] {
            // Pressed while the first trim's job runs, and after it is done.
            for late in [false, true] {
                let tag = format!("reversed={reversed} trims {first} then {second}, late={late}");
                let mut rig = rig();
                let master = loop_of(&mut rig, 8);
                let gap = if late { 2 * job_frames(master) } else { 1 };
                let fpb = rig.fpb();
                if reversed {
                    reverse_lane_0(&mut rig);
                }
                let before = rig.pcm(0);
                rig.advance_to(rig.next_boundary() + master / 5);
                let boundary = rig.next_boundary();
                rig.keep_output();
                let from = rig.frame;
                assert_eq!(trim(&mut rig, 0, first), [], "{tag}");
                rig.advance(gap);
                assert_eq!(trim(&mut rig, 0, second), [], "{tag}");
                assert!(rig.engine.holding(), "{tag}: the second trim waits");
                rig.advance_to(boundary + master + master / 3);
                // Each trim keeps the first bars of the lane's loop as it stands: the second, of the first's.
                let once = first_tiled(&before, first as Frame * fpb);
                let twice = first_tiled(&once, second as Frame * fpb);
                plays(&rig, from, |f, pos| if f < boundary { before[pos] } else { twice[pos] }, &tag);
                assert_eq!(rig.pcm(0), twice, "{tag}");
                assert_eq!(rig.engine.looper().undo_pcm(0), Some(once), "{tag}: UNDO gives back the first trim");
                assert_eq!(rig.lane(0).reversed, reversed, "{tag}");
                steps_stay_small(&rig);
            }
        }
    }
}

/// A jam with trims: of a playing lane, of a reversed copy, UNDO, a trim of a stopped lane and its resume.
/// Its output before the limiter and as heard.
fn jam(block: usize) -> [Vec<f32>; 2] {
    let mut rig = rig_with(block);
    rig.keep_output();
    rig.set(Command::SetMetronome(true));
    let master = loop_of(&mut rig, 4);
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::Reverse(1));
    rig.advance(master / 3 + 17);
    rig.press(Command::Trim(0, 1));
    rig.advance(master / 2);
    rig.press(Command::Trim(1, 3));
    rig.advance(master + 5);
    rig.press(Command::Undo(0));
    rig.advance(master / 4);
    rig.press(Command::PlayStop(1));
    rig.press(Command::Trim(1, 2));
    rig.press(Command::PlayStop(1));
    rig.advance(2 * master + master / 3);
    assert!((0..2).all(|i| rig.state(i) == LaneState::Playing), "block={block}");
    steps_stay_small(&rig);
    [rig.output.take().unwrap().1, std::mem::take(&mut rig.heard)]
}

#[test]
fn g_trims_render_bit_identically_at_any_block_size() {
    let reference = jam(128);
    for block in [1, 32, 64, 127, 480, 1024] {
        for (tap, (out, reference)) in ["pre-limiter", "heard"].iter().zip(jam(block).iter().zip(&reference)) {
            assert_eq!(out.len(), reference.len(), "block={block} {tap}");
            let first = out.iter().zip(reference).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "block={block}: the {tap} output differs from block 128");
        }
    }
}
