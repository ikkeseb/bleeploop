//! The mix a lane and the master go through (`src/audio/looper/mixer.ts`, `src/audio/master.ts`): lane
//! volume (0..1.5) and mute, master volume (0..1) and mute, each gliding to its target like Web Audio's
//! setTargetAtTime (10 ms for a lane, 12 ms for the master) rather than stepping, and a lane that stays
//! in sync while muted. The rig guards never heard the mix; the rendered output is checked here.

mod common;

use common::Rig;
use lf_engine::dsp::compressor::Compressor;
use lf_engine::grid::Frame;
use lf_engine::Command;

const LEVEL: f32 = 0.5;

/// Lane 0 plays a constant LEVEL loop; the input is silent from here on.
fn playing() -> Rig {
    let mut rig = Rig::new();
    rig.set_level(LEVEL);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.advance(4800);
    rig
}

/// The output `frames` from now, as the lane is heard.
fn level_after(rig: &mut Rig, frames: Frame) -> f32 {
    rig.keep_output();
    rig.advance(frames);
    *rig.output.as_ref().unwrap().1.last().unwrap()
}

#[test]
fn lane_volume_and_mute_glide_to_their_target() {
    let mut rig = playing();
    assert_eq!(level_after(&mut rig, 10), LEVEL, "unity");
    rig.press(Command::SetVolume(0, 1.5));
    let tau = 480; // 10 ms at 48 kHz
    let one_tau = level_after(&mut rig, tau - 1);
    assert!((one_tau - LEVEL * (1.5 - 0.5 * (-1f32).exp())).abs() < 1e-3, "one time constant: {one_tau}");
    assert!((level_after(&mut rig, 20 * tau) - LEVEL * 1.5).abs() < 1e-6);
    rig.press(Command::SetMute(0, true));
    assert!(level_after(&mut rig, 1) > 0.5, "no step to silence");
    assert!(level_after(&mut rig, 30 * tau).abs() < 1e-6, "muted");
    rig.press(Command::SetVolume(0, 9.0));
    assert_eq!(rig.engine.looper().volume(0), (1.5, true), "clamped to 1.5, still muted");
    rig.press(Command::SetMute(0, false));
    assert!((level_after(&mut rig, 30 * tau) - LEVEL * 1.5).abs() < 1e-6, "unmuted in sync");
}

#[test]
fn master_volume_and_mute_scale_everything() {
    let mut rig = playing();
    rig.press(Command::SetMasterVolume(0.5));
    assert!((level_after(&mut rig, 20_000) - LEVEL * 0.5).abs() < 1e-6);
    rig.press(Command::SetMasterMute(true));
    assert!(level_after(&mut rig, 20_000).abs() < 1e-6);
    rig.press(Command::SetMasterVolume(3.0));
    rig.press(Command::SetMasterMute(false));
    assert!((level_after(&mut rig, 20_000) - LEVEL).abs() < 1e-6, "clamped to 1");
}

#[test]
fn the_next_take_may_use_32_bars_before_a_master_and_whole_multiples_of_it_after() {
    let mut rig = Rig::new();
    assert_eq!(rig.engine.looper().next_take_max_bars(rig.bpm()), 32);
    rig.set_level(0.5);
    rig.record_first_take(0, 3, 2400);
    // 20 s of lane buffer holds 10 bars at 120 bpm: three loops of the 3-bar master (`tests/multiply.rs`).
    assert_eq!(rig.engine.looper().next_take_max_bars(rig.bpm()), 9);
}

#[test]
fn a_dub_on_a_lane_being_copied_waits_for_the_copy() {
    let mut rig = playing();
    let master = rig.master();
    let pre = rig.pcm(0);
    rig.advance_to(rig.next_boundary() + master * 9 / 10);
    rig.press(Command::Copy(0));
    rig.set_level(0.25);
    rig.press(Command::RecDub(0));
    rig.idle();
    rig.advance(4800);
    rig.press(Command::RecDub(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.pcm(1), pre, "the copy holds the loop from before the dub");
    assert_ne!(rig.pcm(0), pre, "the dub itself went on");
}

#[test]
fn the_output_is_the_limiter_over_the_bus_plus_the_unlimited_monitor() {
    let mut rig = Rig::new();
    let start = rig.frame;
    rig.keep_output();
    rig.set(Command::SetMetronome(true));
    rig.set(Command::SetMasterVolume(0.8));
    rig.set_level(0.9);
    rig.record_first_take(0, 1, 2400);
    // A lane over the threshold, and a played signal that is not the loop.
    rig.set(Command::SetVolume(0, 1.5));
    rig.set_input(|f| 0.3 * (f as f32 * 0.01).sin());
    rig.advance(48_000);
    let mut limiter = Compressor::master_limiter(rig.sr as f32);
    let (mut l, mut r) = (rig.bus.clone(), rig.bus_right.clone());
    limiter.process(start, &mut l, &mut r);
    let plus_monitor = |side: &[f32]| side.iter().zip(&rig.monitor).map(|(x, m)| x + m).collect::<Vec<f32>>();
    assert!(rig.heard == plus_monitor(&l) && rig.heard_right == plus_monitor(&r), "the output is limiter(bus) + monitor");
    let peak = |x: &[f32]| x.iter().fold(0f32, |a, v| a.max(v.abs()));
    // A literal port of the live limiter: it pulls the peak down hard but is no true ceiling (STATUS E6).
    assert!(peak(&rig.bus) > 1.0 && peak(&l) < 0.8 * peak(&rig.bus), "the limiter engaged: {} -> {}", peak(&rig.bus), peak(&l));
    assert!(peak(&rig.monitor) > 0.2, "the monitor carried the played signal");
}
