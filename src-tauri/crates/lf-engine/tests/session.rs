//! Saving and loading a session through the engine (`lf_engine::session`): a snapshot of the committed
//! loops in play order, copied a budget per rendered frame, and a load into an empty engine that plays
//! them back sample-exact on a fresh grid. Every `process` runs under the rig's `assert_no_alloc`, so the
//! engine side of both allocates nothing; the buffers are the test's (the host's), built outside it.
//! A snapshot in flight leaves the rendered output bit for bit as it was (the Web Audio probe
//! recovery-playback.mjs's continuity).

mod common;

use common::{code, Rig};
use lf_engine::dsp::fx::FxKind;
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::session::SNAPSHOT_RATE;
use lf_engine::{Command, LaneState, Load, LoadTrack, SessionError, SessionJob, Snapshot};

/// Hand `job` to the engine and render until it comes back.
fn run(rig: &mut Rig, job: SessionJob) -> SessionJob {
    assert!(rig.session().send(Box::new(job)).is_ok(), "the port takes one job");
    for _ in 0..100_000 {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            return *job;
        }
    }
    panic!("the session job never came back");
}

/// A snapshot into a destination of `samples` (written through, as the host touches its pages).
fn snapshot(rig: &mut Rig, samples: usize) -> Snapshot {
    let mut pcm = Vec::with_capacity(samples);
    pcm.resize(samples, 0.0f32);
    match run(rig, SessionJob::Snapshot(Snapshot::new(pcm))) {
        SessionJob::Snapshot(s) => s,
        SessionJob::Load(_) => unreachable!(),
    }
}

/// Three committed lanes at 120 BPM, a one-bar master: lane 0 the frame code, lane 1 a later take of
/// another signal (then reversed), lane 2 a later take, stopped. Each later take ends with REC 1.3 loops
/// in: one loop, committed at once (E10).
fn three_lanes() -> Rig {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    for (lane, signal) in [(1u8, 0.5f32), (2, -0.25)] {
        rig.set_input(move |f| signal * code(f) * 64.0);
        rig.press(Command::RecDub(lane));
        let master = rig.master();
        rig.advance_to(rig.start_frame() + master * 13 / 10);
        rig.press(Command::RecDub(lane));
        rig.idle();
        assert_eq!(rig.state(lane as usize), LaneState::Playing);
    }
    rig.set_level(0.0);
    rig.press(Command::Reverse(1));
    rig.press(Command::PlayStop(2));
    rig.idle();
    rig
}

/// A load of `s`'s loops, built as the host builds one: each lane's buffer the engine's capacity long, a
/// reversed lane's play-order PCM reversed back, its peaks computed.
fn load_of(s: &Snapshot, capacity: usize, bars: Frame) -> Load {
    let master = s.master as usize;
    let tracks = s.tracks[..s.count]
        .iter()
        .flatten()
        .enumerate()
        .map(|(k, t)| {
            let mut buf = Vec::with_capacity(capacity);
            buf.resize(capacity, 0.0f32);
            let pcm = &s.pcm[k * master..(k + 1) * master];
            buf[..master].copy_from_slice(pcm);
            if t.reversed {
                buf[..master].reverse();
            }
            let peaks = buf[..master].chunks(PEAK_FRAMES).map(|c| c.iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)))).collect();
            LoadTrack { index: t.index, buf, peaks, reversed: t.reversed, playing: t.state == LaneState::Playing }
        })
        .collect();
    Load { bpm: s.bpm, bars, master: s.master, tracks, result: None }
}

#[test]
fn a_snapshot_loads_back_sample_exact_on_a_fresh_grid() {
    let mut rig = three_lanes();
    let master = rig.master();
    let loops: Vec<Vec<f32>> = (0..3).map(|i| rig.pcm(i)).collect();
    let s = snapshot(&mut rig, 5 * master as usize);
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!((s.rate, s.master, s.bpm, s.count), (48_000, master, 120, 3));
    let tracks: Vec<_> = s.tracks.iter().flatten().map(|t| (t.index, t.state, t.reversed)).collect();
    assert_eq!(tracks, [(0, LaneState::Playing, false), (1, LaneState::Playing, true), (2, LaneState::Stopped, false)]);
    for (k, want) in loops.iter().enumerate() {
        assert_eq!(&s.pcm[k * master as usize..(k + 1) * master as usize], &want[..], "lane {k} in play order");
    }

    let mut fresh = Rig::new();
    fresh.advance(4800);
    let bars = master / frames_per_bar(120.0, 48_000);
    let capacity = fresh.engine.looper().capacity() as usize;
    let SessionJob::Load(load) = run(&mut fresh, SessionJob::Load(load_of(&s, capacity, bars))) else { unreachable!() };
    assert_eq!(load.result, Some(Ok(())));
    assert!(load.tracks.iter().all(|t| t.buf.len() == capacity), "the engine's old buffers came back to be freed");
    assert_eq!((fresh.master(), fresh.bpm(), fresh.locked()), (master, 120, true));
    let anchor = fresh.anchor();
    assert!(anchor > 4800 && anchor <= fresh.frame, "the grid anchors on the frame the load applied");
    for (i, want) in loops.iter().enumerate() {
        assert_eq!(&fresh.pcm(i), want, "lane {i} plays back as it was");
    }
    assert_eq!((fresh.state(0), fresh.state(1), fresh.state(2)), (LaneState::Playing, LaneState::Playing, LaneState::Stopped));
    assert!(fresh.lane(1).reversed);
    let again = snapshot(&mut fresh, 5 * master as usize);
    assert_eq!(again.pcm[..3 * master as usize], s.pcm[..3 * master as usize], "snapshot, load, snapshot: the same");

    // The loaded lanes play from loop position 0 on the anchor: lane 0 alone is heard (lane 1 muted and
    // its gain ramped out, no click).
    fresh.press(Command::SetMute(1, true));
    fresh.press(Command::SetMetronome(false));
    fresh.advance(fresh.seconds(0.5));
    fresh.keep_output();
    fresh.advance(master);
    let (start, out) = fresh.output.as_ref().unwrap();
    for (j, &x) in out.iter().enumerate().step_by(997) {
        let pos = (start + j as Frame - anchor).rem_euclid(master) as usize;
        assert!((x - loops[0][pos]).abs() < 1e-6, "frame {}: {x} against {}", start + j as Frame, loops[0][pos]);
    }
}

#[test]
fn an_overdubbing_lane_gives_its_loop_before_the_layer() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    let before = rig.pcm(0);
    rig.set_level(0.125);
    rig.press(Command::RecDub(0));
    rig.advance(master / 2);
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Overdubbing);
    let s = snapshot(&mut rig, master as usize);
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!(s.tracks[0].map(|t| t.state), Some(LaneState::Overdubbing));
    assert_eq!(&s.pcm[..master as usize], &before[..], "the committed loop, not the layer in flight");
}

#[test]
fn a_snapshot_whose_loop_is_written_meanwhile_says_so_and_a_short_destination_asks_for_more() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(60.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    assert!(master as usize > 40 * rig.block * SNAPSHOT_RATE as usize / 64, "long enough to copy over many blocks");

    assert_eq!(snapshot(&mut rig, 10).result, Some(Err(SessionError::TooSmall(master as usize))));

    let mut pcm = Vec::with_capacity(master as usize);
    pcm.resize(master as usize, 0.0f32);
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(pcm)))).is_ok());
    rig.advance(rig.block as Frame);
    rig.set_level(0.5);
    rig.press(Command::RecDub(0));
    let mut back = None;
    for _ in 0..10_000 {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            back = Some(job);
            break;
        }
    }
    let Some(SessionJob::Snapshot(s)) = back.map(|b| *b) else { panic!("no snapshot came back") };
    assert_eq!(s.result, Some(Err(SessionError::Changed)), "an overdub summed into the loop it copied");
}

#[test]
fn a_load_needs_an_empty_engine_and_a_session_that_fits_it() {
    let mut rig = three_lanes();
    let master = rig.master();
    let s = snapshot(&mut rig, 5 * master as usize);
    let capacity = rig.engine.looper().capacity() as usize;
    let SessionJob::Load(load) = run(&mut rig, SessionJob::Load(load_of(&s, capacity, 1))) else { unreachable!() };
    assert_eq!(load.result, Some(Err(SessionError::NotEmpty)));
    assert_eq!(rig.pcm(0), s.pcm[..master as usize], "a refused load changes nothing");

    let mut fresh = Rig::new();
    let SessionJob::Load(load) = run(&mut fresh, SessionJob::Load(load_of(&s, capacity, 3))) else { unreachable!() };
    assert!(matches!(load.result, Some(Err(SessionError::Invalid(_)))), "bars that disagree with the tempo and length");
    assert_eq!(fresh.master(), 0);
    let SessionJob::Load(load) = run(&mut fresh, SessionJob::Load(load_of(&s, capacity - 1, 1))) else { unreachable!() };
    assert!(matches!(load.result, Some(Err(SessionError::Invalid(_)))), "a buffer that is not the engine's capacity");
}

/// Five lanes of one eight-bar loop at 120 BPM (768000 frames each): lane 0 the frame code with its
/// DELAY on, lanes 1 to 4 its copies, lane 1 reversed and lane 2 stopped; the click on.
fn five_long_lanes() -> Rig {
    let mut rig = Rig::new();
    rig.set_input(code);
    rig.record_first_take(0, 8, 2400);
    rig.set_level(0.0);
    rig.idle();
    for _ in 1..5 {
        rig.press(Command::Copy(0));
        rig.idle();
    }
    rig.press(Command::Reverse(1));
    rig.press(Command::PlayStop(2));
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.set(Command::SetMetronome(true));
    rig.idle();
    rig
}

#[test]
fn the_output_is_bit_identical_while_a_snapshot_copies_out() {
    // verify/probes/recovery-playback.mjs: a save never disturbs playback. The same session twice, one
    // with a snapshot copying out over ~30 blocks and a lane resumed while it copies.
    let (mut with, mut without) = (five_long_lanes(), five_long_lanes());
    let master = with.master();
    let mut pcm = Vec::with_capacity(5 * master as usize);
    pcm.resize(5 * master as usize, 0.0f32);
    with.keep_output();
    without.keep_output();
    assert!(with.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(pcm)))).is_ok());
    let copy = 5 * master / SNAPSHOT_RATE;
    assert!(copy > 20 * with.block as Frame, "the copy spans many blocks: {copy} frames");
    for rig in [&mut with, &mut without] {
        rig.advance(copy / 2);
    }
    assert!(with.session().returned().is_none(), "the snapshot is still copying");
    for rig in [&mut with, &mut without] {
        rig.press(Command::PlayStop(2));
        rig.advance(copy);
    }
    let Some(job) = with.session().returned() else { panic!("the snapshot never came back") };
    let SessionJob::Snapshot(s) = *job else { unreachable!() };
    assert_eq!((s.result, s.count), (Some(Ok(())), 5));
    assert_eq!(with.state(2), LaneState::Playing);
    let (looper_with, looper_without) = (&with.output.as_ref().unwrap().1, &without.output.as_ref().unwrap().1);
    for (tap, a, b) in [("left", &with.heard, &without.heard), ("right", &with.heard_right, &without.heard_right), ("looper", looper_with, looper_without)] {
        assert_eq!(a.len(), b.len());
        let first = a.iter().zip(b).position(|(x, y)| x.to_bits() != y.to_bits());
        assert_eq!(first, None, "the {tap} output differs while the snapshot copies");
    }
    assert!(with.heard.iter().any(|&x| x != 0.0));
}

#[test]
fn a_later_take_armed_and_aborted_on_a_loaded_session_leaves_its_grid_and_lanes() {
    // golden-jam.mjs's stop arm, on a load rather than a recorded master.
    let mut rig = three_lanes();
    let master = rig.master();
    let s = snapshot(&mut rig, 5 * master as usize);
    let capacity = rig.engine.looper().capacity() as usize;
    let bars = master / frames_per_bar(120.0, 48_000);
    let loaded = |s: &Snapshot| {
        let mut fresh = Rig::new();
        fresh.advance(4800);
        let SessionJob::Load(load) = run(&mut fresh, SessionJob::Load(load_of(s, capacity, bars))) else { unreachable!() };
        assert_eq!(load.result, Some(Ok(())));
        fresh
    };
    // A later take armed on lane 3 and stopped before its boundary.
    let mut fresh = loaded(&s);
    let (anchor, lanes) = (fresh.anchor(), (0..3).map(|i| (fresh.lane(i), fresh.pcm(i))).collect::<Vec<_>>());
    fresh.press(Command::RecDub(3));
    assert!(fresh.lane(3).armed, "armed for the next boundary");
    fresh.press(Command::Stop(3));
    fresh.advance(master);
    assert_eq!(fresh.state(3), LaneState::Empty);
    assert_eq!((fresh.master(), fresh.anchor(), fresh.locked()), (master, anchor, true), "the loaded grid is untouched");
    for (i, (info, pcm)) in lanes.iter().enumerate() {
        assert!(fresh.lane(i) == *info && fresh.pcm(i) == *pcm, "loaded lane {i} is untouched");
    }
    // The probe's own sequence: lane 1 left the only loop, a later take armed on lane 0, lane 1 cleared
    // while the arm is live: the loaded grid holds; the arm's STOP then leaves a blank session.
    let mut fresh = loaded(&s);
    let anchor = fresh.anchor();
    fresh.press(Command::Clear(0));
    fresh.press(Command::Clear(2));
    fresh.press(Command::RecDub(0));
    assert!(fresh.lane(0).armed);
    fresh.press(Command::Clear(1));
    assert_eq!((fresh.master(), fresh.anchor(), fresh.locked()), (master, anchor, true), "the loaded grid survives while the arm is live");
    fresh.press(Command::Stop(0));
    assert!(fresh.master() == 0 && !fresh.locked() && (0..5).all(|i| fresh.state(i) == LaneState::Empty), "the aborted arm leaves a blank session");
}
