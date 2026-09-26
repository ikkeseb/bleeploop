//! The input sends (`lf_engine::input_fx`, tester report F15): ECHO and REVERB on the wet signal, wet
//! only, into the record tap and the monitor on the same frame. Every scenario runs with slot 0 live and
//! empty (the rig's MIC), so the wet signal is the input itself: with the master at 1 the record tap and
//! the monitor are the input, bit for bit, whenever the sends add nothing. A new guard, not a port: the
//! web path has no input sends.

mod common;

use common::{Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, InputSend, InputSendParam};

/// A 1/8 note at 120 BPM (the default tempo) at 48 kHz.
const EIGHTH: Frame = 12_000;

/// A deterministic, busy input: a 110 Hz sine under a hash of the frame.
fn busy(f: Frame) -> f32 {
    let hash = ((f.wrapping_mul(7919)).rem_euclid(1000)) as f32 / 1000.0 - 0.5;
    0.3 * (f as f32 * 2.0 * std::f32::consts::PI * 110.0 / 48000.0).sin() + 0.1 * hash
}

/// 112 Hz: a 1/8 at 120 BPM is 28 whole cycles, so the echoes add in phase.
const SINE_HZ: f64 = 112.0;

fn sine(f: Frame) -> f32 {
    0.5 * (f as f64 * 2.0 * std::f64::consts::PI * SINE_HZ / 48000.0).sin() as f32
}

fn impulses(at: Vec<Frame>) -> impl Fn(Frame) -> f32 {
    move |f| if at.contains(&f) { 1.0 } else { 0.0 }
}

fn echo(rig: &mut Rig, level: f64, feedback: f64) {
    rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, level));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, feedback));
    rig.set(Command::SetInputSend(InputSend::Echo, true));
}

fn reverb(rig: &mut Rig, level: f64) {
    rig.set(Command::SetInputSendParam(InputSendParam::ReverbLevel, level));
    rig.set(Command::SetInputSend(InputSend::Reverb, true));
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// The largest step between two neighbouring frames.
fn max_step(v: &[f32]) -> f32 {
    v.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

/// The largest step a signal at `SINE_HZ` no louder than `v`'s peak can take between two frames.
fn sine_step(v: &[f32]) -> f32 {
    v.iter().fold(0.0f32, |a, &x| a.max(x.abs())) * (2.0 * std::f64::consts::PI * SINE_HZ / 48000.0) as f32
}

/// What the sends added to the record tap: the tap less the input (the wet signal on MIC).
fn added(rig: &Rig, input: impl Fn(Frame) -> f32) -> Vec<f32> {
    let start = rig.output.as_ref().unwrap().0;
    rig.record.iter().enumerate().map(|(k, &r)| r - input(start + k as Frame)).collect()
}

#[test]
fn sends_that_are_off_leave_the_record_tap_and_the_monitor_as_the_input() {
    let mut rig = Rig::new();
    rig.set_input(busy);
    rig.keep_output();
    let start = rig.frame;
    rig.advance(rig.seconds(1.0));
    let input: Vec<f32> = (0..rig.record.len()).map(|k| busy(start + k as Frame)).collect();
    assert_eq!(bits(&rig.record), bits(&input), "the record tap is the wet signal");
    assert_eq!(bits(&rig.monitor), bits(&input), "the monitor is the wet signal under the master (1)");
    assert!(rig.engine.input_fx().idle());
}

#[test]
fn the_echo_lands_one_delay_time_after_the_dry_frame_and_leaves_the_dry_untouched() {
    let mut rig = Rig::new();
    echo(&mut rig, 0.5, 0.5);
    rig.advance(2400); // past the gate's 20 ms ramp
    rig.keep_output();
    let hit = rig.frame + 777;
    rig.set_input(impulses(vec![hit]));
    rig.advance(4 * EIGHTH);
    let k = (hit - rig.output.as_ref().unwrap().0) as usize;
    // The level 0.5, compensated for the feedback 0.5 (`input_fx`'s module doc).
    let level = 0.5 * (1.0f32 - 0.5 * 0.5).sqrt();
    let want = |j: usize| match j {
        _ if j == k => 1.0,
        _ if j == k + EIGHTH as usize => level,
        _ if j == k + 2 * EIGHTH as usize => 0.5 * level,
        _ if j == k + 3 * EIGHTH as usize => 0.25 * level,
        _ => 0.0,
    };
    for (j, (&r, &m)) in rig.record.iter().zip(&rig.monitor).enumerate() {
        assert_eq!(r, want(j), "record tap at {j} (the hit at {k})");
        assert_eq!(m, want(j), "monitor at {j}");
    }
}

/// White noise, uniform in ±0.5, from a hash of the frame (no period a delay time could line up with).
fn noise(f: Frame) -> f32 {
    let mut z = (f as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32 - 0.5
}

fn rms(v: &[f32]) -> f64 {
    (v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / v.len() as f64).sqrt()
}

/// The monitor has no limiter: at full level and the highest feedback the echo carries the input's
/// energy, not the line's +10 dB build-up (`input_fx`'s module doc).
#[test]
fn the_echo_at_full_level_and_feedback_stays_at_the_inputs_energy() {
    let mut rig = Rig::new();
    rig.set_input(noise);
    echo(&mut rig, 1.0, 0.95);
    // 10 s is 40 repeats of 1/8: 0.95^80 of the build-up's energy is still to come.
    rig.advance(rig.seconds(10.0));
    rig.keep_output();
    rig.advance(rig.seconds(2.0));
    let start = rig.output.as_ref().unwrap().0;
    let input: Vec<f32> = (0..rig.record.len()).map(|k| noise(start + k as Frame)).collect();
    let ratio = rms(&added(&rig, noise)) / rms(&input);
    println!("echo at level 1, feedback 0.95, on white noise: {ratio:.3} of the input's RMS");
    assert!((0.85..1.15).contains(&ratio), "the echo's RMS is {ratio} of the input's");
}

#[test]
fn the_echo_follows_the_division_and_the_tempo() {
    let mut rig = Rig::new();
    echo(&mut rig, 1.0, 0.0);
    rig.set(Command::SetInputSendParam(InputSendParam::EchoTime, 3.0)); // 1/16
    rig.advance(2400);
    rig.keep_output();
    let hit = rig.frame + 100;
    rig.set_input(impulses(vec![hit]));
    rig.advance(EIGHTH);
    let k = (hit - rig.output.as_ref().unwrap().0) as usize;
    assert_eq!(rig.record[k + 6000], 1.0, "a 1/16 at 120 BPM is 6000 frames");
    assert_eq!(rig.record.iter().filter(|&&x| x != 0.0).count(), 2);

    // 90 BPM, a quarter: frames_per_bar(90) / 4 = 32000 frames.
    rig.set(Command::SetBpm(90.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoTime, 0.0));
    // Past the ramp, and far enough that the line's longer reach no longer finds the first hit.
    rig.advance(40_000);
    rig.keep_output();
    let hit = rig.frame + 5;
    rig.set_input(impulses(vec![hit]));
    rig.advance(40_000);
    let k = (hit - rig.output.as_ref().unwrap().0) as usize;
    let peak = rig.record.iter().enumerate().skip(k + 1).fold((0, 0.0f32), |a, (j, &x)| if x.abs() > a.1 { (j, x.abs()) } else { a });
    assert_eq!(peak.0 - k, 32_000, "the first echo one quarter at 90 BPM after the hit");
    // 32000 frames in f32 seconds reads 0.004 of a frame late: the kernel interpolates.
    assert!((peak.1 - 1.0).abs() < 1e-2, "at the input's level: {}", peak.1);
    assert_eq!(rig.record[k], 1.0);
    assert_eq!(rig.record.iter().filter(|&&x| x.abs() > 1e-2).count(), 2, "the hit and its echo");
}

#[test]
fn the_reverb_adds_a_tail_from_the_dry_frame_on() {
    let mut rig = Rig::new();
    reverb(&mut rig, 1.0);
    rig.advance(2400);
    rig.keep_output();
    let hit = rig.frame + 1000;
    rig.set_input(impulses(vec![hit]));
    rig.advance(rig.seconds(3.0));
    let k = (hit - rig.output.as_ref().unwrap().0) as usize;
    assert!(rig.record[..k].iter().all(|&x| x == 0.0), "nothing before the dry frame");
    assert_eq!(rig.record[k], 1.0, "the dry frame as it was");
    let tail = &rig.record[k + 1..];
    let energy: f64 = tail.iter().map(|&x| (x as f64) * (x as f64)).sum();
    let peak = tail.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    println!("reverb at level 1 on a unit impulse: tail energy {energy:.4}, peak {peak:.4}");
    assert!(energy > 1e-3 && peak < 0.5, "a tail, under the dry");
    assert_eq!(bits(&rig.monitor), bits(&rig.record), "heard as recorded");
}

#[test]
fn switching_the_sends_on_and_off_makes_no_click_and_lets_the_tail_ring() {
    let mut rig = Rig::new();
    rig.set_input(sine);
    rig.set(Command::SetInputSendParam(InputSendParam::EchoLevel, 1.0));
    rig.set(Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.5));
    rig.set(Command::SetInputSendParam(InputSendParam::ReverbLevel, 1.0));
    rig.advance(1000);
    rig.keep_output();
    rig.set(Command::SetInputSend(InputSend::Echo, true));
    rig.set(Command::SetInputSend(InputSend::Reverb, true));
    rig.advance(rig.seconds(1.5));
    let on = added(&rig, sine).len();
    rig.set(Command::SetInputSend(InputSend::Echo, false));
    rig.set(Command::SetInputSend(InputSend::Reverb, false));
    rig.advance(rig.seconds(1.0));
    let e = added(&rig, sine);
    // What the sends add is the sine through a linear filter (the ramps are 20 ms, far slower than a
    // cycle): no step larger than a sine at its peak takes, give or take 10 %. A click (a gate with no
    // ramp) steps by a good part of the input's 0.5.
    for (what, window) in [("switching on", &e[..on]), ("switching off", &e[on..])] {
        let (step, bound) = (max_step(window), sine_step(window));
        println!("{what}: max step {step:.5}, a sine's at that peak {bound:.5}");
        assert!(step <= bound * 1.1 + 1e-5, "{what}: a step of {step} where a sine's is {bound}");
    }
    let ring = e[on + 2 * EIGHTH as usize..on + 3 * EIGHTH as usize].iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    assert!(ring > 0.05, "the tail rings on after off: {ring}");
}

#[test]
fn a_send_that_is_off_and_decayed_leaves_the_output_as_if_it_had_never_been_on() {
    let mut never = Rig::new();
    let mut was = Rig::new();
    for rig in [&mut never, &mut was] {
        rig.set_input(busy);
    }
    echo(&mut was, 0.5, 0.4);
    reverb(&mut was, 0.5);
    for rig in [&mut never, &mut was] {
        rig.advance_to(96_000);
    }
    was.set(Command::SetInputSend(InputSend::Echo, false));
    was.set(Command::SetInputSend(InputSend::Reverb, false));
    never.advance(2);
    assert!(!was.engine.input_fx().idle(), "the tails ring");
    // The echo decays under −120 dB in 15 repeats of 1/8 (0.4^15 < 1e-6), then its 2 s line empties;
    // the reverb's IR is 2.62 s long.
    for rig in [&mut never, &mut was] {
        rig.advance(rig.seconds(8.0));
    }
    assert!(was.engine.input_fx().idle(), "both sends idle");
    for rig in [&mut never, &mut was] {
        rig.keep_output();
        rig.advance(rig.seconds(1.0));
    }
    assert_eq!(never.frame, was.frame);
    assert_eq!(bits(&was.record), bits(&never.record), "the record tap");
    assert_eq!(bits(&was.monitor), bits(&never.monitor), "the monitor");
    assert_eq!(bits(&was.heard), bits(&never.heard), "the output, left");
    assert_eq!(bits(&was.heard_right), bits(&never.heard_right), "the output, right");
}

const PHYS: Frame = 1123; // input + output latency as the driver reports it
const PLUGIN: Frame = 57; // the amp-sim's reported latency
const LIMITER: Frame = 288; // the master limiter's pre-delay at 48 kHz

/// A first take of one bar with an amp-sim in the live slot: the player hits one note on the downbeat
/// they hear (as `tests/align.rs`). Returns the take.
fn first_take(sends: impl Fn(&mut Rig)) -> Vec<f32> {
    let mut rig = Rig::with(Opts { align: PHYS + LIMITER, ..Default::default() });
    rig.install(0, Box::new(Delay::new(PLUGIN)));
    sends(&mut rig);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * 24_000;
    assert_eq!(rig.start_frame(), downbeat + PHYS + PLUGIN + LIMITER, "the sends add nothing to the alignment");
    rig.set_input(move |f| if f - LIMITER - PHYS == downbeat { 1.0 } else { 0.0 });
    rig.advance_to(rig.end_frame() + 1);
    rig.pcm(0)
}

#[test]
fn a_take_with_the_sends_on_has_its_dry_note_where_it_has_it_with_them_off() {
    let dry = first_take(|_| {});
    let echoed = first_take(|rig| echo(rig, 0.5, 0.0));
    let both = first_take(|rig| {
        echo(rig, 0.5, 0.0);
        reverb(rig, 0.5);
    });
    let first = |pcm: &[f32]| pcm.iter().position(|&x| x != 0.0);
    assert_eq!(dry[0], 1.0, "the note on beat 1 is the loop's frame 0");
    assert_eq!(dry.iter().filter(|&&x| x != 0.0).count(), 1);
    assert_eq!((first(&echoed), echoed[0]), (Some(0), 1.0), "the echo leaves the dry note on frame 0");
    assert_eq!(echoed[EIGHTH as usize], 0.5, "its echo one 1/8 later, on the grid");
    assert_eq!(echoed.iter().filter(|&&x| x != 0.0).count(), 2);
    assert_eq!((first(&both), both[0]), (Some(0), 1.0), "and so does the reverb");
}

/// The input sends' part of `tests/sound.rs`'s block-size guard: the same commands, landing mid-quantum,
/// render the same bits at any block size.
fn scenario(block: usize) -> [Vec<f32>; 4] {
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    let t = rig.frame;
    rig.set_input(move |f| busy(f) + if (f - t) % 30_011 == 0 { 0.5 } else { 0.0 });
    for (at, command) in [
        (1_001, Command::SetInputSendParam(InputSendParam::EchoFeedback, 0.6)),
        (1_001, Command::SetInputSend(InputSend::Echo, true)),
        (3_333, Command::SetInputSend(InputSend::Reverb, true)),
        (20_011, Command::SetInputSendParam(InputSendParam::EchoTime, 2.0)),
        (30_005, Command::SetInputSendParam(InputSendParam::EchoLevel, 0.9)),
        (40_003, Command::SetBpm(97.0)),
        (60_007, Command::SetInputSend(InputSend::Echo, false)),
        (61_000, Command::SetInputSendParam(InputSendParam::ReverbLevel, 0.2)),
        (70_001, Command::SetInputSend(InputSend::Reverb, false)),
        (90_000, Command::SetInputSend(InputSend::Echo, true)),
    ] {
        rig.send_at(t + at, command);
    }
    rig.keep_output();
    rig.advance(rig.seconds(2.5));
    [rig.heard, rig.heard_right, rig.record, rig.monitor]
}

#[test]
fn the_sends_render_bit_identical_across_block_sizes() {
    let reference = scenario(128);
    assert!(reference[2].iter().zip(&reference[3]).all(|(r, m)| r == m), "heard as recorded");
    for block in [1, 64, 127, 480, 4096] {
        let got = scenario(block);
        for (name, (a, b)) in ["left", "right", "record", "monitor"].iter().zip(reference.iter().zip(&got)) {
            assert_eq!(bits(a), bits(b), "{name} at block {block}");
        }
    }
}

/// A send's switch never waits behind a looper command that waits for a block job, as a note does not.
#[test]
fn a_send_never_waits_behind_a_held_looper_command() {
    let mut rig = Rig::with(Opts { loop_seconds: 60.0, ..Default::default() });
    rig.set_level(0.5);
    rig.record_first_take(0, 16, 2400); // 32 s: a copy job runs for ~31 ms
    rig.set_level(0.0);
    rig.set(Command::SetMute(0, true));
    rig.advance(4800);
    rig.press(Command::Copy(0));
    rig.press(Command::PlayStop(1));
    assert!(rig.engine.holding(), "PLAY on the copy waits for its job");
    rig.press(Command::SetInputSend(InputSend::Echo, true));
    assert!(rig.engine.holding(), "the job is still running");
    assert!(rig.engine.input_fx().is_on(InputSend::Echo), "the echo is on meanwhile");
}
