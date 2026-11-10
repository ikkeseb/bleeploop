//! The listening sheet (`pnpm listen`, `verify/README.md` § The listening sheet): short clips rendered
//! from the real engine through the test rig, each answering one `STATUS.md` question the ear still owes
//! about the engine's own sound (an edge that clicks, a fade's curve, a delay against the click). Every
//! clip is an ignored test, so `pnpm test:engine` never runs one; `scripts/listen.mjs` runs them with
//! `$LF_LISTEN_OUT` set and builds the page. A clip is the device output (after the limiter, with the
//! live monitor), never normalised.
//!
//! The player is a synthetic plucked string (`listen/pluck.rs`) played in time with what it hears: the
//! rig's record alignment is the limiter's pre-delay alone, what the app uses on a device with no
//! latency of its own, so a take plays back exactly where it was played. What no clip can tell: the
//! owner's guitar and amp, latency, the driver, the pedals and the feel of playing.
//!
//! Add a clip: one `#[test] #[ignore]` function here that drives a [`Rig`], marks the moments its question
//! points at ([`Clip::mark`]) and ends in [`Clip::finish`] with its [`Meta`].

mod common;
#[path = "listen/pluck.rs"]
mod pluck;
#[path = "listen/sheet.rs"]
mod sheet;

use std::rc::Rc;

use common::Rig;
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::{Frame, Grid, Meter};
use lf_engine::{Action, Command, Event, InputSend, LaneState};
use pluck::{Note, DOT, LAYER, RIFF_A, RIFF_A2, RIFF_B, RIFF_B2, RIFF_C, SIX_EIGHT, STABS};
use serde_json::json;
use sheet::{Clip, Meta};

const SR: u32 = 48_000;
/// A phrase's peak: some 10 dB under full scale, so a few lanes and the monitor still fit.
const PEAK: f32 = 0.3;

/// An eighth note at `bpm`, in frames.
fn eighth(bpm: f64) -> f64 {
    SR as f64 * 60.0 / bpm / 2.0
}

fn secs(s: f64) -> Frame {
    (s * SR as f64).round() as Frame
}

/// A fresh engine at `bpm` in 4/4, its record alignment the limiter's pre-delay, past the master volume's
/// glide in from silence.
fn rig_at(bpm: f64) -> Rig {
    let mut rig = Rig::new();
    rig.align = rig.engine.limiter_latency();
    rig.set(Command::SetBpm(bpm));
    rig.advance(secs(0.5));
    rig
}

/// `p` repeated from output frame `origin` on, silent before it.
fn looped(p: Rc<Vec<f32>>, origin: Frame) -> impl Fn(Frame) -> f32 + 'static {
    move |f| if f < origin { 0.0 } else { p[(f - origin) as usize % p.len()] }
}

/// `p` once from output frame `origin`.
fn from(p: Rc<Vec<f32>>, origin: Frame) -> impl Fn(Frame) -> f32 + 'static {
    move |f| if f < origin { 0.0 } else { p.get((f - origin) as usize).copied().unwrap_or(0.0) }
}

/// `notes` moved `by` units later.
fn shifted(notes: &[Note], by: f64) -> Vec<Note> {
    notes.iter().map(|n| Note { at: n.at + by, ..*n }).collect()
}

/// `notes` moved `by` units later and `semis` semitones up.
fn moved(notes: &[Note], by: f64, semis: u8) -> Vec<Note> {
    notes.iter().map(|n| Note { at: n.at + by, midi: n.midi + semis, ..*n }).collect()
}

/// `p` once from each of `origins`: one rendered phrase played again, the same samples each time.
fn again(p: Rc<Vec<f32>>, origins: Vec<Frame>) -> impl Fn(Frame) -> f32 + 'static {
    move |f| origins.iter().filter(|&&o| f >= o).map(|&o| p.get((f - o) as usize).copied().unwrap_or(0.0)).sum()
}

/// The frame lane `lane` first reports PLAYING at since event `since`: where a take commits.
fn commits(rig: &Rig, since: usize, lane: u8) -> Frame {
    rig.events[since..]
        .iter()
        .find_map(|e| match e {
            Event::Lane { frame, lane: l, info } if *l == lane && info.state == LaneState::Playing => Some(*frame),
            _ => None,
        })
        .expect("the take never committed")
}

/// REC on an empty looper's `lane`: the count-in, then `p` played from the downbeat as heard, for `bars`
/// bars, stopped just past the bar line; the input silent once the take has committed. The downbeat.
fn first_take(rig: &mut Rig, lane: u8, bars: Frame, p: Rc<Vec<f32>>) -> Frame {
    let press = rig.frame;
    rig.press(Command::RecDub(lane));
    let m = rig.meter();
    let downbeat = Grid::tempo(press, 0, rig.bpm(), rig.sr, m).beat_frame(m.pulses_per_bar());
    rig.set_input(looped(p, downbeat + rig.align));
    rig.advance_to(downbeat + bars * rig.fpb() + 2400);
    rig.press(Command::RecDub(lane));
    rig.advance(secs(0.25));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!((rig.state(lane as usize), rig.master()), (LaneState::Playing, bars * rig.fpb()));
    downbeat
}

/// A later take on `lane` over the playing loop, `p` (one loop long) played in time from the boundary it
/// arms on, stopped just past one pass: one loop, committed at the press. The window's first input frame.
fn later_take(rig: &mut Rig, lane: u8, p: Rc<Vec<f32>>) -> Frame {
    rig.press(Command::RecDub(lane));
    let start = rig.start_frame();
    let m = rig.master();
    assert_eq!(p.len() as Frame, m);
    rig.set_input(looped(p, start));
    rig.advance_to(start + m + 2400);
    rig.press(Command::RecDub(lane));
    rig.advance(secs(0.25));
    rig.set_level(0.0);
    rig.idle();
    assert!(rig.state(lane as usize) == LaneState::Playing && rig.lane(lane as usize).length == m, "lane {lane} holds one loop");
    start
}

/// An overdub on `lane` pressed at `from` and closed at `to`, `p` (one loop long) played in time.
fn overdub(rig: &mut Rig, lane: u8, p: Rc<Vec<f32>>, from: Frame, to: Frame) {
    let (lag, anchor) = (rig.align, rig.anchor());
    rig.set_input(move |f| if f >= from + lag && f <= to + lag { p[(f - lag - anchor).rem_euclid(p.len() as Frame) as usize] } else { 0.0 });
    rig.send_at(from, Command::RecDub(lane));
    rig.send_at(to, Command::RecDub(lane));
    rig.advance_to(to + secs(0.1));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(lane as usize), LaneState::Playing);
}

/// The first frame from `from` where lane `i` plays at least 0.7 of its loudest: mid-note.
fn loud(rig: &Rig, i: usize, from: Frame) -> Frame {
    let (pcm, anchor, master) = (rig.pcm(i), rig.anchor(), rig.master());
    let top = pcm.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    (from..from + master).find(|&f| pcm[(f - anchor).rem_euclid(master) as usize].abs() >= 0.7 * top).unwrap()
}

fn riff(notes: &[Note], bars: f64, bpm: f64, peak: f32) -> Rc<Vec<f32>> {
    pluck::periodic(notes, eighth(bpm), SR, 8.0 * bars, peak)
}

// ── Clicks and seams ─────────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn punch_edges() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 120.0, PEAK));
    rig.advance_to(rig.next_boundary());
    let (b, m, lag) = (rig.frame, rig.master(), rig.align);
    // A double stop held for 1.9 s, ringing long: the DUB goes in half a second into it and out a second later.
    let note = pluck::once(&[pluck::held(0.0, 57, 1.9, 6.0), pluck::held(0.0, 64, 1.9, 6.0)], SR as f64, SR, secs(2.3) as usize, PEAK);
    let mut clip = Clip::begin(&mut rig);
    rig.set_input(from(note, b + lag + secs(0.25)));
    let (punch_in, punch_out) = (b + secs(0.75), b + secs(1.75));
    rig.send_at(punch_in, Command::RecDub(0));
    rig.send_at(punch_out, Command::RecDub(0));
    rig.advance_to(b + 2 * m + secs(0.6));
    assert_eq!(rig.state(0), LaneState::Playing);
    clip.mark("DUB in", punch_in);
    clip.mark("DUB out", punch_out);
    clip.mark("dub starts, played back", punch_in + m);
    clip.mark("dub ends, played back", punch_out + m);
    clip.finish(
        &rig,
        Meta {
            id: "punch-edges",
            group: 0,
            order: 0,
            title: "Punch in and out over a held note",
            question: "A note is held while DUB goes in and out. On the next pass the dubbed note starts and stops mid-note: at those two marks, any click or thump?",
            cannot_tell: "A synthetic pluck, not your guitar through the amp; nothing about latency or the pedal's feel.",
            status: "Next jam 1: \"The amp-sim live, and takes on the click.\" (its 5 ms edges: a DUB punched in and out over a held note)",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "dub_feedback": 1.0, "edge_ramp_ms": 5 }),
        },
    );
}

#[test]
#[ignore]
fn stop_play() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_B, 1.0, 120.0, 0.22));
    later_take(&mut rig, 1, riff(&RIFF_A, 1.0, 120.0, 0.25));
    let (b, m) = (rig.next_boundary(), rig.master());
    overdub(&mut rig, 1, riff(&LAYER, 1.0, 120.0, 0.2), b + m / 4, b + m + m / 4);
    rig.advance_to(rig.next_boundary());
    let b = rig.frame;
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(loud(&rig, 1, b + secs(0.6)));
    let stop = rig.frame;
    rig.press(Command::PlayStop(1));
    assert_eq!(rig.state(1), LaneState::Stopped);
    rig.advance_to(loud(&rig, 1, b + secs(1.5)));
    let play = rig.frame;
    rig.press(Command::PlayStop(1));
    assert_eq!(rig.state(1), LaneState::Playing);
    rig.advance_to(b + secs(2.6));
    let undo = rig.frame;
    rig.press(Command::Undo(1));
    let switch = b + 2 * m;
    rig.advance_to(switch + secs(1.4));
    clip.mark("STOP", stop);
    clip.mark("PLAY", play);
    clip.mark("UNDO pressed", undo);
    clip.mark("UNDO heard", switch);
    clip.finish(
        &rig,
        Meta {
            id: "stop-play",
            group: 0,
            order: 1,
            title: "STOP, PLAY and UNDO mid-note",
            question: "Lane 2 is stopped, played again, then its last overdub undone (heard from the loop's 1, where a note still rings). At STOP, PLAY and the undo, any click?",
            cannot_tell: "A synthetic pluck over a second loop; nothing about how quickly the pedal answers.",
            status: "Next jam 1: \"The amp-sim live, and takes on the click.\" (its 5 ms edges: UNDO, STOP and PLAY a lane mid-note)",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "lanes": 2, "loop_end_stop": false, "edge_ramp_ms": 5 }),
        },
    );
}

#[test]
#[ignore]
fn trim_swap() {
    // Eight bars at 240 fit the clip; the riff moves in quarters, so it sounds as 120 does in eighths.
    // Bars 5 to 8 are bars 1 to 4 a fourth up, so the trimmed loop and the whole one differ by ear.
    let mut rig = rig_at(240.0);
    let eight_bars: Vec<Note> = [&RIFF_A[..], &RIFF_A2[..], &moved(&RIFF_A, 16.0, 5), &moved(&RIFF_A2, 16.0, 5)].concat();
    first_take(&mut rig, 0, 8, pluck::periodic(&eight_bars, eighth(120.0), SR, 32.0, PEAK));
    let (m, fpb) = (rig.master(), rig.fpb());
    assert_eq!(m, 8 * fpb);
    let before = rig.pcm(0);
    let t = rig.next_boundary();
    // TRIM lands on the loop's next 1, and is heard first where bar 5 would come: before the clip.
    rig.advance_to(t - secs(3.0));
    rig.press(Command::Trim(0, 4));
    rig.advance_to(t + 4 * fpb - secs(0.75));
    let trimmed = rig.pcm(0);
    assert_eq!(trimmed[4 * fpb as usize..], trimmed[..4 * fpb as usize], "TRIM repeats the first four bars");
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(t + 5 * fpb + fpb / 2);
    let undo = rig.frame;
    rig.press(Command::Undo(0));
    rig.advance_to(t + 12 * fpb + secs(0.6));
    rig.idle();
    assert_eq!(rig.pcm(0), before, "UNDO gives the eight bars back");
    clip.mark("bar 1 again, where bar 5 was", t + 4 * fpb);
    clip.mark("UNDO pressed", undo);
    clip.mark("UNDO heard", t + 8 * fpb);
    clip.mark("bar 5 is back", t + 12 * fpb);
    clip.finish(
        &rig,
        Meta {
            id: "trim-swap",
            group: 0,
            order: 2,
            title: "TRIM halves an 8-bar loop, UNDO brings it back",
            question: "An eight-bar loop (bars 5 to 8 a fourth higher) was trimmed to its first four bars just before the clip; bar 1 comes round where bar 5 was, then UNDO brings all eight back. Where bar 1 comes round, where UNDO lands and where bar 5 returns: any click or stumble?",
            cannot_tell: "240 BPM to fit eight bars, the riff in quarters; the press of TRIM itself is before the clip. A note ringing across the cut on your guitar may step where this one does not.",
            status: "\"TRIM halves an 8-bar lane while it plays\"",
            stereo: false,
            max_seconds: 10.0,
            settings: json!({ "bpm": 240, "meter": "4/4", "loop_bars": 8, "trim_to_bars": 4 }),
        },
    );
}

#[test]
#[ignore]
fn reverse_turn() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 120.0, PEAK));
    rig.advance_to(rig.next_boundary());
    let (b, m) = (rig.frame, rig.master());
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + secs(1.2));
    let press = rig.frame;
    rig.press(Command::Reverse(0));
    rig.advance_to(b + 3 * m + secs(0.5));
    assert!(rig.lane(0).reversed);
    clip.mark("REV pressed", press);
    clip.mark("the turn", b + m);
    clip.mark("reversed loop point", b + 2 * m);
    clip.finish(
        &rig,
        Meta {
            id: "reverse-turn",
            group: 0,
            order: 3,
            title: "REVERSE while the loop plays",
            question: "REV is pressed mid-loop; the loop turns round at its next 1. At the turn, any click, or does it just change direction?",
            cannot_tell: "A riff that ends quietly at the bar line; one that rings loud across it may step at the turn.",
            status: "\"A later track starts at master phase with no seam; does reverse's turn of direction click?\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1 }),
        },
    );
}

#[test]
#[ignore]
fn multiply_seam() {
    // 160 BPM fits a four-bar loop point in the clip.
    let mut rig = rig_at(160.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 160.0, 0.25));
    let (one, fpb, lag) = (rig.master(), rig.fpb(), rig.align);
    let four_bars: Vec<Note> = [&RIFF_B[..], &RIFF_B2[..], &moved(&RIFF_B, 16.0, 5), &moved(&RIFF_B2, 16.0, 5)].concat();
    let p = riff(&four_bars, 4.0, 160.0, 0.25);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(4.0));
    rig.press(Command::RecDub(1));
    let (start, end) = (rig.start_frame(), rig.end_frame());
    assert_eq!(end - start, 4 * one);
    // The player plays on through the handover: the lane is heard a pre-delay after the take ends.
    rig.set_input(move |f| if f >= start && f < end + lag { p[(f - start) as usize % p.len()] } else { 0.0 });
    rig.advance_to(end - secs(0.75));
    let since = rig.events.len();
    let mut clip = Clip::begin(&mut rig);
    let grows = end - lag;
    rig.advance_to(grows + 4 * fpb + secs(0.75));
    assert_eq!(rig.master(), 4 * one, "the loop grew to four bars");
    clip.mark("take ends: lane 2 plays on", commits(&rig, since, 1));
    clip.mark("lane 1 repeats", grows + fpb);
    clip.mark("the new loop point", grows + 4 * fpb);
    clip.finish(
        &rig,
        Meta {
            id: "multiply-seam",
            group: 0,
            order: 4,
            title: "A FIXED 4 take grows a one-bar loop",
            question: "Over a one-bar loop, a FIXED 4 take on lane 2 (its bars 3 and 4 a fourth higher) ends and the loop becomes four bars, lane 1 repeating across it. At the three marks: any seam, and does lane 1 carry on in time?",
            cannot_tell: "160 BPM to fit the four-bar loop point; the free-take rounding (1.6 loops to two) is not in it.",
            status: "\"Multiply: FIXED 4 over a 1-bar loop grows it to 4 bars with no seam\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 160, "meter": "4/4", "loop_bars": 1, "fixed_bars": 4 }),
        },
    );
}

#[test]
#[ignore]
fn later_lane() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_B, 1.0, 120.0, 0.25));
    let m = rig.master();
    rig.advance_to(rig.next_boundary() + m / 3);
    let boundary = rig.next_boundary();
    rig.advance_to(boundary - secs(0.5));
    let mut clip = Clip::begin(&mut rig);
    rig.press(Command::RecDub(1));
    let start = rig.start_frame();
    let commit = boundary + m + 2400;
    let p = riff(&RIFF_A, 1.0, 120.0, 0.25);
    let lag = rig.align;
    // The player plays from the loop's 1 and stops where the press closes the take.
    let looped = looped(p, start);
    rig.set_input(move |f| if f < commit + lag { looped(f) } else { 0.0 });
    rig.advance_to(commit);
    rig.press(Command::RecDub(1));
    assert!(rig.state(1) == LaneState::Playing && rig.lane(1).length == m, "lane 2 commits one loop at the press");
    rig.advance_to(commit + secs(2.4));
    clip.mark("take starts on the 1", boundary);
    clip.mark("you stop: the new lane carries on", commit);
    clip.mark("the new lane's loop point", boundary + 2 * m);
    clip.finish(
        &rig,
        Meta {
            id: "later-lane",
            group: 0,
            order: 5,
            title: "A second lane joins the first",
            question: "Lane 2 records a riff over lane 1 and is closed just past one pass; from there the lane plays it instead of the player. Where it takes over and at its loop point: any seam, and does it sit with lane 1?",
            cannot_tell: "Played perfectly in time by a machine; a real take's timing and your latency are not in it.",
            status: "\"A later track starts at master phase with no seam\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "lanes": 2 }),
        },
    );
}

#[test]
#[ignore]
fn clear_tail() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&STABS, 1.0, 120.0, PEAK));
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.set(Command::SetFxParam(0, FxParam::Time, 2.0));
    rig.set(Command::SetFxParam(0, FxParam::Feedback, 0.9));
    rig.set(Command::SetFxParam(0, FxParam::Mix, 0.5));
    rig.advance(2 * rig.master());
    rig.advance_to(rig.next_boundary());
    let b = rig.frame;
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + secs(3.0));
    let clear = rig.frame;
    rig.press(Command::Clear(0));
    assert_eq!(rig.state(0), LaneState::Empty);
    rig.advance_to(b + secs(6.5));
    clip.mark("CLEAR", clear);
    clip.finish(
        &rig,
        Meta {
            id: "clear-tail",
            group: 0,
            order: 6,
            title: "CLEAR on a lane with a long delay",
            question: "A loop of two stabs through its delay (dotted eighth, feedback 90 %) is cleared while the echoes ring. After CLEAR: does any echo come back, or is it silent?",
            cannot_tell: "The lane's delay only; the shared reverb and an amp-sim's own delay are not in it.",
            status: "\"CLEAR stops a lane's delay echo at the press\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "delay_time": "1/8.", "delay_feedback": 0.9, "delay_mix": 0.5 }),
        },
    );
}

// ── Timing ───────────────────────────────────────────────────────────────────────────────────────────

/// The count-in beats since event `mark`: (frame, beat in the bar, count left).
fn count_beats(rig: &Rig, mark: usize) -> Vec<(Frame, u8, u8)> {
    rig.beats_since(mark).into_iter().filter(|b| b.2 > 0 && b.3).map(|b| (b.0, b.1, b.2)).collect()
}

#[test]
#[ignore]
fn count_in() {
    let mut rig = rig_at(120.0);
    // The riff twice: the player plays on into the next pass until the press closes the take.
    let p = pluck::once(&[&RIFF_A[..], &shifted(&RIFF_A, 8.0)].concat(), eighth(120.0), SR, secs(4.0) as usize, PEAK);
    let mut clip = Clip::begin(&mut rig);
    rig.advance(secs(0.3));
    let mark = rig.events.len();
    let press = rig.frame;
    rig.press(Command::RecDub(0));
    let downbeat = Grid::tempo(press, 0, rig.bpm(), rig.sr, Meter::FOUR_FOUR).beat_frame(Meter::FOUR_FOUR.pulses_per_bar());
    let (fpb, lag) = (rig.fpb(), rig.align);
    let close = downbeat + fpb + 2400;
    let played = from(p, downbeat + lag);
    rig.set_input(move |f| if f < close + lag { played(f) } else { 0.0 });
    rig.advance_to(close);
    rig.press(Command::RecDub(0));
    rig.advance_to(downbeat + 2 * fpb + secs(0.3));
    assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, fpb));
    let count = count_beats(&rig, mark);
    assert_eq!(count.len(), 4, "one bar of four counted beats");
    for (frame, beat, _) in count {
        clip.mark(if beat == 0 { "1 (accent)".to_string() } else { (beat + 1).to_string() }, frame);
    }
    clip.mark("take starts", downbeat);
    clip.mark("REC closes it: the loop plays on", commits(&rig, mark, 0));
    clip.finish(
        &rig,
        Meta {
            id: "count-in",
            group: 1,
            order: 0,
            title: "The count-in, then the take",
            question: "REC with CLICK off: one bar of count-in, then a one-bar take, closed just past its end, and the loop playing on. Is the 1 clearly the accent, and does the take land right after the 4, on the next 1?",
            cannot_tell: "Played perfectly in time by a machine; whether you land on it through your interface's latency is not in it.",
            status: "\"The count-in (1 bar, accent on 1, the tempo locking mid-count-in)\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "click": false, "count_in_bars": 1 }),
        },
    );
}

#[test]
#[ignore]
fn click_6_8() {
    let mut rig = rig_at(100.0);
    let meter = Meter::new(6, 8).unwrap();
    rig.set(Command::SetMeter(meter));
    rig.set(Command::SetBpm(100.0));
    rig.set(Command::SetMetronome(true));
    // At full volume the click alone peaks near 0.88: half keeps the clip clear of full scale.
    rig.set(Command::SetClickVolume(0.5));
    assert_eq!(rig.meter(), meter);
    let fpb = rig.fpb();
    // An eighth in 6/8 at 100 (dotted quarter): a third of the beat.
    let unit = SR as f64 * 60.0 / 100.0 / 3.0;
    // The phrase twice: the player plays on into the next pass until the press closes the take.
    let p = pluck::once(&[&SIX_EIGHT[..], &shifted(&SIX_EIGHT, 12.0)].concat(), unit, SR, 4 * fpb as usize, PEAK);
    let mut clip = Clip::begin(&mut rig);
    rig.advance(secs(0.25));
    let mark = rig.events.len();
    let press = rig.frame;
    rig.press(Command::RecDub(0));
    let downbeat = Grid::tempo(press, 0, rig.bpm(), rig.sr, meter).beat_frame(meter.pulses_per_bar());
    let (lag, close) = (rig.align, downbeat + 2 * fpb + 2400);
    let played = from(p, downbeat + lag);
    rig.set_input(move |f| if f < close + lag { played(f) } else { 0.0 });
    rig.advance_to(close);
    rig.press(Command::RecDub(0));
    rig.advance_to(downbeat + 4 * fpb + secs(0.4));
    assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, 2 * fpb));
    let count = count_beats(&rig, mark);
    assert_eq!(count.iter().map(|c| c.2).collect::<Vec<_>>(), vec![2, 1], "the 6/8 count-in reads 2-1");
    for (frame, _, left) in count {
        clip.mark(format!("count {left}"), frame);
    }
    clip.mark("take starts", downbeat);
    clip.mark("REC closes it: the loop plays on", commits(&rig, mark, 0));
    clip.finish(
        &rig,
        Meta {
            id: "click-6-8",
            group: 1,
            order: 1,
            title: "6/8: count-in and click",
            question: "6/8 at 100 (dotted quarter), CLICK on: a count-in of 2-1, a two-bar take, its playback. Are the softer eighths between the beats heard without getting busy?",
            cannot_tell: "Whether TAP should count the dotted quarter or every eighth is a choice, not in the sound.",
            status: "\"Time signatures (machine: `tests/meter.rs`)\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 100, "meter": "6/8", "click": true, "click_volume": 0.5, "count_in_bars": 1, "loop_bars": 2 }),
        },
    );
}

#[test]
#[ignore]
fn delay_on_click() {
    let mut rig = rig_at(120.0);
    rig.set(Command::SetMetronome(true));
    // At full volume the click alone peaks near 0.88: half keeps the clip clear of full scale.
    rig.set(Command::SetClickVolume(0.5));
    first_take(&mut rig, 0, 1, riff(&DOT, 1.0, 120.0, PEAK));
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.set(Command::SetFxParam(0, FxParam::Time, 0.0));
    rig.set(Command::SetFxParam(0, FxParam::Feedback, 0.85));
    rig.set(Command::SetFxParam(0, FxParam::Mix, 0.5));
    rig.advance(rig.master());
    rig.advance_to(rig.next_boundary());
    let (b, m) = (rig.frame, rig.master());
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + secs(7.0));
    for k in 0..4 {
        clip.mark(format!("bar {}: the pluck", k + 1), b + k * m);
    }
    clip.finish(
        &rig,
        Meta {
            id: "delay-on-click",
            group: 1,
            order: 2,
            title: "A lane's delay against the click",
            question: "One pluck on each 1, through the lane's delay (a quarter, feedback 85 %), CLICK on. Do the repeats sit on the click, bar after bar, or drift off it?",
            cannot_tell: "The lane's delay alone; an amp-sim's delay and the input ECHO are not in it.",
            status: "\"The lane delay's repeats land a whole division apart\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "click": true, "click_volume": 0.5, "loop_bars": 1, "delay_time": "1/4", "delay_feedback": 0.85, "delay_mix": 0.5 }),
        },
    );
}

// ── Tone and level ───────────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn fade() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_B, 1.0, 120.0, 0.25));
    later_take(&mut rig, 1, riff(&RIFF_A, 1.0, 120.0, 0.25));
    later_take(&mut rig, 2, riff(&RIFF_C, 1.0, 120.0, 0.25));
    for lane in 0..3 {
        rig.set(Command::SetVolume(lane, 0.7));
    }
    rig.set(Command::SetFadeBars(2));
    rig.advance_to(rig.next_boundary());
    let (b, fpb) = (rig.frame, rig.fpb());
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + secs(1.5));
    let press = rig.frame;
    rig.press(Command::Action(Action::FadeAll));
    let anchor = rig.anchor();
    let end = anchor + (press + 2 * fpb - anchor + fpb - 1).div_euclid(fpb) * fpb;
    rig.advance_to(end + secs(1.8));
    assert!((0..3).all(|i| rig.state(i) == LaneState::Stopped), "the lanes stopped on the bar line");
    clip.mark("FADE pressed", press);
    clip.mark("bar line: the lanes stop", end);
    clip.finish(
        &rig,
        Meta {
            id: "fade",
            group: 2,
            order: 0,
            title: "FADE over two bars, three lanes",
            question: "FADE is pressed mid-bar with three lanes playing; they fall to the bar line two bars on and stop there. Does it sound like a natural ending, with no step or bump at the bar line?",
            cannot_tell: "Dry lanes; whether a fade with FX returns or an amp-sim's tail feels right is not in it.",
            status: "Next jam 5: \"End with FADE, then reopen.\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "lanes": 3, "lane_volume": 0.7, "fade_bars": 2 }),
        },
    );
}

#[test]
#[ignore]
fn dub_feedback() {
    let mut rig = rig_at(150.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 150.0, 0.2));
    rig.set(Command::SetDubFeedback(0, 0.5));
    let m = rig.master();
    rig.advance_to(rig.next_boundary() + m / 2);
    let b = rig.next_boundary();
    rig.advance_to(b - secs(0.4));
    let (lag, anchor) = (rig.align, rig.anchor());
    let (pb, pc) = (riff(&RIFF_B, 1.0, 150.0, 0.2), riff(&RIFF_C, 1.0, 150.0, 0.2));
    // Two passes of the low riff at 50 %, then one of the high riff at 0 %.
    rig.set_input(move |f| {
        let (k, pos) = ((f - lag - b).div_euclid(m), (f - lag - anchor).rem_euclid(m) as usize);
        match k {
            0 | 1 => pb[pos],
            2 => pc[pos],
            _ => 0.0,
        }
    });
    let mut clip = Clip::begin(&mut rig);
    rig.send_at(b, Command::RecDub(0));
    rig.send_at(b + 2 * m + lag, Command::SetDubFeedback(0, 0.0));
    rig.send_at(b + 3 * m, Command::RecDub(0));
    rig.advance_to(b + 4 * m + secs(0.25));
    assert_eq!(rig.state(0), LaneState::Playing);
    clip.mark("DUB at 50 %", b);
    clip.mark("second pass at 50 %", b + m);
    clip.mark("0 %: a new phrase", b + 2 * m);
    clip.mark("DUB off: what is left", b + 3 * m);
    clip.finish(
        &rig,
        Meta {
            id: "dub-feedback",
            group: 2,
            order: 1,
            title: "DUB FEEDBACK 50 %, then 0 %",
            question: "Two passes dubbed at 50 % (the first riff fades under the new one), then one at 0 % with a high riff. In the last pass: is only the high riff left, and did the 50 % passes fade the old layer the way you expect?",
            cannot_tell: "Tempo 150 to fit the clip; UNDO afterwards is not in it.",
            status: "\"DUB FEEDBACK 50 % over two passes fades the old layers, 0 % replaces\"",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "bpm": 150, "meter": "4/4", "loop_bars": 1, "dub_feedback": [0.5, 0.5, 0.0] }),
        },
    );
}

#[test]
#[ignore]
fn in_fx() {
    let mut rig = rig_at(120.0);
    let u = eighth(120.0);
    // One rendering of the riff three times, each with a rest after it: dry, through ECHO, through REVERB.
    let p = pluck::once(&shifted(&RIFF_A, 1.0), u, SR, (11.0 * u) as usize, PEAK);
    let t0 = rig.frame;
    let at = |units: f64| t0 + (units * u).round() as Frame;
    rig.set_input(again(p.clone(), vec![t0, at(11.0), at(25.0)]));
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(at(11.0));
    let echo = rig.frame;
    rig.press(Command::SetInputSend(InputSend::Echo, true));
    rig.advance_to(at(25.0));
    let reverb = rig.frame;
    rig.press(Command::SetInputSend(InputSend::Echo, false));
    rig.press(Command::SetInputSend(InputSend::Reverb, true));
    rig.advance_to(t0 + secs(10.0));
    assert_dry(&rig, &p, "in-fx");
    clip.mark_out("dry", at(1.0));
    clip.mark_out("ECHO on", echo);
    clip.mark_out("ECHO off, REVERB on", reverb);
    clip.b_half_out(echo);
    clip.finish(
        &rig,
        Meta {
            id: "in-fx",
            group: 2,
            order: 2,
            title: "IN FX: ECHO, then REVERB",
            question: "The riff dry, then through ECHO, then through REVERB, both at their defaults. Is the echo on the tempo, and does each sound right on a guitar at its default?",
            cannot_tell: "How immediate the sends feel under your hands, and how a take through them sounds afterwards.",
            status: "\"IN FX on the guitar: ECHO and REVERB immediate, the echo on the tempo\"",
            stereo: false,
            max_seconds: 10.0,
            settings: json!({ "bpm": 120, "echo_time": "1/8", "echo_feedback": 0.4, "echo_level": 0.5, "reverb_level": 0.5 }),
        },
    );
}

#[test]
#[ignore]
fn ring_mod() {
    let mut rig = rig_at(120.0);
    let u = eighth(120.0);
    // One rendering of the riff twice: dry, then through RING MOD.
    let p = pluck::once(&shifted(&RIFF_A, 1.0), u, SR, (13.0 * u) as usize, PEAK);
    let t0 = rig.frame;
    rig.set_input(again(p.clone(), vec![t0, t0 + (13.0 * u).round() as Frame]));
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(t0 + (13.0 * u).round() as Frame);
    let ring = rig.frame;
    rig.press(Command::SetInputSend(InputSend::Ring, true));
    rig.advance_to(t0 + secs(6.75));
    assert_dry(&rig, &p, "ring-mod");
    clip.mark_out("dry", t0 + u.round() as Frame);
    clip.mark_out("RING MOD on", ring);
    clip.b_half_out(ring);
    clip.finish(
        &rig,
        Meta {
            id: "ring-mod",
            group: 2,
            order: 3,
            title: "RING MOD at its default",
            question: "The riff dry, then with RING MOD at its default (440 Hz, level 50 %). Keep that default, or name what to change (a lower or higher carrier, more or less of it)?",
            cannot_tell: "Only the default; the 20 to 1500 Hz range is not swept.",
            status: "\"IN FX on the guitar\" (its RING MOD sentence: \"RING MOD (never heard): its 20–1500 Hz range and 440 Hz default\")",
            stereo: false,
            max_seconds: 8.0,
            settings: json!({ "ring_freq_hz": 440, "ring_level": 0.5 }),
        },
    );
}

/// The clip's dry half is the rendered phrase itself, the one its wet half plays again: an A/B that
/// changes the effect alone.
fn assert_dry(rig: &Rig, p: &[f32], id: &str) {
    let heard = rig.heard.iter().zip(&rig.heard_right).map(|(l, r)| 0.5 * (l + r));
    let off = heard.zip(p).fold(0.0f32, |m, (h, x)| m.max((h - x).abs()));
    assert!(off < 1e-6, "{id}: the dry half is {off:.2e} off the phrase its wet half plays");
}

// ── Stereo (headphones) ──────────────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn wide() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 120.0, PEAK));
    rig.advance_to(rig.next_boundary());
    let (b, m) = (rig.frame, rig.master());
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + 2 * m - secs(0.4));
    let press = rig.frame;
    rig.press(Command::CopyWide(0));
    rig.idle();
    rig.advance_to(b + 4 * m);
    assert_eq!(rig.state(1), LaneState::Playing, "the wide copy plays");
    clip.mark("WIDE pressed", press);
    clip.mark("both guitars, a full pass", b + 2 * m);
    clip.b_half(press);
    clip.finish(
        &rig,
        Meta {
            id: "wide",
            group: 3,
            order: 0,
            title: "WIDE COPY on a riff",
            question: "The riff alone and centred, then WIDE COPY: a copy 20 ms behind, one half left, one half right. Does it sound like two guitars, and is 20 ms the gap you want?",
            cannot_tell: "The same take twice, not two performances; your amp's own stereo is not in it.",
            status: "\"↔ WIDE (the copy 20 ms behind, mirrored pan)\"",
            stereo: true,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "wide_ms": 20, "pans": [-0.5, 0.5] }),
        },
    );
}

#[test]
#[ignore]
fn pan() {
    let mut rig = rig_at(120.0);
    first_take(&mut rig, 0, 1, riff(&RIFF_A, 1.0, 120.0, PEAK));
    rig.press(Command::SetPan(0, -1.0));
    rig.advance(secs(0.5));
    rig.advance_to(rig.next_boundary());
    let b = rig.frame;
    let mut clip = Clip::begin(&mut rig);
    rig.advance_to(b + secs(2.5));
    rig.press(Command::SetPan(0, 0.0));
    rig.advance_to(b + secs(5.0));
    rig.press(Command::SetPan(0, 1.0));
    rig.advance_to(b + secs(7.5));
    clip.mark("hard left", b);
    clip.mark("centre", b + secs(2.5));
    clip.mark("hard right", b + secs(5.0));
    clip.finish(
        &rig,
        Meta {
            id: "pan",
            group: 3,
            order: 1,
            title: "A lane panned left, centre, right",
            question: "One lane hard left, then centre, then hard right. Is each side fully on one ear, the centre no quieter, and each move a quick glide with no zipper?",
            cannot_tell: "Your headphones' and the interface's own balance.",
            status: "\"Pan per lane: hard left or right is +3 dB on that side\"",
            stereo: true,
            max_seconds: 8.0,
            settings: json!({ "bpm": 120, "meter": "4/4", "loop_bars": 1, "pans": [-1.0, 0.0, 1.0] }),
        },
    );
}
