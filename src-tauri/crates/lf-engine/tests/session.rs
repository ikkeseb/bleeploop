//! Saving and loading a session through the engine (`lf_engine::session`): a snapshot of the committed
//! loops in play order, copied a budget per rendered frame, and a load into an empty engine that plays
//! them back sample-exact on a fresh grid, each lane at its loaded mix from its first sample, over an FX
//! chain that holds nothing from before the load (nor, after a CLEAR, from before it). Both run after
//! the commands due on their block's first frame, with no device running too. Every `process` runs under the rig's `assert_no_alloc`, so the
//! engine side of both allocates nothing; the buffers are the test's (the host's), built outside it.
//! A snapshot in flight leaves the rendered output bit for bit as it was (the Web Audio probe
//! recovery-playback.mjs's continuity). Each track carries its lane's mix as the engine applied it at the
//! pin (STATUS D21), and the export's wet master renders with that mix, not the host's settings.

mod common;

use common::{code, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam, FxState, MAX_FEEDBACK};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::render::wet_master;
use lf_engine::session::SNAPSHOT_RATE;
use lf_engine::{Command, LaneMix, LaneState, Load, LoadTrack, SessionError, SessionJob, Snapshot};

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
            LoadTrack { index: t.index, buf, peaks, reversed: t.reversed, playing: t.state == LaneState::Playing, mix: t.mix }
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

/// Lane 0's and lane 1's mix moved from the defaults: volume, mute, DUB FEEDBACK and FX.
fn mix_lanes(rig: &mut Rig) -> [LaneMix; 2] {
    for command in [
        Command::SetVolume(0, 0.5),
        Command::SetMute(0, true),
        Command::SetDubFeedback(0, 0.25),
        Command::SetFxParam(0, FxParam::Cutoff, 800.0),
        Command::SetFxBypass(0, FxKind::Filter, false),
        Command::SetFxParam(0, FxParam::Feedback, 0.6),
        Command::SetFxBypass(0, FxKind::Delay, false),
        Command::SetVolume(1, 1.25),
        Command::SetFxParam(1, FxParam::Semitones, -5.0),
        Command::SetFxBypass(1, FxKind::Pitch, false),
        Command::SetFxParam(1, FxParam::Amount, 0.75),
        Command::SetFxBypass(1, FxKind::Reverb, false),
    ] {
        rig.set(command);
    }
    let mut zero = LaneMix { volume: 0.5, muted: true, dub_feedback: 0.25, ..LaneMix::default() };
    zero.fx[FxKind::Filter.index()] = FxState { bypassed: false, params: [800.0, 2.0, 0.0] };
    zero.fx[FxKind::Delay.index()] = FxState { bypassed: false, params: [1.0, 0.6, 0.3] };
    let mut one = LaneMix { volume: 1.25, ..LaneMix::default() };
    one.fx[FxKind::Pitch.index()] = FxState { bypassed: false, params: [-5.0, 0.0, 0.0] };
    one.fx[FxKind::Reverb.index()] = FxState { bypassed: false, params: [0.75, 0.0, 0.0] };
    [zero, one]
}

/// The mix each committed lane of `s` carries, by lane.
fn mixes(s: &Snapshot) -> Vec<(u8, LaneMix)> {
    s.tracks.iter().flatten().map(|t| (t.index, t.mix)).collect()
}

#[test]
fn a_snapshot_carries_each_lanes_mix_as_the_engine_applied_it() {
    let mut rig = three_lanes();
    let master = rig.master();
    let [zero, one] = mix_lanes(&mut rig);
    let s = snapshot(&mut rig, 5 * master as usize);
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!(mixes(&s), [(0, zero), (1, one), (2, LaneMix::default())], "volume, mute, DUB FEEDBACK and FX, lane by lane");
    for (i, mix) in mixes(&s) {
        assert_eq!(rig.engine.looper().mix(i as usize, rig.engine.fx()), mix, "lane {i}: the engine's applied mix");
    }
}

#[test]
fn an_integer_fx_param_set_fractional_reaches_the_snapshot_whole() {
    // session.json's schema (`validateFxStates`) refuses a fractional semitone or division index, so the
    // engine rounds them as the UI does (half up), within their ranges.
    let mut rig = three_lanes();
    let master = rig.master();
    for command in [
        Command::SetFxParam(0, FxParam::Semitones, 0.5),
        Command::SetFxParam(0, FxParam::Rate, 2.4),
        Command::SetFxParam(0, FxParam::Time, 2.6),
        Command::SetFxParam(1, FxParam::Semitones, -2.5),
        Command::SetFxParam(1, FxParam::Time, 3.7),
        Command::SetFxParam(1, FxParam::Q, 0.55),
    ] {
        rig.set(command);
    }
    let s = snapshot(&mut rig, 5 * master as usize);
    assert_eq!(s.result, Some(Ok(())));
    let fx = |lane: usize, kind: FxKind| s.tracks.iter().flatten().find(|t| t.index as usize == lane).unwrap().mix.fx[kind.index()].params;
    assert_eq!(fx(0, FxKind::Pitch)[0], 1.0);
    assert_eq!(fx(0, FxKind::Stutter)[0], 2.0);
    assert_eq!(fx(0, FxKind::Delay)[0], 3.0);
    assert_eq!(fx(1, FxKind::Pitch)[0], -2.0, "half up, as Math.round");
    assert_eq!(fx(1, FxKind::Delay)[0], 3.0, "rounded, then within 0..3");
    assert_eq!(fx(1, FxKind::Filter)[1], 0.55, "a continuous param keeps its fraction");
}

#[test]
fn a_mix_moved_while_a_snapshot_copies_leaves_the_mix_at_its_pin() {
    // A loop long enough to copy over many blocks, its mix set before the snapshot pins it.
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(60.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 2, 2400);
    rig.press(Command::Copy(0));
    rig.set_level(0.0);
    rig.idle();
    assert_eq!(rig.state(1), LaneState::Playing);
    assert!(2 * master > 4 * rig.block as Frame * SNAPSHOT_RATE, "the copy spans several blocks");
    let [zero, one] = mix_lanes(&mut rig);
    let mut pcm = Vec::with_capacity(2 * master as usize);
    pcm.resize(2 * master as usize, 0.0f32);
    assert!(rig.session().send(Box::new(SessionJob::Snapshot(Snapshot::new(pcm)))).is_ok());
    // The next block start pins the loops and takes their mix; then the faders, the mutes and the FX move.
    rig.advance(rig.block as Frame);
    for command in [
        Command::SetVolume(0, 0.1),
        Command::SetMute(0, false),
        Command::SetFxBypass(0, FxKind::Filter, true),
        Command::SetFxParam(0, FxParam::Cutoff, 5000.0),
        Command::SetMute(1, true),
        Command::SetVolume(1, 0.2),
        Command::SetFxParam(1, FxParam::Amount, 0.1),
    ] {
        rig.set(command);
    }
    assert!(rig.session().returned().is_none(), "the snapshot is still copying when the mix moves");
    let s = loop {
        rig.advance(rig.block as Frame);
        if let Some(job) = rig.session().returned() {
            let SessionJob::Snapshot(s) = *job else { unreachable!() };
            break s;
        }
    };
    assert_eq!(s.result, Some(Ok(())), "a mix change writes no loop");
    assert_eq!(mixes(&s), [(0, zero), (1, one)], "the mix at the pin, not at the snapshot's end");
    let now = rig.engine.looper().mix(0, rig.engine.fx());
    assert!(now.volume == 0.1 && !now.muted && now.fx[FxKind::Filter.index()].bypassed, "the engine took the moves: {now:?}");
}

/// The wet master of `s` with the snapshot's own mix, the host's `settings` beside it.
fn master_of(s: &Snapshot, capacity: usize, bars: Frame, settings: &[Command]) -> Vec<f32> {
    let mixes: Vec<LaneMix> = s.tracks.iter().flatten().map(|t| t.mix).collect();
    wet_master(s.rate, load_of(s, capacity, bars), &mixes, settings).expect("the render").left
}

#[test]
fn a_wet_master_rendered_from_a_snapshot_takes_the_snapshots_mix_not_the_settings() {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(|f| 0.1 * (f as f32 * 0.01).sin());
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    let capacity = rig.engine.looper().capacity() as usize;
    let bars = master / frames_per_bar(120.0, 48_000);
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    // The settings say the lane plays at unity: the snapshot muted it, so the master is silent.
    rig.set(Command::SetMute(0, true));
    let muted = snapshot(&mut rig, master as usize);
    let silent = peak(&master_of(&muted, capacity, bars, &[Command::SetMute(0, false), Command::SetVolume(0, 1.0)]));
    assert!(silent < 1e-6, "a lane muted at the pin is out of the master ({silent})");
    // And the reverse: the snapshot plays it at half, whatever the settings say.
    rig.set(Command::SetMute(0, false));
    let unity = peak(&master_of(&snapshot(&mut rig, master as usize), capacity, bars, &[]));
    rig.set(Command::SetVolume(0, 0.5));
    let half = snapshot(&mut rig, master as usize);
    let heard = peak(&master_of(&half, capacity, bars, &[Command::SetMute(0, true), Command::SetVolume(0, 1.0)]));
    assert!(unity > 0.05, "the lane is in the master ({unity})");
    assert!((heard / unity - 0.5).abs() < 0.01, "the lane at the snapshot's half volume: {heard} against {unity}");
}

// ── A load carries each lane's mix; a block's due commands apply before its session job ─────────────

/// A one-bar loop on lane 0 at 120 BPM, snapshotted at the defaults: the snapshot, and the loop as it
/// plays.
fn one_lane() -> (Snapshot, Vec<f32>) {
    let mut rig = Rig::new();
    rig.set(Command::SetBpm(120.0));
    rig.set_input(code);
    let master = rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.idle();
    let s = snapshot(&mut rig, master as usize);
    assert_eq!((s.result, s.count), (Some(Ok(())), 1));
    (s, rig.pcm(0))
}

/// A fresh engine with the click off, and a load of `s` with lane 0 at `mix`; nothing sent yet.
fn fresh_load(s: &Snapshot, mix: LaneMix) -> (Rig, Load) {
    let mut fresh = Rig::new();
    fresh.set(Command::SetMetronome(false));
    fresh.advance(4800);
    let capacity = fresh.engine.looper().capacity() as usize;
    let mut load = load_of(s, capacity, s.master / frames_per_bar(120.0, 48_000));
    load.tracks[0].mix = mix;
    (fresh, load)
}

/// Hand `load` to the engine and render one block: the load's result.
fn load_now(rig: &mut Rig, load: Load) -> Option<Result<(), SessionError>> {
    assert!(rig.session().send(Box::new(SessionJob::Load(load))).is_ok());
    rig.advance(rig.block as Frame);
    match rig.session().returned().map(|job| *job) {
        Some(SessionJob::Load(load)) => load.result,
        _ => None,
    }
}

#[test]
fn a_loaded_lane_plays_at_its_loaded_mix_from_its_first_frame() {
    // The EMPTY lane it loads into is at the defaults (unmuted, unity): no frame plays at that level.
    let (s, pcm) = one_lane();
    let master = s.master as usize;
    for (what, mix, gain) in [("muted", LaneMix { muted: true, ..LaneMix::default() }, 0.0f32), ("at 0.25", LaneMix { volume: 0.25, ..LaneMix::default() }, 0.25)] {
        let (mut fresh, load) = fresh_load(&s, mix);
        fresh.keep_output();
        assert_eq!(load_now(&mut fresh, load), Some(Ok(())));
        fresh.advance(master as Frame);
        let (start, out) = fresh.output.as_ref().unwrap();
        assert_eq!(fresh.anchor(), *start, "the load applied on the first rendered frame");
        let first = out.iter().enumerate().position(|(j, &x)| x != gain * pcm[j % master]);
        assert_eq!(first, None, "a lane loaded {what} plays at it from its first sample");
        assert!(out.iter().any(|&x| x != 0.0) == (gain != 0.0));
    }
}

#[test]
fn a_loaded_lane_takes_the_loads_mix_over_one_queued_before_it() {
    let (s, _) = one_lane();
    let mut mix = LaneMix { volume: 0.5, muted: true, dub_feedback: 0.25, ..LaneMix::default() };
    mix.fx[FxKind::Filter.index()] = FxState { bypassed: false, params: [20_000.0, 4.5, 0.0] };
    mix.fx[FxKind::Pitch.index()] = FxState { bypassed: false, params: [-5.4, 0.0, 0.0] };
    mix.fx[FxKind::Delay.index()] = FxState { bypassed: false, params: [2.0, 0.6, 0.75] };
    let mut applied = mix;
    applied.fx[FxKind::Filter.index()].params[0] = 14_000.0;
    applied.fx[FxKind::Pitch.index()].params[0] = -5.0;
    let (mut fresh, load) = fresh_load(&s, mix);
    // Sent to the EMPTY lane in the load's block, ahead of it: the load's mix wins.
    let at = fresh.frame;
    for command in [Command::SetVolume(0, 0.9), Command::SetMute(0, false), Command::SetDubFeedback(0, 1.0), Command::SetFxBypass(0, FxKind::Reverb, false), Command::SetFxParam(0, FxParam::Cutoff, 500.0)] {
        fresh.send_at(at, command);
    }
    assert_eq!(load_now(&mut fresh, load), Some(Ok(())));
    assert_eq!(fresh.engine.looper().mix(0, fresh.engine.fx()), applied, "volume, mute, DUB FEEDBACK and the FX targets, clamped as a command's");
}

#[test]
fn a_take_queued_before_a_load_in_its_block_still_refuses_it() {
    let (s, _) = one_lane();
    let (mut fresh, load) = fresh_load(&s, LaneMix::default());
    fresh.send_at(fresh.frame, Command::RecDub(0));
    assert_eq!(load_now(&mut fresh, load), Some(Err(SessionError::NotEmpty)));
    assert!(fresh.master() == 0 && fresh.state(0) != LaneState::Playing, "nothing loaded: {:?}", fresh.lane(0));
}

#[test]
fn a_setting_queued_before_a_snapshot_in_its_block_is_in_it() {
    let mut rig = three_lanes();
    let master = rig.master();
    // Due on the block's first frame, with the request: in. Due later in that block: after the pin.
    rig.send_at(rig.frame, Command::SetMute(0, true));
    rig.send_at(rig.frame + 5, Command::SetVolume(1, 0.5));
    let s = snapshot(&mut rig, 5 * master as usize);
    assert_eq!(s.result, Some(Ok(())));
    let mix = mixes(&s);
    assert!(mix[0].1.muted, "the mute sent before the snapshot is in it");
    assert_eq!(mix[1].1.volume, 1.0, "a fader due after the pin is not");
    assert_eq!(rig.engine.looper().mix(1, rig.engine.fx()).volume, 0.5);
}

// ── A load and a CLEAR silence the lane's FX history ────────────────────────────────────────────────

/// A delay on with its top feedback and full mix: what the hot jam below and the loads after it set.
fn hot_delay() -> LaneMix {
    let mut mix = LaneMix::default();
    mix.fx[FxKind::Delay.index()] = FxState { bypassed: false, params: [1.0, MAX_FEEDBACK, 1.0] };
    mix
}

/// A one-bar noise loop on lane 0 played a second through [`hot_delay`], its output kept from there,
/// then CLEARed and, 100 frames on (inside the delay's 20 ms ramp down), `s`'s bar loaded with lane 0
/// holding `pcm` at loop position 0 (silence after) through [`hot_delay`]. The rig, the CLEAR's and the
/// load's frames in its kept output.
fn hot_clear_then_load(s: &Snapshot, pcm: f32) -> (Rig, usize, usize) {
    let mut rig = Rig::new();
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SetBpm(120.0));
    rig.set_input(|f| ((f * 37 % 101) as f32 / 50.0 - 1.0) * 0.5);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.set(Command::SetFxBypass(0, FxKind::Delay, false));
    rig.set(Command::SetFxParam(0, FxParam::Feedback, MAX_FEEDBACK));
    rig.set(Command::SetFxParam(0, FxParam::Mix, 1.0));
    rig.advance(48_000);
    rig.keep_output();
    let kept = rig.frame;
    rig.advance(48_000);
    let clear = (rig.frame - kept) as usize;
    rig.press(Command::Clear(0));
    assert_eq!(rig.state(0), LaneState::Empty);
    rig.advance(99);
    let capacity = rig.engine.looper().capacity() as usize;
    let mut load = load_of(s, capacity, s.master / frames_per_bar(120.0, 48_000));
    let track = &mut load.tracks[0];
    track.buf.fill(0.0);
    track.buf[0] = pcm;
    track.peaks.fill((pcm.min(0.0), pcm.max(0.0)));
    track.mix = hot_delay();
    assert_eq!(load_now(&mut rig, load), Some(Ok(())));
    let loaded = (rig.anchor() - kept) as usize;
    rig.advance(48_000);
    (rig, clear, loaded)
}

#[test]
fn a_clear_and_a_load_bring_back_nothing_the_lanes_delay_heard_before() {
    let (s, _) = one_lane();
    let (rig, clear, loaded) = hot_clear_then_load(&s, 0.0);
    assert!(clear < loaded);
    let hot = rig.bus[..clear].iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(hot > 0.1, "the delay rings before the CLEAR ({hot})");
    // After the lane's FX output (the bus, before the limiter): silent from the CLEAR on, through a load
    // whose delay is on at its top feedback over a silent loop.
    for (side, bus) in [("left", &rig.bus), ("right", &rig.bus_right)] {
        let first = bus[clear..].iter().position(|&x| x != 0.0).map(|k| clear + k);
        assert_eq!(first, None, "{side}: nothing from before the CLEAR (the load at {loaded})");
    }
}

#[test]
fn a_delay_loaded_after_a_clear_echoes_its_own_loop_as_a_new_one_does() {
    let (s, _) = one_lane();
    let (rig, _, loaded) = hot_clear_then_load(&s, 0.5);
    let bus = &rig.bus[loaded..];
    // The impulse on the load's frame, its echoes a 1/8 apart at 120 BPM (12 000 frames) at full mix, the
    // feedback a quantum late: 0.95 of the one before. (The dry impulse itself is mostly gone: the mix
    // ramps on from where the CLEAR's ramp down had got to.)
    assert!((bus[12_000] / 0.5 - 1.0).abs() < 1e-2, "the first echo ({})", bus[12_000]);
    assert!((bus[24_128] / bus[12_000] - MAX_FEEDBACK as f32).abs() < 1e-2, "the second echo ({})", bus[24_128]);
}

// ── With no device running, a session job should come after the commands queued before it ────────

/// The device stops after `fresh_load`'s first frames; `queue` reaches the ring, then `load` the port,
/// and the host services the engine idle (`Engine::service_session_idle`). The load's result.
fn load_idle(rig: &mut Rig, queue: &[Command], load: Load) -> Option<Result<(), SessionError>> {
    rig.punch_out();
    for &command in queue {
        rig.queue(command);
    }
    assert!(rig.session().send(Box::new(SessionJob::Load(load))).is_ok());
    rig.engine.service_session_idle();
    match rig.session().returned().map(|job| *job) {
        Some(SessionJob::Load(load)) => load.result,
        _ => None,
    }
}

#[test]
#[ignore = "red: with no device running, a session job is served before the commands queued ahead of it, open thread (lf-engine briefing)"]
fn with_no_device_a_load_takes_its_mix_over_a_setting_queued_before_it() {
    let (s, _) = one_lane();
    let mix = LaneMix { volume: 0.5, muted: true, ..LaneMix::default() };
    let (mut fresh, load) = fresh_load(&s, mix);
    let later = fresh.frame + 9600;
    fresh.send_at(later, Command::SetVolume(0, 0.75));
    let queued = [Command::SetMute(0, false), Command::SetVolume(0, 0.9)];
    assert_eq!(load_idle(&mut fresh, &queued, load), Some(Ok(())));
    // The device resumes: the load's mix holds; a setting stamped later still lands on its frame.
    fresh.advance(4800);
    assert_eq!(fresh.engine.looper().mix(0, fresh.engine.fx()), mix);
    fresh.advance_to(later + 1);
    assert_eq!(fresh.engine.looper().mix(0, fresh.engine.fx()), LaneMix { volume: 0.75, ..mix });
}

#[test]
#[ignore = "red: with no device running, a session job is served before the commands queued ahead of it, open thread (lf-engine briefing)"]
fn with_no_device_a_take_queued_before_a_load_refuses_it() {
    let (s, _) = one_lane();
    let (mut fresh, load) = fresh_load(&s, LaneMix::default());
    assert_eq!(load_idle(&mut fresh, &[Command::RecDub(0)], load), Some(Err(SessionError::NotEmpty)));
    fresh.advance(4800);
    assert!(fresh.master() == 0 && fresh.state(0) != LaneState::Playing, "nothing loaded: {:?}", fresh.lane(0));
}
