//! DUB FEEDBACK (the owner's night brief, 2026-09-27): a lane setting, 0..1, default 1. While the lane
//! overdubs, each position the dub writes becomes `input + feedback * old`; a lane that does not
//! overdub never changes. 1 is today's sum, bit for bit; 0 replaces what the dub passes over; between,
//! continuous dubbing fades the older layers pass by pass. UNDO still gives back the loop before the dub
//! (its target is taken at dub start), and a second UNDO the faded result. COPY hands the setting on
//! and CLEAR resets it, as they do a lane's volume. The session carries it on the UI side
//! (`session.json`); no web guard precedes this: the Web Audio looper only ever summed.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, LaneState};

/// 8 kHz keeps the loops short; the frame code stays unique.
const SR: u32 = 8000;

/// Lane 0 PLAYING a one-bar loop of the frame code at 200 bpm, the input silent, no job running.
fn looping() -> Rig {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, ..Default::default() });
    rig.set(Command::SetBpm(200.0));
    rig.set_input(code);
    rig.record_first_take(0, 1, 240);
    rig.set_level(0.0);
    rig.idle();
    rig
}

/// The loop position input frame `f` is dubbed onto (the rig's alignment is 0).
fn pos_of(rig: &Rig, f: Frame) -> usize {
    (f - rig.anchor()).rem_euclid(rig.master()) as usize
}

/// Overdub lane 0 from a quarter into the loop for `passes` whole passes, the input an impulse of 1.0 at
/// loop position `at` during the first pass only (the new layer), silent after it. Returns the frame the
/// dub began on.
fn dub(rig: &mut Rig, passes: Frame, at: usize) -> Frame {
    let master = rig.master();
    rig.advance_to(rig.next_boundary() + master / 4);
    let from = rig.frame;
    let (anchor, first) = (rig.anchor(), from + master);
    rig.set_input(move |f| if f < first && (f - anchor).rem_euclid(master) as usize == at { 1.0 } else { 0.0 });
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.advance_to(from + passes * master);
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Playing);
    rig.set_level(0.0);
    rig.idle();
    from
}

/// What a dub pass leaves at each position: `input + fb * old`, in the engine's f32 arithmetic.
fn pass(old: &[f32], fb: f32, input: impl Fn(usize) -> f32) -> Vec<f32> {
    old.iter().enumerate().map(|(p, &o)| input(p) + fb * o).collect()
}

#[test]
fn full_feedback_is_todays_overdub_bit_for_bit() {
    let mut plain = looping();
    let mut set = looping();
    set.set(Command::SetDubFeedback(0, 1.0));
    plain.advance(1); // in step with the frame `set` rendered
    assert_eq!(set.engine.looper().dub_feedback(0), 1.0);
    assert_eq!(plain.engine.looper().dub_feedback(0), 1.0, "the default is 1");
    let before = plain.pcm(0);
    for rig in [&mut plain, &mut set] {
        let master = rig.master();
        rig.advance_to(rig.next_boundary() + master / 3);
        rig.set_input(|f| code(f) * 0.5);
        rig.press(Command::RecDub(0));
        rig.advance(2 * master + 77);
        rig.press(Command::RecDub(0));
        rig.set_level(0.0);
        rig.idle();
    }
    assert_eq!(plain.pcm(0), set.pcm(0), "feedback 1 changes nothing");
    let after = plain.pcm(0);
    let changed = after.iter().zip(&before).filter(|(a, b)| a != b).count();
    assert_eq!(changed, after.len(), "every position was dubbed");
    assert!(after.iter().zip(&before).all(|(a, b)| a > b), "and summed: the old layer is all there");
}

#[test]
fn half_feedback_halves_the_old_layer_each_pass_and_keeps_the_new_one_at_full() {
    let mut rig = looping();
    let old = rig.pcm(0);
    let master = rig.master() as usize;
    let at = master / 2 + 3;
    rig.set(Command::SetDubFeedback(0, 0.5));
    dub(&mut rig, 1, at);
    let one = rig.pcm(0);
    assert_eq!(one, pass(&old, 0.5, |p| if p == at { 1.0 } else { 0.0 }));
    for p in (0..master).filter(|&p| p != at) {
        assert_eq!(one[p], old[p] / 2.0, "one pass: the old layer at half (position {p})");
    }
    assert_eq!(one[at], 1.0 + old[at] / 2.0, "the new impulse at full");

    // Two passes of continuous dub: the old layer at a quarter, the first pass's impulse at half.
    let mut rig = looping();
    rig.set(Command::SetDubFeedback(0, 0.5));
    dub(&mut rig, 2, at);
    let two = rig.pcm(0);
    assert_eq!(two, pass(&one, 0.5, |_| 0.0));
    for p in (0..master).filter(|&p| p != at) {
        assert_eq!(two[p], old[p] / 4.0, "two passes: the old layer at a quarter (position {p})");
    }
    assert_eq!(two[at], (1.0 + old[at] / 2.0) / 2.0);
}

#[test]
fn zero_feedback_replaces_what_the_dub_passed_over_and_keeps_the_rest() {
    let mut rig = looping();
    let old = rig.pcm(0);
    let master = rig.master();
    rig.set(Command::SetDubFeedback(0, 0.0));
    rig.advance_to(rig.next_boundary() + master / 4);
    let from = rig.frame;
    rig.set_input(|f| -code(f));
    rig.press(Command::RecDub(0));
    rig.advance(master / 2 - 1);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    let mut want = old.clone();
    for f in from..from + master / 2 {
        want[pos_of(&rig, f)] = -code(f);
    }
    let got = rig.pcm(0);
    assert_eq!(got, want, "the dub's half holds the new material alone, the other half the old");
    assert_eq!(got.iter().zip(&old).filter(|(g, o)| g == o).count(), (master - master / 2) as usize);
}

#[test]
fn undo_gives_back_the_loop_before_the_dub_and_redo_the_faded_one() {
    let mut rig = looping();
    let old = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.5));
    dub(&mut rig, 2, 5);
    let faded = rig.pcm(0);
    assert!(rig.lane(0).can_undo);
    assert_eq!(rig.engine.looper().undo_pcm(0).unwrap(), old, "the undo target is the loop at dub start");
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), old, "UNDO: the loop before the dub, bit for bit");
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), faded, "a second UNDO: the faded loop again");
}

#[test]
fn a_discarded_layer_restores_the_loop_the_feedback_faded() {
    let mut rig = looping();
    let old = rig.pcm(0);
    let master = rig.master();
    rig.set(Command::SetDubFeedback(0, 0.25));
    rig.advance_to(rig.next_boundary() + master / 4);
    rig.set_level(0.125);
    rig.press(Command::RecDub(0));
    rig.advance(master + master / 2);
    assert_ne!(rig.pcm(0), old);
    rig.press(Command::Stop(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.pcm(0), old, "Stop discards the layer and what it faded");
}

#[test]
fn a_lane_that_does_not_overdub_never_changes() {
    let mut rig = looping();
    let old = rig.pcm(0);
    rig.set(Command::SetDubFeedback(0, 0.0));
    rig.set_level(0.5);
    rig.advance(3 * rig.master());
    rig.press(Command::PlayStop(0));
    rig.advance(rig.master());
    assert_eq!(rig.pcm(0), old, "playing or stopped, the setting alone moves nothing");
}

#[test]
fn copy_hands_the_setting_on_clear_resets_it_and_it_is_clamped() {
    let mut rig = looping();
    rig.set(Command::SetDubFeedback(0, 0.5));
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.engine.looper().dub_feedback(1), 0.5, "COPY takes it, as it takes the volume");
    // The copy's own dub fades its old layer by the copied setting.
    let old = rig.pcm(1);
    let master = rig.master();
    rig.advance_to(rig.next_boundary() + master / 4);
    rig.press(Command::RecDub(1));
    rig.advance(master - 1);
    rig.press(Command::RecDub(1));
    rig.idle();
    assert_eq!(rig.pcm(1), old.iter().map(|x| x / 2.0).collect::<Vec<_>>());
    rig.press(Command::Clear(1));
    assert_eq!(rig.engine.looper().dub_feedback(1), 1.0, "CLEAR resets it, as it resets the volume");
    for (sent, kept) in [(-0.5, 0.0), (1.5, 1.0), (f32::NAN, 1.0), (0.3, 0.3)] {
        rig.set(Command::SetDubFeedback(0, sent));
        assert_eq!(rig.engine.looper().dub_feedback(0), kept, "{sent}");
    }
}
