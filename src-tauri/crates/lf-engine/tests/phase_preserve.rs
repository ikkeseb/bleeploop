//! Ports verify/guards/phase-preserve.mjs: the PHASE-PRESERVING COMMIT. The grid a first take commits
//! to is the counted downbeat, a whole number of loops back from the commit, never the commit instant;
//! later takes and resumes join the running phase; an idle PLAY restarts from the top.
//!
//! Its section E (the Web Audio source's start offset, advanced by a late start) has no engine
//! counterpart: a lane reads loop position `(frame - anchor) mod master` on every frame, so there is no
//! start offset to clamp. What E protected, playback on the grid, is asserted here on the rendered
//! output itself (R, G), which the rig guards could not see.

mod common;

use common::{code, Rig};
use lf_engine::grid::{commit_anchor, frames_per_bar, plan_commit, Frame, Grid};
use lf_engine::{Command, LaneState};

const CFG: [(u32, f64, Frame); 12] = [
    (48000, 120.0, 1),
    (48000, 90.0, 2),
    (48000, 137.0, 4),
    (48000, 100.0, 8),
    (48000, 200.0, 3),
    (48000, 73.5, 2),
    (44100, 120.0, 1),
    (44100, 90.0, 2),
    (44100, 137.0, 4),
    (44100, 100.0, 8),
    (44100, 200.0, 3),
    (44100, 73.5, 2),
];

#[test]
fn a_c_d_the_grid_anchor_is_the_counted_downbeat_a_whole_number_of_loops_back() {
    for (sr, bpm, bars) in CFG {
        let master = bars * frames_per_bar(bpm, sr);
        let downbeat = 59_259_259;
        for frac in [0.0001, 0.013, 0.25, 0.5, 0.731, 0.999, 1.0, 1.5, 2.34, 5.0] {
            let at = downbeat + (frac * master as f64) as Frame;
            let anchor = commit_anchor(Some(downbeat), master, at);
            assert_eq!((anchor - downbeat) % master, 0);
            assert!(anchor <= at && at < anchor + master, "the commit plays inside the anchored loop");
        }
        // A fixed-length take discovered late still anchors to its first counted downbeat.
        assert_eq!(commit_anchor(Some(downbeat), master, downbeat + master + 2880), downbeat + master);
    }
}

#[test]
fn b_loop_wraps_and_click_downbeats_coincide_for_thirty_minutes() {
    for (sr, bpm, bars) in CFG {
        let master = bars * frames_per_bar(bpm, sr);
        let grid = Grid::master(7 * sr as Frame, master, bars);
        let loops = 30 * 60 * sr as Frame / master;
        for k in [0, 1, 2, loops / 2, loops] {
            assert_eq!(grid.beat_frame((4 * bars * k) as u64), 7 * sr as Frame + k * master);
        }
    }
}

#[test]
fn h_the_floor_keeps_the_completed_bars() {
    for sr in [48000, 44100] {
        for bpm in [120.0, 90.0, 137.0, 100.0, 73.5] {
            let fpb = frames_per_bar(bpm, sr);
            let mut played = 0.3;
            while played <= 5.71 {
                let raw = (played * fpb as f64).round() as Frame;
                let plan = plan_commit(raw, bpm, sr, 60 * sr as Frame);
                if raw >= fpb {
                    assert!(plan.master <= raw, "played {played:.1} bars: no tail pad");
                } else {
                    assert_eq!(plan.master, fpb);
                }
                played += 0.1;
            }
        }
    }
}

/// A committed first take whose tap carried the frame code, then silence.
fn committed_take(bpm: u32, sr: u32, bars: Frame, stop_after: f64) -> (Rig, Frame, Frame) {
    let mut rig = Rig::at(sr);
    rig.set(Command::SetBpm(bpm as f64));
    rig.set_input(code);
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    let downbeat = rig.count_one(mark) + (240.0 * sr as f64 / bpm as f64).round() as Frame;
    rig.advance_to(downbeat + bars * rig.fpb() + rig.seconds(stop_after));
    rig.set_level(0.0);
    rig.press(Command::RecDub(0));
    let master = rig.master();
    (rig, downbeat, master)
}

/// The rendered output at every kept frame equals `lanes` summed at the grid phase.
fn plays_on_grid(rig: &Rig, pcms: &[Vec<f32>], from: Frame) {
    let (start, out) = rig.output.as_ref().unwrap();
    let (anchor, master) = (rig.anchor(), rig.master());
    for (k, &y) in out.iter().enumerate() {
        let f = start + k as Frame;
        if f < from {
            continue;
        }
        let pos = (f - anchor).rem_euclid(master) as usize;
        let want: f32 = pcms.iter().map(|p| p[pos]).sum();
        assert_eq!(y, want, "frame {f}: pos {pos}");
    }
}

#[test]
fn r_a_real_first_take_plays_on_its_counted_downbeat() {
    for (bpm, sr, bars, stop_after) in [(120u32, 48000u32, 2, 0.04), (137, 44100, 1, 0.09), (90, 48000, 3, 0.2), (200, 48000, 4, 0.01)] {
        let (mut rig, downbeat, master) = committed_take(bpm, sr, bars, stop_after);
        assert_eq!(master, bars * rig.fpb());
        assert_eq!((rig.anchor() - downbeat) % master, 0);
        rig.keep_output();
        let from = rig.frame;
        rig.advance(master + 4321);
        let pcm = rig.pcm(0);
        assert_eq!(pcm[0], code(downbeat));
        plays_on_grid(&rig, &[pcm], from);
    }
}

#[test]
fn g_later_takes_and_resumes_join_the_live_phase_idle_play_restarts_the_top() {
    for (bpm, sr) in [(120u32, 48000u32), (137, 44100)] {
        let (mut rig, _, master) = committed_take(bpm, sr, 2, 0.05);
        let anchor = rig.anchor();
        rig.set_level(0.25);
        rig.press(Command::RecDub(1));
        let next = rig.next_boundary();
        rig.advance_to(next + master * 3 / 5); // 1.2 of 2 bars: commits one bar, tiled
        rig.press(Command::RecDub(1));
        rig.set_level(0.0);
        rig.advance(rig.seconds(0.1));
        assert_eq!(rig.state(1), LaneState::Playing);
        rig.keep_output();
        let from = rig.frame;
        rig.advance(master / 3);
        plays_on_grid(&rig, &[rig.pcm(0), rig.pcm(1)], from);
        // Lane 2 stops and resumes while lane 1 keeps the transport running.
        rig.press(Command::PlayStop(1));
        rig.advance(master * 77 / 100);
        rig.press(Command::PlayStop(1));
        rig.keep_output();
        let from = rig.frame;
        rig.advance(master / 2);
        plays_on_grid(&rig, &[rig.pcm(0), rig.pcm(1)], from);
        assert_eq!(rig.anchor(), anchor, "a live join never moves the grid");
        // Both stopped: PLAY re-anchors at the press and starts from the top.
        rig.press(Command::PlayStop(0));
        rig.press(Command::PlayStop(1));
        rig.advance(master * 2 / 5);
        let press = rig.frame;
        rig.keep_output();
        rig.press(Command::PlayStop(0));
        assert_eq!(rig.anchor(), press);
        rig.advance(master / 2);
        let (start, out) = rig.output.as_ref().unwrap();
        assert_eq!(*start, press);
        assert_eq!(out[0], rig.pcm(0)[0], "idle PLAY starts from frame 0");
        plays_on_grid(&rig, &[rig.pcm(0)], press);
    }
}

#[test]
fn play_beside_an_armed_lane_joins_and_idle_play_beats_the_master_grid() {
    let (mut rig, _, master) = committed_take(120, 48000, 2, 0.05);
    rig.press(Command::PlayStop(0));
    rig.advance(master / 3);
    rig.press(Command::RecDub(1)); // arms on the idle grid's next boundary
    let anchor = rig.anchor();
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.anchor(), anchor, "a PLAY while a lane is armed never moves its grid");
    rig.press(Command::Stop(1));
    rig.press(Command::PlayStop(0));
    rig.advance(master / 3);
    let mark = rig.events.len();
    let press = rig.frame;
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.anchor(), press);
    rig.advance(master);
    let beats = rig.beats_since(mark);
    let grid = Grid::master(press, master, 2);
    assert!(beats.len() >= 8 && beats.iter().enumerate().all(|(n, b)| b.0 == grid.beat_frame(n as u64)), "8 beats a loop");
}
