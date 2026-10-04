//! D23's playback edges (STATUS § Decisions, D23): an undo's audible switch crossfades the loop heard into
//! the one it gives back over the punch ramp's N frames (240 at 48 kHz), a lane PLAYing into a running
//! loop fades in over them at its grid phase, and an immediate STOP fades out over them from the press,
//! from the level heard. Played audio only: the stored loops, the lane's state and events, the grid and
//! the transport change on the frames they always did. No web guard precedes this: the Web Audio looper
//! cut every one of them.
//!
//! The reference is `common::edges`: each frame's level and outgoing weight from the frame the edge
//! landed on, in the specified f64 operations, against the loops the test holds (the frame code names
//! which position plays where). A lane at volume 1 plays `level * x + outgoing`; the rig's kept output
//! sums the lanes in f32 in lane order.

mod common;

use common::dub::ramp;
use common::edges::{join, level, out, sample, tail};
use common::{code, Opts, Rig};
use lf_engine::grid::{Frame, Grid};
use lf_engine::{Command, Event, LaneState};

const DUB: f32 = 1.0 / 64.0;
/// The block sizes every new edge renders bit-identical at.
const BLOCKS: [usize; 8] = [1, 7, 64, 127, 128, 241, 480, 1024];

fn rig_at(block: usize) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set(Command::SetBpm(200.0));
    rig
}

/// A committed `bars`-bar loop at 200 bpm on lane 0, the take the frame code, PLAYING; the input silent.
fn code_loop(block: usize, bars: Frame) -> (Rig, Frame) {
    let mut rig = rig_at(block);
    rig.set_input(code);
    let master = rig.record_first_take(0, bars, 2400);
    rig.set_level(0.0);
    rig.idle();
    (rig, master)
}

/// Lane 0's loop with a layer of DUB over half a pass across the loop point (loop positions from three
/// quarters to a quarter): the undo target is the loop before it. Returns (the loop before, the dubbed
/// loop).
fn dubbed(rig: &mut Rig) -> (Vec<f32>, Vec<f32>) {
    let master = rig.master();
    let pre = rig.pcm(0);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 4);
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    let layered = rig.pcm(0);
    let (n, m) = (ramp(rig.sr) as usize, master as usize);
    assert!((0..n).chain(m - n..m).all(|k| layered[k] != pre[k]), "the layer covers the loop point");
    (pre, layered)
}

/// Lane `lane` committed from silence onto the master as a later take of `level` (a whole loop), PLAYING.
fn later_lane(rig: &mut Rig, lane: u8, level: f32) {
    rig.set_level(level);
    rig.press(Command::RecDub(lane));
    let end = rig.start_frame() + rig.master();
    rig.advance_to(end + 1);
    assert_eq!(rig.state(lane as usize), LaneState::Recording, "the take runs to its window end");
    rig.press(Command::RecDub(lane));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(lane as usize), LaneState::Playing);
}

/// The loop position frame `f` plays on a grid anchored at `anchor`.
fn pos_at(anchor: Frame, master: Frame, f: Frame) -> usize {
    (f - anchor).rem_euclid(master) as usize
}

/// The kept output over `[a, b)` is `want(frame)`.
fn heard(rig: &Rig, a: Frame, b: Frame, want: impl Fn(Frame) -> f32, tag: &str) {
    let (start, out) = rig.output.as_ref().unwrap();
    assert!(a >= *start && b <= start + out.len() as Frame, "{tag}: [{a}, {b}) lies in the kept output");
    for f in a..b {
        assert_eq!(out[(f - start) as usize], want(f), "{tag}: frame {f}");
    }
}

/// The frame the feed first reported lane `lane` in `state` at or after `from`.
fn reported(rig: &Rig, lane: u8, state: LaneState, from: Frame) -> Option<Frame> {
    rig.events.iter().find_map(|e| match *e {
        Event::Lane { frame, lane: l, info } if l == lane && info.state == state && frame >= from => Some(frame),
        _ => None,
    })
}

#[test]
fn an_undo_crossfades_on_its_boundary_in_heard_order() {
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    assert_eq!(n, 240);
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    let xf = |rig: &Rig, b: Frame, was: &[f32], want: &[f32], tag: &str| {
        heard(rig, b, b + n, |f| sample(1.0, join(f - b, n), want[p(f)], out(f - b, n) * was[p(f)] as f64), tag);
    };
    rig.advance_to(rig.next_boundary() + master / 3);
    rig.keep_output();
    // UNDO mid-loop, REDO pressed inside its crossfade, UNDO pressed on a boundary (it switches at the
    // press). Each crossfades from the loop heard to the one it gives back, from its switch.
    let u0 = rig.frame;
    let b0 = rig.next_boundary();
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre, "UNDO: the logical loop at once");
    rig.advance_to(b0 + n / 2);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), layered, "REDO");
    let b1 = b0 + master;
    let b2 = b1 + master;
    rig.advance_to(b2);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre, "UNDO again");
    rig.advance_to(b2 + n + 100);
    heard(&rig, u0, b0, |f| layered[p(f)], "up to the first boundary");
    xf(&rig, b0, &layered, &pre, "the undo's crossfade");
    heard(&rig, b0 + n, b1, |f| pre[p(f)], "the loop before the layer");
    xf(&rig, b1, &pre, &layered, "the redo's crossfade");
    heard(&rig, b1 + n, b2, |f| layered[p(f)], "the layer again");
    xf(&rig, b2, &layered, &pre, "the crossfade of an undo pressed on the boundary");
    heard(&rig, b2 + n, rig.frame, |f| pre[p(f)], "after it");

    // Two UNDOs before the boundary land on the loop heard: nothing crossfades there.
    rig.advance_to(rig.next_boundary() + master / 2);
    let b = rig.next_boundary();
    let from = rig.frame;
    rig.press(Command::Undo(0));
    rig.press(Command::Undo(0));
    rig.advance_to(b + 2 * n);
    assert_eq!(rig.pcm(0), pre);
    heard(&rig, from, rig.frame, |f| pre[p(f)], "a double toggle");
}

#[test]
fn an_undo_crossfades_a_reversed_loop_into_a_forward_one() {
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    let back = |f: Frame| master as usize - 1 - p(f);
    // REVERSE swaps on its boundary as ever (a cut); the UNDO after it crossfades the reversed layer into
    // the loop before it, forward: each read in heard order.
    rig.advance_to(rig.next_boundary() + master / 3);
    rig.keep_output();
    let from = rig.frame;
    let r = rig.next_boundary();
    rig.press(Command::Reverse(0));
    rig.advance_to(r + master / 3);
    let b = rig.next_boundary();
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == pre && !rig.lane(0).reversed);
    rig.advance_to(b + n + 100);
    heard(&rig, from, r, |f| layered[p(f)], "forward up to the reverse");
    heard(&rig, r, b, |f| layered[back(f)], "the reverse is a cut on its boundary");
    heard(&rig, b, b + n, |f| sample(1.0, join(f - b, n), pre[p(f)], out(f - b, n) * layered[back(f)] as f64), "reversed into forward");
    heard(&rig, b + n, rig.frame, |f| pre[p(f)], "after it");
}

#[test]
fn an_undo_a_dub_forces_crossfades_from_the_dubs_press_across_the_loop_point() {
    // An UNDO still pending when a DUB starts lands at the DUB (D19): the crossfade starts at its press
    // and runs across the loop point. The dub is of silence, so the loop it writes stays what it was.
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(rig.next_boundary() + master / 3);
    rig.keep_output();
    let from = rig.frame;
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre);
    let wrap = rig.next_boundary();
    rig.advance_to(wrap - n / 3);
    let d = rig.frame;
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.advance_to(wrap + n);
    rig.press(Command::RecDub(0));
    rig.advance(100);
    assert_eq!(rig.pcm(0), pre, "a dub of silence leaves the loop");
    heard(&rig, from, d, |f| layered[p(f)], "the layer, up to the dub");
    heard(&rig, d, d + n, |f| sample(1.0, join(f - d, n), pre[p(f)], out(f - d, n) * layered[p(f)] as f64), "the crossfade from the dub's press, across the loop point");
    heard(&rig, d + n, rig.frame, |f| pre[p(f)], "after it");
}

#[test]
fn a_stopped_lanes_undo_switches_nothing_heard_its_tail_plays_on() {
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    // Inside the layer: the loop heard and the one the undo gives back differ there.
    rig.advance_to(rig.next_boundary() + master / 8);
    rig.keep_output();
    let stop = rig.frame;
    assert!((stop..stop + n).all(|f| layered[pos_at(anchor, master, f)] != pre[pos_at(anchor, master, f)]));
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Stopped);
    rig.advance(n / 3);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre, "UNDO on the stopped lane at once");
    rig.advance(master / 2);
    heard(&rig, stop, stop + n, |f| sample(1.0, tail(f - stop, n), layered[pos_at(anchor, master, f)], 0.0), "the tail of the loop heard at the press");
    heard(&rig, stop + n, rig.frame, |_| 0.0, "then silence");
    // An idle PLAY with no tail sounding restarts from the top at once, the undone loop.
    let play = rig.frame;
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.anchor(), play);
    rig.advance(n + 100);
    heard(&rig, play, rig.frame, |f| pre[pos_at(play, master, f)], "an idle PLAY starts at once");
}

#[test]
fn an_undo_crossfade_outlives_a_trim_on_its_boundary_rewriting_its_loop() {
    // trim.rs f's gesture: an UNDO, and a TRIM held for its swap. On the boundary the undo switches (the
    // dubbed loop fades out), then the TRIM takes the buffer that was playing and writes the trimmed loop
    // into it from loop position 0, heard from the same frame (its swap is no crossfade).
    let (mut rig, master) = code_loop(128, 2);
    let (n, fpb) = (ramp(rig.sr), rig.fpb());
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(rig.next_boundary() + master / 5);
    rig.press(Command::Undo(0));
    rig.advance(master / 5);
    let b = rig.next_boundary();
    rig.keep_output();
    rig.press(Command::Trim(0, 1));
    assert!(rig.engine.holding(), "the trim waits for the undo's swap");
    rig.advance_to(b + n + 100);
    rig.idle();
    let trimmed: Vec<f32> = (0..master as usize).map(|k| pre[k % fpb as usize]).collect();
    assert_eq!(rig.pcm(0), trimmed, "the dubbed loop's buffer now holds the trimmed loop");
    assert_eq!(rig.engine.looper().undo_pcm(0), Some(pre.clone()));
    heard(&rig, b, b + n, |f| sample(1.0, join(f - b, n), trimmed[p(f)], out(f - b, n) * layered[p(f)] as f64), "the dubbed loop fades out as it was heard");
    heard(&rig, b + n, rig.frame, |f| trimmed[p(f)], "the trimmed loop");
}

#[test]
fn an_undo_crossfade_outlives_a_dub_taking_its_buffer() {
    // Lane 0's undo crossfades on a boundary; inside it lane 0 dubs one frame (its outgoing buffer becomes
    // the free one, kept as the previous undo target) and lane 1 starts a dub, whose undo copy takes that
    // free buffer and writes lane 1's loop (silence) over the outgoing loop from the frame it starts.
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    let (pre, layered) = dubbed(&mut rig);
    later_lane(&mut rig, 1, 0.0);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(rig.next_boundary() + master / 3);
    rig.press(Command::Undo(0));
    let b = rig.next_boundary();
    rig.advance_to(b - 100);
    rig.keep_output();
    rig.advance_to(b + 3);
    rig.press(Command::RecDub(0));
    rig.press(Command::RecDub(0));
    assert!(rig.state(0) == LaneState::Playing && rig.window().is_none(), "a one-frame layer commits");
    rig.press(Command::RecDub(1));
    assert_eq!(rig.state(1), LaneState::Overdubbing);
    rig.advance_to(b + n + 100);
    rig.press(Command::RecDub(1));
    rig.idle();
    assert_eq!(rig.pcm(0), pre, "the one-frame layer leaves the loop: its only write is the punch-in's first, the loop itself");
    assert!(rig.engine.looper().undo_pcm(1).unwrap().iter().all(|&x| x == 0.0), "lane 1's undo copy is its silent loop");
    heard(&rig, b - 100, b, |f| layered[p(f)], "before the boundary");
    heard(&rig, b, b + n, |f| sample(1.0, join(f - b, n), pre[p(f)], out(f - b, n) * layered[p(f)] as f64), "the dubbed loop fades out as it was heard");
}

/// Lane 0 PLAYING a one-bar loop of the frame code at 200 bpm and lane 1 a later take of `other` beside
/// it (the transport stays running while lane 0 stops), the input silent.
fn two_lanes(block: usize, other: f32) -> (Rig, Frame) {
    let (mut rig, master) = code_loop(block, 1);
    later_lane(&mut rig, 1, other);
    (rig, master)
}

#[test]
fn a_stop_and_a_play_ramp_from_the_press_and_leave_the_grid_the_state_and_the_events_where_they_were() {
    const OTHER: f32 = 0.25;
    let (mut rig, master) = two_lanes(128, OTHER);
    let n = ramp(rig.sr);
    let (anchor, pcm) = (rig.anchor(), rig.pcm(0));
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(rig.next_boundary() + master / 3);
    rig.keep_output();
    let mark = rig.events.len();
    let stop = rig.frame;
    rig.press(Command::PlayStop(0));
    assert_eq!((rig.state(0), rig.anchor()), (LaneState::Stopped, anchor), "STOPPED at the press, the grid where it was");
    rig.advance_to(stop + master / 4);
    let play = rig.frame;
    rig.press(Command::PlayStop(0));
    assert_eq!((rig.state(0), rig.anchor()), (LaneState::Playing, anchor), "PLAYING at the press, at the live phase");
    rig.advance_to(play + master / 4);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, stop), Some(stop));
    assert_eq!(reported(&rig, 0, LaneState::Playing, stop), Some(play));
    let lane0 = |f: Frame| {
        let x = pcm[p(f)];
        if f < stop + n {
            sample(1.0, tail(f - stop, n), x, 0.0)
        } else if f < play {
            0.0
        } else if f < play + n {
            sample(1.0, join(f - play, n), x, 0.0)
        } else {
            x
        }
    };
    heard(&rig, stop, rig.frame, |f| lane0(f) + OTHER, "lane 0's tail and its fade-in beside lane 1");
    // The beats stay on the master grid (one bar, four beats a loop).
    let grid = Grid::master(anchor, master, 1);
    let beats = rig.beats_since(mark);
    assert!(beats.len() >= 2 && beats.iter().all(|b| (b.0 - grid.beat_frame(0)) % (master / 4) == 0), "{beats:?}");

    // The only lane's STOP idles the transport at the press, its tail sounding: the click stops there.
    rig.press(Command::PlayStop(1));
    rig.advance(master / 3);
    rig.keep_output();
    let mark = rig.events.len();
    let stop = rig.frame;
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.engine.looper().transport_until(), None, "a tail is no transport");
    rig.advance(master);
    assert!(rig.beats_since(mark).iter().all(|b| !b.3), "no click from the press");
    heard(&rig, stop, stop + n, |f| sample(1.0, tail(f - stop, n), pcm[p(f)], 0.0), "the tail");
    heard(&rig, stop + n, rig.frame, |_| 0.0, "silence");
}

#[test]
fn a_reversal_inside_the_ramp_turns_from_the_level_reached() {
    // Beside a running lane (no restart): STOP then PLAY, and PLAY then STOP, at every distance from a
    // frame to past the ramp.
    const OTHER: f32 = 0.25;
    let n = 240;
    for d in [1, 2, 3, n / 2, n - 1, n, n + 7] {
        let (mut rig, master) = two_lanes(128, OTHER);
        assert_eq!(ramp(rig.sr), n);
        let (anchor, pcm) = (rig.anchor(), rig.pcm(0));
        let p = |f: Frame| pos_at(anchor, master, f);
        rig.advance_to(rig.next_boundary() + master / 3);
        rig.keep_output();
        let stop = rig.frame;
        rig.press(Command::PlayStop(0));
        rig.advance_to(stop + d);
        rig.press(Command::PlayStop(0));
        let turn = level(1.0, 0.0, d, n);
        rig.advance_to(stop + d + n + 50);
        let lane0 = |f: Frame| {
            let x = pcm[p(f)];
            if f < stop + d {
                sample(1.0, tail(f - stop, n), x, 0.0)
            } else {
                sample(1.0, level(turn, 1.0, f - stop - d, n), x, 0.0)
            }
        };
        heard(&rig, stop, rig.frame, |f| lane0(f) + OTHER, &format!("STOP then PLAY {d} frames later"));

        let play = rig.frame + master / 5;
        rig.advance_to(play);
        rig.press(Command::PlayStop(0)); // STOP
        rig.advance(master / 5);
        let play = rig.frame;
        rig.press(Command::PlayStop(0));
        rig.advance_to(play + d);
        rig.press(Command::PlayStop(0));
        let turn = level(0.0, 1.0, d, n);
        rig.advance_to(play + d + n + 50);
        let lane0 = |f: Frame| {
            let x = pcm[p(f)];
            if f < play + d {
                sample(1.0, join(f - play, n), x, 0.0)
            } else if f < play + d + n {
                sample(1.0, level(turn, 0.0, f - play - d, n), x, 0.0)
            } else {
                0.0
            }
        };
        heard(&rig, play, rig.frame, |f| lane0(f) + OTHER, &format!("PLAY then STOP {d} frames later"));
    }
}

#[test]
fn an_idle_play_during_a_tail_crossfades_from_it_to_the_top() {
    // The only lane stops, and PLAY comes while its tail sounds: the transport is idle, so the grid
    // restarts at the press (as ever) and the lane plays from the top, fading in, while the tail plays
    // out at the phase it had. Past the tail, an idle PLAY starts at once.
    let n = 240;
    for d in [1, 2, n / 3, n - 1, n] {
        let (mut rig, master) = code_loop(128, 1);
        let (old, pcm) = (rig.anchor(), rig.pcm(0));
        rig.advance_to(rig.next_boundary() + master / 3);
        rig.keep_output();
        let stop = rig.frame;
        rig.press(Command::PlayStop(0));
        rig.advance_to(stop + d);
        let play = rig.frame;
        rig.press(Command::PlayStop(0));
        assert_eq!(rig.anchor(), play, "d={d}: an idle PLAY re-anchors at the press");
        rig.advance_to(play + n + 50);
        let lane0 = |f: Frame| {
            let x = pcm[pos_at(play, master, f)];
            if f < play {
                return sample(1.0, tail(f - stop, n), pcm[pos_at(old, master, f)], 0.0);
            }
            if d >= n {
                return x;
            }
            let outgoing = if f < stop + n { tail(f - stop, n) * pcm[pos_at(old, master, f)] as f64 } else { 0.0 };
            sample(1.0, join(f - play, n), x, outgoing)
        };
        heard(&rig, stop, rig.frame, lane0, &format!("d={d}"));
    }
}

/// Lane 0's loop as an overdub of `input` from input frame `start` has left it by input frame `end`, its
/// window still open: each write its punch-in weight, none its punch-out (`common::dub`).
fn dub_in_flight(pre: &[f32], (start, end): (Frame, Frame), n: Frame, pos: impl Fn(Frame) -> usize, input: impl Fn(Frame) -> f32) -> Vec<f32> {
    let mut pcm = pre.to_vec();
    for f in start..end {
        let p = pos(f);
        pcm[p] = common::dub::write(pcm[p], input(f), 1.0, f - start, n, n);
    }
    pcm
}

#[test]
fn a_stop_during_a_dub_fades_what_was_heard_and_restores_the_loop_bit_for_bit() {
    // A layer over more than a pass: from the press the restore copies the loop before it back from
    // the read head on, so what the tail plays (the layered loop as heard) is cached first.
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    // An earlier layer: the undo target the discard must hand back is the loop before that one.
    let (older, pre) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() + master / 3);
    let punch = rig.frame;
    rig.press(Command::RecDub(0));
    rig.advance(master + master / 2);
    rig.set_level(0.0);
    rig.keep_output();
    let stop = rig.frame;
    let layered = dub_in_flight(&pre, (punch, stop), n, p, |_| DUB);
    assert!((stop..stop + n).any(|f| layered[p(f)] != pre[p(f)]), "the tail's positions hold the layer");
    rig.press(Command::Stop(0));
    assert_eq!(rig.state(0), LaneState::Stopped);
    rig.advance(master / 2);
    rig.idle();
    assert_eq!(rig.pcm(0), pre, "the layer is discarded: the loop before it, bit for bit");
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), older, "and the undo target from before the layer");
    heard(&rig, stop, stop + n, |f| sample(1.0, 0.0, 0.0, tail(f - stop, n) * layered[p(f)] as f64), "the layered loop fades out as heard");
    heard(&rig, stop + n, rig.frame, |_| 0.0, "then silence");
}

#[test]
fn stop_all_tails_five_lanes_on_its_frame_and_keeps_a_pending_copy_stopped() {
    // Lanes 0 to 3 PLAYING (1 to 3 copies of 0, at volumes of their own), lane 3 overdubbing with an
    // alignment, a COPY still running into lane 4. STOP ALL: every lane fades from the press, the dub
    // captures on to its aligned end (no second tail there) and the copy lands STOPPED.
    const ALIGN: Frame = 4800;
    let (mut rig, master) = code_loop(128, 1);
    let n = ramp(rig.sr);
    for (lane, volume) in [(1u8, 0.5f32), (2, 0.25), (3, 1.0)] {
        rig.press(Command::Copy(0));
        rig.idle();
        rig.set(Command::SetVolume(lane, volume));
        assert_eq!(rig.state(lane as usize), LaneState::Playing);
    }
    rig.advance(rig.seconds(0.5)); // every volume glide settled
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    let pre = rig.pcm(0);
    rig.align = ALIGN;
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() + master / 4);
    let punch = rig.frame;
    rig.press(Command::RecDub(3));
    rig.advance(master / 3);
    // What was played before the press arrives until ALIGN after it; nothing after.
    let press = rig.frame + 1;
    rig.set_input(move |f| if f < press + ALIGN { DUB } else { 0.0 });
    rig.press(Command::Copy(0));
    assert!(rig.state(4) == LaneState::Stopped && rig.engine.looper().busy(), "the copy into lane 4 runs");
    rig.keep_output();
    assert_eq!(rig.frame, press);
    rig.press(Command::StopAll);
    for lane in 0..4 {
        assert_eq!(rig.state(lane), if lane == 3 { LaneState::Overdubbing } else { LaneState::Stopped }, "lane {lane}");
    }
    rig.advance(master);
    rig.idle();
    assert!((0..5).all(|i| rig.state(i) == LaneState::Stopped), "every lane STOPPED, the copy too");
    for lane in 0..3u8 {
        assert_eq!(reported(&rig, lane, LaneState::Stopped, press), Some(press), "lane {lane} at the press");
    }
    assert_eq!(reported(&rig, 3, LaneState::Stopped, press), Some(press + ALIGN), "the dub once its aligned tail is in");
    assert_eq!(reported(&rig, 4, LaneState::Playing, press), None, "the copy never resumes");
    let mut layer = pre.clone();
    common::dub::dub(&mut layer, (punch + ALIGN, press + ALIGN), n, 1.0, common::dub::pos_fn(anchor, master, ALIGN), |_| DUB);
    assert_eq!(rig.pcm(3), layer, "the layer through the press, ramped");
    // The layer is written behind the read head: the dub's tail plays the loop as it was there.
    let volumes = [1.0, 0.5, 0.25, 1.0];
    heard(
        &rig,
        press,
        rig.frame,
        |f| {
            let mut sum = 0.0f32;
            for v in volumes {
                sum += if f < press + n { sample(v, tail(f - press, n), pre[p(f)], 0.0) } else { 0.0 };
            }
            sum + rig.monitor[(f - press) as usize]
        },
        "four tails from the press, one frame",
    );
}

/// `scenario` at every block size of `BLOCKS`: what it keeps is bit-identical to block 128's.
fn at_every_block_size(tag: &str, scenario: impl Fn(usize) -> Vec<f32>) {
    let reference = scenario(128);
    for block in BLOCKS {
        let out = scenario(block);
        assert_eq!(out.len(), reference.len(), "{tag}: block {block}");
        let first = out.iter().zip(&reference).position(|(a, b)| a.to_bits() != b.to_bits());
        assert_eq!(first, None, "{tag}: block {block} differs from block 128");
    }
}

/// Lane 1 a later take of 0.25, then STOPPED: a lane to PLAY on an idle transport.
fn stopped_beside(rig: &mut Rig) {
    later_lane(rig, 1, 0.25);
    rig.press(Command::PlayStop(1));
    rig.advance(rig.seconds(0.1));
}

/// The commands at `frame` in order, rendering from there in the rig's blocks: they land mid-block.
fn at(rig: &mut Rig, frame: Frame, commands: &[Command]) {
    rig.advance_to(frame - 333);
    for &c in commands {
        rig.send_at(frame, c);
    }
}

// A tail cached beside a running block job: each is cached on the frame the job starts writing the loop
// it plays (an idle PLAY of the other lane restarts the grid there), so its samples ahead of the job come
// from the job's source, at any block size.

#[test]
fn a_tail_cached_as_a_restore_starts_plays_the_restored_loop() {
    // Lane 0's layer, over a pass and damaged by a gap, is rejected at its close; the restore copies the
    // loop before it back from the read head on. Lane 0 STOPs on that frame.
    at_every_block_size("restore", |block| {
        let (mut rig, master) = code_loop(block, 1);
        let n = ramp(rig.sr);
        stopped_beside(&mut rig);
        let (anchor, pre) = (rig.anchor(), rig.pcm(0));
        rig.set_level(DUB);
        rig.advance_to(rig.next_boundary() + master / 3);
        rig.press(Command::RecDub(0));
        rig.advance(master + master / 10);
        rig.gap();
        rig.advance(1000);
        rig.set_level(0.0);
        let e = rig.frame + 1000;
        at(&mut rig, e, &[Command::RecDub(0), Command::Stop(0), Command::PlayStop(1)]);
        let live = rig.engine.looper().live_buffer(0);
        assert!((e..e + n).all(|f| live[pos_at(anchor, master, f)] != pre[pos_at(anchor, master, f)]), "the layer lies ahead of the read head");
        rig.keep_output();
        rig.advance_to(e + n + 100);
        assert_eq!(rig.rejected(), 1);
        assert_eq!(rig.anchor(), e, "the idle PLAY restarts the grid");
        heard(&rig, e, rig.frame, |f| sample(1.0, 0.0, 0.0, if f < e + n { tail(f - e, n) * pre[pos_at(anchor, master, f)] as f64 } else { 0.0 }) + 0.25, "the tail is the restored loop");
        rig.idle();
        assert_eq!(rig.pcm(0), pre);
        rig.output.take().unwrap().1
    });

}

#[test]
fn a_tail_cached_as_a_fill_starts_plays_the_tiled_take() {
    // Lane 1's later take of one bar over a two-bar master commits, and the fill tiles its second bar
    // from the read head on, over what an earlier loop left in the buffer. Lane 1 STOPs on that frame.
    at_every_block_size("fill", |block| {
        let (mut rig, master) = code_loop(block, 2);
        let (n, fpb) = (ramp(rig.sr), rig.fpb());
        rig.press(Command::Copy(0));
        rig.idle();
        rig.press(Command::Clear(1));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        // Armed beside the playing loop, which then stops: the take keeps the grid's next boundary (armed
        // on an idle transport it would count in and restart the loops instead).
        rig.press(Command::RecDub(1));
        rig.press(Command::PlayStop(0));
        let c = rig.start_frame() + fpb;
        rig.set_input(move |f| if f < c { -code(f) } else { 0.0 });
        let anchor = rig.anchor();
        at(&mut rig, c, &[Command::Stop(1), Command::PlayStop(0)]);
        rig.keep_output();
        rig.advance_to(c + n + 100);
        assert_eq!(rig.anchor(), c, "the idle PLAY restarts the grid");
        // The buffer's second bar held lane 0's copy until the fill.
        let (take, pcm0) = (rig.pcm(1), rig.pcm(0));
        assert!((c..c + n).all(|f| take[pos_at(anchor, master, f)] != pcm0[pos_at(anchor, master, f)]), "the fill writes over another loop");
        heard(&rig, c, rig.frame, |f| pcm0[pos_at(c, master, f)] + sample(1.0, 0.0, 0.0, if f < c + n { tail(f - c, n) * take[pos_at(anchor, master, f)] as f64 } else { 0.0 }), "the tail is the tiled take");
        rig.output.take().unwrap().1
    });

}

#[test]
fn a_tail_cached_as_a_trim_starts_plays_the_trimmed_loop() {
    // A TRIM pressed on lane 0's boundary swaps in a buffer never written and writes the trimmed loop into
    // it from loop position 0, heard from that frame. Lane 0 STOPs on it.
    at_every_block_size("trim", |block| {
        let (mut rig, master) = code_loop(block, 2);
        let (n, fpb) = (ramp(rig.sr), rig.fpb());
        stopped_beside(&mut rig);
        let (anchor, before) = (rig.anchor(), rig.pcm(0));
        let b = rig.next_boundary() + master;
        at(&mut rig, b, &[Command::Trim(0, 1), Command::Stop(0), Command::PlayStop(1)]);
        rig.keep_output();
        rig.advance_to(b + n + 100);
        assert_eq!(rig.anchor(), b);
        rig.idle();
        let trimmed: Vec<f32> = (0..master as usize).map(|k| before[k % fpb as usize]).collect();
        assert_eq!(rig.pcm(0), trimmed);
        heard(&rig, b, b + n + 100, |f| sample(1.0, 0.0, 0.0, if f < b + n { tail(f - b, n) * trimmed[pos_at(anchor, master, f)] as f64 } else { 0.0 }) + 0.25, "the tail is the trimmed loop");
        rig.output.take().unwrap().1
    });
}

#[test]
fn a_device_jump_inside_an_edge_plays_on_at_the_frame_it_lands_on() {
    // Every edge runs on the absolute frame: a jump into a tail resumes it where the frame says, and a
    // jump past a fade-in lands at full level. Nothing catches up on the frames skipped.
    at_every_block_size("jumps", |block| {
        const OTHER: f32 = 0.25;
        let (mut rig, master) = two_lanes(block, OTHER);
        let n = ramp(rig.sr);
        let (anchor, pcm) = (rig.anchor(), rig.pcm(0));
        let p = |f: Frame| pos_at(anchor, master, f);
        let mut kept = Vec::new();
        rig.advance_to(rig.next_boundary() + master / 3);
        let stop = rig.frame;
        rig.keep_output();
        rig.press(Command::PlayStop(0));
        rig.advance(n / 4 - 1);
        heard(&rig, stop, stop + n / 4, |f| sample(1.0, tail(f - stop, n), pcm[p(f)], 0.0) + OTHER, "the tail");
        kept.extend(rig.output.take().unwrap().1);
        rig.skip(n / 3);
        let resume = rig.frame;
        rig.keep_output();
        rig.advance(n);
        heard(&rig, resume, resume + n, |f| (if f < stop + n { sample(1.0, tail(f - stop, n), pcm[p(f)], 0.0) } else { 0.0 }) + OTHER, "the tail on the frame it lands on");
        kept.extend(rig.output.take().unwrap().1);
        let play = rig.frame;
        rig.press(Command::PlayStop(0));
        rig.skip(2 * n);
        let resume = rig.frame;
        rig.keep_output();
        rig.advance(100);
        assert!(resume > play + n);
        heard(&rig, resume, rig.frame, |f| pcm[p(f)] + OTHER, "past the fade-in: full level");
        kept.extend(rig.output.take().unwrap().1);
        kept
    });
}

#[test]
fn every_edge_renders_bit_identical_at_any_block_size() {
    // One script of every new edge, its commands stamped to land mid-block: an undo's crossfade, a STOP
    // and PLAYs turning inside their ramps, a dub forcing a pending undo, a STOP discarding a dub, STOP
    // ALL and an idle PLAY ALL over the tails, a FADE stopped early, a COPY resuming into the loop.
    use lf_engine::Action;
    let script = |block: usize| {
        let (mut rig, master) = code_loop(block, 1);
        let n = ramp(rig.sr);
        dubbed(&mut rig);
        rig.press(Command::Copy(0));
        rig.idle();
        rig.set(Command::SetVolume(1, 0.5));
        let t = rig.next_boundary() + master;
        rig.advance_to(t - 1000);
        rig.keep_output();
        rig.set_level(DUB);
        let commands = [
            (t + 1000, Command::Undo(0)),
            (t + master + 77, Command::Stop(1)),
            (t + master + 77 + n / 3, Command::PlayStop(1)),
            (t + master + 400, Command::PlayStop(1)),
            (t + master + 405, Command::PlayStop(1)),
            (t + 2 * master - 500, Command::Undo(0)),
            (t + 2 * master - n / 2, Command::RecDub(0)),
            (t + 3 * master + 13, Command::Stop(0)),
            (t + 3 * master + 2000, Command::PlayStop(0)),
            (t + 4 * master + 100, Command::StopAll),
            (t + 4 * master + 100 + n / 2, Command::PlayAll),
            (t + 4 * master + 5000, Command::Action(Action::FadeAll)),
            (t + 4 * master + 9000, Command::Action(Action::FadeAll)),
            (t + 4 * master + 20000, Command::PlayAll),
            (t + 4 * master + 30000, Command::Copy(1)),
        ];
        for (f, c) in commands {
            rig.send_at(f, c);
        }
        rig.advance_to(t + 5 * master);
        assert!(rig.rejected() == 0 && (0..3).all(|i| rig.state(i) == LaneState::Playing), "the script ends with three lanes playing");
        [rig.output.take().unwrap().1, std::mem::take(&mut rig.heard)].concat()
    };
    at_every_block_size("the script", script);
}

#[test]
fn a_five_lane_burst_of_edges_allocates_nothing() {
    // Five lanes, each with a TRIM to undo, UNDO on all five crossfading on one boundary, then STOP ALL
    // and PLAY ALL inside the tails (an idle restart caching all five), in one 1024-frame block. The rig
    // renders every block under `assert_no_alloc`; this counts the burst's block explicitly.
    let (mut rig, master) = code_loop(1024, 2);
    let n = ramp(rig.sr);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    for lane in 0..5u8 {
        rig.press(Command::Trim(lane, 1));
        rig.idle();
    }
    rig.advance_to(rig.next_boundary() + master / 3);
    for lane in 0..5u8 {
        rig.press(Command::Undo(lane));
    }
    let b = rig.next_boundary();
    rig.send_at(b + n / 2, Command::StopAll);
    rig.send_at(b + n / 2 + 10, Command::PlayAll);
    rig.advance_to(b - 300);
    let before = common::violation_count();
    rig.advance(1024);
    assert_eq!(common::violation_count(), before, "the burst allocated");
    assert!((0..5).all(|i| rig.state(i) == LaneState::Playing) && rig.anchor() == b + n / 2 + 10);
}

#[test]
fn a_scheduled_stop_on_an_undos_switch_is_silent_from_its_frame() {
    // An UNDO switching on the boundary a scheduled stop lands on (END STOP's, then a FADE's end): the
    // stop retires every edge on that frame, the undo's outgoing loop too, so the lane is silent from it.
    let (mut rig, master) = code_loop(128, 2);
    let (n, fpb) = (ramp(rig.sr), rig.fpb());
    let (pre, layered) = dubbed(&mut rig);
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(rig.next_boundary() + fpb / 3);
    rig.keep_output();
    let from = rig.frame;
    let b = rig.next_boundary();
    rig.press(Command::Undo(0));
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Playing, "END STOP waits for the boundary");
    rig.advance_to(b + n + 100);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, from), Some(b));
    assert_eq!(rig.pcm(0), pre, "the undo took");
    heard(&rig, from, b, |f| layered[p(f)], "END STOP: the loop heard up to the boundary");
    heard(&rig, b, rig.frame, |_| 0.0, "END STOP: silent from the boundary");

    // FADE: an UNDO (the redo) and a one-bar FADE pressed in the loop's first bar both land on its end.
    rig.set(Command::SetLoopEndStop(false));
    rig.set(Command::SetFadeBars(1));
    rig.press(Command::PlayStop(0));
    let anchor = rig.anchor();
    let p = |f: Frame| pos_at(anchor, master, f);
    rig.advance_to(anchor + fpb / 3);
    rig.keep_output();
    let from = rig.frame;
    let b = anchor + master;
    rig.press(Command::Undo(0));
    let press = rig.frame;
    rig.press(Command::Action(lf_engine::Action::FadeAll));
    rig.advance_to(b + n + 100);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, from), Some(b), "the FADE ends on the loop boundary");
    assert_eq!(rig.pcm(0), layered, "the redo took");
    let faded = |f: Frame| {
        let r = ((b - f) as f64 * (1.0 / (b - press) as f64)).clamp(0.0, 1.0);
        (r * r * pre[p(f)] as f64) as f32
    };
    heard(&rig, from, press, |f| pre[p(f)], "FADE: the loop heard up to the press");
    heard(&rig, press, b, faded, "FADE: the loop heard, faded");
    heard(&rig, b, rig.frame, |_| 0.0, "FADE: silent from its end");
}

#[test]
fn play_stop_closing_a_dub_caches_its_tail_before_the_capture_running_on_reaches_it() {
    // An accepted alignment of a loop less 100 frames: from the press PLAY/STOP closing a dub fades the
    // lane out while the capture writes 100 positions ahead of the read head until its aligned end. The
    // tail plays the loop as it stood at the press, at any block size.
    const ALIGN: Frame = 38_400 - 100;
    let n = ramp(48000);
    let mut kept = Vec::new();
    for block in [1, 128] {
        let mut rig = Rig::with(Opts { sr: 48000, start: 48000, align: ALIGN, ..Default::default() });
        rig.set(Command::SetBpm(300.0));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.set_input(code);
        rig.press(Command::RecDub(0));
        rig.advance_to(rig.end_frame() + 1);
        rig.set(Command::SetFixedLength(false));
        rig.set_level(0.0);
        rig.idle();
        assert_eq!(rig.state(0), LaneState::Playing, "block {block}: the first take committed");
        let master = rig.master();
        assert_eq!(master, 38_400, "one bar at 300 BPM");
        let anchor = rig.anchor();
        let p = |f: Frame| pos_at(anchor, master, f);
        rig.set(Command::SetDubFeedback(0, 0.0));
        rig.set_level(0.5);
        rig.advance_to(rig.next_boundary() + master / 4);
        let d = rig.frame;
        rig.press(Command::RecDub(0));
        let t = d + ALIGN + master / 2;
        rig.advance_to(t - 1000);
        rig.block = block;
        rig.advance_to(t);
        assert_eq!(rig.state(0), LaneState::Overdubbing);
        let before = rig.engine.looper().live_buffer(0)[..master as usize].to_vec();
        assert!((t..t + n).all(|f| before[p(f)] != -0.5), "block {block}: no position the tail reads holds what the capture writes after the press");
        rig.set_level(-0.5);
        rig.keep_output();
        rig.press(Command::PlayStop(0));
        rig.advance_to(t + n + 100);
        let start = rig.output.as_ref().unwrap().0;
        let monitor = rig.monitor.clone();
        heard(&rig, t, t + n, |f| sample(1.0, tail(f - t, n), before[p(f)], 0.0) + monitor[(f - start) as usize], &format!("block {block}: the tail is the loop at the press"));
        heard(&rig, t + n, rig.frame, |f| monitor[(f - start) as usize], &format!("block {block}: then the monitor alone"));
        kept.push(rig.output.take().unwrap().1);
    }
    assert_eq!(kept[0], kept[1], "block 1 and block 128 hear the same");
}

#[test]
fn an_undo_forced_back_inside_its_crossfade_keeps_identical_loops_at_unity() {
    // The loop and its undo target hold the same samples (a silent dub over a constant take). An UNDO
    // crossfades on its boundary; 60 frames in, UNDO and a DUB force the redo's switch (D19), a second
    // crossfade over the first. Every overlap fades out all it heard: the lane plays the level throughout.
    const LEVEL: f32 = 0.25;
    let mut rig = rig_at(128);
    rig.set_level(LEVEL);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig.advance_to(rig.next_boundary() + master / 4);
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.press(Command::RecDub(0));
    rig.idle();
    assert!(rig.pcm(0).iter().all(|&x| x == LEVEL), "the silent dub leaves the loop");
    assert!(rig.engine.looper().undo_pcm(0).unwrap().iter().all(|&x| x == LEVEL), "the undo target is the same loop");
    rig.advance_to(rig.next_boundary() + master / 3);
    let b = rig.next_boundary();
    rig.press(Command::Undo(0));
    rig.advance_to(b - 100);
    rig.keep_output();
    rig.send_at(b + 60, Command::Undo(0));
    rig.send_at(b + 60, Command::RecDub(0));
    rig.advance_to(b + 60 + 2 * ramp(rig.sr));
    assert_eq!(rig.state(0), LaneState::Overdubbing, "the DUB forced the redo's switch");
    heard(&rig, b - 100, rig.frame, |_| LEVEL, "the level through both crossfades");
}
