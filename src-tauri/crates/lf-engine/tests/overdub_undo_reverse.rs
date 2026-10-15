//! Ports verify/guards/overdub.mjs, undo.mjs and reverse.mjs: the overdub layer, the one-level UNDO/REDO
//! toggle and per-lane REVERSE.
//!
//! The engine sums an overdub in place, so there is no working copy, no boundary swap
//! and no swap timer. overdub.mjs's timer checks (one pending swap per lane, early fire, a stalled
//! re-arm, a swap that throws) guard machinery that no longer exists and are not ported; what they
//! protected is asserted on the loop and on the rendered output: every captured frame summed exactly
//! once, heard on the grid. undo.mjs's "summing leaves the loop alone until the boundary" becomes "the
//! undo target keeps the pre-session loop while the layer sums in". reverse.mjs A's peak check is
//! `tests/peaks.rs`. Two overdub rows of the Web Audio probes are here on the rendered output too:
//! capture-loss.mjs's damaged layer ended by PLAY/STOP, and overdub-window.mjs's aligned punch-outs.
//! A layer's first and last 5 ms are ramped in the stored loop (D23): a committed layer is held to
//! `common::dub`'s reference over its exact window.

mod common;

use common::dub::{pos_fn, ramp};
use common::edges::{join, out, sample, tail};
use common::{code, Rig};
use lf_engine::api::damage;
use lf_engine::grid::Frame;
use lf_engine::looper::{job_frames, JOB_RATE};
use lf_engine::{Action, Command, Event, LaneState, Refusal};

const DUB: f32 = 1.0 / 64.0;

/// A committed one-bar loop at 200 bpm whose take is the frame code; then silence.
fn playing_loop() -> (Rig, Frame) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    (rig, master)
}

fn layer_sum(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) as f64).sum()
}

/// From `from`, the kept output equals `loop_at(position)`.
fn plays(rig: &Rig, from: Frame, to: Frame, loop_at: impl Fn(usize) -> f32) {
    let (start, out) = rig.output.as_ref().unwrap();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in from.max(*start)..to.min(start + out.len() as Frame) {
        let pos = (f - anchor).rem_euclid(master) as usize;
        assert_eq!(out[(f - start) as usize], loop_at(pos), "frame {f} pos {pos}");
    }
}

/// DUB from mid-period, across `boundaries` boundaries, then a quarter period more.
fn overdub_session(rig: &mut Rig, master: Frame, boundaries: usize, input: f32) {
    rig.set_level(input);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    for _ in 0..boundaries {
        rig.advance_to(rig.next_boundary() + 480);
    }
    rig.advance(master / 4);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(960);
}

#[test]
fn overdub_every_captured_frame_sums_once_across_end_start_cycles() {
    for cycles in [0, 1, 2, 4] {
        let (mut rig, master) = playing_loop();
        let pre = rig.pcm(0);
        rig.set_level(DUB);
        rig.advance_to(rig.next_boundary() + 2400);
        // Each press's frame: a start opens the window there (the rig's alignment is 0), a stop closes it.
        let mut presses = Vec::new();
        for tap in 0..(1 + 2 * cycles) {
            presses.push(rig.frame);
            rig.press(Command::RecDub(0));
            let want = if tap % 2 == 0 { LaneState::Overdubbing } else { LaneState::Playing };
            assert_eq!(rig.state(0), want);
            rig.advance(959);
        }
        rig.advance_to(rig.next_boundary() + 5 * master);
        presses.push(rig.frame);
        rig.press(Command::RecDub(0));
        rig.set_level(0.0);
        rig.advance(10);
        assert_eq!(rig.state(0), LaneState::Playing);
        // Every captured frame written once, in capture order, each gesture ramped at its own edges.
        let mut want = pre.clone();
        let pos = pos_fn(rig.anchor(), master, 0);
        for w in presses.chunks(2) {
            common::dub::dub(&mut want, (w[0], w[1]), ramp(rig.sr), 1.0, &pos, |_| DUB);
        }
        assert_eq!(rig.pcm(0), want, "cycles={cycles}: every captured frame, once per pass");
        rig.keep_output();
        let from = rig.frame;
        rig.advance(master + 100);
        let now = rig.pcm(0);
        plays(&rig, from, rig.frame, |p| now[p]);
    }
}

#[test]
fn overdub_play_stop_commits_the_layer_through_the_press_and_lands_stopped() {
    for align in [0, 4800] {
        let (mut rig, master) = playing_loop();
        rig.align = align;
        rig.advance_to(rig.next_boundary() + 2400);
        let pre = rig.pcm(0);
        rig.set_level(DUB);
        rig.press(Command::RecDub(0));
        let punch = rig.frame - 1;
        rig.advance_to(rig.next_boundary() - 1440);
        rig.press(Command::PlayStop(0));
        let stop = rig.frame - 1;
        let tail = stop + 1 + align;
        let anchor = rig.anchor();
        rig.set_input(move |f| if f < tail { DUB } else { 0.0 });
        rig.keep_output();
        let from = rig.frame;
        rig.advance(3 * master);
        assert_eq!(rig.state(0), LaneState::Stopped);
        let out = &rig.output.as_ref().unwrap().1;
        let monitor = |f: Frame| if f < tail { DUB } else { 0.0 };
        // From the press the lane fades out over 5 ms (D23) as the layer's tail comes in: it plays the
        // loop ahead of the read head, which the layer (behind it) never reached.
        let n = ramp(rig.sr);
        let lane = |f: Frame| if f < stop + n { sample(1.0, self::tail(f - stop, n), pre[(f - anchor).rem_euclid(master) as usize], 0.0) } else { 0.0 };
        for (k, &y) in out.iter().enumerate() {
            let f = from + k as Frame;
            assert_eq!(y, lane(f) + monitor(f), "align={align}: frame {f}: the lane's tail, then only the monitor");
        }
        let mut want = pre.clone();
        common::dub::dub(&mut want, (punch + align, stop + align), ramp(rig.sr), 1.0, pos_fn(anchor, master, align), |_| DUB);
        assert_eq!(rig.pcm(0), want, "align={align}: the layer through the press");
    }
}

#[test]
fn undo_a_the_snapshot_the_session_and_the_toggle() {
    let (mut rig, master) = playing_loop();
    assert!(!rig.lane(0).can_undo && rig.engine.looper().undo_pcm(0).is_none());
    let pre = rig.pcm(0);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    let punch = rig.frame;
    rig.press(Command::RecDub(0));
    rig.advance(master / JOB_RATE + 1);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "DUB snapshots the pre-dub loop");
    assert!(!rig.lane(0).can_undo, "no undo while OVERDUBBING");
    rig.advance_to(rig.next_boundary() + 480);
    assert_ne!(rig.pcm(0), pre, "the layer sums in place");
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), pre, "the undo target keeps the pre-session loop");
    rig.advance_to(rig.next_boundary() - master / 2);
    let out = rig.frame;
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(480);
    let dubbed = rig.pcm(0);
    assert_eq!(out - punch, master, "one period of layer");
    let mut want = pre.clone();
    common::dub::dub(&mut want, (punch, out), ramp(rig.sr), 1.0, pos_fn(rig.anchor(), master, 0), |_| DUB);
    assert_eq!(dubbed, want, "one period of layer, ramped at its edges");
    assert!(rig.lane(0).can_undo);
    rig.advance_to(rig.next_boundary() + 480);
    rig.keep_output();
    for (want, label) in [(&pre, "undo"), (&dubbed, "redo"), (&pre, "undo again")] {
        let boundary = rig.next_boundary();
        let before = rig.pcm(0);
        let from = rig.frame;
        rig.press(Command::Undo(0));
        assert_eq!(&rig.pcm(0), want, "{label} sets the loop");
        assert!(rig.lane(0).length == master && rig.master() == master && rig.lane(0).can_undo);
        rig.advance_to(boundary + 480);
        plays(&rig, from, boundary, |p| before[p]);
        // On the boundary (loop position 0) the loop heard crossfades into the one the toggle set over 5 ms
        // (D23): loop position `p` is `p` frames into it.
        let n = ramp(rig.sr);
        plays(&rig, boundary, boundary + n, |p| sample(1.0, join(p as Frame, n), want[p], self::out(p as Frame, n) * before[p] as f64));
        plays(&rig, boundary + n, rig.frame, |p| want[p]);
    }
}

#[test]
fn undo_b_a_fresh_session_re_baselines_the_snapshot() {
    let (mut rig, master) = playing_loop();
    overdub_session(&mut rig, master, 0, DUB);
    let first = rig.pcm(0);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() + master / 4);
    rig.press(Command::RecDub(0));
    rig.advance(master / 4);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), first);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    assert_ne!(rig.pcm(0), first);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), first, "undo restores the prior committed loop, not the take");
}

#[test]
fn undo_c_no_op_without_a_snapshot_while_overdubbing_or_stopping() {
    let (mut rig, master) = playing_loop();
    let take = rig.pcm(0);
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), take);
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.next_boundary() + 480);
    let mid = rig.pcm(0);
    let snapshot = rig.engine.looper().undo_pcm(0);
    rig.press(Command::Undo(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    assert!(rig.engine.looper().undo_pcm(0) == snapshot && layer_sum(&rig.pcm(0), &mid) == DUB as f64);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    let dubbed = rig.pcm(0);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert!(rig.lane(0).stop_at.is_some());
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), dubbed, "a pending loop-end stop blocks undo");
}

#[test]
fn undo_d_stopped_undoes_in_place_play_plays_it_clear_drops_it() {
    let (mut rig, master) = playing_loop();
    let pre = rig.pcm(0);
    overdub_session(&mut rig, master, 1, DUB);
    let (dubbed, stop, anchor) = (rig.pcm(0), rig.frame, rig.anchor());
    rig.press(Command::PlayStop(0));
    assert!(rig.state(0) == LaneState::Stopped && rig.lane(0).can_undo);
    rig.keep_output();
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre);
    rig.advance(1000);
    // Only the STOP's 5 ms tail (D23), of the loop heard at the press: the undo starts no playback.
    let n = ramp(rig.sr);
    for (k, &y) in rig.output.as_ref().unwrap().1.iter().enumerate() {
        let f = stop + 1 + k as Frame;
        let want = if f < stop + n { sample(1.0, tail(f - stop, n), dubbed[(f - anchor).rem_euclid(master) as usize], 0.0) } else { 0.0 };
        assert_eq!(y, want, "no playback from a STOPPED undo: frame {f}");
    }
    rig.press(Command::PlayStop(0));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master);
    plays(&rig, from, rig.frame, |p| pre[p]);
    rig.press(Command::Clear(0));
    assert!(!rig.lane(0).can_undo && rig.engine.looper().undo_pcm(0).is_none());
    rig.set_input(code);
    assert_eq!(rig.record_first_take(0, 3, 2400), 3 * master, "the lane keeps its full capacity");
}

#[test]
fn undo_e_a_layer_with_an_input_gap_is_kept_marked_and_undo_takes_both_away() {
    let (mut rig, master) = playing_loop();
    overdub_session(&mut rig, master, 0, DUB);
    let first = rig.pcm(0);
    rig.advance(master / JOB_RATE + 2);
    let clean = {
        // The same layer with no gap: a point gap changes no sample.
        let (mut twin, _) = playing_loop();
        overdub_session(&mut twin, master, 0, DUB);
        twin.advance(master / JOB_RATE + 2);
        twin.set_level(DUB);
        twin.advance_to(twin.next_boundary() - master / 2);
        twin.press(Command::RecDub(0));
        twin.advance(2 * (master / 3));
        twin.press(Command::RecDub(0));
        twin.advance(master / JOB_RATE + 2);
        twin.pcm(0)
    };
    rig.set_level(DUB);
    rig.advance_to(rig.next_boundary() - master / 2);
    rig.press(Command::RecDub(0));
    rig.advance(master / 3);
    rig.gap();
    let gap = rig.frame;
    rig.advance(master / 3);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    assert_eq!(rig.dropouts().iter().map(|d| d.2).collect::<Vec<_>>(), [true], "one Dropout, an overdub's");
    let layered = rig.pcm(0);
    assert_eq!(layered, clean, "kept as recorded, bit for bit");
    let at = ((gap - rig.anchor()).rem_euclid(master) / 1024) as usize;
    assert_eq!(rig.marks(0), [at], "the bin the break landed in");
    assert!(rig.state(0) == LaneState::Playing && rig.lane(0).can_undo);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), first, "undo targets the loop at dub start");
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == first && rig.marks(0).is_empty(), "UNDO drops the layer and its mark in one press");
    rig.advance(master / JOB_RATE + 2);
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == layered && rig.marks(0) == [at], "REDO brings both back");
}

/// The bins of `n` loop positions from `first` (wrapping at `master`).
fn bins(first: Frame, n: Frame, master: Frame) -> Vec<usize> {
    let mut out: Vec<usize> = (0..n).map(|k| ((first + k).rem_euclid(master) / 1024) as usize).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// `common::dub` over window `[start, end)` with the frames in `hole` never written.
fn dub_around(pcm: &mut [f32], (start, end): (Frame, Frame), hole: (Frame, Frame), fb: f32, n: Frame, pos: impl Fn(Frame) -> usize, input: impl Fn(Frame) -> f32) {
    for f in (start..end).filter(|f| !(hole.0..hole.1).contains(f)) {
        let p = pos(f);
        pcm[p] = common::dub::write(pcm[p], input(f), fb, f - start, end - f, n);
    }
}

#[test]
fn a_layer_exactly_one_silent_block_long_is_kept_as_recorded_and_marked_whole() {
    // The device rendered one block from silence (an ASIO cycle without input), and the layer's window
    // is exactly that block: it is kept as recorded, its every bin marked.
    let (mut rig, master) = playing_loop();
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 64;
    let b = rig.next_boundary() + master / 4;
    rig.advance_to(b - 512);
    rig.block = 512;
    rig.send_at(b - 64, Command::RecDub(0)); // the window opens on `b`
    rig.send_at(b + 448, Command::RecDub(0)); // and closes 512 frames later
    rig.advance(512);
    assert_eq!(rig.window(), Some((0, Some(b), None)));
    rig.damage(); // the block [b, b + 512) renders from silence
    rig.advance(1024);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let mut want = before.clone();
    common::dub::dub(&mut want, (b, b + 512), ramp(rig.sr), 0.5, pos_fn(rig.anchor(), master, 64), |_| 0.0);
    assert_eq!(rig.pcm(0), want, "the layer as recorded, bit for bit");
    assert_eq!(rig.marks(0), bins(pos_fn(rig.anchor(), master, 64)(b) as Frame, 512, master));
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == before && rig.marks(0).is_empty());
}

#[test]
fn a_layer_opened_and_closed_inside_a_silent_block_is_kept_and_marked() {
    // The block's damage reaches the looper with its frames, before or after the presses in it: the
    // window they open there is marked over every frame of it.
    let (mut rig, master) = playing_loop();
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 368;
    let b = rig.next_boundary() + master / 4;
    rig.advance_to(b);
    rig.block = 1024;
    rig.send_at(b, Command::RecDub(0)); // the window opens on b + 368
    rig.send_at(b + 656, Command::RecDub(0)); // and closes on b + 1024, the block's end
    rig.damage(); // the block [b, b + 1024) renders from silence
    rig.advance(1024);
    rig.advance(1024);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let pos = pos_fn(rig.anchor(), master, 368);
    let mut want = before.clone();
    common::dub::dub(&mut want, (b + 368, b + 1024), ramp(rig.sr), 0.5, &pos, |_| 0.0);
    assert_eq!(rig.pcm(0), want, "the layer as recorded, bit for bit");
    assert_eq!(rig.marks(0), bins(pos(b + 368) as Frame, 656, master));
}

#[test]
fn a_silent_block_marks_the_layer_its_frames_reach_through_a_live_plugin() {
    // The live slot delays its input 512 frames to the record tap: the silent block [b, b + 512) is
    // what the tap carries over [b + 512, b + 1024), the layer's whole window. A block before the
    // window lands inside it and is marked at the frames it reaches.
    const LATENCY: Frame = 512;
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 64; // with the plugin's latency: a window opens 576 frames after its press
    let b = rig.next_boundary() + master / 4;
    rig.advance_to(b - 512);
    rig.block = 512;
    rig.send_at(b - 64, Command::RecDub(0)); // the window opens on b + 512
    rig.send_at(b + 448, Command::RecDub(0)); // and closes on b + 1024
    rig.advance(512);
    assert_eq!(rig.window(), Some((0, Some(b + LATENCY), None)));
    rig.damage(); // the block [b, b + 512) renders from silence
    rig.advance(1024);
    rig.advance(512);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let pos = pos_fn(rig.anchor(), master, 64 + LATENCY);
    let mut want = before.clone();
    common::dub::dub(&mut want, (b + 512, b + 1024), ramp(rig.sr), 0.5, &pos, |_| 0.0);
    assert_eq!(rig.pcm(0), want, "the layer as recorded, bit for bit");
    assert_eq!(rig.marks(0), bins(pos(b + 512) as Frame, 512, master));
}

#[test]
fn a_jump_in_a_layers_closing_tail_keeps_it_and_marks_the_hole_through_a_live_plugin() {
    // The live slot delays its input 512 frames to the record tap. The device never delivers
    // [e - 384, e - 128), so the tap never carries those frames of the layer's window either: those
    // positions keep the old loop, marked, and the rest is the layer as recorded.
    const LATENCY: Frame = 512;
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 64; // with the plugin's latency: a window opens 576 frames after its press
    let b = rig.next_boundary() + master / 4;
    let e = b + 2048;
    rig.advance_to(b - 512);
    rig.block = 256;
    rig.send_at(b - 64, Command::RecDub(0)); // the window opens on b + 512
    rig.send_at(e - 576, Command::RecDub(0)); // and closes on e
    rig.advance_to(e - 384);
    assert_eq!(rig.window(), Some((0, Some(b + LATENCY), Some(e))));
    rig.skip(256);
    rig.advance(1024);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let pos = pos_fn(rig.anchor(), master, 64 + LATENCY);
    let mut want = before.clone();
    dub_around(&mut want, (b + LATENCY, e), (e - 384, e - 128), 0.5, ramp(rig.sr), &pos, |_| 0.0);
    assert_eq!(rig.pcm(0), want, "the old loop in the hole, the layer as recorded around it");
    assert_eq!(rig.marks(0), bins(pos(e - 384) as Frame, 256, master), "the hole marked, nothing else");
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == before && rig.marks(0).is_empty());
}

#[test]
fn a_layer_with_an_input_gap_ended_by_play_stop_lands_stopped_kept_and_marked() {
    // verify/probes/capture-loss.mjs's overdub row: the layer keeps the press's STOP, and the gap only
    // marks it.
    for align in [0, 4800] {
        let (mut rig, master) = playing_loop();
        overdub_session(&mut rig, master, 0, DUB);
        let first = rig.pcm(0);
        rig.advance(master / JOB_RATE + 2);
        rig.align = align;
        rig.set_level(DUB);
        rig.advance_to(rig.next_boundary() - master / 2);
        rig.press(Command::RecDub(0));
        rig.advance(master / 3);
        rig.gap();
        rig.advance(master / 3);
        rig.set_level(0.0);
        rig.keep_output();
        let (press, anchor) = (rig.frame, rig.anchor());
        rig.press(Command::PlayStop(0));
        rig.advance(2 * master);
        assert_eq!(rig.state(0), LaneState::Stopped, "align={align}");
        assert_eq!(rig.dropouts().iter().map(|d| d.2).collect::<Vec<_>>(), [true], "align={align}");
        // Silent from the press but for its 5 ms tail (D23), of the loop ahead of the read head, which
        // the layer never reached.
        let n = ramp(rig.sr);
        for (k, &y) in rig.output.as_ref().unwrap().1.iter().enumerate() {
            let f = press + k as Frame;
            let want = if f < press + n { sample(1.0, tail(f - press, n), first[(f - anchor).rem_euclid(master) as usize], 0.0) } else { 0.0 };
            assert_eq!(y, want, "align={align}: silent from the press but for its tail: frame {f}");
        }
        assert!(rig.pcm(0) != first && rig.marks(0).len() == 1, "align={align}: the layer kept, marked");
        assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), first, "align={align}: UNDO gives the loop before it");
    }
}

#[test]
fn stop_all_a_second_stop_and_clear_punch_out_an_aligned_overdub() {
    // verify/probes/overdub-window.mjs's stopAll, doubleStop and clear modes: impulses played just
    // outside and inside each punch edge (their wet arrives ALIGN later, one inside the window only after
    // the press) are kept exactly inside the window, at their grid position, and playback fades out over
    // 5 ms from the press (D23) while the tail comes in. Inside, the ramps weigh them (D23): the one on the punch frame
    // is the punch-in's first frame (weight 0), the one just before the stop the punch-out's last (1/N).
    const ALIGN: Frame = 7200;
    for gesture in ["stop all", "second stop", "second stop at once", "clear"] {
        let (mut rig, master) = playing_loop();
        rig.align = ALIGN;
        rig.advance_to(rig.next_boundary() + 2400);
        let pre = rig.pcm(0);
        let punch = rig.frame;
        let stop = punch + 1 + master / 4;
        let played: Vec<(Frame, f32)> =
            [punch - 1920, punch - 1, punch, punch + 1920, stop - 960, stop - 1, stop, stop + 1920].iter().enumerate().map(|(k, &f)| (f, (k + 1) as f32 / 32.0)).collect();
        let wet = played.clone();
        rig.set_input(move |f| wet.iter().find(|p| p.0 + ALIGN == f).map_or(0.0, |p| p.1));
        rig.press(Command::RecDub(0));
        rig.advance_to(stop);
        rig.keep_output();
        match gesture {
            "stop all" => rig.press(Command::StopAll),
            "second stop" => {
                rig.press(Command::PlayStop(0));
                rig.advance(ALIGN / 2);
                rig.press(Command::PlayStop(0));
            }
            "second stop at once" => {
                rig.send_at(rig.frame, Command::PlayStop(0));
                rig.send_at(rig.frame, Command::PlayStop(0));
                rig.advance(1);
            }
            _ => rig.press(Command::Clear(0)),
        }
        rig.advance(2 * master);
        let out = &rig.output.as_ref().unwrap().1;
        // A stop fades the lane out over 5 ms from its first press (D23), the loop ahead of the read head,
        // which the layer never reaches; CLEAR silences it at once.
        let (n, anchor) = (ramp(rig.sr), rig.anchor());
        for (k, (&y, &m)) in out.iter().zip(&rig.monitor).enumerate() {
            let f = stop + k as Frame;
            let lane = if gesture != "clear" && f < stop + n { sample(1.0, tail(f - stop, n), pre[(f - anchor).rem_euclid(master) as usize], 0.0) } else { 0.0 };
            assert_eq!(y, lane + m, "{gesture}: frame {f}: the lane's tail from the press, then only the monitor");
        }
        assert!(rig.window().is_none(), "{gesture}: the recorder is free");
        if gesture == "clear" {
            assert!(rig.state(0) == LaneState::Empty && rig.lane(0).length == 0 && rig.master() == 0, "{gesture}: nothing kept");
            continue;
        }
        assert!(rig.state(0) == LaneState::Stopped && rig.lane(0).can_undo, "{gesture}");
        let anchor = rig.anchor();
        let mut want = pre.clone();
        let wet = |f: Frame| played.iter().find(|p| p.0 + ALIGN == f).map_or(0.0, |p| p.1);
        common::dub::dub(&mut want, (punch + ALIGN, stop + ALIGN), ramp(rig.sr), 1.0, pos_fn(anchor, master, ALIGN), wet);
        assert_eq!(rig.pcm(0), want, "{gesture}: exactly the musical punch window, on the grid");
        let at = |f: Frame| (f - anchor).rem_euclid(master) as usize;
        assert_eq!(want[at(punch)], pre[at(punch)], "{gesture}: the punch frame's impulse is the ramp's first frame");
        assert_eq!(want[at(punch + 1920)], pre[at(punch + 1920)] + 4.0 / 32.0, "{gesture}: inside, at full");
    }
}

#[test]
fn reverse_a_flips_the_loop_keeps_its_length() {
    for (bpm, sr) in [(200u32, 48000u32), (137, 44100)] {
        let mut rig = Rig::at(sr);
        rig.set(Command::SetBpm(bpm as f64));
        rig.set_input(code);
        let master = rig.record_first_take(0, 1, 2400);
        let pre = rig.pcm(0);
        rig.press(Command::Reverse(0));
        let flipped: Vec<f32> = pre.iter().rev().copied().collect();
        assert_eq!(rig.pcm(0), flipped);
        if master % 2 == 1 {
            assert_eq!(rig.pcm(0)[(master as usize - 1) / 2], pre[(master as usize - 1) / 2]);
        }
        assert!(rig.lane(0).length == master && rig.master() == master && rig.lane(0).reversed);
    }
}

#[test]
fn reverse_b_a_playing_lane_flips_on_the_next_boundary_twice_is_identity() {
    let (mut rig, master) = playing_loop();
    let pre = rig.pcm(0);
    rig.advance_to(rig.next_boundary() + 480);
    rig.keep_output();
    for k in 0..7 {
        rig.advance(master * (3 * k + 1) / 23);
        let boundary = rig.next_boundary();
        let before = rig.pcm(0);
        let from = rig.frame;
        rig.press(Command::Reverse(0));
        let want: Vec<f32> = if k % 2 == 0 { pre.iter().rev().copied().collect() } else { pre.clone() };
        assert_eq!(rig.pcm(0), want);
        assert_eq!(rig.lane(0).reversed, k % 2 == 0);
        rig.advance_to(boundary + 480);
        plays(&rig, from, boundary, |p| before[p]);
        plays(&rig, boundary, rig.frame, |p| want[p]);
    }
    rig.press(Command::Reverse(0));
    assert!(rig.pcm(0) == pre && !rig.lane(0).reversed);
}

#[test]
fn reverse_c_no_op_unless_committed_stopped_flips_in_place_clear_resets() {
    let (mut rig, master) = playing_loop();
    assert!(rig.lane(0).can_reverse && !rig.lane(1).can_reverse, "a committed lane can reverse, an EMPTY one cannot");
    rig.press(Command::Reverse(1));
    assert!(!rig.lane(1).reversed);
    rig.set_input(code);
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.next_boundary() + 4800);
    assert_eq!(rig.state(1), LaneState::Recording);
    assert!(!rig.lane(1).can_reverse, "no reverse while RECORDING");
    rig.press(Command::Reverse(1));
    assert!(!rig.lane(1).reversed);
    rig.press(Command::Stop(1));
    rig.set_level(DUB);
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    assert!(!rig.lane(0).can_reverse, "no reverse while OVERDUBBING");
    rig.press(Command::Reverse(0));
    assert!(!rig.lane(0).reversed);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.advance(master / JOB_RATE + 2);
    let dubbed = rig.pcm(0);
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    rig.press(Command::Reverse(0));
    assert!(!rig.lane(0).reversed, "a pending loop-end stop blocks reverse: {:?}", rig.lane(0));
    assert!(rig.pcm(0) == dubbed, "the loop is untouched");
    rig.advance_to(rig.next_boundary() + 2400);
    assert!(rig.state(0) == LaneState::Stopped && rig.lane(0).can_reverse);
    rig.keep_output();
    rig.press(Command::Reverse(0));
    let flipped: Vec<f32> = dubbed.iter().rev().copied().collect();
    assert!(rig.lane(0).reversed && rig.pcm(0) == flipped);
    rig.advance(1000);
    assert!(rig.output.as_ref().unwrap().1.iter().all(|&y| y == 0.0));
    rig.set(Command::SetLoopEndStop(false));
    rig.press(Command::PlayStop(0));
    rig.keep_output();
    let from = rig.frame;
    rig.advance(master);
    plays(&rig, from, rig.frame, |p| flipped[p]);
    rig.press(Command::Clear(0));
    assert!(!rig.lane(0).reversed);
}

#[test]
fn reverse_d_the_flag_stays_honest_across_dub_reverse_undo_and_dub_is_refused_reversed() {
    let (mut rig, master) = playing_loop();
    let mut cur = (rig.pcm(0), false);
    let mut undo: Option<(Vec<f32>, bool)> = None;
    let mut seed: u32 = 12345;
    let mut counts = [0; 4];
    let mut layer = 0.0;
    for step in 0..36 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        match (seed >> 16) % 3 {
            1 => {
                rig.press(Command::Reverse(0));
                cur = (cur.0.iter().rev().copied().collect(), !cur.1);
                counts[1] += 1;
            }
            2 => {
                rig.advance(master / JOB_RATE + 2);
                rig.press(Command::Undo(0));
                if let Some(u) = undo.take() {
                    undo = Some(std::mem::replace(&mut cur, u));
                }
                counts[2] += 1;
            }
            _ if cur.1 => {
                let mark = rig.events.len();
                rig.press(Command::Action(Action::RecDub));
                assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Refused { reason: Refusal::Reversed, .. })), "step {step}");
                rig.press(Command::RecDub(0));
                assert_eq!(rig.state(0), LaneState::Playing, "the engine refuses DUB while reversed");
                counts[3] += 1;
            }
            _ => {
                layer += 1.0;
                rig.set_level(layer / 256.0);
                rig.press(Command::RecDub(0));
                rig.advance(rig.seconds(0.1));
                rig.press(Command::RecDub(0));
                rig.set_level(0.0);
                rig.advance(960);
                let next = (rig.pcm(0), cur.1);
                let prev = std::mem::replace(&mut cur, next);
                assert_ne!(cur.0, prev.0);
                undo = Some(prev);
                counts[0] += 1;
            }
        }
        assert_eq!(rig.pcm(0), cur.0, "step {step}: the loop");
        assert_eq!(rig.lane(0).reversed, cur.1, "step {step}: the flag");
        let target = rig.engine.looper().undo_pcm(0);
        match &undo {
            None => assert!(target.is_none()),
            Some(u) => {
                rig.advance(master / JOB_RATE + 2);
                assert_eq!(rig.engine.looper().undo_pcm(0).as_ref(), Some(&u.0), "step {step}: the undo target");
            }
        }
    }
    assert!(counts.iter().all(|&n| n >= 3), "{counts:?}");
}

#[test]
fn the_undo_copy_is_spread_over_frames_and_undo_waits_for_it() {
    let (mut rig, master) = playing_loop();
    let pre = rig.pcm(0);
    rig.set_level(DUB);
    let start = rig.frame;
    rig.press(Command::RecDub(0));
    let done = start + job_frames(master);
    rig.advance_to(done - 3);
    rig.press(Command::RecDub(0)); // commit the layer at once
    rig.set_level(0.0);
    rig.press(Command::Undo(0)); // lands before the copy is done: it waits for it
    assert!(rig.engine.looper().busy() && rig.pcm(0) != pre, "the undo waits for the copy");
    rig.advance_to(done + 1);
    assert!(!rig.engine.looper().busy(), "done on its frame");
    assert_eq!(rig.pcm(0), pre, "the held undo ran once the copy was complete");
}

#[test]
fn a_punch_out_never_waits_for_the_undo_copy() {
    let mut rig = Rig::new();
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 2400); // 384000 frames: the copy takes 375
    rig.set_level(0.0);
    let pre = rig.pcm(0);
    rig.set_level(DUB);
    let punch = rig.frame;
    rig.press(Command::RecDub(0));
    rig.advance(99);
    assert!(rig.engine.looper().busy(), "the undo copy is still running");
    rig.press(Command::RecDub(0)); // punch out on this frame, not when the copy is done
    rig.set_level(0.0);
    rig.idle();
    // Shorter than one ramp: both ramps weigh every one of the 100 frames (D23).
    let mut want = pre.clone();
    common::dub::dub(&mut want, (punch, punch + 100), ramp(rig.sr), 1.0, pos_fn(rig.anchor(), master, 0), |_| DUB);
    assert_eq!(rig.pcm(0), want, "exactly the 100 frames played");
    assert!(master > 0);
}

#[test]
fn rec_on_a_lane_stopping_at_the_loop_end_starts_no_overdub() {
    let (mut rig, _) = playing_loop();
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    rig.set_level(DUB);
    rig.press(Command::RecDub(0));
    assert!(rig.state(0) == LaneState::Playing && rig.lane(0).stop_at.is_some() && rig.window().is_none());
}

// ── Through a live plugin: the mark goes where the tap carries the dropout (ported from the engine-red
// fix branch; a former "commits clean" is no mark, no Dropout and the PCM exact, a former rejection is
// the layer kept as the tap carried it, marked there) ───────────────────────────────────────────────

/// The layer `(start, end)` as `before` with the tap `tap` (frames from `from`) dubbed at `fb`, alignment
/// `align` (with the plugin's latency).
fn as_carried(rig: &Rig, before: &[f32], (start, end): (Frame, Frame), fb: f32, align: Frame, tap: &[f32], from: Frame) -> Vec<f32> {
    let mut want = before.to_vec();
    common::dub::dub(&mut want, (start, end), ramp(rig.sr), fb, pos_fn(rig.anchor(), rig.master(), align), |f| tap[(f - from) as usize]);
    want
}

#[test]
fn a_silent_block_leaves_unmarked_a_layer_over_its_own_frames_through_a_live_plugin() {
    // The live slot delays its input 512 frames to the record tap. Over the silent block [b, b + 512) the
    // tap still carries the input from before it (the silence reaches it over [b + 512, b + 1024)), and
    // the layer's window is exactly [b, b + 512): with no jump in the device frame it lost nothing.
    const LATENCY: Frame = 512;
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 64; // with the plugin's latency: a window opens 576 frames after its press
    let b = rig.next_boundary() + master / 4;
    // As the device side renders the damaged block: silence in its place.
    rig.set_input(move |f| if (b..b + 512).contains(&f) { 0.0 } else { code(f) });
    rig.advance_to(b - 1024);
    rig.block = 512;
    rig.send_at(b - 576, Command::RecDub(0)); // the window opens on b
    rig.send_at(b - 64, Command::RecDub(0)); // and closes on b + 512
    rig.advance(1024);
    assert_eq!(rig.window(), Some((0, Some(b), Some(b + 512))));
    rig.damage();
    rig.keep_output();
    rig.advance(512);
    let carried: Vec<f32> = (b..b + 512).map(|f| code(f - LATENCY)).collect();
    assert_eq!(rig.record, carried, "the tap over the silent block carries the input from before it");
    assert!(rig.record_damage.iter().all(|&d| d == 0), "no LOST over the window");
    assert_eq!(rig.engine.diag().xruns, 1, "the damaged block counts as one xrun");
    rig.advance(1024);
    rig.idle();
    assert!(rig.dropouts().is_empty() && rig.marks(0).is_empty(), "no mark, no Dropout");
    let mut want = before;
    common::dub::dub(&mut want, (b, b + 512), ramp(rig.sr), 0.5, pos_fn(rig.anchor(), master, 64 + LATENCY), |f| code(f - LATENCY));
    assert_eq!(rig.pcm(0), want, "the layer as the tap carried it");
}

#[test]
fn a_short_jump_marks_a_layer_in_the_frames_a_live_plugin_carries_out_of_place() {
    // The live slot delays its input 1024 frames to the record tap, and the device skips 256 frames at j.
    // From s = j + 256 the tap plays out the input the plugin still held from before the jump, 256
    // frames late, until the input from after it arrives on s + 1024. The layer's window [s, s + 512)
    // lies wholly there: kept as carried, every frame marked out of place.
    const LATENCY: Frame = 1024;
    const JUMP: Frame = 256;
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 64; // with the plugin's latency: a window opens 1088 frames after its press
    let j = rig.next_boundary() + master / 4;
    let s = j + JUMP;
    rig.advance_to(j - 1536);
    rig.block = 256;
    rig.send_at(s - 1088, Command::RecDub(0)); // the window opens on s
    rig.send_at(s - 576, Command::RecDub(0)); // and closes on s + 512
    rig.advance_to(j);
    assert_eq!(rig.window(), Some((0, Some(s), Some(s + 512))));
    rig.skip(JUMP);
    rig.keep_output();
    rig.advance(512);
    let late: Vec<f32> = (s..s + 512).map(|f| code(f - LATENCY - JUMP)).collect();
    assert_eq!(rig.record, late, "the tap carries the input from before the jump, out of place");
    assert!(rig.record_damage.iter().all(|&d| d & damage::LOST != 0), "every frame of it LOST");
    let tap = rig.record.clone();
    rig.advance(2048);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let first = (s - 64 - LATENCY - rig.anchor()).rem_euclid(master);
    assert_eq!(rig.marks(0), bins(first, 512, master), "the window marked whole");
    assert_eq!(rig.pcm(0), as_carried(&rig, &before, (s, s + 512), 0.5, 64 + LATENCY, &tap, s), "kept as carried");
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), before, "UNDO: the loop before the layer, bit for bit");
}

/// A committed one-bar loop at 200 bpm through a live slot that delays its input `latency` frames, the
/// loop settled; the alignment 512 frames without the plugin's latency. From `b`, the block start that
/// comes next, the input is the frame code but on each `silent` block (128 frames from `b` plus each).
fn through_a_plugin(latency: Frame, silent: &[Frame]) -> (Rig, Vec<f32>, Frame) {
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(latency)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 512;
    let b = rig.next_boundary() + master / 4;
    rig.advance_to(b);
    // As the device side renders a damaged block: silence in its place.
    let silent = silent.to_vec();
    rig.set_input(move |f| if silent.iter().any(|&s| (b + s..b + s + 128).contains(&f)) { 0.0 } else { code(f) });
    (rig, before, b)
}

/// The layer's loop positions of tap frames `[from, to)`, the window aligned `align`.
fn tap_bins(rig: &Rig, align: Frame, from: Frame, to: Frame) -> Vec<usize> {
    bins((from - align - rig.anchor()).rem_euclid(rig.master()), to - from, rig.master())
}

#[test]
fn a_silent_block_a_plugin_plays_out_after_go_live_off_marks_the_layer_it_reaches() {
    // The live slot delays its input 1024 frames to the record tap. The silent block [b, b + 128) reaches
    // it over [b + 1024, b + 1152); GO LIVE off on b + 128 drops the record latency to 0 from the next
    // block, but the plugin still plays out what it held. A second silent block [b + 256, b + 384), its
    // first 112 frames still inside the off ramp, reaches the tap over [b + 1280, b + 1392). A layer opens
    // on b + 896 (its press on b + 384) and closes on b + 1408, over both: kept as carried, marked there.
    const LATENCY: Frame = 1024;
    let (mut rig, before, b) = through_a_plugin(LATENCY, &[0, 256]);
    rig.send_at(b + 128, Command::SetSlotLive(0, false));
    rig.send_at(b + 384, Command::RecDub(0)); // the window opens on b + 896
    rig.send_at(b + 896, Command::RecDub(0)); // and closes on b + 1408
    rig.keep_output();
    rig.damage();
    rig.advance(256);
    rig.damage();
    rig.advance(128);
    assert_eq!(rig.engine.live_latency(), 0, "the record latency follows GO LIVE off");
    rig.advance(896);
    assert_eq!(rig.window(), Some((0, Some(b + 896), Some(b + 1408))));
    let carried: Vec<f32> = (b + 896..b + 1024).map(|f| code(f - LATENCY)).collect();
    assert_eq!(rig.record[896..1024], carried, "the plugin plays out what it held, into the window");
    assert!(rig.record[1024..1152].iter().all(|&x| x == 0.0), "and the first silent block after it");
    rig.advance(1024);
    let tap = rig.record.clone();
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    let mut want = tap_bins(&rig, 512, b + 1024, b + 1152);
    want.extend(tap_bins(&rig, 512, b + 1280, b + 1392));
    want.sort_unstable();
    want.dedup();
    assert_eq!(rig.marks(0), want, "marked where the plugin plays each block out");
    assert_eq!(rig.pcm(0), as_carried(&rig, &before, (b + 896, b + 1408), 0.5, 512, &tap, b), "kept as carried");
}

#[test]
fn a_silent_block_in_a_go_live_off_ramp_marks_the_layer_the_plugin_plays_it_into() {
    // The live slot delays its input 1024 frames to the record tap. GO LIVE off on b ramps the slot's
    // input out over 240 frames and drops the record latency to 0 from the next block, but the plugin
    // still plays the ramp out 1024 frames late: of the silent block [b + 128, b + 256) the ramp lets
    // [b + 128, b + 240) through, which reaches the tap over [b + 1152, b + 1264), inside a layer that
    // opens on b + 768 (its press on b + 256) and closes on b + 1280.
    const LATENCY: Frame = 1024;
    let (mut rig, before, b) = through_a_plugin(LATENCY, &[128]);
    rig.send_at(b, Command::SetSlotLive(0, false));
    rig.send_at(b + 256, Command::RecDub(0)); // the window opens on b + 768
    rig.send_at(b + 768, Command::RecDub(0)); // and closes on b + 1280
    rig.keep_output();
    rig.advance(128);
    rig.damage();
    rig.advance(1152);
    assert_eq!(rig.window(), Some((0, Some(b + 768), Some(b + 1280))));
    assert!(rig.record[1024..1152].iter().all(|&x| x != 0.0), "the ramp's first frames play out, faded");
    assert!(rig.record[1152..1264].iter().all(|&x| x == 0.0), "and its silent block after them");
    let lost: Vec<usize> = rig.record_damage.iter().enumerate().filter(|(_, &d)| d & damage::LOST != 0).map(|(k, _)| k).collect();
    assert_eq!(lost, (1152..1264).collect::<Vec<_>>(), "LOST exactly on what the ramp let through");
    rig.advance(1024);
    let tap = rig.record.clone();
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert_eq!(rig.marks(0), tap_bins(&rig, 512, b + 1152, b + 1264));
    assert_eq!(rig.pcm(0), as_carried(&rig, &before, (b + 768, b + 1280), 0.5, 512, &tap, b), "kept as carried");
}

#[test]
fn a_layer_a_go_live_off_ramps_silent_block_reaches_the_tap_just_after_is_unmarked() {
    // The live slot delays its input 1024 frames to the record tap. GO LIVE off on b ramps the slot's
    // input out over 240 frames and drops the record latency to 0 from the next block; the silent block
    // [b + 128, b + 256), in the ramp, reaches the tap over [b + 1152, b + 1264), just after a layer that
    // opens on b + 640 (its press on b + 128) and closes on b + 1152: what the layer holds is clean.
    const LATENCY: Frame = 1024;
    let (mut rig, before, b) = through_a_plugin(LATENCY, &[128]);
    let master = rig.master();
    rig.send_at(b, Command::SetSlotLive(0, false));
    rig.send_at(b + 128, Command::RecDub(0)); // the window opens on b + 640
    rig.send_at(b + 640, Command::RecDub(0)); // and closes on b + 1152
    rig.advance(128);
    rig.damage();
    rig.advance(640);
    assert_eq!(rig.window(), Some((0, Some(b + 640), Some(b + 1152))));
    rig.advance(2048);
    rig.idle();
    assert!(rig.dropouts().is_empty() && rig.marks(0).is_empty(), "no mark, no Dropout");
    // The tap carries the input 1024 frames late, the ramp's frames as the live gate fades them.
    let tap = move |f: Frame| {
        let x = f - LATENCY;
        let gate = if x < b { 1.0 } else { 1.0 + (0.0 - 1.0) * ((x - b) as f32 / 240.0) };
        code(x) * gate
    };
    let mut want = before;
    common::dub::dub(&mut want, (b + 640, b + 1152), ramp(rig.sr), 0.5, pos_fn(rig.anchor(), master, 512), tap);
    assert_eq!(rig.pcm(0), want, "the layer as the tap carried it");
}

/// Through `through_a_plugin` (1024 frames, `silent`, `block`-frame blocks from `b`): GO LIVE off on
/// `b + off` ramps the slot's input out over 240 frames, and drops the record latency to 0 from the next
/// block; the silent blocks are damaged. A layer opens on `b + open` and closes 512 frames on, after
/// everything the ramp lets through has reached the tap: the plugin plays silence into it, unmarked.
fn a_layer_after_a_go_live_off_ramp(off: Frame, silent: &[Frame], block: usize, open: Frame) {
    let (mut rig, before, b) = through_a_plugin(1024, silent);
    let master = rig.master();
    rig.block = block;
    rig.send_at(b + off, Command::SetSlotLive(0, false));
    rig.send_at(b + open - 512, Command::RecDub(0));
    rig.send_at(b + open, Command::RecDub(0));
    let quiet = silent.iter().max().unwrap() + 128;
    while rig.frame < b + quiet {
        let at = rig.frame - b;
        if silent.iter().any(|&s| (at..at + block as Frame).contains(&s)) {
            rig.damage();
        }
        rig.advance(block as Frame);
    }
    rig.advance_to(b + open);
    rig.keep_output();
    rig.advance(512);
    assert_eq!(rig.window(), Some((0, Some(b + open), Some(b + open + 512))));
    assert!(rig.record.iter().all(|&x| x == 0.0), "the plugin plays silence into the layer");
    assert!(rig.record_damage.iter().all(|&d| d & damage::LOST == 0), "no LOST in it");
    rig.advance(2048);
    rig.idle();
    assert!(rig.dropouts().is_empty() && rig.marks(0).is_empty(), "no mark, no Dropout");
    let mut want = before;
    common::dub::dub(&mut want, (b + open, b + open + 512), ramp(rig.sr), 0.5, pos_fn(rig.anchor(), master, 512), |_| 0.0);
    assert_eq!(rig.pcm(0), want, "the layer as the tap carried it");
}

#[test]
fn a_layer_after_what_a_go_live_off_ramp_lets_through_of_a_silent_block_is_unmarked() {
    // GO LIVE off on b: the gate is shut from b + 240, so of the silent block [b + 128, b + 256) the
    // plugin plays only [b + 1152, b + 1264) into the tap, and a layer from b + 1264 holds none of it.
    a_layer_after_a_go_live_off_ramp(0, &[128], 128, 1264);
}

#[test]
fn a_layer_after_what_a_go_live_off_in_a_silent_block_lets_through_is_unmarked() {
    // The silent block [b, b + 512) holds the GO LIVE off on b + 128: the gate is shut from b + 368, so the
    // plugin plays only [b + 1024, b + 1392) of it into the tap, and a layer from b + 1392 holds none of it.
    a_layer_after_a_go_live_off_ramp(128, &[0, 128, 256, 384], 512, 1392);
}

#[test]
fn a_silent_block_a_removal_fades_in_on_its_own_frames_marks_the_layer_there() {
    // The live slot delays its input 1024 frames to the record tap, latched for a layer that closes on
    // b + 256. Its removal on b crossfades to the slot's dry side over 480 frames, which reaches the tap
    // on its own frames: the silent block [b + 128, b + 256) is in the layer at once, not 1024 frames on.
    const LATENCY: Frame = 1024;
    let mut rig = Rig::new();
    rig.install(0, Box::new(common::Delay::new(LATENCY)));
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.idle();
    let before = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.align = 512; // with the plugin's latency: a window opens 1536 frames after its press
    let b = rig.next_boundary() + master / 4;
    rig.advance_to(b - 2048);
    rig.send_at(b - 2048, Command::RecDub(0)); // the window opens on b - 512
    rig.send_at(b - 1280, Command::RecDub(0)); // and closes on b + 256
    rig.advance_to(b - 512);
    rig.keep_output();
    rig.advance_to(b);
    assert_eq!(rig.window(), Some((0, Some(b - 512), Some(b + 256))));
    // As the device side renders the damaged block: silence in its place.
    rig.set_input(move |f| if (b + 128..b + 256).contains(&f) { 0.0 } else { code(f) });
    rig.remove(0);
    rig.advance(128);
    rig.damage();
    rig.advance(128);
    assert!(rig.record[640..768].iter().zip(b + 128..).all(|(&x, f)| x != code(f - LATENCY)), "the dry side's silence is in the tap");
    let lost: Vec<usize> = rig.record_damage.iter().enumerate().filter(|(_, &d)| d & damage::LOST != 0).map(|(k, _)| k).collect();
    assert_eq!(lost, (640..768).collect::<Vec<_>>(), "LOST on the block's own frames, through the dry side");
    let tap = rig.record.clone();
    rig.advance(2048);
    rig.idle();
    assert_eq!(rig.dropouts().len(), 1);
    assert!(rig.state(0) == LaneState::Playing);
    assert_eq!(rig.marks(0), tap_bins(&rig, 512 + LATENCY, b + 128, b + 256));
    assert_eq!(rig.pcm(0), as_carried(&rig, &before, (b - 512, b + 256), 0.5, 512 + LATENCY, &tap, b - 512), "kept as carried");
}

/// Through `through_a_plugin` (1024 frames, the silent block [b, b + 128) damaged): the plugin's removal
/// asked on `b + removed` crossfades it out over 480 frames, before the silence it took in could come out
/// 1024 frames on; with no capture running the record latency drops to 0 once it has left. A layer opens
/// on `b + open` (alignment `align`) and closes 512 frames on, over where the plugin would have played the
/// silence: the slot passes its clean input there, unmarked.
fn a_layer_where_a_removed_plugin_would_have_played_a_silent_block(removed: Frame, align: Frame, open: Frame) {
    let (mut rig, before, b) = through_a_plugin(1024, &[0]);
    let master = rig.master();
    rig.align = align;
    rig.send_at(b + open - align, Command::RecDub(0));
    rig.send_at(b + open + 512 - align, Command::RecDub(0));
    if removed == 0 {
        rig.remove(0);
    }
    rig.damage();
    rig.advance(128);
    if removed > 0 {
        rig.advance_to(b + removed);
        rig.remove(0);
    }
    rig.advance_to(b + open + 512);
    assert_eq!(rig.window(), Some((0, Some(b + open), Some(b + open + 512))));
    assert!(rig.returned(0).is_some(), "the plugin has left");
    rig.advance(2048);
    rig.idle();
    assert!(rig.dropouts().is_empty() && rig.marks(0).is_empty(), "no mark, no Dropout");
    let mut want = before;
    common::dub::dub(&mut want, (b + open, b + open + 512), ramp(rig.sr), 0.5, pos_fn(rig.anchor(), master, align), code);
    assert_eq!(rig.pcm(0), want, "the layer as the tap carried it");
}

#[test]
fn a_layer_where_a_plugin_removed_in_a_silent_block_would_have_played_it_is_unmarked() {
    // Removed on b, gone on b + 480; the silence would have come out over [b + 1024, b + 1152).
    a_layer_where_a_removed_plugin_would_have_played_a_silent_block(0, 512, 1024);
}

#[test]
fn a_layer_where_a_plugin_removed_after_a_silent_block_would_have_played_it_is_unmarked() {
    // Removed on b + 128, gone on b + 608, the silence already inside it; it would have come out over
    // [b + 1024, b + 1152), inside the layer [b + 768, b + 1280).
    a_layer_where_a_removed_plugin_would_have_played_a_silent_block(128, 128, 768);
}
