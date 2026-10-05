//! A lane's pan (the engine's own, no Web Audio counterpart): the law ([`pan_gains`]), applied to the
//! lane's direct output after its FX while the shared reverb send stays unpanned; a `SetPan` glides
//! frame by frame and lands exactly, the same at any block size; construction, CLEAR, COPY and a load
//! set the position with no glide; the feed, a snapshot and the engine's applied mixes carry the target,
//! once per change; and the export's wet master renders it. The centre's bits: `effects.rs`'s unit test.
//! The loops, their PCM and the looper's own tap (the lanes before their FX) never see the pan.

mod common;

use std::f64::consts::{FRAC_PI_8, SQRT_2};

use common::{code, Opts, Rig};
use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::effects::{pan_gains, PAN_TAU_SECONDS};
use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::render::wet_master;
use lf_engine::{Command, CompactMix, Event, LaneMix, LaneState, Load, LoadTrack, SessionJob, Snapshot};

const LEVEL: f32 = 0.5;

/// Lane 0 plays a constant LEVEL loop of one bar, the input silent from then on, rendered in `block`s.
fn playing(block: usize) -> Rig {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.set_level(LEVEL);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.advance(4800);
    rig
}

/// The pans the feed heard for `lane` since event `mark`.
fn pans(rig: &Rig, mark: usize, lane: u8) -> Vec<f32> {
    rig.events[mark..]
        .iter()
        .filter_map(|e| match *e {
            Event::Mix { lane: l, mix, .. } if l == lane => Some(mix.pan),
            _ => None,
        })
        .collect()
}

#[test]
fn the_law_is_constant_power_from_the_centre_with_exact_ends() {
    assert_eq!(pan_gains(0.0), [1.0, 1.0], "the centre is unity on both sides, exactly");
    assert_eq!(pan_gains(-1.0), [SQRT_2, 0.0], "hard left: +3 dB and silence, exactly");
    assert_eq!(pan_gains(1.0), [0.0, SQRT_2], "hard right");
    let close = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12;
    let (c, s) = (FRAC_PI_8.cos(), FRAC_PI_8.sin());
    assert!(close(pan_gains(-0.5), [SQRT_2 * c, SQRT_2 * s]), "{:?}", pan_gains(-0.5));
    assert!(close(pan_gains(0.5), [SQRT_2 * s, SQRT_2 * c]), "{:?}", pan_gains(0.5));
    let mut last = pan_gains(-1.0);
    for k in -1000..=1000 {
        let g = pan_gains(k as f64 / 1000.0);
        assert!((g[0] * g[0] + g[1] * g[1] - 2.0).abs() < 1e-12, "constant summed power at {k}: {g:?}");
        assert!(g[0] <= last[0] && g[1] >= last[1], "left falls and right rises toward the right: {k}");
        last = g;
    }
    assert_eq!((pan_gains(3.0), pan_gains(-3.0)), (pan_gains(1.0), pan_gains(-1.0)), "past an end is that end");
}

#[test]
fn a_settled_pan_scales_the_lanes_direct_output_by_the_law_and_leaves_the_loop_alone() {
    for p in [-1.0f32, -0.5, 0.5, 1.0] {
        let (mut centre, mut panned) = (playing(128), playing(128));
        centre.press(Command::SetPan(0, 0.0));
        panned.press(Command::SetPan(0, p));
        for rig in [&mut centre, &mut panned] {
            rig.advance(rig.seconds(0.5));
            rig.keep_output();
            rig.advance(rig.seconds(1.0));
        }
        assert_eq!(panned.engine.fx().pan_position(0), p as f64, "settled on its target");
        assert_eq!(centre.bus, centre.bus_right, "the centre plays both sides alike");
        assert!(centre.bus.iter().any(|x| x.abs() > 0.4), "the loop plays");
        let [gl, gr] = pan_gains(p as f64);
        for (k, &x) in centre.bus.iter().enumerate() {
            assert_eq!(panned.bus[k], (gl * x as f64) as f32, "pan {p}: left at frame {k}");
            assert_eq!(panned.bus_right[k], (gr * x as f64) as f32, "pan {p}: right at frame {k}");
        }
        if p.abs() == 1.0 {
            let silent = if p < 0.0 { &panned.bus_right } else { &panned.bus };
            assert!(silent.iter().all(|&x| x == 0.0), "pan {p}: the far side is exactly silent");
        }
        assert_eq!(panned.pcm(0), centre.pcm(0), "pan {p}: the loop as recorded");
        assert_eq!(panned.output, centre.output, "pan {p}: the looper's tap (the lanes before their FX)");
    }
}

#[test]
fn a_move_glides_without_a_step_lands_exactly_and_is_the_same_at_every_block_size() {
    let coef = (-1.0 / (PAN_TAU_SECONDS * 48_000.0)).exp();
    // The steepest a gain moves per unit of pan (sqrt(2) pi / 4) times the largest position step of a
    // move across the whole range (2, from hard right to hard left).
    let step_bound = (SQRT_2 * std::f64::consts::FRAC_PI_4 * 2.0 * (1.0 - coef) * LEVEL as f64 * 1.01) as f32;
    let mut reference: Option<(Vec<f32>, Vec<f32>)> = None;
    for block in [1, 32, 64, 128, 480, 4096] {
        let mut rig = playing(block);
        let start = rig.frame;
        rig.keep_output();
        rig.press(Command::SetPan(0, 1.0));
        rig.advance(rig.seconds(0.4));
        assert_eq!(rig.engine.fx().pan_position(0), 1.0, "block {block}: the move landed exactly");
        let right_from = rig.frame;
        rig.press(Command::SetPan(0, -1.0));
        rig.advance(rig.seconds(0.4));
        assert_eq!(rig.engine.fx().pan_position(0), -1.0, "block {block}: and back");
        let (l, r) = (rig.bus.clone(), rig.bus_right.clone());
        for side in [&l, &r] {
            let worst = side.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            assert!(worst <= step_bound, "block {block}: a step of {worst} (the glide allows {step_bound})");
        }
        let settled = (right_from - start) as usize - 1;
        assert_eq!(l[settled], 0.0, "block {block}: hard right leaves the left exactly silent");
        assert!(r[settled] > 0.6, "block {block}: at +3 dB on the right ({})", r[settled]);
        assert_eq!(*r.last().unwrap(), 0.0, "block {block}: hard left leaves the right exactly silent");
        match &reference {
            None => reference = Some((l, r)),
            Some((rl, rr)) => assert!(*rl == l && *rr == r, "block {block}: the same bits as block 1"),
        }
    }
}

/// A lane with its delay and reverb send on plays the frame code, panned to `p` from the start.
fn wet_lane(p: f32) -> Rig {
    let mut rig = Rig::new();
    rig.set(Command::SetPan(0, p));
    for command in [
        Command::SetFxParam(0, FxParam::Feedback, 0.5),
        Command::SetFxParam(0, FxParam::Mix, 0.5),
        Command::SetFxBypass(0, FxKind::Delay, false),
        Command::SetFxParam(0, FxParam::Amount, 1.0),
        Command::SetFxBypass(0, FxKind::Reverb, false),
    ] {
        rig.set(command);
    }
    rig.set_input(code);
    rig.record_first_take(0, 1, 2400);
    rig.set_level(0.0);
    rig.advance(rig.seconds(1.0));
    rig.keep_output();
    rig.advance(rig.seconds(1.0));
    rig
}

#[test]
fn the_pan_follows_the_fx_and_leaves_the_shared_reverb_unpanned() {
    // Each side is `gain * direct + wet`, the direct output after the lane's FX (its delay's echoes
    // included) and the reverb bus's wet the same at every pan: hard right leaves the left the wet
    // alone, hard left the right.
    let (centre, left, right) = (wet_lane(0.0), wet_lane(-1.0), wet_lane(1.0));
    let (wet_l, wet_r) = (&right.bus, &left.bus_right);
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak(wet_l) > 0.01 && peak(wet_r) > 0.01, "the reverb sounds on both sides");
    let mut direct_peak = 0.0f32;
    let mut worst = 0.0f32;
    for k in 0..centre.bus.len() {
        // The direct output, read on each side of the centre against the wet alone.
        let (direct_l, direct_r) = (centre.bus[k] - wet_l[k], centre.bus_right[k] - wet_r[k]);
        direct_peak = direct_peak.max(direct_l.abs());
        let off = [
            direct_l - direct_r,
            left.bus[k] - (wet_l[k] + SQRT_2 as f32 * direct_l),
            right.bus_right[k] - (wet_r[k] + SQRT_2 as f32 * direct_r),
        ];
        worst = off.iter().fold(worst, |m, x| m.max(x.abs()));
    }
    assert!(direct_peak > 0.02, "the direct output plays ({direct_peak})");
    assert!(worst < 1e-5, "one direct output, scaled by the law, over the same wet at every pan ({worst})");
    assert_eq!(centre.pcm(0), left.pcm(0), "the recording does not hear the pan");
}

#[test]
fn copy_seeds_the_copys_pan_and_clear_centres_the_lane_with_no_glide() {
    let mut rig = playing(64);
    rig.press(Command::SetPan(0, -0.75));
    rig.advance(rig.seconds(0.3));
    let fx = |rig: &Rig, lane: usize| (rig.engine.fx().pan(lane), rig.engine.fx().pan_position(lane));
    assert_eq!(fx(&rig, 0), (-0.75, -0.75f32 as f64));
    let mark = rig.events.len();
    rig.keep_output();
    rig.press(Command::Copy(0));
    assert_eq!(fx(&rig, 1), (-0.75, -0.75f32 as f64), "the copy sits where its source's pan is, from the COPY's frame");
    rig.idle();
    assert_eq!(rig.state(1), LaneState::Playing, "the copy plays on once its PCM is in");
    rig.advance(rig.seconds(0.5));
    assert_eq!(pans(&rig, mark, 1), [-0.75], "one Mix for the copy, at the source's pan");
    // Both lanes play the same loop: a copy gliding in from the centre would move the sides' ratio.
    let [gl, gr] = pan_gains(-0.75f32 as f64);
    assert!(rig.bus.iter().any(|&l| l.abs() > 1.0), "the copy plays beside its source");
    for (k, (&l, &r)) in rig.bus.iter().zip(&rig.bus_right).enumerate() {
        assert!((r as f64 - gr / gl * l as f64).abs() < 1e-6, "frame {k} from the COPY: left {l}, right {r}");
    }

    let mark = rig.events.len();
    rig.press(Command::Clear(1));
    assert_eq!(fx(&rig, 1), (0.0, 0.0), "a CLEAR centres the lane at once");
    rig.advance(256);
    assert_eq!(pans(&rig, mark, 1), [0.0], "its Mix says so");
    assert_eq!(fx(&rig, 0), (-0.75, -0.75f32 as f64), "the source keeps its pan");
    let fresh = Rig::new();
    assert!((0..5).all(|lane| fx(&fresh, lane) == (0.0, 0.0)), "a new engine's lanes are centred");
}

#[test]
fn a_set_pan_is_clamped_and_a_value_that_is_no_number_centres_the_lane() {
    let mut rig = Rig::new();
    for (sent, kept) in [(7.0f32, 1.0f32), (-0.25, -0.25), (f32::NAN, 0.0), (-3.0, -1.0), (f32::NEG_INFINITY, 0.0), (-0.0, 0.0)] {
        rig.press(Command::SetPan(2, sent));
        let applied = rig.engine.looper().mix(2, rig.engine.fx()).pan;
        assert_eq!(applied.to_bits(), kept.to_bits(), "SetPan({sent}) keeps {kept}");
    }
    rig.press(Command::SetPan(9, 0.5));
    assert!((0..5).all(|lane| lane == 2 || rig.engine.fx().pan(lane) == 0.0), "a lane out of range sets nothing");
}

#[test]
fn the_feed_and_the_applied_mixes_carry_the_target_once_per_change_not_the_glide() {
    let mut rig = playing(32);
    // The click splits every block on its beats, and each split publishes.
    rig.set(Command::SetMetronome(true));
    rig.advance(4800);
    let mark = rig.events.len();
    rig.press(Command::SetPan(0, 0.5));
    let moving = rig.engine.fx().pan_position(0);
    assert!(moving > 0.0 && moving < 0.5, "a frame in, the pan is on its way ({moving})");
    let (_, applied) = rig.engine.applied_mixes().expect("the engine has run");
    assert_eq!(applied[0].pan, 0.5, "the engine's applied mix is the target");
    assert_eq!(rig.engine.looper().mix(0, rig.engine.fx()).pan, 0.5);
    rig.advance(rig.seconds(1.0));
    assert_eq!(pans(&rig, mark, 0), [0.5], "one Mix over the whole glide, at its target");
    let mark = rig.events.len();
    rig.press(Command::SetPan(0, 0.5));
    rig.advance(4800);
    assert!(pans(&rig, mark, 0).is_empty(), "the same pan again is no change");
}

/// A one-bar session at 120 BPM: lane 0 a constant LEVEL loop, PLAYING, at `mix`.
fn session(rig: &Rig, mix: LaneMix) -> Load {
    let master = frames_per_bar(120.0, 48_000);
    let mut buf = vec![0.0f32; rig.engine.looper().capacity() as usize];
    buf[..master as usize].fill(LEVEL);
    let track = LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix };
    Load { bpm: 120, bars: 1, master, tracks: vec![track], result: None }
}

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

#[test]
fn a_load_seeds_its_pan_and_a_snapshot_pins_it() {
    let mut rig = Rig::new();
    rig.advance(4800);
    rig.press(Command::SetPan(0, -1.0));
    rig.advance(4800);
    let mark = rig.events.len();
    rig.keep_output();
    let load = session(&rig, LaneMix { pan: 0.5, ..LaneMix::default() });
    let SessionJob::Load(load) = run(&mut rig, SessionJob::Load(load)) else { unreachable!() };
    assert_eq!(load.result, Some(Ok(())));
    assert_eq!(rig.engine.fx().pan_position(0), 0.5, "the load's pan over the EMPTY lane's, with no glide from it");
    assert_eq!(pans(&rig, mark, 0), [0.5], "the feed hears the loaded pan");
    // From the first loaded sample, both sides at the loaded pan's gains.
    let [gl, gr] = pan_gains(0.5);
    let start = rig.output.as_ref().unwrap().0;
    let first = (rig.anchor() - start) as usize;
    assert!(rig.bus[first] > 0.1, "the loop plays from the anchor");
    for k in first..rig.bus.len() {
        let x = rig.bus[k] as f64 / gl;
        assert!((rig.bus_right[k] as f64 - gr * x).abs() < 1e-6, "frame {k} after the anchor: right {} for left {}", rig.bus_right[k], rig.bus[k]);
    }

    let pcm = vec![0.0; rig.master() as usize];
    let SessionJob::Snapshot(s) = run(&mut rig, SessionJob::Snapshot(Snapshot::new(pcm))) else { unreachable!() };
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!(s.tracks[0].map(|t| t.mix.pan), Some(0.5), "the snapshot carries the lane's pan");
    rig.press(Command::SetPan(0, -0.25));
    let pcm = vec![0.0; rig.master() as usize];
    let SessionJob::Snapshot(s) = run(&mut rig, SessionJob::Snapshot(Snapshot::new(pcm))) else { unreachable!() };
    assert_eq!(s.tracks[0].map(|t| t.mix.pan), Some(-0.25), "its target, pinned while the glide still moves");
}

#[test]
fn the_wet_master_renders_each_lanes_pan() {
    const MASTER: usize = 96_000;
    let track = |mix: LaneMix| {
        let mut buf = vec![0.0f32; MASTER];
        for p in [1_000, 30_000, 70_000] {
            buf[p] = 0.25;
        }
        LoadTrack { index: 1, buf, peaks: Vec::new(), reversed: false, playing: true, mix }
    };
    let render = |track_mix: LaneMix, mix: LaneMix| {
        let load = Load { bpm: 120, bars: 1, master: MASTER as Frame, tracks: vec![track(track_mix)], result: None };
        wet_master(48_000, load, &[mix], &[]).expect("the render")
    };
    let centre = render(LaneMix::default(), LaneMix::default());
    assert_eq!(centre.left, centre.right);
    let left = render(LaneMix { pan: -1.0, ..LaneMix::default() }, LaneMix { pan: -1.0, ..LaneMix::default() });
    assert!(left.right.iter().all(|&x| x == 0.0), "hard left: the right is exactly silent");
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak(&left.left) / peak(&centre.left) - SQRT_2 as f32).abs() < 1e-3, "and the left 3 dB up");
    // The snapshot's mix wins over the load track's: the render glides there inside its warm-up.
    let moved = render(LaneMix::default(), LaneMix { pan: 1.0, ..LaneMix::default() });
    assert!(moved.left.iter().all(|&x| x == 0.0), "hard right from the mix: the left is exactly silent");
    let mirror = moved.right.iter().zip(&left.left).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(mirror < 1e-6, "the mirror image ({mirror})");
}

#[test]
fn a_compact_mix_carries_the_pan_both_ways() {
    let mix = LaneMix { pan: -0.3, ..LaneMix::default() };
    let compact = CompactMix::from(&mix);
    assert_eq!(compact.pan, -0.3);
    assert_eq!(compact.widen(), mix);
    assert_ne!(compact, CompactMix::from(&LaneMix::default()), "a pan alone is a change of mix");
}
