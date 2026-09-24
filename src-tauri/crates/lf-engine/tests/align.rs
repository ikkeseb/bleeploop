//! The take alignment that replaces verify/guards/record-compensation.mjs. That guard proved the Web
//! Audio compensation C (a formula over bridge queues, output latencies and a by-ear trim) and is
//! deleted, not ported: the engine has one clock, and a take starts exactly `align_frames` (the driver's
//! input + output latency) plus the plugin's reported latency after its downbeat. What the player played
//! to the click then lands on the grid: the first take's frame 0, a later take's frame 0 and an overdub
//! all put a note played on beat 1 on loop position 0. A wrong alignment shifts a take uniformly by
//! exactly the error, never its length (the guard's section D).

mod common;

use common::{Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, LaneState};

const PHYS: Frame = 1123; // input + output latency as the driver reports it
const PLUGIN: Frame = 57; // the amp-sim's reported latency

/// The player hits a note on each downbeat they hear: it reaches the input `delay` frames after the
/// click left the engine.
fn player(downbeats: Vec<Frame>, delay: Frame) -> impl Fn(Frame) -> f32 {
    move |f| if downbeats.contains(&(f - delay)) { 1.0 } else { 0.0 }
}

fn rig(align: Frame) -> Rig {
    let mut rig = Rig::with(Opts { align, ..Default::default() });
    rig.inserts = Box::new(Delay::new(PLUGIN));
    rig
}

#[test]
fn a_take_starts_align_plus_plugin_latency_after_its_downbeat() {
    let mut rig = rig(PHYS);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + 4 * 24_000;
    assert_eq!(rig.start_frame(), downbeat + PHYS + PLUGIN);
    rig.set_input(player(vec![downbeat], PHYS));
    rig.advance_to(rig.end_frame() + 1);
    let pcm = rig.pcm(0);
    assert_eq!(pcm[0], 1.0, "the note on beat 1 is the loop's frame 0");
    assert_eq!(pcm.iter().filter(|&&x| x != 0.0).count(), 1);
    assert_eq!((rig.anchor() - downbeat) % rig.master(), 0);
}

#[test]
fn later_takes_and_overdubs_land_on_the_grid_too() {
    let mut rig = rig(PHYS);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.press(Command::RecDub(0));
    rig.advance_to(rig.end_frame() + 1);
    let master = rig.master();
    rig.set_input(|_| 0.0);
    rig.press(Command::RecDub(1));
    let boundary = rig.start_frame() - PHYS - PLUGIN;
    assert_eq!((boundary - rig.anchor()) % master, 0, "armed on a master boundary");
    rig.set_input(player(vec![boundary], PHYS));
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(1), LaneState::Playing);
    assert_eq!(rig.pcm(1)[0], 1.0, "a later take's note on the boundary is its frame 0");
    // Overdub lane 0 on the next downbeat and a quarter later: both land where they were played.
    let next = rig.next_boundary();
    rig.set_input(player(vec![next, next + master / 4], PHYS));
    rig.press(Command::RecDub(0));
    rig.advance_to(next + master / 2);
    rig.press(Command::RecDub(0));
    rig.advance(PHYS + PLUGIN + 1);
    let pcm = rig.pcm(0);
    assert_eq!((pcm[0], pcm[master as usize / 4]), (1.0, 1.0));
}

#[test]
fn a_wrong_alignment_shifts_the_take_by_exactly_the_error() {
    for error in [-300, -1, 1, 480] {
        let mut rig = rig(PHYS + error); // the driver misreports by `error`
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(2.0));
        let mark = rig.events.len();
        rig.press(Command::RecDub(0));
        let downbeat = rig.count_one(mark) + 4 * 24_000;
        let beats: Vec<Frame> = (0..8).map(|b| downbeat + b * 24_000).collect();
        rig.set_input(player(beats, PHYS));
        rig.advance_to(rig.end_frame() + 1);
        assert_eq!(rig.master(), 2 * rig.fpb(), "the length never moves");
        let pcm = rig.pcm(0);
        let hits: Vec<Frame> = pcm.iter().enumerate().filter(|(_, &x)| x != 0.0).map(|(k, _)| k as Frame).collect();
        let first = (-error).rem_euclid(24_000);
        assert_eq!(hits[0], first, "error {error}: every note shifted by exactly the error");
        assert!(hits.windows(2).all(|w| w[1] - w[0] == 24_000));
    }
}
