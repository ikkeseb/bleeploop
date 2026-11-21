//! N units per slot (`docs/unit-contract.md` §9 E2): a chain's order, a move and its dip, the bypass,
//! the edits held for a take, the lifecycle (install at an index, a source's replacement, removal
//! anywhere, a withdrawn seat, a park, an eviction), the latency sums, and a proptest of edit scripts.
//! Every scenario renders through the rig (`tests/common`), so every `process` runs under
//! `assert_no_alloc`, and those the contract asks it of at block sizes 1, 32, 64, 127, 128, 480 and
//! 1024, against values computed frame by frame (so bit-identical across them).
//!
//! The fakes are test units that log what they saw (each call's frame and length, each note, each
//! `set_bypass`, the `stop`) into buffers allocated before they are installed, and count their drops,
//! so a test sees the engine never drop one.

mod common;

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use common::{code, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{ChainSeat, Command, Event, LaneState, NoteTarget, Pair, ProcessContext, SeatOp, SeatOutcome, SeatRefusal, SlotEvent, SlotEventKind, SlotKind, UnitCx, UnitId, UnitInfo, UnitKind, UnitProcessor, MAX_UNITS};
use proptest::prelude::*;

const BLOCKS: [usize; 7] = [1, 32, 64, 127, 128, 480, 1024];
/// The seat's crossfades and a move's dip at 48 kHz: 10 ms.
const FADE: u32 = 480;
/// What a fake logs before it stops logging.
const LOG: usize = 1 << 17;
/// The first frame the rig renders, where an install made before it seats.
const F0: Frame = 48_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Seen {
    Call { frame: Frame, len: usize },
    Note { frame: Frame, kind: SlotEventKind },
    Bypass { frame: Frame, on: bool },
    Stop,
}

/// The test's view of a fake while the engine holds it.
#[derive(Clone)]
struct Probe {
    log: Arc<Mutex<Vec<Seen>>>,
    drops: Arc<AtomicUsize>,
}

impl Probe {
    fn new() -> Probe {
        Probe { log: Arc::new(Mutex::new(Vec::with_capacity(LOG))), drops: Arc::new(AtomicUsize::new(0)) }
    }

    fn seen(&self) -> Vec<Seen> {
        let log = self.log.lock().unwrap();
        assert!(log.len() < LOG, "the fake's log filled up");
        log.clone()
    }

    fn calls(&self) -> Vec<(Frame, usize)> {
        self.seen().into_iter().filter_map(|s| if let Seen::Call { frame, len } = s { Some((frame, len)) } else { None }).collect()
    }

    fn stops(&self) -> usize {
        self.log.lock().unwrap().iter().filter(|s| **s == Seen::Stop).count()
    }

    fn drops(&self) -> usize {
        self.drops.load(SeqCst)
    }
}

/// What a fake does to each side.
#[derive(Clone, Copy, Debug)]
enum Dsp {
    Gain(f32),
    /// A hard clip at ±the level.
    Clip(f32),
    /// Its input this many frames late (reported as its latency), times the gain.
    Lag(usize, f32),
    /// Its input plus the frames it rendered before this one, × 2^-16: its state shows any frame it
    /// skipped, saw twice, or lost to a reset.
    Count,
    /// A source: a constant level.
    Level(f32),
}

const COUNT_STEP: f32 = 1.0 / 65536.0;

struct Unit {
    dsp: Dsp,
    source: bool,
    /// It bypasses itself (passing its input as it is) when asked.
    self_bypass: bool,
    bypassed: bool,
    line: [Vec<f32>; 2],
    at: usize,
    count: u64,
    probe: Probe,
    /// It panics in a call that reaches this frame.
    panic_from: Frame,
}

impl Unit {
    fn new(dsp: Dsp, probe: &Probe) -> Box<Unit> {
        let lag = if let Dsp::Lag(n, _) = dsp { n } else { 0 };
        Box::new(Unit { dsp, source: matches!(dsp, Dsp::Level(_)), self_bypass: false, bypassed: false, line: [vec![0.0; lag], vec![0.0; lag]], at: 0, count: 0, probe: probe.clone(), panic_from: Frame::MAX })
    }

    fn panicking_from(mut self: Box<Self>, frame: Frame) -> Box<Unit> {
        self.panic_from = frame;
        self
    }

    fn self_bypassing(mut self: Box<Self>) -> Box<Unit> {
        self.self_bypass = true;
        self
    }

    /// A source that reports this latency (it plays no notes, so nothing shows it).
    fn source_lat(level: f32, latency: usize, probe: &Probe) -> Box<Unit> {
        let mut u = Unit::new(Dsp::Level(level), probe);
        u.line = [vec![0.0; latency], vec![0.0; latency]];
        u
    }
}

impl UnitProcessor for Unit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: self.source, latency: self.line[0].len() as Frame }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let len = out[0].len();
        assert!(cx.frame + (len as Frame) <= self.panic_from, "the unit panics at frame {}", self.panic_from);
        {
            let mut log = self.probe.log.lock().unwrap();
            if log.len() + 1 + events.len() < LOG {
                log.push(Seen::Call { frame: cx.frame, len });
                log.extend(events.iter().map(|e| Seen::Note { frame: cx.frame + e.offset as Frame, kind: e.kind }));
            }
        }
        let mut end = self.at;
        for (c, out) in out.into_iter().enumerate() {
            let mut at = self.at;
            for (j, (y, &x)) in out.iter_mut().zip(input[c]).enumerate() {
                *y = match self.dsp {
                    Dsp::Gain(g) => g * x,
                    Dsp::Clip(l) => x.clamp(-l, l),
                    Dsp::Lag(0, g) => g * x,
                    Dsp::Lag(n, g) => {
                        let line = &mut self.line[c];
                        let d = line[at] * g;
                        line[at] = x;
                        at = (at + 1) % n;
                        d
                    }
                    Dsp::Count => x + (self.count + j as u64) as f32 * COUNT_STEP,
                    Dsp::Level(l) => l,
                };
                if self.bypassed && self.self_bypass {
                    *y = x;
                }
            }
            end = at;
        }
        self.at = end;
        self.count += len as u64;
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        self.probe.log.lock().unwrap().push(Seen::Bypass { frame: cx.frame, on: bypassed });
        self.bypassed = bypassed;
        self.self_bypass
    }

    fn stop(&mut self) {
        self.probe.log.lock().unwrap().push(Seen::Stop);
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, SeqCst);
    }
}

fn rig(block: usize) -> Rig {
    Rig::with(Opts { block, ..Opts::default() })
}

/// Hand `unit` to `slot` under `id` at index `at`.
fn put(rig: &mut Rig, slot: usize, id: UnitId, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Box<Unit>) {
    assert!(rig.port(slot).install_at(id, at, bypassed, replaces, unit).is_ok(), "slot {slot}'s message ring is full");
}

/// Every [`Event::Seat`] of `slot`: (frame, unit, op, outcome).
fn answers(rig: &Rig, slot: u8) -> Vec<(Frame, UnitId, SeatOp, SeatOutcome)> {
    rig.events
        .iter()
        .filter_map(|e| match *e {
            Event::Seat { frame, slot: s, unit, op, outcome } if s == slot => Some((frame, unit, op, outcome)),
            _ => None,
        })
        .collect()
}

/// Every [`Event::Chain`] of `slot`: its frame and its seats.
fn chains(rig: &Rig, slot: u8) -> Vec<(Frame, Vec<ChainSeat>)> {
    rig.events
        .iter()
        .filter_map(|e| match *e {
            Event::Chain { frame, slot: s, len, seats } if s == slot => Some((frame, seats[..len as usize].to_vec())),
            _ => None,
        })
        .collect()
}

fn ids(seats: &[ChainSeat]) -> Vec<UnitId> {
    seats.iter().map(|s| s.unit).collect()
}

/// The units `slot`'s port handed back so far, as (id, bypassed, refused), each unit kept to drop later.
fn take_returns(rig: &mut Rig, slot: usize, kept: &mut Vec<Box<dyn UnitProcessor>>) -> Vec<(UnitId, bool, Option<SeatRefusal>)> {
    let mut out = Vec::new();
    while let Some(r) = rig.port(slot).returned_unit() {
        out.push((r.id, r.bypassed, r.refused));
        kept.push(r.unit);
    }
    out
}

/// The crossfade `k` frames of [`FADE`] from `from` to `to`, as the rack computes it.
fn xfade(from: f32, to: f32, k: u32) -> f32 {
    match k {
        0 => from,
        FADE => to,
        k => from + (k as f32 / FADE as f32) * (to - from),
    }
}

fn assert_frames(got: &[f32], from: Frame, want: impl Fn(Frame) -> f32, what: &str) {
    for (j, &y) in got.iter().enumerate() {
        let f = from + j as Frame;
        assert_eq!(y.to_bits(), want(f).to_bits(), "{what}: frame {f} (+{j}): {y} vs {}", want(f));
    }
}

/// Each call at least one frame long, the calls back to back from `from` through `to`: every frame
/// once, in order.
fn assert_every_frame_once(calls: &[(Frame, usize)], from: Frame, to: Frame, what: &str) {
    let mut next = from;
    for &(frame, len) in calls {
        assert!(len > 0, "{what}: an empty call at frame {frame}");
        assert_eq!(frame, next, "{what}: the calls run back to back");
        next += len as Frame;
    }
    assert!(next >= to, "{what}: called up to {next}, short of {to}");
}

fn nonzero(x: &[f32]) -> Vec<usize> {
    x.iter().enumerate().filter(|(_, v)| **v != 0.0).map(|(k, _)| k).collect()
}

/// A FIXED first take of `bars` on lane 0, the click off: the rig at the window's start, which it returns.
fn arm(rig: &mut Rig, bars: f64) -> Frame {
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.press(Command::RecDub(0));
    rig.start_frame()
}

// ── Order ────────────────────────────────────────────────────────────────────────────────────────────

/// A wave over [-1, 1) in exact steps of 1/100.
fn saw(f: Frame) -> f32 {
    ((f % 200) as f32 - 100.0) / 100.0
}

/// Slot 0 seating a hard clip at 0.5 and a gain of 2, `clip_first` or not, built by installing the
/// second first and the first `at` index 0: the monitor over 2048 frames from the first.
fn order(block: usize, clip_first: bool) -> (Vec<f32>, Vec<UnitId>) {
    let probe = Probe::new();
    let mut rig = rig(block);
    rig.set_input(saw);
    let (clip, gain) = (Unit::new(Dsp::Clip(0.5), &probe), Unit::new(Dsp::Gain(2.0), &probe));
    let ((first, a), (second, b)) = if clip_first { ((1, clip), (2, gain)) } else { ((2, gain), (1, clip)) };
    put(&mut rig, 0, second, MAX_UNITS, false, None, b);
    put(&mut rig, 0, first, 0, false, None, a);
    rig.keep_output();
    rig.advance(2048);
    assert_eq!(rig.monitor, rig.monitor_right);
    let chain = chains(&rig, 0).last().map(|c| ids(&c.1)).expect("the chain was sent");
    (rig.monitor.clone(), chain)
}

#[test]
fn each_order_equals_its_composition_bit_for_bit_and_they_differ() {
    for block in BLOCKS {
        let (clip_gain, a) = order(block, true);
        let (gain_clip, b) = order(block, false);
        assert_eq!((a, b), (vec![1, 2], vec![2, 1]), "block {block}: the install at index 0 heads the chain");
        assert_frames(&clip_gain, F0, |f| 2.0 * saw(f).clamp(-0.5, 0.5), &format!("block {block}: clip, then gain"));
        assert_frames(&gain_clip, F0, |f| (2.0 * saw(f)).clamp(-0.5, 0.5), &format!("block {block}: gain, then clip"));
        assert_ne!(clip_gain, gain_clip, "block {block}: the orders differ");
    }
}

// ── Move ─────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_move_dips_to_silence_reorders_on_the_dips_frame_and_carries_every_units_state() {
    for block in BLOCKS {
        let (pg, pc) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        rig.set_level(0.25);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Count, &pc));
        rig.advance(5000);
        let s = rig.frame;
        assert!(rig.port(0).move_to(2, 0));
        rig.keep_output();
        rig.advance(3000);
        let at = s + FADE as Frame;
        // The count is each frame's own: a frame skipped or seen twice by the counter shifts it.
        let k = |f: Frame| (f - F0) as f32 * COUNT_STEP;
        let before = |f: Frame| 0.25f32 * 2.0 + k(f);
        let after = |f: Frame| 2.0 * (0.25 + k(f));
        let want = |f: Frame| {
            let (y, d) = if f < at { (before(f), at - f) } else { (after(f), f - at) };
            if d >= FADE as Frame {
                y
            } else {
                y * (d as f32 / FADE as f32)
            }
        };
        assert_frames(&rig.monitor, s, want, &format!("block {block}: the dip"));
        let end = rig.frame;
        for (what, p) in [("the gain", &pg), ("the counter", &pc)] {
            let calls = p.calls();
            assert_every_frame_once(&calls, F0, end, &format!("block {block}: {what}"));
            assert!(calls.iter().any(|c| c.0 == at), "block {block}: {what} is called from the reorder's frame");
        }
        assert_eq!(answers(&rig, 0).last(), Some(&(at, 2, SeatOp::Move, SeatOutcome::Applied)), "block {block}: answered on the reorder");
        let last = chains(&rig, 0).last().cloned().expect("the reorder was sent");
        assert_eq!((last.0, ids(&last.1)), (at, vec![2, 1]), "block {block}: the order changed on the dip's frame");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
        assert_eq!((pg.drops(), pc.drops()), (0, 0));
    }
}

/// A device-frame jump past a move's reorder (the device never delivered the frame its dip reaches
/// silence): the overdue reorder applies on the resumed block's first frame, the dip rises on from
/// where the device clock puts it, and the slot takes its next message.
#[test]
fn a_jump_past_a_moves_reorder_applies_it_where_the_device_resumes() {
    for block in BLOCKS {
        let (pg, pc) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        rig.set_level(0.25);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Clip(0.3), &pc));
        rig.advance(5000);
        let s = rig.frame;
        assert!(rig.port(0).move_to(2, 0));
        rig.advance(64);
        rig.skip(512);
        let r = rig.frame;
        let at = s + FADE as Frame;
        assert!(r > at);
        rig.keep_output();
        rig.advance(2000);
        let what = format!("block {block}");
        // The clip first, then the gain: 2 × 0.25 clipped at 0.3.
        assert_frames(&rig.monitor, r, |f| if f - at >= FADE as Frame { 0.5 } else { 0.5 * ((f - at) as f32 / FADE as f32) }, &format!("{what}: the new order, rising"));
        let last = chains(&rig, 0).last().cloned().expect("the reorder was sent");
        assert_eq!((last.0, ids(&last.1)), (r, vec![2, 1]), "{what}: reordered where the device resumed");
        assert!(rig.port(0).remove(1));
        let t = rig.frame;
        rig.advance(block as Frame);
        assert_eq!(answers(&rig, 0).last(), Some(&(t, 1, SeatOp::Remove, SeatOutcome::Applied)), "{what}: the slot takes its next message");
        rig.advance(1000);
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2, "{what}");
        assert_eq!((pg.drops(), pc.drops()), (0, 0));
    }
}

/// A move answers `Applied` and sends its chain on the reorder's frame, when the order changes, and
/// sends no chain when its dip starts (nothing has changed there yet).
#[test]
fn a_moves_answer_and_its_chain_land_on_the_reorder_and_not_before() {
    for block in BLOCKS {
        let (pg, pc) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Clip(0.3), &pc));
        rig.advance(5000);
        let s = rig.frame;
        assert!(rig.port(0).move_to(2, 0));
        rig.advance(2000);
        let at = s + FADE as Frame;
        let what = format!("block {block}");
        let moved: Vec<(Frame, Vec<UnitId>)> = chains(&rig, 0).into_iter().filter(|c| c.0 >= s).map(|c| (c.0, ids(&c.1))).collect();
        assert_eq!(moved, vec![(at, vec![2, 1])], "{what}: one chain, the reorder's");
        assert_eq!(answers(&rig, 0).last(), Some(&(at, 2, SeatOp::Move, SeatOutcome::Applied)), "{what}: answered on the reorder");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
    }
}

/// Every applied change is its own [`Event::Chain`], on its frame, with the chain as that change left it:
/// two installs taken at one block start send two, and two bypass commands on one frame send both
/// values, in order.
#[test]
fn each_applied_change_sends_its_own_chain() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = rig(block);
        rig.advance(1000);
        let s = rig.frame;
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &p));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(3.0), &p));
        rig.advance(1000);
        let what = format!("block {block}");
        let got: Vec<(Frame, Vec<UnitId>)> = chains(&rig, 0).into_iter().map(|c| (c.0, ids(&c.1))).collect();
        assert_eq!(got, vec![(s, vec![1]), (s, vec![1, 2])], "{what}: one chain per install");
        let t = rig.frame + 7;
        rig.send_at(t, Command::SetUnitBypass(2, true));
        rig.send_at(t, Command::Action(lf_engine::Action::ToggleUnit(2)));
        rig.advance(1000);
        let flips: Vec<(Frame, bool)> = chains(&rig, 0).into_iter().filter(|c| c.0 > s).map(|c| (c.0, c.1[1].bypassed)).collect();
        assert_eq!(flips, vec![(t, true), (t, false)], "{what}: both bypass values, in order");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
    }
}

// ── Bypass ───────────────────────────────────────────────────────────────────────────────────────────

/// A plugin seat (64 frames late, ×3) bypassed by a stamped command mid-block and back by a toggle: it
/// crossfades to its input delayed by its latency, on the command's frame, settles there bit for bit,
/// is called every frame meanwhile, and the chain's latency stays.
#[test]
fn a_plugin_seat_bypassed_settles_to_its_input_delayed_by_its_latency_still_called() {
    for block in BLOCKS {
        let probe = Probe::new();
        let mut rig = rig(block);
        rig.set_input(code);
        put(&mut rig, 0, 7, MAX_UNITS, false, None, Unit::new(Dsp::Lag(64, 3.0), &probe));
        rig.advance(4000);
        let (on, off) = (rig.frame + 37, rig.frame + 1500);
        rig.send_at(on, Command::SetUnitBypass(7, true));
        rig.send_at(off, Command::Action(lf_engine::Action::ToggleUnit(7)));
        let from = rig.frame;
        rig.keep_output();
        rig.advance(3000);
        let wet = |f: Frame| -> u32 {
            if f < on {
                FADE
            } else if f < off {
                FADE - (f - on).min(FADE as Frame) as u32
            } else {
                (f - off).min(FADE as Frame) as u32
            }
        };
        let want = |f: Frame| xfade(code(f - 64), 3.0 * code(f - 64), wet(f));
        assert_frames(&rig.monitor, from, want, &format!("block {block}: the bypass crossfade"));
        assert!((on + FADE as Frame..off).all(|f| want(f) == code(f - 64)), "settled on the delayed input");
        let bypasses: Vec<Seen> = probe.seen().into_iter().filter(|s| matches!(s, Seen::Bypass { .. })).collect();
        assert_eq!(bypasses, vec![Seen::Bypass { frame: on, on: true }, Seen::Bypass { frame: off, on: false }], "block {block}: on each command's frame");
        let seen = probe.seen();
        for f in [on, off] {
            let k = seen.iter().position(|s| *s == Seen::Bypass { frame: f, on: f == on }).unwrap();
            assert!(matches!(seen[k + 1], Seen::Call { frame, .. } if frame == f), "block {block}: set_bypass before the frames from {f}");
        }
        assert_every_frame_once(&probe.calls(), F0, rig.frame, &format!("block {block}: still called"));
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 64)), "the latency stays");
        let shown: Vec<(Frame, bool, u32)> = chains(&rig, 0).iter().skip(1).map(|(f, s)| (*f, s[0].bypassed, s[0].latency)).collect();
        assert_eq!(shown, vec![(on, true, 64), (off, false, 64)], "block {block}: each bypass sent as the chain");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(7, false, None)]);
        assert_eq!(probe.drops(), 0);
    }
}

/// A unit that bypasses itself gets `set_bypass` on the command's frame and its seat passes its output
/// on, with no crossfade of the seat's own; a bypass naming an id no seat holds is dropped and counted.
#[test]
fn a_self_bypassing_unit_gets_set_bypass_on_the_commands_frame() {
    for block in BLOCKS {
        let probe = Probe::new();
        let mut rig = rig(block);
        rig.set_input(code);
        put(&mut rig, 0, 3, MAX_UNITS, false, None, Unit::new(Dsp::Gain(3.0), &probe).self_bypassing());
        rig.advance(4000);
        let (on, off) = (rig.frame + 11, rig.frame + 700);
        rig.send_at(on, Command::SetUnitBypass(3, true));
        rig.send_at(on + 5, Command::SetUnitBypass(99, true));
        rig.send_at(off, Command::Action(lf_engine::Action::ToggleUnit(3)));
        let from = rig.frame;
        rig.keep_output();
        rig.advance(1500);
        assert_frames(&rig.monitor, from, |f| if (on..off).contains(&f) { code(f) } else { 3.0 * code(f) }, &format!("block {block}"));
        let bypasses: Vec<Seen> = probe.seen().into_iter().filter(|s| matches!(s, Seen::Bypass { .. })).collect();
        assert_eq!(bypasses, vec![Seen::Bypass { frame: on, on: true }, Seen::Bypass { frame: off, on: false }], "block {block}");
        assert_eq!(rig.engine.diag().unit_commands_dropped, 1, "block {block}: the unseated id");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(3, false, None)]);
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Bypass {
    Never,
    /// The install arrives bypassed.
    AtInstall,
    /// A command mid-take.
    MidTake,
}

/// A one-bar take through a 64-frame unit at ×0.5, its impulse at loop position 2000: the loop, and the
/// chain's events.
fn take_through(block: usize, bypass: Bypass) -> (Vec<f32>, Vec<(Frame, Vec<ChainSeat>)>, Frame) {
    let probe = Probe::new();
    let mut rig = rig(block);
    put(&mut rig, 0, 5, MAX_UNITS, bypass == Bypass::AtInstall, None, Unit::new(Dsp::Lag(64, 0.5), &probe));
    let start = arm(&mut rig, 1.0);
    assert_eq!(rig.engine.live_latency(), 64);
    let x = start + 2000 - 64;
    rig.set_input(move |f| if f == x { 1.0 } else { 0.0 });
    let toggle = start + 500;
    if bypass == Bypass::MidTake {
        rig.send_at(toggle, Command::SetUnitBypass(5, true));
    }
    rig.advance_to(rig.end_frame() + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    let seen: Vec<Seen> = probe.seen().into_iter().filter(|s| matches!(s, Seen::Bypass { .. })).collect();
    let want = match bypass {
        Bypass::Never => vec![],
        Bypass::AtInstall => vec![Seen::Bypass { frame: F0, on: true }],
        Bypass::MidTake => vec![Seen::Bypass { frame: toggle, on: true }],
    };
    assert_eq!(seen, want, "block {block}: {bypass:?}");
    (rig.pcm(0), chains(&rig, 0), toggle)
}

#[test]
fn a_takes_impulse_lands_on_the_same_frame_bypassed_or_not() {
    for block in BLOCKS {
        for (bypass, gain) in [(Bypass::Never, 0.5), (Bypass::AtInstall, 1.0), (Bypass::MidTake, 1.0)] {
            let (pcm, chain, toggle) = take_through(block, bypass);
            assert_eq!(nonzero(&pcm), vec![2000], "block {block}: {bypass:?}");
            assert_eq!(pcm[2000], gain, "block {block}: {bypass:?}");
            if bypass == Bypass::MidTake {
                let last = chain.last().unwrap();
                assert_eq!((last.0, last.1[0].bypassed), (toggle, true), "block {block}: a bypass mid-take applies at once");
            }
        }
    }
}

// ── Held edits ───────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Edit {
    /// A 50-frame unit added to a 17-frame chain.
    AddLatent,
    /// The 50-frame unit removed from a 17 + 50 chain.
    RemoveLatent,
    /// A zero-latency source added at the head of a zero-latency input-fed chain.
    AddSource,
    /// A zero-latency effect added to the 17-frame chain.
    AddZero,
}

struct Held {
    pcm: Vec<f32>,
    answers: Vec<(Frame, UnitId, SeatOp, SeatOutcome)>,
    chains: Vec<(Frame, Vec<ChainSeat>)>,
    /// The monitor from the take's start on.
    monitor: Vec<f32>,
    start: Frame,
    end: Frame,
    /// The port held a message just after the edit's block, and just after the take.
    holding: (bool, bool),
}

/// Impulses at these offsets from the take's start; the edit sent at [`EDIT`]; one more after the take.
const IMPULSES: [Frame; 3] = [2000, 40_000, 70_000];
const EDIT: Frame = 20_000;
const AFTER: Frame = 5000;

fn held(block: usize, edit: Edit) -> Held {
    let p = Probe::new();
    let mut rig = rig(block);
    match edit {
        Edit::AddLatent | Edit::AddZero => put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p)),
        Edit::RemoveLatent => {
            put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p));
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
        }
        Edit::AddSource => put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p)),
    }
    let start = arm(&mut rig, 1.0);
    let end = rig.end_frame();
    let xs: Vec<Frame> = IMPULSES.iter().map(|k| start + k).chain([end + AFTER]).collect();
    rig.set_input(move |f| if xs.contains(&f) { 1.0 } else { 0.0 });
    rig.keep_output();
    let from = rig.frame;
    rig.advance_to(start + EDIT);
    match edit {
        Edit::AddLatent => {
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
            // A later message waits behind the held one, though it would not wait itself.
            put(&mut rig, 0, 3, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
        }
        Edit::RemoveLatent => assert!(rig.port(0).remove(2)),
        Edit::AddSource => put(&mut rig, 0, 2, 4, false, None, Unit::new(Dsp::Level(0.0), &p)),
        Edit::AddZero => put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p)),
    }
    rig.advance(1);
    let during = rig.port(0).holding();
    rig.advance_to(end + 1);
    assert_eq!(rig.state(0), LaneState::Playing);
    rig.advance(rig.block as Frame);
    let after = rig.port(0).holding();
    rig.advance_to(end + AFTER + 2000);
    let skip = (start - from) as usize;
    let monitor = rig.monitor[skip..].to_vec();
    let out = Held { pcm: rig.pcm(0), answers: answers(&rig, 0), chains: chains(&rig, 0), monitor, start, end, holding: (during, after) };
    rig.engine.evict_slots();
    drop(rig);
    assert_eq!(p.drops(), if edit == Edit::AddLatent { 3 } else { 2 }, "every unit back to the port, none dropped before");
    out
}

#[test]
fn an_edit_that_moves_the_alignment_waits_for_the_take_and_applies_on_the_first_block_after() {
    for edit in [Edit::AddLatent, Edit::RemoveLatent, Edit::AddSource, Edit::AddZero] {
        let mut first: Option<Vec<f32>> = None;
        for block in BLOCKS {
            let h = held(block, edit);
            let what = format!("block {block}: {edit:?}");
            let (before, after): (Frame, Option<Frame>) = match edit {
                Edit::AddLatent => (17, Some(67)),
                Edit::RemoveLatent => (67, Some(17)),
                Edit::AddSource => (0, None),
                Edit::AddZero => (17, Some(17)),
            };
            // The take: every impulse on its loop frame at the chain's sum when it armed.
            let want: Vec<usize> = IMPULSES.iter().map(|k| (k + before) as usize).collect();
            assert_eq!(nonzero(&h.pcm), want, "{what}: the take on the grid");
            assert!(want.iter().all(|&k| h.pcm[k] == 1.0), "{what}: at its gain");
            assert_eq!(first.get_or_insert_with(|| h.pcm.clone()), &h.pcm, "{what}: the same loop at every block size");
            // Heard: the take's impulses through the chain it armed with, the one after through the edited one.
            let mut heard: Vec<usize> = IMPULSES.iter().map(|k| (k + before) as usize).collect();
            if let Some(after) = after {
                heard.push((h.end - h.start + AFTER + after) as usize);
            }
            assert_eq!(nonzero(&h.monitor), heard, "{what}: heard");
            let sent = h.start + EDIT;
            let applied = h.chains.last().map(|c| c.0).expect("the edit was sent as the chain");
            if edit == Edit::AddZero {
                assert_eq!(h.answers.last(), Some(&(sent, 2, SeatOp::Install, SeatOutcome::Applied)), "{what}: a zero-latency effect does not wait");
                assert_eq!(applied, sent, "{what}");
                assert_eq!(h.holding, (false, false), "{what}");
                continue;
            }
            let op = if edit == Edit::RemoveLatent { SeatOp::Remove } else { SeatOp::Install };
            let mut want = vec![(sent, 2, op, SeatOutcome::Waiting)];
            if edit == Edit::AddLatent {
                // The message behind it is answered when it applies, with it.
                want.push((applied, 3, SeatOp::Install, SeatOutcome::Applied));
            }
            assert_eq!(h.answers[h.answers.len() - want.len()..], want[..], "{what}: answered waiting once");
            assert!(applied >= h.end && applied <= h.end + block as Frame, "{what}: applied at {applied}, the first block after the take ending at {}", h.end);
            let after: Vec<Frame> = h.chains.iter().filter(|c| c.0 > h.start).map(|c| c.0).collect();
            assert_eq!(after, vec![applied; want.len()], "{what}: nothing applied before, one chain per message applied");
            assert_eq!(h.holding, (true, false), "{what}: the port held it, then let it go");
        }
    }
}

/// A take ending on a block's first frame: the edit it held applies on that frame, in the block that
/// starts where the take ends (its end is resolved before the hold is judged), not a block later. A
/// RETAKE roll's pass edge there hands the capture on: the edit stays held across it.
#[test]
fn a_held_edit_applies_in_the_block_that_starts_where_the_take_ends() {
    for retake in [false, true] {
        for block in BLOCKS {
            let p = Probe::new();
            let mut rig = rig(block);
            put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p));
            rig.set(Command::SetRetake(retake));
            let start = arm(&mut rig, 1.0);
            let end = rig.end_frame();
            rig.advance_to(start + EDIT);
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
            rig.advance_to(end);
            let what = format!("block {block}, RETAKE {retake}");
            assert!(rig.port(0).holding(), "{what}: held through the take");
            rig.advance(block as Frame);
            let applied: Vec<Frame> = chains(&rig, 0).iter().map(|c| c.0).filter(|&f| f > start).collect();
            if retake {
                assert!(rig.port(0).holding() && applied.is_empty(), "{what}: the roll holds it on");
                assert_eq!(rig.window().and_then(|w| w.1), Some(end), "{what}: the next pass starts there");
            } else {
                assert_eq!(rig.state(0), LaneState::Playing, "{what}");
                assert_eq!((rig.port(0).holding(), applied), (false, vec![end]), "{what}: applied where the take ended");
            }
            rig.engine.evict_slots();
            drop(rig);
            assert_eq!(p.drops(), 2, "{what}: every unit back to the port, none dropped before");
        }
    }
}

/// Silence, then a soft 0.01 attack for 300 frames, then 0.2: AUTO REC's onset at `at`.
fn onset_at(at: Frame) -> impl Fn(Frame) -> f32 {
    move |f| if f < at { 0.0 } else if f < at + 300 { 0.01 } else { 0.2 }
}

/// AUTO REC listening across an edit that changes the alignment: the alignment is fixed from the onset
/// (`lf-engine`'s rule), so the edit applies at once, and what AUTO retained before it, at the old
/// alignment, is cut as an input gap cuts it: the onset it copies starts on the edit's frame, one run
/// under the alignment the take starts with.
#[test]
fn auto_listening_across_an_alignment_change_keeps_nothing_from_before_it() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = rig(block);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p));
        rig.set(Command::SetAutoRecord(true));
        rig.press(Command::RecDub(0));
        assert!(rig.lane(0).auto_armed);
        let at = rig.frame + 3000;
        rig.set_input(onset_at(at));
        rig.advance_to(at + 100);
        let edit = rig.frame;
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
        rig.advance_to(at + 2000);
        let what = format!("block {block}");
        assert!(!rig.lane(0).auto_armed, "{what}: the onset triggered");
        assert_eq!(answers(&rig, 0).last(), Some(&(edit, 2, SeatOp::Install, SeatOutcome::Applied)), "{what}: listening holds no edit");
        assert_eq!(rig.engine.live_latency(), 67, "{what}");
        assert_eq!(rig.start_frame(), edit, "{what}: the retained onset starts at the alignment change");
        rig.engine.evict_slots();
        drop(rig);
        assert_eq!(p.drops(), 2, "{what}");
    }
}

// ── Lifecycle ────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_ninth_unit_and_an_effect_ahead_of_a_source_are_refused() {
    let p = Probe::new();
    let mut rig = rig(128);
    for id in 1..=8 {
        put(&mut rig, 0, id, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    }
    put(&mut rig, 0, 9, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    put(&mut rig, 1, 20, 3, false, None, Unit::new(Dsp::Level(0.0), &p));
    put(&mut rig, 1, 21, 0, false, None, Unit::new(Dsp::Gain(1.0), &p));
    put(&mut rig, 1, 22, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    assert!(rig.port(1).move_to(22, 0));
    put(&mut rig, 1, 23, 5, false, None, Unit::new(Dsp::Level(0.0), &p));
    rig.advance(128);
    assert_eq!(answers(&rig, 0).last(), Some(&(F0, 9, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Full))));
    assert_eq!(answers(&rig, 0).len(), 9, "each message answered once");
    let mut kept = Vec::new();
    let (ninth, why) = rig.port(0).refused(9).expect("the ninth comes back");
    assert_eq!(why, SeatRefusal::Full);
    kept.push(ninth);
    let one = answers(&rig, 1).iter().map(|a| (a.1, a.2, a.3)).collect::<Vec<_>>();
    assert_eq!(
        one,
        vec![
            (20, SeatOp::Install, SeatOutcome::Applied),
            (21, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Placement)),
            (22, SeatOp::Install, SeatOutcome::Applied),
            (22, SeatOp::Move, SeatOutcome::Refused(SeatRefusal::Placement)),
            (23, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Placement)),
        ],
        "an effect ahead of the source, a move there and a second source unnamed in `replaces`"
    );
    let back = take_returns(&mut rig, 1, &mut kept);
    assert_eq!(back, vec![(21, false, Some(SeatRefusal::Placement)), (23, false, Some(SeatRefusal::Placement))]);
    assert_eq!(ids(&rig.engine.chain(1).collect::<Vec<_>>()), vec![20, 22]);
    assert_eq!(p.stops(), 0, "a refused unit is never started or stopped");
    rig.engine.evict_slots();
    assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 8);
    assert_eq!(take_returns(&mut rig, 1, &mut kept).len(), 2);
    assert_eq!(p.drops(), 0);
}

/// A source installed at any index lands at the head; a second one that `replaces` it takes its seat in
/// one crossfade from the old unit's output to the new one's, and the old unit stops and comes back once
/// it ends.
#[test]
fn a_source_lands_at_the_head_and_replaces_the_seated_one_in_one_crossfade() {
    for block in BLOCKS {
        let (pa, pb, pf) = (Probe::new(), Probe::new(), Probe::new());
        let mut rig = rig(block);
        put(&mut rig, 1, 10, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pf));
        rig.advance(1000);
        put(&mut rig, 1, 11, 3, false, None, Unit::new(Dsp::Level(0.25), &pa));
        rig.advance(2000);
        assert_eq!(ids(&rig.engine.chain(1).collect::<Vec<_>>()), vec![11, 10], "block {block}: the source at the head");
        assert_eq!(rig.engine.slot(1).map(|s| s.0), Some(SlotKind::Instrument));
        let s = rig.frame;
        put(&mut rig, 1, 12, 6, false, Some(11), Unit::new(Dsp::Level(0.5), &pb));
        rig.keep_output();
        rig.advance(1500);
        assert_frames(&rig.bus, s, |f| 2.0 * xfade(0.25, 0.5, (f - s).min(FADE as Frame) as u32), &format!("block {block}: one crossfade"));
        assert_eq!(answers(&rig, 1).last(), Some(&(s, 12, SeatOp::Install, SeatOutcome::Applied)));
        assert_eq!(ids(&chains(&rig, 1).last().unwrap().1), vec![12, 10]);
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 1, &mut kept), vec![(11, false, None)], "block {block}: the old source back");
        let calls = pa.calls();
        let end = calls.last().map(|c| c.0 + c.1 as Frame).unwrap();
        assert!(end >= s + FADE as Frame && end < s + FADE as Frame + block as Frame, "block {block}: called through the crossfade's end, its call's last frame");
        assert_eq!((pa.stops(), pa.seen().last()), (1, Some(&Seen::Stop)), "block {block}: stopped once, after its last call");
        rig.engine.evict_slots();
        assert_eq!(take_returns(&mut rig, 1, &mut kept).len(), 2);
        assert_eq!([pa.drops(), pb.drops(), pf.drops()], [0, 0, 0]);
    }
}

/// A source removed from a live slot: its held note released on the removal's frame, the seat faded to
/// the slot's input, the unit stopped once after its last call and back under its id; the slot stays
/// live (its input passes, no live toggle). An effect removed mid-chain fades its seat to its input.
#[test]
fn a_removal_releases_notes_stops_once_returns_under_its_id_and_leaves_live() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = rig(block);
        rig.set_level(0.125);
        put(&mut rig, 0, 4, 2, false, None, Unit::new(Dsp::Level(0.25), &p));
        rig.set(Command::SelectInstrument(NoteTarget::Slot(0)));
        rig.advance(500);
        rig.set(Command::NoteOn(60, 1.0));
        rig.advance(1000);
        let s = rig.frame;
        assert!(rig.port(0).remove(4));
        rig.keep_output();
        rig.advance(1000);
        let what = format!("block {block}");
        assert_frames(&rig.bus, s, |f| if f - s < FADE as Frame { xfade(0.0, 0.25, FADE - (f - s) as u32) } else { 0.0 }, &format!("{what}: the source fades out"));
        assert_frames(&rig.monitor, s, |f| if f - s < FADE as Frame { xfade(0.125, 0.0, FADE - (f - s) as u32) } else { 0.125 }, &format!("{what}: the input passes"));
        let seen = p.seen();
        let notes: Vec<(Frame, SlotEventKind)> = seen.iter().filter_map(|e| if let Seen::Note { frame, kind } = *e { Some((frame, kind)) } else { None }).collect();
        assert_eq!(notes.last(), Some(&(s, SlotEventKind::NoteOff { key: 60 })), "{what}: released on the removal's frame");
        assert_eq!((p.stops(), seen.last()), (1, Some(&Seen::Stop)), "{what}");
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(4, false, None)], "{what}");
        assert!(!rig.events.iter().any(|e| matches!(*e, Event::Toggled { toggle: lf_engine::Toggle::Live(0), frame, .. } if frame >= s)), "{what}: LIVE untouched");
        assert_eq!(rig.engine.slot(0), None);

        // Mid-chain: ×2, ×3, ×5, the ×3 removed.
        let q = Probe::new();
        for (id, g) in [(1, 2.0), (2, 3.0), (3, 5.0)] {
            put(&mut rig, 0, id, MAX_UNITS, false, None, Unit::new(Dsp::Gain(g), &q));
        }
        rig.advance(2000);
        let s = rig.frame;
        assert!(rig.port(0).remove(2));
        rig.keep_output();
        rig.advance(1000);
        let x = 0.125f32 * 2.0;
        assert_frames(&rig.monitor, s, |f| 5.0 * xfade(x, 3.0 * x, FADE.saturating_sub((f - s) as u32)), &format!("{what}: mid-chain"));
        assert_eq!(ids(&chains(&rig, 0).last().unwrap().1), vec![1, 3]);
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, None)], "{what}");
        assert_eq!(q.stops(), 1);
        rig.engine.evict_slots();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
        assert_eq!((p.drops(), q.drops()), (0, 0));
    }
}

/// A withdrawn seat (×2, then the 64-frame ×3 withdrawn, then ×5) fades to its input delayed by its
/// latency and passes it, in place; an install under its id reseats it, faded in from that bypass; a
/// source under its id cannot reseat a mid-chain seat and parks it; a removal takes the parked seat out.
#[test]
fn a_withdrawn_seat_passes_its_input_at_its_last_latency_until_reseated_or_parked() {
    for block in BLOCKS {
        let (p, w) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        rig.set_input(code);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &p));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(64, 3.0), &w));
        put(&mut rig, 0, 3, MAX_UNITS, false, None, Unit::new(Dsp::Gain(5.0), &p));
        rig.advance(3000);
        let what = format!("block {block}");
        let x = |f: Frame| 2.0 * code(f);
        let mut kept = Vec::new();

        // Withdrawn.
        let s = rig.frame;
        assert!(rig.port(0).withdraw(2));
        rig.keep_output();
        rig.advance(1000);
        assert_frames(&rig.monitor, s, |f| 5.0 * xfade(x(f - 64), 3.0 * x(f - 64), FADE.saturating_sub((f - s) as u32)), &format!("{what}: withdrawn"));
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, None)], "{what}");
        assert_eq!((w.stops(), w.seen().last()), (1, Some(&Seen::Stop)), "{what}");
        let last = chains(&rig, 0).last().unwrap().1.clone();
        assert_eq!((ids(&last), last[1].latency), (vec![1, 2, 3], 64), "{what}: the seat keeps its place and latency");
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 64)));

        // Reseated: a fresh unit (its own line empty) fades in from the bypass.
        let w2 = Probe::new();
        let s = rig.frame;
        put(&mut rig, 0, 2, 0, false, None, Unit::new(Dsp::Lag(64, 3.0), &w2));
        rig.keep_output();
        rig.advance(1000);
        let core = |f: Frame| if f - s < 64 { 0.0 } else { 3.0 * x(f - 64) };
        assert_frames(&rig.monitor, s, |f| 5.0 * xfade(x(f - 64), core(f), (f - s).min(FADE as Frame) as u32), &format!("{what}: reseated"));
        assert_eq!(answers(&rig, 0).last(), Some(&(s, 2, SeatOp::Install, SeatOutcome::Applied)));
        assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1, 2, 3], "{what}: in its place, whatever the index");

        // Parked: withdrawn again, and a source under its id is refused.
        assert!(rig.port(0).withdraw(2));
        rig.advance(1000);
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, None)], "{what}");
        let s = rig.frame;
        let chains_before = chains(&rig, 0).len();
        put(&mut rig, 0, 2, 1, false, None, Unit::new(Dsp::Level(0.5), &w2));
        rig.keep_output();
        rig.advance(1000);
        assert_eq!(answers(&rig, 0).last(), Some(&(s, 2, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Placement))));
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, Some(SeatRefusal::Placement))], "{what}");
        assert_frames(&rig.monitor, s, |f| 5.0 * x(f - 64), &format!("{what}: parked"));
        assert_eq!(chains(&rig, 0).len(), chains_before, "{what}: the chain unchanged");

        // Removed: the parked seat fades out to its undelayed input; nothing comes back.
        let s = rig.frame;
        assert!(rig.port(0).remove(2));
        rig.keep_output();
        rig.advance(1000);
        assert_frames(&rig.monitor, s, |f| 5.0 * xfade(x(f), x(f - 64), FADE.saturating_sub((f - s) as u32)), &format!("{what}: removed"));
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![], "{what}");
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 0)));
        assert_eq!(w2.stops(), 1, "{what}: the reseated unit stopped once, the refused one never");
        rig.engine.evict_slots();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
        assert_eq!((p.drops(), w.drops(), w2.drops()), (0, 0, 0));
    }
}

/// An eviction mid-take hands back every seat with its bypass, a held install too, each stopped once.
#[test]
fn an_eviction_returns_every_seat_with_its_bypass_and_every_waiting_install() {
    let p = Probe::new();
    let mut rig = rig(128);
    put(&mut rig, 0, 1, MAX_UNITS, true, None, Unit::new(Dsp::Lag(10, 1.0), &p));
    put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    put(&mut rig, 0, 3, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    put(&mut rig, 1, 4, MAX_UNITS, false, None, Unit::new(Dsp::Level(0.0), &p));
    rig.advance(1000);
    rig.set(Command::SetUnitBypass(3, true));
    arm(&mut rig, 1.0);
    rig.advance(1000);
    put(&mut rig, 0, 5, MAX_UNITS, false, None, Unit::new(Dsp::Lag(30, 1.0), &p));
    rig.advance(256);
    assert!(rig.port(0).holding(), "the latent install waits for the take");
    rig.engine.evict_slots();
    let mut kept = Vec::new();
    let mut zero = take_returns(&mut rig, 0, &mut kept);
    zero.sort_by_key(|r| r.0);
    assert_eq!(zero, vec![(1, true, None), (2, false, None), (3, true, None), (5, false, None)]);
    assert_eq!(take_returns(&mut rig, 1, &mut kept), vec![(4, false, None)]);
    assert_eq!(p.stops(), 5, "each stopped once");
    assert_eq!((rig.engine.slot(0), rig.engine.slot(1)), (None, None));
    assert_eq!(p.drops(), 0, "the engine dropped none");
    drop(kept);
    assert_eq!(p.drops(), 5);
}

// ── Latency ──────────────────────────────────────────────────────────────────────────────────────────

/// 5 + 7 + 11 reads 23, bypassed units included; two live chains align a take on the larger.
#[test]
fn latencies_sum_within_a_chain_and_two_live_chains_align_on_the_larger() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = rig(block);
        for (id, lat) in [(1, 5), (2, 7), (3, 11)] {
            put(&mut rig, 0, id, MAX_UNITS, false, None, Unit::new(Dsp::Lag(lat, 1.0), &p));
        }
        put(&mut rig, 1, 4, MAX_UNITS, false, None, Unit::new(Dsp::Lag(40, 1.0), &p));
        rig.advance(1000);
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 23)), "block {block}");
        let shown: Vec<u32> = chains(&rig, 0).last().unwrap().1.iter().map(|s| s.latency).collect();
        assert_eq!(shown, vec![5, 7, 11]);
        rig.set(Command::SetUnitBypass(2, true));
        rig.advance(1000);
        assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 23)), "block {block}: a bypassed unit counts");
        let x = rig.frame + 100;
        rig.set_input(move |f| if f == x { 1.0 } else { 0.0 });
        let from = rig.frame;
        rig.keep_output();
        rig.advance(500);
        assert_eq!(nonzero(&rig.monitor), vec![(x + 23 - from) as usize], "block {block}: heard 23 late, bypass or not");

        rig.set(Command::SetSlotLive(1, true));
        rig.set_inputs(|_| 0.0, |_| 0.0);
        let start = arm(&mut rig, 1.0);
        assert_eq!(rig.engine.live_latency(), 40, "block {block}");
        let x = start + 3000;
        rig.set_inputs(move |f| if f == x { 1.0 } else { 0.0 }, move |f| if f == x { 0.5 } else { 0.0 });
        rig.keep_output();
        let from = rig.frame;
        rig.advance_to(rig.end_frame() + 1);
        let pcm = rig.pcm(0);
        assert_eq!(nonzero(&pcm), vec![3040], "block {block}: both on one loop frame");
        assert_eq!(pcm[3040], 1.5);
        let heard: Vec<usize> = nonzero(&rig.monitor);
        assert_eq!(heard, vec![(x + 23 - from) as usize, (x + 40 - from) as usize], "block {block}: each heard at its own");
        rig.engine.evict_slots();
    }
}

// ── A proptest of edit scripts ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Step {
    /// A fresh unit: a source or an effect `lat` frames late at a gain, at an index, bypassed or not.
    Add { slot: usize, source: bool, lat: usize, gain: f32, at: usize, bypassed: bool },
    /// The `k`-th id minted so far (mod their count).
    Remove(usize),
    Move(usize, usize),
    /// A stamped bypass toggle this many frames into the next wait.
    Bypass(usize, Frame),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0usize..2, prop::bool::weighted(0.2), 0usize..40, prop_oneof![Just(0.5f32), Just(1.0), Just(-1.5)], 0usize..10, any::<bool>())
            .prop_map(|(slot, source, lat, gain, at, bypassed)| Step::Add { slot, source, lat, gain, at, bypassed }),
        2 => any::<usize>().prop_map(Step::Remove),
        2 => (any::<usize>(), 0usize..9).prop_map(|(k, to)| Step::Move(k, to)),
        3 => (any::<usize>(), 0 as Frame..GRID).prop_map(|(k, at)| Step::Bypass(k, at)),
    ]
}

/// Port messages land at block starts: the script sends each on this grid, a common multiple of both
/// block sizes, and waits that long (past every crossfade) before the next, so every message applies
/// on the same frame at either size. The bypass commands are stamped anywhere.
const GRID: Frame = 1024;
const SCRIPT_BLOCKS: [usize; 2] = [64, 1024];

struct Outcome {
    heard: [Vec<f32>; 2],
    /// Each minted id: how many times it came back.
    returned: HashMap<UnitId, usize>,
    minted: Vec<UnitId>,
    /// Port messages sent, and the (unit, op) of each.
    messages: Vec<(UnitId, SeatOp)>,
    answered: Vec<(UnitId, SeatOp)>,
    drops: usize,
}

fn run_script(block: usize, steps: &[Step]) -> Outcome {
    let p = Probe::new();
    let mut rig = Rig::with(Opts { block, sr: 48_000, start: 48 * GRID, ..Opts::default() });
    rig.set_inputs(|f| ((f % 97) as f32 - 48.0) / 64.0, |f| ((f % 61) as f32 - 30.0) / 40.0);
    rig.set(Command::SetSlotLive(1, true));
    rig.advance_to(49 * GRID);
    rig.keep_output();
    let mut minted: Vec<(UnitId, usize)> = Vec::new();
    let mut messages = Vec::new();
    let mut returned: HashMap<UnitId, usize> = HashMap::new();
    let mut kept: Vec<Box<dyn UnitProcessor>> = Vec::new();
    let mut back = |rig: &mut Rig, kept: &mut Vec<Box<dyn UnitProcessor>>| {
        for slot in 0..2 {
            for (id, _, _) in take_returns(rig, slot, kept) {
                *returned.entry(id).or_default() += 1;
            }
        }
    };
    for s in steps {
        assert_eq!(rig.frame % GRID, 0);
        let pick = |k: usize| (!minted.is_empty()).then(|| minted[k % minted.len()]);
        match *s {
            Step::Add { slot, source, lat, gain, at, bypassed } => {
                let id = 100 + minted.len() as UnitId;
                let unit = if source { Unit::source_lat(gain, lat, &p) } else { Unit::new(Dsp::Lag(lat, gain), &p) };
                put(&mut rig, slot, id, at, bypassed, None, unit);
                minted.push((id, slot));
                messages.push((id, SeatOp::Install));
            }
            Step::Remove(k) => {
                if let Some((id, slot)) = pick(k) {
                    assert!(rig.port(slot).remove(id));
                    messages.push((id, SeatOp::Remove));
                }
            }
            Step::Move(k, to) => {
                if let Some((id, slot)) = pick(k) {
                    assert!(rig.port(slot).move_to(id, to));
                    messages.push((id, SeatOp::Move));
                }
            }
            Step::Bypass(k, at) => {
                if let Some((id, _)) = pick(k) {
                    rig.send_at(rig.frame + at, Command::Action(lf_engine::Action::ToggleUnit(id)));
                }
            }
        }
        rig.advance(GRID);
        back(&mut rig, &mut kept);
    }
    rig.advance(GRID);
    let drops = p.drops();
    rig.engine.evict_slots();
    back(&mut rig, &mut kept);
    let answered = (0..2u8).flat_map(|s| answers(&rig, s)).map(|a| (a.1, a.2)).collect();
    Outcome { heard: [rig.heard.clone(), rig.heard_right.clone()], returned, minted: minted.iter().map(|m| m.0).collect(), messages, answered, drops }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Any script of adds, moves, removals and bypasses: finite, bit-identical at two block sizes, every
    /// unit back exactly once (none dropped by the engine), every port message answered exactly once.
    #[test]
    fn edit_scripts_are_finite_bit_identical_and_answer_and_return_everything_once(steps in prop::collection::vec(step(), 1..24)) {
        let [a, b] = SCRIPT_BLOCKS.map(|block| run_script(block, &steps));
        for o in [&a, &b] {
            prop_assert!(o.heard.iter().flatten().all(|y| y.is_finite()), "finite");
            prop_assert_eq!(o.drops, 0, "the engine dropped a unit");
            for id in &o.minted {
                prop_assert_eq!(o.returned.get(id).copied(), Some(1), "unit {} back once", id);
            }
            prop_assert_eq!(o.returned.len(), o.minted.len());
            let (mut sent, mut got) = (o.messages.clone(), o.answered.clone());
            sent.sort_by_key(|m| (m.0, m.1 as u8));
            got.sort_by_key(|m| (m.0, m.1 as u8));
            prop_assert_eq!(sent, got, "every message answered once");
        }
        prop_assert!(a.heard[0].iter().zip(&b.heard[0]).chain(a.heard[1].iter().zip(&b.heard[1])).all(|(x, y)| x.to_bits() == y.to_bits()), "bit-identical across block sizes");
        prop_assert_eq!(a.heard[0].len(), b.heard[0].len());
    }
}

// ── Owed replies ─────────────────────────────────────────────────────────────────────────────────────

/// One reply of a slot's raw stream, in the order the event ring gave them: an [`Event::Seat`], or an
/// [`Event::Chain`] as (unit, bypassed) per seat.
#[derive(Clone, Debug, PartialEq)]
enum Reply {
    Seat(Frame, UnitId, SeatOp, SeatOutcome),
    Chain(Frame, Vec<(UnitId, bool)>),
}

fn replies(rig: &Rig, slot: u8) -> Vec<Reply> {
    rig.events
        .iter()
        .filter_map(|e| match *e {
            Event::Seat { frame, slot: s, unit, op, outcome } if s == slot => Some(Reply::Seat(frame, unit, op, outcome)),
            Event::Chain { frame, slot: s, len, seats } if s == slot => Some(Reply::Chain(frame, seats[..len as usize].iter().map(|c| (c.unit, c.bypassed)).collect())),
            _ => None,
        })
        .collect()
}

fn bypasses(p: &Probe) -> usize {
    p.seen().iter().filter(|s| matches!(s, Seen::Bypass { .. })).count()
}

fn toggle_unit(id: UnitId) -> Command {
    Command::Action(lf_engine::Action::ToggleUnit(id))
}

/// Bypass commands past the room among what a slot owes, all on one frame, the event ring with room:
/// each sends its own chain on that frame, in order (what the slot owes is offered between them).
#[test]
fn bypass_commands_past_the_owed_room_on_one_frame_each_send_their_chain() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = rig(block);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &p));
        rig.advance(1000);
        let t = rig.frame + 7;
        for _ in 0..34 {
            rig.send_at(t, toggle_unit(1));
        }
        rig.advance(2000);
        let what = format!("block {block}");
        let got: Vec<Reply> = replies(&rig, 0).into_iter().filter(|r| matches!(r, Reply::Chain(f, _) if *f >= t)).collect();
        let want: Vec<Reply> = (0..34).map(|k| Reply::Chain(t, vec![(1, k % 2 == 0)])).collect();
        assert_eq!(got, want, "{what}: 34 chains on the commands' frame, in order");
        assert_eq!(bypasses(&p), 34, "{what}");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 1);
    }
}

/// An event ring that stopped reading: bypass commands apply only while their chains have a place among
/// what the slot owes, the rest wait in order, and once the ring drains every one applies and sends its
/// chain, none lost.
#[test]
fn a_stalled_event_ring_holds_bypass_commands_back_and_loses_none() {
    for block in BLOCKS {
        let p = Probe::new();
        let mut rig = Rig::with_event_capacity(Opts { block, ..Opts::default() }, 4);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &p));
        rig.advance(1000);
        rig.hold_events = true;
        let t = rig.frame + 7;
        for _ in 0..48 {
            rig.send_at(t, toggle_unit(1));
        }
        rig.advance(1100);
        let what = format!("block {block}");
        let held = bypasses(&p);
        assert!(held < 48, "{what}: application held back ({held} applied)");
        rig.hold_events = false;
        rig.read_events();
        // The ring takes at most 4 a block.
        for _ in 0..64 {
            rig.advance(block as Frame);
        }
        let got: Vec<(Frame, bool)> = replies(&rig, 0).into_iter().filter_map(|r| if let Reply::Chain(f, s) = r { (f >= t).then(|| (f, s[0].1)) } else { None }).collect();
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), (0..48).map(|k| k % 2 == 0).collect::<Vec<_>>(), "{what}: every value, in order");
        assert!(got.windows(2).all(|w| w[0].0 <= w[1].0) && got[0].0 == t, "{what}: in frame order from the commands' frame");
        assert_eq!(bypasses(&p), 48, "{what}");
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 1);
    }
}

/// A port edit's chain is owed with its answer, never folded into a later change's: whatever depth bypass
/// commands filled what the slot owes on a stalled ring, an install and a bypass after it each send
/// their own chain, the install's right after its answer.
#[test]
fn a_port_edits_chain_is_its_own_whatever_the_slot_owes() {
    for block in [1, 128, 480] {
        for n in 20..44 {
            let p = Probe::new();
            let mut rig = Rig::with_event_capacity(Opts { block, ..Opts::default() }, 4);
            put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &p));
            rig.advance(1000);
            rig.hold_events = true;
            let t = rig.frame + 3;
            for _ in 0..n {
                rig.send_at(t, toggle_unit(1));
            }
            rig.advance(block as Frame + 3);
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
            rig.send_at(rig.frame + 1, toggle_unit(1));
            rig.advance(1000);
            rig.hold_events = false;
            rig.read_events();
            // The ring takes at most 4 a block.
            for _ in 0..64 {
                rig.advance(block as Frame);
            }
            let what = format!("block {block}, {n} bypass commands");
            let r = replies(&rig, 0);
            assert_eq!(r.iter().filter(|x| matches!(x, Reply::Chain(..))).count(), 1 + n + 1 + 1, "{what}: one chain per change");
            let k = r.iter().position(|x| matches!(x, Reply::Seat(_, 2, SeatOp::Install, SeatOutcome::Applied))).expect("the install was answered");
            let Reply::Seat(f2, ..) = r[k] else { unreachable!() };
            match &r[k + 1] {
                Reply::Chain(f, seats) => assert_eq!((*f, seats.iter().map(|s| s.0).collect::<Vec<_>>()), (f2, vec![1, 2]), "{what}: the install's own chain"),
                other => panic!("{what}: the install's answer is followed by {other:?}"),
            }
            rig.engine.evict_slots();
            let mut kept = Vec::new();
            assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2);
        }
    }
}

/// An event ring that frees one entry per block, slot 0 owing a new answer every block: slot 1's answer
/// still gets its turn (the first offer goes round the slots).
#[test]
fn a_slot_owing_an_answer_every_block_starves_no_other_slot() {
    for block in BLOCKS {
        let mut rig = Rig::with_event_capacity(Opts { block, ..Opts::default() }, 1);
        assert!(rig.port(1).remove(98));
        for _ in 0..24 {
            assert!(rig.port(0).remove(99));
            rig.advance(block as Frame);
        }
        let what = format!("block {block}");
        let got: Vec<(UnitId, SeatOp, SeatOutcome)> = answers(&rig, 1).into_iter().map(|a| (a.1, a.2, a.3)).collect();
        assert_eq!(got, vec![(98, SeatOp::Remove, SeatOutcome::Refused(SeatRefusal::Unknown))], "{what}: slot 1 answered");
        assert!(answers(&rig, 0).len() > 2, "{what}: slot 0's answers flow too");
    }
}

/// One block as the device callback renders it, under `catch_unwind` (its guard), silent input: false
/// on a panic. Not under `assert_no_alloc`: the unwind allocates.
fn guarded_block(rig: &mut Rig) -> bool {
    let n = rig.block;
    let ctx = ProcessContext { frame: rig.frame, xrun: false, damaged: false, align_frames: rig.align - rig.engine.limiter_latency(), input_frames: 0 };
    let (input, mut left, mut right) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let engine = &mut rig.engine;
    let ok = catch_unwind(AssertUnwindSafe(|| engine.process(&ctx, &input, &mut left, &mut right))).is_ok();
    rig.frame += n as Frame;
    ok
}

/// A unit that panics in its first call after a move's reorder: the move's answer and its chain were
/// owed before any unit was called in the new order, so the owner's eviction of the faulted engine
/// delivers them, in order, before the eviction's own chain.
#[test]
fn a_moves_answer_and_chain_survive_a_panic_right_after_its_reorder() {
    for block in BLOCKS {
        let (pg, pc) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        let at = F0 + 5000 + FADE as Frame;
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Clip(0.3), &pc).panicking_from(at));
        rig.advance(5000);
        assert!(rig.port(0).move_to(2, 0));
        let what = format!("block {block}");
        let blocks = (0..2 * FADE as usize / block + 2).take_while(|_| guarded_block(&mut rig)).count();
        assert!(blocks <= 2 * FADE as usize / block, "{what}: the unit panicked");
        assert!(rig.frame > at && rig.frame - (block as Frame) <= at, "{what}: in the reorder's block");
        assert!(catch_unwind(AssertUnwindSafe(|| rig.engine.evict_slots())).is_ok());
        rig.read_events();
        let r = replies(&rig, 0);
        let k = r.iter().position(|x| matches!(x, Reply::Seat(_, 2, SeatOp::Move, _))).expect("the move was answered");
        assert_eq!(r[k..k + 2], [Reply::Seat(at, 2, SeatOp::Move, SeatOutcome::Applied), Reply::Chain(at, vec![(2, false), (1, false)])], "{what}: its answer, then its chain");
        assert!(matches!(&r[k + 2..], [Reply::Chain(_, empty)] if empty.is_empty()), "{what}: then the eviction's chain alone: {:?}", &r[k + 2..]);
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2, "{what}");
        assert_eq!((pg.drops(), pc.drops()), (0, 0), "{what}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ending {
    /// The take ends: nothing captures on.
    Take,
    /// A RETAKE roll's pass edge: the next pass captures on.
    RetakePass,
    /// RETAKE's REC on another lane: that lane captures on from the pass edge.
    Handoff,
}

/// A fresh edit that moves the alignment, sent for the block that starts where a capture ends: the end
/// is resolved before the port is served, so the edit applies there with one `Applied`. A RETAKE pass
/// edge or a handoff to another lane there hands the capture on: the edit waits, answered `Waiting` once.
#[test]
fn a_fresh_edit_on_a_captures_end_frame_applies_unless_the_capture_goes_on() {
    for ending in [Ending::Take, Ending::RetakePass, Ending::Handoff] {
        for block in BLOCKS {
            let p = Probe::new();
            let mut rig = rig(block);
            put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p));
            rig.set(Command::SetRetake(ending != Ending::Take));
            arm(&mut rig, 1.0);
            let mut end = rig.end_frame();
            if ending == Ending::Handoff {
                rig.advance_to(end + 1000);
                rig.press(Command::RecDub(1));
                end = rig.end_frame();
            }
            rig.advance_to(end);
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
            rig.advance(block as Frame);
            let what = format!("block {block}, {ending:?}");
            let got: Vec<(Frame, UnitId, SeatOp, SeatOutcome)> = answers(&rig, 0).into_iter().filter(|a| a.1 == 2).collect();
            if ending == Ending::Take {
                assert_eq!(got, vec![(end, 2, SeatOp::Install, SeatOutcome::Applied)], "{what}: applied once, on the take's end");
                assert!(!rig.port(0).holding(), "{what}");
                assert_eq!(chains(&rig, 0).last().map(|c| (c.0, ids(&c.1))), Some((end, vec![1, 2])), "{what}");
            } else {
                assert_eq!(got, vec![(end, 2, SeatOp::Install, SeatOutcome::Waiting)], "{what}: waiting, once");
                assert!(rig.port(0).holding(), "{what}");
                let lane = if ending == Ending::Handoff { 1 } else { 0 };
                assert_eq!(rig.window().map(|w| (w.0, w.1)), Some((lane, Some(end))), "{what}: the capture goes on from there");
            }
            rig.engine.evict_slots();
            drop(rig);
            assert_eq!(p.drops(), 2, "{what}: every unit back to the port, none dropped before");
        }
    }
}

/// AUTO listening on slot 0 (a 17-frame chain) beside live slot 1 (a 100-frame chain, silent): the
/// alignment is slot 1's 100 frames throughout, and on `e` slot 0's chain becomes 67 frames, its
/// compensation at the record tap 83 → 33. Slot 0's input, a quarter on the right: with `burst`, a loud
/// attack its first chain has passed into its compensation line before `e`; a soft onset (0.01) from
/// `e + soft`, 0.2 from `e + loud`. The take is stopped at `e + 4000`. Returns whether AUTO still listened
/// at `e + 900`, the take's start and its loop.
fn auto_beside_a_longer_chain(block: usize, burst: bool, soft: Frame, loud: Frame) -> (Frame, bool, Frame, Vec<Pair>) {
    let p = Probe::new();
    let mut rig = rig(block);
    put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(17, 1.0), &p));
    put(&mut rig, 1, 3, MAX_UNITS, false, None, Unit::new(Dsp::Lag(100, 1.0), &p));
    rig.set(Command::SetSlotLive(1, true));
    rig.set(Command::SetAutoRecord(true));
    rig.press(Command::RecDub(0));
    assert!(rig.lane(0).auto_armed);
    let e = rig.frame + 3000;
    let x = move |f: Frame| if burst && (e - 50..e - 17).contains(&f) { 0.5 } else if f >= e + loud { 0.2 } else if f >= e + soft { 0.01 } else { 0.0 };
    rig.set_stereo_inputs(move |f| [x(f), 0.25 * x(f)], |_| [0.0; 2]);
    rig.advance_to(e);
    assert_eq!(rig.engine.live_latency(), 100);
    put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Lag(50, 1.0), &p));
    rig.advance(900);
    let listening = rig.lane(0).auto_armed;
    rig.advance_to(e + 4000);
    assert_eq!(rig.engine.live_latency(), 100);
    assert!(!rig.lane(0).auto_armed, "the onset triggered");
    let start = rig.start_frame();
    rig.press(Command::RecDub(0));
    let stop = rig.frame + 5 * 48_000;
    while rig.state(0) != LaneState::Playing && rig.frame < stop {
        rig.advance(block as Frame);
    }
    rig.idle();
    assert_eq!(rig.state(0), LaneState::Playing);
    let out = (e, listening, start, rig.pcm2(0));
    assert!(out.3.len() > 2000, "the take holds the onset");
    rig.engine.evict_slots();
    drop(rig);
    assert_eq!(p.drops(), 3);
    out
}

/// AUTO listening across a change of one path's compensation at the record tap, the alignment itself
/// unchanged (another live slot holds it): what it retained before is cut, and what that path's line
/// held from before the change never reaches it. An earlier loud attack still in the line leaves no
/// trace (no trigger, the same take frame for frame, both sides), and a soft onset begun before the
/// change is retained only from the change on.
#[test]
fn auto_listening_across_one_paths_compensation_change_keeps_nothing_from_before_it() {
    for block in BLOCKS {
        let what = format!("block {block}");
        let (e, listening, start, take) = auto_beside_a_longer_chain(block, true, 1000, 1300);
        let (_, quiet_listening, quiet_start, quiet_take) = auto_beside_a_longer_chain(block, false, 1000, 1300);
        assert!(listening && quiet_listening, "{what}: the attack from before the change triggers nothing");
        assert!(start > e + 600, "{what}: the take starts on the onset, at {start} (the change at {e})");
        assert_eq!((start, take.len()), (quiet_start, quiet_take.len()), "{what}: the same take as with no attack");
        assert!(take.iter().zip(&quiet_take).all(|(a, b)| a[0].to_bits() == b[0].to_bits() && a[1].to_bits() == b[1].to_bits()), "{what}: frame for frame, both sides");
        assert!(take.iter().any(|s| s[1] != s[0]) && take.iter().all(|s| s[0].abs() < 0.3), "{what}: a stereo take, no attack in it");
        let (e, _, start, _) = auto_beside_a_longer_chain(block, false, -300, 500);
        assert!(start >= e, "{what}: the soft onset retained from the change on, not from {start} (the change at {e})");
    }
}

/// A port message sent while a move dips, before a device jump past its reorder: the reorder applies
/// where the device resumes, before the port is served, so the message applies in that block.
#[test]
fn a_message_sent_before_a_jump_past_a_reorder_applies_in_the_resumed_block() {
    for block in BLOCKS {
        let (pg, pc) = (Probe::new(), Probe::new());
        let mut rig = rig(block);
        put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
        put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Clip(0.3), &pc));
        rig.advance(5000);
        assert!(rig.port(0).move_to(2, 0));
        rig.advance(64);
        assert!(rig.port(0).remove(1));
        rig.skip(512);
        let r = rig.frame;
        rig.advance(block as Frame);
        let what = format!("block {block}");
        let got: Vec<Reply> = replies(&rig, 0).into_iter().filter(|x| !matches!(x, Reply::Seat(_, _, SeatOp::Install, _) | Reply::Chain(F0, _))).collect();
        let want = vec![
            Reply::Seat(r, 2, SeatOp::Move, SeatOutcome::Applied),
            Reply::Chain(r, vec![(2, false), (1, false)]),
            Reply::Seat(r, 1, SeatOp::Remove, SeatOutcome::Applied),
            Reply::Chain(r, vec![(2, false)]),
        ];
        assert_eq!(got, want, "{what}: the reorder, then the removal, both where the device resumed");
        rig.advance(1000);
        rig.engine.evict_slots();
        let mut kept = Vec::new();
        assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2, "{what}");
    }
}

/// A slot's raw reply stream through two installs, a move and a removal, read after every block (and
/// with the event ring stalled across the reorder): each message's answer before its chain, nothing
/// twice, a dipping move's both on its reorder, each delivered by the end of the block it is dated in
/// whenever the ring has room, and never dated past it.
#[test]
fn a_slots_replies_arrive_in_order_once_and_by_the_end_of_their_block() {
    for stall in [false, true] {
        for block in BLOCKS {
            let (pg, pc) = (Probe::new(), Probe::new());
            let mut rig = rig(block);
            put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(2.0), &pg));
            put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Clip(0.3), &pc));
            let what = format!("block {block}, stalled {stall}");
            let step = |rig: &mut Rig| {
                rig.advance(block as Frame);
                if !rig.hold_events {
                    let r = replies(rig, 0);
                    assert!(r.iter().all(|x| match x {
                        Reply::Seat(f, ..) | Reply::Chain(f, _) => *f < rig.frame,
                    }), "{what}: a reply dated past the block it came with");
                    let pending = rig.port(0).holding();
                    assert!(!pending, "{what}");
                }
            };
            while rig.frame < F0 + 5000 {
                step(&mut rig);
            }
            let s = rig.frame;
            assert!(rig.port(0).move_to(2, 0));
            let at = s + FADE as Frame;
            rig.hold_events = stall;
            while rig.frame <= at {
                step(&mut rig);
                if !stall && rig.frame > at {
                    assert!(replies(&rig, 0).contains(&Reply::Seat(at, 2, SeatOp::Move, SeatOutcome::Applied)), "{what}: answered by the end of the reorder's block");
                }
            }
            while rig.frame < at + 2 * FADE as Frame {
                step(&mut rig);
            }
            rig.hold_events = false;
            rig.read_events();
            assert!(rig.port(0).remove(1));
            let t = rig.frame;
            step(&mut rig);
            let want = vec![
                Reply::Seat(F0, 1, SeatOp::Install, SeatOutcome::Applied),
                Reply::Chain(F0, vec![(1, false)]),
                Reply::Seat(F0, 2, SeatOp::Install, SeatOutcome::Applied),
                Reply::Chain(F0, vec![(1, false), (2, false)]),
                Reply::Seat(at, 2, SeatOp::Move, SeatOutcome::Applied),
                Reply::Chain(at, vec![(2, false), (1, false)]),
                Reply::Seat(t, 1, SeatOp::Remove, SeatOutcome::Applied),
                Reply::Chain(t, vec![(2, false)]),
            ];
            assert_eq!(replies(&rig, 0), want, "{what}: the raw stream");
            rig.advance(1000);
            rig.engine.evict_slots();
            let mut kept = Vec::new();
            assert_eq!(take_returns(&mut rig, 0, &mut kept).len(), 2, "{what}");
        }
    }
}
