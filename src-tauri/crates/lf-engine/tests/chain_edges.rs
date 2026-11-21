//! Bounded edge-case regressions for slot chains (`slots.rs`): latency-sum overflow, extreme
//! crossfade endpoints, panic recovery in `set_bypass` during install, 8th install while a removed
//! seat fades, cross-role `replaces` refusal, atomic ordering of `holding` vs `Ticket::answered`,
//! preserving latest applied bypass on same-id reseat/reload, and duplicate-id checks against
//! fading/outgoing instances.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use assert_no_alloc::assert_no_alloc;
use common::{code, violation_count, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{
    Action, ChainSeat, Command, Engine, EngineConfig, Event, Left, ProcessContext, SeatOp,
    SeatOutcome, SeatRefusal, SlotEvent, SlotKind, TimedCommand, UnitCx, UnitId, UnitInfo,
    UnitKind, UnitProcessor, MAX_UNITS,
};

const BLOCKS: [usize; 6] = [1, 32, 64, 128, 480, 1024];
/// Crossfade length at 48 kHz: 10 ms.
const FADE: u32 = 480;
const LOG: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Seen {
    Bypass { frame: Frame, on: bool },
    Stop,
}

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

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy, Debug)]
enum Dsp {
    Gain(f32),
    Lag(usize, f32),
    Level(f32),
}

struct Unit {
    dsp: Dsp,
    source: bool,
    reported_latency: Frame,
    self_bypass: bool,
    bypassed: bool,
    panic_set_bypass: bool,
    line: [Vec<f32>; 2],
    at: usize,
    probe: Probe,
}

impl Unit {
    fn new(dsp: Dsp, probe: &Probe) -> Box<Unit> {
        let lag = if let Dsp::Lag(n, _) = dsp { n } else { 0 };
        Box::new(Unit {
            dsp,
            source: matches!(dsp, Dsp::Level(_)),
            reported_latency: lag as Frame,
            self_bypass: false,
            bypassed: false,
            panic_set_bypass: false,
            line: [vec![0.0; lag], vec![0.0; lag]],
            at: 0,
            probe: probe.clone(),
        })
    }

    fn with_reported_latency(mut self: Box<Self>, latency: Frame) -> Box<Unit> {
        self.reported_latency = latency;
        self
    }

    fn self_bypassing(mut self: Box<Self>) -> Box<Unit> {
        self.self_bypass = true;
        self
    }

    fn initially_bypassed(mut self: Box<Self>) -> Box<Unit> {
        self.bypassed = true;
        self
    }

    fn panicking_in_set_bypass(mut self: Box<Self>) -> Box<Unit> {
        self.panic_set_bypass = true;
        self
    }
}

impl UnitProcessor for Unit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: self.source, latency: self.reported_latency }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let mut end = self.at;
        for (c, out) in out.into_iter().enumerate() {
            let mut at = self.at;
            for (y, &x) in out.iter_mut().zip(input[c]) {
                *y = match self.dsp {
                    Dsp::Gain(g) => g * x,
                    Dsp::Lag(0, g) => g * x,
                    Dsp::Lag(n, g) => {
                        let line = &mut self.line[c];
                        let d = line[at] * g;
                        line[at] = x;
                        at = (at + 1) % n;
                        d
                    }
                    Dsp::Level(l) => l,
                };
                if self.bypassed && self.self_bypass {
                    *y = x;
                }
            }
            end = at;
        }
        self.at = end;
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        if self.panic_set_bypass {
            panic!("the unit panics in set_bypass()");
        }
        let mut log = self.probe.log.lock().unwrap();
        if log.len() < LOG {
            log.push(Seen::Bypass { frame: cx.frame, on: bypassed });
        }
        self.bypassed = bypassed;
        self.self_bypass
    }

    fn stop(&mut self) {
        let mut log = self.probe.log.lock().unwrap();
        if log.len() < LOG {
            log.push(Seen::Stop);
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn rig(block: usize) -> Rig {
    Rig::with(Opts { block, ..Opts::default() })
}

fn put(rig: &mut Rig, slot: usize, id: UnitId, at: usize, bypassed: bool, replaces: Option<UnitId>, unit: Box<Unit>) -> lf_engine::Ticket {
    rig.port(slot).install_at(id, 0, at, bypassed, replaces, unit).unwrap_or_else(|_| panic!("slot {slot}'s message ring is full"))
}

fn answers(rig: &Rig, slot: u8) -> Vec<(Frame, UnitId, SeatOp, SeatOutcome)> {
    rig.events
        .iter()
        .filter_map(|e| match *e {
            Event::Seat { frame, slot: s, unit, op, outcome } if s == slot => Some((frame, unit, op, outcome)),
            _ => None,
        })
        .collect()
}

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

fn take_returns(rig: &mut Rig, slot: usize, kept: &mut Vec<Box<dyn UnitProcessor>>) -> Vec<(UnitId, bool, Option<SeatRefusal>)> {
    let mut out = Vec::new();
    while let Some(r) = rig.port(slot).returned_unit() {
        let refused = match r.left {
            Left::Refused(why) => Some(why),
            _ => None,
        };
        out.push((r.id, r.bypassed, refused));
        kept.push(r.unit);
    }
    out
}

fn xfade(from: f32, to: f32, k: u32) -> f32 {
    match k {
        0 => from,
        FADE => to,
        k => from + (k as f32 / FADE as f32) * (to - from),
    }
}

fn arm(rig: &mut Rig, bars: f64) -> Frame {
    rig.set(Command::SetMetronome(false));
    rig.set(Command::SetFixedLength(true));
    rig.set(Command::SetFixedBars(bars));
    rig.press(Command::RecDub(0));
    rig.start_frame()
}

fn guarded_block(rig: &mut Rig) -> bool {
    let n = rig.block;
    let ctx = ProcessContext {
        frame: rig.frame,
        xrun: false,
        damaged: false,
        align_frames: rig.align - rig.engine.limiter_latency(),
        input_frames: 0,
    };
    let (input, mut left, mut right) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let engine = &mut rig.engine;
    let ok = catch_unwind(AssertUnwindSafe(|| engine.process(&ctx, &input, &mut left, &mut right))).is_ok();
    rig.frame += n as Frame;
    ok
}

// ── A12 = N3: latency sum overflows before refusal ───────────────────────────────────────────────────

#[test]
fn a12_n3_latency_sum_overflow_is_refused_without_panic() {
    let (p1, p2) = (Probe::new(), Probe::new());
    let mut rig = rig(128);
    let mut kept = Vec::new();
    put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Lag(10, 1.0), &p1));
    rig.advance(128);

    let s = rig.frame;
    let ticket = put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p2).with_reported_latency(Frame::MAX));
    rig.advance(128);

    assert!(ticket.answered());
    assert_eq!(answers(&rig, 0).last(), Some(&(s, 2, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Latency))));
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, Some(SeatRefusal::Latency))]);
    assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1]);
    assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 10)));
    assert_eq!((p1.drops(), p2.drops()), (0, 0));
}

// ── A13: extreme finite crossfade endpoints ──────────────────────────────────────────────────────────

#[test]
fn a13_extreme_finite_crossfade_stays_finite_and_ordinary_samples_match_exact_bits() {
    for block in [64, 1024] {
        // Extreme finite endpoints: -f32::MAX -> f32::MAX and back.
        let (p1, p2, p3) = (Probe::new(), Probe::new(), Probe::new());
        let mut rig = rig(block);
        let mut kept = Vec::new();
        put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Level(-f32::MAX), &p1));
        rig.advance(128);

        put(&mut rig, 0, 2, 0, false, Some(1), Unit::new(Dsp::Level(f32::MAX), &p2));
        rig.keep_output();
        rig.advance(FADE as Frame + 128);
        assert!(rig.bus.iter().chain(&rig.bus_right).all(|y| y.is_finite()), "block {block}: -MAX -> +MAX produced non-finite sample");
        assert!(rig.bus[1] > -f32::MAX && rig.bus[1] < f32::MAX, "block {block}: first interior sample should interpolate between endpoints, got {}", rig.bus[1]);
        assert_eq!(rig.engine.diag().slot_protocol_errors, 0);

        put(&mut rig, 0, 3, 0, false, Some(2), Unit::new(Dsp::Level(-f32::MAX), &p3));
        rig.keep_output();
        rig.advance(FADE as Frame + 128);
        assert!(rig.bus.iter().chain(&rig.bus_right).all(|y| y.is_finite()), "block {block}: +MAX -> -MAX produced non-finite sample");
        assert!(rig.bus[1] > -f32::MAX && rig.bus[1] < f32::MAX, "block {block}: reverse first interior sample got {}", rig.bus[1]);

        let _ = take_returns(&mut rig, 0, &mut kept);
        rig.engine.evict_slots();
        let _ = take_returns(&mut rig, 0, &mut kept);
        assert_eq!((p1.drops(), p2.drops(), p3.drops()), (0, 0, 0));

        // Ordinary samples stay bit-identical to the original formula.
        let (q1, q2) = (Probe::new(), Probe::new());
        let mut rig = self::rig(block);
        put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Level(-0.3), &q1));
        rig.advance(128);
        put(&mut rig, 0, 2, 0, false, Some(1), Unit::new(Dsp::Level(0.7), &q2));
        rig.keep_output();
        rig.advance(FADE as Frame + 64);
        for (k, &y) in rig.bus.iter().enumerate() {
            let want = xfade(-0.3, 0.7, (k as u32).min(FADE));
            assert_eq!(y.to_bits(), want.to_bits(), "block {block}: ordinary sample mismatch at +{k}: {y} vs {want}");
        }
    }
}

// ── A14: panic in set_bypass during install ──────────────────────────────────────────────────────────

#[test]
fn a14_panic_in_set_bypass_during_install_is_recovered_and_answered_once() {
    // Recovery on the next block.
    let p = Probe::new();
    let mut rig = rig(128);
    let mut kept = Vec::new();
    rig.advance(128);
    let ticket = put(&mut rig, 0, 1, MAX_UNITS, true, None, Unit::new(Dsp::Gain(2.0), &p).panicking_in_set_bypass());

    assert!(!guarded_block(&mut rig), "set_bypass() should panic during install");
    assert_eq!(p.drops(), 0, "the unwind must not drop the unit");
    assert!(rig.engine.chain(0).next().is_none(), "a unit whose set_bypass panicked must not be seated");
    assert!(!ticket.answered(), "unanswered until recovery refuses it");

    let recover_at = rig.frame;
    rig.advance(128);
    assert!(ticket.answered(), "recovery must answer the ticket");
    let seat_answers: Vec<_> = answers(&rig, 0).into_iter().filter(|a| a.1 == 1).collect();
    assert_eq!(seat_answers, vec![(recover_at, 1, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Unknown))]);
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, true, Some(SeatRefusal::Unknown))]);
    assert_eq!(p.drops(), 0);
    drop(kept);
    assert_eq!(p.drops(), 1);

    // Recovery via evict_slots() immediately after the panic (and on a replacement, leaving the old seat intact).
    let (p_old, p_new) = (Probe::new(), Probe::new());
    let mut rig = self::rig(128);
    let mut kept = Vec::new();
    put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Gain(2.0), &p_old));
    rig.advance(128);
    let ticket = put(&mut rig, 0, 2, 0, true, Some(1), Unit::new(Dsp::Gain(3.0), &p_new).panicking_in_set_bypass());

    assert!(!guarded_block(&mut rig), "set_bypass() should panic during replacement install");
    assert_eq!((p_old.drops(), p_new.drops()), (0, 0));
    assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1], "old unit must remain seated");

    assert!(catch_unwind(AssertUnwindSafe(|| rig.engine.evict_slots())).is_ok());
    assert!(ticket.answered(), "evict recovery must answer the ticket");
    let back = take_returns(&mut rig, 0, &mut kept);
    assert!(back.contains(&(2, true, Some(SeatRefusal::Unknown))), "panicking unit returned as Refused(Unknown): {back:?}");
    assert!(back.contains(&(1, false, None)), "old seated unit also evicted cleanly: {back:?}");
    assert_eq!((p_old.drops(), p_new.drops()), (0, 0));
}

// ── N4: 8th install while a removed seat fades ───────────────────────────────────────────────────────

#[test]
fn n4_eighth_install_while_removed_seat_fades_succeeds_at_every_block_size() {
    for block in BLOCKS {
        // Case 1: remove(8) and install(9) queued in the same batch before a block.
        let p = Probe::new();
        let mut rig = rig(block);
        let mut kept = Vec::new();
        for id in 1..=MAX_UNITS as UnitId {
            put(&mut rig, 0, id, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
        }
        rig.advance(128);
        assert!(rig.port(0).remove(8));
        let ticket = put(&mut rig, 0, 9, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
        rig.advance(FADE as Frame + block as Frame * 2 + 128);

        assert!(ticket.answered(), "block {block} (same batch): install 9 answered");
        let a9: Vec<_> = answers(&rig, 0).into_iter().filter(|a| a.1 == 9).collect();
        assert_eq!(a9.len(), 1, "block {block} (same batch): one answer for 9");
        assert_eq!(a9[0].3, SeatOutcome::Applied, "block {block} (same batch): 8th install must apply, not Full");
        assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1, 2, 3, 4, 5, 6, 7, 9], "block {block}");

        // A real 9th install while all 8 are active (none removing) is still refused Full.
        let t10 = put(&mut rig, 0, 10, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
        rig.advance(block as Frame);
        assert!(t10.answered());
        assert_eq!(answers(&rig, 0).last().map(|a| (a.1, a.3)), Some((10, SeatOutcome::Refused(SeatRefusal::Full))), "block {block}");
        let _ = take_returns(&mut rig, 0, &mut kept);
        rig.engine.evict_slots();
        let _ = take_returns(&mut rig, 0, &mut kept);
        assert_eq!(p.drops(), 0);
    }

    // Case 2: remove(8), render 64 frames (mid-fade), then install(9).
    let p = Probe::new();
    let mut rig = rig(64);
    let mut kept = Vec::new();
    for id in 1..=MAX_UNITS as UnitId {
        put(&mut rig, 0, id, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    }
    rig.advance(64);
    assert!(rig.port(0).remove(8));
    rig.advance(64);
    let ticket = put(&mut rig, 0, 9, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    rig.advance(FADE as Frame + 256);

    assert!(ticket.answered());
    let a9: Vec<_> = answers(&rig, 0).into_iter().filter(|a| a.1 == 9).collect();
    assert_eq!(a9.len(), 1);
    assert_eq!(a9[0].3, SeatOutcome::Applied, "mid-fade 8th install must apply, not Full");
    assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1, 2, 3, 4, 5, 6, 7, 9]);
    let _ = take_returns(&mut rig, 0, &mut kept);
    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    assert_eq!(p.drops(), 0);
}

// ── N6: cross-role replaces refused as Placement ─────────────────────────────────────────────────────

#[test]
fn n6_cross_role_replaces_is_refused_as_placement() {
    let p = Probe::new();
    let mut rig = rig(128);
    let mut kept = Vec::new();

    // Slot 0: Effect replaced by Source -> Placement.
    put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Lag(50, 2.0), &p));
    // Slot 1: Source replaced by Effect -> Placement.
    put(&mut rig, 1, 10, 0, false, None, Unit::new(Dsp::Level(0.5), &p));
    rig.advance(128);

    let s = rig.frame;
    let t0 = put(&mut rig, 0, 2, 0, false, Some(1), Unit::new(Dsp::Level(0.25), &p));
    let t1 = put(&mut rig, 1, 11, 0, false, Some(10), Unit::new(Dsp::Gain(2.0), &p));
    rig.advance(128);

    assert!(t0.answered() && t1.answered());
    assert_eq!(answers(&rig, 0).last(), Some(&(s, 2, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Placement))));
    assert_eq!(answers(&rig, 1).last(), Some(&(s, 11, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Placement))));
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(2, false, Some(SeatRefusal::Placement))]);
    assert_eq!(take_returns(&mut rig, 1, &mut kept), vec![(11, false, Some(SeatRefusal::Placement))]);
    assert_eq!(rig.engine.slot(0), Some((SlotKind::Effect, 50)));
    assert_eq!(rig.engine.slot(1), Some((SlotKind::Instrument, 0)));
    assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![1]);
    assert_eq!(ids(&rig.engine.chain(1).collect::<Vec<_>>()), vec![10]);

    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    let _ = take_returns(&mut rig, 1, &mut kept);
    assert_eq!(p.drops(), 0);
}

// ── N7: holding published before taken ───────────────────────────────────────────────────────────────

/// Spin until `done`; after 10 s, or once the worker has exited, raise `stop` (the worker returns on it)
/// and fail the test, so a lost phase never hangs the suite.
fn bounded_wait(what: &str, done: impl Fn() -> bool, worker_alive: impl Fn() -> bool, stop: &AtomicBool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        if !worker_alive() || std::time::Instant::now() > deadline {
            let alive = worker_alive();
            stop.store(true, Ordering::Release);
            panic!("{what}: never happened (the worker {})", if alive { "is stuck" } else { "exited" });
        }
        std::hint::spin_loop();
    }
}

#[test]
fn n7_holding_is_visible_before_ticket_is_answered() {
    let mut cfg = EngineConfig::new(48_000);
    cfg.max_loop_seconds = 1.0;
    let (mut engine, handle) = Engine::new(cfg);
    let mut cmds = handle.commands;
    let mut events = handle.events;
    let mut port = {
        let [s0, _s1] = handle.slots;
        s0
    };
    let mut frame: Frame = 48_000;
    let (input, mut left, mut right) = ([0.0f32; 1], [0.0f32; 1], [0.0f32; 1]);

    // Configure slot 0 live, click off, fixed length 1 bar, and arm a take so `hold == true`.
    let push_cmd = |cmds: &mut rtrb::Producer<TimedCommand>, f: Frame, command: Command| {
        cmds.push(TimedCommand { frame: Some(f), command }).expect("command ring full");
    };
    push_cmd(&mut cmds, frame, Command::SetSlotLive(0, true));
    push_cmd(&mut cmds, frame, Command::SetMetronome(false));
    push_cmd(&mut cmds, frame, Command::SetFixedLength(true));
    push_cmd(&mut cmds, frame, Command::SetFixedBars(1.0));
    push_cmd(&mut cmds, frame, Command::RecDub(0));
    let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: -engine.limiter_latency(), input_frames: 0 };
    let v = violation_count();
    assert_no_alloc(|| engine.process(&ctx, &input, &mut left, &mut right));
    assert_eq!(violation_count(), v);
    frame += 1;
    while events.pop().is_ok() {}

    // Reusable latent unit (latency 10 -> alignment-changing edit -> held while capture is active).
    let probe = Probe::new();
    let mut unit: Option<Box<dyn UnitProcessor>> = Some(Unit::new(Dsp::Lag(10, 1.0), &probe));

    // Synchronize two threads tightly around `service_slots_idle()` so Thread B checks
    // `port.holding()` on the exact cycle `ticket.answered()` turns true.
    let phase = Arc::new(AtomicUsize::new(0));
    let race_seen = Arc::new(AtomicBool::new(false));
    let iters = 15_000usize;

    let phase_a = phase.clone();
    let race_a = race_seen.clone();

    thread::scope(|s| {
        let h = s.spawn(move || {
            for k in 1..=iters {
                // Wait for port thread to push Install (phase == 2 * k - 1).
                while phase_a.load(Ordering::Acquire) != 2 * k - 1 {
                    if race_a.load(Ordering::Relaxed) {
                        return;
                    }
                    std::hint::spin_loop();
                }
                // Hold is active -> hits `Verdict::Apply { edit: true } if hold`.
                engine.service_slots_idle();
                phase_a.store(2 * k, Ordering::Release);

                // Wait for the port thread to check `holding()` and ask for the reset.
                while phase_a.load(Ordering::Acquire) != 2 * k + 1_000_000 {
                    if race_a.load(Ordering::Relaxed) {
                        return;
                    }
                    std::hint::spin_loop();
                }
                // Cancel capture, apply held install (clears holding), then wait for remove.
                engine.punch_out();
                engine.service_slots_idle();
                phase_a.store(2 * k + 2_000_000, Ordering::Release);

                while phase_a.load(Ordering::Acquire) != 2 * k + 3_000_000 {
                    if race_a.load(Ordering::Relaxed) {
                        return;
                    }
                    std::hint::spin_loop();
                }
                engine.service_slots_idle();
                // Re-arm capture (1 frame under assert_no_alloc).
                push_cmd(&mut cmds, frame, Command::RecDub(0));
                let ctx = ProcessContext { frame, xrun: false, damaged: false, align_frames: -engine.limiter_latency(), input_frames: 0 };
                let v = violation_count();
                assert_no_alloc(|| engine.process(&ctx, &input, &mut left, &mut right));
                assert_eq!(violation_count(), v);
                frame += 1;
                while events.pop().is_ok() {}
                phase_a.store(2 * k + 4_000_000, Ordering::Release);
            }
        });

        for k in 1..=iters {
            let u = unit.take().expect("unit returned");
            let ticket = port.install_at(1, 0, MAX_UNITS, false, None, u).ok().expect("push install");
            phase.store(2 * k - 1, Ordering::Release);

            // Spin until ticket is answered, then IMMEDIATELY read port.holding(). Every wait is bounded:
            // a worker that exits or sticks fails the test instead of hanging it.
            let alive = || !h.is_finished();
            bounded_wait("the install answered", || ticket.answered(), alive, &race_seen);
            let held = port.holding();
            if !held {
                race_seen.store(true, Ordering::Release);
                break;
            }
            bounded_wait("the hold judged", || phase.load(Ordering::Acquire) == 2 * k, alive, &race_seen);
            // Reset for next iteration.
            phase.store(2 * k + 1_000_000, Ordering::Release);
            bounded_wait("the held install applied", || phase.load(Ordering::Acquire) == 2 * k + 2_000_000, alive, &race_seen);
            assert!(port.remove(1));
            phase.store(2 * k + 3_000_000, Ordering::Release);
            bounded_wait("the removal and the re-arm", || phase.load(Ordering::Acquire) == 2 * k + 4_000_000, alive, &race_seen);
            let (_, returned) = port.returned().expect("removed unit returned");
            unit = Some(returned);
        }
        h.join().expect("engine thread panicked");
    });

    assert!(!race_seen.load(Ordering::Acquire), "observed ticket.answered() == true while port.holding() == false");
}

// ── A-s1 = N-s1: same-id reseat/reload keeps latest applied bypass ───────────────────────────────────

#[test]
fn a_s1_withdrawn_toggled_on_keeps_bypass_when_reseated_stale() {
    // A withdrawn seat is toggled ON, then reseated under its id with a stale bypassed=false.
    for self_bypass in [false, true] {
        let (p1, p2) = (Probe::new(), Probe::new());
        let mut rig = rig(64);
        let mut kept = Vec::new();
        rig.set_input(code);
        let u1 = if self_bypass { Unit::new(Dsp::Lag(64, 3.0), &p1).self_bypassing() } else { Unit::new(Dsp::Lag(64, 3.0), &p1) };
        put(&mut rig, 0, 1, 0, false, None, u1);
        rig.advance(1000);

        assert!(rig.port(0).withdraw(1));
        rig.advance(FADE as Frame + 128);
        assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, false, None)]);

        // Toggle bypass ON while withdrawn.
        rig.press(Command::Action(Action::ToggleUnit(1)));
        rig.advance(128);
        assert!(chains(&rig, 0).last().unwrap().1[0].bypassed);

        // Reseat under same id with stale bypassed=false.
        let s = rig.frame;
        let u2 = if self_bypass { Unit::new(Dsp::Lag(64, 3.0), &p2).self_bypassing() } else { Unit::new(Dsp::Lag(64, 3.0), &p2) };
        put(&mut rig, 0, 1, 0, false, None, u2);
        rig.keep_output();
        rig.advance(FADE as Frame + 256);

        assert!(chains(&rig, 0).last().unwrap().1[0].bypassed, "self_bypass={self_bypass}: reseat must keep bypassed=true");
        assert!(p2.seen().contains(&Seen::Bypass { frame: s, on: true }), "self_bypass={self_bypass}: reseated unit must receive set_bypass(true)");
        // After fade settles, monitor passes dry (delayed by 64 for non-self-bypassing, or input for self-bypassing fake), NOT 3x.
        let tail_start = FADE as usize + 128;
        for (j, &y) in rig.monitor[tail_start..].iter().enumerate() {
            let f = s + (tail_start + j) as Frame;
            let want = if self_bypass { code(f) } else { code(f - 64) };
            assert_eq!(y.to_bits(), want.to_bits(), "self_bypass={self_bypass}: expected bypassed output at frame {f}");
        }
        rig.engine.evict_slots();
        let _ = take_returns(&mut rig, 0, &mut kept);
        assert_eq!((p1.drops(), p2.drops()), (0, 0));
    }
}

#[test]
fn a_s1_withdrawn_toggled_off_unbypasses_a_self_bypassing_reseat() {
    // Toggled OFF while withdrawn, reseated with a stale bypassed=true on a self-bypassing unit that starts bypassed.
    let (p1, p2) = (Probe::new(), Probe::new());
    let mut rig = rig(64);
    let mut kept = Vec::new();
    rig.set_input(code);
    put(&mut rig, 0, 1, 0, true, None, Unit::new(Dsp::Gain(3.0), &p1).self_bypassing());
    rig.advance(1000);

    assert!(rig.port(0).withdraw(1));
    rig.advance(FADE as Frame + 128);
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, true, None)]);

    // Toggle bypass OFF while withdrawn.
    rig.press(Command::Action(Action::ToggleUnit(1)));
    rig.advance(128);
    assert!(!chains(&rig, 0).last().unwrap().1[0].bypassed);

    let s = rig.frame;
    put(&mut rig, 0, 1, 0, true, None, Unit::new(Dsp::Gain(3.0), &p2).self_bypassing().initially_bypassed());
    rig.keep_output();
    rig.advance(FADE as Frame + 256);

    assert!(!chains(&rig, 0).last().unwrap().1[0].bypassed, "reseat must keep bypassed=false");
    assert!(p2.seen().contains(&Seen::Bypass { frame: s, on: false }), "self-bypassing reseat must be explicitly unbypassed");
    let tail_start = FADE as usize + 128;
    for (j, &y) in rig.monitor[tail_start..].iter().enumerate() {
        let f = s + (tail_start + j) as Frame;
        assert_eq!(y.to_bits(), (3.0 * code(f)).to_bits(), "expected active 3x output at frame {f}");
    }
    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    assert_eq!((p1.drops(), p2.drops()), (0, 0));
}

#[test]
fn a_s1_held_same_id_reload_keeps_bypass_toggled_while_it_waits() {
    // A same-id reload with bypassed=false is held during a take; the unit is toggled ON mid-take.
    let (p1, p2) = (Probe::new(), Probe::new());
    let mut rig = rig(64);
    let mut kept = Vec::new();
    put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Lag(64, 3.0), &p1));
    let _start = arm(&mut rig, 1.0);

    // Latency changes 64 -> 32, so the reload is held during the capture.
    let ticket = put(&mut rig, 0, 1, 0, false, Some(1), Unit::new(Dsp::Lag(32, 3.0), &p2));
    rig.advance(128);
    assert!(ticket.answered() && rig.port(0).holding());

    // Toggle bypass ON while the reload waits.
    rig.press(Command::Action(Action::ToggleUnit(1)));
    rig.advance_to(rig.end_frame() + FADE as Frame + 256);

    let last = chains(&rig, 0).last().unwrap().1.clone();
    assert_eq!(last.len(), 1);
    assert_eq!((last[0].unit, last[0].latency, last[0].bypassed), (1, 32, true), "held same-ID reload must keep bypassed=true");
    assert!(p2.seen().iter().any(|s| matches!(s, Seen::Bypass { on: true, .. })), "reloaded unit must receive set_bypass(true)");
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, true, None)]);
    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    assert_eq!((p1.drops(), p2.drops()), (0, 0));
}

// ── A-s4: ids held by fading/outgoing instances counted in duplicate check ───────────────────────────

#[test]
fn a_s4_leaving_id_is_refused_even_when_the_install_replaces_another_seat() {
    // Remove A (id=1), then during A's fade install a new A replacing B (id=2).
    let p = Probe::new();
    let mut rig = rig(64);
    let mut kept = Vec::new();
    put(&mut rig, 0, 1, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    put(&mut rig, 0, 2, MAX_UNITS, false, None, Unit::new(Dsp::Gain(1.0), &p));
    rig.advance(128);

    assert!(rig.port(0).remove(1));
    rig.advance(64); // id 1 is mid-fade

    let s = rig.frame;
    let ticket = put(&mut rig, 0, 1, 0, false, Some(2), Unit::new(Dsp::Gain(2.0), &p));
    rig.advance(64);

    assert!(ticket.answered());
    assert_eq!(answers(&rig, 0).last(), Some(&(s, 1, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Id))));
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, false, Some(SeatRefusal::Id))]);
    assert_eq!(ids(&rig.engine.chain(0).collect::<Vec<_>>()), vec![2], "B must remain seated");

    rig.advance(FADE as Frame + 128);
    let _ = take_returns(&mut rig, 0, &mut kept);
    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    assert_eq!(p.drops(), 0);
}

#[test]
fn a_s4_outgoing_id_in_another_slot_is_refused() {
    // Slot 1 replaces id=1 with id=2; while id=1 is outgoing there, slot 0 tries to install id=1.
    let p = Probe::new();
    let mut rig = rig(64);
    let mut kept = Vec::new();
    put(&mut rig, 1, 1, 0, false, None, Unit::new(Dsp::Level(0.2), &p));
    rig.advance(128);

    put(&mut rig, 1, 2, 0, false, Some(1), Unit::new(Dsp::Level(0.4), &p));
    rig.advance(64); // id 1 is outgoing in slot 1

    let s = rig.frame;
    let ticket = put(&mut rig, 0, 1, 0, false, None, Unit::new(Dsp::Gain(1.0), &p));
    rig.advance(64);

    assert!(ticket.answered());
    assert_eq!(answers(&rig, 0).last(), Some(&(s, 1, SeatOp::Install, SeatOutcome::Refused(SeatRefusal::Id))));
    assert_eq!(take_returns(&mut rig, 0, &mut kept), vec![(1, false, Some(SeatRefusal::Id))]);
    assert!(rig.engine.chain(0).next().is_none());

    // After slot 1's swap finishes, a same-ID reload of id=2 in slot 1 is still allowed.
    rig.advance(FADE as Frame + 128);
    let reload_at = rig.frame;
    let t_reload = put(&mut rig, 1, 2, 0, false, Some(2), Unit::new(Dsp::Level(0.6), &p));
    rig.advance(FADE as Frame + 128);
    assert!(t_reload.answered());
    assert_eq!(answers(&rig, 1).last(), Some(&(reload_at, 2, SeatOp::Install, SeatOutcome::Applied)));

    let _ = take_returns(&mut rig, 1, &mut kept);
    rig.engine.evict_slots();
    let _ = take_returns(&mut rig, 0, &mut kept);
    let _ = take_returns(&mut rig, 1, &mut kept);
    assert_eq!(p.drops(), 0);
}
