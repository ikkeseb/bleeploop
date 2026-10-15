//! Time signatures (the meter, `lf_engine::grid::Meter`): every offered meter on the real looper. A
//! count-in is one bar of the meter's pulses, counted down in its beats, and the take starts on the
//! downbeat after it; a free stop commits whole bars of it with a quarter of a counted beat of grace;
//! FIXED N records N of its bars; the click's accents land on the master's bar lines across a multiply;
//! TRIM and HALVE keep whole bars; FADE ends on a bar line; AUTO's grid runs in it; a snapshot carries it
//! and a load brings it back, refusing one whose frames disagree with it; the meter locks with the
//! tempo; the lane buffer bounds a take in its whole bars; the FX follow its quarter note. 4/4 is the
//! default and behaves as before: `four_four_renders` compares it across a change (by hand, ignored),
//! and the rest of the suite runs in it. `render_the_click_per_meter` writes one click WAV per meter for
//! the ear (ignored).

mod common;

use common::refs::{sha256_f32, write_wav};
use common::{code, Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::{frames_per_bar, max_whole_bars, Frame, Grid, Meter};
use lf_engine::render::wet_master;
use lf_engine::{Action, Command, Event, InputSend, InputSendParam, LaneMix, LaneState, Load, LoadTrack, SessionError, SessionJob, Snapshot};

const BPM: u32 = 100;

fn rig_in(meter: Meter) -> Rig {
    let mut rig = Rig::new();
    rig.set(Command::SetMeter(meter));
    rig.set(Command::SetBpm(BPM as f64));
    assert_eq!(rig.meter(), meter);
    rig
}

/// REC on `lane` now: the downbeat its count-in ends on.
fn press_rec(rig: &mut Rig, lane: u8) -> Frame {
    let press = rig.frame;
    rig.press(Command::RecDub(lane));
    let m = rig.meter();
    Grid::tempo(press, 0, rig.bpm(), rig.sr, m).beat_frame(m.pulses_per_bar())
}

/// A free first take on lane 0 stopped `bars` bars and `after` frames past its downbeat; the loop plays.
fn first_take(rig: &mut Rig, bars: Frame, after: Frame) -> Frame {
    let downbeat = press_rec(rig, 0);
    rig.advance_to(downbeat + bars * rig.fpb() + after);
    rig.press(Command::RecDub(0));
    // A press inside the grace records on to the bar line.
    rig.advance_to(downbeat + bars * rig.fpb() + 1);
    rig.advance(rig.seconds(0.1));
    downbeat
}

/// A quarter of a counted beat: the stop's grace.
fn grace(rig: &Rig) -> Frame {
    rig.fpb() / (4 * rig.meter().beats_per_bar() as Frame)
}

/// The meter `rig` does not count in as many beats (a load in it disagrees with the frames).
fn other_than(meter: Meter) -> Meter {
    Meter::OFFERED.into_iter().find(|m| m.beats_per_bar() != meter.beats_per_bar()).unwrap()
}

#[test]
fn a_count_in_is_one_bar_counted_in_beats_and_the_take_starts_on_its_downbeat() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        let mark = rig.events.len();
        let press = rig.frame;
        let downbeat = press_rec(&mut rig, 0);
        assert_eq!(rig.start_frame(), downbeat, "{meter:?}: the take's window starts on the downbeat");
        rig.advance_to(downbeat + 1);
        let info = rig.lane(0);
        assert!(info.state == LaneState::Recording && !info.armed, "{meter:?}: recording from the downbeat");
        let count: Vec<_> = rig.beats_since(mark).into_iter().filter(|b| b.2 > 0).collect();
        let beats = meter.beats_per_bar();
        assert_eq!(count.iter().map(|b| b.2 as u64).collect::<Vec<_>>(), (1..=beats).rev().collect::<Vec<_>>(), "{meter:?}: counted down in beats");
        assert_eq!(count.iter().map(|b| b.1 as u64).collect::<Vec<_>>(), (0..beats).collect::<Vec<_>>());
        let grid = Grid::tempo(press, 0, BPM, rig.sr, meter);
        assert_eq!(count.iter().map(|b| b.0).collect::<Vec<_>>(), (0..beats).map(|k| grid.beat_frame(k * meter.pulses_per_beat())).collect::<Vec<_>>());
        assert!(count.iter().all(|b| b.3), "every count beat clicks");
        assert_eq!(downbeat - press, frames_per_bar(BPM as f64, rig.sr, meter), "{meter:?}: one bar");
        let transport = rig.events[mark..].iter().rev().find_map(|e| match *e {
            Event::Transport { meter, locked, .. } => Some((meter, locked)),
            _ => None,
        });
        assert_eq!(transport, Some((meter, true)), "the count locks the tempo and the meter");
    }
}

#[test]
fn a_free_stop_commits_whole_bars_with_a_quarter_beat_of_grace() {
    for meter in Meter::OFFERED {
        for (early, bars) in [(0, 2), (1, 2), (-1, 2), (2, 1)] {
            let mut rig = rig_in(meter);
            let g = grace(&rig);
            // early: 0 on the grace's edge, 1 a frame inside it, -1 a frame past it before the bar line, 2 just outside.
            let at = match early {
                0 => -g,
                1 => -g + 1,
                -1 => -1,
                _ => -g - 1,
            };
            first_take(&mut rig, 2, at);
            assert_eq!(rig.master(), bars * rig.fpb(), "{meter:?} stopped {at} frames from bar 2");
            assert_eq!(rig.state(0), LaneState::Playing);
        }
    }
}

#[test]
fn fixed_records_its_bars_of_the_meter() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(3.0));
        let downbeat = press_rec(&mut rig, 0);
        assert_eq!(rig.end_frame(), downbeat + 3 * rig.fpb(), "{meter:?}");
        rig.advance_to(downbeat + 3 * rig.fpb() + 10);
        assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, 3 * rig.fpb()));
    }
}

#[test]
fn the_accents_land_on_the_bar_lines_across_a_multiply() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set(Command::SetMetronome(true));
        rig.set_level(0.2);
        first_take(&mut rig, 1, 300);
        let fpb = rig.fpb();
        assert_eq!(rig.master(), fpb);
        // A later take stopped past two loops: a multiply to two bars.
        rig.press(Command::RecDub(1));
        let start = rig.start_frame();
        rig.advance_to(start + 2 * fpb + 500);
        rig.press(Command::RecDub(1));
        rig.advance(rig.seconds(0.2));
        rig.idle();
        assert_eq!(rig.master(), 2 * fpb, "{meter:?}: the loop doubled");
        let mark = rig.events.len();
        rig.advance(3 * fpb);
        let anchor = rig.anchor();
        let beats = rig.beats_since(mark);
        let grid = Grid::master(anchor, rig.master(), 2, meter);
        let per_beat = meter.pulses_per_beat();
        assert_eq!(beats.len() as u64, 3 * meter.beats_per_bar(), "{meter:?}: one event a counted beat");
        for (frame, in_bar, _, clicked) in beats {
            assert!(clicked);
            let pulse = grid.first_beat_at_or_after(frame);
            assert_eq!(grid.beat_frame(pulse), frame, "{meter:?}: every beat on the master's grid");
            assert_eq!(pulse % per_beat, 0, "{meter:?}: on a beat, not a pulse between");
            assert_eq!((pulse % meter.pulses_per_bar()) / per_beat, in_bar as u64);
            assert_eq!(in_bar == 0, (frame - anchor) % fpb == 0, "{meter:?}: the downbeats are the bar lines");
        }
    }
}

#[test]
fn trim_and_halve_keep_whole_bars_of_the_meter() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set_input(code);
        first_take(&mut rig, 4, 200);
        let fpb = rig.fpb();
        assert_eq!(rig.master(), 4 * fpb);
        rig.press(Command::Action(Action::Halve));
        rig.idle();
        rig.advance_to(rig.next_boundary() + 10);
        let pcm = rig.pcm(0);
        assert_eq!(pcm.len() as Frame, 4 * fpb, "{meter:?}: the length stays");
        assert!((0..pcm.len()).all(|p| pcm[p] == pcm[p % (2 * fpb as usize)]), "{meter:?}: the first two bars, repeating");
        assert_ne!(pcm[0], pcm[fpb as usize]);
    }
}

#[test]
fn a_fade_ends_on_a_bar_line_of_the_meter() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set_level(0.2);
        first_take(&mut rig, 2, 100);
        rig.advance(rig.fpb() / 3);
        rig.press(Command::Action(Action::FadeAll));
        let end = rig.lane(0).stop_at.expect("a fade pends");
        assert_eq!((end - rig.anchor()) % rig.fpb(), 0, "{meter:?}: on a bar line");
        rig.advance_to(end + 1);
        assert_eq!(rig.state(0), LaneState::Stopped);
    }
}

#[test]
fn auto_record_runs_the_meters_grid() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set(Command::SetAutoRecord(true));
        rig.set(Command::SetMetronome(true));
        rig.press(Command::RecDub(0));
        let at = rig.frame + rig.seconds(0.7);
        rig.set_input(move |f| if f < at { 0.0 } else { 0.3 });
        rig.advance_to(at + 600);
        let start = rig.start_frame();
        let mark = rig.events.len();
        rig.advance(rig.fpb());
        let grid = Grid::tempo(start, 0, BPM, rig.sr, meter);
        let beats = rig.beats_since(mark);
        assert_eq!(beats.len() as u64, meter.beats_per_bar(), "{meter:?}");
        for (frame, in_bar, _, clicked) in beats {
            let pulse = grid.first_beat_at_or_after(frame);
            assert!(clicked && grid.beat_frame(pulse) == frame && pulse % meter.pulses_per_beat() == 0, "{meter:?}");
            assert_eq!((pulse % meter.pulses_per_bar()) / meter.pulses_per_beat(), in_bar as u64);
        }
        rig.advance_to(start + 2 * rig.fpb() + 300);
        rig.press(Command::RecDub(0));
        rig.advance(10);
        assert_eq!(rig.master(), 2 * rig.fpb(), "{meter:?}: two bars of it");
    }
}

fn run_job(rig: &mut Rig, job: SessionJob) -> SessionJob {
    assert!(rig.session().send(Box::new(job)).is_ok());
    for _ in 0..10_000 {
        rig.advance(128);
        if let Some(job) = rig.session().returned() {
            return *job;
        }
    }
    panic!("the session job never came back");
}

fn load_of(rig: &Rig, s: &Snapshot, meter: Meter, bars: Frame) -> Load {
    let master = s.master as usize;
    let capacity = rig.engine.looper().capacity() as usize;
    let tracks = (0..s.count)
        .map(|k| {
            let t = s.tracks[k].unwrap();
            let mut buf = s.pcm[k * master..(k + 1) * master].to_vec();
            buf.resize(capacity, 0.0);
            LoadTrack { index: t.index, buf, peaks: Vec::new(), reversed: false, playing: true, mix: t.mix }
        })
        .collect();
    Load { bpm: s.bpm, meter, bars, master: s.master, tracks, result: None }
}

fn load_result(rig: &mut Rig, load: Load) -> Result<(), SessionError> {
    match run_job(rig, SessionJob::Load(load)) {
        SessionJob::Load(Load { result: Some(r), .. }) => r,
        _ => panic!("no load came back"),
    }
}

#[test]
fn a_snapshot_carries_its_meter_and_a_load_brings_it_back() {
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set_input(code);
        first_take(&mut rig, 2, 50);
        // Locked with the tempo: a meter sent now is dropped.
        rig.set(Command::SetMeter(other_than(meter)));
        assert_eq!(rig.meter(), meter, "the meter locks with the loop");
        let pcm = vec![0.0; rig.master() as usize];
        let SessionJob::Snapshot(s) = run_job(&mut rig, SessionJob::Snapshot(Snapshot::new(pcm))) else { panic!("no snapshot came back") };
        assert_eq!((s.result, s.meter, s.bpm), (Some(Ok(())), meter, BPM));
        rig.press(Command::ClearAll);
        rig.idle();
        rig.set(Command::SetMeter(other_than(meter)));
        assert_ne!(rig.meter(), meter, "free again once cleared");
        // A meter that disagrees with the frames is refused before anything changes.
        let wrong = load_of(&rig, &s, other_than(meter), 2);
        assert!(matches!(load_result(&mut rig, wrong), Err(SessionError::Invalid(_))), "{meter:?}");
        assert_eq!((rig.state(0), rig.locked()), (LaneState::Empty, false));
        let mark = rig.events.len();
        let load = load_of(&rig, &s, meter, 2);
        assert_eq!(load_result(&mut rig, load), Ok(()));
        assert_eq!((rig.meter(), rig.locked(), rig.master()), (meter, true, s.master), "{meter:?}: the load sets and locks it");
        assert!(rig.events[mark..].iter().any(|e| matches!(*e, Event::Transport { meter: m, locked: true, .. } if m == meter)));
        assert_eq!(rig.pcm(0), s.pcm[..s.master as usize].to_vec());
    }
}

#[test]
fn the_lane_buffer_bounds_a_take_in_whole_bars_of_the_meter() {
    // A free first take runs to the buffer's end, not a bar line, and keeps the whole bars in it; a FIXED
    // request past it and a later take's multiply close on a bar line inside it.
    for meter in Meter::OFFERED {
        let opts = || Opts { loop_seconds: 7.0, ..Opts::default() };
        let mut rig = Rig::with(opts());
        rig.set(Command::SetMeter(meter));
        rig.set(Command::SetBpm(137.0));
        let capacity = rig.engine.looper().capacity();
        let fpb = rig.fpb();
        let whole = max_whole_bars(capacity, fpb);
        let downbeat = press_rec(&mut rig, 0);
        assert_eq!(rig.end_frame(), downbeat + capacity, "{meter:?}: the capture ends at the buffer");
        rig.advance_to(downbeat + capacity + 10);
        assert_eq!(rig.master(), whole * fpb, "{meter:?}: its whole bars kept");
        assert_ne!(capacity % fpb, 0, "{meter:?}: the buffer's end is no bar line here");

        let mut rig = Rig::with(opts());
        rig.set(Command::SetMeter(meter));
        rig.set(Command::SetBpm(137.0));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(32.0));
        let downbeat = press_rec(&mut rig, 0);
        assert_eq!(rig.end_frame(), downbeat + whole * fpb, "{meter:?}: FIXED past the buffer closes on its last bar line");
        rig.advance_to(rig.end_frame() + 10);
        assert_eq!(rig.master(), whole * fpb);

        let mut rig = Rig::with(opts());
        rig.set(Command::SetMeter(meter));
        rig.set(Command::SetBpm(137.0));
        rig.set_level(0.2);
        first_take(&mut rig, 1, 100);
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(32.0));
        rig.press(Command::RecDub(1));
        let start = rig.start_frame();
        assert_eq!(rig.end_frame(), start + whole * fpb, "{meter:?}: a multiply of whole loops within the buffer");
        rig.advance_to(rig.end_frame() + 10);
        rig.idle();
        assert_eq!(rig.master(), whole * fpb);
    }
}

/// The input echo's quarter note at `meter` and 60 BPM, in frames: an impulse and its first echo.
fn echo_gap(meter: Meter) -> Frame {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(60.0));
    rig.set(Command::SetMeter(meter));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, 1.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoTime, 0.0));
    rig.set(Command::SetInputSend(InputSend::Echo, true));
    rig.advance(4800);
    rig.keep_output();
    let hit = rig.frame + 5;
    rig.set_input(move |f| if f == hit { 1.0 } else { 0.0 });
    rig.advance(60_000);
    let k = (hit - rig.output.as_ref().unwrap().0) as usize;
    let peak = rig.record.iter().enumerate().skip(k + 1).fold((0, 0.0f32), |a, (j, &x)| if x.abs() > a.1 { (j, x.abs()) } else { a });
    (peak.0 - k) as Frame
}

#[test]
fn the_fx_follow_the_meters_quarter_note() {
    // At one BPM a compound meter's quarter is shorter (its beat is the dotted quarter): a meter change
    // re-times the echo though the tempo stays.
    assert_eq!(echo_gap(Meter::FOUR_FOUR), 48_000);
    assert_eq!(echo_gap(Meter::new(3, 4).unwrap()), 48_000);
    assert_eq!(echo_gap(Meter::new(6, 8).unwrap()), 32_000);
    assert_eq!(echo_gap(Meter::new(12, 8).unwrap()), 32_000);
    // A lane's delay: an impulse loop in 4/4 and in 6/8 at the same tempo, its first echo a quarter on.
    for (meter, quarter) in [(Meter::FOUR_FOUR, 24_000), (Meter::new(6, 8).unwrap(), 16_000)] {
        let mut rig = rig_in(meter);
        rig.set(Command::SetBpm(120.0));
        let downbeat = press_rec(&mut rig, 0);
        let hit = downbeat + 1000;
        rig.set_input(move |f| if f == hit { 0.5 } else { 0.0 });
        rig.advance_to(downbeat + rig.fpb() + 50);
        rig.press(Command::RecDub(0));
        rig.set_input(|_| 0.0);
        rig.set(Command::SetFxParam(0, FxParam::Time, 0.0));
        rig.set(Command::SetFxParam(0, FxParam::Feedback, 0.0));
        rig.set(Command::SetFxParam(0, FxParam::Mix, 1.0));
        rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
        rig.advance_to(rig.next_boundary());
        rig.keep_output();
        let from = rig.frame;
        rig.advance(rig.fpb());
        let played = rig.anchor() + rig.master() * ((from - rig.anchor()) / rig.master()) + 1000;
        let bus = &rig.bus;
        let loud: Vec<Frame> = bus.iter().enumerate().filter(|(_, x)| x.abs() > 0.05).map(|(j, _)| from + j as Frame).collect();
        assert!(loud.iter().any(|&f| (f - (played + quarter)).abs() <= 2), "{meter:?}: an echo a quarter after {played}: {loud:?}");
    }
}

/// One click WAV per offered meter, for the ear (and an .mp4 beside each, made outside: the meter's
/// still and this audio): a one-bar count-in, then a two-bar take with the click on, at 100 BPM (a
/// compound meter's dotted quarter). Into `$LF_MEDIA_OUT`, else the test target's tmp dir, as
/// `<num>-<den>.wav`. `cargo test -p lf-engine --test meter -- --ignored render_the_click_per_meter`
#[test]
#[ignore]
fn render_the_click_per_meter() {
    let dir = std::env::var_os("LF_MEDIA_OUT").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("meter-clicks"));
    std::fs::create_dir_all(&dir).unwrap();
    for meter in Meter::OFFERED {
        let mut rig = rig_in(meter);
        rig.set(Command::SetMetronome(true));
        // Past the master volume's glide in from a new engine's silence.
        rig.advance(rig.seconds(0.5));
        rig.keep_output();
        let downbeat = press_rec(&mut rig, 0);
        rig.advance_to(downbeat + 2 * rig.fpb() + 10);
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(0.15));
        assert_eq!(rig.master(), 2 * rig.fpb());
        let path = dir.join(format!("{}-{}.wav", meter.num(), meter.den()));
        write_wav(&path, rig.sr, &[rig.heard.clone(), rig.heard_right.clone()]);
        println!("{}", path.display());
    }
}

/// A burst of a 220 Hz sine every half second, 40 ms long: something for the delays to echo.
fn bursts(sr: u32) -> impl Fn(Frame) -> f32 {
    move |f| {
        let t = f.rem_euclid(sr as Frame / 2);
        if t < sr as Frame / 25 {
            0.3 * (std::f32::consts::TAU * 220.0 * t as f32 / sr as f32).sin()
        } else {
            0.0
        }
    }
}

/// The beats and the transports the rig saw, as plain numbers (their shapes may grow).
fn feed(rig: &Rig) -> Vec<f32> {
    let mut out = Vec::new();
    for e in &rig.events {
        match *e {
            Event::Beat { frame, beat_in_bar, count_left, clicked } => out.extend([1.0, frame as f32, beat_in_bar as f32, count_left as f32, clicked as u8 as f32]),
            Event::Transport { frame, master, bpm, locked, .. } => out.extend([2.0, frame as f32, master as f32, bpm as f32, locked as u8 as f32]),
            _ => {}
        }
    }
    out
}

fn hashes(rig: &Rig) -> String {
    format!("out {} feed {}", sha256_f32(&[&rig.heard, &rig.heard_right]), sha256_f32(&[&feed(rig)]))
}

/// The 4/4 renders the meter work must leave bit-identical: the click through a count-in, a loop, a
/// multiply, HALVE and FADE; AUTO's grid; a lane's tempo-synced delay and stutter; the input echo across
/// a tempo change. Run before and after a change to the clock, the grid or the FX timing and compare the
/// lines (`-- --ignored --nocapture four_four_renders`): the sums depend on the platform's libm, so they
/// are compared on one machine, never pinned here.
#[test]
#[ignore]
fn four_four_renders() {
    let sr = 48000;
    // The click: a count-in at 100, a two-bar loop, a later take that multiplies it, HALVE, FADE.
    let mut rig = Rig::at(sr);
    rig.set_input(bursts(sr));
    rig.keep_output();
    rig.set(Command::SetMetronome(true));
    rig.set(Command::SetBpm(100.0));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(2.4 + 4.8 + 0.05));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(1.0));
    rig.press(Command::RecDub(1));
    rig.advance(rig.seconds(4.8 * 2.6));
    rig.press(Command::RecDub(1));
    rig.advance(rig.seconds(3.0));
    rig.press(Command::Action(Action::Halve));
    rig.advance(rig.seconds(5.0));
    rig.press(Command::Action(Action::FadeAll));
    rig.advance(rig.seconds(6.0));
    println!("click {}", hashes(&rig));

    // AUTO's grid at 137, with the click.
    let mut rig = Rig::at(sr);
    rig.keep_output();
    rig.set(Command::SetMetronome(true));
    rig.set(Command::SetBpm(137.0));
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.7));
    rig.set_input(bursts(sr));
    rig.advance(rig.seconds(3.6));
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(5.0));
    println!("auto {}", hashes(&rig));

    // A lane's delay (dotted eighth) and stutter on a one-bar loop at 120, then a free re-take at 90.
    let mut rig = Rig::at(sr);
    rig.set_input(bursts(sr));
    rig.keep_output();
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(2.0 + 2.0 + 0.02));
    rig.press(Command::RecDub(0));
    rig.set(Command::SetFxParam(0, FxParam::Time, 2.0));
    rig.set(Command::SetFxParam(0, FxParam::Feedback, 0.5));
    rig.set(Command::SetFxParam(0, FxParam::Mix, 0.5));
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.set(Command::SetFxBypass(1, FxKind::Stutter, false));
    rig.advance(rig.seconds(3.0));
    rig.press(Command::RecDub(1));
    rig.advance(rig.seconds(4.5));
    rig.press(Command::RecDub(1));
    rig.set(Command::SetFxBypass(1, FxKind::Stutter, false));
    rig.advance(rig.seconds(4.0));
    rig.press(Command::ClearAll);
    rig.set(Command::SetBpm(90.0));
    rig.press(Command::RecDub(2));
    rig.advance(rig.seconds(2.67 + 2.67 + 0.02));
    rig.press(Command::RecDub(2));
    rig.set(Command::SetFxBypass(2, FxKind::Delay, false));
    rig.advance(rig.seconds(4.0));
    println!("lane-fx {}", hashes(&rig));

    // The input echo, free-running at 97 and then 130.
    let mut rig = Rig::at(sr);
    rig.set_input(bursts(sr));
    rig.keep_output();
    rig.set(Command::SetBpm(97.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoTime, 1.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.4));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, 0.6));
    rig.set(Command::SetInputSend(InputSend::Echo, true));
    rig.advance(rig.seconds(3.0));
    rig.set(Command::SetBpm(130.0));
    rig.advance(rig.seconds(3.0));
    println!("echo {}", hashes(&rig));

    // The export's wet master: a two-bar loop at 100 with a lane's delay at a quarter note.
    let master = 2 * 115_200;
    let mut buf = vec![0.0f32; master];
    for p in (0..master).step_by(24_000) {
        buf[p] = 0.25;
    }
    let mut mix = LaneMix::default();
    mix.fx[FxKind::Delay.index()].params = [3.0, 0.6, 0.5];
    mix.fx[FxKind::Delay.index()].bypassed = false;
    let track = LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix };
    let load = Load { bpm: 100, meter: Meter::FOUR_FOUR, bars: 2, master: master as Frame, tracks: vec![track], result: None };
    let wet = wet_master(sr, load, &[mix], &[]).expect("the render");
    println!("export out {} warmup {}", sha256_f32(&[&wet.left, &wet.right]), wet.warmup);
}
