//! The looper's sample-level joins under a SUSTAINED tone (STATUS § Not heard yet: "punching out of a
//! sustained note leaves a clean layer seam; the undo swap and reverse are click-free", "TRIM ... no click
//! at either swap"). No web guard precedes this: the other looper tests prove each join lands on its exact
//! frame with the frame code, which says nothing about the audio's continuity across it.
//!
//! The criterion: an audible click is a sample step far above the tone's own slope. Within +-10 ms of a
//! join, the largest |x[n] - x[n-1]| of the played-back loop must stay within `K` times the steady step
//! (the largest one on the same output away from every bar line and punch), plus `EPS` for f32 rounding.
//! `K` = 2: a seamless join has at most the steady step (a repeated sample, as reverse's flip plays, has
//! none), and 2 tolerates a one-frame slip of the tone (a skipped sample doubles its step) on purpose: it
//! is a regression bound, not an audibility proof. The layer cuts measured here step 11 to 13 times the
//! steady step. Each join prints its frame, window, steady step, the window's largest step and where it
//! falls, and asserts the window's RMS, so a window that missed the tone cannot pass. The RMS proves the
//! tone is there, not that the swap happened in the window: the frame-code tests
//! (`overdub_undo_reverse.rs`, `trim.rs`) hold each swap's timing.
//!
//! What is measured: the rig's kept output (the lanes before their FX plus the click and the monitor) over
//! passes where the input is silent, and the test asserts the monitor is silent there, so it is the loop
//! alone. The tone is a whole number of cycles per bar at 120 bpm and 48 kHz (220 Hz in bar 1, 330 Hz in
//! a second bar), its phase pi/4 at every bar line, so the content itself is seamless (a kink at most) at
//! every bar line and loop point: a step measured at a join is the engine's. What this cannot see: a real
//! note's phase where a loop wraps (the engine has no crossfade at a wrap or a swap, so a tone not whole
//! cycles long steps at its loop point by its own phase jump), and a slope reversal (reverse's flip is a
//! kink, no step).

mod common;

use std::f64::consts::PI;

use common::Rig;
use lf_engine::grid::Frame;
use lf_engine::{Command, LaneState};

const AMP: f64 = 0.5;
/// Bar k of a take plays `TONES[k]`: whole cycles per bar (440 and 660 in 2 s), so every bar line joins.
const TONES: [f64; 2] = [220.0, 330.0];
const PHASE: f64 = PI / 4.0;
/// +-10 ms at 48 kHz.
const WINDOW: Frame = 480;
const K: f64 = 2.0;
const EPS: f64 = 1e-6;
/// The window's RMS floor: the 220 Hz tone alone is about 0.35.
const MIN_RMS: f64 = 0.1;

/// The sustained tone at `d` frames from the take's downbeat.
fn tone(d: Frame, fpb: Frame, bars: Frame, sr: u32) -> f32 {
    let freq = TONES[d.div_euclid(fpb).rem_euclid(bars) as usize];
    (AMP * (2.0 * PI * freq * d.rem_euclid(fpb) as f64 / sr as f64 + PHASE).sin()) as f32
}

/// A committed `bars`-bar take of the tone on lane 0 at 120 bpm, PLAYING, the input silent. Returns the
/// rig and the take's downbeat (its loop position 0).
fn sustained_loop(bars: Frame) -> (Rig, Frame) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    let (sr, fpb) = (rig.sr, rig.fpb());
    assert_eq!(fpb, 96_000);
    for freq in TONES {
        assert_eq!((freq * fpb as f64 / sr as f64).fract(), 0.0, "{freq} Hz is whole cycles per bar");
    }
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * fpb / 4;
    rig.set_input(move |f| tone(f - downbeat, fpb, bars, sr));
    rig.advance_to(downbeat + bars * fpb + 2400);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.25));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!((rig.state(0), rig.master()), (LaneState::Playing, bars * fpb));
    assert_eq!((rig.anchor() - downbeat).rem_euclid(rig.master()), 0, "the take starts on its downbeat");
    (rig, downbeat)
}

/// Lane 0's 1-bar tone loop with a layer of the same tone held across its loop point: DUB at three
/// quarters of a pass, punched out half-way through the next. The input is silent from the punch-out.
/// Returns the rig, the loop before the layer, and the punch-in and punch-out press frames.
fn dubbed_across_the_loop_point() -> (Rig, Vec<f32>, Frame, Frame) {
    let (mut rig, downbeat) = sustained_loop(1);
    let (sr, fpb, master) = (rig.sr, rig.fpb(), rig.master());
    let pre = rig.pcm(0);
    rig.advance_to(rig.next_boundary() - master / 4);
    rig.set_input(move |f| tone(f - downbeat, fpb, 1, sr));
    let punch_in = rig.frame;
    rig.press(Command::RecDub(0));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.advance_to(rig.next_boundary() + master / 2);
    let punch_out = rig.frame;
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    // The layer sounds at both punches: not on a zero crossing, where a cut would step by nothing.
    let layer = |f: Frame| {
        let pos = (f - rig.anchor()).rem_euclid(master) as usize;
        rig.pcm(0)[pos] - pre[pos]
    };
    for f in [punch_in, punch_out - 1] {
        assert!(layer(f).abs() as f64 >= 0.3 * AMP, "the layer at frame {f} is {}", layer(f));
    }
    (rig, pre, punch_in, punch_out)
}

/// One join's numbers.
struct Seam {
    name: &'static str,
    frame: Frame,
    max_step: f64,
    at: Frame,
    rms: f64,
    limit: f64,
}

/// The joins over the kept output: the steady step away from every bar line and `joins` frame, then each
/// window's largest step and RMS, printed.
fn measure(rig: &Rig, joins: &[(&'static str, Frame)]) -> Vec<Seam> {
    let (start, out) = rig.output.as_ref().expect("keep_output first");
    let (start, end) = (*start, start + out.len() as Frame);
    assert!(rig.monitor.iter().all(|&m| m == 0.0), "the input is silent: the output is the loop alone");
    let (anchor, fpb) = (rig.anchor(), rig.fpb());
    let x = |f: Frame| out[(f - start) as usize] as f64;
    let near_join = |f: Frame| {
        let bar = (f - anchor).rem_euclid(fpb);
        bar.min(fpb - bar) <= WINDOW || joins.iter().any(|&(_, j)| (f - j).abs() <= WINDOW)
    };
    let (mut steady, mut peak) = (0.0f64, 0.0f64);
    for f in start + 1..end {
        if !near_join(f) {
            steady = steady.max((x(f) - x(f - 1)).abs());
            peak = peak.max(x(f).abs());
        }
    }
    // The steady step is the tone's own slope: an unexpected step outside the windows would raise the limit.
    let slope = 2.0 * PI * TONES[1] / rig.sr as f64 * peak;
    assert!(steady > 0.0 && steady <= 1.05 * slope, "steady step {steady:.5} is the tone's (at most {slope:.5})");
    let limit = K * steady + EPS;
    joins
        .iter()
        .map(|&(name, frame)| {
            let (lo, hi) = (frame - WINDOW, frame + WINDOW);
            assert!(lo > start && hi < end, "{name}: the window [{lo}, {hi}] lies in the kept output [{start}, {end})");
            let (mut max_step, mut at, mut sum) = (0.0f64, lo, 0.0f64);
            for f in lo..=hi {
                let step = (x(f) - x(f - 1)).abs();
                if step > max_step {
                    (max_step, at) = (step, f);
                }
                sum += x(f) * x(f);
            }
            let rms = (sum / (hi - lo + 1) as f64).sqrt();
            let pos = (frame - anchor).rem_euclid(rig.master());
            println!(
                "{name}: join frame {frame} (loop position {pos}), window [{lo}, {hi}], steady step {steady:.5}, window max step {max_step:.5} at frame {at} ({:+}), rms {rms:.3}, limit {limit:.5}",
                at - frame
            );
            Seam { name, frame, max_step, at, rms, limit }
        })
        .collect()
}

/// Every seam within its limit over a tone that is there; a failure names each seam over it.
fn assert_clean(seams: &[Seam]) {
    for s in seams {
        assert!(s.rms >= MIN_RMS, "{}: the window's RMS {:.3} is the tone's", s.name, s.rms);
    }
    let over: Vec<String> = seams
        .iter()
        .filter(|s| s.max_step > s.limit)
        .map(|s| format!("{}: a step of {:.5} at frame {} ({:+} from the join at {}) over the limit {:.5}", s.name, s.max_step, s.at, s.at - s.frame, s.frame, s.limit))
        .collect();
    assert!(over.is_empty(), "{}", over.join("; "));
}

#[test]
fn e_the_first_takes_loop_point_wraps_without_a_step() {
    let (mut rig, _) = sustained_loop(1);
    let master = rig.master();
    let wrap = rig.next_boundary() + master;
    rig.advance_to(wrap - master / 2);
    rig.keep_output();
    rig.advance_to(wrap + master / 2);
    assert_clean(&measure(&rig, &[("loop point", wrap)]));
}

#[test]
#[ignore = "red, STATUS D23: a layer's edges are hard cuts: its start steps 0.364 and the punch-out 0.333, over a limit of 0.058"]
fn a_punching_out_of_a_sustained_note_leaves_a_clean_layer_seam() {
    let (mut rig, _, punch_in, punch_out) = dubbed_across_the_loop_point();
    let master = rig.master();
    rig.keep_output();
    let (start, wrap, end) = (punch_in + master, punch_out - master / 2 + master, punch_out + master);
    rig.advance_to(end + master / 4);
    assert!(rig.output.as_ref().unwrap().0 < start - WINDOW);
    assert_clean(&measure(&rig, &[("layer start", start), ("layer across the loop point", wrap), ("punch-out", end)]));
}

#[test]
#[ignore = "red, STATUS D23: the undo swap cuts a layer sounding at the loop point: a step of 0.333, over a limit of 0.058"]
fn b_the_undo_swap_is_click_free() {
    let (mut rig, pre, _, _) = dubbed_across_the_loop_point();
    let master = rig.master();
    // Kept from past the layer's start (three quarters in), so the output holds no layer edge.
    rig.advance_to(rig.next_boundary() - master / 4 + 4 * WINDOW);
    let swap = rig.next_boundary();
    let layer = rig.pcm(0)[master as usize - 1] - pre[master as usize - 1];
    println!("the layer at the last loop position: {layer:.5}");
    rig.keep_output();
    rig.press(Command::Undo(0));
    assert_eq!(rig.pcm(0), pre, "the undo gives back the loop before the layer");
    rig.advance_to(swap + master / 2);
    assert_clean(&measure(&rig, &[("undo swap", swap)]));
}

#[test]
fn c_reverse_while_playing_flips_without_a_step() {
    let (mut rig, _) = sustained_loop(1);
    let master = rig.master();
    rig.advance_to(rig.next_boundary() + master / 4);
    let flip = rig.next_boundary();
    rig.keep_output();
    rig.press(Command::Reverse(0));
    rig.advance_to(flip + master + master / 2);
    assert!(rig.lane(0).reversed);
    assert_clean(&measure(&rig, &[("reverse flip", flip), ("reversed loop point", flip + master)]));
}

#[test]
fn d_trim_and_its_undo_swap_without_a_step() {
    let (mut rig, _) = sustained_loop(2);
    let (master, fpb) = (rig.master(), rig.fpb());
    let before = rig.pcm(0);
    rig.advance_to(rig.next_boundary() + master / 2 + fpb / 4);
    let trim = rig.next_boundary();
    rig.keep_output();
    rig.press(Command::Trim(0, 1));
    rig.advance_to(trim + master / 2 + fpb / 4);
    rig.idle();
    let trimmed: Vec<f32> = (0..master as usize).map(|k| before[k % fpb as usize]).collect();
    assert!(rig.pcm(0) == trimmed && trimmed != before, "the first bar, twice: not the take");
    let undo = trim + master;
    rig.press(Command::Undo(0));
    rig.advance_to(undo + fpb / 2);
    rig.idle();
    assert_eq!(rig.pcm(0), before, "UNDO gives back both bars");
    assert_clean(&measure(&rig, &[("trim swap", trim), ("trimmed bar's repeat", trim + fpb), ("trim's undo swap", undo)]));
}
