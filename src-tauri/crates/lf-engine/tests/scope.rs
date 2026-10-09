//! The live scope taps (`lf_engine::scope`), the data the stage view draws of the sound coming out
//! now. No web counterpart: the old looper drew the recorded waveform alone. What these hold the
//! engine to: a column is 4 ms of sound and the columns come out contiguous however the block splits;
//! a lane's column follows its FX and its mix, not the take in its buffer; the master's column keeps
//! what a `(l + r) / 2` tap would cancel; off, nothing is folded and nothing is pushed; and the scope
//! on changes no sample of what the device plays, which is the one that protects the jam.
//!
//! Every `process` here runs under `assert_no_alloc` through `common`'s render.

mod common;

use common::{violation_count, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::scope::{scope_bin_frames, ScopeBin, SCOPE_CAPACITY, SCOPE_MASTER, SCOPE_MONITOR};
use lf_engine::{Command, LaneState};

/// The frames one column covers at the rig's rate.
fn bin(rig: &Rig) -> Frame {
    scope_bin_frames(rig.sr) as Frame
}

/// Source `s`'s largest peak-to-peak span over `columns`.
fn span(columns: &[ScopeBin], s: usize) -> f32 {
    columns.iter().map(|c| c.hi[s] - c.lo[s]).fold(0.0, f32::max)
}

/// Every column covers `bin` frames from the one before it, with no hole and no overlap.
fn contiguous(columns: &[ScopeBin], bin: Frame) -> bool {
    columns.windows(2).all(|w| w[1].frame == w[0].frame + bin)
}

/// A ramp of constant amplitude, 400 frames a cycle: every column of it spans about the same.
fn ramp(f: Frame) -> f32 {
    (f.rem_euclid(400) as f32 / 400.0 - 0.5) * 0.8
}

/// The scope's columns come out contiguous from the frame it came on, however the block splits: a
/// block size that is no multiple of a column, the click's beats, and a command stamped inside every
/// block, so chunks are short and ragged.
#[test]
fn the_columns_come_out_contiguous_across_short_chunks() {
    let mut rig = Rig::with(Opts { block: 97, ..Default::default() });
    rig.set(Command::SetMetronome(true));
    rig.set_input(ramp);
    rig.record_first_take(0, 1, 2400);
    let bin = bin(&rig);
    let before = violation_count();
    let on_at = rig.frame;
    rig.press(Command::SetScope(true));
    let wanted = 400;
    while rig.frame < on_at + wanted * bin {
        rig.send_at(rig.frame + 37, Command::SetVolume(1, 0.9));
        rig.advance(97);
    }
    assert_eq!(violation_count(), before, "the fold allocated");
    let columns = rig.read_scope();
    assert!(columns.len() as Frame >= wanted - 1, "about {wanted} columns, not {}", columns.len());
    assert_eq!(columns[0].frame, on_at, "the first column starts where the scope came on");
    assert!(contiguous(&columns, bin), "a hole or an overlap in the columns");
    assert_eq!(rig.engine.diag().scope_dropped, 0, "nothing was lost");
    assert!(span(&columns, 0) > 0.1, "lane 0 plays: {}", span(&columns, 0));
    assert!(span(&columns, SCOPE_MASTER) > 0.1, "the master carries it");
}

/// Off (the default) the consumer stays empty however long the engine renders, and a view that closes
/// stops the columns.
#[test]
fn off_means_no_columns_at_all() {
    let mut rig = Rig::new();
    rig.set(Command::SetMetronome(true));
    rig.set_input(ramp);
    rig.record_first_take(0, 1, 2400);
    rig.advance(rig.seconds(3.0));
    assert!(rig.read_scope().is_empty(), "the default is off");
    assert_eq!(rig.engine.diag().scope_dropped, 0);

    rig.set(Command::SetScope(true));
    rig.advance(rig.seconds(0.1));
    assert!(!rig.read_scope().is_empty(), "on, the columns come");
    rig.set(Command::SetScope(false));
    rig.advance(rig.seconds(1.0));
    assert!(rig.read_scope().is_empty(), "the stage view closed");
}

/// The jam's guard: the same scripted render with the scope on and off plays the same bits. The script
/// sends `SetScope` unstamped at the same frame in both runs and renders nothing extra for it, so the
/// two timelines are the same render.
#[test]
fn the_sound_does_not_change_with_the_scope_on() {
    let render = |on: bool| {
        let mut rig = Rig::with(Opts { block: 64, ..Default::default() });
        rig.send_at(rig.frame, Command::SetScope(on));
        rig.set(Command::SetMetronome(true));
        rig.set_input(ramp);
        rig.keep_output();
        let master = rig.record_first_take(0, 2, 2400);
        rig.press(Command::Copy(0));
        rig.idle();
        rig.set(Command::SetPan(1, -0.6));
        rig.set(Command::SetVolume(0, 0.7));
        rig.set(Command::SetFxBypass(0, lf_engine::dsp::fx::FxKind::Delay, false));
        rig.set(Command::SetFxParam(0, lf_engine::dsp::fx::FxParam::Feedback, 0.8));
        rig.set(Command::SetInputSend(lf_engine::InputSend::Echo, true));
        rig.press(Command::RecDub(1));
        rig.advance(master / 2);
        rig.drop_scope();
        rig.press(Command::RecDub(1));
        rig.idle();
        rig.advance(master);
        rig.press(Command::Action(lf_engine::Action::FadeAll));
        rig.advance(master * 2);
        let columns = rig.read_scope().len();
        (rig.heard.clone(), rig.heard_right.clone(), columns)
    };
    let (off_l, off_r, off_columns) = render(false);
    let (on_l, on_r, on_columns) = render(true);
    assert_eq!(off_columns, 0, "off: no columns");
    assert!(on_columns > 100, "on: the columns came ({on_columns})");
    assert_eq!(off_l.len(), on_l.len(), "the same render");
    let bits = |x: &[f32]| x.iter().map(|v| v.to_bits()).collect::<Vec<u32>>();
    assert_eq!(bits(&off_l), bits(&on_l), "the left channel changed with the scope on");
    assert_eq!(bits(&off_r), bits(&on_r), "the right channel changed with the scope on");
}

/// A lane's column is what its chain plays now, not what its buffer holds: a lane turned down and a
/// lane fading show it, the lane beside them does not, and no buffer changed.
#[test]
fn a_lanes_column_follows_its_fx_and_its_fade_not_its_take() {
    let mut rig = Rig::new();
    rig.set_input(ramp);
    rig.record_first_take(0, 1, 2400);
    rig.press(Command::Copy(0));
    rig.idle();
    assert_eq!((rig.state(0), rig.state(1)), (LaneState::Playing, LaneState::Playing));
    let pcm = rig.pcm(0);
    let loudest = |pcm: &[f32]| pcm.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    let held = loudest(&pcm);

    rig.set(Command::SetScope(true));
    rig.advance(rig.seconds(0.3));
    let both = rig.read_scope();
    let (zero, one) = (span(&both, 0), span(&both, 1));
    assert!((zero - one).abs() < 0.02 * zero, "the same loop on both lanes: {zero} and {one}");

    // The lane's volume feeds its chain, so its column follows it; past the glide (10 ms), only the
    // lane that moved is quieter.
    rig.set(Command::SetVolume(0, 0.1));
    rig.advance(rig.seconds(0.1));
    rig.drop_scope();
    rig.advance(rig.seconds(0.3));
    let turned = rig.read_scope();
    assert!(span(&turned, 0) < 0.3 * zero, "lane 0 turned down: {} of {zero}", span(&turned, 0));
    assert!((span(&turned, 1) - one).abs() < 0.02 * one, "lane 1 stayed put: {} of {one}", span(&turned, 1));

    // A FADE takes every playing lane down to its bar line: the columns show the ramp, and the
    // buffers are untouched throughout.
    rig.set(Command::SetFadeBars(1));
    rig.drop_scope();
    rig.press(Command::Action(lf_engine::Action::FadeAll));
    rig.advance(rig.fpb() * 2);
    let faded = rig.read_scope();
    let eighth = faded.len() / 8;
    let (opened, ended) = (span(&faded[..eighth], 1), span(&faded[faded.len() - eighth..], 1));
    assert!(ended < 0.3 * opened, "lane 1 faded out: {ended} after {opened}");
    assert_eq!(loudest(&rig.pcm(0)), held, "the take in the buffer never moved");
}

/// The master's column takes the wider of the two sides per sample, so two lanes panned hard apart
/// with opposite signs still report the level the room hears; a `(l + r) / 2` tap would read silence.
#[test]
fn the_master_column_keeps_what_a_mean_tap_would_cancel() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 1, 2400);
    // The same loop inverted on lane 1: a FIXED take of one bar off the inverted input, which arms on
    // the master boundary, so the two loops play the same phase against each other.
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.set_level(-0.5);
    rig.press(Command::RecDub(1));
    rig.advance(master * 3);
    rig.idle();
    rig.set_level(0.0);
    assert_eq!((rig.state(0), rig.state(1)), (LaneState::Playing, LaneState::Playing));
    rig.set(Command::SetPan(0, -1.0));
    rig.set(Command::SetPan(1, 1.0));
    rig.advance(rig.seconds(0.2));

    rig.set(Command::SetScope(true));
    rig.keep_output();
    rig.advance(rig.seconds(0.2));
    let columns = rig.read_scope();
    // What the two sides of the master bus actually did over the same frames.
    let mean = rig.bus.iter().zip(&rig.bus_right).map(|(l, r)| ((l + r) / 2.0).abs()).fold(0.0f32, f32::max);
    let sides = rig.bus.iter().chain(&rig.bus_right).fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(sides > 0.6, "the lanes play hard apart ({sides})");
    assert!(mean < 0.05 * sides, "a mean tap would read this silent ({mean} against {sides})");
    assert!(span(&columns, SCOPE_MASTER) > 1.2, "the master's column spans both sides: {}", span(&columns, SCOPE_MASTER));
    assert!(columns.iter().all(|c| c.lo[SCOPE_MASTER] < 0.0 && c.hi[SCOPE_MASTER] > 0.0), "every column holds both sides");
    assert_eq!(span(&columns, SCOPE_MONITOR), 0.0, "nothing is played live: the monitor is silent");
}

/// The master's column is the engine's output, not the bus before the limiter: five lanes playing the
/// same loud loop drive the bus well past full scale, and the column sits at the limiter's ceiling.
#[test]
fn the_master_column_is_the_limited_output_not_the_bus_before_it() {
    let mut rig = Rig::new();
    rig.set_level(0.5);
    rig.record_first_take(0, 1, 2400);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.set_level(0.0);
    assert!((0..5).all(|i| rig.state(i) == LaneState::Playing), "five lanes play the same loop");
    rig.set(Command::SetScope(true));
    rig.keep_output();
    rig.advance(rig.seconds(0.3));
    let columns = rig.read_scope();
    let bus = rig.bus.iter().chain(&rig.bus_right).fold(0.0f32, |m, &x| m.max(x.abs()));
    let heard = rig.heard.iter().chain(&rig.heard_right).fold(0.0f32, |m, &x| m.max(x.abs()));
    let (lo, hi) = (columns.iter().fold(0.0f32, |m, c| m.min(c.lo[SCOPE_MASTER])), columns.iter().fold(0.0f32, |m, c| m.max(c.hi[SCOPE_MASTER])));
    assert!(bus > 2.0, "the bus before the limiter is far past full scale ({bus})");
    assert!(heard < 1.05, "the limiter holds the output near full scale ({heard})");
    assert!(hi <= heard && -lo <= heard, "no column reads past what the device was handed ({lo} to {hi}, output {heard})");
    assert!(hi > 0.5, "and the master reads loud, not silent ({hi})");
}

/// The monitor joins the output after the limiter, so a player with no loops at all still sees the
/// master move: the master's column follows the monitor's.
#[test]
fn the_master_column_carries_the_monitor_that_joins_after_the_limiter() {
    let mut rig = Rig::new();
    rig.set_level(0.4);
    rig.set(Command::SetScope(true));
    rig.advance(rig.seconds(0.3));
    let columns = rig.read_scope();
    assert!(!columns.is_empty(), "the columns come with no loop at all");
    assert_eq!((0..5).map(|i| span(&columns, i)).fold(0.0, f32::max), 0.0, "no lane plays");
    let monitor = span(&columns, SCOPE_MONITOR);
    let master = span(&columns, SCOPE_MASTER);
    assert!(monitor > 0.3, "the live input is in the monitor ({monitor})");
    assert!((master - monitor).abs() < 0.01 * monitor, "the master is the monitor, not silence ({master} against {monitor})");
}

/// A full ring drops the columns it cannot take and counts them, rather than making the audio wait;
/// the reader sees the loss in the `frame` sequence, which is what the feed turns into its `gap`.
#[test]
fn a_full_ring_drops_counts_and_breaks_the_sequence_the_reader_splices_by() {
    let mut rig = Rig::new();
    rig.set_input(ramp);
    rig.record_first_take(0, 1, 2400);
    rig.set(Command::SetScope(true));
    let bin = bin(&rig);
    // A reader that stopped reading: more columns than the ring holds.
    rig.advance((SCOPE_CAPACITY as Frame + 64) * bin);
    let dropped = rig.engine.diag().scope_dropped;
    assert!(dropped >= 60, "the refused columns counted ({dropped})");
    let held = rig.read_scope();
    assert_eq!(held.len(), SCOPE_CAPACITY, "the ring handed over what it holds");
    assert!(contiguous(&held, bin), "what it holds is contiguous");

    // The reader is back: the next column does not follow the last one it took, so the trace it holds
    // is broken and the feed says `gap`.
    rig.advance(4 * bin);
    let next = rig.read_scope();
    assert!(!next.is_empty() && contiguous(&next, bin), "the columns go on");
    assert_ne!(next[0].frame, held[held.len() - 1].frame + bin, "the loss shows in the frames");
    assert_eq!(rig.engine.diag().scope_dropped, dropped, "and nothing more was lost once it read again");
}
