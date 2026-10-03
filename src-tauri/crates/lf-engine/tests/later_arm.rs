//! Ports verify/guards/later-arm.mjs and looper-arm.mjs: a later take lands frame-exact on the master
//! grid; an armed lane aborts cleanly; the recorder slot belongs to its lane.
//!
//! later-arm's main-thread stall (the capture ring filling while the drain is blocked) has no engine
//! counterpart; what it guarded maps onto an input gap: before the window it costs nothing, inside it
//! the take is rejected, never committed shifted. looper-arm's "STOP keeps the committed PCM" on an
//! overdubbing lane: the engine's Stop discards the whole layer (the Web Audio looper dropped only the
//! pass since its last boundary swap), so the committed loop is the pre-dub loop.
//!
//! A free later take runs until the press (E10; `tests/multiply.rs` holds the take that grows the loop):
//! the takes here end with a REC press 1.3 loops in, which keeps one loop and commits at once.
//!
//! The engine's own, the `idle_` group at the end: a later take armed on an idle transport (a master,
//! every loop stopped) counts in as the first take did, and on the count's downbeat the grid re-anchors
//! and every stopped loop starts from the top, the take recording from there. A cancel during the count
//! leaves the loops stopped and gives the pulse back to the old grid; a PLAY during it is dropped, COPY
//! and TRIM are refused; a REC held for a block job counts from the frame the job is done.

mod common;

use common::{code, frame_of, Opts, Rig};
use lf_engine::grid::{frames_per_bar, Frame, Grid, COUNT_IN_BEATS};
use lf_engine::looper::job_frames;
use lf_engine::{Action, Command, Event, LaneState, Refusal};

#[test]
fn a_later_takes_land_frame_exact_on_the_master_grid() {
    for sr in [44100u32, 48000] {
        for bpm in [60u32, 97, 137, 220] {
            for bars in [1, 2] {
                let mut rig = Rig::with(common::Opts { sr, start: sr as Frame / 3, ..Default::default() });
                rig.set(Command::SetBpm(bpm as f64));
                rig.set_level(0.5);
                let master = rig.record_first_take(0, bars, 1000);
                assert_eq!(master, bars * frames_per_bar(bpm as f64, sr));
                let track1_frame0 = rig.anchor();
                rig.set_input(code);
                for phase in [0.013, 0.37, 0.81] {
                    let next = rig.next_boundary();
                    rig.advance_to(next + (phase * master as f64) as Frame);
                    rig.press(Command::RecDub(1));
                    let boundary = rig.start_frame();
                    assert_eq!(boundary, rig.next_boundary(), "arms on the next boundary");
                    rig.advance_to(boundary + master * 13 / 10);
                    rig.press(Command::RecDub(1));
                    rig.advance(master * 8 / 10);
                    let pcm = rig.pcm(1);
                    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize);
                    let first = frame_of(pcm[0], boundary);
                    assert_eq!(first, boundary, "sr={sr} bpm={bpm} bars={bars} phase={phase}");
                    assert_eq!((first - track1_frame0) % master, 0);
                    assert!(pcm.iter().enumerate().all(|(k, &x)| frame_of(x, first + k as Frame) == first + k as Frame));
                    rig.press(Command::Clear(1));
                }
            }
        }
    }
}

#[test]
fn b_a_gap_before_the_window_costs_nothing_inside_it_rejects_the_take() {
    for inside in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        let master = rig.record_first_take(0, 2, 2400);
        rig.set_input(code);
        rig.advance(rig.seconds(0.5));
        rig.press(Command::RecDub(1));
        let boundary = rig.start_frame();
        if inside {
            rig.advance_to(boundary + rig.seconds(0.3));
        }
        rig.gap();
        rig.advance(1);
        rig.advance_to(boundary + master * 13 / 10);
        rig.press(Command::RecDub(1));
        rig.advance(rig.seconds(4.5));
        if inside {
            assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0, "a damaged take is never committed");
            assert_eq!(rig.rejected(), 1, "reported once");
            assert!(rig.state(0) == LaneState::Playing && rig.master() == master);
        } else {
            let pcm = rig.pcm(1);
            assert_eq!(rig.state(1), LaneState::Playing);
            assert_eq!((frame_of(pcm[0], boundary), rig.rejected()), (boundary, 0));
        }
    }
}

fn with_master() -> (Rig, Frame) {
    let mut rig = Rig::new();
    rig.set_level(0.4);
    let master = rig.record_first_take(0, 4, 2400);
    (rig, master)
}

#[test]
fn arm_1_stopping_an_armed_later_lane_aborts_to_empty() {
    for abort in [Command::RecDub(1), Command::PlayStop(1), Command::Stop(1)] {
        let (mut rig, master) = with_master();
        rig.set_level(0.5);
        rig.advance(rig.seconds(0.9));
        rig.press(Command::RecDub(1));
        rig.advance(rig.seconds(0.2));
        assert!(rig.lane(1).armed && rig.engine.looper().written(1) == 0);
        rig.press(abort);
        assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0 && rig.window().is_none(), "{abort:?}");
        assert!(rig.master() == master && rig.state(0) == LaneState::Playing && rig.locked());
        rig.advance(master + 4800);
        assert_eq!(rig.state(1), LaneState::Empty, "nothing records at the boundary afterwards");
    }
}

#[test]
fn arm_2_3_a_later_take_records_from_frame_0_and_a_short_one_tiles() {
    let (mut rig, master) = with_master();
    rig.set_level(0.7);
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    let boundary = rig.start_frame();
    rig.advance_to(boundary + master * 13 / 10);
    rig.press(Command::RecDub(1));
    rig.advance(master);
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize && !rig.lane(1).armed);
    assert!(pcm[0] == 0.7 && pcm[master as usize - 1] == 0.7);

    let (mut rig, master) = with_master();
    let fpb = rig.fpb() as usize;
    rig.set_input(|f| ((f % 997) + 1) as f32 / 1024.0);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(1.0));
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    rig.advance(2 * master);
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize);
    assert!((fpb..master as usize).all(|k| pcm[k] == pcm[k % fpb]) && pcm[master as usize - 1] != 0.0);
}

#[test]
fn arm_4_stop_keeps_the_committed_loop_and_cancels_a_loop_end_stop() {
    let (mut rig, master) = with_master();
    let before = rig.pcm(0);
    rig.press(Command::RecDub(0));
    rig.advance(rig.seconds(0.3));
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    rig.press(Command::Stop(0));
    rig.advance(rig.seconds(0.1));
    assert_eq!(rig.state(0), LaneState::Stopped);
    assert_eq!(rig.pcm(0), before, "the pre-dub loop survives");
    assert!(rig.master() == master && rig.locked());

    let (mut rig, _) = with_master();
    rig.set(Command::SetLoopEndStop(true));
    rig.press(Command::PlayStop(0));
    assert!(rig.lane(0).stop_at.is_some() && rig.state(0) == LaneState::Playing);
    rig.press(Command::Stop(0));
    assert!(rig.lane(0).stop_at.is_none() && rig.state(0) == LaneState::Stopped);
}

#[test]
fn arm_5_only_the_recording_lane_releases_its_window() {
    let (mut rig, master) = with_master();
    rig.advance(rig.seconds(0.9));
    rig.press(Command::RecDub(1));
    let window = rig.window();
    rig.press(Command::Clear(3));
    rig.press(Command::Stop(4));
    assert_eq!(rig.window(), window);
    rig.press(Command::Clear(0)); // the only committed lane
    assert!(rig.master() == master && rig.locked(), "an in-flight lane keeps the grid");
    rig.press(Command::Stop(1));
    assert!(rig.window().is_none() && rig.master() == 0 && !rig.locked(), "its own cancel resets the blank session");
    assert!(rig.events.iter().any(|e| matches!(e, Event::Transport { master: 0, locked: false, .. })));
}

/// Every beat since `mark` sits exactly on the master grid (4 * bars beats per master).
fn beats_on_master_grid(rig: &Rig, mark: usize, bars: Frame) -> usize {
    let grid = lf_engine::grid::Grid::master(rig.anchor(), rig.master(), bars);
    let beats = rig.beats_since(mark);
    for b in &beats {
        assert_eq!(grid.beat_frame(grid.first_beat_at_or_after(b.0)), b.0, "beat at {} is off the master grid", b.0);
    }
    beats.len()
}

#[test]
fn an_aborted_or_rejected_later_take_keeps_the_master_pulse() {
    // 137 bpm at 44.1 kHz: a master beat (19313.75 frames) and a tempo beat (19313.87) part within a few beats.
    for reject in [false, true] {
        let mut rig = Rig::at(44100);
        rig.set(Command::SetBpm(137.0));
        rig.set_level(0.5);
        rig.record_first_take(0, 2, 1000);
        rig.press(Command::RecDub(1));
        if reject {
            rig.advance_to(rig.start_frame() + 1000);
            rig.gap();
            rig.advance(10);
            rig.press(Command::PlayStop(1));
            rig.advance_to(rig.end_frame() + 1);
            assert_eq!(rig.state(1), LaneState::Empty);
        } else {
            rig.press(Command::Stop(1));
        }
        let mark = rig.events.len();
        rig.advance(rig.seconds(15.0));
        assert!(beats_on_master_grid(&rig, mark, 2) >= 30);
    }
}

#[test]
fn a_gap_on_a_window_edge_damages_nothing() {
    for at_end in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        let master = rig.record_first_take(0, 2, 2400);
        rig.set_input(code);
        // FIXED at the loop's bars: the window closes on its own edge, one loop in.
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(2.0));
        rig.press(Command::RecDub(1));
        let (start, end) = (rig.start_frame(), rig.end_frame());
        assert_eq!(end - start, master);
        rig.advance_to(if at_end { end } else { start });
        rig.gap(); // the block starting on the edge follows the gap
        rig.advance(master + 4800);
        assert!(rig.state(1) == LaneState::Playing && rig.rejected() == 0, "a gap on the {} edge", if at_end { "end" } else { "start" });
    }
}

#[test]
fn a_jump_across_a_window_edge_rejects_the_take() {
    // The frames the device skipped overlap the window although the block after them starts outside it.
    for at_end in [false, true] {
        let mut rig = Rig::new();
        rig.set_level(0.5);
        let master = rig.record_first_take(0, 2, 2400);
        rig.set_input(code);
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(2.0));
        rig.press(Command::RecDub(1));
        let edge = if at_end { rig.end_frame() } else { rig.start_frame() };
        rig.advance_to(edge - 64);
        rig.skip(128);
        rig.advance(master + 4800);
        let side = if at_end { "end" } else { "start" };
        assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0, "a jump across the {side}: never committed");
        assert_eq!(rig.rejected(), 1, "a jump across the {side}: rejected once");
        assert!(rig.state(0) == LaneState::Playing && rig.master() == master);
    }
}

#[test]
fn a_take_armed_inside_a_damaged_block_whose_window_opens_there_is_rejected() {
    // The block's damage reaches the looper before the press in it does: the window it opens there must
    // still see it.
    let mut rig = Rig::new();
    rig.set_level(0.5);
    let master = rig.record_first_take(0, 2, 2400);
    rig.set_input(code);
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(2.0));
    let boundary = rig.next_boundary();
    rig.advance_to(boundary - 100);
    rig.block = 1024;
    rig.send_at(boundary - 100, Command::RecDub(1)); // its window opens on the boundary, inside the block
    rig.damage(); // the block [boundary - 100, boundary + 924) renders from silence
    rig.advance(1024);
    assert_eq!(rig.start_frame(), boundary);
    rig.block = 128;
    rig.advance(master + 4800);
    assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0, "never committed");
    assert_eq!(rig.rejected(), 1);
    assert!(rig.state(0) == LaneState::Playing && rig.master() == master);
}

// ── A later take from an idle transport: the count-in, then every loop from the top ────────────────

/// A committed 2-bar first take on lane 0 that holds the frame code, stopped at an arbitrary phase: an
/// idle transport over a master, the input still the frame code. Returns the master and lane 0's loop.
fn idle_master(opts: Opts, bpm: u32) -> (Rig, Frame, Vec<f32>) {
    let mut rig = Rig::with(opts);
    rig.set(Command::SetBpm(bpm as f64));
    rig.set_input(code);
    let master = rig.record_first_take(0, 2, 1000);
    rig.idle();
    rig.advance(master * 37 / 100);
    rig.press(Command::PlayStop(0));
    rig.advance(master * 29 / 100 + 777);
    assert!(rig.state(0) == LaneState::Stopped && rig.window().is_none());
    let loop0 = rig.pcm(0);
    (rig, master, loop0)
}

/// The count-in a REC applied at `press` starts: its tempo grid, and the downbeat it counts to.
fn count_grid(rig: &Rig, press: Frame) -> (Grid, Frame) {
    let grid = Grid::tempo(press, 0, rig.bpm(), rig.sr);
    (grid, grid.beat_frame(COUNT_IN_BEATS))
}

/// REC on the EMPTY `lane` at the current frame: the press frame, the event mark before it and the
/// downbeat its count-in ends on.
fn arm_idle(rig: &mut Rig, lane: u8) -> (Frame, usize, Frame) {
    let (press, mark) = (rig.frame, rig.events.len());
    rig.press(Command::RecDub(lane));
    (press, mark, count_grid(rig, press).1)
}

/// What the looper itself played on the frame rendered last: the lanes and the click, no monitor.
fn looper_tap(rig: &Rig) -> f32 {
    *rig.engine.taps().looper.last().unwrap()
}

fn refusals(rig: &Rig, mark: usize) -> Vec<(u8, Refusal)> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Refused { lane, reason, .. } => Some((lane, reason)),
            _ => None,
        })
        .collect()
}

/// A whole counted-in take at one block size and alignment: its events and output from the press on.
fn counted_in_take(block: usize, align: Frame) -> (Vec<Event>, Vec<f32>) {
    let label = format!("block={block} align={align}");
    let (mut rig, master, loop0) = idle_master(Opts { sr: 8000, start: 8000, block, align, ..Default::default() }, 120);
    let old_anchor = rig.anchor();
    let old_origin = rig.engine.looper().grid_origin();
    rig.keep_output();
    let (press, mark, downbeat) = arm_idle(&mut rig, 1);
    let (grid, _) = count_grid(&rig, press);
    assert!(rig.lane(1).armed && rig.state(1) == LaneState::Recording, "{label}");
    assert_eq!(rig.start_frame(), downbeat + align, "{label}: the window opens `align` after the counted downbeat");
    // Up to the frame before the downbeat: four forced clicks on the tempo grid from the press, lane 0
    // silent, the grid where it was.
    rig.advance_to(downbeat);
    let count: Vec<_> = (0..4u8).map(|n| (grid.beat_frame(n as u64), n, 4 - n, true)).collect();
    assert_eq!(rig.beats_since(mark), count, "{label}");
    assert!(rig.state(0) == LaneState::Stopped && rig.anchor() == old_anchor, "{label}: nothing moves before the downbeat");
    assert_eq!(rig.engine.looper().grid_origin(), old_origin, "{label}: the FX grid stays where it was before the downbeat");
    // The downbeat's own frame: the grid re-anchors there and lane 0 plays its loop position 0.
    rig.advance(1);
    assert!(rig.state(0) == LaneState::Playing && rig.anchor() == downbeat, "{label}: the loops restart on the downbeat");
    assert_eq!(rig.engine.looper().grid_origin(), downbeat, "{label}: the FX grid restarts on the downbeat");
    assert_eq!(looper_tap(&rig), loop0[0], "{label}: lane 0 from the top");
    let started = |e: &Event| matches!(e, Event::Lane { frame, lane: 0, info } if *frame == downbeat && info.state == LaneState::Playing);
    assert!(rig.events[mark..].iter().any(started), "{label}: lane 0 reported PLAYING on the downbeat's frame");
    assert_eq!(rig.beats_since(mark)[4], (downbeat, 0, 0, false), "{label}: the master's first beat, no count left");
    rig.advance(1);
    assert_eq!(looper_tap(&rig), loop0[1], "{label}");
    // The take: free, stopped 1.3 loops in, it keeps one loop from its first frame.
    rig.advance_to(downbeat + align + master * 13 / 10);
    rig.press(Command::RecDub(1));
    rig.advance(master * 8 / 10);
    rig.idle();
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize, "{label}");
    assert!(rig.master() == master && rig.anchor() == downbeat, "{label}: the grid stays on the downbeat");
    assert_eq!(rig.engine.looper().grid_origin(), downbeat, "{label}: the FX grid stays on the downbeat");
    let first = downbeat + align;
    assert_eq!(frame_of(pcm[0], first), first, "{label}: the take's first kept frame");
    assert!(pcm.iter().enumerate().all(|(k, &x)| frame_of(x, first + k as Frame) == first + k as Frame), "{label}");
    (rig.events[mark..].to_vec(), rig.output.unwrap().1)
}

#[test]
fn idle_1_a_later_take_from_idle_counts_in_restarts_the_loops_on_its_downbeat_and_records_from_there() {
    for align in [0, 37] {
        let reference = counted_in_take(61, align);
        for block in [1, 1024] {
            assert!(counted_in_take(block, align) == reference, "align={align} block={block}: the same events and output at any block size");
        }
    }
}

#[test]
fn idle_2_the_pulse_takes_the_restarted_grid_and_a_cancelled_count_gives_back_the_old_one() {
    // 137 bpm at 44.1 kHz, where the count's tempo grid and the master grid part within a few beats.
    let opts = || Opts { sr: 44100, start: 44100, ..Default::default() };

    // A completed count: from the downbeat on, every beat sits on the master grid anchored there (a
    // cancel past the downbeat is an ordinary one: the loops play on).
    let (mut rig, _, _) = idle_master(opts(), 137);
    let (_, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(downbeat);
    let mark = rig.events.len();
    rig.advance(1000);
    rig.press(Command::Stop(1));
    assert!(rig.state(1) == LaneState::Empty && rig.state(0) == LaneState::Playing && rig.anchor() == downbeat);
    rig.advance(rig.seconds(15.0));
    assert!(beats_on_master_grid(&rig, mark, 2) >= 30);
    assert_eq!(rig.beats_since(mark)[0], (downbeat, 0, 0, false));

    // Cancelled between the count's 2 and 3: every later beat sits on the OLD master grid, none forced,
    // none clicked (the metronome is off).
    let (mut rig, master, _) = idle_master(opts(), 137);
    let old = Grid::master(rig.anchor(), master, 2);
    let (press, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(press + (downbeat - press) * 3 / 8);
    let (mark, cancel) = (rig.events.len(), rig.frame);
    rig.press(Command::Stop(1));
    rig.advance(rig.seconds(15.0));
    let beats = rig.beats_since(mark);
    assert!(beats.len() >= 30);
    assert_eq!(beats[0].0, old.beat_frame(old.first_beat_at_or_after(cancel)), "the old grid's very next beat");
    for b in &beats {
        assert_eq!(old.beat_frame(old.first_beat_at_or_after(b.0)), b.0, "beat at {} is off the old master grid", b.0);
        assert!(b.2 == 0 && !b.3, "beat at {}: no count beat and no click after the cancel", b.0);
    }

    // Rejected for an input gap inside its window: the loops play on, on the new grid.
    let (mut rig, master, _) = idle_master(opts(), 137);
    let (_, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(downbeat + 1000);
    rig.gap();
    rig.advance(10);
    rig.press(Command::PlayStop(1));
    rig.advance_to(rig.end_frame() + 1);
    assert!(rig.state(1) == LaneState::Empty && rig.rejected() == 1);
    assert!(rig.state(0) == LaneState::Playing && rig.master() == master && rig.anchor() == downbeat);
    let mark = rig.events.len();
    rig.advance(rig.seconds(15.0));
    assert!(beats_on_master_grid(&rig, mark, 2) >= 30);
}

#[test]
fn idle_3_every_stopped_lane_starts_on_the_downbeat_a_reversed_one_from_its_heard_top_a_muted_one_muted() {
    let (mut rig, master, loop0) = idle_master(Opts::default(), 120);
    for _ in 0..2 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::Reverse(1));
    rig.press(Command::SetMute(2, true));
    rig.advance(rig.seconds(0.5)); // the muted lane's gain has glided to silence
    assert!((0..3).all(|i| rig.state(i) == LaneState::Stopped) && rig.lane(1).reversed);
    let (_, _, downbeat) = arm_idle(&mut rig, 4);
    rig.advance_to(downbeat);
    assert!((0..3).all(|i| rig.state(i) == LaneState::Stopped), "stopped until the downbeat");
    let last = master as usize - 1;
    for k in 0..3 {
        rig.advance(1);
        assert!((0..3).all(|i| rig.state(i) == LaneState::Playing), "every stopped lane plays from the downbeat");
        assert!(rig.state(3) == LaneState::Empty && rig.lane(1).reversed && rig.engine.looper().volume(2).1);
        // Lane 0 from its top, the reversed copy from its heard top (the loop's last frame), the muted copy silent.
        assert_eq!(looper_tap(&rig), loop0[k] + loop0[last - k], "frame {k} past the downbeat");
    }
    assert_eq!(rig.anchor(), downbeat);
}

#[test]
fn idle_4_a_cancel_during_the_count_leaves_the_loops_stopped_and_the_grid_where_it_was() {
    type Cancel = (&'static str, fn(&mut Rig));
    let cancels: [Cancel; 6] = [
        ("REC again", |r| r.press(Command::RecDub(1))),
        ("PLAY/STOP", |r| r.press(Command::PlayStop(1))),
        ("STOP", |r| r.press(Command::Stop(1))),
        ("STOP ALL", |r| r.press(Command::StopAll)),
        ("CLEAR", |r| r.press(Command::Clear(1))),
        ("HOLD release", |r| r.press(Command::Action(Action::Release(0)))),
    ];
    for (label, cancel) in cancels {
        let (mut rig, master, loop0) = idle_master(Opts::default(), 120);
        let anchor = rig.anchor();
        let origin = rig.engine.looper().grid_origin();
        rig.press(Command::SelectTrack(1));
        let (press, mark) = (rig.frame, rig.events.len());
        // The HOLD pedal arms with its press and cancels with its release.
        rig.press(if label == "HOLD release" { Command::Action(Action::Hold(0)) } else { Command::RecDub(1) });
        let downbeat = count_grid(&rig, press).1;
        rig.advance_to(press + (downbeat - press) * 5 / 8);
        assert!(rig.lane(1).armed && rig.start_frame() == downbeat, "{label}");
        cancel(&mut rig);
        assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0 && rig.window().is_none(), "{label}: back to EMPTY, the recorder free");
        assert!(rig.state(0) == LaneState::Stopped && rig.anchor() == anchor && rig.master() == master && rig.locked(), "{label}");
        assert_eq!(rig.engine.looper().grid_origin(), origin, "{label}: the FX grid stays where it was");
        rig.advance_to(downbeat + master);
        assert!(rig.state(0) == LaneState::Stopped && rig.state(1) == LaneState::Empty && rig.anchor() == anchor, "{label}: nothing starts on the cancelled downbeat");
        assert_eq!(rig.engine.looper().grid_origin(), origin, "{label}: the FX grid is not moved by the cancelled downbeat");
        assert_eq!(rig.pcm(0), loop0, "{label}");
        let counted: Vec<u8> = rig.beats_since(mark).iter().map(|b| b.2).filter(|&left| left > 0).collect();
        assert_eq!(counted, [4, 3, 2], "{label}: the count stops with the cancel");
    }
}

#[test]
fn idle_5_a_play_during_the_count_is_dropped_and_copy_and_trim_are_refused_until_the_downbeat() {
    let (mut rig, _, _) = idle_master(Opts::default(), 120);
    let anchor = rig.anchor();
    let (press, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(press + (downbeat - press) / 3);
    for play in [Command::PlayStop(0), Command::ActionOn(0, Action::PlayStop), Command::PlayAll, Command::Action(Action::PlayAll)] {
        rig.press(play);
        assert!(rig.state(0) == LaneState::Stopped && rig.anchor() == anchor && !rig.engine.holding(), "{play:?} is dropped");
        assert!(rig.lane(1).armed && rig.start_frame() == downbeat, "{play:?} leaves the count alone");
    }
    let mark = rig.events.len();
    rig.press(Command::ActionOn(0, Action::Copy));
    rig.press(Command::ActionOn(0, Action::Halve));
    assert_eq!(refusals(&rig, mark), [(0, Refusal::OtherRecording), (0, Refusal::OtherRecording)], "the pedal's COPY and HALVE");
    let mark = rig.events.len();
    rig.press(Command::Copy(0));
    assert_eq!(refusals(&rig, mark), [], "a direct COPY is dropped as it always refuses: silently");
    rig.press(Command::Trim(0, 1));
    assert_eq!(refusals(&rig, mark), [(0, Refusal::OtherRecording)], "a direct TRIM says why");
    assert!(!rig.engine.looper().busy() && !rig.engine.holding(), "no block job, nothing held across the downbeat");
    assert!((2..5).all(|i| rig.state(i) == LaneState::Empty) && !rig.lane(0).can_undo, "nothing copied, nothing trimmed");
    rig.advance_to(downbeat);
    assert_eq!(rig.state(0), LaneState::Stopped, "stopped until the downbeat");
    rig.advance(1);
    assert!(rig.state(0) == LaneState::Playing && rig.anchor() == downbeat);
    // From the downbeat both work again.
    let mark = rig.events.len();
    rig.press(Command::ActionOn(0, Action::Copy));
    assert!(rig.engine.looper().busy(), "COPY runs again");
    rig.idle();
    assert_eq!(rig.state(2), LaneState::Playing);
    rig.press(Command::ActionOn(0, Action::Halve));
    rig.idle();
    assert!(rig.lane(0).can_undo, "TRIM runs again");
    assert_eq!(refusals(&rig, mark), []);
}

/// A later take on lane 1 over the 2-bar master under `settings`, armed from an idle transport or
/// beside the playing loop, stopped `stop` frames past its boundary (`None`: its window closes by
/// itself), and what must not depend on how it was armed, measured from that boundary (the count's
/// downbeat; the loop boundary it waited for): the anchor's offset in the loop, the master, the first
/// kept frame, how many frames follow it in order, and both lanes' states.
fn later_take(idle: bool, settings: &[Command], stop: Option<Frame>, run: Frame) -> (Frame, Frame, Frame, usize, LaneState, LaneState) {
    let (mut rig, master, _) = idle_master(Opts { align: 53, ..Default::default() }, 120);
    for &setting in settings {
        rig.set(setting);
    }
    if !idle {
        rig.press(Command::PlayStop(0));
        rig.advance(master * 3 / 7);
    }
    let press = rig.frame;
    rig.press(Command::RecDub(1));
    let boundary = rig.start_frame() - rig.align;
    assert_eq!(boundary, if idle { count_grid(&rig, press).1 } else { rig.next_boundary() });
    if let Some(stop) = stop {
        rig.advance_to(boundary + stop);
        rig.press(Command::RecDub(1));
    }
    rig.advance_to(boundary + run);
    rig.idle();
    if idle {
        assert_eq!(rig.anchor(), boundary, "the anchor stays the count's downbeat");
    }
    let pcm = rig.pcm(1);
    let first = frame_of(pcm[0], boundary);
    let in_order = pcm.iter().enumerate().take_while(|&(k, &x)| frame_of(x, first + k as Frame) == first + k as Frame).count();
    ((rig.anchor() - boundary).rem_euclid(rig.master()), rig.master(), first - boundary, in_order, rig.state(1), rig.state(0))
}

#[test]
fn idle_6_fixed_a_multiply_a_free_take_and_a_retake_roll_from_idle_match_their_playing_twins() {
    let m = 2 * frames_per_bar(120.0, 48000);
    let (on, playing) = (Command::SetFixedLength(true), LaneState::Playing);
    let cases: [(&str, Vec<Command>, Option<Frame>, Frame, (Frame, Frame, Frame, usize, LaneState, LaneState)); 4] = [
        ("FIXED, one loop", vec![on, Command::SetFixedBars(2.0)], None, m * 3 / 2, (0, m, 53, m as usize, playing, playing)),
        ("FIXED past the master: a multiply", vec![on, Command::SetFixedBars(4.0)], None, m * 5 / 2, (0, 2 * m, 53, 2 * m as usize, playing, playing)),
        ("free, stopped 1.3 loops in", vec![], Some(m * 13 / 10), m * 5 / 2, (0, m, 53, m as usize, playing, playing)),
        ("a RETAKE roll stopped mid pass 3: pass 2 kept", vec![Command::SetRetake(true)], Some(m * 5 / 2), 3 * m, (0, m, 53 + m, m as usize, playing, playing)),
    ];
    for (label, settings, stop, run, want) in cases {
        let from_idle = later_take(true, &settings, stop, run);
        assert_eq!(from_idle, want, "{label}, from idle");
        assert_eq!(from_idle, later_take(false, &settings, stop, run), "{label}: as beside a playing loop");
    }
}

#[test]
fn idle_7_a_jump_across_the_downbeat_restarts_on_the_scheduled_downbeat_and_completes_the_count() {
    let (mut rig, master, loop0) = idle_master(Opts::default(), 120);
    let (press, _, downbeat) = arm_idle(&mut rig, 1);
    let (grid, _) = count_grid(&rig, press);
    rig.advance_to(grid.beat_frame(1) + 100); // the count's 1 and 2 have sounded
    let mark = rig.events.len();
    let landed = downbeat + 5000;
    rig.skip(landed - rig.frame);
    rig.advance(1);
    assert_eq!(rig.anchor(), downbeat, "the scheduled downbeat, not the frame it was delivered on");
    assert_eq!(rig.state(0), LaneState::Playing);
    assert_eq!(looper_tap(&rig), loop0[5000], "the loop plays at the phase the jump landed on");
    let beats = rig.beats_since(mark);
    let count: Vec<_> = beats.iter().map(|b| (b.0, b.1, b.2)).collect();
    assert_eq!(count, [(landed, 2, 2), (landed, 3, 1)], "the count's last two beats fire late, and no tempo-grid beat after them");
    assert_eq!(beats.iter().filter(|b| b.3).count(), 1, "as one click");
    // The jump's gap lies inside the take's window: the existing rule rejects it, the loops play on.
    rig.advance(master / 2);
    rig.press(Command::RecDub(1));
    rig.advance(master);
    assert!(rig.state(1) == LaneState::Empty && rig.rejected() == 1, "the take the jump cut is rejected");
    assert!(rig.state(0) == LaneState::Playing && rig.anchor() == downbeat && rig.master() == master);
    let at = rig.frame;
    rig.advance(1);
    assert_eq!(looper_tap(&rig), loop0[(at - downbeat).rem_euclid(master) as usize]);
}

#[test]
fn idle_8_a_rec_held_for_a_block_job_counts_from_the_frame_the_job_is_done() {
    let (mut rig, master, _) = idle_master(Opts::default(), 120);
    let copy = rig.frame;
    rig.press(Command::Copy(0)); // into lane 1, both stopped
    assert!(rig.engine.looper().busy());
    let done = copy + job_frames(master);
    let (press, mark) = (rig.frame, rig.events.len());
    rig.press(Command::RecDub(2));
    assert!(rig.engine.holding() && rig.state(2) == LaneState::Empty && rig.window().is_none(), "held behind the COPY");
    rig.advance_to(done + 1);
    assert!(press < done && rig.count_one(mark) == done, "the count's 1 on the frame the job is done, not on the press");
    let downbeat = count_grid(&rig, done).1;
    assert!(rig.lane(2).armed && rig.start_frame() == downbeat);
    assert!(rig.state(0) == LaneState::Stopped && rig.state(1) == LaneState::Stopped);
    rig.advance_to(downbeat + 1);
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Playing && rig.anchor() == downbeat);
}

#[test]
fn idle_9_an_arm_beside_a_playing_loop_is_not_held_for_another_lanes_job() {
    let (mut rig, master, _) = idle_master(Opts { align: 53, ..Default::default() }, 120);
    rig.press(Command::PlayStop(0));
    rig.advance(master * 3 / 7);
    rig.press(Command::Copy(0)); // a block job into lane 1, lane 0 playing
    assert!(rig.engine.looper().busy() && rig.state(0) == LaneState::Playing);
    rig.press(Command::RecDub(2));
    assert!(rig.engine.looper().busy() && !rig.engine.holding(), "armed at once: only an idle count waits for every job");
    assert!(rig.state(2) == LaneState::Recording && rig.lane(2).armed);
    assert_eq!(rig.start_frame(), rig.next_boundary() + rig.align, "the window opens `align` after the next loop boundary");
}

#[test]
fn idle_10_a_device_lost_during_the_count_leaves_the_loops_stopped_and_gives_back_the_old_pulse() {
    // 137 bpm at 44.1 kHz, as `idle_2`: the count's tempo grid and the old master grid part within a few beats.
    let (mut rig, master, loop0) = idle_master(Opts { sr: 44100, start: 44100, ..Default::default() }, 137);
    let old = Grid::master(rig.anchor(), master, 2);
    let (anchor, origin) = (rig.anchor(), rig.engine.looper().grid_origin());
    let (press, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(press + (downbeat - press) * 3 / 8); // between the count's 2 and 3
    let (mark, lost) = (rig.events.len(), rig.frame);
    rig.punch_out();
    assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0 && rig.window().is_none(), "back to EMPTY, the recorder free");
    assert!(rig.state(0) == LaneState::Stopped && rig.master() == master && rig.locked());
    assert!(rig.anchor() == anchor && rig.engine.looper().grid_origin() == origin, "the grid stays where it was");
    rig.advance(rig.seconds(15.0));
    assert!(rig.state(0) == LaneState::Stopped && rig.state(1) == LaneState::Empty && rig.anchor() == anchor, "nothing starts on the lost count's downbeat");
    assert_eq!(rig.engine.looper().grid_origin(), origin);
    assert_eq!(rig.pcm(0), loop0);
    let beats = rig.beats_since(mark);
    assert!(beats.len() >= 30);
    assert_eq!(beats[0].0, old.beat_frame(old.first_beat_at_or_after(lost)), "the old grid's very next beat");
    for b in &beats {
        assert_eq!(old.beat_frame(old.first_beat_at_or_after(b.0)), b.0, "beat at {} is off the old master grid", b.0);
        assert!(b.2 == 0 && !b.3, "beat at {}: no count beat and no click after the device is lost", b.0);
    }
}

#[test]
fn idle_11_clear_all_during_the_count_blanks_the_session_and_clearing_the_only_loop_keeps_the_count() {
    // (a) CLEAR ALL: the count's lane goes with the loops, and the blank session's pulse runs free.
    let (mut rig, _, _) = idle_master(Opts::default(), 120);
    let (press, _, downbeat) = arm_idle(&mut rig, 1);
    rig.advance_to(press + (downbeat - press) * 3 / 8);
    let mark = rig.events.len();
    rig.press(Command::ClearAll);
    assert!((0..5).all(|i| rig.state(i) == LaneState::Empty && rig.lane(i).length == 0) && rig.window().is_none(), "every lane EMPTY");
    assert!(rig.master() == 0 && !rig.locked(), "no master, the tempo unlocked");
    assert!(rig.events[mark..].iter().any(|e| matches!(e, Event::Transport { master: 0, locked: false, .. })));
    rig.advance_to(downbeat + rig.seconds(4.0));
    assert!((0..5).all(|i| rig.state(i) == LaneState::Empty) && rig.master() == 0, "nothing starts on the cleared count's downbeat");
    let beats = rig.beats_since(mark);
    assert!(beats.len() >= 8 && beats.iter().all(|b| b.2 == 0 && !b.3), "no count beat and no click after CLEAR ALL");

    // (b) The only loop cleared: the recorder holds the grid, the count runs on, the downbeat re-anchors
    // with no lane to resume, and the take is an ordinary later take one master long.
    let align = 37;
    let (mut rig, master, _) = idle_master(Opts { align, ..Default::default() }, 120);
    let anchor = rig.anchor();
    let (press, mark, downbeat) = arm_idle(&mut rig, 1);
    let (grid, _) = count_grid(&rig, press);
    rig.advance_to(press + (downbeat - press) * 3 / 8);
    rig.press(Command::Clear(0));
    assert!(rig.state(0) == LaneState::Empty && rig.lane(0).length == 0, "the loop is cleared");
    assert!(rig.master() == master && rig.locked() && rig.anchor() == anchor, "the count's recorder keeps the grid");
    assert!(rig.lane(1).armed && rig.state(1) == LaneState::Recording && rig.start_frame() == downbeat + align, "the count runs on");
    rig.advance_to(downbeat);
    assert_eq!(rig.anchor(), anchor, "nothing moves before the downbeat");
    rig.advance(1);
    assert!(rig.anchor() == downbeat && rig.engine.looper().grid_origin() == downbeat, "the grid re-anchors on the downbeat");
    assert!(rig.state(0) == LaneState::Empty && (2..5).all(|i| rig.state(i) == LaneState::Empty), "no lane to resume");
    let count: Vec<_> = (0..4u8).map(|n| (grid.beat_frame(n as u64), n, 4 - n, true)).collect();
    assert_eq!(rig.beats_since(mark)[..5], [&count[..], &[(downbeat, 0, 0, false)]].concat(), "the whole count, then the master's first beat");
    rig.advance_to(downbeat + align + master * 13 / 10);
    rig.press(Command::RecDub(1));
    rig.advance(master * 8 / 10);
    rig.idle();
    let pcm = rig.pcm(1);
    assert!(rig.state(1) == LaneState::Playing && pcm.len() == master as usize, "one master long, PLAYING");
    assert!(rig.master() == master && rig.anchor() == downbeat && rig.state(0) == LaneState::Empty);
    let first = downbeat + align;
    assert_eq!(frame_of(pcm[0], first), first, "the take records from the downbeat plus `align`");
    assert!(pcm.iter().enumerate().all(|(k, &x)| frame_of(x, first + k as Frame) == first + k as Frame));
}

#[test]
fn idle_12_a_cancel_queued_behind_a_held_rec_leaves_no_count_behind() {
    let (mut rig, master, loop0) = idle_master(Opts::default(), 120);
    let (anchor, origin) = (rig.anchor(), rig.engine.looper().grid_origin());
    let copy = rig.frame;
    rig.press(Command::Copy(0)); // into lane 1, both stopped
    let done = copy + job_frames(master);
    let mark = rig.events.len();
    rig.press(Command::RecDub(2));
    rig.press(Command::Stop(2)); // waits behind the held REC, and cancels it on the frame it is applied
    assert!(rig.frame < done && rig.engine.holding() && rig.state(2) == LaneState::Empty && rig.window().is_none(), "both wait for the COPY");
    rig.advance_to(done + 1);
    assert!(!rig.engine.looper().busy() && !rig.engine.holding(), "the job is done, nothing held");
    assert!(rig.state(2) == LaneState::Empty && rig.lane(2).length == 0 && rig.window().is_none(), "back to EMPTY, the recorder free");
    let downbeat = count_grid(&rig, done).1;
    rig.advance_to(downbeat + master);
    assert!(rig.state(0) == LaneState::Stopped && rig.state(1) == LaneState::Stopped && rig.state(2) == LaneState::Empty, "nothing starts on the cancelled downbeat");
    assert!(rig.anchor() == anchor && rig.engine.looper().grid_origin() == origin && rig.master() == master && rig.locked());
    assert_eq!(rig.pcm(0), loop0);
    let beats = rig.beats_since(mark);
    assert!(!beats.is_empty() && beats.iter().all(|b| b.2 == 0 && !b.3), "no count beat ever fired: {beats:?}");
}

#[test]
fn idle_13_a_command_on_the_downbeats_own_frame_finds_the_loops_restarted() {
    // (a) PLAY/STOP on a stopped loop, stamped for the downbeat: the restart runs first, so the press
    // finds the loop playing and stops it (during the count it would have been dropped).
    let (mut rig, master, _) = idle_master(Opts::default(), 120);
    let (_, _, downbeat) = arm_idle(&mut rig, 1);
    rig.send_at(downbeat, Command::PlayStop(0));
    rig.advance_to(downbeat);
    assert!(rig.state(0) == LaneState::Stopped && rig.anchor() != downbeat, "nothing moves before the downbeat");
    rig.advance(1);
    assert_eq!(rig.state(0), LaneState::Stopped, "the press stops the loop the downbeat started");
    assert!(rig.anchor() == downbeat && rig.engine.looper().grid_origin() == downbeat && rig.master() == master, "the grid re-anchored");
    assert!(rig.state(1) == LaneState::Recording && !rig.lane(1).armed, "the take records on");

    // (b) REC on the counted lane, stamped for the downbeat, its window still `align` ahead: the loops
    // have restarted, and the press cancels the armed take as an ordinary later one.
    let (mut rig, master, _) = idle_master(Opts { align: 37, ..Default::default() }, 120);
    let (_, _, downbeat) = arm_idle(&mut rig, 1);
    rig.send_at(downbeat, Command::RecDub(1));
    rig.advance_to(downbeat + 1);
    assert!(rig.state(1) == LaneState::Empty && rig.lane(1).length == 0 && rig.window().is_none(), "back to EMPTY, the recorder free");
    assert!(rig.state(0) == LaneState::Playing && rig.anchor() == downbeat && rig.master() == master, "the loops play on, on the new grid");
    let mark = rig.events.len();
    rig.advance(rig.seconds(8.0));
    assert!(beats_on_master_grid(&rig, mark, 2) >= 16);
    assert!(rig.state(0) == LaneState::Playing && rig.state(1) == LaneState::Empty);
}
