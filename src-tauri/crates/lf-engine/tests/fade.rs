//! FADE (the owner's night brief, 2026-09-27): a hands-free action on every playing lane. From the press,
//! each fades to silence and stops on a bar line: the first at or after the fade's bars (1, 2, 4 or 8;
//! default 2) from the press, on the click's grid. The fade is its own gain over the lane's volume,
//! which never moves, so PLAY ALL brings the lanes back at their level. A second press stops the
//! fading lanes at once; so do STOP ALL and a lane's PLAY/STOP, as for a loop-end stop (each lane's 5 ms
//! tail, D23, starting at the level the fade had reached). Refused while a
//! lane captures and with nothing playing. The lane's FX returns fall with it (its delay's feedback takes
//! the ramp): on the final output, past the bar line only the tail of the faded loop rings on, far below
//! what a plain stop there leaves; a stop at once leaves them ringing as STOP ALL does, and a lane
//! started during the fade keeps its returns whole. No web guard precedes this: the Web Audio looper
//! never faded.

mod common;

use common::dub::ramp;
use common::edges::{level, sample};
use common::{Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
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

/// The FADE's `r` at frame `f`, for a fade pressed at `from` that ends at `to`: the lane plays at `r²`.
fn fade_r(from: Frame, to: Frame, f: Frame) -> f64 {
    ((to - f) as f64 * (1.0 / (to - from).max(1) as f64)).clamp(0.0, 1.0)
}

/// A lane at `volume` stopped at `stop` while it faded (pressed at `from`, ending at `to`), at frame `f`
/// (D23): its 5 ms tail starts at the level the fade had reached, `r²`, and falls to silence.
fn tail_from_fade(volume: f64, (from, to): (Frame, Frame), stop: Frame, f: Frame) -> f32 {
    let (n, r) = (ramp(SR), fade_r(from, to, stop));
    if f >= stop + n { 0.0 } else { sample(volume, level(r * r, 0.0, f - stop, n), LEVEL, 0.0) }
}

#[test]
fn a_second_press_stop_all_or_a_lanes_stop_ends_the_fade_at_once() {
    // At once: STOPPED on the press, from where each lane's 5 ms tail (D23) starts at the level the fade
    // had reached, never back at full.
    let mut rig = two_lanes(128);
    let pressed = rig.frame;
    fade(&mut rig);
    let fading = (pressed, rig.lane(0).stop_at.unwrap());
    rig.advance(rig.fpb() / 2);
    rig.keep_output();
    let second = rig.frame;
    fade(&mut rig);
    rig.advance(100);
    for i in 0..2u8 {
        assert_eq!(reported(&rig, i, LaneState::Stopped, second), Some(second), "lane {i}: the second press stops it at once");
    }
    let r = fade_r(fading.0, fading.1, second);
    assert!(r > 0.6 && r < 0.9, "half a bar into a two-bar fade: {r}");
    for (k, &y) in heard(&rig, second, rig.frame).iter().enumerate() {
        let f = second + k as Frame;
        assert_eq!(y, tail_from_fade(1.0, fading, second, f) + tail_from_fade(0.5, fading, second, f), "the second press: frame {f}");
    }

    // With the loop-end stop on, STOP ALL during a fade stops at once, as its second press does.
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayAll);
    let pressed = rig.frame;
    fade(&mut rig);
    let fading = (pressed, rig.lane(0).stop_at.unwrap());
    let now = rig.frame;
    rig.press(Command::StopAll);
    rig.advance(100);
    assert_eq!(reported(&rig, 0, LaneState::Stopped, now), Some(now));
    assert_eq!(reported(&rig, 1, LaneState::Stopped, now), Some(now));
    for (k, &y) in heard(&rig, now, rig.frame).iter().enumerate() {
        let f = now + k as Frame;
        assert_eq!(y, tail_from_fade(1.0, fading, now, f) + tail_from_fade(0.5, fading, now, f), "STOP ALL: frame {f}");
    }

    // A lane's PLAY/STOP stops that lane at once; the other fades on.
    rig.press(Command::PlayAll);
    let pressed = rig.frame;
    fade(&mut rig);
    let fading = (pressed, rig.lane(0).stop_at.unwrap());
    let now = rig.frame;
    rig.press(Command::PlayStop(1));
    rig.advance(100);
    assert_eq!(reported(&rig, 1, LaneState::Stopped, now), Some(now));
    assert!(rig.lane(0).fading && rig.state(0) == LaneState::Playing);
    for (k, &y) in heard(&rig, now, rig.frame).iter().enumerate() {
        let f = now + k as Frame;
        let r = fade_r(fading.0, fading.1, f);
        let lane0 = ((r * r) * LEVEL as f64) as f32;
        assert_eq!(y, lane0 + tail_from_fade(0.5, fading, now, f), "lane 1's STOP: frame {f}");
    }
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

// ── The lane's FX returns: its delay and its reverb send ─────────────────────────────────────────────

/// A deterministic noise, within ±`level`.
fn noise(f: Frame, level: f32) -> f32 {
    let h = (f as u64 ^ 0x5DEE_CE66D).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((h >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * level
}

/// Lane `lane`'s delay on at feedback 0.95 and half wet, and its reverb send full.
fn engage_returns(rig: &mut Rig, lane: u8) {
    rig.set(Command::SetFxBypass(lane, FxKind::Delay, false));
    rig.set(Command::SetFxParam(lane, FxParam::Feedback, 0.95));
    rig.set(Command::SetFxParam(lane, FxParam::Mix, 0.5));
    rig.set(Command::SetFxBypass(lane, FxKind::Reverb, false));
    rig.set(Command::SetFxParam(lane, FxParam::Amount, 1.0));
}

/// Lane 0 PLAYING a two-bar loop of noise at 120 bpm through its delay and reverb (`engage_returns`),
/// four bars on so the echoes have built up, the input silent; the output kept from here.
fn with_returns() -> Rig {
    let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, ..Default::default() });
    rig.set_input(|f| noise(f, 0.05));
    rig.record_first_take(0, 2, 240);
    rig.set_level(0.0);
    engage_returns(&mut rig, 0);
    rig.idle();
    rig.advance(4 * rig.fpb());
    rig.advance_to(next_bar(&rig));
    rig.keep_output();
    rig
}

/// The final output (the left channel, after the limiter) over device frames `[a, b)`.
fn out(rig: &Rig, a: Frame, b: Frame) -> &[f32] {
    let start = rig.output.as_ref().unwrap().0;
    &rig.heard[(a - start) as usize..(b - start) as usize]
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.log10()
}

#[test]
fn a_fade_takes_the_lanes_delay_and_reverb_down_with_it_and_leaves_only_their_tail() {
    let mut faded = with_returns();
    let mut stopped = with_returns();
    let (press, fpb) = (faded.frame, faded.fpb());
    let end = press + 2 * fpb;
    fade(&mut faded);
    assert_eq!(faded.lane(0).stop_at, Some(end));
    // The twin stops plainly on the same bar line: its returns ring on with the loop at full level.
    stopped.advance_to(end);
    stopped.press(Command::PlayStop(0));
    for rig in [&mut faded, &mut stopped] {
        rig.advance_to(end + 2 * fpb);
    }
    let before = rms(out(&stopped, press, end));
    let bars = [rms(out(&faded, press, press + fpb)), rms(out(&faded, press + fpb, end))];
    let tail = [rms(out(&faded, end, end + fpb)), rms(out(&faded, end + fpb, end + 2 * fpb))];
    let full = rms(out(&stopped, end, end + fpb));
    println!(
        "unfaded {before:.5}; fading bars {bars:.5?}; after the bar line [{:.2e}, {:.2e}] ({:.1} dB below a plain stop's {full:.5})",
        tail[0],
        tail[1],
        -db(tail[0] / full)
    );
    assert!(bars[0] < before && bars[1] < bars[0] / 4.0, "the whole lane falls through the fade");
    assert!(full > before / 4.0, "a plain stop leaves the returns ringing");
    assert!(db(tail[0] / full) < -30.0, "a fade leaves only the tail of what had faded: {:.1} dB", db(tail[0] / full));
    assert!(tail[0] > 0.0 && tail[1] < tail[0], "a tail, dying away");
}

#[test]
fn a_second_press_stops_the_returns_input_as_stop_all_does_and_they_ring_on_from_where_they_were() {
    let mut second = with_returns();
    let mut stop_all = with_returns();
    let fpb = second.fpb();
    for rig in [&mut second, &mut stop_all] {
        fade(rig);
        rig.advance(fpb / 2);
    }
    let now = second.frame;
    fade(&mut second);
    stop_all.press(Command::StopAll);
    for rig in [&mut second, &mut stop_all] {
        rig.advance(fpb);
    }
    assert_eq!(second.state(0), LaneState::Stopped);
    assert_eq!(out(&second, now, now + fpb), out(&stop_all, now, now + fpb), "the returns as STOP ALL leaves them");
    let quarter = fpb / 4;
    let (last, next) = (rms(out(&second, now - quarter, now)), rms(out(&second, now, now + quarter)));
    println!("the quarter bar before the second press {last:.5}, after it {next:.5}");
    assert!(next > 0.0 && next < last, "they ring on, and nothing comes back louder");
}

#[test]
fn a_lane_started_during_a_fade_keeps_its_returns_whole() {
    // Lane 0 fades (muted from the start, so it and its FX never sound); lane 1, started during the fade
    // with its own delay and reverb, sounds bit for bit as it does when lane 0 simply stops on the
    // fade's bar line.
    let twin = |fades: bool| {
        let mut rig = Rig::with(Opts { sr: SR, start: SR as Frame, ..Default::default() });
        rig.set(Command::SetMute(0, true));
        rig.set_input(|f| noise(f, 0.05));
        rig.record_first_take(0, 2, 240);
        rig.set_level(0.0);
        rig.press(Command::Copy(0));
        rig.idle();
        rig.press(Command::PlayStop(1));
        rig.set(Command::SetMute(1, false));
        engage_returns(&mut rig, 1);
        rig.advance_to(next_bar(&rig));
        rig.keep_output();
        let end = rig.frame + 2 * rig.fpb();
        if fades {
            fade(&mut rig);
            assert_eq!(rig.lane(0).stop_at, Some(end));
        } else {
            rig.send_at(end, Command::PlayStop(0));
            rig.advance(1);
        }
        rig.advance(rig.fpb() / 2);
        rig.press(Command::PlayStop(1));
        assert!(rig.state(1) == LaneState::Playing && !rig.lane(1).fading);
        rig.advance_to(end + 2 * rig.fpb());
        assert_eq!(rig.state(0), LaneState::Stopped);
        rig.heard.clone()
    };
    let (fading, plain) = (twin(true), twin(false));
    assert!(rms(&fading) > 0.0);
    assert!(fading.iter().zip(&plain).all(|(a, b)| a.to_bits() == b.to_bits()), "lane 1 untouched by lane 0's fade");
}
