//! The Share mix (`Engine::share`): the master bus split before the click, under the same master volume,
//! through its own master limiter, plus the monitor. What Share output mirrors, so a stream hears the jam
//! and never the click (count-in, accents, CLICK on), while the device output is untouched.

mod common;

use common::{Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{Command, InputSend, Instrument, LaneState, NoteTarget};
use sha2::{Digest, Sha256};

/// A loud sine: two lanes of it sum past the limiter's threshold, so the master limits.
fn loud(f: Frame) -> f32 {
    0.7 * (f as f32 * std::f32::consts::TAU * 330.0 / 48000.0).sin()
}

/// What the live input plays after the takes: heard on the monitor and through ECHO.
fn guitar(f: Frame) -> f32 {
    0.3 * (f as f32 * std::f32::consts::TAU * 196.0 / 48000.0).sin()
}

/// The jam, in blocks of `block`: a hot take copied to three more lanes, one with its delay on, one with
/// its reverb send, one panned, a lead line, the live input through ECHO, the master volume moved and
/// back, CLICK off and on. With `click` false the click volume is 0, so no click sounds (the count-in
/// included); nothing else changes.
fn jam(block: usize, click: bool) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.keep_output();
    if !click {
        // With the next setting, on the same frame: the jam's frames stay where they are.
        rig.send_at(rig.frame, Command::SetClickVolume(0.0));
    }
    rig.set(Command::SetMetronome(true));
    rig.set_input(loud);
    rig.record_first_take(0, 2, 2400);
    rig.set_input(|_| 0.0);
    for _ in 0..3 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    for command in [
        Command::SetFxParam(1, FxParam::Time, 0.5),
        Command::SetFxParam(1, FxParam::Feedback, 0.6),
        Command::SetFxParam(1, FxParam::Mix, 0.5),
        Command::SetFxBypass(1, FxKind::Delay, false),
        Command::SetFxParam(2, FxParam::Amount, 1.0),
        Command::SetFxBypass(2, FxKind::Reverb, false),
        Command::SetPan(3, -0.8),
        Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)),
    ] {
        rig.set(command);
    }
    rig.advance(rig.seconds(1.0));
    for (k, note) in [64u8, 67, 71, 72].into_iter().enumerate() {
        rig.press(Command::NoteOn(note, 0.9));
        rig.advance(rig.seconds(0.2 + 0.05 * k as f64));
        rig.press(Command::NoteOff(note));
    }
    rig.set_input(guitar);
    rig.set(Command::SetInputSend(InputSend::Echo, true));
    rig.advance(rig.seconds(1.0));
    rig.set(Command::SetMasterVolume(0.4));
    rig.advance(rig.seconds(0.7));
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SetMasterVolume(1.0));
    rig.advance(rig.seconds(1.3));
    rig.set(Command::SetMetronome(true));
    rig.set_input(|_| 0.0);
    // The limiter's recovery after the last hot frames.
    rig.press(Command::StopAll);
    rig.advance(rig.seconds(1.5));
    rig
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// The first frame where `a` and `b` differ, if any (-0.0 and 0.0 are the same sample).
fn first_difference(a: &[f32], b: &[f32]) -> Option<usize> {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn sha256(channels: &[&[f32]]) -> String {
    let mut hash = Sha256::new();
    for x in channels.iter().flat_map(|c| c.iter()) {
        hash.update(x.to_le_bytes());
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The device output before the Share mix existed: the jam's at 128 frames, hashed on its parent commit
/// (Windows: the synths, the click and the limiter call the platform's maths library, so Linux renders
/// other last bits). The same bits are the same timing: the play path's latency did not move.
const DEVICE_BEFORE: &str = "221f8992bf5fc5bae9ef8c7598b51b300ebb1b4a08522743bfd372a79b111720";

#[test]
#[cfg_attr(not(windows), ignore = "the pinned hash is the Windows render's")]
fn the_device_output_is_unchanged() {
    let rig = jam(128, true);
    assert!(rig.bus.iter().any(|x| x.abs() > 1.0), "the jam is hot: the limiter limits");
    assert_eq!(sha256(&[&rig.heard, &rig.heard_right]), DEVICE_BEFORE);
}

/// The click at its loudest: the count-in with CLICK off (it clicks anyway), then CLICK on over a silent
/// loop, accents and all. Every click reaches the device output, a pre-delay late; the Share mix stays
/// exactly silent.
#[test]
fn the_share_mix_holds_no_click_and_the_device_output_does() {
    let mut rig = Rig::new();
    // Past the limiter's start-up dip (`Engine::new`).
    rig.advance(rig.seconds(0.5));
    rig.keep_output();
    let start = rig.frame;
    rig.set(Command::SetMetronome(false));
    rig.record_first_take(0, 1, 2400);
    rig.set(Command::SetMetronome(true));
    rig.advance(rig.seconds(4.0));
    let beats = rig.beats();
    assert!(beats.iter().filter(|b| b.2 > 0 && b.3).count() == 4, "the count-in clicks with CLICK off");
    let clicks = rig.clicks_since(0);
    assert!(clicks.len() >= 12 && clicks.iter().filter(|c| c.1).count() >= 3, "{clicks:?}");
    let latency = rig.engine.limiter_latency();
    let window = rig.seconds(0.07) as usize;
    for (frame, accent) in clicks {
        let k = (frame + latency - start) as usize;
        let want = 0.5 * if accent { 1.0 } else { 0.56 } * 0.7;
        for side in [&rig.heard, &rig.heard_right] {
            assert!(peak(&side[k..k + window]) > want, "the click at {frame} (accent {accent}) is heard");
        }
    }
    assert_eq!(rig.share.len(), rig.heard.len());
    assert!(rig.share.iter().chain(&rig.share_right).all(|&x| x == 0.0), "the Share mix carries no click");
}

/// The Share mix is the jam with the click at 0, bit for bit, both sides: the lanes, the FX, the pan,
/// the lead, the live input and its echo, the master volume's moves, a hot mix the limiter holds down and
/// its recovery after the stop. Both limiters hear the same bus from the first frame. The device output
/// with the click differs from it.
#[test]
fn the_share_mix_is_the_jam_with_the_click_at_zero_bit_for_bit() {
    let (with, without) = (jam(128, true), jam(128, false));
    assert!(without.bus.iter().any(|x| x.abs() > 1.0), "the jam is hot: the limiter limits");
    assert!(first_difference(&with.heard, &without.heard).is_some(), "the device output carries the click");
    assert_eq!(first_difference(&with.share, &without.heard), None, "left");
    assert_eq!(first_difference(&with.share_right, &without.heard_right), None, "right");
    assert_eq!(first_difference(&without.share, &without.heard), None, "with no click, the Share mix is the device output");
    assert!(first_difference(&with.share, &with.share_right).is_some(), "the pan reaches the Share mix");
}

/// The same commands give the same Share mix at any block size, bit for bit.
#[test]
fn the_share_mix_is_the_same_at_any_block_size() {
    let reference = jam(128, true);
    for block in [1, 64, 127, 480, 1024] {
        let rig = jam(block, true);
        assert_eq!(first_difference(&rig.share, &reference.share), None, "block {block}, left");
        assert_eq!(first_difference(&rig.share_right, &reference.share_right), None, "block {block}, right");
    }
}

/// An impulse on a lane and on the live input lands on the same frame in the Share mix as in the device
/// output: the lane a pre-delay late in both (the limiters' is the same), the input at once in both.
#[test]
fn an_impulse_lands_on_the_same_frame_in_the_share_mix_as_in_the_device_output() {
    let mut rig = Rig::new();
    rig.keep_output();
    let start = rig.frame;
    rig.set(Command::SetMetronome(true));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let take = rig.start_frame();
    // Half a beat into the take, clear of the click's 70 ms.
    let hit = take + rig.seconds(0.25);
    rig.set_input(move |f| if f == hit { 0.6 } else { 0.0 });
    rig.advance_to(take + rig.fpb() + 2400);
    let stop = rig.frame;
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Playing);
    let at = rig.pcm(0).iter().position(|x| x.abs() > 0.3).expect("the take holds the impulse") as Frame;
    let (anchor, master) = (rig.anchor(), rig.master());
    let live = anchor + master * ((stop - anchor) / master + 1) + rig.seconds(0.1);
    rig.set_input(move |f| if f == live { 0.6 } else { 0.0 });
    rig.advance_to(live + master);
    assert!(rig.clicks_since(mark).len() > 4, "the click runs throughout");
    let latency = rig.engine.limiter_latency();
    let found = |x: &[f32]| x.iter().enumerate().filter(|(_, s)| s.abs() > 0.3).map(|(k, _)| start + k as Frame).collect::<Vec<_>>();
    // The lane's passes from the stop on, each a pre-delay late; the input's impulses as they came.
    let mut want: Vec<Frame> = (0..4).map(|k| anchor + k * master + at + latency).filter(|&f| f > stop && f < rig.frame).collect();
    want.extend([hit, live]);
    want.sort();
    assert!(want.len() >= 4, "{want:?}");
    for side in [&rig.share, &rig.share_right] {
        assert_eq!(found(side), want, "the Share mix: the take's input, the lane, the input");
    }
    for side in [&rig.heard, &rig.heard_right] {
        for &frame in &want {
            assert!(side[(frame - start) as usize].abs() > 0.3, "the device output at {frame}");
        }
    }
}
