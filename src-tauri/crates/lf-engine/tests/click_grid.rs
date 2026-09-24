//! Ports verify/guards/grid.mjs, accent-grid.mjs and pulse-forced-clamp.mjs: the looper and the click
//! on ONE grid, the free-run pulse and its hand-offs, and the click's gates.
//!
//! The Web Audio waker (a 25 ms timer over a 100 ms lookahead) is gone: beats fire on their frame. Its
//! "stalled waker" cases map onto a jump in the device frame counter, the one way the engine can find
//! beats behind it. Clock-only cases drive `Clock` directly, as the rig guards drove `clock.ts`.

mod common;

use common::Rig;
use lf_engine::clock::{Beat, Clock};
use lf_engine::grid::{frames_per_bar, Frame, Grid, COUNT_IN_BEATS};
use lf_engine::{Command, LaneState};

const SR: u32 = 48000;

/// Fire every beat before `until`, as the engine's block loop does.
fn drive(clock: &mut Clock, until: Frame, transport: Option<Frame>) -> Vec<Beat> {
    let mut beats = Vec::new();
    while let Some(at) = clock.next_beat_frame().filter(|&at| at < until) {
        beats.extend(clock.fire_due(at, transport));
    }
    beats
}

fn clicking(clock: &mut Clock) {
    clock.set_metronome(true);
}

const LIVE: Option<Frame> = Some(Frame::MAX);

#[test]
fn b_the_master_pulse_starts_on_its_anchor_or_the_next_beat() {
    for (bpm, bars) in [(120.0, 2), (137.0, 1), (90.0, 4), (200.0, 3)] {
        let master = bars * frames_per_bar(bpm, SR);
        let mut clock = Clock::new(SR);
        clock.start_master(1000, master, bars, 1000);
        let b = clock.fire_due(1000, None).unwrap();
        assert_eq!((b.frame, b.beat_in_bar), (1000, 0));
    }
    let mut clock = Clock::new(SR);
    let now = 480_000;
    let anchor = now - 62_400; // 2.6 beat periods of 0.5 s back
    clock.start_master(anchor, 96_000, 1, now);
    let b = drive(&mut clock, now + SR as Frame, None)[0];
    assert_eq!((b.frame, b.beat_in_bar), (anchor + 3 * 24_000, 3), "resumes on beat 3, never in the past");
}

#[test]
fn c_twelve_minutes_without_phase_walk_every_boundary_an_accented_click() {
    for (bpm, sr, bars) in [(120.0, 48000, 2), (137.0, 44100, 1), (100.0, 48000, 4), (73.5, 44100, 3), (200.0, 48000, 8)] {
        let master = bars * frames_per_bar(bpm, sr);
        let mut clock = Clock::new(sr);
        clicking(&mut clock);
        let anchor = 12_345;
        clock.start_master(anchor, master, bars, anchor);
        let end = anchor + 12 * 60 * sr as Frame;
        let beats = drive(&mut clock, end, LIVE);
        let grid = Grid::master(anchor, master, bars);
        assert_eq!(beats.len() as u64, grid.first_beat_at_or_after(end), "every beat exactly once");
        for (n, b) in beats.iter().enumerate() {
            assert_eq!(b.frame, grid.beat_frame(n as u64));
            assert_eq!(b.beat_in_bar as usize, n % 4);
            assert!(b.clicked, "metronome on, transport live: every beat clicks");
            let exact = anchor as f64 + n as f64 * master as f64 / (4 * bars) as f64;
            assert!((b.frame as f64 - exact).abs() <= 0.5);
        }
        for (k, b) in beats.iter().step_by(4 * bars as usize).enumerate() {
            assert_eq!(b.frame, anchor + k as Frame * master, "loop boundary {k}");
            assert_eq!(b.beat_in_bar, 0);
        }
    }
}

#[test]
fn d_a_frame_jump_drops_past_master_beats_and_resumes_on_grid() {
    let mut clock = Clock::new(SR);
    clicking(&mut clock);
    let anchor = 480_000;
    clock.start_master(anchor, 96_000, 1, anchor);
    assert_eq!(clock.fire_due(anchor, LIVE).unwrap().frame, anchor);
    let wake = anchor + 93_120; // 1.94 s: beats 1..3 were never rendered
    assert!(clock.fire_due(wake, LIVE).is_none(), "no catch-up burst");
    let b = drive(&mut clock, anchor + 2 * SR as Frame + 1, LIVE)[0];
    assert_eq!((b.frame, b.beat_in_bar, b.clicked), (anchor + 96_000, 0, true));
}

#[test]
fn e_anti_flam_suppresses_only_a_true_coincidence() {
    // E1: quarter notes at 300 bpm are 0.2 s apart; none is suppressed.
    let mut clock = Clock::new(SR);
    clicking(&mut clock);
    clock.start_master(1000, 4 * 9600, 1, 1000);
    let beats = drive(&mut clock, 1000 + 200 * SR as Frame, LIVE);
    assert!(beats.len() >= 1000 && beats.iter().all(|b| b.clicked));
    // E2/E3: a beat already sounded at A; the re-anchored grid's beat 0 lands 10 ms later.
    let mut clock = Clock::new(SR);
    clicking(&mut clock);
    let a = 480_000;
    clock.start_master(a, 96_000, 1, a);
    assert!(clock.fire_due(a, LIVE).unwrap().clicked);
    clock.start_master(a + 480, 96_000, 1, a);
    let after = drive(&mut clock, a + 34_000, LIVE);
    assert!(!after[0].clicked && after[0].frame == a + 480, "the re-anchored beat 0 is suppressed");
    assert!(after[1].clicked && after[1].frame == a + 480 + 24_000, "the next real beat sounds");
    // E4: a long silent stretch cannot leave a stale reference.
    let mut clock = Clock::new(SR);
    clicking(&mut clock);
    clock.start_master(1000, 96_000, 1, 1000);
    drive(&mut clock, 1000 + SR as Frame, LIVE);
    clock.set_metronome(false);
    drive(&mut clock, 1000 + 301 * SR as Frame, LIVE);
    clock.set_metronome(true);
    assert!(drive(&mut clock, 1000 + 302 * SR as Frame, LIVE).iter().any(|b| b.clicked));
    // E5: exactly 0.12 s apart is outside the window.
    let mut clock = Clock::new(SR);
    clicking(&mut clock);
    clock.start_master(24_000, 4 * 5760, 1, 24_000);
    let beats = drive(&mut clock, 24_000 + 12_000, LIVE);
    assert!(beats[0].clicked && beats[1].clicked && beats[1].frame - beats[0].frame == 5760);
}

#[test]
fn accent_a_a_fresh_transport_beats_from_its_first_frame() {
    for bpm in [40u32, 90, 120, 137, 200, 300] {
        let mut rig = Rig::with(common::Opts { start: 100 * SR as Frame, ..Default::default() });
        let start = rig.frame;
        rig.set(Command::SetBpm(bpm as f64));
        let grid = Grid::tempo(start, 0, bpm, SR);
        rig.advance_to(grid.beat_frame(8) + 1);
        let beats = rig.beats();
        assert_eq!(beats.len(), 9, "bpm={bpm}");
        assert!(beats.iter().enumerate().all(|(n, b)| b.0 == grid.beat_frame(n as u64) && b.1 as usize == n % 4));
    }
}

#[test]
fn accent_b_a_tempo_change_mid_free_run_carries_the_phase() {
    for (a, b) in [(120u32, 90u32), (90, 200), (200, 40), (120, 121)] {
        let mut rig = Rig::new();
        rig.set(Command::SetBpm(a as f64));
        let before_grid = Grid::tempo(rig.frame - 1, 0, a, SR);
        rig.advance(before_grid.beat_frame(5) - rig.frame + 624);
        let before = rig.beats();
        let n = before.len() as u64;
        let expected = before_grid.beat_frame(n);
        let change = rig.frame;
        rig.set(Command::SetBpm(b as f64));
        rig.advance(rig.seconds(6.0 * 60.0 / b as f64 + 0.05));
        let after = rig.beats_since(0)[before.len()..].to_vec();
        assert_eq!(after[0].0, expected, "{a}->{b}: the next beat keeps the outgoing grid's frame");
        assert_eq!(after[0].1 as u64, n % 4, "{a}->{b}: bar index continuous");
        let new = Grid::tempo(expected, n, b, SR);
        assert!(after.iter().enumerate().all(|(k, x)| x.0 == new.beat_frame(n + k as u64) && x.1 as u64 == (n + k as u64) % 4));
        assert!(after.iter().all(|x| x.0 >= change));
    }
}

#[test]
fn accent_c_a_master_reset_carries_the_loop_grid_into_free_run() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(137.0));
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 2, 1440);
    assert!(master > 0 && rig.state(0) == LaneState::Playing);
    rig.advance(rig.seconds(60.0));
    let grid = Grid::master(rig.anchor(), master, 2);
    let before = rig.beats();
    let last = *before.last().unwrap();
    let n = grid.first_beat_at_or_after(last.0);
    assert_eq!(grid.beat_frame(n), last.0, "the loop grid is the master grid");
    rig.press(Command::ClearAll);
    rig.advance(rig.seconds(3.0));
    let after = rig.beats_since(0)[before.len()..].to_vec();
    assert_eq!(after[0].0, grid.beat_frame(n + 1), "first free-run beat where the master grid had it");
    assert_eq!(after[0].1 as u64, (n + 1) % 4);
    let free = Grid::tempo(after[0].0, n + 1, 137, SR);
    assert!(after.iter().enumerate().all(|(k, b)| b.0 == free.beat_frame(n + 1 + k as u64)));
    assert!(!rig.locked());
}

#[test]
fn accent_e_only_a_live_free_run_pulse_follows_the_tempo() {
    // A count-in pulse (unlocked here, to reach the pulse gate itself) keeps its grid.
    let mut clock = Clock::new(SR);
    clock.start_count_in(1000, COUNT_IN_BEATS, 1000);
    let grid = Grid::tempo(1000, 0, 120, SR);
    drive(&mut clock, 1000 + 14_400, None);
    clock.set_bpm(90.0, 1000 + 14_400);
    let beats: Vec<_> = drive(&mut clock, 1000 + 3 * SR as Frame, None);
    assert!(beats.iter().enumerate().all(|(k, b)| b.frame == grid.beat_frame(k as u64 + 1)));
    assert_eq!(clock.bpm(), 90);
    // A master pulse while locked: neither the tempo nor the pulse moves.
    let mut clock = Clock::new(SR);
    clock.start_master(1000, 95_990, 1, 1000);
    clock.set_locked(true);
    clock.set_bpm(90.0, 1000);
    assert_eq!((clock.bpm(), clock.next_beat_frame()), (120, Some(1000)));
    // No pulse yet: a tempo change fabricates no grid.
    let mut clock = Clock::new(SR);
    clock.set_bpm(90.0, 0);
    assert_eq!(clock.next_beat_frame(), None);
    // A value-identical tempo (120.3 rounds to 120) changes nothing.
    let mut clock = Clock::new(SR);
    clock.ensure_running(0);
    drive(&mut clock, 20_000, None);
    let next = clock.next_beat_frame();
    clock.set_bpm(120.3, 20_000);
    assert_eq!(clock.next_beat_frame(), next);
}

#[test]
fn accent_f_the_carried_beat_survives_a_large_beat_index() {
    for n in [1u64, 999, 40_000, 123_457] {
        let mut clock = Clock::new(SR);
        let now = 3 * SR as Frame;
        let want = now + 14_400;
        let anchor = want - n as Frame * 24_000;
        clock.start_master(anchor, 96_000, 1, now);
        clock.stop_master(now);
        let b = drive(&mut clock, want + 1, None)[0];
        assert_eq!((b.frame, b.beat_in_bar as u64), (want, n % 4));
    }
}

#[test]
fn forced_a_the_count_fires_every_beat_on_its_frame() {
    let mut rig = Rig::new();
    let mark = rig.events.len();
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(2.4));
    let beats = rig.beats_since(mark);
    let anchor = beats[0].0;
    assert!(beats.iter().enumerate().all(|(n, b)| b.0 == anchor + n as Frame * 24_000));
    assert_eq!(beats[..5].iter().map(|b| b.2).collect::<Vec<_>>(), [4, 3, 2, 1, 0]);
    let clicks = rig.clicks_since(mark);
    assert_eq!(clicks, [(anchor, true), (anchor + 24_000, false), (anchor + 48_000, false), (anchor + 72_000, false)]);
}

#[test]
fn forced_c_a_jump_over_count_beats_fires_them_late_as_one_click() {
    let mut clock = Clock::new(SR);
    let anchor = 480_000;
    clock.start_count_in(anchor, COUNT_IN_BEATS, anchor);
    assert!(clock.fire_due(anchor, None).unwrap().clicked);
    let wake = anchor + 76_800; // 1.6 s: beats 1, 2 and 3 were jumped over
    let mut late = Vec::new();
    while let Some(b) = clock.fire_due(wake, None) {
        late.push(b);
    }
    assert_eq!(late.iter().map(|b| (b.frame, b.count_left)).collect::<Vec<_>>(), [(wake, 3), (wake, 2), (wake, 1)]);
    assert_eq!(late.iter().filter(|b| b.clicked).count(), 1, "one click, not a burst");
    let comein = drive(&mut clock, anchor + 96_001, None)[0];
    assert_eq!((comein.frame, comein.count_left), (anchor + 96_000, 0));
}

#[test]
fn forced_d_f_the_beat_after_a_late_count_still_sounds() {
    // D: beat 3 on time, 0.2 s after the late beats 1 and 2.
    let mut clock = Clock::new(SR);
    let anchor = 480_000;
    clock.start_count_in(anchor, COUNT_IN_BEATS, anchor);
    clock.fire_due(anchor, None);
    let wake = anchor + 62_400;
    while clock.fire_due(wake, None).is_some() {}
    assert!(drive(&mut clock, anchor + 72_001, None)[0].clicked);
    // F: the come-in "1" lands 80 ms after the late count beats; it still clicks.
    for bpm in [40u32, 100, 120, 200] {
        let mut clock = Clock::new(SR);
        clock.set_bpm(bpm as f64, 0);
        clicking(&mut clock);
        clock.start_count_in(anchor, COUNT_IN_BEATS, anchor);
        let grid = Grid::tempo(anchor, 0, bpm, SR);
        clock.fire_due(anchor, LIVE);
        let wake = grid.beat_frame(4) - 3840;
        let late: Vec<_> = std::iter::from_fn(|| clock.fire_due(wake, LIVE)).collect();
        assert_eq!(late.iter().filter(|b| b.clicked).count(), 1, "bpm={bpm}");
        let comein = drive(&mut clock, grid.beat_frame(4) + 1, LIVE)[0];
        assert!(comein.clicked && comein.beat_in_bar == 0 && comein.frame == grid.beat_frame(4), "bpm={bpm}");
    }
}

#[test]
fn forced_g_a_metronome_on_first_take_never_double_strikes() {
    for stop_after_beats in [8.0, 8.02, 8.2, 8.5, 8.9, 9.97] {
        let mut rig = Rig::new();
        rig.set(Command::SetMetronome(true));
        rig.set_level(0.25);
        let mark = rig.events.len();
        rig.press(Command::RecDub(0));
        let one = rig.count_one(mark);
        rig.advance_to(one + 96_000 + rig.seconds(stop_after_beats * 0.5));
        rig.press(Command::RecDub(0));
        rig.advance(rig.seconds(9.0));
        let master = rig.master();
        assert!(master > 0 && rig.state(0) == LaneState::Playing);
        let clicks = rig.clicks_since(0);
        assert!(clicks.windows(2).all(|w| w[1].0 - w[0].0 >= 5760), "no flam after {stop_after_beats} beats");
        let grid = Grid::master(rig.anchor(), master, master / 96_000);
        let from = rig.frame - rig.seconds(6.0);
        let (lo, hi) = (grid.first_beat_at_or_after(from), grid.first_beat_at_or_after(rig.frame - SR as Frame));
        assert!(hi - lo >= 8 && (lo..hi).all(|n| clicks.iter().any(|c| c.0 == grid.beat_frame(n))));
    }
}

#[test]
fn forced_h_the_click_is_a_transport_mode() {
    let mut rig = Rig::new();
    rig.set(Command::SetMetronome(true));
    rig.set_level(0.25);
    rig.record_first_take(0, 1, 2400);
    rig.advance(rig.seconds(3.0));
    rig.press(Command::PlayStop(0));
    assert_eq!(rig.state(0), LaneState::Stopped);
    let mark = rig.events.len();
    rig.advance(rig.seconds(3.0));
    assert!(rig.beats_since(mark).len() >= 5 && rig.clicks_since(mark).is_empty(), "stopped: the LED beats, no click");
    rig.press(Command::PlayStop(0));
    let mark = rig.events.len();
    rig.advance(rig.seconds(3.0));
    let beats = rig.beats_since(mark);
    assert!(beats.len() >= 5 && beats.iter().all(|b| b.3), "live again: every beat clicks");
    // A loop-end stop: the beat on the deadline stays on the LED but is silent.
    rig.press(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    let stop_at = rig.lane(0).stop_at.expect("loop-end stop pending");
    rig.advance(rig.seconds(3.0));
    let at: Vec<_> = rig.beats().into_iter().filter(|b| b.0 == stop_at).collect();
    assert_eq!(at.len(), 1);
    assert!(!at[0].3 && rig.clicks_since(0).iter().all(|c| c.0 < stop_at));
    assert_eq!(rig.state(0), LaneState::Stopped);
    // Count beats bypass the gate and a pending stop cutoff.
    let mut clock = Clock::new(SR);
    clock.start_count_in(1000, 2, 1000);
    let beats = drive(&mut clock, 1000 + SR as Frame, Some(1000 + 12_000));
    assert_eq!(beats.iter().filter(|b| b.clicked).map(|b| b.frame).collect::<Vec<_>>(), [1000, 25_000]);
}
