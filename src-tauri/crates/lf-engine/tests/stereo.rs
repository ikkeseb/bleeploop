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
//! What E1 leaves to E2 shows here too: a latent unit that lands mid-take moves what a live chain records
//! after it (the held edit is E2's); this file's mid-take unit lands after the impulses it measures.

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
    rig.advance(rig.master());
    let playback = [rig.output.as_ref().unwrap().1.clone(), rig.output_right.clone()];
    assert_eq!((nonzero(&playback[0]), nonzero(&playback[1])), (vec![1000], vec![3000]), "its playback, each side at its loop position");
    assert_eq!((playback[0][1000], playback[1][3000]), (1.0, 0.375));
    (rig, Heard { monitor, record, pcm, playback }, [1000, 3000])
}

#[test]
fn distinct_impulses_keep_their_side_frame_and_gain_from_input_to_the_wet_master() {
    let (mut rig, heard, [pa, pb]) = distinct(128);
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
    // A one-channel load (`engine_io` copies a one-channel track to both sides): the block on both.
    let mut mono = Rig::new();
    mono.set(Command::SetMetronome(false));
    let mut one = load(capacity);
    for p in one.tracks[0].buf.iter_mut() {
        *p = [p[0], p[0]];
    }
    let SessionJob::Load(done) = run(&mut mono, SessionJob::Load(one)) else { unreachable!() };
    assert_eq!(done.result, Some(Ok(())));
    assert_eq!(mono.pcm2(0)[pa], [1.0, 1.0], "a one-channel track plays on both sides");
    // The wet master: each side's impulse at its frame (the stems' frame 0 is the master's), the other
    // side silent there.
    let wet = wet_master(SR, load(master), &[]).expect("the render");
    let peak = |x: &[f32]| x.iter().enumerate().fold((0, 0.0f32), |(k, m), (i, v)| if v.abs() > m { (i, v.abs()) } else { (k, m) }).0;
    assert_eq!((peak(&wet.left), peak(&wet.right)), (pa, pb), "the wet master, each impulse on its side at its frame");
    assert!(wet.right[pa].abs() < 1e-6 && wet.left[pb].abs() < 1e-6, "nowhere else: {} {}", wet.right[pa], wet.left[pb]);
}

#[test]
fn distinct_impulses_render_bit_identical_at_every_block_size() {
    let reference = distinct(128).1;
    for block in BLOCKS {
        assert_eq!(distinct(block).1, reference, "block {block}");
    }
}

#[test]
fn unequal_latencies_on_two_live_inputs_land_on_one_loop_frame_each_heard_at_its_own() {
    for block in BLOCKS {
        let mut rig = Rig::with(Opts { block, ..Opts::default() });
        rig.install(0, Lag::new(64, [1.0; 2]));
        rig.install(0, Lag::new(32, [1.0; 2]));
        rig.install(1, Lag::new(17, [1.0; 2]));
        rig.set(Command::SetSlotLive(1, true));
        rig.set_stereo_inputs(|_| [0.0; 2], |_| [0.0; 2]);
        let start = arm_one_bar(&mut rig, 1.0);
        let end = rig.end_frame();
        assert_eq!((rig.engine.slot(0).map(|s| s.1), rig.engine.slot(1).map(|s| s.1), rig.engine.live_latency()), (Some(96), Some(17), 96));
        let x = start + 2000;
        rig.set_stereo_inputs(move |f| [if f == x { 1.0 } else { 0.0 }, 0.0], move |f| [0.0, if f == x { 1.0 } else { 0.0 }]);
        rig.keep_output();
        let from = rig.frame;
        rig.advance_to(x + 200);
        // A unit that comes mid-take, after the impulses: the take's window and alignment stay.
        rig.install(1, Lag::new(50, [1.0; 2]));
        rig.advance(1);
        assert_eq!((rig.start_frame(), rig.end_frame(), rig.engine.slot(1).map(|s| s.1)), (start, end, Some(67)), "block {block}: the window did not move");
        rig.advance_to(end + 1);
        assert_eq!(rig.state(0), LaneState::Playing);
        let (l, r) = (&rig.monitor, &rig.monitor_right);
        assert_eq!((nonzero(&l[..300 + (x - from) as usize]), nonzero(&r[..300 + (x - from) as usize])), (vec![(x + 96 - from) as usize], vec![(x + 17 - from) as usize]), "block {block}: each heard at its own latency");
        let pcm = rig.pcm2(0);
        let (pl, pr): (Vec<f32>, Vec<f32>) = pcm.iter().map(|&[l, r]| (l, r)).unzip();
        let p = (x + 96 - start) as usize;
        assert_eq!((nonzero(&pl), nonzero(&pr)), (vec![p], vec![p]), "block {block}: one loop frame, each on its side");
        assert_eq!(pcm[p], [1.0, 1.0]);
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
    assert!(at_most.iter().any(|p| p[0] != 0.0 && p[0] != 1.0), "the synth recorded");
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
