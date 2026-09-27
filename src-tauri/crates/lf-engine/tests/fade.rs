//! FADE (the owner's night brief, 2026-09-27): a hands-free action on every playing lane. From the press,
//! each fades to silence and stops on a bar line: the first at or after the fade's bars (1, 2, 4 or 8;
//! default 2) from the press, on the click's grid. The fade is its own gain over the lane's volume,
//! which never moves, so PLAY ALL brings the lanes back at their level. A second press stops the
//! fading lanes at once; so do STOP ALL and a lane's PLAY/STOP, as for a loop-end stop. Refused while a
//! lane captures and with nothing playing. No web guard precedes this: the Web Audio looper never faded.

mod common;

use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Action, Command, Event, LaneState, Refusal};

/// 8 kHz keeps the loops short: a bar at 120 bpm is 16000 frames.
const SR: u32 = 8000;
const LEVEL: f32 = 0.25;

/// Lanes 0 and 1 PLAYING one two-bar loop of a constant level at 120 bpm (lane 1 a COPY at half volume),
/// the input silent, every volume glide settled.
fn two_lanes(block: usize) -> Rig {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, block, ..Default::default() });
    rig.set_level(LEVEL);
    rig.record_first_take(0, 2, 240);
    rig.set_level(0.0);
    rig.press(Command::Copy(0));
    rig.set(Command::SetVolume(1, 0.5));
    rig.idle();
    rig.advance(SR as Frame);
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Playing);
    assert!(rig.pcm(0).iter().all(|&x| x == LEVEL));
    rig
}

/// The bar line at or after the current frame.
fn next_bar(rig: &Rig) -> Frame {
    let (anchor, fpb) = (rig.anchor(), rig.fpb());
    anchor + (rig.frame - anchor + fpb - 1).div_euclid(fpb) * fpb
}

fn fade(rig: &mut Rig) {
    rig.press(Command::Action(Action::FadeAll));
}

/// The kept output over device frames `[a, b)`.
fn heard(rig: &Rig, a: Frame, b: Frame) -> &[f32] {
    let (start, out) = rig.output.as_ref().unwrap();
    &out[(a - start) as usize..(b - start) as usize]
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / x.len() as f64).sqrt()
}

/// The frame the feed first reported lane `lane` in `state` at or after `from`.
fn reported(rig: &Rig, lane: u8, state: LaneState, from: Frame) -> Option<Frame> {
    rig.events.iter().find_map(|e| match *e {
        Event::Lane { frame, lane: l, info } if l == lane && info.state == state && frame >= from => Some(frame),
        _ => None,
    })
}

fn refusals(rig: &Rig, from: usize) -> Vec<(u8, Refusal)> {
    rig.events[from..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { lane, reason, .. } => Some((lane, reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn every_playing_lane_fades_to_silence_over_its_bars_and_stops_on_the_bar_line() {
    let mut kept = Vec::new();
    for block in [128, 37] {
        let mut rig = two_lanes(block);
        let fpb = rig.fpb();
        let press = next_bar(&rig);
        rig.advance_to(press);
        rig.keep_output();
        fade(&mut rig);
        let end = press + 2 * fpb;
        for i in 0..2 {
            let info = rig.lane(i);
            assert!(info.fading && info.stop_at == Some(end), "lane {i}: {info:?}");
        }
        rig.advance_to(end + fpb);
        // Every sample is the ramp, `r²` from 1 at the press to 0 at the bar line, over each lane's volume.
        let inv = 1.0 / (end - press) as f64;
        for (k, &y) in heard(&rig, press, end).iter().enumerate() {
            let r = ((end - press - k as Frame) as f64 * inv).clamp(0.0, 1.0);
            let lane = |volume: f64| (volume * (r * r) * LEVEL as f64) as f32;
            assert_eq!(y, lane(1.0) + lane(0.5), "block {block}, frame {k} of the fade");
        }
        assert_eq!(heard(&rig, press, press + 1)[0], 1.5 * LEVEL, "the fade starts at full level");
        let quarters: Vec<f64> = (0..8).map(|q| rms(heard(&rig, press + q * fpb / 4, press + (q + 1) * fpb / 4))).collect();
        println!("block {block}: per-quarter-bar RMS {quarters:.4?}");
        assert!(quarters.windows(2).all(|w| w[1] < w[0]) && quarters[7] > 0.0, "falling every quarter bar");
        let bars = [rms(heard(&rig, press, press + fpb)), rms(heard(&rig, press + fpb, end))];
        assert!(bars[0] < 1.5 * LEVEL as f64 && bars[1] < bars[0] / 4.0, "per bar: {bars:?}");
        assert!(heard(&rig, end, end + fpb).iter().all(|&y| y == 0.0), "silence from the bar line");
        for i in 0..2 {
            assert_eq!(reported(&rig, i, LaneState::Stopped, press), Some(end), "lane {i} STOPPED on the bar line");
            assert!(!rig.lane(i as usize).fading && rig.lane(i as usize).stop_at.is_none());
        }
        assert_eq!(rig.engine.looper().volume(0), (1.0, false), "the stored volumes never moved");
        assert_eq!(rig.engine.looper().volume(1), (0.5, false));
        kept.push(heard(&rig, press, end + fpb).to_vec());
    }
    assert_eq!(kept[0], kept[1], "the same fade at any block size");
}

#[test]
fn play_all_after_a_fade_plays_at_full_level() {
    let mut rig = two_lanes(128);
    let press = next_bar(&rig);
    rig.advance_to(press);
    fade(&mut rig);
    let end = rig.lane(0).stop_at.unwrap();
    rig.advance_to(end + 100);
    rig.keep_output();
    let again = rig.frame;
    rig.press(Command::PlayAll);
    rig.advance(rig.master());
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Playing && !rig.lane(0).fading);
    assert_eq!(rig.anchor(), again, "an idle transport restarts from the top");
    assert!(heard(&rig, again, rig.frame).iter().all(|&y| y == 1.5 * LEVEL), "both lanes at their own level");
}

#[test]
fn a_press_between_bar_lines_ends_on_the_bar_line_the_fades_bars_after_the_next_one() {
    let mut rig = two_lanes(128);
    let fpb = rig.fpb();
    for (sent, kept) in [(0, 1), (3, 2), (4, 4), (7, 4), (100, 8), (1, 1)] {
        rig.set(Command::SetFadeBars(sent));
        assert_eq!(rig.engine.looper().fade_bars(), kept, "{sent} bars");
    }
    let bar = next_bar(&rig);
    rig.advance_to(bar + fpb / 4);
    fade(&mut rig);
    assert_eq!(rig.lane(0).stop_at, Some(bar + 2 * fpb), "one bar after the next bar line: 1.75 bars");
    rig.advance(fpb / 2);
    rig.press(Command::Action(Action::FadeAll)); // the second press: at once
    rig.set(Command::SetFadeBars(8));
    rig.press(Command::PlayAll); // an idle transport: the grid restarts here
    let bar = next_bar(&rig);
    rig.advance_to(bar);
    fade(&mut rig);
    assert_eq!(rig.lane(1).stop_at, Some(bar + 8 * fpb), "a press on a bar line: exactly the fade's bars");
}

#[test]
fn a_second_press_stop_all_or_a_lanes_stop_ends_the_fade_at_once() {
    let mut rig = two_lanes(128);
    fade(&mut rig);
    rig.advance(rig.fpb() / 2);
    rig.keep_output();
    let second = rig.frame;
    fade(&mut rig);
    rig.advance(100);
    for i in 0..2u8 {
        assert_eq!(reported(&rig, i, LaneState::Stopped, second), Some(second), "lane {i}: the second press stops it at once");
    }
    assert!(heard(&rig, second, rig.frame).iter().all(|&y| y == 0.0));

    // With the loop-end stop on, STOP ALL during a fade stops at once, as its second press does.
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayAll);
    fade(&mut rig);
    let now = rig.frame;
    rig.press(Command::StopAll);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, now), Some(now));
    assert_eq!(reported(&rig, 1, LaneState::Stopped, now), Some(now));

    // A lane's PLAY/STOP stops that lane at once; the other fades on.
    rig.press(Command::PlayAll);
    fade(&mut rig);
    let now = rig.frame;
    rig.press(Command::PlayStop(1));
    assert_eq!(reported(&rig, 1, LaneState::Stopped, now), Some(now));
    assert!(rig.lane(0).fading && rig.state(0) == LaneState::Playing);
}

#[test]
fn a_lane_started_during_a_fade_plays_on_at_its_level() {
    let mut rig = two_lanes(128);
    rig.press(Command::PlayStop(1));
    assert_eq!(rig.state(1), LaneState::Stopped);
    let press = next_bar(&rig);
    rig.advance_to(press);
    fade(&mut rig);
    assert!(rig.lane(0).fading && !rig.lane(1).fading);
    rig.advance(rig.fpb() / 2);
    rig.press(Command::PlayStop(1));
    assert!(rig.state(1) == LaneState::Playing && !rig.lane(1).fading, "joins at its level, not fading");
    let end = rig.lane(0).stop_at.unwrap();
    rig.advance_to(end);
    rig.keep_output();
    rig.advance(1000);
    assert_eq!(rig.state(0), LaneState::Stopped);
    assert!(heard(&rig, end, end + 1000).iter().all(|&y| y == 0.5 * LEVEL), "lane 1 alone, at half volume");
}

#[test]
fn fade_is_refused_while_a_lane_captures_and_with_nothing_playing() {
    let mut rig = two_lanes(128);
    rig.set_level(0.125);
    rig.press(Command::RecDub(1));
    assert_eq!(rig.state(1), LaneState::Overdubbing);
    let mark = rig.events.len();
    fade(&mut rig);
    assert_eq!(refusals(&rig, mark), [(1, Refusal::Capturing)], "named on the lane that records");
    assert!(!rig.lane(0).fading && rig.lane(0).stop_at.is_none(), "nothing fades");
    rig.press(Command::RecDub(1));
    rig.set_level(0.0);
    rig.press(Command::StopAll);
    let mark = rig.events.len();
    fade(&mut rig);
    assert_eq!(refusals(&rig, mark), [(0, Refusal::NoFade)], "on the selected lane");
    assert_eq!(Refusal::NoFade.text(), "nothing is playing to fade");
}

#[test]
fn a_fading_lane_refuses_what_a_stopping_one_refuses_and_says_it_fades() {
    let mut rig = two_lanes(128);
    fade(&mut rig);
    let mark = rig.events.len();
    rig.press(Command::Action(Action::RecDub));
    rig.press(Command::Action(Action::Undo));
    assert_eq!(rig.state(0), LaneState::Playing, "no overdub on a fading lane");
    let got = refusals(&rig, mark);
    assert_eq!(got[0], (0, Refusal::Fading));
    assert_eq!(Refusal::Fading.text(), "fading out, wait or stop now");
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Stopped, "a COPY of a fading lane lands STOPPED");
}

/// The pedal's own REVERSE and HALVE (`tests/actions.rs`) refuse a fading lane as they refuse a stopping
/// one, and say it fades; its COPY lands STOPPED, as the on-screen one does.
#[test]
fn a_fading_lanes_pedal_reverse_and_halve_say_it_fades_and_its_copy_lands_stopped() {
    let mut rig = two_lanes(128);
    fade(&mut rig);
    let mark = rig.events.len();
    rig.press(Command::Action(Action::Reverse));
    rig.press(Command::Action(Action::Halve));
    assert_eq!(refusals(&rig, mark), [(0, Refusal::Fading), (0, Refusal::Fading)]);
    assert!(rig.lane(0).fading && !rig.lane(0).reversed && !rig.lane(0).can_undo, "neither acted");
    rig.press(Command::Action(Action::Copy));
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Stopped, "a pedal COPY of a fading lane lands STOPPED");
}

#[test]
fn the_click_stops_where_the_fade_ends() {
    let mut rig = two_lanes(128);
    rig.set(Command::SetMetronome(true));
    let press = next_bar(&rig);
    rig.advance_to(press);
    let mark = rig.events.len();
    fade(&mut rig);
    let end = rig.lane(0).stop_at.unwrap();
    rig.advance_to(end + 2 * rig.fpb());
    let clicks = rig.clicks_since(mark);
    assert_eq!(clicks.len(), 8, "the two bars' beats click: {clicks:?}");
    assert!(clicks.iter().all(|&(f, _)| f < end), "and none from the bar line on");
}
