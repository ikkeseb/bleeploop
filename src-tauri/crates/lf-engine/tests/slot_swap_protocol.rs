//! The hot swap's protocol driven through the engine (the rules: `src/slots.rs`): every unit comes back
//! once, tagged, with why, in send order through a full return ring and through partial reads while
//! its parked overflow wraps around; a replace into an empty slot is an install at its gain; a slot
//! swapping or removing refuses a replace, and one playing an install; a withdraw or a remove
//! mid-staging and mid-fade does only what its tag names, a withdraw that names no staging candidate
//! and a second remove of a unit being removed getting no answer; an eviction mid-swap leaves nothing, nor one after a candidate panicked as it
//! arrived; a candidate stages its latency plus the stage, and the unit it replaces releases its notes
//! in its last calls, a later release of one reaching neither unit. (The fade itself:
//! `tests/slot_swap.rs`.)

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::SeqCst;

use common::units::{Fake, PanicIn, Probe, Seen};
use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::HandbackReason::{Evicted, Rejected, Removed, ReplacedOut};
use lf_engine::Rejection::{Busy, Cancelled, Occupied};
use lf_engine::SlotKind::{Effect, Instrument};
use lf_engine::{Command, HandbackReason, NoteTarget, ProcessContext, SlotKind, SlotProcessor, SlotReply};

/// Frames at 48 kHz of a candidate's staging past its latency, of the swap fade, and of the bypass fade.
const STAGE: Frame = 480;
const SWAP: Frame = 480;
const FADE: Frame = 480;

/// A triangle wave, exact in f32.
fn tri(f: Frame) -> f32 {
    (f.rem_euclid(200) as f32 - 100.0) / 200.0
}

/// `frames` rounded up to whole blocks of `b`.
fn whole(frames: Frame, b: Frame) -> Frame {
    (frames + b - 1) / b * b
}

fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|y| y.to_bits()).collect()
}

/// A reply as the host reads it, its unit set aside.
#[derive(Debug, PartialEq)]
enum Got {
    Committed(u64),
    NotHeld(u64),
    Back(u64, HandbackReason),
}

use Got::{Back, Committed, NotHeld};

/// Every reply on `slot`'s port now, in order; the units handed back go into `units`.
fn answers(rig: &mut Rig, slot: usize, units: &mut Vec<Box<dyn SlotProcessor>>) -> Vec<Got> {
    let mut got = Vec::new();
    while let Some(reply) = rig.reply(slot) {
        got.push(match reply {
            SlotReply::Committed { tag, .. } => Committed(tag),
            SlotReply::NotHeld { tag } => NotHeld(tag),
            SlotReply::Handback(back) => {
                units.push(back.unit);
                Back(back.tag, back.reason)
            }
        });
    }
    got
}

fn stopped_once(p: &Probe) -> bool {
    p.stops() == 1 && p.seen().last() == Some(&Seen::Stop)
}

fn drops(probes: &[&Probe]) -> Vec<usize> {
    probes.iter().map(|p| p.drops.load(SeqCst)).collect()
}

fn unit(kind: SlotKind, level: f32, probe: &Probe) -> Box<Fake> {
    match kind {
        Effect => Fake::effect(level, 0.0, probe),
        Instrument => Fake::instrument(level, 0, probe),
    }
}

/// Proof 4: three replaces, a remove of a gone tag and a withdraw of a staging candidate, then a remove, the
/// host reading nothing until six replies are out: the ring's four, then the two parked behind them,
/// each unit back once with its tag and why, `Committed` before the `ReplacedOut` it causes.
#[test]
fn every_unit_comes_back_once_tagged_with_why_in_send_order_through_a_full_return_ring() {
    for block in [1, 128, 480] {
        let b = block as Frame;
        // From a replace to its old unit back: the stage, the commit's block, the fade, a block more.
        let through = whole(STAGE, b) + whole(SWAP, b) + 2 * b;
        let p: [Probe; 4] = std::array::from_fn(|_| Probe::new());
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        rig.set_level(0.25);
        rig.install_tagged(0, 1, Fake::effect(0.1, 0.5, &p[0]));
        rig.advance(4 * b);
        rig.replace(0, 2, Fake::effect(0.2, 0.5, &p[1]), 1.0);
        rig.advance(through);
        rig.replace(0, 3, Fake::effect(0.3, 0.5, &p[2]), 1.0);
        rig.advance(through);
        rig.remove_tagged(0, 2);
        rig.replace(0, 4, Fake::effect(0.4, 0.5, &p[3]), 1.0);
        rig.advance(b);
        rig.withdraw(0, 4);
        rig.advance(b);
        let mut units = Vec::new();
        let ring = answers(&mut rig, 0, &mut units);
        assert_eq!(ring, [Committed(2), Back(1, ReplacedOut), Committed(3), Back(2, ReplacedOut)], "block {block}: the ring's four");
        rig.advance(b);
        let parked = answers(&mut rig, 0, &mut units);
        assert_eq!(parked, [NotHeld(2), Back(4, Rejected(Cancelled))], "block {block}: the parked two, in order");
        rig.remove_tagged(0, 3);
        rig.advance(whole(FADE, b) + b);
        assert_eq!(answers(&mut rig, 0, &mut units), [Back(3, Removed)], "block {block}");
        rig.advance(b);
        assert_eq!(answers(&mut rig, 0, &mut units), [], "block {block}: nothing more");
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "block {block}: nothing leaked");
        assert!(p.iter().all(stopped_once), "block {block}: each stopped once, after its last call");
        assert_eq!(drops(&[&p[0], &p[1], &p[2], &p[3]]), [0; 4], "block {block}: none dropped by the engine");
        drop(units);
        assert_eq!(drops(&[&p[0], &p[1], &p[2], &p[3]]), [1; 4], "block {block}: each back once");
    }
}

/// The parked overflow keeps send order through partial reads: the ring full and both parked places
/// taken, the host reads one or two replies before each block and the block adds as many, so the
/// oldest parked reply sits at either place in turn (the overflow wraps around) with a newer one parked
/// behind it. Every reply (a remove's `NotHeld` on the empty slot, tagged by its count) arrives in send
/// order, none leaked.
#[test]
fn replies_keep_send_order_through_partial_reads_while_the_parked_overflow_wraps_around() {
    let mut rig = Rig::new();
    rig.advance(128);
    let (mut sent, mut read) = (0, Vec::new());
    let take = |rig: &mut Rig, read: &mut Vec<u64>| match rig.reply(0) {
        Some(SlotReply::NotHeld { tag }) => read.push(tag),
        other => panic!("after {read:?}: {other:?}"),
    };
    // (replies read, removes sent) before each block: the ring's four, the parked two, then the
    // overflow drained one or two at a time as it refills.
    for (reads, sends) in [(0, 4), (0, 2), (1, 1), (1, 1), (1, 1), (2, 2), (1, 1), (2, 1), (0, 0)] {
        for _ in 0..reads {
            take(&mut rig, &mut read);
        }
        for _ in 0..sends {
            sent += 1;
            rig.remove_tagged(0, sent);
        }
        rig.advance(128);
    }
    while read.len() < sent as usize {
        take(&mut rig, &mut read);
        rig.advance(128);
    }
    assert!(rig.reply(0).is_none(), "nothing more");
    assert_eq!(read, (1..=sent).collect::<Vec<_>>(), "in send order");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "nothing leaked");
}

/// Proof 5a: a replace into an empty slot renders bit for bit as an install with a `SetSlotGain` on the
/// same frame, and is answered `Committed`.
#[test]
fn a_replace_into_an_empty_slot_is_an_install_with_its_gain_set_on_the_same_frame() {
    for kind in [Effect, Instrument] {
        for block in [1, 128] {
            let run = |replace: bool| {
                let p = Probe::new();
                let mut rig = Rig::with(Opts { block, ..Default::default() });
                rig.set_input(tri);
                rig.advance(4 * block as Frame);
                rig.keep_output();
                let unit = match kind {
                    Effect => Fake::effect(0.3, 0.5, &p),
                    Instrument => Fake::instrument(0.3, 0, &p),
                };
                if replace {
                    rig.replace(0, 7, unit, 0.5);
                } else {
                    rig.install_tagged(0, 7, unit);
                    rig.send_at(rig.frame, Command::SetSlotGain(0, 0.5));
                }
                rig.advance(2_000);
                let mut units = Vec::new();
                let got = answers(&mut rig, 0, &mut units);
                rig.remove_tagged(0, 7);
                rig.advance(1_000);
                let gone = answers(&mut rig, 0, &mut units);
                (rig, p, got, gone)
            };
            let ((rig, p, got, gone), (reference, p_ref, got_ref, gone_ref)) = (run(true), run(false));
            for (a, b, what) in [(&rig.monitor, &reference.monitor, "monitor"), (&rig.bus, &reference.bus, "bus"), (&rig.record, &reference.record, "record")] {
                assert_eq!(bits(a), bits(b), "{kind:?}, block {block}: the {what}");
            }
            assert_eq!(p.calls(), p_ref.calls(), "{kind:?}, block {block}: the unit's calls");
            assert_eq!((got, got_ref), (vec![Committed(7)], vec![]), "{kind:?}, block {block}: a replace is answered");
            assert_eq!((gone, gone_ref), (vec![Back(7, Removed)], vec![Back(7, Removed)]), "{kind:?}, block {block}");
        }
    }
}

/// Proof 5b: a replace while a candidate stages, while the swap fades or while the unit is removed goes
/// straight back `Busy`, and an install while the slot holds a unit `Occupied`, each counted and never
/// started or stopped; the swap and the removal go on.
#[test]
fn a_slot_swapping_or_removing_refuses_a_replace_busy_and_one_holding_a_unit_an_install_occupied() {
    let b = 128;
    let (old, new) = (Probe::new(), Probe::new());
    let refused: [Probe; 5] = std::array::from_fn(|_| Probe::new());
    let mut rig = Rig::with(Opts { block: b as usize, ..Default::default() });
    let mut units = Vec::new();
    rig.install_tagged(0, 1, Fake::effect(0.1, 0.0, &old));
    rig.advance(4 * b);
    let s = rig.frame;
    rig.replace(0, 2, Fake::effect(0.2, 0.0, &new), 1.0);
    rig.advance(b);
    rig.replace(0, 3, Fake::effect(0.3, 0.0, &refused[0]), 1.0);
    rig.install_tagged(0, 4, Fake::effect(0.4, 0.0, &refused[1]));
    rig.advance(b);
    assert_eq!(answers(&mut rig, 0, &mut units), [Back(3, Rejected(Busy)), Back(4, Rejected(Occupied))], "staging");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 2);
    let c = s + whole(STAGE, b);
    rig.advance_to(c + b);
    assert_eq!(answers(&mut rig, 0, &mut units), [Committed(2)], "the swap goes on");
    rig.replace(0, 5, Fake::effect(0.5, 0.0, &refused[2]), 1.0);
    rig.advance(b);
    assert_eq!(answers(&mut rig, 0, &mut units), [Back(5, Rejected(Busy))], "fading");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 3);
    rig.advance_to(c + whole(SWAP, b) + b);
    assert_eq!(answers(&mut rig, 0, &mut units), [Back(1, ReplacedOut)]);
    rig.remove_tagged(0, 2);
    rig.advance(b);
    rig.replace(0, 6, Fake::effect(0.6, 0.0, &refused[3]), 1.0);
    rig.install_tagged(0, 7, Fake::effect(0.7, 0.0, &refused[4]));
    rig.advance(b);
    assert_eq!(answers(&mut rig, 0, &mut units), [Back(6, Rejected(Busy)), Back(7, Rejected(Occupied))], "removing");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 5);
    rig.advance(whole(FADE, b));
    assert_eq!(answers(&mut rig, 0, &mut units), [Back(2, Removed)], "the removal goes on");
    assert!(refused.iter().all(|p| p.seen().is_empty()), "a refused unit is never started or stopped");
    assert!(stopped_once(&old) && stopped_once(&new));
}

/// The latency of [`scripted`]'s candidate.
const LATENCY: Frame = 100;

/// Slot 0 at `block` on a triangle input: tag 1 playing, and from block start S (unless `replace` is
/// false) tag 2, latent, replacing it at 0.5, so it commits at C = S + whole(`LATENCY` + `STAGE`). `mid`
/// acts on the rig at the block start `at(S, C)`. The rig keeps its output from S and renders well past
/// the swap and a removal. Returns the rig, C and the probes (old, new).
fn scripted(block: usize, replace: bool, at: impl Fn(Frame, Frame) -> Frame, mid: impl FnOnce(&mut Rig)) -> (Rig, Frame, Probe, Probe) {
    let b = block as Frame;
    let (old, new) = (Probe::new(), Probe::new());
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_input(tri);
    rig.install_tagged(0, 1, Fake::effect(0.1, 0.5, &old));
    rig.advance(4 * b);
    let s = rig.frame;
    let c = s + whole(LATENCY + STAGE, b);
    rig.keep_output();
    if replace {
        rig.replace(0, 2, Fake::effect(0.3, 1.0, &new).with_latency(LATENCY), 0.5);
    }
    rig.advance_to(at(s, c));
    assert_eq!(rig.frame, at(s, c), "block {block}: `mid` acts at a block start");
    mid(&mut rig);
    rig.advance_to(c + SWAP + FADE + 4 * b);
    (rig, c, old, new)
}

fn same_output(rig: &Rig, reference: &Rig, what: &str) {
    for (a, b, tap) in [(&rig.monitor, &reference.monitor, "monitor"), (&rig.bus, &reference.bus, "bus"), (&rig.record, &reference.record, "record")] {
        assert_eq!(bits(a), bits(b), "{what}: the {tap}");
    }
}

/// Proof 6: mid-staging, a withdraw of the candidate sends it back `Cancelled` and the slot plays on as
/// if no replace had come; a remove of the playing unit cancels the candidate first, then removes; a
/// remove of the candidate's tag, and any unknown tag, is `NotHeld`, a withdraw of an unknown tag gets
/// no answer, and the swap goes on untouched. Mid-fade, a withdraw of either tag and a remove of the
/// outgoing unit's get no answer, and the swap renders as without them.
#[test]
fn a_withdraw_or_a_remove_mid_swap_does_only_what_its_tag_names() {
    for block in [1, 32, 128, 480] {
        let b = block as Frame;
        let staging = |s: Frame, _: Frame| s + b;
        let mut units = Vec::new();
        let (still, _, _, _) = scripted(block, false, staging, |_| {});
        let (swapped, c_ref, old_ref, _) = scripted(block, true, staging, |_| {});

        let (mut rig, _, old, new) = scripted(block, true, staging, |rig| rig.withdraw(0, 2));
        assert_eq!(answers(&mut rig, 0, &mut units), [Back(2, Rejected(Cancelled))], "block {block}: withdrawn");
        same_output(&rig, &still, &format!("block {block}: withdrawn"));
        assert!(stopped_once(&new), "block {block}: the candidate stopped");
        assert_eq!(rig.engine.slot(0), Some((Effect, 0)), "block {block}: the old unit plays on");
        assert_eq!(old.stops(), 0, "block {block}");

        let (mut rig, _, old, new) = scripted(block, true, staging, |rig| rig.remove_tagged(0, 1));
        assert_eq!(answers(&mut rig, 0, &mut units), [Back(2, Rejected(Cancelled)), Back(1, Removed)], "block {block}: removed");
        assert!(stopped_once(&old) && stopped_once(&new), "block {block}");
        assert_eq!(rig.engine.slot(0), None, "block {block}");

        let (mut rig, c, _, _) = scripted(block, true, staging, |rig| {
            rig.remove_tagged(0, 2);
            rig.remove_tagged(0, 9);
            rig.withdraw(0, 9);
        });
        assert_eq!(c, c_ref);
        same_output(&rig, &swapped, &format!("block {block}: not held"));
        let mut got = answers(&mut rig, 0, &mut units);
        rig.advance(b);
        got.extend(answers(&mut rig, 0, &mut units));
        assert_eq!(got, [NotHeld(2), NotHeld(9), Committed(2), Back(1, ReplacedOut)], "block {block}: not held");

        // A block start inside the fade: none at block 480, whose fade is one block.
        if b < SWAP {
            let fading = |_: Frame, c: Frame| c + b;
            let (mut rig, _, old, _) = scripted(block, true, fading, |rig| {
                rig.withdraw(0, 2);
                rig.withdraw(0, 1);
                rig.remove_tagged(0, 1);
            });
            assert_eq!(answers(&mut rig, 0, &mut units), [Committed(2), Back(1, ReplacedOut)], "block {block}: mid-fade");
            same_output(&rig, &swapped, &format!("block {block}: mid-fade"));
            assert_eq!(old.calls(), old_ref.calls(), "block {block}: the old unit's calls");
        }
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "block {block}");
    }
}

/// Proof 6, the overlap: the new unit removed one block after its commit, so its bypass fade runs over
/// the swap fade (at block 480 they meet end to end). Every unit comes back once, nothing is non-finite,
/// and no frame of the wet or the bus is louder than the louder of the two units' terms.
#[test]
fn the_new_unit_removed_just_after_its_commit_fades_out_within_the_two_units_terms() {
    for (from, to) in [(Effect, Effect), (Instrument, Instrument), (Effect, Instrument), (Instrument, Effect)] {
        for block in [1, 32, 128, 480] {
            let b = block as Frame;
            let (old, new) = (Probe::new(), Probe::new());
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            // The bypass at the slot's gain stays under both terms: 0.07 under 0.25 and 0.7.
            rig.set_level(0.1);
            rig.install_tagged(0, 1, unit(from, 0.25, &old));
            rig.advance(4 * b);
            let c = rig.frame + whole(STAGE, b);
            rig.replace(0, 2, unit(to, -1.0, &new), 0.7);
            rig.keep_output();
            rig.advance_to(c + b);
            rig.remove_tagged(0, 2);
            rig.advance(SWAP + FADE + 2 * b);
            let what = format!("{from:?} to {to:?}, block {block}");
            let mut units = Vec::new();
            assert_eq!(answers(&mut rig, 0, &mut units), [Committed(2), Back(1, ReplacedOut), Back(2, Removed)], "{what}");
            assert!(stopped_once(&old) && stopped_once(&new), "{what}");
            assert_eq!(rig.engine.slot(0), None, "{what}");
            for (tap, x) in [("monitor", &rig.monitor), ("bus", &rig.bus), ("record", &rig.record)] {
                assert!(x.iter().all(|y| y.is_finite()), "{what}: the {tap} is finite");
            }
            for (tap, x) in [("monitor", &rig.monitor), ("bus", &rig.bus)] {
                let (k, peak) = x.iter().enumerate().fold((0, 0.0f32), |m, (k, y)| if y.abs() > m.1 { (k, y.abs()) } else { m });
                assert!(peak <= 0.7, "{what}: the {tap} reaches {peak} at frame {}", c + k as Frame - whole(STAGE, b));
            }
        }
    }
}

/// Rule 5: a remove of a unit already being removed gets no answer of its own, whether it comes in the
/// batch that started the removal or in a later block of its fade, and changes nothing: one `Removed`,
/// and the slot renders as with a single remove.
#[test]
fn a_second_remove_of_a_unit_being_removed_gets_no_answer_and_changes_nothing() {
    for block in [1, 128, 480] {
        let b = block as Frame;
        let run = |again: bool| {
            let p = Probe::new();
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            rig.set_input(tri);
            rig.install_tagged(0, 1, Fake::effect(0.1, 0.5, &p));
            rig.advance(4 * b);
            rig.keep_output();
            let r = rig.frame;
            rig.remove_tagged(0, 1);
            if again {
                rig.remove_tagged(0, 1);
                rig.remove_tagged(0, 1);
            }
            rig.advance(b);
            // Every later block start inside the fade, which ends on frame R + FADE.
            while rig.frame < r + FADE {
                if again {
                    rig.remove_tagged(0, 1);
                }
                rig.advance(b);
            }
            rig.advance(2 * b);
            let mut units = Vec::new();
            let got = answers(&mut rig, 0, &mut units);
            (rig, p, got)
        };
        let ((rig, p, got), (reference, p_ref, _)) = (run(true), run(false));
        assert_eq!(got, [Back(1, Removed)], "block {block}: one answer");
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "block {block}");
        same_output(&rig, &reference, &format!("block {block}"));
        assert_eq!(p.calls(), p_ref.calls(), "block {block}: the unit's calls");
        assert!(stopped_once(&p), "block {block}");
    }
}

/// One block as the device callback renders it, under `catch_unwind`, its guard: false on a panic.
fn guarded_block(rig: &mut Rig) -> bool {
    let n = rig.block;
    let ctx = ProcessContext { frame: rig.frame, xrun: false, damaged: false, align_frames: rig.align - rig.engine.limiter_latency(), input_frames: 0 };
    let (input, mut left, mut right) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let engine = &mut rig.engine;
    let ok = catch_unwind(AssertUnwindSafe(|| engine.process(&ctx, &input, &mut left, &mut right))).is_ok();
    rig.frame += n as Frame;
    ok
}

/// A candidate that panics as it takes the side seat (in `kind` or `latency`) faults the block that
/// took the replace, under the callback's guard; the device side's owner then evicts the engine under
/// its own (`engine_io/owner.rs`: idle servicing, then the eviction), and both units come back once,
/// none dropped: the candidate as a staging one, `Committed`, the old unit `ReplacedOut`, then the
/// candidate `Evicted`.
#[test]
fn a_candidate_that_panics_as_it_arrives_comes_back_with_the_old_unit_at_the_eviction() {
    for at in [PanicIn::Kind, PanicIn::Latency] {
        let (old, new) = (Probe::new(), Probe::new());
        let mut rig = Rig::new();
        rig.install_tagged(0, 1, Fake::effect(0.1, 0.0, &old));
        rig.advance(4 * 128);
        rig.replace(0, 2, Fake::effect(0.2, 0.0, &new).panicking_in(at), 1.0);
        assert!(!guarded_block(&mut rig), "{at:?}: the block that took the replace panicked");
        assert_eq!(drops(&[&old, &new]), [0, 0], "{at:?}: the unwind dropped nothing");
        let engine = &mut rig.engine;
        let evicted = catch_unwind(AssertUnwindSafe(|| {
            engine.service_slots_idle();
            engine.evict_slots();
        }));
        assert!(evicted.is_ok(), "{at:?}: the eviction ran through");
        let mut units = Vec::new();
        assert_eq!(answers(&mut rig, 0, &mut units), [Committed(2), Back(1, ReplacedOut), Back(2, Evicted)], "{at:?}");
        assert!(stopped_once(&old) && stopped_once(&new), "{at:?}");
        assert_eq!(rig.engine.slot(0), None, "{at:?}: nothing left");
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "{at:?}");
        assert_eq!(drops(&[&old, &new]), [0, 0], "{at:?}: none dropped by the engine");
        drop(units);
        assert_eq!(drops(&[&old, &new]), [1, 1], "{at:?}: each back once");
    }
}

/// Proof 7: an eviction with a replace still on the port, mid-staging or mid-fade commits the swap if it
/// has not, and hands back the old unit `ReplacedOut` (its held note released before it stops), then
/// the new one `Evicted`. Nothing is left in the slot or on the port.
#[test]
fn an_eviction_mid_swap_hands_back_the_old_unit_replaced_out_then_the_new_one_evicted() {
    let b = 128;
    for (case, wait) in [("on the port", 0), ("staging", b), ("fading", whole(STAGE, b) + b)] {
        let (old, new) = (Probe::new(), Probe::new());
        let mut rig = Rig::with(Opts { block: b as usize, ..Default::default() });
        rig.queue(Command::SelectInstrument(NoteTarget::Slot(0)));
        rig.install_tagged(0, 1, Fake::instrument(0.2, 0, &old));
        rig.advance(4 * b);
        rig.queue(Command::NoteOn(60, 1.0));
        rig.advance(b);
        rig.replace(0, 2, Fake::instrument(0.3, 0, &new), 1.0);
        rig.advance(wait);
        rig.engine.evict_slots();
        let mut units = Vec::new();
        assert_eq!(answers(&mut rig, 0, &mut units), [Committed(2), Back(1, ReplacedOut), Back(2, Evicted)], "{case}");
        assert_eq!(old.keys(), [(true, 60), (false, 60)], "{case}: the old unit's note released");
        assert!(stopped_once(&old) && stopped_once(&new), "{case}");
        assert_eq!(rig.engine.slot(0), None, "{case}: nothing left");
        rig.engine.service_slots_idle();
        assert_eq!(answers(&mut rig, 0, &mut units), [], "{case}: nothing more");
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "{case}");
        assert_eq!(drops(&[&old, &new]), [0, 0], "{case}");
    }
}

/// Proof 10: a candidate processes, unheard, from the replace's block for at least its latency plus the
/// stage, and commits at the first block start after; the notes the old unit holds then (one struck
/// before the replace, one while the candidate staged) are released on the commit frame, inside its last
/// calls, and the new unit hears only the notes struck after it: their keys' releases after the commit
/// reach neither unit, so no note hangs in either.
#[test]
fn a_candidate_stages_its_latency_plus_the_stage_and_the_old_unit_releases_its_notes_in_its_last_calls() {
    for block in [1, 32, 128, 480] {
        for latency in [0, 100, 1_000] {
            let b = block as Frame;
            let what = format!("block {block}, latency {latency}");
            let (old, new) = (Probe::new(), Probe::new());
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            rig.queue(Command::SelectInstrument(NoteTarget::Slot(0)));
            rig.install_tagged(0, 1, Fake::instrument(0.0, 0, &old));
            rig.advance(4 * b);
            rig.queue(Command::NoteOn(60, 1.0));
            rig.advance(b);
            let s = rig.frame;
            rig.replace(0, 2, Fake::instrument(0.0, latency, &new), 1.0);
            rig.queue(Command::NoteOn(64, 1.0));
            let c = loop {
                let at = rig.frame;
                rig.advance(b);
                match rig.reply(0) {
                    None => assert!(at < s + rig.seconds(1.0), "{what}: no commit"),
                    Some(SlotReply::Committed { tag: 2, .. }) => break at,
                    Some(other) => panic!("{what}: {other:?}"),
                }
            };
            let need = latency + STAGE;
            let staged: Frame = new.calls().iter().filter(|(f, _)| *f < c).map(|&(_, n)| n as Frame).sum();
            assert_eq!(new.calls()[0].0, s, "{what}: it stages from the replace's block");
            assert!(staged >= need && staged < need + b, "{what}: staged {staged} for {need}");
            assert_eq!(c, s + staged, "{what}: committed at the block start after");
            rig.queue(Command::NoteOn(67, 1.0));
            rig.queue(Command::NoteOff(60));
            rig.queue(Command::NoteOff(64));
            rig.advance(whole(SWAP, b) + b);
            assert!(matches!(rig.reply(0), Some(SlotReply::Handback(back)) if (back.tag, back.reason) == (1, ReplacedOut)), "{what}");
            let off: Vec<_> = old.events().into_iter().filter(|(_, _, k)| matches!(k, lf_engine::SlotEventKind::NoteOff { .. })).collect();
            assert_eq!(old.keys(), [(true, 60), (true, 64), (false, 60), (false, 64)], "{what}: the old unit's notes");
            assert!(off.iter().all(|&(f, offset, _)| f == c && offset == 0), "{what}: released on the commit frame: {off:?}");
            let (last, len) = *old.calls().last().unwrap();
            assert!(last < c + SWAP && c + SWAP <= last + len as Frame, "{what}: its calls end with the fade");
            assert!(stopped_once(&old), "{what}");
            assert_eq!(new.keys(), [(true, 67)], "{what}: the new unit's notes");
        }
    }
}
