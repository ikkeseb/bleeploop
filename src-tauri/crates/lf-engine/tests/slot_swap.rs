//! The hot plugin swap (`src/slots/swap.rs`, `SlotPort::replace`) driven through the engine: the slots
//! render bit for bit as they did before it outside a swap; the swap fade is exact from the commit
//! frame at any block size, each unit on its own destination and no bypass under it; a candidate stages
//! on the slot's real input; a gain change while it stages moves the playing unit alone; a candidate
//! that refuses to start goes back and leaves the slot untouched; a swap that changes the latency (an
//! empty slot's is 0) waits for a take to stop capturing, whatever the kinds; a commit to another kind
//! takes a live slot off live as GO LIVE off would, answered as one, and no other commit touches the
//! live flag.

mod common;

use common::refs::sha256_f32;
use common::units::{fake, Fake, Probe, Seen};
use common::{code, Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, Event, HandbackReason, NoteTarget, Rejection, SlotKind, SlotReply, Toggle};

/// Frames at 48 kHz of a candidate's staging past its latency, and of the swap fade: 10 ms each; of the
/// live gate's ramp: 5 ms.
const STAGE: Frame = 480;
const SWAP: Frame = 480;
const LIVE: Frame = 240;

/// Two triangle waves, exact in f32: slot 0's input and slot 1's.
fn input_a(f: Frame) -> f32 {
    (f.rem_euclid(200) as f32 - 100.0) / 200.0
}

fn input_b(f: Frame) -> f32 {
    f.rem_euclid(77) as f32 / 154.0
}

/// Installs, removals (one through each crossfade, two serviced at once with no device), notes, and live
/// toggles inside an install fade and a removal fade, on both slots, with no gain change (the glide's
/// coefficient calls the platform's `exp`; everything else here is plain arithmetic). The hash of the
/// monitor, the bus and the record tap.
fn plain_session(block: usize) -> String {
    let (a, b, c, d) = (Probe::new(), Probe::new(), Probe::new(), Probe::new());
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_inputs(input_a, input_b);
    let t = rig.frame;
    let script = [
        (0, Command::SelectInstrument(NoteTarget::Slot(1))),
        (1_100, Command::SetSlotLive(0, false)),
        (1_300, Command::SetSlotLive(0, true)),
        (3_001, Command::NoteOn(60, 1.0)),
        (7_777, Command::NoteOn(62, 0.5)),
        (9_000, Command::NoteOff(60)),
        (12_000, Command::SetSlotLive(1, true)),
        (20_200, Command::SetSlotLive(1, false)),
    ];
    for (at, command) in script {
        rig.send_at(t + at, command);
    }
    rig.keep_output();
    rig.advance_to(t + 1_000);
    rig.install(0, Box::new(Delay::new(57)));
    rig.install(1, Fake::instrument(0.3, 33, &a));
    rig.advance_to(t + 15_000);
    rig.remove(0);
    rig.advance_to(t + 20_000);
    rig.remove(1);
    rig.advance_to(t + 25_000);
    rig.install(0, Fake::effect(0.1, 0.5, &b));
    rig.advance_to(t + 30_000);
    rig.remove(0);
    rig.advance_to(t + 32_000);
    rig.install(0, Fake::effect(0.2, 0.25, &c));
    rig.install(1, Fake::effect(-0.1, 1.0, &d));
    rig.engine.service_slots_idle();
    rig.advance_to(t + 36_000);
    rig.remove(0);
    rig.remove(1);
    rig.engine.service_slots_idle();
    rig.advance_to(t + 40_000);
    let back = [0, 0, 0, 1, 1].into_iter().filter_map(|s| rig.returned(s)).count();
    assert_eq!(back, 5, "every unit came back");
    sha256_f32(&[&rig.monitor, &rig.bus, &rig.record])
}

/// [`plain_session`] at each block size, hashed on the engine before the hot swap (304842ba, Windows).
/// The record tap follows a latent effect's live toggle from the next block start, so each block size
/// has its own hash.
const BEFORE: [(usize, &str); 4] = [
    (128, "15d319118842bfc8d5d03ba9469f919770921a2a3716cfc90dbf7584998bb86b"),
    (1, "18ff445dde631b3c37d97e2c9592203634a38d88f3a2b46f80165525a0fe5b7b"),
    (32, "e02451c4cdf6721f5fa8ab9aa1ed6958e68b5949597927270ad72bfa24f6f1de"),
    (480, "64f9ed725629df78434c7e1802bcaddd336a8725d487af29cf3f76fa4b01e240"),
];

#[test]
fn outside_a_swap_the_slots_render_as_before_it() {
    for (block, hash) in BEFORE {
        assert_eq!(plain_session(block), hash, "block {block}");
    }
}

/// The swap fade `step` frames into its `SWAP` from `from` (the outgoing unit) towards `to`, as the
/// rack's crossfade: exactly either at its ends.
fn crossed(from: f32, to: f32, step: Frame) -> f32 {
    match step {
        ..=0 => from,
        SWAP.. => to,
        k => from + (k as f32 / SWAP as f32) * (to - from),
    }
}

/// `got` holds the frames from `first` on: each one's bits are `want`'s for its frame.
fn assert_bits(got: &[f32], first: Frame, want: impl Fn(Frame) -> f32, what: &str) {
    for (k, &y) in got.iter().enumerate() {
        let f = first + k as Frame;
        assert_eq!(y.to_bits(), want(f).to_bits(), "{what}: frame {f}: {y} vs {}", want(f));
    }
}

/// `frames` rounded up to whole blocks of `b`.
fn whole(frames: Frame, b: Frame) -> Frame {
    (frames + b - 1) / b * b
}

fn bits(x: &[f32]) -> Vec<u32> {
    x.iter().map(|y| y.to_bits()).collect()
}

/// Slot 0's GO LIVE answers (`Event::Toggled`) the rig read from event `mark` on, as (frame, on).
fn live_answers(rig: &Rig, mark: usize) -> Vec<(Frame, bool)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Toggled { frame, toggle: Toggle::Live(0), on } => Some((frame, on)),
            _ => None,
        })
        .collect()
}

/// A swap on slot 0 at block size `block`, the input at `level`: `old` (tag 1) installed before the
/// first block, so engaged at once, then `new` (tag 2) replacing it at `gain` from block start S. It
/// commits at C, the first block start once it has staged its latency plus `STAGE`; checked here:
/// no answer before C, `Committed` from C's block (the live slot taken off live exactly when the kind
/// changes, answered by one `Toggled` off at C, and by none when it does not), the old unit back
/// `ReplacedOut` after the fade. The rig keeps its output from S and has rendered whole blocks past the
/// fade. Returns the rig, S and C.
fn swap(block: usize, level: f32, old: Box<Fake>, new: Box<Fake>, gain: f32) -> (Rig, Frame, Frame) {
    let b = block as Frame;
    let need = new.latency + STAGE;
    let changed = old.kind != new.kind;
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_level(level);
    rig.install_tagged(0, 1, old);
    rig.advance(4 * b);
    let s = rig.frame;
    rig.replace(0, 2, new, gain);
    rig.keep_output();
    let mark = rig.events.len();
    let c = s + whole(need, b);
    rig.advance(c - s);
    assert!(rig.reply(0).is_none(), "block {block}: staging until {c}");
    rig.advance(b);
    let reply = rig.reply(0);
    assert!(matches!(reply, Some(SlotReply::Committed { tag: 2 })), "block {block}: committed at {c}: {reply:?}");
    assert_eq!(rig.engine.looper().overview().slot_live(0), !changed, "block {block}: GO LIVE after the commit");
    rig.advance(whole(SWAP, b));
    match rig.reply(0) {
        Some(SlotReply::Handback(back)) => assert_eq!((back.tag, back.reason), (1, HandbackReason::ReplacedOut), "block {block}"),
        other => panic!("block {block}: the old unit back, not {other:?}"),
    }
    let answers: &[(Frame, bool)] = if changed { &[(c, false)] } else { &[] };
    assert_eq!(live_answers(&rig, mark), answers, "block {block}: the commit's GO LIVE answer");
    (rig, s, c)
}

/// Proof 1: the swap fade lands on the same frames at any block size, counted from the commit frame C
/// = S + ceil(need / B)·B, the old unit at the slot's gain as it stood and the new one at the replace's.
#[test]
fn the_swap_fade_is_bit_exact_from_the_commit_frame_at_any_block_size() {
    for block in [1, 32, 128, 480] {
        let (a, b) = (Probe::new(), Probe::new());
        let (rig, s, c) = swap(block, 0.0, Fake::effect(0.25, 0.0, &a), Fake::effect(-1.0, 0.0, &b).with_latency(100), 0.5);
        // The new unit at 0.5: -0.5 exactly.
        assert_bits(&rig.monitor, s, |f| crossed(0.25, -0.5, f - c), &format!("block {block}"));
        assert!(rig.bus.iter().all(|&x| x == 0.0), "block {block}: an effect swap leaves the bus alone");
        assert_eq!(b.calls()[0].0, s, "block {block}: the candidate processes from S, unheard");
        let (frame, len) = *a.calls().last().unwrap();
        assert!(frame < c + SWAP && c + SWAP <= frame + len as Frame, "block {block}: the old unit's last call ends the fade");
        assert_eq!((a.stops(), a.seen().last()), (1, Some(&Seen::Stop)), "block {block}: stopped once, after its last call");
    }
}

/// Proof 2: mid-swap, each unit's term stays on its own destination (an effect's on the wet, an
/// instrument's on the bus), and the input, live at full scale, is nowhere under the swap.
#[test]
fn mid_swap_each_unit_sounds_on_its_own_destination_and_the_bypass_not_at_all() {
    use SlotKind::{Effect, Instrument};
    let unit = |kind, level, probe: &Probe| match kind {
        Effect => Fake::effect(level, 0.0, probe),
        Instrument => Fake::instrument(level, 0, probe),
    };
    // A unit's level as (wet, bus).
    let on = |kind, y: f32| if kind == Instrument { (0.0, y) } else { (y, 0.0) };
    for (from, to) in [(Effect, Effect), (Instrument, Instrument), (Effect, Instrument), (Instrument, Effect)] {
        let (a, b) = (Probe::new(), Probe::new());
        let (rig, s, c) = swap(128, 1.0, unit(from, 0.25, &a), unit(to, -0.5, &b), 1.0);
        let ((old_wet, old_bus), (wet, bus)) = (on(from, 0.25), on(to, -0.5));
        assert_bits(&rig.monitor, s, |f| crossed(old_wet, wet, f - c), &format!("{from:?} to {to:?}: the wet"));
        assert_bits(&rig.bus, s, |f| crossed(old_bus, bus, f - c), &format!("{from:?} to {to:?}: the bus"));
    }
}

/// The live gate `step` frames into a GO LIVE off from full, as the rack's ramp: silence from its end.
fn ramped_off(step: Frame) -> f32 {
    if step < LIVE {
        1.0 + (0.0 - 1.0) * (step as f32 / LIVE as f32)
    } else {
        0.0
    }
}

/// A commit to another kind takes a live slot off live as a GO LIVE off at its frame would: an identity
/// effect on a constant input, replaced by a silent instrument, fades out over the swap fade on its
/// input ramped from full to silence over the live ramp from the commit frame, frame-exact at a block
/// size that splits the ramp and one that holds it, and nothing of the input passes after the ramp.
#[test]
fn a_commit_to_another_kind_takes_a_live_slot_off_live_as_go_live_off_would() {
    for block in [128, 480] {
        let (a, b) = (Probe::new(), Probe::new());
        let level = 0.5;
        let (rig, s, c) = swap(block, level, Fake::effect(0.0, 1.0, &a), Fake::instrument(0.0, 0, &b), 1.0);
        assert_bits(&rig.monitor, s, |f| if f < c { level } else { crossed(level * ramped_off(f - c), 0.0, f - c) }, &format!("block {block}"));
    }
}

/// A commit to the same kind leaves the live flag alone, and one into a slot not live leaves it so:
/// effect → effect stays live, instrument → effect stays off, neither answered by a `Toggled`.
#[test]
fn a_commit_takes_a_slot_off_live_only_when_live_and_only_to_another_kind() {
    use SlotKind::{Effect, Instrument};
    for (live, from, to) in [(true, Effect, Effect), (false, Instrument, Effect)] {
        let what = format!("{from:?} to {to:?}, live {live}");
        let unit = |kind, probe: &Probe| match kind {
            Effect => Fake::effect(0.25, 1.0, probe),
            Instrument => Fake::instrument(0.25, 0, probe),
        };
        let (p1, p2) = (Probe::new(), Probe::new());
        let mut rig = Rig::with(Opts { block: 128, ..Default::default() });
        rig.queue(Command::SetSlotLive(0, live));
        rig.install_tagged(0, 1, unit(from, &p1));
        rig.advance(4 * 128);
        rig.replace(0, 2, unit(to, &p2), 1.0);
        let mark = rig.events.len();
        let give_up = rig.frame + rig.seconds(1.0);
        let reply = loop {
            rig.advance(128);
            if let Some(reply) = rig.reply(0) {
                break reply;
            }
            assert!(rig.frame < give_up, "{what}: no commit");
        };
        assert!(matches!(reply, SlotReply::Committed { tag: 2 }), "{what}: {reply:?}");
        assert_eq!(rig.engine.looper().overview().slot_live(0), live, "{what}");
        assert_eq!(live_answers(&rig, mark), [], "{what}: no GO LIVE answer");
    }
}

/// A candidate stages on the slot's real input: a delay line its latency long, replacing a silent effect
/// on a frame-coded input, sounds from the commit frame with the input its latency before, frames it
/// took while it staged unheard (under the swap fade: `crossed`).
#[test]
fn a_staging_candidate_takes_the_slots_input_so_it_sounds_from_its_history_at_the_commit() {
    for block in [1, 32, 128, 480] {
        for latency in [100, 700] {
            let b = block as Frame;
            let what = format!("block {block}, latency {latency}");
            let old = Probe::new();
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            rig.set_input(code);
            rig.install_tagged(0, 1, Fake::effect(0.0, 0.0, &old));
            rig.advance(4 * b);
            let s = rig.frame;
            let c = s + whole(latency + STAGE, b);
            rig.replace(0, 2, Box::new(Delay::new(latency)), 1.0);
            rig.keep_output();
            rig.advance_to(c + SWAP + latency + b);
            assert_bits(&rig.monitor, s, |f| if f < c { 0.0 } else { crossed(0.0, code(f - latency), f - c) }, &what);
            assert!(matches!(rig.reply(0), Some(SlotReply::Committed { tag: 2, .. })), "{what}");
            assert!(rig.returned(0).is_some(), "{what}: the old unit back");
        }
    }
}

/// Rule 2: a gain change while a candidate stages moves the unit the slot plays, gliding; at the commit
/// the outgoing unit fades out at the gain the slot had reached on the frame before, frozen, under the
/// candidate at the replace's gain from the commit frame on, the slot's from then on.
#[test]
fn a_gain_change_while_a_candidate_stages_moves_the_playing_unit_and_not_the_swap() {
    for block in [1, 32, 128, 480] {
        let b = block as Frame;
        let (a, p) = (Probe::new(), Probe::new());
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        rig.install_tagged(0, 1, Fake::effect(0.25, 0.0, &a));
        rig.advance(4 * b);
        let s = rig.frame;
        let c = s + whole(100 + STAGE, b);
        rig.replace(0, 2, Fake::effect(-1.0, 0.0, &p).with_latency(100), 0.5);
        rig.keep_output();
        rig.advance(b);
        let g = rig.frame;
        rig.queue(Command::SetSlotGain(0, 0.2));
        rig.advance_to(c + SWAP + 2 * b);
        let at = |f: Frame| rig.monitor[(f - s) as usize];
        assert!((s..g).all(|f| at(f) == 0.25), "block {block}: the old unit at gain 1 until the change");
        // 0.25 gliding towards 0.2 × 0.25, far from it before the commit.
        assert!((g..c).all(|f| at(f) < at(f - 1) && at(f) > 0.1), "block {block}: the playing unit glides from {g}");
        let frozen = at(c - 1);
        assert_bits(&rig.monitor[(c - s) as usize..], c, |f| crossed(frozen, -0.5, f - c), &format!("block {block}: the swap from {c}"));
    }
}

/// Proof 8: a candidate that refuses to start goes back `StartRefused` after its one call, stopped, and
/// the slot renders bit for bit as a rig that never had the replace.
#[test]
fn a_candidate_that_refuses_to_start_goes_back_stopped_and_the_slot_plays_on_untouched() {
    for block in [1, 32, 128, 480] {
        let run = |candidate: Option<&Probe>| {
            let a = Probe::new();
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            rig.set_input(input_a);
            rig.install_tagged(0, 1, Fake::effect(0.1, 0.5, &a));
            rig.keep_output();
            rig.advance(4 * block as Frame);
            if let Some(probe) = candidate {
                rig.replace(0, 2, Fake::effect(0.3, 1.0, probe).refusing(), 0.5);
            }
            rig.advance(2_000);
            (rig, a)
        };
        let refused = Probe::new();
        let (mut rig, a) = run(Some(&refused));
        let (reference, a_ref) = run(None);
        for (got, want, what) in [(&rig.monitor, &reference.monitor, "monitor"), (&rig.bus, &reference.bus, "bus"), (&rig.record, &reference.record, "record")] {
            assert_eq!(bits(got), bits(want), "block {block}: the {what}");
        }
        assert_eq!(a.calls(), a_ref.calls(), "block {block}: the old unit's calls");
        let back = match rig.reply(0) {
            Some(SlotReply::Handback(back)) => back,
            other => panic!("block {block}: the candidate back, not {other:?}"),
        };
        assert_eq!((back.tag, back.reason), (2, HandbackReason::Rejected(Rejection::StartRefused)), "block {block}");
        assert!(rig.reply(0).is_none(), "block {block}: no other answer");
        assert_eq!(refused.calls().len(), 1, "block {block}: back after the call it refused in");
        assert_eq!((refused.stops(), refused.seen().last()), (1, Some(&Seen::Stop)), "block {block}: stopped");
        drop(fake(back.unit));
    }
}

/// Whether a take captures at the next block start, as the engine decides it (its alignment fixed).
fn capturing(rig: &Rig) -> bool {
    rig.window().is_some_and(|(_, start, _)| start.is_some())
}

/// Render one block; whether a take captured at its start.
fn one_block(rig: &mut Rig) -> bool {
    let captured = capturing(rig);
    rig.advance(rig.block as Frame);
    captured
}

/// Proof 9: while a take captures, a candidate with the playing unit's latency commits once staged, and
/// one whose latency differs stays staged (the slot's latency holds) until the first block start after
/// the capture ends.
#[test]
fn a_swap_that_changes_the_latency_waits_while_a_take_captures_and_one_that_keeps_it_does_not() {
    for block in [1, 128, 480] {
        let b = block as Frame;
        let (p1, p2, p3) = (Probe::new(), Probe::new(), Probe::new());
        let mut rig = Rig::with(Opts { block, ..Default::default() });
        rig.install_tagged(0, 1, Fake::effect(0.0, 1.0, &p1).with_latency(64));
        rig.advance(4 * b);
        rig.queue(Command::RecDub(0));
        rig.advance(b);
        assert!(capturing(&rig), "block {block}: the take's alignment is fixed from its arm");

        let s = rig.frame;
        rig.replace(0, 2, Fake::effect(0.0, 1.0, &p2).with_latency(64), 1.0);
        let c = s + whole(64 + STAGE, b);
        loop {
            let at = rig.frame;
            assert!(one_block(&mut rig), "block {block}: still capturing");
            match rig.reply(0) {
                None => assert!(at < c, "block {block}: the same latency commits at {c}"),
                Some(SlotReply::Committed { tag: 2, .. }) => break assert_eq!(at, c, "block {block}"),
                other => panic!("block {block}: {other:?}"),
            }
        }
        while rig.returned(0).is_none() {
            assert!(one_block(&mut rig), "block {block}: still capturing");
        }

        rig.replace(0, 3, Fake::effect(0.0, 1.0, &p3).with_latency(200), 1.0);
        let stop_at = rig.start_frame() + rig.seconds(0.5);
        while rig.frame < stop_at {
            assert!(one_block(&mut rig), "block {block}: still capturing");
            assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 64)), "block {block}: staged while the take captures");
        }
        assert!(rig.reply(0).is_none(), "block {block}: no answer while it waits");
        rig.queue(Command::RecDub(0));
        let give_up = rig.frame + rig.seconds(10.0);
        while one_block(&mut rig) {
            assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 64)), "block {block}: staged while the take captures");
            assert!(rig.frame < give_up, "block {block}: the take never stopped capturing");
        }
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 200)), "block {block}: committed at the first block start after the capture");
        assert!(matches!(rig.reply(0), Some(SlotReply::Committed { tag: 3, .. })), "block {block}");
    }
}

/// Rule 6, the matrix: while a take captures, a candidate commits as it would without one when its
/// latency is the playing unit's (an empty slot's is 0), whatever the two kinds, and otherwise stays
/// staged (an empty slot stays empty) until the first block start after the capture ends. Into an
/// empty slot it is ready as it arrives, beside a unit once it has staged its latency plus `STAGE`.
#[test]
fn a_candidate_waits_for_the_capture_exactly_when_the_latency_changes_whatever_the_kinds() {
    use SlotKind::{Effect, Instrument};
    let unit = |(kind, latency): (SlotKind, Frame), level: f32, probe: &Probe| match kind {
        Effect => Fake::effect(level, 0.0, probe).with_latency(latency),
        Instrument => Fake::instrument(level, latency, probe),
    };
    // (the playing unit, the candidate, whether the capture holds it)
    let cases = [
        (None, (Effect, 100), true),
        (None, (Instrument, 100), true),
        (None, (Effect, 0), false),
        (None, (Instrument, 0), false),
        (Some((Effect, 64)), (Instrument, 64), false),
        (Some((Instrument, 64)), (Effect, 64), false),
        (Some((Effect, 64)), (Instrument, 200), true),
        (Some((Instrument, 64)), (Effect, 0), true),
    ];
    for block in [1, 128, 480] {
        for (from, to, held) in cases {
            let b = block as Frame;
            let what = format!("block {block}: {from:?} to {to:?}");
            let (p1, p2) = (Probe::new(), Probe::new());
            let mut rig = Rig::with(Opts { block, ..Default::default() });
            if let Some(from) = from {
                rig.install_tagged(0, 1, unit(from, 0.0, &p1));
            }
            rig.advance(4 * b);
            rig.queue(Command::RecDub(0));
            rig.advance(b);
            assert!(capturing(&rig), "{what}: the take's alignment is fixed from its arm");
            let s = rig.frame;
            let ready = if from.is_some() { s + whole(to.1 + STAGE, b) } else { s };
            rig.replace(0, 2, unit(to, 0.0, &p2), 1.0);
            // The take is asked to stop a few blocks past `ready`; the engine decides a block's capture
            // before it takes the block's commands, as `capturing` reads it.
            let stop = ready + 4 * b;
            let give_up = stop + rig.seconds(10.0);
            let mut free = None;
            let (at, captured) = loop {
                let at = rig.frame;
                if at == stop {
                    rig.queue(Command::RecDub(0));
                }
                let captured = one_block(&mut rig);
                if !captured {
                    free = free.or(Some(at));
                }
                match rig.reply(0) {
                    None => {
                        assert!(at < give_up, "{what}: no commit");
                        assert_eq!(rig.engine.slot(0), from, "{what}: staged at {at}");
                    }
                    Some(SlotReply::Committed { tag: 2, .. }) => break (at, captured),
                    other => panic!("{what}: {other:?}"),
                }
            };
            if held {
                assert!(at > ready && Some(at) == free, "{what}: committed at {at}, the first block start after the capture {free:?}");
            } else {
                assert_eq!((at, captured), (ready, true), "{what}: committed once ready, the take capturing");
            }
            assert_eq!(rig.engine.slot(0), Some(to), "{what}");
        }
    }
}
