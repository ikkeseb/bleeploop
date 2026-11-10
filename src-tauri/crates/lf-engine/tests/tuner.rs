//! The tuner's engine side (`lf_engine::tuner`, `engine.rs`): off in a new engine; its press
//! (`Action::Tuner`) switches it from the engine's value and its setter (`Command::SetTuner`) sets it
//! outright, each change answered once by `Event::Tuner`, owed while the full event ring refuses it; it
//! mutes the monitor alone, gliding on its exact frame, so loops, the click and the record tap are
//! untouched and the output is the same at any block size; it and a take never coexist (a press over a
//! take is refused, a take that comes to exist closes it on that frame and records exactly as with no
//! tuner); a stopped device closes it; its tap hands both slots' raw input to a reader, frame by frame,
//! with an epoch that changes wherever that input breaks. A new guard, not a port.

mod common;

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::tuner::{tuner_capacity, TunerFrame};
use lf_engine::{Action, Command, Event, LaneState, Refusal, Toggle};

const SR: f64 = 48000.0;

fn tuner() -> Command {
    Command::Action(Action::Tuner)
}

/// The monitor's glide coefficient: a 2 ms time constant at 48 kHz (`engine.rs` `MONITOR_TAU_SECONDS`).
fn coef() -> f64 {
    (-1.0 / (0.002 * SR)).exp()
}

fn ms(frames: usize) -> f64 {
    frames as f64 * 1000.0 / SR
}

fn sine(f: Frame) -> f32 {
    (0.5 * (std::f64::consts::TAU * 440.0 * f as f64 / SR).sin()) as f32
}

/// The `Tuner` events since event `mark`: (frame, on).
fn tuner_since(rig: &Rig, mark: usize) -> Vec<(Frame, bool)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Tuner { frame, on } => Some((frame, on)),
            _ => None,
        })
        .collect()
}

fn refusals_since(rig: &Rig, mark: usize) -> Vec<Refusal> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
}

fn without_tuner(events: &[Event]) -> Vec<Event> {
    events.iter().copied().filter(|e| !matches!(e, Event::Tuner { .. })).collect()
}

/// The largest step between neighbouring samples.
fn largest_step(x: &[f32]) -> f64 {
    x.windows(2).map(|w| (w[1] as f64 - w[0] as f64).abs()).fold(0.0, f64::max)
}

/// A playing one-bar loop on lane 0 (the tempo locked), the input silent.
fn looping() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig
}

#[test]
fn off_in_a_new_engine_a_press_switches_it_and_the_setter_changes_it_once() {
    let mut rig = Rig::new();
    rig.advance(256);
    assert!(!rig.engine.tuner(), "a new engine starts off");
    assert!(tuner_since(&rig, 0).is_empty(), "and says nothing of it");
    let mark = rig.events.len();
    let on_at = rig.frame;
    rig.press(tuner());
    assert!(rig.engine.tuner());
    rig.advance(100);
    let off_at = rig.frame;
    rig.press(Command::ActionOn(3, Action::Tuner));
    assert!(!rig.engine.tuner(), "a press on a named lane is the same press");
    assert_eq!(tuner_since(&rig, mark), [(on_at, true), (off_at, false)], "each press answered on its frame");

    let mark = rig.events.len();
    rig.press(Command::SetTuner(false));
    rig.press(Command::SetTuner(false));
    assert!(tuner_since(&rig, mark).is_empty(), "off and off again changes nothing");
    let set_on = rig.frame;
    rig.press(Command::SetTuner(true));
    rig.press(Command::SetTuner(true));
    assert!(rig.engine.tuner());
    let set_off = rig.frame;
    rig.press(Command::SetTuner(false));
    rig.press(Command::SetTuner(false));
    assert!(!rig.engine.tuner());
    assert_eq!(tuner_since(&rig, mark), [(set_on, true), (set_off, false)], "one change and one event each");
}

/// A 0.5 sine on the input, slot 0 live, the output kept from frame 52800; `script` from there.
fn sine_run(block: usize, script: impl FnOnce(&mut Rig)) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_input(sine);
    rig.advance(4800);
    rig.keep_output();
    script(&mut rig);
    rig
}

#[test]
fn it_mutes_the_monitor_alone_without_a_click_and_lands_exactly() {
    let total = 14_000;
    let reference = sine_run(128, |rig| rig.advance(total));
    let (i0, i1) = (1037usize, 1037usize + 4800);
    let run = sine_run(128, |rig| {
        rig.send_at(rig.frame + i0 as Frame, tuner());
        rig.send_at(rig.frame + i1 as Frame, tuner());
        rig.advance(total);
    });
    let (m, r) = (&run.monitor, &reference.monitor);
    assert!(r.iter().any(|&x| x.abs() > 0.49), "the unmuted monitor carries the sine");
    assert_eq!(m[..i0], r[..i0], "untouched before the press");

    // Muted: exactly 0 from some frame on, to the second press.
    let silent_from = (i0..i1).rev().find(|&k| m[k] != 0.0).map_or(i0, |k| k + 1);
    let mute = silent_from - i0;
    assert!(mute <= 1344, "exactly silent within 28 ms of the press ({:.1} ms)", ms(mute));
    assert!(m[i0 + 720..i1].iter().all(|x| x.abs() <= 0.5e-3), "-60 dB within 15 ms");
    // Back: the unmuted signal exactly, from some frame on.
    let back_from = (i1..total as usize).rev().find(|&k| m[k] != r[k]).map_or(i1, |k| k + 1);
    let unmute = back_from - i1;
    assert!(unmute <= 1344, "back to the unmuted signal exactly within 28 ms ({:.1} ms)", ms(unmute));
    assert!(back_from < total as usize - 1000);
    println!("tuner glide at 48 kHz: exactly silent {:.2} ms after the press, exactly back {:.2} ms after the release", ms(mute), ms(unmute));

    // No click: no step larger than the sine's own plus the gain's largest step times its amplitude.
    let bound = largest_step(r) + 0.5 * (1.0 - coef()) + 1e-6;
    assert!(largest_step(m) <= bound, "largest step {} against {bound}", largest_step(m));
    assert_eq!(run.record, reference.record, "the record tap is not muted");
}

#[test]
fn loops_and_the_click_play_on_bit_for_bit() {
    let run = |press: bool| {
        let mut rig = looping();
        rig.set(Command::SetMetronome(true));
        rig.set_input(sine);
        rig.keep_output();
        if press {
            rig.send_at(rig.frame + 500, tuner());
            rig.send_at(rig.frame + 30_500, tuner());
        }
        rig.advance(48_000);
        rig
    };
    let (a, b) = (run(true), run(false));
    assert!(a.bus.iter().any(|&x| x.abs() > 1e-3), "the loop and the click play");
    assert_ne!(a.monitor, b.monitor, "the tuner muted the monitor");
    assert_eq!(a.bus, b.bus, "the master bus (left) without the monitor");
    assert_eq!(a.bus_right, b.bus_right, "the master bus (right) without the monitor");
}

#[test]
fn the_mute_starts_on_its_exact_frame_at_any_block_size() {
    let script = |rig: &mut Rig| {
        let at = rig.frame;
        rig.send_at(at + 1037, tuner());
        rig.send_at(at + 1537, tuner()); // mid-glide down
        rig.send_at(at + 4537, Command::SetTuner(true));
        rig.send_at(at + 9001, Command::SetTuner(false));
        rig.advance(12_000);
    };
    let runs: Vec<Rig> = [1, 64, 128, 480].into_iter().map(|block| sine_run(block, script)).collect();
    for rig in &runs[1..] {
        assert_eq!(rig.heard, runs[0].heard, "block {}", rig.block);
        assert_eq!(rig.heard_right, runs[0].heard_right, "block {}", rig.block);
        assert_eq!(rig.monitor, runs[0].monitor, "block {}", rig.block);
        assert_eq!(rig.record, runs[0].record, "block {}", rig.block);
    }
    let reference = sine_run(128, |rig| rig.advance(12_000));
    let (m, r) = (&runs[2].monitor, &reference.monitor);
    assert_eq!(m[..1037], r[..1037], "untouched up to the press frame");
    assert_eq!(m[1037], (r[1037] as f64 * coef()) as f32, "the gain moves on the press frame itself");
}

/// Press the tuner and its setter over a take: refused, nothing switched, no `Tuner` event.
fn refused_over_a_take(mut rig: Rig, what: &str) {
    assert!(rig.window().is_some(), "{what}: a take exists");
    let mark = rig.events.len();
    rig.press(tuner());
    assert!(!rig.engine.tuner(), "{what}: stays off");
    assert_eq!(refusals_since(&rig, mark), [Refusal::TunerCapturing], "{what}");
    assert!(tuner_since(&rig, mark).is_empty(), "{what}: no Tuner event");
    let mark = rig.events.len();
    rig.press(Command::SetTuner(true));
    assert!(!rig.engine.tuner(), "{what}: the setter leaves it off");
    assert!(refusals_since(&rig, mark).is_empty() && tuner_since(&rig, mark).is_empty(), "{what}: and says nothing");
}

#[test]
fn a_press_over_any_take_is_refused() {
    assert_eq!(Refusal::TunerCapturing.text(), "a take is recording, the tuner opens after it");

    let mut rig = Rig::new();
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).armed);
    refused_over_a_take(rig, "counting in");

    let mut rig = Rig::new();
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(2.5));
    assert!(rig.state(0) == LaneState::Recording && !rig.lane(0).armed);
    refused_over_a_take(rig, "recording");

    let mut rig = looping();
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    refused_over_a_take(rig, "overdubbing");

    let mut rig = looping();
    rig.press(Command::RecDub(1));
    assert!(rig.lane(1).armed, "a later take waits for the boundary");
    refused_over_a_take(rig, "a later take armed");

    let mut rig = Rig::new();
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).auto_armed);
    refused_over_a_take(rig, "AUTO listening");
}

/// Run `setup`, the tuner on (or a frame with nothing), then `take` on its frame, then `finish`; the
/// tuner must close on the take's frame and the take must record as it does with no tuner.
fn a_take_closes_it(what: &str, setup: impl Fn() -> Rig, take: Command, finish: impl Fn(&mut Rig), lane: usize) {
    let run = |with_tuner: bool| {
        let mut rig = setup();
        rig.advance(300);
        if with_tuner {
            rig.press(tuner());
            assert!(rig.engine.tuner(), "{what}: on before the take");
        } else {
            rig.advance(1);
        }
        rig.advance(200);
        let (at, mark) = (rig.frame, rig.events.len());
        rig.press(take);
        assert!(rig.window().is_some(), "{what}: the take exists");
        assert!(!rig.engine.tuner(), "{what}: the tuner closed");
        finish(&mut rig);
        (rig, at, mark)
    };
    let (a, at, mark) = run(true);
    let (b, ..) = run(false);
    assert_eq!(tuner_since(&a, mark), [(at, false)], "{what}: closed on the take's frame");
    assert_eq!(without_tuner(&a.events), without_tuner(&b.events), "{what}: the take's events");
    assert!(a.window().is_none() && a.state(lane) != LaneState::Empty, "{what}: a committed loop");
    let pcm = a.pcm(lane);
    assert!(pcm.iter().any(|&x| x != 0.0), "{what}: it recorded");
    assert_eq!(pcm, b.pcm(lane), "{what}: the recorded loop, sample for sample");
}

#[test]
fn a_take_that_comes_to_exist_closes_it_on_its_frame_and_records_as_without_it() {
    let fresh = || {
        let mut rig = Rig::new();
        rig.set_input(code);
        rig
    };
    let stop_after = |seconds: f64| {
        move |rig: &mut Rig| {
            rig.advance(rig.seconds(seconds));
            let lane = rig.window().unwrap().0 as u8;
            rig.press(Command::RecDub(lane));
            rig.advance(rig.seconds(2.5));
            rig.idle();
        }
    };
    a_take_closes_it("a count-in on an empty lane", fresh, Command::RecDub(0), stop_after(4.05), 0);
    let playing = || {
        let mut rig = looping();
        rig.set_input(code);
        rig
    };
    a_take_closes_it("an overdub on a playing loop", playing, Command::RecDub(0), stop_after(2.5), 0);
    a_take_closes_it("a later take on a second lane", playing, Command::RecDub(1), stop_after(4.5), 1);
    let auto = || {
        let mut rig = Rig::new();
        rig.set(Command::SetAutoRecord(true));
        rig.set_input(|f| if f < 52_000 { 0.0 } else { 0.2 + code(f) / 16.0 });
        rig
    };
    a_take_closes_it("AUTO REC's arm", auto, Command::RecDub(0), stop_after(3.0), 0);
    let retake = || {
        let mut rig = fresh();
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.set(Command::SetRetake(true));
        rig
    };
    a_take_closes_it("a RETAKE roll", retake, Command::RecDub(0), stop_after(7.0), 0);

    // The tuner opened and a take started on one frame: the check runs after that frame's commands.
    let mut rig = fresh();
    rig.advance(300);
    let (at, mark) = (rig.frame, rig.events.len());
    rig.send_at(at, Command::SetTuner(true));
    rig.send_at(at, Command::RecDub(0));
    rig.advance(1);
    assert!(!rig.engine.tuner() && rig.window().is_some());
    assert_eq!(tuner_since(&rig, mark), [(at, true), (at, false)]);
}

#[test]
fn no_frame_taps_the_tuner_while_a_take_exists_through_scheduled_starts_and_passes() {
    // A one-bar loop at 240 BPM (48000 frames), RETAKE on, then a later take on lane 1: armed by a
    // press, started on the boundary with no command there, rolling pass after pass, stopped mid-pass.
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(240.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    assert_eq!(master, 48_000);
    rig.idle();
    rig.set(Command::SetRetake(true));
    rig.press(tuner());
    rig.read_tuner();
    rig.block = 1;
    let mut took = 0;
    let mut check = |rig: &mut Rig, frames: Frame| {
        for _ in 0..frames {
            let f = rig.frame;
            rig.advance(1);
            let pushed = rig.read_tuner();
            if rig.window().is_some() {
                assert!(!rig.engine.tuner(), "the tuner is off while a take exists (frame {f})");
                assert!(pushed.is_empty(), "nothing tapped on a frame a take exists (frame {f})");
            } else if rig.engine.tuner() {
                assert_eq!(pushed.iter().map(|t| t.frame).collect::<Vec<_>>(), [f], "tapped on its frame");
                took += 1;
            }
        }
    };
    check(&mut rig, 1000);
    let mark = rig.events.len();
    let armed_at = rig.frame;
    rig.press(Command::RecDub(1));
    assert!(rig.lane(1).armed && !rig.engine.tuner());
    let start = rig.start_frame();
    for _ in 0..4 {
        check(&mut rig, 9000);
        rig.press(tuner());
    }
    let rolled = start + 2 * master + master / 2 - rig.frame;
    check(&mut rig, rolled);
    assert!(rig.lane(1).retake_pass >= 3, "the roll passed scheduled edges");
    rig.press(Command::RecDub(1));
    let after = rig.seconds(0.5);
    check(&mut rig, after);
    rig.block = 128;
    rig.idle();
    assert!(rig.window().is_none() && rig.state(1) == LaneState::Playing);
    assert_eq!(tuner_since(&rig, mark), [(armed_at, false)], "closed once, by the arm");
    assert_eq!(refusals_since(&rig, mark).iter().filter(|&&r| r == Refusal::TunerCapturing).count(), 4);
    rig.press(tuner());
    rig.read_tuner();
    rig.block = 1;
    check(&mut rig, 500);
    assert!(rig.engine.tuner() && took >= 1000 + 499, "on again after the take, and tapping");
}

#[test]
fn a_stopped_device_closes_it_and_a_full_ring_owes_the_event() {
    let mut rig = Rig::new();
    rig.advance(256);
    rig.press(tuner());
    rig.advance(100);
    let mark = rig.events.len();
    rig.punch_out();
    assert!(!rig.engine.tuner());
    assert_eq!(tuner_since(&rig, mark), [(rig.frame, false)], "said at once when the ring has room");

    // The ring full at the stop: the idle publish delivers it once the feed drained the ring.
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    rig.advance(128);
    rig.read_events();
    let on_at = rig.frame;
    let mark = rig.events.len();
    rig.press(tuner());
    for lane in [1, 2, 3, 4, 0] {
        rig.queue(Command::SelectTrack(lane));
    }
    rig.advance(128);
    assert!(rig.engine.tuner());
    rig.engine.punch_out(); // the ring still full
    assert!(!rig.engine.tuner());
    rig.read_events();
    assert_eq!(tuner_since(&rig, mark), [(on_at, true)], "the close refused at the stop");
    for _ in 0..4 {
        rig.engine.publish_idle();
        rig.read_events();
    }
    assert_eq!(tuner_since(&rig, mark), [(on_at, true), (rig.frame, false)], "delivered by the idle publish, once");

    // The ring full over three switches: one event, at the value now.
    let mut rig = Rig::with_event_capacity(Opts::default(), 4);
    rig.hold_events = true;
    rig.advance(128);
    let mark = rig.events.len();
    rig.press(tuner());
    rig.press(tuner());
    rig.press(tuner());
    assert!(rig.engine.tuner());
    rig.read_events();
    assert!(tuner_since(&rig, mark).is_empty(), "all three refused");
    for _ in 0..16 {
        rig.read_events();
        rig.advance(128);
    }
    rig.read_events();
    assert_eq!(tuner_since(&rig, mark).iter().map(|e| e.1).collect::<Vec<_>>(), [true], "one event, at the value now");
    assert!(rig.engine.diag().events_dropped > 0);
}

fn input_a(f: Frame) -> f32 {
    sine(f)
}

fn input_b(f: Frame) -> f32 {
    code(f) - 0.25
}

/// Frames contiguous, one epoch, each the raw inputs of both slots.
fn assert_raw(frames: &[TunerFrame], what: &str) {
    assert!(!frames.is_empty(), "{what}");
    for t in frames {
        assert_eq!(t.x, [input_a(t.frame), input_b(t.frame)], "{what}: frame {}", t.frame);
        assert_eq!(t.epoch, frames[0].epoch, "{what}: one epoch");
    }
    assert!(frames.windows(2).all(|w| w[1].frame == w[0].frame + 1), "{what}: contiguous");
}

#[test]
fn the_tap_hands_on_both_raw_inputs_with_an_epoch_that_breaks_where_they_break() {
    let mut rig = Rig::new();
    rig.set_inputs(input_a, input_b);
    rig.advance(128 * 40);
    assert!(rig.read_tuner().is_empty(), "off: nothing");
    assert!(!rig.engine.toggles()[Toggle::Live(1).index()], "slot 1 is not live");

    let on_at = rig.frame + 37;
    rig.send_at(on_at, tuner());
    rig.advance(128 * 10);
    let frames = rig.read_tuner();
    assert_eq!(frames[0].frame, on_at, "from the press frame, mid-block");
    assert_eq!(frames.last().unwrap().frame, rig.frame - 1);
    assert_raw(&frames, "on");
    let first = frames[0].epoch;

    let off_at = rig.frame + 50;
    rig.send_at(off_at, tuner());
    rig.advance(500);
    assert_eq!(rig.read_tuner().last().unwrap().frame, off_at - 1, "to the frame before the release");
    rig.advance(500);
    assert!(rig.read_tuner().is_empty());
    rig.press(tuner());
    rig.advance(500);
    let frames = rig.read_tuner();
    assert_raw(&frames, "on again");
    assert_eq!(frames[0].epoch, first + 1, "off then on: a new epoch");

    // A jump in the device frame with no xrun reported, an xrun, a damaged block.
    let mut epoch = frames[0].epoch;
    for (what, before) in [("a jump", (|rig: &mut Rig| rig.frame += 1000) as fn(&mut Rig)), ("an xrun", Rig::gap), ("a damaged block", Rig::damage)] {
        rig.advance(128 - rig.frame % 128);
        let from = rig.frame;
        let tail = rig.read_tuner();
        assert!(tail.iter().all(|t| t.epoch == epoch), "{what}: before it, one epoch");
        before(&mut rig);
        let at = rig.frame;
        rig.advance(128);
        let frames = rig.read_tuner();
        assert_eq!(frames[0].frame, at, "{what}");
        assert_raw(&frames, what);
        assert_eq!(frames[0].epoch, epoch + 1, "{what}: a new epoch from its first frame (block at {from})");
        epoch += 1;
    }
    rig.advance(128);
    assert_eq!(rig.read_tuner()[0].epoch, epoch + 1, "the frames after a damaged block start another");
    epoch += 1;

    // A reader that does not drain: the ring refuses, counts, and the frames after carry a new epoch.
    assert_eq!(rig.engine.diag().tuner_dropped, 0);
    let rendered = rig.seconds(0.5);
    rig.advance(rendered);
    let capacity = tuner_capacity(48_000);
    assert_eq!(rig.engine.diag().tuner_dropped, rendered as u64 - capacity as u64);
    let kept = rig.read_tuner();
    assert_eq!(kept.len(), capacity, "the ring keeps the oldest");
    assert_raw(&kept, "the kept frames");
    assert_eq!(kept[0].epoch, epoch);
    rig.advance(128);
    let after = rig.read_tuner();
    assert_raw(&after, "after the drain");
    assert_eq!(after[0].epoch, epoch + 1, "a new epoch after the drop");
}

/// Lanes 0 and 1 playing the same 4-bar loop, lane 0 selected.
fn two_lanes() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 4, 2400);
    rig.set_level(0.0);
    rig.idle();
    rig.press(Command::Copy(0));
    rig.idle();
    rig.press(Command::SelectTrack(0));
    rig
}

/// A pedal's CLEAR on lane 0, then `between`, then CLEAR again: did it clear?
fn clears_past(between: &[Command]) -> bool {
    let mut rig = two_lanes();
    rig.press(Command::Action(Action::Clear));
    for &command in between {
        rig.press(command);
    }
    rig.idle();
    rig.press(Command::Action(Action::Clear));
    rig.state(0) == LaneState::Empty
}

#[test]
fn its_press_is_a_looper_press_and_its_setter_is_not() {
    assert!(clears_past(&[]), "CLEAR, CLEAR clears");
    assert!(!clears_past(&[tuner()]), "CLEAR, TUNER, CLEAR asks again");
    assert!(!clears_past(&[tuner(), tuner()]), "CLEAR, TUNER twice, CLEAR asks again");
    assert!(!clears_past(&[Command::ActionOn(3, Action::Tuner)]));
    assert!(clears_past(&[Command::SetTuner(true)]), "a setting alone disarms nothing");
}

#[test]
fn its_press_waits_behind_a_held_looper_command() {
    // A 16-bar loop (32 s) on lane 0, copied to lane 1: the copy job runs for some 31 ms.
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set_level(0.5);
    rig.record_first_take(0, 16, 2400);
    rig.set_level(0.0);
    rig.set(Command::SetMute(0, true));
    rig.advance(4800);
    rig.press(Command::SelectTrack(0));
    rig.press(Command::Copy(0));
    rig.press(Command::PlayStop(1));
    assert!(rig.engine.holding(), "PLAY on the copy waits for its job");
    rig.press(tuner());
    assert!(rig.engine.holding() && !rig.engine.tuner(), "the press waits in order");
    rig.idle();
    assert!(rig.engine.tuner(), "then switches");
}
