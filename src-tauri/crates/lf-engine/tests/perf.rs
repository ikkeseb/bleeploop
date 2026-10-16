//! The cost bars, measured offline at 48 kHz in 64-frame blocks. Ignored by default (timing on a shared
//! CI runner is noise); run them on the PC, in release:
//!
//!   cargo test -p lf-engine --release --test perf -- --ignored --nocapture --test-threads=1
//!
//! Keep `--test-threads=1`: run in parallel, the two bars slow each other down.
//!
//! - Stage 2: five lanes (one overdubbing), the click and the master limiter under 10 % of the block's
//!   real time (with the Stage 3 sound wired in and idle: bypassed FX, silent instruments).
//! - Stage 3: that engine with every effect on, every input send on and the drum kit playing, plus
//!   the other synths beside it (below), under 50 %.
//! - The input sends (F15, F24): what ECHO, REVERB and RING MOD on the live input add to the Stage 2 engine, printed
//!   beside it (no bar of their own; the Stage 3 bar carries them).
//! - A lane's pan: the Stage 2 engine with every lane centred, then every lane panned and resting, then
//!   every lane's pan always moving, printed side by side (no bar of its own).
//! - The live scope taps: the Stage 2 engine with them off, then on with a reader keeping up, printed
//!   side by side (no bar of its own).
//! - The master at 64 and 128 frames: the Stage 2 engine quiet and in sustained gain reduction, and one
//!   master limiter alone, under and over its threshold on every frame, each block's median beside the
//!   mean (no bar of its own).
//! - A multiply burst (F14): the eight extension jobs of the worst multiply add under 10 % to the blocks
//!   they run in.
//! - A TRIM (F16) of the longest loop (32 bars, the whole 60 s buffer): its job adds under 10 % to the
//!   blocks it runs in, and no step of it moves more than `JOB_RATE` positions a frame.
//! - The damage lane (`api::damage`): the Stage 2 engine with slot 0 a live Delay(1024) and slot 1 an
//!   instrument, so both crossfade branches run, at 64 and 128 frames, with no dropout and with a damaged
//!   block every 1,024 frames, beside the audio's hash. Its bar: no dropout adds at most 0.5 % of a
//!   64-frame block over the engine before the lane (`origin/dev`, the same test compiled there).
//! - A hole (`Looper::fill_hole`): the whole callback that zeroes a 59 s hole closing a 60 s take, less
//!   the same callback without it, at 48 and 192 kHz, and a one-block lock miss's hole at 64 frames. Its
//!   bars: at most 1 ns per lost frame, the one-block hole at most 1 % of its block.
//! - A hole's deadline: the callback that finds the largest hole each backend makes, a RETAKE pass edge
//!   overdue in it, on the heavy scene's heaviest phase, at the smallest periods (ASIO 32 and 64 frames,
//!   WASAPI's 10 ms) and 44.1 to 192 kHz, the worst of eight. Its bar: at most a quarter of the period
//!   more than the scene's callback on that phase (the test says why).
//! - A multiply's push with every source bin marked, over four lanes with undo targets (eight fills'
//!   marks at once): at most 5 % of a 64-frame block more than the same push unmarked.
//!
//! lf-engine builds at opt-level 3 in the dev profile too, but the Stage 3 load mixes in this file,
//! which the dev profile leaves unoptimized: take the numbers from `--release`. The one test that is
//! not ignored runs the Stage 3 load for a second under `assert_no_alloc`.
//!
//! What a change is compared against (dev PC, release, five runs): the Stage 2 bar reads 3.8 to 4.0 %
//! (0.33 % before the Stage 3 sound was wired: Blink renders a connected node whether or not it sounds,
//! and the port does the same, split about evenly between the bypassed chains with the idle reverb bus
//! and the six silent instruments). The Stage 3 load reads a mean of 24.7 to 25.0 % of the 1333 µs
//! block, the engine's share 18.2 to 18.4 %; the synths and FX compute whole 128-frame quanta, so every
//! other 64-frame block carries their work (47.8 to 48.4 %, the worst of the load's own cycle 51.9 to
//! 52.6 %). Its p99.9 (79 to 80 %) and worst block (89 to 127 %) are preemption on a busy desktop at
//! normal priority. A Stage 3 mean near 33 % instead of 25 % is the codegen split `src-tauri/Cargo.toml`
//! guards against. The Share mix's second limiter (six runs alternating with its parent's build, on a
//! desktop busy enough to put the Stage 2 mean at 4.3 to 8.3 % and the Stage 3 mean at 34 to 49 % in
//! both): the engine's median block grew by 2 to 8 µs (0.2 to 0.3 point), inside the runs' spread. One
//! master limiter alone (four runs, as busy) takes 2.3 µs of a 64-frame block under its threshold and
//! 3.3 µs over it (0.17 and 0.25 %), 4.7 and 6.6 µs of a 128-frame block (0.18 and 0.25 %).

mod common;

use std::time::Instant;

use common::refs::sha256_f32;
use lf_engine::{SlotEvent, SlotKind, SlotProcessor};

use assert_no_alloc::assert_no_alloc;
use common::{code, violation_count, Opts, Rig};
use lf_engine::dsp::compressor::{Compressor, Params};
use lf_engine::dsp::fx::{FxKind, FxParam, MAX_FEEDBACK};
use lf_engine::dsp::synth::{Bass, PolyKind, PolySynth};
use lf_engine::grid::Frame;
use lf_engine::looper::{job_frames, JOB_RATE};
use lf_engine::{Command, InputSend, InputSendParam, Instrument, LaneState, NoteTarget, ProcessContext};

const RATE: f32 = 48000.0;
const BLOCK: usize = 64;

/// Five lanes playing a 4-bar loop, lane 0 overdubbing, the click on; returns the rig and its master.
fn five_lanes_one_overdubbing() -> (Rig, Frame) {
    five_lanes_one_overdubbing_from(code)
}

/// [`five_lanes_one_overdubbing`], the take recorded from `input`.
fn five_lanes_one_overdubbing_from(input: impl Fn(Frame) -> f32 + 'static) -> (Rig, Frame) {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set(Command::SetMetronome(true));
    rig.set_input(input);
    let master = rig.record_first_take(0, 4, 2400);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::RecDub(0));
    assert!((1..5).all(|i| rig.state(i) == LaneState::Playing) && rig.state(0) == LaneState::Overdubbing);
    (rig, master)
}

/// Every input send on, at its heaviest: the echo at full level and feedback 0.95, the reverb and the
/// ring at full level (the rig's slot 0 is live, so they hear the input).
fn input_sends_on(rig: &mut Rig) {
    for command in [
        Command::SetInputSendParam(InputSendParam::EchoTime, 1.0),
        Command::SetInputSendParam(InputSendParam::EchoFeedback, MAX_FEEDBACK),
        Command::SetInputSendParam(InputSendParam::EchoLevel, 1.0),
        Command::SetInputSendParam(InputSendParam::ReverbLevel, 1.0),
        Command::SetInputSendParam(InputSendParam::RingFreq, 1500.0),
        Command::SetInputSendParam(InputSendParam::RingLevel, 1.0),
        Command::SetInputSend(InputSend::Echo, true),
        Command::SetInputSend(InputSend::Reverb, true),
        Command::SetInputSend(InputSend::Ring, true),
    ] {
        rig.set(command);
    }
}

/// Render `blocks` blocks of 64 with a busy input; returns the mean, p99.9 and worst block time as a
/// fraction of the block's real time.
fn time_blocks(rig: &mut Rig, blocks: usize) -> (f64, f64, f64) {
    let input: Vec<f32> = (0..BLOCK).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut frame = rig.frame;
    let mut worst = 0.0f64;
    let mut times = Vec::with_capacity(blocks);
    let started = Instant::now();
    for _ in 0..blocks {
        let t = Instant::now();
        let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
        rig.engine.process(&ctx, &input, &mut left, &mut right);
        let dt = t.elapsed().as_secs_f64();
        worst = worst.max(dt);
        times.push(dt);
        frame += BLOCK as Frame;
    }
    rig.frame = frame;
    let total = started.elapsed().as_secs_f64();
    let period = BLOCK as f64 / 48000.0;
    times.sort_by(f64::total_cmp);
    (total / blocks as f64 / period, times[blocks * 999 / 1000] / period, worst / period)
}

#[test]
#[ignore]
fn five_lanes_one_overdubbing_and_the_click_cost_under_a_tenth_of_the_block() {
    let (mut rig, master) = five_lanes_one_overdubbing();
    let blocks = 48000 * 60 / BLOCK;
    let (mean, p999, worst) = time_blocks(&mut rig, blocks);
    println!(
        "master {master} frames; {blocks} blocks: mean {:.2} %, p99.9 {:.2} %, worst {:.1} % of the block",
        mean * 100.0,
        p999 * 100.0,
        worst * 100.0
    );
    assert!(mean < 0.10, "mean block time {:.2} % of the block", mean * 100.0);
}

/// The master's cost at the two ASIO sizes the play path runs, 64 and 128 frames: the Stage 2 engine,
/// then the same five lanes recorded from a loud sine, so the five copies sum far past the limiter's
/// threshold and its gain reduction never lets go (the sine still dips under the threshold around each
/// zero crossing, where a frame skips the curve's log and power); then one master limiter alone, fed a
/// square wave whose magnitude stays under the threshold on every frame (quiet) or over it on every
/// frame (limiting, checked outside the timing): what the Share mix's second limiter adds at its least
/// and at its most. 30 s each; the median block prints beside the mean, since a busy desktop moves the
/// mean by more than the limiter costs. No bar of its own (the Stage 2 bar carries it): the numbers
/// print, to compare a change to the master with its parent.
#[test]
#[ignore]
fn the_master_cost_at_64_and_128_frames() {
    let loud = |f: Frame| 0.8 * (f as f32 * std::f32::consts::TAU * 220.0 / RATE).sin();
    let square = |f: Frame| if (f as f32 * 220.0 / RATE).fract() < 0.5 { 1.0f32 } else { -1.0 };
    let threshold = 10f32.powf(Params::MASTER_LIMITER.threshold / 20.0);
    let row = |name: &str, block: usize, mut times: Vec<f64>| {
        let period = block as f64 / 48000.0;
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        times.sort_by(f64::total_cmp);
        let (median, p999, worst) = (times[times.len() / 2], times[times.len() * 999 / 1000], times[times.len() - 1]);
        println!(
            "{name:<16} {block:>3} frames: median {:.1} µs ({:.2} %), mean {:.1} µs ({:.2} %), p99.9 {:.2} %, worst {:.1} %",
            median * 1e6,
            median / period * 100.0,
            mean * 1e6,
            mean / period * 100.0,
            p999 / period * 100.0,
            worst / period * 100.0
        );
    };
    for block in [64, 128] {
        let blocks = 48000 * 30 / block;
        for (name, (mut rig, _)) in [("engine, stage 2", five_lanes_one_overdubbing()), ("engine, limiting", five_lanes_one_overdubbing_from(loud))] {
            let input: Vec<f32> = (0..block).map(|k| code(k as Frame) - 0.25).collect();
            let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
            let mut times = Vec::with_capacity(blocks);
            for _ in 0..blocks {
                let ctx = ProcessContext { frame: rig.frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                let t = Instant::now();
                rig.engine.process(&ctx, &input, &mut left, &mut right);
                times.push(t.elapsed().as_secs_f64());
                rig.frame += block as Frame;
            }
            row(name, block, times);
        }
        for (name, level) in [("limiter, quiet", 0.25f32), ("limiter, limiting", 4.0)] {
            let mut limiter = Compressor::master_limiter(RATE);
            let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
            let mut times = Vec::with_capacity(blocks);
            for b in 0..blocks {
                let frame = (b * block) as Frame;
                for (k, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
                    *l = level * square(frame + k as Frame);
                    *r = *l;
                }
                assert!(left.iter().all(|x| (x.abs() > threshold) == (level > threshold)), "{name}: a frame on the wrong side of the threshold");
                let t = Instant::now();
                limiter.process(frame, &mut left, &mut right);
                times.push(t.elapsed().as_secs_f64());
                std::hint::black_box(&left);
            }
            row(name, block, times);
        }
    }
}

/// The multiply burst (F14): four one-bar loops, each with an undo target, multiplied to 30 bars (the
/// 60 s buffer at 120 BPM) by a FIXED take on lane 4, the click on. From the commit, eight extension
/// jobs run one after another for about 0.45 s; the blocks they run in may cost at most a tenth of the
/// block more than the same engine's blocks after them, on average. The worst block prints without a bar.
#[test]
#[ignore]
fn a_multiply_burst_costs_under_a_tenth_of_the_block_more() {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, block: BLOCK, ..Default::default() });
    rig.set(Command::SetMetronome(true));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    for _ in 1..4 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    for lane in 0..4 {
        rig.press(Command::RecDub(lane));
        rig.advance(master);
        rig.press(Command::RecDub(lane));
        rig.idle();
    }
    assert!((0..4).all(|i| rig.lane(i).can_undo && rig.state(i) == LaneState::Playing));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(30.0));
    rig.press(Command::RecDub(4));
    let end = rig.end_frame();
    assert_eq!(end - rig.start_frame(), 30 * master, "a 30-bar window over the one-bar loop");
    rig.advance_to(end - BLOCK as Frame);
    assert!(!rig.engine.looper().busy());

    let input: Vec<f32> = (0..BLOCK).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut frame = rig.frame;
    let mut block = |engine: &mut lf_engine::Engine| {
        let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
        let t = Instant::now();
        engine.process(&ctx, &input, &mut left, &mut right);
        frame += BLOCK as Frame;
        t.elapsed().as_secs_f64()
    };
    let mut burst = vec![block(&mut rig.engine)]; // the take's last block: the commit is due in the next
    while burst.len() < 2 || rig.engine.looper().busy() {
        burst.push(block(&mut rig.engine));
    }
    assert_eq!(rig.master(), 30 * master, "the multiply committed");
    let after: Vec<f64> = (0..burst.len()).map(|_| block(&mut rig.engine)).collect();
    let period = BLOCK as f64 / 48000.0;
    let pct = |t: &[f64]| t.iter().sum::<f64>() / t.len() as f64 / period * 100.0;
    let worst = burst.iter().copied().fold(0.0, f64::max) / period * 100.0;
    let (during, steady) = (pct(&burst), pct(&after));
    println!("multiply burst, {} blocks of {BLOCK}: mean {during:.1} % (after it {steady:.1} %), worst {worst:.1} % of the block", burst.len());
    assert!(during - steady < 10.0, "the burst costs {:.1} % of the block more", during - steady);
}

/// The TRIM burst (F16): a 32-bar loop at 128 BPM, the whole 60 s lane buffer, on five playing lanes with
/// the click on, lane 0 trimmed to its first half. The job copies the loop at `JOB_RATE` positions a frame
/// (about 59 ms); the blocks it runs in may cost at most a tenth of the block more than the same engine's
/// blocks after it, on average, and no step moves more than `JOB_RATE` positions a rendered frame. The
/// worst block prints without a bar.
#[test]
#[ignore]
fn a_trim_of_the_longest_loop_costs_under_a_tenth_of_the_block_more() {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, block: BLOCK, ..Default::default() });
    rig.set(Command::SetBpm(128.0));
    rig.set(Command::SetMetronome(true));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(32.0));
    rig.set_input(code);
    rig.press(Command::RecDub(0));
    while rig.master() == 0 {
        rig.advance(BLOCK as Frame);
    }
    let master = rig.master();
    assert_eq!(master, 32 * rig.fpb(), "32 bars fill the 60 s buffer");
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    assert!((0..5).all(|i| rig.state(i) == LaneState::Playing));

    let input: Vec<f32> = (0..BLOCK).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    rig.send_at(rig.frame, Command::Trim(0, 16));
    let mut frame = rig.frame;
    let mut block = |engine: &mut lf_engine::Engine| {
        let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
        let t = Instant::now();
        engine.process(&ctx, &input, &mut left, &mut right);
        frame += BLOCK as Frame;
        t.elapsed().as_secs_f64()
    };
    let mut burst = Vec::new();
    while burst.is_empty() || rig.engine.looper().busy() {
        burst.push(block(&mut rig.engine));
    }
    assert!(burst.len() as Frame >= job_frames(master) / BLOCK as Frame, "the job ran over {} blocks", burst.len());
    let after: Vec<f64> = (0..burst.len()).map(|_| block(&mut rig.engine)).collect();
    let period = BLOCK as f64 / 48000.0;
    let pct = |t: &[f64]| t.iter().sum::<f64>() / t.len() as f64 / period * 100.0;
    let worst = burst.iter().copied().fold(0.0, f64::max) / period * 100.0;
    let (during, steady) = (pct(&burst), pct(&after));
    let step = rig.engine.looper().job_step_max();
    println!("trim of {master} frames, {} blocks of {BLOCK}: mean {during:.1} % (after it {steady:.1} %), worst {worst:.1} % of the block; largest job step {step} positions", burst.len());
    assert!(step <= JOB_RATE * BLOCK as Frame, "a job step moved {step} positions");
    assert!(during - steady < 10.0, "the trim costs {:.1} % of the block more", during - steady);
    assert!(rig.lane(0).can_undo && rig.master() == master);
}

/// What the input sends add: the Stage 2 engine timed with them off, then (past the reverb's IR and the
/// echo's build-up) with all three on at their heaviest, 20 s each, in one run.
#[test]
#[ignore]
fn the_input_sends_cost() {
    let (mut rig, _) = five_lanes_one_overdubbing();
    let blocks = 48000 * 20 / BLOCK;
    let period = BLOCK as f64 / 48000.0;
    let (off, ..) = time_blocks(&mut rig, blocks);
    input_sends_on(&mut rig);
    time_blocks(&mut rig, 48000 * 3 / BLOCK);
    let (on, p999, worst) = time_blocks(&mut rig, blocks);
    println!(
        "stage 2 engine, {blocks} blocks of {BLOCK} at 48 k: sends off mean {:.1} µs ({:.2} %); all on mean {:.1} µs ({:.2} %), p99.9 {:.2} %, worst {:.1} %; the sends add {:.1} µs ({:.2} %)",
        off * period * 1e6,
        off * 100.0,
        on * period * 1e6,
        on * 100.0,
        p999 * 100.0,
        worst * 100.0,
        (on - off) * period * 1e6,
        (on - off) * 100.0
    );
}

/// What the live scope taps cost (`lf_engine::scope`): the Stage 2 engine timed with them off, then
/// with them on and a reader keeping up (the feed drains about 60 times a second, so the ring never
/// fills), 20 s each, in one run. No bar of its own: the two numbers print side by side so a later
/// change to the fold or the column size can see what it did.
#[test]
#[ignore]
fn the_scope_taps_cost() {
    let (mut rig, _) = five_lanes_one_overdubbing();
    let blocks = 48000 * 20 / BLOCK;
    let period = BLOCK as f64 / 48000.0;
    let (off, ..) = time_blocks(&mut rig, blocks);
    rig.set(Command::SetScope(true));
    // In rounds, drained between them as the feed drains: a ring left to fill would stop counting the
    // push, which is the part the scope adds beside the fold.
    const ROUND: usize = 256;
    let rounds = blocks / ROUND;
    let mut on = 0.0;
    let mut columns = 0;
    for _ in 0..rounds {
        let (mean, ..) = time_blocks(&mut rig, ROUND);
        on += mean / rounds as f64;
        columns += rig.drop_scope();
    }
    assert_eq!(rig.engine.diag().scope_dropped, 0, "the reader kept up");
    println!(
        "stage 2 engine, {blocks} blocks of {BLOCK} at 48 k: scope off mean {:.1} µs ({:.2} %); on mean {:.1} µs ({:.2} %), {columns} columns; the taps add {:.1} µs ({:.2} %)",
        off * period * 1e6,
        off * 100.0,
        on * period * 1e6,
        on * 100.0,
        (on - off) * period * 1e6,
        (on - off) * 100.0
    );
}

/// What a lane's pan costs: the Stage 2 engine timed with every lane centred, then with every lane
/// panned and resting, then with every lane's pan moving all the time (a glide across most of the range
/// restarted every 32 blocks, about four time constants), 20 s each, in one run.
#[test]
#[ignore]
fn the_lanes_pan_cost() {
    let (mut rig, _) = five_lanes_one_overdubbing();
    let blocks = 48000 * 20 / BLOCK;
    let period = BLOCK as f64 / 48000.0;
    let (centre, ..) = time_blocks(&mut rig, blocks);
    for (lane, pan) in [-0.7, -0.3, 0.1, 0.5, 0.9].into_iter().enumerate() {
        rig.set(Command::SetPan(lane as u8, pan));
    }
    time_blocks(&mut rig, 48000 / BLOCK);
    assert!((0..5).all(|lane| rig.engine.fx().pan_position(lane) == rig.engine.fx().pan(lane) as f64), "every pan rests");
    let (resting, ..) = time_blocks(&mut rig, blocks);
    let (mut moving, mut worst) = (0.0, 0.0f64);
    let rounds = blocks / 32;
    for round in 0..rounds {
        let side = if round % 2 == 0 { -0.9 } else { 0.9 };
        for lane in 0..5u8 {
            rig.send_at(rig.frame, Command::SetPan(lane, side));
        }
        let (mean, _, w) = time_blocks(&mut rig, 32);
        moving += mean / rounds as f64;
        worst = worst.max(w);
        assert!((0..5).all(|lane| rig.engine.fx().pan_position(lane) != side as f64), "every pan is still moving");
    }
    let pct = |x: f64| x * 100.0;
    println!(
        "stage 2 engine, {blocks} blocks of {BLOCK} at 48 k: centred mean {:.1} µs ({:.2} %); panned, resting {:.1} µs ({:.2} %); moving {:.1} µs ({:.2} %, worst block {:.1} %); resting adds {:.2} %, moving {:.2} %",
        centre * period * 1e6,
        pct(centre),
        resting * period * 1e6,
        pct(resting),
        moving * period * 1e6,
        pct(moving),
        pct(worst),
        pct(resting - centre),
        pct(moving - centre)
    );
}

/// The timed parts of a Stage 3 block, in render order; each includes adding its output into the mix.
const PARTS: [&str; 6] = ["engine: lanes + FX + reverb + input sends + drum kit + click + limiter", "lead x8", "pad x12", "piano x12", "organ x8", "bass"];

/// The Stage 3 acceptance load, the worst a session can ask of the built-in sound at once:
///
/// - the engine as the Stage 2 bar runs it (five lanes, one overdubbing, the click), with every effect
///   on every lane: the filter at Q 6 with its cutoff ramping all the time (a new 20 ms ramp every 14
///   blocks: per-frame coefficients), the pitch at +7, the stutter at 1/16, the delay at 1/8 and full
///   feedback (0.95), the reverb send at 1, so the one reverb bus is busy; every input send on at
///   its heaviest (the echo at feedback 0.95, every level 1) on the live input; and the drum kit selected
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
        input_sends_on(&mut rig);
        rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Drums)));
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
        let ctx = ProcessContext { frame: at, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
        self.rig.engine.process(&ctx, &self.engine_input, l, r);
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


// ── The damage lane, a hole, and a marked multiply ───────────────────────────────────────────────────

/// A pure delay of `latency` frames on a ring (no allocation, constant work per frame).
struct RingDelay {
    line: Vec<f32>,
    next: usize,
}

impl SlotProcessor for RingDelay {
    fn kind(&self) -> SlotKind {
        SlotKind::Effect
    }
    fn latency(&self) -> Frame {
        self.line.len() as Frame
    }
    fn process(&mut self, _frame: Frame, input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (w, &x) in out.iter_mut().zip(input) {
            *w = std::mem::replace(&mut self.line[self.next], x);
            self.next = (self.next + 1) % self.line.len();
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// An instrument plugin stand-in: a quiet hum, notes or not.
struct Hum;

impl SlotProcessor for Hum {
    fn kind(&self) -> SlotKind {
        SlotKind::Instrument
    }
    fn latency(&self) -> Frame {
        256
    }
    fn process(&mut self, frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
        for (k, o) in out.iter_mut().enumerate() {
            *o = 0.01 * ((frame + k as Frame) % 89) as f32 / 89.0;
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// The damage lane's scenario at `block` frames: the Stage 2 engine (five lanes, lane 0 overdubbing, the
/// click), slot 0 a live Delay(1024), slot 1 the instrument; 30 s timed, a damaged block every 1,024
/// frames when `damage`. Returns the mean, p99.9 and worst block as a fraction of its period, and the
/// hash of the record tap, the looper's mix and every lane after.
fn damage_lane_run(block: usize, damage: bool) -> (f64, f64, f64, String) {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, block, ..Default::default() });
    rig.install(0, Box::new(RingDelay { line: vec![0.0; 1024], next: 0 }));
    rig.install(1, Box::new(Hum));
    rig.set(Command::SetMetronome(true));
    rig.set_input(code);
    let master = rig.record_first_take(0, 4, 2400);
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::RecDub(0));
    assert!(master > 0 && rig.state(0) == LaneState::Overdubbing);
    let input: Vec<f32> = (0..block).map(|k| code(k as Frame) - 0.25).collect();
    let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
    let (mut record, mut looper) = (Vec::new(), Vec::new());
    let blocks = 48_000 * 30 / block;
    let mut times = Vec::with_capacity(blocks);
    let mut frame = rig.frame;
    for k in 0..blocks {
        let damaged = damage && (k * block) % 1024 < block;
        let ctx = ProcessContext { frame, xrun: false, damaged, align_frames: 0, input_frames: 0 };
        let t = Instant::now();
        rig.engine.process(&ctx, &input, &mut left, &mut right);
        times.push(t.elapsed().as_secs_f64());
        let taps = rig.engine.taps();
        record.extend_from_slice(taps.record);
        looper.extend_from_slice(taps.looper);
        frame += block as Frame;
    }
    rig.frame = frame;
    let lanes: Vec<Vec<f32>> = (0..5).map(|i| rig.pcm(i)).collect();
    let hash = sha256_f32(&[&record, &looper, &lanes[0], &lanes[1], &lanes[2], &lanes[3], &lanes[4]]);
    let period = block as f64 / 48_000.0;
    let mean = times.iter().sum::<f64>() / blocks as f64 / period;
    times.sort_by(f64::total_cmp);
    (mean, times[blocks * 999 / 1000] / period, times[blocks - 1] / period, hash)
}

/// The damage lane's cost (run 2 against `origin/dev`'s run 1: the same test there, `cargo test -p
/// lf-engine --release --test perf -- --ignored --nocapture --test-threads=1 the_damage_lane_cost`) and
/// with dropouts in flight (run 3). The hash of run 2 must equal run 1's: undamaged audio is
/// bit-identical with the lane in.
#[test]
#[ignore]
fn the_damage_lane_cost() {
    for block in [64usize, 128] {
        for damage in [false, true] {
            let (mean, p999, worst, hash) = damage_lane_run(block, damage);
            let period = block as f64 / 48_000.0 * 1e6;
            println!(
                "damage lane, {block}-frame blocks, {}: mean {:.2} µs ({:.2} %), p99.9 {:.2} %, worst {:.1} %; hash {hash}",
                if damage { "a damaged block every 1,024 frames" } else { "no dropout" },
                mean * period,
                mean * 100.0,
                p999 * 100.0,
                worst * 100.0
            );
        }
    }
}

/// The callback that finds a `hole`-frame hole at the end of a take of `seconds` at `rate` (a FIXED take
/// the length of the buffer; the device skips `hole` frames from the window's last `hole` on, `block`
/// frames a callback), and the same callback with no hole: seconds each, the best of `tries`. With
/// `retake` the take rolls two passes of half the buffer, and the jump cuts the first a second in and
/// lands a second before the second's end: an overdue pass edge, two windows' holes in one callback.
/// With `pair` each time is that callback and the one after it: two 64-frame blocks hold one 128-frame
/// quantum of the synths' and FX's work whatever its phase, which one 64-frame block holds every other
/// time (a block of whole quanta holds it every time).
fn hole_callback(rate: u32, seconds: f64, hole: Frame, block: usize, tries: usize, retake: bool, pair: bool) -> (f64, f64) {
    let mut best = (f64::MAX, f64::MAX);
    for _ in 0..tries {
        for skip in [true, false] {
            let mut rig = Rig::with(Opts { sr: rate, start: rate as Frame, loop_seconds: seconds, block, ..Default::default() });
            rig.set(Command::SetBpm(120.0));
            let bars = (seconds / 2.0).floor() / if retake { 2.0 } else { 1.0 };
            rig.set(Command::SetFixedLength(true));
            rig.set(Command::SetFixedBars(bars));
            rig.set(Command::SetRetake(retake));
            rig.set_input(code);
            rig.press(Command::RecDub(0));
            let (s, e) = (rig.start_frame(), rig.end_frame());
            let len = e - s;
            // Where the jump starts and the frame the timed callback renders.
            let (from, to) = if retake { (s + rate as Frame, e + len - rate as Frame) } else { ((e - hole).max(s + block as Frame), e) };
            rig.advance_to(from);
            let mut frame = rig.frame + if skip { to - from } else { 0 };
            let input: Vec<f32> = (0..block).map(|k| code(k as Frame)).collect();
            let (mut left, mut right) = (vec![0.0f32; block], vec![0.0f32; block]);
            if !skip {
                // Render up to it as delivered: the timed callback has nothing to zero.
                while frame < to {
                    let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                    rig.engine.process(&ctx, &input, &mut left, &mut right);
                    frame += block as Frame;
                }
            }
            let ctx = ProcessContext { frame, xrun: skip, damaged: false, align_frames: 0, input_frames: 0 };
            let next = ProcessContext { frame: frame + block as Frame, xrun: false, ..ctx };
            let t = Instant::now();
            rig.engine.process(&ctx, &input, &mut left, &mut right);
            if pair {
                rig.engine.process(&next, &input, &mut left, &mut right);
            }
            let dt = t.elapsed().as_secs_f64();
            if !retake {
                assert_eq!(rig.master(), len, "the take committed its planned bars");
            }
            if skip {
                let want = if retake { to - from - 2 * block as Frame } else { to - from - block as Frame };
                assert!(rig.engine.looper().zeroed_max() >= want, "the holes were zeroed in that callback");
                best.0 = best.0.min(dt);
            } else {
                best.1 = best.1.min(dt);
            }
        }
    }
    best
}

#[test]
#[ignore]
fn a_holes_cost_per_lost_frame() {
    // The hole's own cost, over the bare engine (the deadline under load: the next test): per lost frame
    // at 48 and 192 kHz, a 59 s hole closing a 60 s take and an overdue RETAKE edge, and a one-block lock
    // miss's hole at 64 frames against its block.
    for (rate, block) in [(48_000u32, 480usize), (192_000, 1920)] {
        for retake in [false, true] {
            let hole = if retake { 58 * rate as Frame } else { (59.0 * rate as f64) as Frame };
            let (with, without) = hole_callback(rate, 60.0, hole, block, 3, retake, false);
            let per = (with - without) / hole as f64 * 1e9;
            println!(
                "{} at {rate} Hz: the callback {:.2} ms ({:.2} ms without it), {per:.3} ns per lost frame",
                if retake { "an overdue RETAKE edge, two passes' holes (58 s)" } else { "a 59 s hole closing a 60 s take" },
                with * 1e3,
                without * 1e3,
            );
            assert!(per <= 1.0, "a hole costs {per:.3} ns per lost frame");
        }
    }
    let (with, without) = hole_callback(48_000, 4.0, 64, 64, 50, false, true);
    let period = 64.0 / 48_000.0;
    println!("a one-block lock miss's hole at 64 frames: {:.2} µs more over the callback and the next ({:.3} % of a block)", (with - without) * 1e6, (with - without) / period * 100.0);
    assert!((with - without) / period <= 0.01, "the one-block hole costs {:.3} % of its block", (with - without) / period * 100.0);
}

/// The heavy scene a hole's callback is held against, at `rate`, in a 60 s buffer (the app's,
/// `engine_io::HostConfig`): a master of `bars` at 120 BPM on lanes 0 to 3, with `lane_fx` every effect
/// on every lane as the Stage 3 load sets them, every input send at its heaviest, the pad holding a
/// four-note chord, the click on; lane 4 rolls a RETAKE later take, a pass the master long. Built at
/// 1,024-frame blocks.
fn heavy_hole_scene(rate: u32, bars: f64, lane_fx: bool) -> Rig {
    let mut rig = Rig::with(Opts { sr: rate, start: rate as Frame, loop_seconds: 60.0, block: 1024, ..Default::default() });
    rig.set(Command::SetBpm(120.0));
    rig.set(Command::SetMetronome(true));
    rig.set_input(code);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.press(Command::RecDub(0));
    let end = rig.end_frame();
    rig.advance_to(end + 1);
    rig.set(Command::SetFixedLength(false));
    rig.idle();
    assert_eq!(rig.master(), bars as Frame * 2 * rate as Frame, "the master is {bars} bars");
    for _ in 1..4 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    for lane in (0..4).filter(|_| lane_fx) {
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
    input_sends_on(&mut rig);
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Pad)));
    for note in [48, 52, 55, 59] {
        rig.set(Command::NoteOn(note, 0.8));
    }
    rig.set(Command::SetRetake(true));
    rig.press(Command::RecDub(4));
    while rig.lane(4).armed || rig.window().is_some_and(|w| w.1.is_none()) {
        rig.advance(1024);
    }
    assert!((0..4).all(|i| rig.state(i) == LaneState::Playing) && rig.state(4) == LaneState::Recording);
    rig
}

/// One device callback of `io`'s length at the heavy scene, `skipped` frames after the last (an xrun
/// when the device skipped any), the filters' cutoff ramped anew every 18 ms as the Stage 3 load does:
/// its seconds. `io` holds the input and the two outputs.
fn scene_callback(rig: &mut Rig, io: &mut [Vec<f32>; 3], skipped: Frame) -> f64 {
    let [input, left, right] = io;
    rig.frame += skipped;
    for (k, x) in input.iter_mut().enumerate() {
        *x = code(rig.frame + k as Frame) - 0.25;
    }
    let block = input.len() as Frame;
    let ramp = rig.seconds(0.018);
    if rig.frame / ramp != (rig.frame + block) / ramp {
        let cutoff = if (rig.frame / ramp) % 2 == 0 { 300.0 } else { 6000.0 };
        for lane in 0..4 {
            rig.send_at(rig.frame, Command::SetFxParam(lane, FxParam::Cutoff, cutoff));
        }
    }
    let ctx = ProcessContext { frame: rig.frame, xrun: skipped > 0, damaged: false, align_frames: rig.align - rig.engine.limiter_latency(), input_frames: 0 };
    let t = Instant::now();
    rig.engine.process(&ctx, input, left, right);
    let dt = t.elapsed().as_secs_f64();
    rig.frame += block;
    rig.read_events();
    dt
}

/// What [`hole_deadline`] measured, seconds each: the callbacks that found the holes, and the scene's
/// callbacks before them that start a 128-frame quantum of the synths' and FX's work, the phase every
/// hole's callback is put on (a block of 128 or more holds a quantum start every time).
struct Deadline {
    holes: Vec<f64>,
    scene: Vec<f64>,
}

/// Each of `trials` holes in the heavy scene at `rate` and `block`: the RETAKE roll's pass edge
/// overdue in the callback that finds it, the cut pass's rest and the landing pass's start zeroed there,
/// that callback starting a quantum on the DSP clock. With `long` the device skips from a second into
/// one pass to a second before the next one's end (two windows of 59 s, the most one callback zeroes:
/// `Looper::fill_hole`), over 60 s passes; else `lost` frames across the edge, over 4 s passes.
fn hole_deadline(rate: u32, block: usize, long: bool, lost: Frame, lane_fx: bool, trials: usize) -> Deadline {
    let mut rig = heavy_hole_scene(rate, if long { 30.0 } else { 2.0 }, lane_fx);
    let mut io = [vec![0.0f32; block], vec![0.0f32; block], vec![0.0f32; block]];
    let mut out = Deadline { holes: Vec::with_capacity(trials), scene: Vec::new() };
    let (sec, q) = (rate as Frame, 128 as Frame);
    // The engine's DSP clock is the device frame less the frames skipped (`Engine::process`): the rig's
    // first block's start, then every jump.
    let mut skipped = rate as Frame;
    for _ in 0..trials {
        let (mut s, e) = (rig.start_frame(), rig.end_frame());
        let len = e - s;
        // The next pass the jump can start a second into (a long jump lands a second before its pass's end).
        while long && s + sec <= rig.frame {
            s += len;
        }
        let e = s + len;
        let from = if long { s + sec } else { e - lost / 2 };
        let from = from - (from - skipped).rem_euclid(q); // a quantum starts with the hole's callback
        let to = if long { e + len - sec } else { from + lost };
        assert!(rig.frame < from && to > e, "the scene renders up to the jump, which crosses the pass edge");
        while rig.frame + block as Frame <= from {
            let phase = (rig.frame - skipped).rem_euclid(q);
            let t = scene_callback(&mut rig, &mut io, 0);
            if phase == 0 || phase + block as Frame > q {
                out.scene.push(t);
            }
        }
        rig.advance_to(from);
        out.holes.push(scene_callback(&mut rig, &mut io, to - from));
        skipped += to - from;
        let want = if long { 2 * (len - sec) - block as Frame - q } else { lost };
        assert!(rig.engine.looper().zeroed_max() >= want, "the callback zeroed both windows' holes");
        assert_eq!(rig.state(4), LaneState::Recording, "the roll goes on");
    }
    out
}

#[test]
#[ignore]
fn a_holes_callback_meets_its_deadline_under_the_heavy_scene() {
    // The whole callback that finds the largest hole each backend can make, with a RETAKE pass edge
    // overdue in it, on the heaviest phase of the heavy scene, against the scene's own callbacks on that
    // phase: the worst of eight holes. The bar: a hole adds at most a quarter of its period. The engine's
    // callback is to stay within half its period, the other half left to the device side (the WASAPI
    // join's pull, the Share mirror), a live plugin and the scheduler's lateness, and the heavy scene
    // takes about a quarter on that phase at the pickable rates: a hole may take the other quarter.
    //
    // - ASIO, at the smallest blocks a driver runs (the select offers 64; a driver fixed in its own
    //   panel can run 32): a hole is a block per lock miss, and in a release build nothing holds the
    //   engine lock while a device runs; the longest hold is the DEV `engine_hold_lock`'s clamp, a second.
    // - WASAPI, at its endpoint's default period (10 ms on every endpoint run so far) and rates up to
    //   192 kHz (an endpoint's own): its dry frames are the stall, unbounded, so the hole is the most one
    //   callback zeroes, the rest of the pass the jump cut and the start of the one it lands in, at 60 s
    //   passes (the whole buffer).
    // - At 96 and 192 kHz the heavy scene alone overruns half its period (every lane effect at four
    //   times the frames), so the scene there drops the lanes' effects: the most of it the engine plays.
    // - The two 59 s windows at 64 frames: no backend makes that, printed to show the margin a long hole
    //   on a short period would lack.
    let cases: [(&str, u32, usize, bool, bool); 11] = [
        ("ASIO", 44_100, 32, false, true),
        ("ASIO", 44_100, 64, false, true),
        ("ASIO", 48_000, 32, false, true),
        ("ASIO", 48_000, 64, false, true),
        ("ASIO", 96_000, 64, false, true),
        ("ASIO", 192_000, 64, false, true),
        ("WASAPI", 44_100, 441, true, true),
        ("WASAPI", 48_000, 480, true, true),
        ("WASAPI", 96_000, 960, true, true),
        ("WASAPI", 192_000, 1920, true, true),
        ("no backend", 48_000, 64, true, false),
    ];
    let mut over = Vec::new();
    for (backend, rate, block, long, barred) in cases {
        let lane_fx = rate <= 48_000;
        let Deadline { mut holes, mut scene } = hole_deadline(rate, block, long, rate as Frame, lane_fx, 8);
        let period = block as f64 / rate as f64;
        holes.sort_by(f64::total_cmp);
        scene.sort_by(f64::total_cmp);
        let pct = |t: f64| t / period * 100.0;
        let (worst, median, typical) = (holes[holes.len() - 1], holes[holes.len() / 2], scene[scene.len() / 2]);
        println!(
            "{backend}, {rate} Hz, {block}-frame period ({:.2} ms), {}, {}: the hole's callback worst {:.3} ms ({:.0} %, {:.0} % over the scene's), median {:.0} %; the scene's callbacks on that phase median {:.0} %, p99.9 {:.0} %, worst {:.0} % ({} of them)",
            period * 1e3,
            if long { "two 59 s windows" } else { "1 s across the edge" },
            if lane_fx { "the heavy scene" } else { "no lane effects" },
            worst * 1e3,
            pct(worst),
            pct(worst - typical),
            pct(median),
            pct(typical),
            pct(scene[scene.len() * 999 / 1000]),
            pct(scene[scene.len() - 1]),
            scene.len()
        );
        if barred && worst - typical > 0.25 * period {
            over.push(format!("{backend} {rate} Hz {block}: {:.0} % over the scene's", pct(worst - typical)));
        }
    }
    assert!(over.is_empty(), "a hole adds over a quarter of its period: {over:?}");
}

/// The multiply's push (F14) with every bin of four one-bar loops and their undo targets marked: the
/// commit callback, which sets eight fills' marks at once, against the same callback unmarked.
#[test]
#[ignore]
fn a_marked_multiplys_push_costs_under_a_twentieth_of_the_block_more() {
    let push = |marked: bool| -> f64 {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut rig = Rig::with(Opts { loop_seconds: 60.0, block: BLOCK, ..Default::default() });
            rig.set_input(code);
            let damaged = |rig: &mut Rig, frames: Frame| {
                let end = rig.frame + frames;
                while rig.frame < end {
                    if marked {
                        rig.damage();
                    }
                    rig.advance(BLOCK as Frame);
                }
            };
            rig.press(Command::RecDub(0));
            while rig.lane(0).armed || rig.window().is_some_and(|w| w.1.is_none()) {
                rig.advance(BLOCK as Frame);
            }
            let fpb = rig.fpb();
            damaged(&mut rig, fpb + 2400);
            rig.press(Command::RecDub(0));
            rig.advance(4800);
            rig.idle();
            let master = rig.master();
            assert_eq!(master, fpb);
            for _ in 1..4 {
                rig.press(Command::Copy(0));
                rig.idle();
            }
            for lane in 0..4 {
                rig.press(Command::RecDub(lane));
                damaged(&mut rig, master);
                rig.press(Command::RecDub(lane));
                rig.idle();
            }
            if marked {
                assert!((0..4).all(|i| rig.marks(i).len() as Frame == (master + 1023) / 1024), "every bin marked");
            }
            rig.set(Command::SetFixedLength(true));
            rig.set(Command::SetFixedBars(30.0));
            rig.press(Command::RecDub(4));
            let end = rig.end_frame();
            rig.advance_to(end - BLOCK as Frame);
            rig.advance(BLOCK as Frame); // the take's last block
            let input: Vec<f32> = (0..BLOCK).map(|k| code(k as Frame) - 0.25).collect();
            let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
            let ctx = ProcessContext { frame: rig.frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            let t = Instant::now();
            rig.engine.process(&ctx, &input, &mut left, &mut right);
            best = best.min(t.elapsed().as_secs_f64());
            rig.frame += BLOCK as Frame;
            assert_eq!(rig.master(), 30 * master, "the multiply committed in the timed callback");
        }
        best
    };
    let (marked, clean) = (push(true), push(false));
    let period = BLOCK as f64 / 48_000.0;
    println!("a multiply's push over four lanes with undo targets, every bin marked: {:.2} µs, unmarked {:.2} µs: {:.2} % of the block more", marked * 1e6, clean * 1e6, (marked - clean) / period * 100.0);
    assert!((marked - clean) / period <= 0.05, "the marked push costs {:.2} % of the block more", (marked - clean) / period * 100.0);
}
