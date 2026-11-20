//! The thin stereo slice (`docs/unit-contract.md` §9 E1): a pair per slot input, a chain seating two
//! units, the stereo wet, monitor and record tap, the stereo lanes, AUTO REC's stereo onset, the record
//! lines' sizing, the snapshot, the load and the wet master. The fakes are test units that scale each
//! side and lag it on a ring, allocated before they are installed, so every `process` runs under the
//! rig's `assert_no_alloc`.
//!
//! The regression bar (a signal with equal sides renders, on each side, today's mono bits) is the rest
//! of the suite, which feeds its inputs as equal pairs: the rig checks every block's monitor has equal
//! sides, `common::mono` that every loop it reads does. Deliberate breaks of E1, each named where it
//! shows: a built-in synth's recorded sound is its stereo bus (the drum kit's two sides; the mono synths
//! read as before), and a plugin's output is its own pair (`src-tauri/src/host`).
//!
//! A latent unit that comes or goes mid-take moves nothing the take records outside its own crossfade:
//! the alignment holds for the capture and each chain's wet follows its own sum (`Rack::latch_record`).
//! What E1 leaves to E2: a chain that grows past the take's alignment mid-take (E2 holds that edit).

mod common;

use common::{Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{
    max_chain_latency, wet_master, Command, Event, Instrument, LaneState, Load, LoadTrack, NoteTarget, Pair, SeatOp, SeatOutcome, SeatRefusal, SessionJob, SlotEvent, Snapshot, UnitCx, UnitInfo, UnitKind,
    UnitProcessor,
};

const SR: u32 = 48_000;
const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];

/// A test unit: each side times its gain, `latency` frames late, on a ring allocated in `new`.
struct Lag {
    gain: [f32; 2],
    line: [Vec<f32>; 2],
    at: usize,
}

impl Lag {
    fn new(latency: usize, gain: [f32; 2]) -> Box<Lag> {
        Box::new(Lag { gain, line: [vec![0.0; latency], vec![0.0; latency]], at: 0 })
    }
}

impl UnitProcessor for Lag {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: self.line[0].len() as Frame }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let n = self.line[0].len();
        let mut end = self.at;
        for (c, out) in out.into_iter().enumerate() {
            let (line, g) = (&mut self.line[c], self.gain[c]);
            let mut at = self.at;
            for (y, &x) in out.iter_mut().zip(input[c]) {
                if n == 0 {
                    *y = x * g;
                } else {
                    *y = line[at] * g;
                    line[at] = x;
                    at = (at + 1) % n;
                }
            }
            end = at;
        }
        self.at = end;
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
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

/// Every position of `x` that is not exactly 0.
fn nonzero(x: &[f32]) -> Vec<usize> {
    x.iter().enumerate().filter(|(_, v)| **v != 0.0).map(|(k, _)| k).collect()
}

/// A one-bar FIXED first take on lane 0, the click off: the rig at the window's start, which it returns.
fn arm_one_bar(rig: &mut Rig, bars: f64) -> Frame {
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.press(Command::RecDub(0));
    rig.start_frame()
}

/// What one block size heard of the distinct impulses: the monitor and the record tap over the take,
/// the loop, and one pass of its playback, each side.
#[derive(PartialEq, Debug)]
struct Heard {
    monitor: [Vec<f32>; 2],
    record: [Vec<f32>; 2],
    pcm: Vec<Pair>,
    playback: [Vec<f32>; 2],
    /// The same pass as the device plays it (after the lanes' FX, the master and its limiter), from the
    /// frame the limiter's pre-delay puts loop position 0 at.
    master: [Vec<f32>; 2],
}

/// Left 1.0 at one input frame, right 0.5 at another, through two units (latencies 64 and 32) that
/// scale the sides differently: left by 2 · 0.5 = 1, right by 0.25 · 3 = 0.75.
fn distinct(block: usize) -> (Rig, Heard, [usize; 2]) {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.install(0, Lag::new(64, [2.0, 0.25]));
    rig.install(0, Lag::new(32, [0.5, 3.0]));
    rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
    let start = arm_one_bar(&mut rig, 1.0);
    assert_eq!(rig.engine.slot(0).map(|s| s.1), Some(96), "the chain's latency is its units' sum");
    // Each impulse reaches the tap 96 frames after its input frame: loop positions 1000 and 3000.
    let (a, b) = (start + 1000 - 96, start + 3000 - 96);
    rig.set_stereo_inputs(move |f| [if f == a { 1.0 } else { 0.0 }, if f == b { 0.5 } else { 0.0 }], |_| [0.0; 2]);
    rig.keep_output();
    let from = rig.frame;
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    let monitor = [rig.monitor.clone(), rig.monitor_right.clone()];
    let record = [rig.record.clone(), rig.record_right.clone()];
    let (heard_a, heard_b) = ((a + 96 - from) as usize, (b + 96 - from) as usize);
    for (what, [l, r]) in [("monitor", &monitor), ("record tap", &record)] {
        assert_eq!((nonzero(l), nonzero(r)), (vec![heard_a], vec![heard_b]), "{what}: each impulse on its own side at its frame, nowhere else");
        assert_eq!((l[heard_a], r[heard_b]), (1.0, 0.375), "{what}: at its gain");
    }
    let pcm = rig.pcm2(0);
    let (l, r): (Vec<f32>, Vec<f32>) = pcm.iter().map(|&[l, r]| (l, r)).unzip();
    assert_eq!((nonzero(&l), nonzero(&r)), (vec![1000], vec![3000]), "the lane holds each on its side");
    assert_eq!((pcm[1000], pcm[3000]), ([1.0, 0.0], [0.0, 0.375]));
    // One pass of its playback: the looper's own mix (lanes before their FX), the click off.
    let next = rig.next_boundary();
    rig.advance_to(next);
    rig.keep_output();
    let (master, limiter) = (rig.master(), rig.engine.limiter_latency());
    rig.advance(master + limiter);
    let m = master as usize;
    let playback = [rig.output.as_ref().unwrap().1[..m].to_vec(), rig.output_right[..m].to_vec()];
    assert_eq!((nonzero(&playback[0]), nonzero(&playback[1])), (vec![1000], vec![3000]), "its playback, each side at its loop position");
    assert_eq!((playback[0][1000], playback[1][3000]), (1.0, 0.375));
    let l = limiter as usize;
    let master = [rig.heard[l..l + m].to_vec(), rig.heard_right[l..l + m].to_vec()];
    (rig, Heard { monitor, record, pcm, playback, master }, [1000, 3000])
}

#[test]
fn distinct_impulses_keep_their_side_frame_and_gain_from_input_to_the_wet_master() {
    let (mut rig, heard, _) = distinct(128);
    let master = rig.master() as usize;
    // The snapshot's stems: the lane's pairs, bit for bit.
    let mut pcm = Vec::with_capacity(master);
    pcm.resize(master, [0.0f32; 2]);
    let SessionJob::Snapshot(s) = run(&mut rig, SessionJob::Snapshot(Snapshot::new(pcm))) else { unreachable!() };
    assert_eq!(s.result, Some(Ok(())));
    assert_eq!(s.pcm[..master], heard.pcm[..], "the stems");
    let mix = s.tracks[0].unwrap().mix;
    // A fresh engine loaded from them plays them back as they were.
    let mut fresh = Rig::new();
    fresh.set(Command::SetMetronome(false));
    let load = |capacity: usize| {
        let mut buf = s.pcm[..master].to_vec();
        buf.resize(capacity, [0.0; 2]);
        Load { bpm: s.bpm, meter: s.meter, bars: 1, master: s.master, tracks: vec![LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix }], result: None }
    };
    let capacity = fresh.engine.looper().capacity() as usize;
    let SessionJob::Load(done) = run(&mut fresh, SessionJob::Load(load(capacity))) else { unreachable!() };
    assert_eq!(done.result, Some(Ok(())));
    assert_eq!(fresh.pcm2(0), heard.pcm, "the loaded lane");
    let next = fresh.next_boundary();
    fresh.advance_to(next);
    fresh.keep_output();
    fresh.advance(fresh.master());
    assert_eq!([fresh.output.as_ref().unwrap().1.clone(), fresh.output_right.clone()], heard.playback, "the loaded lane plays what the take played");
    // The wet master: the live path by construction, so both of its channels, complete, are what the
    // device played of that pass (each impulse on its side, through the lanes' FX, the master and the
    // limiter). A one-channel session's load is `engine_io`'s (`tests.rs`, its real bytes).
    let wet = wet_master(SR, load(master), &[]).expect("the render");
    // Not bit for bit: the live pass's limiter and bypassed FX crossfades carry the take's history, the
    // render's its warm-up (at the impulses about 1e-4 apart, measured).
    for (side, (wet, live)) in [&wet.left, &wet.right].into_iter().zip(&heard.master).enumerate() {
        assert_eq!(wet.len(), live.len());
        // `f32::max` passes over a NaN difference: every sample finite first.
        assert!(wet.iter().chain(live).all(|x| x.is_finite()), "side {side}: every sample finite");
        let worst = wet.iter().zip(live).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 5e-4, "side {side}: the wet master is the pass the device played, every frame ({worst} apart)");
    }
}

#[test]
fn distinct_impulses_render_bit_identical_at_every_block_size() {
    let reference = distinct(128).1;
    for block in BLOCKS {
        assert_eq!(distinct(block).1, reference, "block {block}");
    }
}

/// A take whose sides differ, `[x, -x]` among them (format v3, `docs/unit-contract.md` §8): its
/// snapshot's stems, a fresh engine loaded from them and that engine's playback are the take's, bit for
/// bit on each side. A mean of the sides would hold `[x, -x]` as silence. The bytes between snapshot and
/// load are `engine_io`'s (`src-tauri/src/engine_io/tests.rs`, `a_stereo_sessions_sides_survive_…`).
#[test]
fn an_unalike_stereo_take_comes_back_from_its_snapshot_bit_for_bit_on_each_side() {
    let mut rig = Rig::new();
    rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
    let start = arm_one_bar(&mut rig, 1.0);
    let half = (rig.end_frame() - start) / 2;
    // The first half `[x, -x]`; the second unalike sides, the right a slower saw.
    let x = |f: Frame| 0.25 * (f as f32 * 0.013_7).sin();
    rig.set_stereo_inputs(move |f| if f < start + half { [x(f), -x(f)] } else { [x(f), 0.4 * ((f % 331) as f32 / 331.0) - 0.2] }, |_| [0.0; 2]);
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    // Silence from here: the rig's output carries the monitor, and the pass below is the loop's alone.
    rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
    let take = rig.pcm2(0);
    let m = take.len();
    let opposite = take.iter().filter(|[l, r]| *l != 0.0 && r.to_bits() == (-*l).to_bits()).count();
    let unalike = take.iter().filter(|[l, r]| l.abs() > 1e-3 && r.abs() > 1e-3 && (*l + *r).abs() > 1e-3 && l != r).count();
    assert!(opposite > m / 3 && unalike > m / 3, "the take holds both: {opposite} opposite frames, {unalike} unalike, of {m}");

    let mut pcm = Vec::with_capacity(m);
    pcm.resize(m, [0.0f32; 2]);
    let SessionJob::Snapshot(s) = run(&mut rig, SessionJob::Snapshot(Snapshot::new(pcm))) else { unreachable!() };
    assert_eq!(s.result, Some(Ok(())));
    let bits = |p: &[Pair]| p.iter().map(|[l, r]| [l.to_bits(), r.to_bits()]).collect::<Vec<_>>();
    assert_eq!(bits(&s.pcm[..m]), bits(&take), "the stems");
    // One pass of the take's playback, the looper's own mix, from a boundary.
    let pass = |rig: &mut Rig| {
        let next = rig.next_boundary();
        rig.advance_to(next);
        rig.keep_output();
        rig.advance(rig.master());
        [rig.output.as_ref().unwrap().1.clone(), rig.output_right.clone()]
    };
    let heard = pass(&mut rig);
    assert!(heard[0].iter().zip(&heard[1]).filter(|(l, r)| **l != 0.0 && **r == -**l).count() > m / 3, "the [x, -x] half plays opposite");

    let mut fresh = Rig::new();
    fresh.set(Command::SetMetronome(false));
    let mut buf = s.pcm[..m].to_vec();
    buf.resize(fresh.engine.looper().capacity() as usize, [0.0; 2]);
    let mix = s.tracks[0].unwrap().mix;
    let load = Load { bpm: s.bpm, meter: s.meter, bars: 1, master: s.master, tracks: vec![LoadTrack { index: 0, buf, peaks: Vec::new(), reversed: false, playing: true, mix }], result: None };
    let SessionJob::Load(done) = run(&mut fresh, SessionJob::Load(load)) else { unreachable!() };
    assert_eq!(done.result, Some(Ok(())));
    assert_eq!(bits(&fresh.pcm2(0)), bits(&take), "the loaded lane");
    let replay = pass(&mut fresh);
    let side_bits = |v: &[Vec<f32>; 2]| v.iter().map(|c| c.iter().map(|x| x.to_bits()).collect::<Vec<_>>()).collect::<Vec<_>>();
    assert_eq!(side_bits(&replay), side_bits(&heard), "the loaded lane plays the take's pass, each side bit for bit");
}

/// Impulses on both inputs at each of `at` (frames after the window's start), slot 0 chaining 64 + 32
/// and slot 1 17, both live; `edit` runs on the rig once the frame `edits[k]` after the start is
/// reached. Returns the loop and the monitor, each side, from `from`.
fn two_live_inputs(block: usize, at: &[Frame], edits: &[(Frame, &dyn Fn(&mut Rig))]) -> (Vec<Pair>, [Vec<f32>; 2], Frame, Frame) {
    let mut rig = Rig::with(Opts { block, ..Opts::default() });
    rig.install(0, Lag::new(64, [1.0; 2]));
    rig.install(0, Lag::new(32, [1.0; 2]));
    rig.install(1, Lag::new(17, [1.0; 2]));
    rig.set(Command::SetSlotLive(1, true));
    rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
    let start = arm_one_bar(&mut rig, 1.0);
    let end = rig.end_frame();
    assert_eq!((rig.engine.slot(0).map(|s| s.1), rig.engine.slot(1).map(|s| s.1), rig.engine.live_latency()), (Some(96), Some(17), 96));
    let xs: Vec<Frame> = at.iter().map(|&k| start + k).collect();
    let ys = xs.clone();
    rig.set_stereo_inputs(move |f| [if xs.contains(&f) { 1.0 } else { 0.0 }, 0.0], move |f| [0.0, if ys.contains(&f) { 1.0 } else { 0.0 }]);
    rig.keep_output();
    let from = rig.frame;
    for (k, edit) in edits {
        rig.advance_to(start + k);
        edit(&mut rig);
    }
    rig.advance_to(end - 1);
    assert_eq!((rig.start_frame(), rig.end_frame()), (start, end), "block {block}: the window did not move");
    rig.advance_to(end + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    (rig.pcm2(0), [rig.monitor.clone(), rig.monitor_right.clone()], start, from)
}

#[test]
fn unequal_latencies_on_two_live_inputs_land_on_one_loop_frame_each_heard_at_its_own() {
    for block in BLOCKS {
        let (pcm, [l, r], start, from) = two_live_inputs(block, &[2000], &[]);
        let x = (start + 2000 - from) as usize;
        assert_eq!((nonzero(&l), nonzero(&r)), (vec![x + 96], vec![x + 17]), "block {block}: each heard at its own latency");
        let (pl, pr): (Vec<f32>, Vec<f32>) = pcm.iter().map(|&[l, r]| (l, r)).unzip();
        let p = 2000 + 96;
        assert_eq!((nonzero(&pl), nonzero(&pr)), (vec![p], vec![p]), "block {block}: one loop frame, each on its side");
        assert_eq!(pcm[p], [1.0, 1.0]);
    }
}

/// A latent unit added to the 17-frame chain mid-take and removed again, each between two impulses:
/// every impulse lands on its loop frame, on both sides, and the middle one is heard 50 frames later.
#[test]
fn a_unit_that_comes_and_goes_mid_take_moves_no_impulse() {
    let install = |rig: &mut Rig| rig.install(1, Lag::new(50, [1.0; 2]));
    let remove = |rig: &mut Rig| rig.remove(1);
    for block in BLOCKS {
        let at = [2000, 30_000, 60_000];
        let (pcm, [l, r], start, from) = two_live_inputs(block, &at, &[(10_000, &install), (40_000, &remove)]);
        let (pl, pr): (Vec<f32>, Vec<f32>) = pcm.iter().map(|&[l, r]| (l, r)).unzip();
        let ps: Vec<usize> = at.iter().map(|&k| k as usize + 96).collect();
        assert_eq!((nonzero(&pl), nonzero(&pr)), (ps.clone(), ps.clone()), "block {block}: each impulse on its loop frame, each side");
        assert!(ps.iter().all(|&p| pcm[p] == [1.0, 1.0]), "block {block}: at its gain");
        let x: Vec<usize> = at.iter().map(|&k| (start + k - from) as usize).collect();
        assert_eq!(nonzero(&l), x.iter().map(|x| x + 96).collect::<Vec<_>>(), "block {block}: slot 0 heard as before");
        assert_eq!(nonzero(&r), vec![x[0] + 17, x[1] + 67, x[2] + 17], "block {block}: slot 1 heard through what it seats");
    }
}

/// An install serviced in the block a take arms in, raising the live latency past the other chain's:
/// the take aligns on it, and both inputs' impulses land on one loop frame.
#[test]
fn an_install_in_the_block_a_take_arms_aligns_the_take() {
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { block, ..Opts::default() });
        rig.install(0, Lag::new(96, [1.0; 2]));
        rig.set(Command::SetSlotLive(1, true));
        rig.set(Command::SetMetronome(false));
        rig.set(Command::SetFixedLength(true));
        rig.set(Command::SetFixedBars(1.0));
        rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
        rig.install(1, Lag::new(200, [1.0; 2]));
        rig.press(Command::RecDub(0));
        assert_eq!(rig.engine.live_latency(), 200, "block {block}: serviced with the arm");
        let start = rig.start_frame();
        let x = start + 2000;
        rig.set_stereo_inputs(move |f| [if f == x { 1.0 } else { 0.0 }, 0.0], move |f| [0.0, if f == x { 1.0 } else { 0.0 }]);
        rig.advance_to(rig.end_frame() + 1);
        let pcm = rig.pcm2(0);
        let (pl, pr): (Vec<f32>, Vec<f32>) = pcm.iter().map(|&[l, r]| (l, r)).unzip();
        assert_eq!((nonzero(&pl), nonzero(&pr)), (vec![2200], vec![2200]), "block {block}: one loop frame");
    }
}

/// A take of two bars with impulses on both slots and a built-in synth's note, at the largest input
/// side, `chain` in slot 0 (a 1.0-gain lag of that latency, or none): the loop.
fn capacity_take(chain: Option<usize>) -> Vec<Pair> {
    let mut rig = Rig::with(Opts { block: 480, ..Opts::default() });
    let max_input = rig.engine.config().max_input_frames;
    rig.input_latency = max_input;
    rig.align = max_input + 256;
    if let Some(latency) = chain {
        rig.install(0, Lag::new(latency, [1.0; 2]));
    }
    rig.set(Command::SetSlotLive(1, true));
    rig.set(Command::SelectInstrument(NoteTarget::Builtin(Instrument::Lead)));
    rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
    // Frames counted from where the take would start with no live chain: the window opens the chain's
    // latency later, and what the player played lands where it lands without it.
    let start = arm_one_bar(&mut rig, 2.0) - rig.engine.live_latency();
    let x = start + 4000;
    rig.set_stereo_inputs(move |f| [if f == x { 1.0 } else { 0.0 }, 0.0], move |f| [0.0, if f == x + 500 { 1.0 } else { 0.0 }]);
    rig.send_at(x + 1000, Command::NoteOn(60, 1.0));
    rig.send_at(x + 30_000, Command::NoteOff(60));
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    rig.pcm2(0)
}

#[test]
fn a_chain_at_the_most_latency_records_every_start_intact_and_one_frame_more_is_refused() {
    let max = max_chain_latency(SR) as usize;
    let at_most = capacity_take(Some(max));
    // Each start on its own: the chained slot's impulse at 4000 (left), the dry slot's at 4500 (right),
    // each at its gain; the synth's note, played 1000 frames after the first and recorded the input
    // side later (a note played when heard), starts there and not a frame before, on both sides.
    let input = Rig::new().engine.config().max_input_frames as usize;
    let onset = 5000 + input;
    let (l, r): (Vec<f32>, Vec<f32>) = at_most[..onset].iter().map(|&[l, r]| (l, r)).unzip();
    assert_eq!((nonzero(&l), nonzero(&r)), (vec![4000], vec![4500]), "the two impulses, nothing else before the note");
    assert_eq!((at_most[4000], at_most[4500]), ([1.0, 0.0], [0.0, 1.0]));
    let synth = &at_most[onset..onset + 64];
    assert!(synth.iter().any(|p| p[0] != 0.0) && synth.iter().all(|p| p[0] == p[1]), "the synth from its onset, both sides");
    assert_eq!(at_most, capacity_take(None), "every start lands where it lands with no chain: the dry slot's, the chained slot's, the synth's");

    let mut rig = Rig::new();
    let past = rig.install_id(1, Lag::new(max + 1, [1.0; 2]));
    let first = rig.install_id(0, Lag::new(max, [1.0; 2]));
    let one_more = rig.install_id(0, Lag::new(1, [1.0; 2]));
    rig.advance(1);
    let seats: Vec<_> = rig.events.iter().filter_map(|e| match *e {
        Event::Seat { slot, unit, op: SeatOp::Install, outcome, .. } => Some((slot, unit, outcome)),
        _ => None,
    }).collect();
    assert_eq!(seats, [(0, first, SeatOutcome::Applied), (0, one_more, SeatOutcome::Refused(SeatRefusal::Latency)), (1, past, SeatOutcome::Refused(SeatRefusal::Latency))]);
    assert_eq!(rig.returned_id(1).map(|r| r.0), Some(past), "a refused unit comes straight back");
    assert_eq!(rig.returned_id(0).map(|r| r.0), Some(one_more));
    assert_eq!(rig.engine.slot(0).map(|s| s.1), Some(max as Frame));
}

#[test]
fn auto_rec_triggers_on_an_attack_of_opposite_polarity_and_opens_the_loop_with_that_pair() {
    let mut rig = Rig::new();
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    let at = rig.frame + rig.seconds(1.0);
    // The mean of the two sides is silent throughout: only linked energy hears it.
    let onset = move |f: Frame| if f < at { [0.0; 2] } else { let x = 0.2 + ((f - at) % 7) as f32 / 100.0; [x, -x] };
    rig.set_stereo_inputs(onset, |_| [0.0; 2]);
    rig.advance_to(at + 600);
    let info = rig.lane(0);
    assert!(!info.auto_armed && info.state == LaneState::Recording, "it triggered");
    let start = rig.start_frame();
    let take = rig.engine.looper().take_pcm(0);
    assert_eq!(take.len() as Frame, rig.frame - start);
    assert!(take.iter().enumerate().all(|(k, &p)| p == onset(start + k as Frame)), "one contiguous stereo run from the retained start");
    assert!(take.iter().any(|p| p[0] > 0.0 && p[1] == -p[0]), "the pair itself, not its mean");
}

/// The rings full: the feed stops reading (the event ring fills) and the host stops taking its units
/// back (the return ring fills) while it keeps sending installs and removals. A message with no place
/// for its answer or its unit waits in its port (which then refuses the host); once the host reads
/// again, every message is answered exactly once and every unit comes back exactly once: no answer
/// dropped, no unit leaked, no protocol error.
#[test]
fn full_rings_keep_every_answer_and_every_unit_until_the_host_reads_again() {
    let mut rig = Rig::with_config(Opts::default(), |c| c.event_capacity = 8);
    rig.advance(128);
    rig.hold_events = true;
    let (mut sent, mut installed, mut removals) = (Vec::new(), Vec::new(), Vec::new());
    let (mut next, mut turned_away) = (1u32, 0);
    let send = |rig: &mut Rig, removals: &mut Vec<(u8, u32)>, sent: &mut Vec<(u8, u32, SeatOp)>| {
        removals.retain(|&(slot, id)| {
            let pushed = rig.port(slot as usize).remove(id);
            if pushed {
                sent.push((slot, id, SeatOp::Remove));
            }
            !pushed
        });
    };
    for _ in 0..200 {
        send(&mut rig, &mut removals, &mut sent);
        for slot in 0..2u8 {
            match rig.port(slot as usize).install(next, Lag::new(8, [1.0; 2])) {
                Ok(_) => {
                    sent.push((slot, next, SeatOp::Install));
                    installed.push((slot, next));
                    removals.push((slot, next));
                    next += 1;
                }
                Err(_) => turned_away += 1,
            }
        }
        send(&mut rig, &mut removals, &mut sent);
        rig.advance(rig.block as Frame);
    }
    assert!(turned_away > 0, "the ports filled up");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 0, "nothing dropped or leaked while full");
    rig.hold_events = false;
    let mut back = Vec::new();
    for _ in 0..10_000 {
        send(&mut rig, &mut removals, &mut sent);
        for slot in 0..2u8 {
            while let Some((id, _)) = rig.returned_id(slot as usize) {
                back.push((slot, id));
            }
        }
        rig.advance(rig.block as Frame);
        if removals.is_empty() && back.len() == installed.len() {
            break;
        }
    }
    rig.advance(rig.block as Frame);
    let mut answered: Vec<(u8, u32, SeatOp)> = rig.events.iter().filter_map(|e| match *e {
        Event::Seat { slot, unit, op, outcome, .. } => {
            assert_eq!(outcome, SeatOutcome::Applied, "unit {unit}: {op:?}");
            Some((slot, unit, op))
        }
        _ => None,
    }).collect();
    let key = |&(slot, id, op): &(u8, u32, SeatOp)| (slot, id, op == SeatOp::Remove);
    answered.sort_by_key(key);
    sent.sort_by_key(key);
    assert_eq!(answered, sent, "every message answered, once");
    back.sort();
    installed.sort();
    assert_eq!(back, installed, "every unit back, once");
    assert_eq!(rig.engine.diag().slot_protocol_errors, 0);
}
