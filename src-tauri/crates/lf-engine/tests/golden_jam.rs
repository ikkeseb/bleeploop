//! Ports verify/probes/golden-jam.mjs: the golden jam, end to end on the engine, at 44.1 and 48 kHz and at
//! every block size of the plan (1, 32, 64, 127, 128, 480, 1024). Its assertions 1–6, 8 and 9 are here;
//! 7 (injected plugin-bridge loss) becomes an injected input gap; 10 (real key presses, the lane cue)
//! stays a UI probe, while the engine half it relied on, the gates, the refusals and the CLEAR double
//! press, is asserted here. The session import, local recovery, FX and export blocking of the browser
//! jam belong to later stages.
//!
//! The browser jam could only compare takes with each other and never heard playback. Here the frames
//! are absolute and the output is in hand: every impulse is checked against the frame it was injected
//! on (a uniform slip of all takes fails), the rendered output against the lanes at the grid phase, and
//! the whole jam's output is bit-identical across block sizes.

mod common;

use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::looper::JOB_RATE;
use lf_engine::{Action, Command, Event, LaneState, Refusal};

const BPM: u32 = 120;
const BARS: Frame = 2;
const BEATS: usize = 8;
const FOUND: f32 = 0.5;

/// One amplitude per beat, repeating once per master loop, so a whole-beat slip changes amplitudes.
fn fingerprint(b: usize) -> f32 {
    0.9 + 0.25 * b as f32 / (BEATS - 1) as f32
}

fn hits(pcm: &[f32]) -> Vec<(usize, f32)> {
    pcm.iter().enumerate().filter(|(_, x)| x.abs() > FOUND).map(|(k, x)| (k, x.abs())).collect()
}

fn peak(pcm: &[f32]) -> f32 {
    pcm.iter().fold(0.0, |m, x| m.max(x.abs()))
}

fn tiles(pcm: &[f32], bar: usize) -> bool {
    (bar..pcm.len()).all(|k| pcm[k] == pcm[k % bar])
}

fn rejected(rig: &Rig, mark: usize) -> usize {
    rig.events[mark..].iter().filter(|e| matches!(e, Event::TakeRejected { .. })).count()
}

fn refused(rig: &Rig, mark: usize, reason: Refusal) -> bool {
    rig.events[mark..].iter().any(|e| matches!(*e, Event::Refused { reason: r, .. } if r == reason))
}

/// The output from `from` to now equals the input plus the lanes at the grid phase.
fn output_on_grid(rig: &Rig, from: Frame, lanes: &[usize], input: &dyn Fn(Frame) -> f32) {
    let (start, out) = rig.output.as_ref().unwrap();
    let pcms: Vec<Vec<f32>> = lanes.iter().map(|&i| rig.pcm(i)).collect();
    let (anchor, master) = (rig.anchor(), rig.master());
    for f in from..rig.frame {
        let pos = (f - anchor).rem_euclid(master) as usize;
        let mut want = 0.0f32;
        for p in &pcms {
            want += p[pos];
        }
        want += input(f);
        assert_eq!(out[(f - start) as usize], want, "frame {f}");
    }
}

/// Render until the recorder's window, if one is still open, has closed.
fn settle(rig: &mut Rig) {
    if let Some(end) = rig.window().and_then(|w| w.2) {
        rig.advance_to(end + 1);
    }
}

/// The whole jam; returns its rendered output.
fn jam(sr: u32, block: usize) -> Vec<f32> {
    let mut rig = Rig::with(Opts { sr, start: sr as Frame, loop_seconds: 20.0, block, align: 0 });
    rig.keep_output();
    let beat = (60.0 / BPM as f64 * sr as f64).round() as Frame;
    let bar = 4 * beat;
    let expected_master = BARS * bar;
    let s = |x: f64| (x * sr as f64).round() as Frame;

    // ── 8: AUTO REC ─────────────────────────────────────────────────────────────────────────────
    rig.set(Command::SetAutoSensitivity(50.0));
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    rig.advance(s(0.18));
    let info = rig.lane(0);
    assert!(info.state == LaneState::Recording && !info.armed && info.auto_armed);
    assert!(rig.engine.looper().written(0) == 0 && !rig.locked(), "AUTO listens blank and unlocked");
    rig.press(Command::RecDub(0));
    assert!(rig.state(0) == LaneState::Empty && !rig.lane(0).auto_armed && rig.master() == 0 && !rig.locked());
    rig.press(Command::RecDub(0));
    let soft = rig.frame + s(0.05) + s(0.02);
    let (loud, end) = (soft + s(0.008), soft + s(0.008) + s(0.02));
    rig.set_input(move |f| if f >= soft && f < loud { 0.01 } else if f >= loud && f < end { 0.1 } else { 0.0 });
    rig.advance_to(loud + s(0.02));
    assert!(!rig.lane(0).auto_armed && rig.engine.looper().written(0) > 0, "AUTO triggered from the input");
    rig.advance(s(0.18));
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    let pcm = rig.pcm(0);
    let first = pcm.iter().position(|x| x.abs() > 0.001).unwrap() as Frame;
    assert!(rig.state(0) == LaneState::Playing && pcm.len() as Frame == bar, "a sub-bar AUTO take pads to a bar");
    assert!(first <= s(0.012) && peak(&pcm) > 0.05, "the soft onset near frame 0: {first}");
    rig.press(Command::ClearAll);
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetFixedLength(true));
    rig.press(Command::RecDub(0));
    let on = rig.frame + s(0.05) + s(0.01);
    let off = on + s(0.03);
    rig.set_input(move |f| if f >= on && f < off { 0.1 } else { 0.0 });
    rig.advance_to(on + s(0.02));
    let window_end = rig.end_frame();
    rig.advance_to(window_end + 1);
    assert!(rig.state(0) == LaneState::Playing && rig.master() == bar && !rig.lane(0).auto_armed, "FIXED + AUTO: one bar");
    rig.set_level(0.0);
    rig.press(Command::ClearAll);
    rig.set(Command::SetFixedLength(false));
    rig.set(Command::SetAutoRecord(false));

    // ── RETAKE: a rolling take keeps the last complete pass ─────────────────────────────────────
    // The input is a slow ramp: every sample names the frame it was captured on.
    let t0 = rig.frame + s(0.05);
    let ramp = move |f: Frame| if f < t0 { 0.0 } else { (f - t0) as f32 / 4_000_000.0 };
    let frame_of = move |v: f32| t0 + (v * 4_000_000.0).round() as Frame;
    let unbroken = |pcm: &[f32]| pcm.windows(2).all(|w| w[1] > w[0]);
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetRetake(true));
    rig.set_input(ramp);
    rig.press(Command::RecDub(0));
    let a_start = rig.start_frame();
    rig.advance_to(a_start + 2 * bar + s(0.6));
    assert!(rig.lane(0).retake_pass == 3 && rig.master() == 0, "a rolling first take shows its pass, commits nothing");
    assert!(rig.engine.looper().rec_dub_gate(1).is_ok(), "an EMPTY lane offers REC as the approve gesture");
    rig.press(Command::RecDub(0));
    let take_a = rig.pcm(0);
    assert!(rig.state(0) == LaneState::Playing && take_a.len() as Frame == bar && unbroken(&take_a));
    assert_eq!(frame_of(take_a[0]), a_start + bar, "the last COMPLETE pass (2), not the one in flight");
    rig.press(Command::RecDub(1));
    let b_start = rig.start_frame();
    rig.advance_to(b_start + bar + s(0.5));
    assert_eq!(rig.lane(1).retake_pass, 2);
    rig.press(Command::RecDub(2));
    let take_b = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && rig.state(2) == LaneState::Recording, "REC elsewhere approves and takes over");
    assert!(unbroken(&take_b) && frame_of(take_b[0]) == b_start, "the handed-off lane kept its last complete pass");
    assert_eq!((frame_of(take_b[0]) - frame_of(take_a[0])) % bar, 0, "every pass on the first take's grid");
    let c_start = rig.start_frame();
    assert_eq!(c_start, b_start + 2 * bar, "the handoff starts lane 3 on the pass edge");
    let edge = c_start + 2 * bar;
    rig.advance_to(edge - s(0.06));
    rig.press(Command::PlayStop(2));
    assert_eq!(rig.state(2), LaneState::Recording, "a press inside the grace lets the pass finish");
    rig.advance_to(edge + 1);
    let take_c = rig.pcm(2);
    assert!(rig.state(2) == LaneState::Stopped && unbroken(&take_c) && frame_of(take_c[0]) == edge - bar);
    // A gap in pass 1 drops it; the clean pass 2 is what an approval later keeps.
    let mark = rig.events.len();
    rig.press(Command::RecDub(3));
    let d_start = rig.start_frame();
    rig.advance_to(d_start + bar / 2);
    rig.gap();
    rig.advance_to(d_start + bar + bar / 2);
    rig.press(Command::RecDub(4));
    assert!(rig.state(3) == LaneState::Recording && rig.state(4) == LaneState::Empty, "nothing clean kept: REC elsewhere ignored");
    rig.advance_to(d_start + 2 * bar + s(0.5));
    rig.press(Command::RecDub(3));
    let take_d = rig.pcm(3);
    assert!(rig.state(3) == LaneState::Playing && unbroken(&take_d) && frame_of(take_d[0]) == d_start + bar);
    assert_eq!(rig.events[mark..].iter().filter(|e| matches!(e, Event::PassDropped { .. })).count(), 1);
    rig.set_level(0.0);
    rig.press(Command::ClearAll);
    rig.set(Command::SetRetake(false));
    rig.set(Command::SetFixedLength(false));

    // ── arrange the jam: an impulse train, one fingerprinted impulse per beat ───────────────────
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(BARS as f64));
    let t0 = rig.frame + s(0.2);
    let train = move |f: Frame| if f >= t0 && (f - t0) % beat == 0 { fingerprint(((f - t0) / beat) as usize % BEATS) } else { 0.0 };
    rig.set_input(train);
    let mark = rig.events.len();
    let xruns = rig.engine.diag().xruns;

    // ── 1–3, 5: take 1 defines the grid, take 2 arms on its boundary ─────────────────────────────
    rig.press(Command::RecDub(0));
    let start1 = rig.start_frame();
    rig.advance_to(rig.end_frame() + 1);
    rig.press(Command::RecDub(1));
    let start2 = rig.start_frame();
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.master(), expected_master, "1: the master is exactly {BARS} bars");
    for (i, start) in [(0, start1), (1, start2)] {
        let pcm = rig.pcm(i);
        assert_eq!(pcm.len() as Frame, expected_master);
        let h = hits(&pcm);
        assert!(h.len() == BEATS || h.len() == BEATS - 1, "track {i}: the impulse train");
        assert!(h.windows(2).all(|w| (w[1].0 - w[0].0) as Frame == beat), "2: every gap exactly one beat");
        assert_eq!(expected_master - h.last().unwrap().0 as Frame + h[0].0 as Frame, beat, "the wrap closes to the sample");
        for &(k, amp) in &h {
            assert_eq!(amp, train(start + k as Frame), "track {i}: the impulse injected on frame {}", start + k as Frame);
        }
    }
    assert_eq!(hits(&rig.pcm(0)), hits(&rig.pcm(1)), "3: track 2 is frame-identical to track 1");
    assert!(rejected(&rig, mark) == 0 && rig.engine.diag().xruns == xruns, "5: nothing lost");
    let from = rig.frame;
    rig.advance(expected_master / 2);
    output_on_grid(&rig, from, &[0, 1], &train);

    // ── 9: short later takes tile across the master ─────────────────────────────────────────────
    rig.set(Command::SetFixedLength(false));
    rig.press(Command::RecDub(2));
    rig.advance_to(rig.start_frame() + s(0.5));
    rig.press(Command::PlayStop(2));
    assert_eq!(rig.state(2), LaneState::Recording, "a stop inside the first bar records on to the bar line");
    rig.advance_to(rig.end_frame() + 1 + bar / JOB_RATE + 2);
    let early = rig.pcm(2);
    assert!(rig.state(2) == LaneState::Stopped && early.len() as Frame == expected_master);
    assert!(tiles(&early, bar as usize) && peak(&early) > FOUND);
    rig.press(Command::Clear(2));
    rig.set(Command::SetFixedBars(1.0));
    rig.set(Command::SetFixedLength(true));
    assert_eq!(rig.engine.looper().next_take_max_bars(rig.bpm()), BARS);
    rig.press(Command::RecDub(2));
    rig.advance_to(rig.end_frame() + 1 + expected_master / JOB_RATE + 2);
    let fixed = rig.pcm(2);
    assert!(rig.state(2) == LaneState::Playing && fixed.len() as Frame == expected_master);
    assert!(tiles(&fixed, bar as usize) && peak(&fixed) > FOUND);
    rig.press(Command::Clear(2));
    rig.set(Command::SetRetake(true));
    rig.press(Command::RecDub(3));
    let roll = rig.start_frame();
    assert_eq!(rig.end_frame() - roll, expected_master, "RETAKE rolls at the master whatever FIXED says");
    rig.press(Command::Stop(3));
    rig.press(Command::Clear(3));
    rig.set(Command::SetRetake(false));
    rig.set(Command::SetFixedBars(BARS as f64));
    let states: Vec<_> = (0..5).map(|i| rig.state(i)).collect();
    assert_eq!(states, [LaneState::Playing, LaneState::Playing, LaneState::Empty, LaneState::Empty, LaneState::Empty]);

    // ── 4, 6: overdub, undo, redo ────────────────────────────────────────────────────────────────
    let base = hits(&rig.pcm(0));
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.advance(expected_master + s(0.4));
    rig.press(Command::RecDub(0));
    let dubbed = rig.pcm(0);
    let dh = hits(&dubbed);
    assert_eq!(dh.iter().map(|h| h.0).collect::<Vec<_>>(), base.iter().map(|h| h.0).collect::<Vec<_>>(), "4: the dub landed on the grid");
    let multiples: Vec<f32> = dh.iter().zip(&base).map(|(d, b)| d.1 / b.1).collect();
    assert!(multiples.iter().all(|m| (m - m.round()).abs() <= 0.01 && (2.0..=4.0).contains(&m.round())), "4: same absolute position {multiples:?}");
    assert!(peak(&dubbed) > 1.5);
    rig.advance(expected_master / JOB_RATE + 2);
    rig.press(Command::Undo(0));
    assert!(peak(&rig.pcm(0)) < 1.5, "undo restored the pre-dub take");
    rig.press(Command::Undo(0));
    assert!(rig.pcm(0) == dubbed, "redo restored the dub");

    // ── 7: an input gap rejects an overdub (restoring PCM and undo) and a later take ─────────────
    let mark = rig.events.len();
    rig.advance(expected_master / JOB_RATE + 2);
    rig.press(Command::RecDub(0));
    rig.advance(s(0.3));
    rig.gap();
    rig.advance(s(0.3));
    rig.press(Command::RecDub(0));
    rig.advance(expected_master / JOB_RATE + 2);
    assert!(rig.state(0) == LaneState::Playing && rig.pcm(0) == dubbed, "the exact pre-layer PCM");
    assert!(rig.lane(0).can_undo && rejected(&rig, mark) == 1);
    rig.press(Command::Undo(0));
    assert!(peak(&rig.pcm(0)) < 1.5, "undo still reaches the take before the earlier overdub");
    rig.advance(expected_master / JOB_RATE + 2);
    rig.press(Command::Undo(0));
    rig.press(Command::RecDub(2));
    rig.advance_to(rig.start_frame() + s(0.1));
    rig.gap();
    rig.press(Command::PlayStop(2));
    rig.advance_to(rig.end_frame() + 1);
    assert!(rig.state(2) == LaneState::Empty && rig.lane(2).length == 0 && rig.master() == expected_master);
    assert_eq!(rejected(&rig, mark), 2);

    // ── the engine half of the key section: gates, refusals, selection, the CLEAR double press ──
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Stopped);
    let mark = rig.events.len();
    rig.press(Command::SelectTrack(0));
    rig.press(Command::Action(Action::RecDub));
    assert!(refused(&rig, mark, Refusal::PlayFirst) && rig.state(0) == LaneState::Stopped, "a refused REC/DUB says why");
    rig.press(Command::Action(Action::Undo));
    assert!(peak(&rig.pcm(0)) < 1.5, "UNDO on the selected lane");
    rig.advance(expected_master / JOB_RATE + 2);
    rig.press(Command::Action(Action::Undo));
    assert_eq!(rig.pcm(0), dubbed);
    let mut seen = Vec::new();
    for a in [Action::PrevTrack, Action::NextTrack, Action::PrevTrack, Action::NextTrack, Action::NextTrack, Action::PrevTrack, Action::NextTrack, Action::NextTrack] {
        rig.press(Command::Action(a));
        seen.push(rig.engine.looper().selected() + 1);
    }
    assert_eq!(seen, [5, 1, 5, 1, 2, 1, 2, 3], "next/prev wrap at both ends");
    rig.press(Command::Copy(1));
    rig.advance(expected_master / JOB_RATE + 2);
    let copy = (0..5).find(|&i| rig.events.iter().any(|e| matches!(*e, Event::Copied { to, .. } if to as usize == i))).unwrap();
    rig.press(Command::SelectTrack(copy as u8));
    let mark = rig.events.len();
    rig.press(Command::Action(Action::Clear));
    assert!(rig.state(copy) != LaneState::Empty && refused(&rig, mark, Refusal::ConfirmClear), "one CLEAR only arms");
    rig.press(Command::Action(Action::NextTrack));
    rig.press(Command::Action(Action::PrevTrack));
    rig.press(Command::Action(Action::Clear));
    assert!(rig.state(copy) != LaneState::Empty, "a looper press between the two breaks the guard");
    rig.press(Command::SelectTrack(0));
    rig.press(Command::SelectTrack(copy as u8));
    rig.press(Command::Action(Action::Clear));
    assert!(rig.state(copy) != LaneState::Empty, "a selection between the two breaks the guard");
    rig.advance(s(3.0));
    rig.press(Command::Action(Action::Clear));
    assert!(rig.state(copy) != LaneState::Empty, "a press after the confirm window arms again");
    rig.press(Command::Action(Action::Clear));
    assert_eq!(rig.state(copy), LaneState::Empty, "two in a row clear");
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Playing);

    // ── 9: PLAY from the top, and a join beside a playing lane ──────────────────────────────────
    rig.press(Command::StopAll);
    assert!(rig.state(0) == LaneState::Stopped && rig.state(1) == LaneState::Stopped);
    rig.advance(expected_master * 37 / 100);
    let press = rig.frame;
    rig.press(Command::PlayAll);
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Playing && rig.anchor() == press, "PLAY ALL restarts from the top");
    rig.advance(s(0.3));
    output_on_grid(&rig, press, &[0, 1], &train);
    let anchor = rig.anchor();
    rig.advance(expected_master * 3 / 10);
    rig.press(Command::PlayStop(1));
    rig.press(Command::PlayStop(1));
    let from = rig.frame;
    rig.advance(s(0.3));
    assert_eq!(rig.anchor(), anchor, "a resume beside a playing lane joins its phase");
    output_on_grid(&rig, from, &[0, 1], &train);

    // ── 6: COPY and CLEAR ────────────────────────────────────────────────────────────────────────
    rig.press(Command::SetVolume(0, 0.4));
    let src = rig.pcm(0);
    let mark = rig.events.len();
    rig.press(Command::Copy(0));
    rig.advance(expected_master / JOB_RATE + 2);
    assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Copied { from: 0, to: 2, .. })), "the first EMPTY lane");
    assert!(rig.state(2) == LaneState::Playing && rig.lane(2).length == expected_master && rig.pcm(2) == src);
    assert_eq!(rig.engine.looper().volume(2), (0.4, false));
    rig.press(Command::Reverse(2));
    let flipped: Vec<f32> = src.iter().rev().copied().collect();
    assert!(rig.pcm(0) == src && rig.pcm(2) == flipped, "the copy shares nothing with its source");
    rig.press(Command::Clear(2));
    rig.press(Command::Copy(9));
    assert_eq!(rig.state(2), LaneState::Empty, "a missing lane is a no-op");
    rig.press(Command::SetVolume(0, 0.3));
    rig.press(Command::SetMute(0, true));
    rig.press(Command::Clear(0));
    assert!(rig.state(0) == LaneState::Empty && rig.engine.looper().volume(0) == (1.0, false), "CLEAR resets the lane's mix");

    // ── 6: a cancelled arm resets a grid whose committed lanes were cleared ──────────────────────
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).armed);
    rig.press(Command::Clear(1));
    assert_eq!(rig.master(), expected_master, "the grid survives while the arm is live");
    rig.press(Command::RecDub(0));
    assert!(rig.master() == 0 && !rig.locked() && (0..5).all(|i| rig.state(i) == LaneState::Empty));

    // ── 7: a gap in the discarded count-in costs nothing; one in a first take rejects it ─────────
    rig.set(Command::SetFixedLength(false));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    rig.advance(s(0.5));
    rig.gap();
    rig.advance_to(rig.start_frame() + s(0.3));
    rig.press(Command::PlayStop(0));
    settle(&mut rig);
    assert!(rig.state(0) == LaneState::Stopped && rig.master() > 0 && rejected(&rig, mark) == 0);
    rig.press(Command::ClearAll);
    rig.press(Command::RecDub(0));
    rig.idle();
    rig.advance_to(rig.start_frame() + s(0.3));
    rig.gap();
    rig.press(Command::PlayStop(0));
    settle(&mut rig);
    assert!(rig.state(0) == LaneState::Empty && rig.master() == 0 && !rig.locked() && rejected(&rig, mark) == 1);

    // ── 6: STOP on an armed lane resets the blank session ───────────────────────────────────────
    rig.set(Command::SetFixedLength(true));
    rig.press(Command::RecDub(1));
    rig.advance_to(rig.end_frame() + 1);
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).armed);
    rig.press(Command::Clear(1));
    assert_eq!(rig.master(), expected_master);
    rig.press(Command::Stop(0));
    assert!(rig.master() == 0 && !rig.locked() && (0..5).all(|i| rig.state(i) == LaneState::Empty));

    // ── 6: a stop a hair before the bar line keeps the take (the deferred commit) ────────────────
    rig.set(Command::SetFixedLength(false));
    rig.press(Command::RecDub(0));
    let start = rig.start_frame();
    rig.advance_to(start + expected_master - 2000);
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Recording, "the stop lands inside the window");
    rig.advance_to(start + expected_master + 1);
    assert!(rig.state(0) == LaneState::Stopped && rig.lane(0).length == expected_master, "the take survives, whole bars");

    assert_eq!(rig.engine.diag().events_dropped, 0);
    rig.output.take().unwrap().1
}

#[test]
fn golden_jam_at_44k1_and_48k_bit_identical_across_block_sizes() {
    for sr in [44100, 48000] {
        let reference = jam(sr, 128);
        for block in [1, 32, 64, 127, 480, 1024] {
            let out = jam(sr, block);
            assert_eq!(out.len(), reference.len(), "sr={sr} block={block}");
            let first = out.iter().zip(&reference).position(|(a, b)| a.to_bits() != b.to_bits());
            assert_eq!(first, None, "sr={sr} block={block}: output differs from block 128");
        }
    }
}
