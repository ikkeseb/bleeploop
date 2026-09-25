//! The cost bars, measured offline at 48 kHz in 64-frame blocks. Ignored by default (timing on a shared
//! CI runner is noise); run them on the PC, in release:
//!
//!   cargo test -p lf-engine --release --test perf -- --ignored --nocapture --test-threads=1
//!
//! Keep `--test-threads=1`: run in parallel, the two bars slow each other down.
//!
//! - Stage 2: five lanes (one overdubbing), the click and the master limiter under 10 % of the block's
//!   real time (with the Stage 3 sound wired in and idle: bypassed FX, silent instruments).
//! - Stage 3: that engine with every effect on and the drum kit playing, plus the other synths
//!   beside it (below), under 50 %.
//!
//! lf-engine builds at opt-level 3 in the dev profile too, but the Stage 3 load mixes in this file,
//! which the dev profile leaves unoptimized: take the numbers from `--release`. The one test that is
//! not ignored runs the Stage 3 load for a second under `assert_no_alloc`.

mod common;

use std::time::Instant;

use assert_no_alloc::assert_no_alloc;
use common::{code, violation_count, Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam, MAX_FEEDBACK};
use lf_engine::dsp::synth::{Bass, PolyKind, PolySynth};
use lf_engine::grid::Frame;
use lf_engine::{Command, Dry, Instrument, LaneState, ProcessContext};

const RATE: f32 = 48000.0;
const BLOCK: usize = 64;

/// Five lanes playing a 4-bar loop, lane 0 overdubbing, the click on; returns the rig and its master.
fn five_lanes_one_overdubbing() -> (Rig, Frame) {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set(Command::SetMetronome(true));
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 2400);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::RecDub(0));
    assert!((1..5).all(|i| rig.state(i) == LaneState::Playing) && rig.state(0) == LaneState::Overdubbing);
    (rig, master)
}

#[test]
#[ignore]
fn five_lanes_one_overdubbing_and_the_click_cost_under_a_tenth_of_the_block() {
    let (mut rig, master) = five_lanes_one_overdubbing();
    let blocks = 48000 * 60 / BLOCK;
    let input: Vec<f32> = (0..BLOCK).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut frame = rig.frame;
    let mut worst = 0.0f64;
    let mut times = Vec::with_capacity(blocks);
    let started = Instant::now();
    for _ in 0..blocks {
        let t = Instant::now();
        let ctx = ProcessContext { frame, xrun: false, align_frames: 0, input_frames: 0 };
        rig.engine.process(&ctx, &input, &mut left, &mut right, &mut Dry);
        let dt = t.elapsed().as_secs_f64();
        worst = worst.max(dt);
        times.push(dt);
        frame += BLOCK as Frame;
    }
    let total = started.elapsed().as_secs_f64();
    let period = BLOCK as f64 / 48000.0;
    let mean = total / blocks as f64 / period;
    times.sort_by(f64::total_cmp);
    let p999 = times[blocks * 999 / 1000] / period;
    println!(
        "master {master} frames; {blocks} blocks: mean {:.2} %, p99.9 {:.2} %, worst {:.1} % of the block",
        mean * 100.0,
        p999 * 100.0,
        worst / period * 100.0
    );
    assert!(mean < 0.10, "mean block time {:.2} % of the block", mean * 100.0);
}

/// The timed parts of a Stage 3 block, in render order; each includes adding its output into the mix.
const PARTS: [&str; 6] = ["engine: lanes + FX + reverb + drum kit + click + limiter", "lead x8", "pad x12", "piano x12", "organ x8", "bass"];

/// The Stage 3 acceptance load, the worst a session can ask of the built-in sound at once:
///
/// - the engine as the Stage 2 bar runs it (five lanes, one overdubbing, the click), with every effect
///   on every lane: the filter at Q 6 with its cutoff ramping all the time (a new 20 ms ramp every 14
///   blocks: per-frame coefficients), the pitch at +7, the stutter at 1/16, the delay at 1/8 and full
///   feedback (0.95), the reverb send at 1, so the one reverb bus is busy; and the drum kit selected
///   with its 16 voices re-hit every 30 blocks (40 ms, the pedal hat's ring), so every voice sounds;
/// - beside the engine, the other five synths sounding at once, which the engine never asks for (notes
///   reach only the selected instrument; the others only ring out): the four poly synths with every
///   voice held (lead 8, pad 12, piano 12, organ 8), bent, the mod wheel full (the vibrato path live),
///   and the bass re-struck every 188 blocks (~250 ms, its filter envelope moving).
struct Stage3 {
    rig: Rig,
    engine_frame: Frame,
    engine_input: [f32; BLOCK],
    poly: Vec<PolySynth>,
    bass: Bass,
    bass_note: u8,
    drum_notes: Vec<u8>,
    /// The frame the synths beside the engine render next (their clock starts at 0).
    frame: u64,
    /// Blocks rendered.
    block: u64,
    mono: [f32; BLOCK],
    mix: [[f32; BLOCK]; 2],
}

const BASS_NOTES: [u8; 5] = [28, 31, 33, 35, 36];

impl Stage3 {
    fn new() -> Self {
        let (mut rig, _) = five_lanes_one_overdubbing();
        for lane in 0..5 {
            let set = |p, v| Command::SetFxParam(lane, p, v);
            for command in [
                set(FxParam::Cutoff, 1200.0),
                set(FxParam::Q, 6.0),
                set(FxParam::Semitones, 7.0),
                set(FxParam::Rate, 3.0),
                set(FxParam::Time, 1.0),
                set(FxParam::Feedback, MAX_FEEDBACK),
                set(FxParam::Mix, 0.5),
                set(FxParam::Amount, 1.0),
            ] {
                rig.set(command);
            }
            for kind in FxKind::ALL {
                rig.set(Command::SetFxBypass(lane, kind, false));
            }
        }
        rig.set(Command::SelectInstrument(Some(Instrument::Drums)));
        let engine_frame = rig.frame;
        let mut engine_input = [0.0f32; BLOCK];
        for (k, x) in engine_input.iter_mut().enumerate() {
            *x = code(k as Frame) - 0.25;
        }

        let poly = [PolyKind::Lead, PolyKind::Pad, PolyKind::Piano, PolyKind::Organ]
            .into_iter()
            .map(|kind| {
                let mut synth = PolySynth::new(kind, RATE, 0.0, 0);
                for k in 0..kind.spec().max_polyphony as u8 {
                    synth.note_on(48 + 3 * k, 0.8, 0.0, 0);
                }
                synth.set_pitch_bend(0.5, 0.0, 0);
                synth.set_modulation(1.0, 0.0, 0);
                synth
            })
            .collect();
        let mut bass = Bass::new(RATE, 0.0, 0);
        bass.set_modulation(1.0, 0.0, 0);
        // The kit's GM notes (`dsp::synth::drum`).
        let drum_notes = vec![35, 36, 37, 38, 39, 40, 42, 44, 45, 46, 47, 49, 50, 51, 54, 56];

        Stage3 {
            rig,
            engine_frame,
            engine_input,
            poly,
            bass,
            bass_note: BASS_NOTES[0],
            drum_notes,
            frame: 0,
            block: 0,
            mono: [0.0; BLOCK],
            mix: [[0.0; BLOCK]; 2],
        }
    }

    /// Render one block; `parts` gains each part's seconds. Returns the block's seconds.
    fn render(&mut self, parts: &mut [f64; PARTS.len()]) -> f64 {
        let (f, b) = (self.frame, self.block);
        let now = f as f64 / RATE as f64;
        let [l, r] = &mut self.mix;
        let started = Instant::now();
        let mut t = started;
        let mut lap = |part: usize| {
            let next = Instant::now();
            parts[part] += (next - t).as_secs_f64();
            t = next;
        };

        let at = self.engine_frame;
        if b % 14 == 0 {
            for lane in 0..5u8 {
                let cutoff = if (b / 14 + lane as u64) % 2 == 0 { 300.0 } else { 6000.0 };
                self.rig.send_at(at, Command::SetFxParam(lane, FxParam::Cutoff, cutoff));
            }
        }
        if b % 30 == 0 {
            for &note in &self.drum_notes {
                self.rig.send_at(at, Command::NoteOn(note, 1.0));
            }
        }
        let ctx = ProcessContext { frame: at, xrun: false, align_frames: 0, input_frames: 0 };
        self.rig.engine.process(&ctx, &self.engine_input, l, r, &mut Dry);
        lap(0);

        for (i, synth) in self.poly.iter_mut().enumerate() {
            synth.render(f, &mut self.mono);
            for ((l, r), &x) in l.iter_mut().zip(r.iter_mut()).zip(&self.mono) {
                *l += 0.25 * x;
                *r += 0.25 * x;
            }
            lap(1 + i);
        }

        if b % 188 == 0 {
            let next = BASS_NOTES[(b / 188) as usize % BASS_NOTES.len()];
            self.bass.note_on(next, 0.9, now, f);
            if next != self.bass_note {
                self.bass.note_off(self.bass_note, now, f);
            }
            self.bass_note = next;
        }
        self.bass.render(f, &mut self.mono);
        for ((l, r), &x) in l.iter_mut().zip(r.iter_mut()).zip(&self.mono) {
            *l += 0.25 * x;
            *r += 0.25 * x;
        }
        lap(5);

        std::hint::black_box(&self.mix);
        self.frame += BLOCK as u64;
        self.engine_frame += BLOCK as Frame;
        self.block += 1;
        t.duration_since(started).as_secs_f64()
    }
}

/// The Stage 3 bar: the load above under half the block's real time on average. The mean is the bar;
/// p99.9 and the worst block print without a bar (Windows preempts a test thread now and then). The
/// synths compute whole 128-frame quanta, so their work lands on every other 64-frame block: the mean
/// of those blocks prints too. The load repeats every 64 blocks (the reverb's largest FFT stage runs
/// every 32nd quantum), so the median per phase of that cycle is the scheduled worst block, free of
/// preemption.
#[test]
#[ignore]
fn stage3_full_load_costs_under_half_the_block() {
    let mut load = Stage3::new();
    let mut parts = [0.0f64; PARTS.len()];
    // Past the pad's 0.8 s attack and the reverb's 2.62 s IR, then 20 s timed.
    for _ in 0..48000 * 3 / BLOCK {
        load.render(&mut parts);
    }
    let blocks = 48000 * 20 / BLOCK;
    let mut parts = [0.0f64; PARTS.len()];
    let mut times = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        times.push(load.render(&mut parts));
    }
    let period = BLOCK as f64 / 48000.0;
    let pct = |t: f64| t / period * 100.0;
    let total: f64 = times.iter().sum();
    let mean = total / blocks as f64;
    let quantum_blocks = times.iter().step_by(2).sum::<f64>() / blocks.div_ceil(2) as f64;
    let mut sorted = times.clone();
    sorted.sort_by(f64::total_cmp);
    let p999 = sorted[blocks * 999 / 1000];
    let worst = sorted[blocks - 1];
    let phase_median = |p: usize| {
        let mut t: Vec<f64> = times.iter().skip(p).step_by(64).copied().collect();
        t.sort_by(f64::total_cmp);
        t[t.len() / 2]
    };
    let worst_phase = (0..64).map(phase_median).fold(0.0, f64::max);
    println!(
        "stage 3 load, {blocks} blocks of {BLOCK} at 48 k ({:.0} µs): mean {:.1} µs ({:.1} %); blocks that start a quantum {:.1} %; scheduled worst {:.1} %; p99.9 {:.1} %; worst {:.1} %",
        period * 1e6,
        mean * 1e6,
        pct(mean),
        pct(quantum_blocks),
        pct(worst_phase),
        pct(p999),
        pct(worst)
    );
    for (name, t) in PARTS.iter().zip(parts) {
        println!("  {name:<33} {:>6.1} µs  {:>5.2} %", t / blocks as f64 * 1e6, pct(t / blocks as f64));
    }
    assert!(pct(mean) < 50.0, "mean block time {:.1} % of the block", pct(mean));
}

/// The Stage 3 load renders without allocating: a second of it (drum re-hits, bass notes and cutoff
/// ramps included) under `assert_no_alloc` (debug builds check; release builds do not).
#[test]
fn stage3_full_load_renders_without_allocating() {
    let mut load = Stage3::new();
    let mut parts = [0.0f64; PARTS.len()];
    let before = violation_count();
    assert_no_alloc(|| {
        for _ in 0..48000 / BLOCK {
            load.render(&mut parts);
        }
    });
    assert_eq!(violation_count(), before, "the Stage 3 load allocated while rendering");
    assert!(load.mix.iter().flatten().all(|x| x.is_finite()));
}
