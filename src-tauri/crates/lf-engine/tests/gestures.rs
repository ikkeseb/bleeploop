//! Property tests over gesture scripts (plan § Stage 2 Tests): any sequence of presses, settings and
//! input gaps keeps one recorder, a whole-bar master, every committed lane at the master length (until
//! F14/F16 lift that), finite output and no panic; UNDO twice is the identity; UNDO after an N-cycle
//! overdub gives back the pre-dub loop bit for bit. Rendered at 8 kHz so a case stays cheap; the engine
//! is rate-agnostic.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::{Action, Command, LaneState, TRACK_COUNT};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use std::cell::Cell;

const SR: u32 = 8000;

#[derive(Clone, Debug)]
enum Op {
    Press(Command),
    Wait(Frame),
    Gap,
    /// On the first lane that can undo.
    UndoTwice,
    /// On the first lane that plays forward, for this many loops.
    DubCycles(u8),
}

fn lane() -> impl Strategy<Value = u8> {
    0u8..TRACK_COUNT as u8
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => lane().prop_map(|i| Op::Press(Command::RecDub(i))),
        3 => lane().prop_map(|i| Op::Press(Command::PlayStop(i))),
        1 => lane().prop_map(|i| Op::Press(Command::Stop(i))),
        2 => lane().prop_map(|i| Op::Press(Command::Undo(i))),
        2 => lane().prop_map(|i| Op::Press(Command::Reverse(i))),
        1 => lane().prop_map(|i| Op::Press(Command::Copy(i))),
        1 => lane().prop_map(|i| Op::Press(Command::Clear(i))),
        1 => Just(Op::Press(Command::PlayAll)),
        1 => Just(Op::Press(Command::StopAll)),
        1 => Just(Op::Press(Command::ClearAll)),
        1 => any::<bool>().prop_map(|b| Op::Press(Command::SetFixedLength(b))),
        1 => (1u8..5).prop_map(|b| Op::Press(Command::SetFixedBars(b as f64))),
        1 => any::<bool>().prop_map(|b| Op::Press(Command::SetRetake(b))),
        1 => any::<bool>().prop_map(|b| Op::Press(Command::SetLoopEndStop(b))),
        1 => any::<bool>().prop_map(|b| Op::Press(Command::SetAutoRecord(b))),
        1 => prop_oneof![Just(Action::RecDub), Just(Action::PlayStop), Just(Action::Undo), Just(Action::Clear), Just(Action::NextTrack)]
            .prop_map(|a| Op::Press(Command::Action(a))),
        8 => (1i64..24_000).prop_map(Op::Wait),
        1 => Just(Op::Gap),
        2 => Just(Op::UndoTwice),
        2 => (1u8..4).prop_map(Op::DubCycles),
    ]
}

fn check_invariants(rig: &Rig) {
    let looper = rig.engine.looper();
    let capturing: Vec<usize> =
        (0..TRACK_COUNT).filter(|&i| matches!(rig.state(i), LaneState::Recording | LaneState::Overdubbing)).collect();
    assert!(capturing.len() <= 1, "one recorder: {capturing:?}");
    assert_eq!(capturing.first().copied(), looper.recorder().map(|r| r.0), "the recorder is the capturing lane");
    let master = rig.master();
    if master > 0 {
        assert!(rig.locked(), "a master locks the tempo");
        assert_eq!(master % frames_per_bar(rig.bpm() as f64, SR), 0, "a whole-bar master");
    }
    for i in 0..TRACK_COUNT {
        let info = rig.lane(i);
        match info.state {
            LaneState::Playing | LaneState::Stopped | LaneState::Overdubbing => assert_eq!(info.length, master, "lane {i} at the master"),
            LaneState::Empty => assert_eq!(info.length, 0),
            LaneState::Recording => {}
        }
    }
    if let Some((_, out)) = &rig.output {
        assert!(out.iter().all(|x| x.is_finite()), "finite output");
    }
}

/// Run a script at `block` frames per callback; returns a hash of every rendered sample.
fn run(ops: &[Op], block: usize) -> u64 {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, loop_seconds: 20.0, block, align: 37 });
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    rig.set_input(|f| code(f) - 0.25);
    rig.keep_output();
    for op in ops {
        match *op {
            Op::Press(command) => rig.press(command),
            Op::Wait(frames) => rig.advance(frames),
            Op::Gap => rig.gap(),
            Op::UndoTwice => {
                rig.idle();
                if let Some(i) = (0..TRACK_COUNT).find(|&i| rig.lane(i).can_undo) {
                    let before = rig.pcm(i);
                    rig.press(Command::Undo(i as u8));
                    rig.idle();
                    rig.press(Command::Undo(i as u8));
                    rig.idle();
                    assert_eq!(rig.pcm(i), before, "undo twice is the identity");
                    DEEP_CHECKS.with(|c| c.set(c.get() + 1));
                }
            }
            Op::DubCycles(n) => {
                rig.idle();
                let forward = |i: usize| {
                    let info = rig.lane(i);
                    info.state == LaneState::Playing && !info.reversed && info.stop_at.is_none()
                };
                if let Some(i) = (0..TRACK_COUNT).find(|&i| forward(i)).filter(|_| rig.window().is_none()) {
                    let before = rig.pcm(i);
                    rig.press(Command::RecDub(i as u8));
                    rig.advance(n as Frame * rig.master() + 123);
                    rig.press(Command::RecDub(i as u8));
                    rig.advance(100);
                    rig.idle();
                    rig.press(Command::Undo(i as u8));
                    rig.idle();
                    assert_eq!(rig.pcm(i), before, "undo after a {n}-cycle dub gives back the pre-dub loop");
                    DEEP_CHECKS.with(|c| c.set(c.get() + 1));
                }
            }
        }
        check_invariants(&rig);
        if let Some((start, out)) = rig.output.as_mut() {
            // Keep memory flat: hash what was rendered since the last op, then drop it.
            for x in out.iter() {
                hash = (hash ^ x.to_bits() as u64).wrapping_mul(0x0100_0000_01b3);
            }
            *start += out.len() as Frame;
            out.clear();
        }
    }
    hash
}

/// A committed two-bar loop on lane 0 to start from, so most scripts reach overdub and undo.
fn with_loop(ops: Vec<Op>) -> Vec<Op> {
    let bar = frames_per_bar(120.0, SR);
    [Op::Press(Command::RecDub(0)), Op::Wait(3 * bar), Op::Press(Command::RecDub(0)), Op::Wait(bar)].into_iter().chain(ops).collect()
}

thread_local! {
    static DEEP_CHECKS: Cell<usize> = const { Cell::new(0) };
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn gesture_scripts_keep_the_looper_invariants(start_looping in any::<bool>(), ops in proptest::collection::vec(op(), 1..60)) {
        let ops = if start_looping { with_loop(ops) } else { ops };
        prop_assert_eq!(run(&ops, 64), run(&ops, 127), "the same script renders bit-identically at another block size");
    }
}

#[test]
fn the_deep_checks_are_reached() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    for _ in 0..64 {
        let ops = proptest::collection::vec(op(), 20..40).new_tree(&mut runner).unwrap().current();
        run(&with_loop(ops), 64);
    }
    let ran = DEEP_CHECKS.with(Cell::get);
    assert!(ran >= 20, "undo and dub checks ran {ran} times");
}
