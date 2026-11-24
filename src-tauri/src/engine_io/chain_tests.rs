//! Tests for E2 P2: the table, the model, the gate, the rebuild handoff, and legacy `SlotHost`.

use std::any::Any;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release, SeqCst}};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{
    max_chain_latency, Action, Command, InputSendParam, LaneState, SeatRefusal, SlotEvent,
    TimedCommand, UnitCx, UnitId, UnitInfo, UnitKind, UnitParam, UnitProcessor,
};

use super::chain::{PlateState, PluginRef, UNOWNED_KEEP};
use super::fake_driver::{Fake, FakeDriver};
use super::test_rig::{TestDevice, PATIENCE};
use super::unit_host::{Activated, Back, BuiltinFactory, Op, OpState, Params, Removal, Spec, SubmitError, Terms, Unit, GATE_DEADLINE};
use super::{DeviceRequest, EngineHost, HostConfig};
use crate::audio_output::AudioBackend;

fn ok_op(res: Result<Op, (Unit, SubmitError)>) -> Op {
    res.unwrap_or_else(|(_, e)| panic!("submit failed: {e:?}"))
}

fn plugin_ref(id: &str) -> PluginRef {
    PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{id}.clap"), id: id.into() }
}

/// Simple plugin test unit with a tag, configurable latency, and stop counter.
struct LatentUnit {
    tag: u32,
    latency: Frame,
    source: bool,
    stops: Arc<AtomicUsize>,
}

impl LatentUnit {
    fn new(tag: u32, latency: Frame, stops: Arc<AtomicUsize>) -> Box<Self> {
        Box::new(LatentUnit { tag, latency, source: false, stops })
    }
}

impl UnitProcessor for LatentUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: self.source, latency: self.latency }
    }
    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        for (o, i) in out.into_iter().zip(input) {
            o.copy_from_slice(i);
        }
    }
    fn stop(&mut self) {
        self.stops.fetch_add(1, SeqCst);
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

const MAX_RECORDED_PARAMS: usize = 32;

/// Preallocated, lock-free recorder for `ClampEcho::set_param` on the audio thread (invariant 5).
struct ParamRecorder {
    values: [AtomicU64; MAX_RECORDED_PARAMS],
    len: AtomicUsize,
}

impl ParamRecorder {
    fn new() -> Arc<Self> {
        Arc::new(ParamRecorder {
            values: std::array::from_fn(|_| AtomicU64::new(0)),
            len: AtomicUsize::new(0),
        })
    }

    fn record(&self, value: f64) {
        let idx = self.len.load(Relaxed);
        if idx < MAX_RECORDED_PARAMS {
            self.values[idx].store(value.to_bits(), Relaxed);
            self.len.store(idx + 1, Release);
        }
    }

    fn snapshot(&self) -> Vec<f64> {
        let n = self.len.load(Acquire).min(MAX_RECORDED_PARAMS);
        (0..n).map(|i| f64::from_bits(self.values[i].load(Relaxed))).collect()
    }

    fn is_empty(&self) -> bool {
        self.len.load(Acquire) == 0
    }
}

/// A clamping built-in fake (E2 test-only `BuiltinFactory`): clamps `EchoLevel` to `0.0..=1.0`
/// (so 2.0 -> 1.0) and records every `set_param` value applied on the audio thread without locking
/// or allocating.
struct ClampEcho {
    level: f64,
    latency: Frame,
    applied: Arc<ParamRecorder>,
}

impl ClampEcho {
    fn new(level: f64, latency: Frame, applied: Arc<ParamRecorder>) -> Box<Self> {
        Box::new(ClampEcho { level, latency, applied })
    }
}

impl UnitProcessor for ClampEcho {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: self.latency }
    }
    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let g = self.level as f32;
        for (o, i) in out.into_iter().zip(input) {
            for (y, &x) in o.iter_mut().zip(i) {
                *y = x * g;
            }
        }
    }
    fn set_param(&mut self, _cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        if param == UnitParam::Send(InputSendParam::EchoLevel) {
            let c = value.clamp(0.0, 1.0);
            self.level = c;
            self.applied.record(c);
            Some(c)
        } else {
            None
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

struct ClampFactory {
    applied: Arc<ParamRecorder>,
}

impl BuiltinFactory for ClampFactory {
    fn build(&self, kind: UnitKind, params: &Params, _terms: Terms) -> Result<(Unit, Params), String> {
        if kind != UnitKind::Echo {
            return Err(format!("unsupported kind {kind:?}"));
        }
        let mut level = 0.5;
        for &(p, v) in params {
            if p == UnitParam::Send(InputSendParam::EchoLevel) {
                level = v.clamp(0.0, 1.0);
            }
        }
        let clamped = vec![(UnitParam::Send(InputSendParam::EchoLevel), level)];
        Ok((ClampEcho::new(level, 0, self.applied.clone()), clamped))
    }
}

fn start_take(device: &TestDevice) {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let mut ev = Vec::new();
        let (_, ov) = host.drain_feed(&mut ev);
        if let Some(ov) = ov {
            let l = ov.lane(0);
            if l.state == LaneState::Recording && l.frames > 0 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "recording take never started");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn stop_take(device: &TestDevice) {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let mut ev = Vec::new();
        let (_, ov) = host.drain_feed(&mut ev);
        if let Some(ov) = ov {
            if ov.lane(0).state != LaneState::Recording {
                break;
            }
        }
        assert!(Instant::now() < deadline, "recording take never stopped");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn engine_chain_ids(device: &TestDevice, slot: usize) -> Vec<(UnitId, bool)> {
    let rt = device.host().core.rt.lock().unwrap();
    rt.engine.as_ref().unwrap().chain(slot).map(|s| (s.unit, s.bypassed)).collect()
}

fn fill_event_ring(device: &TestDevice) {
    let ring_full = || {
        let ends = device.host().core.ends.lock().unwrap();
        let ev = &ends.as_ref().unwrap().events;
        ev.slots() == ev.buffer().capacity()
    };
    for _batch in 0..120u32 {
        for i in 0..50u32 {
            let _ = device.host().send(TimedCommand { frame: None, command: Command::SetMetronome(i % 2 == 0) });
        }
        assert!(device.wait_blocks(2));
        if ring_full() {
            return;
        }
    }
    panic!("event ring never filled");
}

/// 1. Two units of a slot return to their owners out of order, each by its install token.
#[test]
fn two_units_of_a_slot_return_to_their_owners_out_of_order() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let terms = ua.terms().unwrap();
    let op_a = ok_op(ua.submit(LatentUnit::new(101, 0, stops.clone()), terms));
    let op_b = ok_op(ub.submit(LatentUnit::new(202, 0, stops.clone()), terms));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_b, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Remove B first, then A; poll A first (must not receive B's box), then B, then A.
    let Removal::Sent(rem_b) = host.remove_unit(20).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(rem_b, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));
    assert!(ua.take_back().is_none(), "unit A must not receive unit B's box");

    let Removal::Sent(rem_a) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(rem_a, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));

    // Take A's box before B's even though B returned first on the port.
    let Back::Removed(box_a) = ua.take_back().expect("A gets its box") else { panic!("expected Removed for A") };
    let Back::Removed(box_b) = ub.take_back().expect("B gets its box") else { panic!("expected Removed for B") };
    assert_eq!(box_a.into_any().downcast::<LatentUnit>().unwrap().tag, 101);
    assert_eq!(box_b.into_any().downcast::<LatentUnit>().unwrap().tag, 202);
    ua.release();
    ub.release();
}

/// 2. A rebuild halfway through a same-id crossfade: each box goes to its own row, and the old
///    outgoing row receives `Superseded` (never `Evicted`, never re-activated).
#[test]
fn rebuild_halfway_through_same_id_crossfade_routes_each_box_to_its_row() {
    // 64-frame blocks at 48 kHz: 10 ms crossfade = 480 frames (~7.5 blocks).
    let mut device = TestDevice::start(48_000, 64, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let old_host = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    old_host.activated(Activated { name: "Amp v1".into(), source: false, latency: 0 });
    let t1 = old_host.terms().unwrap();
    let op1 = ok_op(old_host.submit(LatentUnit::new(1, 0, stops.clone()), t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Start a same-id reload.
    let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
    cand.activated(Activated { name: "Amp v2".into(), source: false, latency: 0 });
    // Pause rendering so we can step exactly 2 blocks (128 < 480 frames: mid-crossfade).
    device.stop();
    let op2 = ok_op(cand.submit(LatentUnit::new(2, 0, stops.clone()), t1));
    // While the candidate stands in flight, the outgoing row's withdraw is deferred (`Reloading`).
    assert_eq!(old_host.withdraw(), Err(SubmitError::Reloading));

    // Render 2 blocks manually under rt lock: swap crossfade is 128 / 480 frames in flight.
    // Do NOT call `wait_op` while stopped (that would call `service_if_idle`, finishing the fade);
    // drain the event ring directly so `op2`'s `Applied` reaches the model while the swap still runs.
    {
        let mut rt = host.core.rt.lock().unwrap();
        let engine = rt.engine.as_mut().unwrap();
        let (x, mut l, mut r) = ([0.0f32; 64], [0.0f32; 64], [0.0f32; 64]);
        for k in 0..2i64 {
            let ctx = lf_engine::ProcessContext { frame: k * 64, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            engine.process(&ctx, &x, &mut l, &mut r);
        }
    }
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(cand.poll(op2), OpState::Applied);
    assert_eq!(old_host.withdraw(), Err(SubmitError::Superseded));
    assert!(old_host.take_back().is_none(), "old unit is still crossfading out");

    // Rebuild halfway through the crossfade!
    device.rebuild_at(44_100, 256);

    let Back::Superseded(old_box) = old_host.take_back().expect("old row gets Superseded") else {
        panic!("expected Back::Superseded for outgoing reload row, never Evicted");
    };
    assert_eq!(old_box.into_any().downcast::<LatentUnit>().unwrap().tag, 1);

    let Back::Evicted { unit: new_box, bypassed } = cand.take_back().expect("candidate gets Evicted") else {
        panic!("expected Back::Evicted for new reload row");
    };
    assert!(!bypassed);
    let new_box = new_box.into_any().downcast::<LatentUnit>().unwrap();
    assert_eq!(new_box.tag, 2);

    // Old row releases; candidate reseats at the new terms.
    old_host.release();
    let t2 = cand.terms().unwrap();
    assert_eq!(t2.rate, 44_100);
    let op3 = ok_op(cand.submit(new_box, t2));
    assert_eq!(host.wait_op(op3, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)]);
    cand.release();
}

/// 3. An unrelated bypass during a latency-changing reseat: the op stays `Held`, and its bound
///    does not restart (nor time out while held).
#[test]
fn unrelated_bypass_during_latency_changing_reseat_keeps_op_held_and_bound_intact() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let terms = ua.terms().unwrap();
    let op_a = ok_op(ua.submit(LatentUnit::new(1, 0, stops.clone()), terms));
    let op_b = ok_op(ub.submit(LatentUnit::new(2, 0, stops.clone()), terms));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_b, Duration::from_secs(2)).unwrap(), OpState::Applied);

    start_take(&device);

    // Withdraw unit A (does not change latency -> applies immediately).
    let op_w = ua.withdraw().unwrap();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let withdrawn = loop {
        if let Some(b) = ua.take_back() {
            break b;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(matches!(withdrawn, Back::Withdrawn { .. }));

    // Reseat unit A with latency 64 -> held until take ends.
    ua.activated(Activated { name: "a".into(), source: false, latency: 64 });
    let op_reseat = ok_op(ua.submit(LatentUnit::new(3, 64, stops.clone()), terms));
    let deadline = Instant::now() + Duration::from_secs(2);
    while ua.poll(op_reseat) != OpState::Held {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert!(Instant::now() < deadline, "reseat never entered Held");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ua.held());

    // Wait on `op_reseat` with a short 150 ms bound while the take lasts > 300 ms and an unrelated
    // bypass on unit B emits `Event::Chain`.
    let waited = std::thread::scope(|s| {
        let waiter = s.spawn(|| host.wait_op(op_reseat, Duration::from_millis(150)));
        std::thread::sleep(Duration::from_millis(80));
        host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(20, true) }).unwrap();
        assert!(device.wait_blocks(10));
        let mut ev = Vec::new();
        let drain = host.drain_feed_taps(&mut ev, None, None, None);
        assert!(drain.chains.chains[0].held, "slot stays held after unrelated bypass");
        assert!(drain.chains.chains[0].units.iter().find(|u| u.id == 20).unwrap().bypass, "B is bypassed");
        assert_eq!(ua.poll(op_reseat), OpState::Held, "reseat op stays Held");
        // Keep holding past the 150 ms bound.
        std::thread::sleep(Duration::from_millis(220));
        assert!(!waiter.is_finished(), "bound must not expire while held");
        stop_take(&device);
        // Pump feed so waiter sees Applied promptly.
        let end = Instant::now() + Duration::from_secs(2);
        while !waiter.is_finished() && Instant::now() < end {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            std::thread::sleep(Duration::from_millis(2));
        }
        waiter.join().unwrap()
    });
    assert_eq!(waited.unwrap(), OpState::Applied);
    assert!(!ua.held());
    ua.release();
    ub.release();
}

/// 4. A full event ring with the last applied param outside it: across a rebuild, both the model
///    and the rebuilt clamping fake read the clamped applied value (1.0 for sent 2.0), verified by
///    rendering a block through the rebuilt fake.
#[test]
fn full_event_ring_keeps_last_applied_param_across_rebuild() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.25);
    let host = device.host();
    let applied_log = ParamRecorder::new();
    let factory = ClampFactory { applied: applied_log.clone() };

    let init_params = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.25)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init_params.clone() }, 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let (unit1, clamped1) = factory.build(UnitKind::Echo, &init_params, t1).unwrap();
    u.committed(clamped1);
    let op1 = ok_op(u.submit(unit1, t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Fill the tiny 8-entry event ring without draining it.
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    fill_event_ring(&device);

    // Send SetUnitParam(2.0) -> clamps to 1.0, cannot enter the full event ring, stays in `unsent_unit_params`.
    host.send(TimedCommand {
        frame: None,
        command: Command::SetUnitParam(10, UnitParam::Send(InputSendParam::EchoLevel), 2.0),
    })
    .unwrap();
    assert!(device.wait_blocks(10));
    assert_eq!(applied_log.snapshot(), vec![1.0], "engine applied clamped 1.0");

    // Rebuild WITHOUT draining the old event ring.
    device.rebuild_at(44_100, 256);

    // The model must read 1.0 from `unsent_unit_params`!
    let snap = host.core.chain_snapshot();
    let model_params = snap.chains[0].units[0].params.as_ref().expect("built-in params").0.clone();
    assert_eq!(model_params, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);

    // Rebuild the fake from the model's params at the new terms, render audio through it, and
    // verify both metadata and the rendered output reflect the clamped 1.0 gain (not initial 0.25).
    let Back::Evicted { unit: old_unit, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
    drop(old_unit);
    let t2 = u.terms().unwrap();
    let (rebuilt_unit, clamped2) = factory.build(UnitKind::Echo, &model_params, t2).unwrap();
    assert_eq!(clamped2, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);
    let op2 = ok_op(u.submit(rebuilt_unit, t2));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let snap2 = host.core.chain_snapshot();
    assert_eq!(snap2.chains[0].units[0].params.as_ref().unwrap().0, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);

    host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
    assert!(device.wait_blocks(15));
    let out = device.output.lock().unwrap();
    let tail = &out[out.len().saturating_sub(64)..];
    assert_eq!(tail.len(), 64);
    for &sample in tail {
        assert!(
            (sample - 0.25).abs() < 1e-3,
            "rebuilt fake must render input 0.25 * clamped level 1.0 = 0.25, got {sample}"
        );
    }
    drop(out);
    u.release();
}

/// 5. Open thread #1: an eviction with owed replies full returns the queued install (settling its
///    pending op as `OpState::Rebuilt` and filing its box as `Back::Evicted`, alongside any seated
///    unit returned to its own row).
#[test]
fn open_thread_eviction_with_owed_replies_full_returns_queued_install() {
    let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    fill_event_ring(&device);
    device.stop();
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    // Seat unit 1, then withdraw + reseat it 7 times while the event ring stays full:
    // initial install (2 owed) + 7 * (withdraw 2 + reseat 2) = 30 owed replies -> `can_take()` is false!
    let u_seat = host.unit(0, 1, Spec::Plugin(plugin_ref("seat")), 0, false).unwrap();
    let t = u_seat.terms().unwrap();
    let _ = ok_op(u_seat.submit(LatentUnit::new(10, 0, stops.clone()), t));
    super::unit_host::service_if_idle(&host.core);
    for round in 0..7u32 {
        let _ = u_seat.withdraw().unwrap();
        super::unit_host::service_if_idle(&host.core);
        let Back::Withdrawn { unit, .. } = u_seat.take_back().expect("withdrawn box") else {
            panic!("round {round}: expected Withdrawn");
        };
        let _ = ok_op(u_seat.submit(unit, t));
        super::unit_host::service_if_idle(&host.core);
    }

    // Now owed replies are full (30): submitting unit 2 stays queued in `port.tx` (`can_take()` is false).
    let u_queued = host.unit(0, 2, Spec::Plugin(plugin_ref("queued")), 1, false).unwrap();
    let op = ok_op(u_queued.submit(LatentUnit::new(99, 0, stops.clone()), t));
    super::unit_host::service_if_idle(&host.core);
    assert!(u_queued.take_back().is_none(), "install stayed queued while owed replies were full");

    // Rebuild must evict and return BOTH the seated unit and the queued install to their own rows,
    // settling the queued install's op as `OpState::Rebuilt`.
    device.rebuild_at(44_100, 256);
    assert_eq!(u_queued.poll(op), OpState::Rebuilt);
    let Back::Evicted { unit: back_seat, .. } = u_seat.take_back().expect("seated unit returned") else {
        panic!("expected Back::Evicted for seated unit");
    };
    assert_eq!(back_seat.into_any().downcast::<LatentUnit>().unwrap().tag, 10);
    let Back::Evicted { unit: back_q, .. } = u_queued.take_back().expect("eviction must return the queued install") else {
        panic!("expected Back::Evicted for queued install");
    };
    assert_eq!(back_q.into_any().downcast::<LatentUnit>().unwrap().tag, 99);
    u_seat.release();
    u_queued.release();
}

/// 6. Open thread #2: a refusal after the settle bound reaches its owner with its reason (both
///    legacy `SlotHost` and chain `UnitHost`).
#[test]
fn open_thread_refusal_after_settle_bound_reaches_owner() {
    // Part A: legacy `SlotHost` (was RED on 95d4319a: `take_evicted()` returned `None`).
    {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let slot = device.host().slot(0);
        let past = max_chain_latency(48_000) + 1;
        let stops = Arc::new(AtomicUsize::new(0));
        let unit = LatentUnit::new(77, past, stops);

        let res = std::thread::scope(|s| {
            let held = device.host().core.rt.lock().unwrap();
            let handle = s.spawn(|| slot.install(unit, 48_000));
            std::thread::sleep(Duration::from_millis(2200));
            let r = handle.join().unwrap();
            drop(held);
            r
        });
        assert!(res.is_ok(), "timed out settle stands as sent");
        assert!(device.wait_blocks(20));
        let back = slot.take_evicted().expect("refusal after settle bound must reach its owner, not go unseen");
        assert_eq!(back.into_any().downcast::<LatentUnit>().unwrap().tag, 77);
    }

    // Part B: chain `UnitHost` receives `Back::Refused { why: SeatRefusal::Latency }` even if a
    // wait timed out first.
    {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host();
        let u = host.unit(0, 10, Spec::Plugin(plugin_ref("late")), 0, false).unwrap();
        let terms = u.terms().unwrap();
        let past = max_chain_latency(48_000) + 1;
        let op = {
            let _held = host.core.rt.lock().unwrap();
            let op = ok_op(u.submit(LatentUnit::new(88, past, Arc::new(AtomicUsize::new(0))), terms));
            assert!(host.wait_op(op, Duration::from_millis(50)).is_err(), "stalled engine times out short wait");
            op
        };
        assert!(device.wait_blocks(10));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Refused(SeatRefusal::Latency));
        let Back::Refused { unit, why } = u.take_back().expect("refused box reaches owner") else {
            panic!("expected Back::Refused");
        };
        assert_eq!(why, SeatRefusal::Latency);
        assert_eq!(unit.into_any().downcast::<LatentUnit>().unwrap().tag, 88);
        u.release();
    }
}

/// 7. Two `ToggleUnit`s on two ids, one applied unanswered, one not taken, each flips once across
///    a rebuild.
#[test]
fn two_toggle_units_one_unanswered_one_unapplied_each_flip_once_across_rebuild() {
    let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let t1 = ua.terms().unwrap();
    let op_a = ok_op(ua.submit(LatentUnit::new(1, 0, stops.clone()), t1));
    let op_b = ok_op(ub.submit(LatentUnit::new(2, 0, stops.clone()), t1));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_b, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Fill the tiny event ring so A's toggle applies unanswered.
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    fill_event_ring(&device);
    host.send(TimedCommand { frame: None, command: Command::Action(Action::ToggleUnit(10)) }).unwrap();
    assert!(device.wait_blocks(10));

    // Stop rendering so B's toggle is queued in the command ring and NEVER taken by engine 1.
    device.stop();
    host.send(TimedCommand { frame: None, command: Command::Action(Action::ToggleUnit(20)) }).unwrap();

    // Rebuild!
    device.rebuild_at(44_100, 256);

    let Back::Evicted { unit: box_a, bypassed: byp_a } = ua.take_back().expect("A evicted") else { panic!("A") };
    let Back::Evicted { unit: box_b, bypassed: byp_b } = ub.take_back().expect("B evicted") else { panic!("B") };
    assert!(byp_a, "A's applied unanswered toggle is already true");
    assert!(!byp_b, "B's unapplied toggle has not flipped yet");

    // Resume rendering and reseat both units.
    device.resume();
    let t2 = ua.terms().unwrap();
    let op_a2 = ok_op(ua.submit(box_a, t2));
    let op_b2 = ok_op(ub.submit(box_b, t2));
    assert_eq!(host.wait_op(op_a2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_b2, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Drain flushes the gate (which sends B's unapplied ToggleUnit(20)), then render and drain.
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);

    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true), (20, true)], "each flipped exactly once");
    let snap = host.core.chain_snapshot();
    assert_eq!(snap.chains[0].units.iter().map(|u| (u.id, u.bypass)).collect::<Vec<_>>(), vec![(10, true), (20, true)]);
    ua.release();
    ub.release();
}

/// 8. A toggle unanswered and a per-unit change after it are both kept across a rebuild.
#[test]
fn unanswered_toggle_and_per_unit_change_after_it_both_kept_across_rebuild() {
    let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let applied_log = ParamRecorder::new();
    let factory = ClampFactory { applied: applied_log.clone() };

    let init = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.25)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let (unit1, clamped) = factory.build(UnitKind::Echo, &init, t1).unwrap();
    u.committed(clamped);
    let op1 = ok_op(u.submit(unit1, t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    fill_event_ring(&device);

    // Unanswered toggle applied while ring is full, then a per-unit param change queued after stop.
    host.send(TimedCommand { frame: None, command: Command::Action(Action::ToggleUnit(10)) }).unwrap();
    assert!(device.wait_blocks(10));
    device.stop();
    host.send(TimedCommand {
        frame: None,
        command: Command::SetUnitParam(10, UnitParam::Send(InputSendParam::EchoLevel), 0.75),
    })
    .unwrap();

    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: old_box, bypassed } = u.take_back().expect("evicted") else { panic!("evicted") };
    drop(old_box);
    assert!(bypassed, "unanswered toggle kept");

    device.resume();
    let t2 = u.terms().unwrap();
    let model_params = host.core.chain_snapshot().chains[0].units[0].params.as_ref().unwrap().0.clone();
    let (unit2, _) = factory.build(UnitKind::Echo, &model_params, t2).unwrap();
    let op2 = ok_op(u.submit(unit2, t2));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);

    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)]);
    let snap = host.core.chain_snapshot();
    assert!(snap.chains[0].units[0].bypass);
    assert_eq!(snap.chains[0].units[0].params.as_ref().unwrap().0, vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.75)]);
    u.release();
}

/// 9. A second rebuild with the gate non-empty keeps its command order.
#[test]
fn second_rebuild_with_non_empty_gate_keeps_order() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let applied_log = ParamRecorder::new();
    let factory = ClampFactory { applied: applied_log.clone() };

    let init = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.1)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let (unit1, clamped) = factory.build(UnitKind::Echo, &init, t1).unwrap();
    u.committed(clamped);
    let op1 = ok_op(u.submit(unit1, t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Rebuild #1: unit 10 is evicted and NOT reseated yet -> commands enter the gate.
    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: old_box, .. } = u.take_back().expect("evicted") else { panic!("evicted") };
    drop(old_box);
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, UnitParam::Send(InputSendParam::EchoLevel), 0.3) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, UnitParam::Send(InputSendParam::EchoLevel), 0.8) }).unwrap();

    // Rebuild #2 while gate is non-empty!
    device.rebuild_at(48_000, 256);
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, UnitParam::Send(InputSendParam::EchoLevel), 0.9) }).unwrap();

    // Now reseat unit 10 into engine #3 and flush gate.
    let t3 = u.terms().unwrap();
    let (unit3, _) = factory.build(UnitKind::Echo, &init, t3).unwrap();
    let op3 = ok_op(u.submit(unit3, t3));
    assert_eq!(host.wait_op(op3, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);

    assert_eq!(applied_log.snapshot(), vec![0.3, 0.8, 0.9], "gate order preserved across second rebuild");
    u.release();
}

/// 10. A late submit after the gate deadline at the same rate is refused `Stale` (old terms),
///     while its plate reads `failed` and its gated commands dropped; fresh terms still seat it.
#[test]
fn late_submit_after_gate_deadline_at_same_rate_is_refused_stale() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let old_terms = u.terms().unwrap();
    assert_eq!(old_terms.rate, 48_000);
    let op1 = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), old_terms));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Rebuild at the SAME rate (48_000) under a controllable clock -> generation bumps, rate stays 48_000.
    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);
    device.rebuild_at(48_000, 256);
    let Back::Evicted { unit: back, .. } = u.take_back().expect("evicted") else { panic!("evicted") };
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10), "command entered the gate while reseating");

    // Just before the gate deadline: plate has not failed yet and the command stays gated.
    host.core.state.lock().unwrap().model.advance_clock(GATE_DEADLINE - Duration::from_millis(1));
    let mut ev = Vec::new();
    let before = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(before.chains.chains[0].units[0].state, PlateState::Seated);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 0);

    // Advance across the gate deadline deterministically, then flush gate -> plate fails and gated command drops.
    host.core.state.lock().unwrap().model.advance_clock(Duration::from_millis(1));
    let drain = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(drain.chains.chains[0].units[0].state, PlateState::Failed);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 1, "gated command dropped after deadline");

    // Late submit with old terms (same rate, old generation) is refused `Stale`.
    let (back, err) = u.submit(back, old_terms).err().expect("stale terms must be refused");
    assert_eq!(err, SubmitError::Stale);

    // Submitting with the fresh terms still seats it.
    let new_terms = u.terms().unwrap();
    assert_eq!(new_terms.rate, 48_000);
    assert_ne!(new_terms.generation, old_terms.generation);
    let op2 = ok_op(u.submit(back, new_terms));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.core.chain_snapshot().chains[0].units[0].state, PlateState::Seated);
    u.release();
}

/// 11. Reseats finishing in reverse order land in model order with their applied bypass.
#[test]
fn reseats_finishing_in_reverse_land_in_model_order_with_bypass() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, true).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let uc = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 2, true).unwrap();
    let t1 = ua.terms().unwrap();
    for (u, tag) in [(&ua, 1), (&ub, 2), (&uc, 3)] {
        let op = ok_op(u.submit(LatentUnit::new(tag, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true), (20, false), (30, true)]);

    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: ba, .. } = ua.take_back().expect("A") else { panic!("A") };
    let Back::Evicted { unit: bb, .. } = ub.take_back().expect("B") else { panic!("B") };
    let Back::Evicted { unit: bc, .. } = uc.take_back().expect("C") else { panic!("C") };

    // Reseat in reverse order: C, then B, then A.
    let t2 = ua.terms().unwrap();
    for (u, bx) in [(&uc, bc), (&ub, bb), (&ua, ba)] {
        let op = ok_op(u.submit(bx, t2));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true), (20, false), (30, true)]);
    let snap = host.core.chain_snapshot();
    assert_eq!(
        snap.chains[0].units.iter().map(|u| (u.id, u.bypass)).collect::<Vec<_>>(),
        vec![(10, true), (20, false), (30, true)]
    );
    ua.release();
    ub.release();
    uc.release();
}

/// 12. A held remove outlives `REMOVE_TIMEOUT` (2 s): `wait_op` does not count time while held.
#[test]
#[ignore = "quarantined: on CI's Windows runner the take never starts (start_take, 30 s); passes locally; src-tauri/OPEN-THREADS.md"]
fn held_remove_outlives_remove_timeout() {
    // 64-frame blocks every 5 ms (~12,800 frames/s) so the 2-second max loop (96,000 frames) stays
    // open for > 7 real seconds while we wait past the 2 s REMOVE_TIMEOUT.
    let device = TestDevice::start(48_000, 64, Duration::from_millis(5), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
    u.activated(Activated { name: "lat".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    start_take(&device);
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    let remove_timeout = Duration::from_secs(2);
    let result = std::thread::scope(|s| {
        let waiter = s.spawn(|| host.wait_op(op_rem, remove_timeout));
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!waiter.is_finished(), "held removal must outlive 2 s REMOVE_TIMEOUT");
        stop_take(&device);
        waiter.join().unwrap()
    });
    assert_eq!(result.unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let back = loop {
        if let Some(b) = u.take_back() {
            break b;
        }
        assert!(Instant::now() < deadline, "removed unit box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(matches!(back, Back::Removed(_)));
    u.release();
}

/// 13a. A move with a failed plate ahead of the target translates its plate index to engine index.
#[test]
fn move_with_failed_plate_ahead_of_target() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    // Plate 0: failed add (no seat in engine).
    let uf = host.unit(0, 1, Spec::Plugin(plugin_ref("fail")), 0, false).unwrap();
    uf.failed("load failed".into());
    uf.release();

    // Plates 1 and 2: seated units A (10) and B (20).
    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 1, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 2, false).unwrap();
    let t = ua.terms().unwrap();
    let op_a = ok_op(ua.submit(LatentUnit::new(1, 0, stops.clone()), t));
    let op_b = ok_op(ub.submit(LatentUnit::new(2, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_b, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false), (20, false)]);

    // Move B (plate 2) to plate index 1 (behind F, ahead of A) -> engine index 0.
    let op_mv = host.move_unit(20, 1).unwrap();
    assert_eq!(host.wait_op(op_mv, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false), (10, false)]);
    let snap = host.core.chain_snapshot();
    assert_eq!(snap.chains[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1, 20, 10]);

    // Removing the failed add's plate returns Removal::Plate at once.
    assert_eq!(host.remove_unit(1).unwrap(), Removal::Plate);
    ua.release();
    ub.release();
}

/// 13b. A move with a held removal ahead of it translates its index past the pending removal.
#[test]
fn move_with_held_removal_ahead_of_it() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let ur = host.unit(0, 5, Spec::Plugin(plugin_ref("r")), 0, false).unwrap();
    ur.activated(Activated { name: "r".into(), source: false, latency: 64 });
    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 1, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 2, false).unwrap();
    let t = ur.terms().unwrap();
    for (u, lat) in [(&ur, 64), (&ua, 0), (&ub, 0)] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), lat, stops.clone()), t));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }

    start_take(&device);
    let Removal::Sent(op_rem) = host.remove_unit(5).unwrap() else { panic!("expected Sent") };
    let deadline = Instant::now() + Duration::from_secs(2);
    while ur.poll(op_rem) != OpState::Held {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert!(Instant::now() < deadline, "removal never entered Held");
        std::thread::sleep(Duration::from_millis(2));
    }

    // While R's removal is Held at plate index 0 (`[R, A, B]`), move B to plate index 1 (ahead of A).
    let op_mv = host.move_unit(20, 1).unwrap();
    assert!(device.wait_blocks(5));
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    // While the removal is still held, the move stays queued behind it and neither engine nor model
    // order has shifted yet.
    assert_eq!(ur.poll(op_rem), OpState::Held, "removal stays Held during take");
    assert_eq!(ub.poll(op_mv), OpState::Pending, "move stays queued behind the held removal");
    assert_eq!(
        engine_chain_ids(&device, 0),
        vec![(5, false), (10, false), (20, false)],
        "engine order unchanged while removal is still held"
    );
    let held_snap = host.core.chain_snapshot();
    assert!(held_snap.chains[0].held);
    assert_eq!(
        held_snap.chains[0].units.iter().map(|u| u.id).collect::<Vec<_>>(),
        vec![5, 10, 20],
        "model order unchanged while removal is still held"
    );

    stop_take(&device);
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_mv, Duration::from_secs(2)).unwrap(), OpState::Applied);

    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false), (10, false)]);
    let snap = host.core.chain_snapshot();
    assert!(!snap.chains[0].held);
    assert_eq!(snap.chains[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![20, 10]);
    ur.release();
    ua.release();
    ub.release();
}

/// 14. A removal of a parked seat settles (`Applied`) and posts `Back::Dismissed`.
#[test]
fn removal_of_parked_seat_settles_and_posts_dismissed() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("p")), 0, false).unwrap();
    u.activated(Activated { name: "p".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Withdraw and park the unit (the seat stays in the engine with no unit inside it).
    let op_w = u.withdraw().unwrap();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let box_w = loop {
        if let Some(b) = u.take_back() {
            break b;
        }
        assert!(Instant::now() < deadline, "withdrawn box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(matches!(box_w, Back::Withdrawn { .. }));
    u.parked("reseat refused".into());
    let snap = host.core.chain_snapshot();
    assert!(snap.chains[0].units[0].parked);
    assert_eq!(snap.chains[0].units[0].state, PlateState::Failed);

    // Remove the parked seat -> settles Applied and posts Back::Dismissed.
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(host.core.chain_snapshot().chains[0].units.is_empty());
    assert!(matches!(u.take_back(), Some(Back::Dismissed)));
    u.release();
}

/// 15. Shutdown with a take open and the feed stopped ends inside 3 s: `wait_op` drains via stash
///     after `STASH_AFTER`, meets `Held` with `closing` set, returns at once, and `host.shutdown()`
///     evicts the unit back to its owner.
#[test]
fn shutdown_with_take_open_and_feed_stopped_ends_well_inside_its_bound() {
    let fake = FakeDriver(Fake::new());
    let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 2.0 }, fake).unwrap();
    host.open(
        DeviceRequest {
            backend: AudioBackend::Asio,
            input: None,
            output: None,
            input_channels: [None; lf_engine::SLOT_COUNT],
            buffer: Some(256),
            sample_rate: None,
        },
        false,
    )
    .unwrap();

    let stops = Arc::new(AtomicUsize::new(0));
    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
    u.activated(Activated { name: "lat".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Start a recording take on lane 0, draining via `drain_feed` (simulating a ticking feed).
    host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let mut ev = Vec::new();
        let (_, ov) = host.drain_feed(&mut ev);
        if ov.is_some_and(|o| {
            let l = o.lane(0);
            l.state == LaneState::Recording && l.frames > 0
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "take never started");
        std::thread::sleep(Duration::from_millis(2));
    }

    // Now the feed STOPS (no more `drain_feed` calls), and shutdown begins with the take open!
    let began = Instant::now();
    host.set_closing();
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    let res = host.wait_op(op_rem, Duration::from_secs(10)).unwrap();
    assert_eq!(res, OpState::Held, "closing + held returns Held at once via stash drain");

    // Host shutdown evicts the remaining unit back to its owner as Removed (pending remove).
    host.shutdown();
    let elapsed = began.elapsed();
    // A held removal that never returned would wait out `wait_op`'s 10 s; 6.5 s leaves room for a
    // loaded test run (3-4 s seen in full runs, under 1 s alone).
    assert!(elapsed < Duration::from_millis(6500), "shutdown took {} ms (must be < 6500 ms)", elapsed.as_millis());
    let back = u.take_back().expect("shutdown evicted unit to its owner");
    assert!(matches!(back, Back::Removed(_)));
    assert_eq!(stops.load(SeqCst), 1);
    u.release();
}

/// 16. Finding 1: a legacy install of a latent unit during a take returns within the settle bound
///     (not after the take), seats when the take ends, and a refusal at the take's end reaches
///     `take_evicted` with its reason.
#[test]
#[ignore = "quarantined: on CI's Windows runner the take never starts (start_take, 30 s); passes locally; src-tauri/OPEN-THREADS.md"]
fn legacy_install_during_take_returns_within_settle_seats_on_end_and_surfaces_refusal() {
    // Part A: latent legacy unit installed during an open take returns within SETTLE while the
    // take is still recording, and seats once the take ends.
    {
        let device = TestDevice::start(48_000, 64, Duration::from_millis(5), |_| 0.0);
        let host = device.host();
        let slot = host.slot(0);
        let stops = Arc::new(AtomicUsize::new(0));

        start_take(&device);

        let settle = Duration::from_secs(2);
        let began = Instant::now();
        let installed = std::thread::scope(|s| {
            let installer = s.spawn(|| slot.install(LatentUnit::new(11, 64, stops.clone()), 48_000));
            let deadline = Instant::now() + settle;
            while !installer.is_finished() && Instant::now() < deadline {
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                std::thread::sleep(Duration::from_millis(5));
            }
            let finished_in_time = installer.is_finished();
            let elapsed = began.elapsed();
            if !finished_in_time {
                // End the take so the scoped thread can unwind cleanly before asserting.
                stop_take(&device);
                while !installer.is_finished() {
                    let mut ev = Vec::new();
                    host.drain_feed(&mut ev);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let res = installer.join().unwrap();
            (finished_in_time, elapsed, res)
        });
        assert!(
            installed.0 && installed.1 < settle,
            "legacy install during take must return within SETTLE (2 s), took {} ms",
            installed.1.as_millis()
        );
        assert!(installed.2.is_ok(), "held legacy install returns Ok");

        // Still recording and still held (not seated yet).
        let mut ev = Vec::new();
        let (_, ov) = host.drain_feed(&mut ev);
        assert_eq!(ov.unwrap().lane(0).state, LaneState::Recording, "take is still open when install returns");
        assert!(engine_chain_ids(&device, 0).is_empty(), "latent unit must not seat while take is open");

        // End the take: the unit seats in slot 0.
        stop_take(&device);
        let seat_deadline = Instant::now() + Duration::from_secs(2);
        let id = host.core.legacy_unit_id(0);
        assert_ne!(id, 0);
        while engine_chain_ids(&device, 0) != vec![(id, false)] {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            assert!(Instant::now() < seat_deadline, "held legacy unit never seated after take ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let back = slot.remove(Duration::from_secs(2)).expect("remove").expect("unit");
        assert_eq!(back.into_any().downcast::<LatentUnit>().unwrap().tag, 11);
        assert_eq!(stops.load(SeqCst), 1);
    }

    // Part B: a legacy install held behind an open take whose verdict at the take's end is
    // Refused(Latency) returns within SETTLE, then delivers Back::Refused { why: Latency }
    // through the mailbox and `take_evicted`, freeing the slot.
    {
        let device = TestDevice::start(48_000, 64, Duration::from_millis(5), |_| 0.0);
        let host = device.host();
        let stops = Arc::new(AtomicUsize::new(0));

        // Seat a 64-frame blocker directly on slot 0 (no table row) and hold its removal during a
        // take so the legacy install queues behind the held op and is judged when the take ends.
        let blocker_id: UnitId = 999;
        let blocker_tok: u64 = 999_999;
        let blocker_op = {
            let mut port = host.core.ports[0].lock().unwrap();
            let mut state = host.core.state.lock().unwrap();
            let gen = host.core.engine_gen.load(std::sync::atomic::Ordering::Acquire);
            assert!(port
                .as_mut()
                .unwrap()
                .install_at(blocker_id, blocker_tok, 0, false, None, LatentUnit::new(99, 64, stops.clone()))
                .is_ok());
            let op = state.model.mint_op(blocker_tok, gen, 0);
            state.model.push_pending(
                0,
                super::chain::PendingOp {
                    op,
                    unit: blocker_id,
                    seat_op: lf_engine::SeatOp::Install,
                    state: OpState::Pending,
                    kind: super::chain::PendingKind::Install {
                        token: blocker_tok,
                        is_reload: false,
                        replaces: None,
                        at: 0,
                        source: false,
                    },
                },
            );
            op
        };
        assert_eq!(host.wait_op(blocker_op, Duration::from_secs(2)).unwrap(), OpState::Applied);

        start_take(&device);

        // Hold blocker's removal during the take.
        let rem_op = {
            let mut port = host.core.ports[0].lock().unwrap();
            let mut state = host.core.state.lock().unwrap();
            let gen = host.core.engine_gen.load(std::sync::atomic::Ordering::Acquire);
            assert!(port.as_mut().unwrap().remove(blocker_id));
            let op = state.model.mint_op(blocker_tok, gen, 0);
            state.model.push_pending(
                0,
                super::chain::PendingOp {
                    op,
                    unit: blocker_id,
                    seat_op: lf_engine::SeatOp::Remove,
                    state: OpState::Pending,
                    kind: super::chain::PendingKind::Remove { owner_token: None },
                },
            );
            op
        };
        let held_deadline = Instant::now() + Duration::from_secs(2);
        while {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            host.core.state.lock().unwrap().poll(rem_op) != OpState::Held
        } {
            assert!(Instant::now() < held_deadline, "blocker remove never entered Held");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Legacy install of an over-budget latent unit while slot 0 is held: must return Ok within
        // SETTLE (before the take ends), then be refused with SeatRefusal::Latency at take's end.
        let slot = host.slot(0);
        let past = max_chain_latency(48_000) + 1;
        let settle = Duration::from_secs(2);
        let began = Instant::now();
        let installed = std::thread::scope(|s| {
            let installer = s.spawn(|| slot.install(LatentUnit::new(77, past, stops.clone()), 48_000));
            let deadline = Instant::now() + settle;
            while !installer.is_finished() && Instant::now() < deadline {
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                std::thread::sleep(Duration::from_millis(5));
            }
            let finished_in_time = installer.is_finished();
            let elapsed = began.elapsed();
            if !finished_in_time {
                stop_take(&device);
                while !installer.is_finished() {
                    let mut ev = Vec::new();
                    host.drain_feed(&mut ev);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let res = installer.join().unwrap();
            (finished_in_time, elapsed, res)
        });
        assert!(
            installed.0 && installed.1 < settle,
            "held legacy install (to be refused) must return within SETTLE, took {} ms",
            installed.1.as_millis()
        );
        assert!(installed.2.is_ok(), "held legacy install returns Ok");

        // End the take: blocker leaves and the over-budget legacy install is refused with Latency.
        stop_take(&device);
        let ref_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            {
                let mut port = host.core.ports[0].lock().unwrap();
                let mut state = host.core.state.lock().unwrap();
                if let Some(p) = port.as_mut() {
                    state.table.reap_port(0, p);
                }
                if state.table.legacy_has_evicted(0) {
                    break;
                }
            }
            assert!(Instant::now() < ref_deadline, "refused legacy unit never reached mailbox after take ended");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Verify the mailbox holds Back::Refused { why: SeatRefusal::Latency }, zero protocol errors,
        // and `take_evicted` hands the unit back and frees the slot.
        assert_eq!(host.core.chain_protocol_errors.load(SeqCst), 0);
        {
            let state = host.core.state.lock().unwrap();
            let id = state.table.legacy_unit_id(0);
            let row = state.table.active_row_for_id(id).expect("legacy row present");
            assert!(
                matches!(row.mailbox.front(), Some(Back::Refused { why: SeatRefusal::Latency, .. })),
                "expected Back::Refused(Latency) in mailbox"
            );
        }
        let back = slot.take_evicted().expect("refusal at take's end reaches take_evicted");
        assert_eq!(back.into_any().downcast::<LatentUnit>().unwrap().tag, 77);
        assert!(!host.core.legacy_occupied(0), "slot freed after take_evicted");
    }
}

/// 17. Finding 2: shutdown through `mode::run_shutdown` (without setting `closing` by hand) with
///     a take open and a held legacy removal sets `closing` first, fails the held removal fast, and
///     ends inside 3 s.
#[test]
fn shutdown_mode_with_take_open_and_held_legacy_removal_ends_before_the_watchdog() {
    let fake = FakeDriver(Fake::new());
    let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 2.0 }, fake).unwrap();
    host.open(
        DeviceRequest {
            backend: AudioBackend::Asio,
            input: None,
            output: None,
            input_channels: [None; lf_engine::SLOT_COUNT],
            buffer: Some(256),
            sample_rate: None,
        },
        false,
    )
    .unwrap();
    let feed = super::feed::FeedThread::spawn(host.clone()).unwrap();

    let stops = Arc::new(AtomicUsize::new(0));
    let slot = host.slot(0);
    assert!(slot.install(LatentUnit::new(1, 64, stops.clone()), 48_000).is_ok());

    // Record a 1-bar loop, then open an indefinite overdub take on lane 0 so the capture holds
    // alignment indefinitely (far past 3 s) without needing a large loop buffer.
    host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedLength(true) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedBars(1.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let playing = {
            let ends = host.core.ends.lock().unwrap();
            ends.as_ref().is_some_and(|e| e.overview.lane(0).state == LaneState::Playing)
        };
        if playing {
            break;
        }
        assert!(Instant::now() < deadline, "initial 1-bar loop never committed");
        std::thread::sleep(Duration::from_millis(5));
    }
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let overdubbing = {
            let ends = host.core.ends.lock().unwrap();
            ends.as_ref().is_some_and(|e| e.overview.lane(0).state == LaneState::Overdubbing)
        };
        if overdubbing {
            break;
        }
        assert!(Instant::now() < deadline, "overdub take never started");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Run `mode::run_shutdown` WITHOUT calling `host.set_closing()` by hand, with a held legacy
    // removal inside the `unload_all` step (matching `plugins::unload_all` + `teardown`). A
    // watchdog stops the overdub past 7.2 s so a broken shutdown fails cleanly instead of hanging; the
    // 6.5 s bound below leaves room for a loaded test run (3-4 s seen in full runs, under 1 s alone).
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_wd = done.clone();
    let host_wd = host.clone();
    let watchdog = std::thread::spawn(move || {
        let limit = Instant::now() + Duration::from_millis(7200);
        while !done_wd.load(std::sync::atomic::Ordering::Acquire) {
            if Instant::now() >= limit {
                let _ = host_wd.send(TimedCommand { frame: None, command: Command::Stop(0) });
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let began = Instant::now();
    let mut remove_res = None;
    super::mode::run_shutdown(
        &host,
        &feed,
        || {},
        || {},
        || {
            let _ = host.silence_slot(0, Duration::from_secs(1));
            let res = slot.remove(Duration::from_secs(10));
            if res.is_err() {
                slot.abandon();
            }
            remove_res = Some(res);
        },
    );
    let elapsed = began.elapsed();
    done.store(true, std::sync::atomic::Ordering::Release);
    let _ = watchdog.join();

    assert!(
        elapsed < Duration::from_millis(6500),
        "mode::run_shutdown with held legacy removal took {} ms (must be < 6500 ms)",
        elapsed.as_millis()
    );
    assert!(
        remove_res.expect("unload ran").is_err(),
        "held legacy removal must fail fast while closing"
    );
    assert_eq!(stops.load(SeqCst), 1, "host.shutdown stopped and evicted the abandoned unit");
}

/// 18. Review round B, finding 1: a param answer the event ring took and nobody drained, then a
///     rebuild: the handoff drains the old ring into the model before it advances, so the model and
///     the unit rebuilt from it carry the new value (not the one before it).
#[test]
fn param_answer_in_the_undrained_ring_survives_a_rebuild() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.25);
    let host = device.host();
    let applied_log = ParamRecorder::new();
    let factory = ClampFactory { applied: applied_log.clone() };
    let level = UnitParam::Send(InputSendParam::EchoLevel);

    let init_params = vec![(level, 0.25)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init_params.clone() }, 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let (unit1, clamped1) = factory.build(UnitKind::Echo, &init_params, t1).unwrap();
    u.committed(clamped1);
    let op1 = ok_op(u.submit(unit1, t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);

    // The engine applies 0.7 and its ring takes the answer; nothing drains it before the rebuild.
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, level, 0.7) }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while applied_log.is_empty() {
        assert!(Instant::now() < deadline, "the engine never applied the param");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(device.wait_blocks(4));
    {
        let rt = host.core.rt.lock().unwrap();
        assert_eq!(rt.engine.as_ref().unwrap().unsent_unit_params().count(), 0, "the ring took the answer");
    }
    device.rebuild_at(44_100, 256);

    let model_params = host.core.chain_snapshot().chains[0].units[0].params.as_ref().expect("built-in params").0.clone();
    assert_eq!(model_params, vec![(level, 0.7)], "the model carries the answer the old ring held");
    let Back::Evicted { unit: old_unit, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
    drop(old_unit);
    let t2 = u.terms().unwrap();
    let (rebuilt, clamped2) = factory.build(UnitKind::Echo, &model_params, t2).unwrap();
    assert_eq!(clamped2, vec![(level, 0.7)], "the rebuilt unit carries it too");
    let op2 = ok_op(u.submit(rebuilt, t2));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.core.chain_snapshot().chains[0].units[0].params.as_ref().unwrap().0, vec![(level, 0.7)]);

    host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
    assert!(device.wait_blocks(15));
    let out = device.output.lock().unwrap();
    let tail = &out[out.len().saturating_sub(64)..];
    assert_eq!(tail.len(), 64);
    for &sample in tail {
        assert!(
            (sample - 0.175).abs() < 1e-3,
            "rebuilt fake must render input 0.25 * 0.7 = 0.175, got {sample}"
        );
    }
    drop(out);
    u.release();
}

/// 19. Review round B, finding 2: a refused install whose box a port reader filed before its `Seat`
///     reached the model (the event ring full) settles `Refused` across a rebuild, once: one
///     `Back::Refused`, no `Rebuilt` retry.
#[test]
fn refused_install_reaped_before_a_rebuild_settles_refused_once() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host();
    fill_event_ring(&device);

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("late")), 0, false).unwrap();
    let terms = u.terms().unwrap();
    let past = max_chain_latency(48_000) + 1;
    let op = ok_op(u.submit(LatentUnit::new(88, past, Arc::new(AtomicUsize::new(0))), terms));

    // Another reader of the port (here the test) files the refused box into the row's mailbox; its
    // `Seat` is owed behind the full ring, so the op is still pending in the model.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let mut port = host.core.ports[0].lock().unwrap();
            let mut state = host.core.state.lock().unwrap();
            if let Some(p) = port.as_mut() {
                state.table.reap_port(0, p);
            }
            if state.table.active_row_for_id(10).is_some_and(|r| !r.mailbox.is_empty()) {
                break;
            }
        }
        assert!(Instant::now() < deadline, "the refused box never came back");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Inspect the model directly so we do not trigger stale-feed draining on the intentionally full ring.
    assert_eq!(host.core.state.lock().unwrap().poll(op), OpState::Pending, "its Seat has not reached the model");

    device.rebuild_at(44_100, 256);
    assert_eq!(u.poll(op), OpState::Refused(SeatRefusal::Latency), "settled from what the row received");
    let Back::Refused { unit, why } = u.take_back().expect("the refused box") else { panic!("expected Back::Refused") };
    assert_eq!(why, SeatRefusal::Latency);
    assert_eq!(unit.into_any().downcast::<LatentUnit>().unwrap().tag, 88);
    assert!(u.take_back().is_none(), "one box, once");
    u.release();
}

/// 20. Review round B, finding 3: plates `[R, A, B]`, a queued removal of `R`, then a move of `B` ahead
///     of `A` (plate index 1, engine index 0), both pending at a rebuild: the model ends `[B, A]`, and
///     so does the rebuilt engine's chain.
#[test]
fn pending_move_behind_a_pending_removal_keeps_its_place_across_a_rebuild() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ur = host.unit(0, 5, Spec::Plugin(plugin_ref("r")), 0, false).unwrap();
    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 1, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 2, false).unwrap();
    let t = ur.terms().unwrap();
    for u in [&ur, &ua, &ub] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), 0, stops.clone()), t));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(5, false), (10, false), (20, false)]);

    // Nothing renders: both messages stay queued in the port until the rebuild.
    device.stop();
    let Removal::Sent(op_rem) = host.remove_unit(5).unwrap() else { panic!("expected Sent") };
    let op_mv = host.move_unit(20, 1).unwrap();
    device.rebuild_at(44_100, 256);
    assert_eq!(ur.poll(op_rem), OpState::Applied);
    assert_eq!(ub.poll(op_mv), OpState::Applied);
    let ids = |host: &EngineHost| host.core.chain_snapshot().chains[0].units.iter().map(|u| u.id).collect::<Vec<_>>();
    assert_eq!(ids(&host), vec![20, 10], "the model's order after the rebuild");

    assert!(matches!(ur.take_back(), Some(Back::Removed(_))));
    ur.release();
    let t2 = ua.terms().unwrap();
    for u in [&ua, &ub] {
        let Some(Back::Evicted { unit, .. }) = u.take_back() else { panic!("expected Evicted") };
        let op = ok_op(u.submit(unit, t2));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false), (10, false)], "the rebuilt engine's chain");
    assert_eq!(ids(&host), vec![20, 10]);
    ua.release();
    ub.release();
}

/// 21. Review round B, finding 4: a feed drain between the handoff (the model advanced and settled)
///     and the swap's publish of the new generation gets the old generation with the old engine's
///     chains, never the new chains; after the publish, the new generation with the new chains.
#[test]
fn a_drain_mid_swap_never_pairs_the_new_chains_with_the_old_generation() {
    use std::sync::Weak;
    /// Drains the feed from the hook's `rebuild`: after the handoff, before the publish.
    struct DrainsMidSwap(Weak<super::Core>, Mutex<Vec<(u64, Frame)>>);
    impl super::RebuildHook for DrainsMidSwap {
        fn pause(&self) {}
        fn rebuild(&self, _generation: u64) -> Vec<Command> {
            let host = EngineHost { core: self.0.upgrade().expect("the host lives") };
            let mut ev = Vec::new();
            let drain = host.drain_feed_taps(&mut ev, None, None, None);
            self.1.lock().unwrap().push((drain.gen, drain.chains.chains[0].latency));
            Vec::new()
        }
        fn resume(&self) {}
    }
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    u.activated(Activated { name: "a".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op = ok_op(u.submit(LatentUnit::new(1, 64, Arc::new(AtomicUsize::new(0))), t));
    assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    let before = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(before.chains.chains[0].latency, 64);

    let hook = Arc::new(DrainsMidSwap(Arc::downgrade(&host.core), Mutex::new(Vec::new())));
    host.set_rebuild_hook(Some(hook.clone()));
    device.rebuild_at(44_100, 256);
    host.set_rebuild_hook(None);

    assert_eq!(
        *hook.1.lock().unwrap(),
        vec![(before.gen, 64)],
        "mid-swap: the old generation with the old engine's chains"
    );
    let after = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!((after.gen, after.chains.chains[0].latency), (before.gen + 1, 0), "after the publish: the new pair");
    u.release();
}

/// 22. Review round C, finding 1a: unit commands sent between a withdrawal and its reseat are
///     held in the gate and all reach the reseated unit in order (none dropped on the empty seat).
#[test]
fn commands_sent_between_withdrawal_and_reseat_reach_reseated_unit_in_order() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let applied_log = ParamRecorder::new();
    let level = UnitParam::Send(InputSendParam::EchoLevel);

    let init = vec![(level, 0.25)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    let t = u.terms().unwrap();
    let op1 = ok_op(u.submit(ClampEcho::new(0.25, 0, applied_log.clone()), t));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);

    // Withdraw unit 10; send one command immediately (while Withdraw is in flight), wait for the
    // withdrawn box, then send bypass + param commands while the seat is empty.
    let op_w = u.withdraw().unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, level, 0.4) }).unwrap();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let back = loop {
        if let Some(b) = u.take_back() {
            break b;
        }
        assert!(Instant::now() < deadline, "withdrawn box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    let Back::Withdrawn { unit: box_w, .. } = back else {
        panic!("expected Back::Withdrawn");
    };

    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, level, 0.8) }).unwrap();
    assert!(device.wait_blocks(5));
    host.drain_feed(&mut ev);
    assert!(applied_log.is_empty(), "commands must stay gated while seat is withdrawn");

    // Reseat unit 10: gated commands flush in order once the reseat applies.
    let op_reseat = ok_op(u.submit(box_w, t));
    assert_eq!(host.wait_op(op_reseat, Duration::from_secs(2)).unwrap(), OpState::Applied);
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);

    assert_eq!(applied_log.snapshot(), vec![0.4, 0.8], "all param commands reach the reseated unit in order");
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)], "bypass command reached the reseated unit");
    assert_eq!(host.diag().engine.unit_commands_dropped, 0, "no unit command dropped on the empty seat");
    let snap = host.core.chain_snapshot();
    assert!(snap.chains[0].units[0].bypass);
    assert_eq!(snap.chains[0].units[0].params.as_ref().unwrap().0, vec![(level, 0.8)]);
    u.release();
}

/// 23. Review round C, finding 1b: after a rebuild, a timely submit that is then held across the
///     gate deadline keeps its gated commands and does not fail its plate.
#[test]
fn timely_submit_held_across_gate_deadline_keeps_gated_commands() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host();
    let applied_log = ParamRecorder::new();
    let level = UnitParam::Send(InputSendParam::EchoLevel);

    let init = vec![(level, 0.25)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    u.activated(Activated { name: "echo".into(), source: false, latency: 64 });
    let t1 = u.terms().unwrap();
    let op1 = ok_op(u.submit(ClampEcho::new(0.25, 64, applied_log.clone()), t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Rebuild under a controllable clock, then open a take on the new engine so the latent reseat is Held.
    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);
    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: old_box, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
    start_take(&device);

    // Commands sent while reseating enter the gate.
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, level, 0.6) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();

    // Timely submit before the deadline: held because of the open take.
    let t2 = u.terms().unwrap();
    let op2 = ok_op(u.submit(old_box, t2));
    let deadline = Instant::now() + Duration::from_secs(2);
    while u.poll(op2) != OpState::Held {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert!(Instant::now() < deadline, "reseat never entered Held");
        std::thread::sleep(Duration::from_millis(2));
    }

    // Advance the controllable clock past GATE_DEADLINE while the timely submit is still Held;
    // flush the gate via a feed drain and send another unit command after the deadline.
    host.core.state.lock().unwrap().model.advance_clock(GATE_DEADLINE + Duration::from_secs(5));
    let mut ev = Vec::new();
    let mid = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 0, "timely submit must not drop gated commands");
    assert_eq!(mid.chains.chains[0].units[0].state, PlateState::Waiting, "plate stays Waiting, not Failed");
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(10, level, 0.9) }).unwrap();
    assert_eq!(host.core.gate_dropped.load(SeqCst), 0, "post-deadline command stays gated while submit is held");

    // End the take: reseat applies and all gated commands reach the unit in order.
    stop_take(&device);
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);

    assert_eq!(applied_log.snapshot(), vec![0.6, 0.9], "all gated param commands reached the reseated unit in order");
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)], "gated bypass command reached the reseated unit");
    u.release();
}

/// 24. Review round C, finding 2: a remove queued behind a withdraw always delivers one terminal
///     box (`Back::Removed`) or `Back::Dismissed`, never leaving the owner waiting.
#[test]
fn remove_queued_behind_withdraw_delivers_terminal_box_or_dismissed() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    // Part A: withdraw and remove queued back-to-back while the withdrawing row still holds its box.
    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("w")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(101, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    device.stop();
    let op_w = u.withdraw().unwrap();
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    device.resume();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));

    let mut backs = Vec::new();
    while let Some(b) = u.take_back() {
        backs.push(b);
    }
    let has_terminal = backs.iter().any(|b| matches!(b, Back::Removed(_) | Back::Dismissed));
    assert!(
        has_terminal,
        "owner must receive Back::Removed or Back::Dismissed after remove queued behind withdraw; got {} item(s)",
        backs.len()
    );
    let terminal_count = backs.iter().filter(|b| matches!(b, Back::Removed(_) | Back::Dismissed)).count();
    assert_eq!(terminal_count, 1, "owner gets exactly one terminal signal");
    let box_count = backs
        .iter()
        .filter(|b| matches!(b, Back::Removed(_) | Back::Withdrawn { .. }))
        .count();
    assert_eq!(box_count, 1, "owner gets its unit box back once");
    u.release();

    // Part B: withdraw finishes and owner takes Back::Withdrawn (not parked), then remove_unit
    // removes the empty seat -> owner receives Back::Dismissed.
    let u2 = host.unit(0, 20, Spec::Plugin(plugin_ref("w2")), 0, false).unwrap();
    let op_in2 = ok_op(u2.submit(LatentUnit::new(202, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_in2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let op_w2 = u2.withdraw().unwrap();
    assert_eq!(host.wait_op(op_w2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let back_w2 = loop {
        if let Some(b) = u2.take_back() {
            break b;
        }
        assert!(Instant::now() < deadline, "withdrawn box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(matches!(back_w2, Back::Withdrawn { .. }));
    let Removal::Sent(op_rem2) = host.remove_unit(20).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_rem2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(matches!(u2.take_back(), Some(Back::Dismissed)), "owner gets Back::Dismissed for withdrawn seat");
    assert!(u2.take_back().is_none());
    u2.release();

    // Part C: latent unit with withdraw + remove queued during an open take (Withdraw completes
    // while Remove is Held): owner still gets one terminal Back::Removed, never left waiting.
    let u3 = host.unit(0, 30, Spec::Plugin(plugin_ref("w3")), 0, false).unwrap();
    u3.activated(Activated { name: "w3".into(), source: false, latency: 64 });
    let op_in3 = ok_op(u3.submit(LatentUnit::new(303, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in3, Duration::from_secs(2)).unwrap(), OpState::Applied);
    start_take(&device);
    let op_w3 = u3.withdraw().unwrap();
    let Removal::Sent(op_rem3) = host.remove_unit(30).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_w3, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));
    stop_take(&device);
    assert_eq!(host.wait_op(op_rem3, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut backs3 = Vec::new();
    while let Some(b) = u3.take_back() {
        backs3.push(b);
    }
    assert!(
        backs3.iter().filter(|b| matches!(b, Back::Removed(_) | Back::Dismissed)).count() == 1,
        "held remove behind withdraw delivers one terminal signal"
    );
    u3.release();
}

/// 25. Review round C, finding 3: `poll` stays idempotent past 1,024 later completions (and across
///     a rebuild) while the row has not been released.
#[test]
fn poll_remains_idempotent_past_1024_completions() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    device.stop();
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let uc = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 2, false).unwrap();
    let t = ua.terms().unwrap();

    let op_applied = ok_op(ua.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_applied, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let past = max_chain_latency(48_000) + 1;
    let op_refused = ok_op(ub.submit(LatentUnit::new(2, past, stops.clone()), t));
    assert_eq!(host.wait_op(op_refused, Duration::from_secs(2)).unwrap(), OpState::Refused(SeatRefusal::Latency));
    assert!(matches!(ub.take_back(), Some(Back::Refused { why: SeatRefusal::Latency, .. })));

    let op_c = ok_op(uc.submit(LatentUnit::new(3, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_c, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Run 1,100 later completions on slot 0 (move C back and forth while idle).
    let mut ev = Vec::new();
    for i in 0..1100usize {
        let target = if i % 2 == 0 { 0 } else { 1 };
        let op_mv = host.move_unit(30, target).unwrap();
        super::unit_host::service_if_idle(&host.core);
        ev.clear();
        host.drain_feed(&mut ev);
        assert_eq!(uc.poll(op_mv), OpState::Applied);
    }

    assert_eq!(ua.poll(op_applied), OpState::Applied, "Applied outcome must survive >1024 later completions");
    assert_eq!(uc.poll(op_c), OpState::Applied, "same-row Applied outcome must survive >1024 later completions");
    assert_eq!(
        ub.poll(op_refused),
        OpState::Refused(SeatRefusal::Latency),
        "Refused outcome must survive >1024 later completions"
    );

    // Across a rebuild, already-settled ops must still report their terminal outcome, never Rebuilt.
    device.rebuild_at(44_100, 256);
    assert_eq!(ua.poll(op_applied), OpState::Applied, "Applied outcome must not flip to Rebuilt");
    assert_eq!(uc.poll(op_c), OpState::Applied, "same-row Applied outcome must not flip to Rebuilt");
    assert_eq!(
        ub.poll(op_refused),
        OpState::Refused(SeatRefusal::Latency),
        "Refused outcome must not flip to Rebuilt"
    );
    assert!(ua.take_back().is_some());
    assert!(uc.take_back().is_some());
    ua.release();
    ub.release();
    uc.release();
}

/// 26. Review round C, finding 6: with no feed ticking, an install submitted through `UnitHost`
///     reads `Applied` from repeated `poll`s alone (both running and idle).
#[test]
fn poll_alone_settles_install_with_no_feed_ticking() {
    // Part A: running device, no feed drain, no wait_op.
    {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host();
        let u = host.unit(0, 10, Spec::Plugin(plugin_ref("run")), 0, false).unwrap();
        let t = u.terms().unwrap();
        let op = ok_op(u.submit(LatentUnit::new(1, 0, Arc::new(AtomicUsize::new(0))), t));
        let deadline = Instant::now() + Duration::from_secs(2);
        while u.poll(op) != OpState::Applied {
            assert!(Instant::now() < deadline, "running: poll alone never reached Applied with no feed ticking");
            std::thread::sleep(Duration::from_millis(2));
        }
        u.release();
    }
    // Part B: idle (stopped) device, no feed drain, no wait_op.
    {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        device.stop();
        let host = device.host();
        let u = host.unit(0, 10, Spec::Plugin(plugin_ref("idle")), 0, false).unwrap();
        let t = u.terms().unwrap();
        let op = ok_op(u.submit(LatentUnit::new(2, 0, Arc::new(AtomicUsize::new(0))), t));
        assert_eq!(u.poll(op), OpState::Applied, "idle: poll alone must service and drain to Applied");
        u.release();
    }
}

/// 27. Round D1 / Astra 1 (both branches): a removal queued during reactivation (or immediate
///     after a rebuild) makes `submit` return `SubmitError::Removing` (never `Superseded`), and
///     delivers `Back::Dismissed` once the removal settles.
#[test]
fn d1_astra1_removal_during_reactivation_and_immediate_returns_removing() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    // Branch 1: withdrawn latent unit, Sent remove while owner holds the box.
    {
        let u = host.unit(0, 10, Spec::Plugin(plugin_ref("a1")), 0, false).unwrap();
        u.activated(Activated { name: "a1".into(), source: false, latency: 64 });
        let t = u.terms().unwrap();
        let op_in = ok_op(u.submit(LatentUnit::new(101, 64, stops.clone()), t));
        assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let op_w = u.withdraw().unwrap();
        assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
        let deadline = Instant::now() + Duration::from_secs(2);
        let box_w = loop {
            if let Some(Back::Withdrawn { unit, .. }) = u.take_back() {
                break unit;
            }
            assert!(Instant::now() < deadline, "withdrawn box never arrived");
            std::thread::sleep(Duration::from_millis(2));
        };

        start_take(&device);
        let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
        let (box_w, err) = u.submit(box_w, t).expect_err("submit during pending remove must fail");
        assert_eq!(err, SubmitError::Removing, "pending remove must return SubmitError::Removing");
        stop_take(&device);
        assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
        assert!(matches!(u.take_back(), Some(Back::Dismissed)), "removal settle must deliver Back::Dismissed");
        assert!(u.take_back().is_none());
        drop(box_w);
        u.release();
    }

    // Branch 2: immediate remove after a rebuild while the owner holds the Evicted box.
    {
        let u = host.unit(0, 11, Spec::Plugin(plugin_ref("a2")), 0, false).unwrap();
        let t1 = u.terms().unwrap();
        let op_in = ok_op(u.submit(LatentUnit::new(102, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

        device.rebuild_at(44_100, 256);
        let Back::Evicted { unit: box_e, .. } = u.take_back().expect("evicted box") else {
            panic!("expected Back::Evicted");
        };
        let t2 = u.terms().unwrap();
        assert_eq!(host.remove_unit(11).unwrap(), Removal::Plate);
        let (box_e, err) = u.submit(box_e, t2).expect_err("submit after immediate remove must fail");
        assert_eq!(err, SubmitError::Removing, "immediate remove must return SubmitError::Removing");
        assert!(matches!(u.take_back(), Some(Back::Dismissed)), "immediate remove must deliver Back::Dismissed");
        assert!(u.take_back().is_none());
        drop(box_e);
        u.release();
    }
}

/// 28. Round D1 / Astra 2: while a Remove is `Held` behind a withdraw, a returned `Withdrawn` box
///     stays held outside the mailbox (`take_back()` is `None`) and `host.unit(id)` is refused;
///     once the Remove applies, `take_back()` delivers `Back::Removed(box)` once with the same tag.
#[test]
fn d1_astra2_held_remove_holds_box_and_reserves_id_until_settle() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 30, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
    u.activated(Activated { name: "lat".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(303, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    start_take(&device);
    let op_w = u.withdraw().unwrap();
    let Removal::Sent(op_rem) = host.remove_unit(30).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));

    // Poll until op_rem reads Held.
    let deadline = Instant::now() + Duration::from_secs(2);
    while u.poll(op_rem) != OpState::Held {
        assert!(Instant::now() < deadline, "remove never reached Held");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(u.take_back().is_none(), "withdrawn box must stay held while Remove is Held");
    assert!(
        host.unit(0, 30, Spec::Plugin(plugin_ref("dup")), 0, false).is_err(),
        "id must remain reserved while Leaving plate stands"
    );

    stop_take(&device);
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let Back::Removed(box_r) = u.take_back().expect("Removed box after settle") else {
        panic!("expected Back::Removed after held Remove settled");
    };
    assert_eq!(box_r.into_any().downcast::<LatentUnit>().unwrap().tag, 303);
    assert!(u.take_back().is_none(), "terminal box delivered exactly once");
    u.release();

    // Part B (D1 review): release the owner row while the Remove is still Held so `is_id_reserved`
    // is false; `host.unit(id)` must still be refused by the live-plate guard until Remove settles.
    let u2 = host.unit(0, 31, Spec::Plugin(plugin_ref("lat2")), 0, false).unwrap();
    u2.activated(Activated { name: "lat2".into(), source: false, latency: 64 });
    let op_in2 = ok_op(u2.submit(LatentUnit::new(304, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in2, Duration::from_secs(2)).unwrap(), OpState::Applied);

    start_take(&device);
    let op_w2 = u2.withdraw().unwrap();
    let Removal::Sent(op_rem2) = host.remove_unit(31).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_w2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));
    let deadline = Instant::now() + Duration::from_secs(2);
    while u2.poll(op_rem2) != OpState::Held {
        assert!(Instant::now() < deadline, "remove 2 never reached Held");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(u2.take_back_for_shutdown().is_some(), "claim held box before releasing owner");
    u2.release();
    assert!(
        !host.core.state.lock().unwrap().table.is_id_reserved(31),
        "owner row is gone after release"
    );
    assert!(
        host.unit(0, 31, Spec::Plugin(plugin_ref("dup2")), 0, false).is_err(),
        "id must remain reserved after owner release while Leaving plate stands"
    );
    stop_take(&device);
    assert_eq!(host.wait_op(op_rem2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let fresh = host.unit(0, 31, Spec::Plugin(plugin_ref("fresh")), 0, false).expect("id free after settle");
    fresh.release();
}

/// 29. Round D1 / X1 (Rule 3): a reload candidate submitted after a queued removal returns
///     `Err(SubmitError::Superseded)`; the engine chain ends empty and the original gets `Removed`.
#[test]
fn d1_x1_reload_candidate_submitted_after_queued_remove_is_superseded() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
    device.stop();
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    let (box2, err) = cand.submit(LatentUnit::new(2, 0, stops.clone()), t).expect_err("candidate submit during Leaving must fail");
    assert_eq!(err, SubmitError::Superseded);
    drop(box2);
    cand.release();

    device.resume();
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));
    assert!(engine_chain_ids(&device, 0).is_empty(), "engine chain must be empty");
    let Back::Removed(box1) = u.take_back().expect("original gets Removed") else {
        panic!("expected Back::Removed for original");
    };
    assert_eq!(box1.into_any().downcast::<LatentUnit>().unwrap().tag, 1);
    u.release();
}

/// 30. Round D1 / B1 (Rule 1): `withdraw` returns `SubmitError::Reloading` while a live candidate
///     row stands, and succeeds once the candidate releases.
#[test]
fn d1_b1_withdraw_returns_reloading_while_candidate_stands() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
    assert_eq!(u.withdraw(), Err(SubmitError::Reloading));
    cand.release();

    let op_w = u.withdraw().expect("withdraw must succeed once candidate released");
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    u.release();
}

/// 31. Round D1 / B2 (Rule 2): `reload_unit` refuses a unit with a pending withdrawal.
#[test]
fn d1_b2_reload_unit_refuses_pending_withdraw() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    device.stop();
    let _op_w = u.withdraw().unwrap();
    assert!(
        host.reload_unit(10, plugin_ref("v2")).is_err(),
        "reload_unit must refuse a pending withdraw"
    );
    device.resume();
    u.release();
}

/// 32. Round D1 / B3 (Rule 4 + Astra CR6): a rebuild promotes a candidate that idle service
///     already applied, and cancels a candidate still in flight (`Unsent` forced or unsubmitted).
#[test]
fn d1_b3_rebuild_promotes_applied_candidate_and_cancels_unsent_candidate() {
    // Part A: promoted candidate (reply room available -> idle service applies candidate before eviction).
    {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let orig = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
        orig.activated(Activated { name: "v1".into(), source: false, latency: 0 });
        let t1 = orig.terms().unwrap();
        let op1 = ok_op(orig.submit(LatentUnit::new(1, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
        cand.activated(Activated { name: "v2".into(), source: false, latency: 0 });
        device.stop();
        let op_cand = ok_op(cand.submit(LatentUnit::new(2, 0, stops.clone()), t1));
        device.rebuild_at(44_100, 256);

        assert_eq!(cand.poll(op_cand), OpState::Applied, "idle-applied candidate stays Applied");
        let Back::Superseded(box1) = orig.take_back().expect("orig gets Superseded") else {
            panic!("expected Superseded for original");
        };
        assert_eq!(box1.into_any().downcast::<LatentUnit>().unwrap().tag, 1);
        let Back::Evicted { unit: box2, .. } = cand.take_back().expect("cand gets Evicted") else {
            panic!("expected Evicted for promoted candidate");
        };
        let box2 = box2.into_any().downcast::<LatentUnit>().unwrap();
        assert_eq!(box2.tag, 2);
        orig.release();

        let t2 = cand.terms().unwrap();
        let op2 = ok_op(cand.submit(box2, t2));
        assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
        let snap = host.core.chain_snapshot();
        let unit = &snap.chains[0].units[0];
        assert_eq!(unit.state, PlateState::Seated);
        assert!(!unit.reloading);
        assert_eq!(unit.plugin.as_ref(), Some(&plugin_ref("v2")));
        cand.release();
    }

    // Part B: cancelled candidate (`Unsent` forced by filling owed replies).
    {
        let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let orig = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
        orig.activated(Activated { name: "v1".into(), source: false, latency: 0 });
        let t1 = orig.terms().unwrap();
        let op1 = ok_op(orig.submit(LatentUnit::new(1, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

        // Seat a helper unit 1, fill event ring, and exhaust owed replies on slot 0.
        let helper = host.unit(0, 1, Spec::Plugin(plugin_ref("h")), 1, false).unwrap();
        let op_h = ok_op(helper.submit(LatentUnit::new(50, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op_h, Duration::from_secs(2)).unwrap(), OpState::Applied);

        fill_event_ring(&device);
        device.stop();
        for _ in 0..8u32 {
            let _ = helper.withdraw().unwrap();
            super::unit_host::service_if_idle(&host.core);
            let Back::Withdrawn { unit, .. } = helper.take_back().expect("helper withdrawn") else {
                panic!("expected Withdrawn for helper");
            };
            let _ = ok_op(helper.submit(unit, t1));
            super::unit_host::service_if_idle(&host.core);
        }

        let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
        cand.activated(Activated { name: "v2".into(), source: false, latency: 0 });
        let op_cand = ok_op(cand.submit(LatentUnit::new(2, 0, stops.clone()), t1));
        super::unit_host::service_if_idle(&host.core);
        assert!(cand.take_back().is_none(), "candidate stayed queued (owed full)");

        device.rebuild_at(44_100, 256);

        assert_eq!(cand.poll(op_cand), OpState::Rebuilt);
        let Back::Superseded(box2) = cand.take_back().expect("cand gets Superseded") else {
            panic!("expected Back::Superseded for cancelled candidate");
        };
        assert_eq!(box2.into_any().downcast::<LatentUnit>().unwrap().tag, 2);
        cand.release();

        let Back::Evicted { unit: box1, .. } = orig.take_back().expect("orig gets Evicted") else {
            panic!("expected Back::Evicted for original after cancelled reload");
        };
        let box1 = box1.into_any().downcast::<LatentUnit>().unwrap();
        assert_eq!(box1.tag, 1);
        let t2 = orig.terms().unwrap();
        let op_res = ok_op(orig.submit(box1, t2));
        assert_eq!(host.wait_op(op_res, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let snap = host.core.chain_snapshot();
        let plate10 = snap.chains[0].units.iter().find(|u| u.id == 10).expect("plate 10");
        assert_eq!(plate10.state, PlateState::Seated);
        assert!(!plate10.reloading);
        assert_eq!(plate10.plugin.as_ref(), Some(&plugin_ref("v1")));

        let _ = helper.take_back();
        helper.release();
        orig.release();
    }

    // Part C: unsubmitted candidate across rebuild gets Superseded on later submit.
    {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let orig = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
        let t1 = orig.terms().unwrap();
        let op1 = ok_op(orig.submit(LatentUnit::new(1, 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let cand = host.reload_unit(10, plugin_ref("v2")).unwrap();
        device.rebuild_at(44_100, 256);

        let t2 = orig.terms().unwrap();
        let (box2, err) = cand.submit(LatentUnit::new(2, 0, stops.clone()), t2).expect_err("unsubmitted candidate must be Superseded after rebuild");
        assert_eq!(err, SubmitError::Superseded);
        drop(box2);
        cand.release();

        let Back::Evicted { unit: box1, .. } = orig.take_back().expect("orig evicted") else {
            panic!("expected Evicted");
        };
        let op2 = ok_op(orig.submit(box1, t2));
        assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
        let snap = host.core.chain_snapshot();
        assert!(!snap.chains[0].units[0].reloading);
        assert_eq!(snap.chains[0].units[0].plugin.as_ref(), Some(&plugin_ref("v1")));
        orig.release();
    }
}

/// 33. Round D1 / Astra CR1: removal queued behind a candidate that the engine refuses for latency
///     retires the original (`Removed`), while the candidate receives `Refused` (even if both boxes
///     are reaped before either `Event::Seat` drains). Also verifies the applied-candidate branch.
#[test]
fn d1_cr1_removal_behind_refused_reload_retires_original_not_candidate() {
    // Part A: candidate B refused for latency -> A removed; reap both boxes before draining events.
    {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let a = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
        let t = a.terms().unwrap();
        let op_a = ok_op(a.submit(LatentUnit::new(1, 0, stops.clone()), t));
        assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let b = host.reload_unit(10, plugin_ref("v2")).unwrap();
        device.stop();
        let past = max_chain_latency(48_000) + 1;
        let op_b = ok_op(b.submit(LatentUnit::new(2, past, stops.clone()), t));
        let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };

        // Service idle without draining Event::Seat: engine refuses B (latency) then removes A.
        super::unit_host::service_if_idle(&host.core);

        // Reap both boxes BEFORE draining either Event::Seat!
        let back_a = a.take_back().expect("A's box must be ready");
        let back_b = b.take_back().expect("B's box must be ready");

        assert!(
            matches!(back_a, Back::Removed(_)),
            "original A must receive Back::Removed (retires tone), not Superseded"
        );
        assert!(
            matches!(back_b, Back::Refused { why: SeatRefusal::Latency, .. }),
            "refused candidate B must receive Back::Refused(Latency), not Removed"
        );

        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert_eq!(b.poll(op_b), OpState::Refused(SeatRefusal::Latency));
        assert_eq!(a.poll(op_rem), OpState::Applied);
        assert!(a.take_back().is_none(), "no duplicate terminal for A");
        assert!(b.take_back().is_none(), "no duplicate terminal for B");
        a.release();
        b.release();
    }

    // Part B: candidate B applies -> B removed; reap both boxes before draining events.
    {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let a = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
        let t = a.terms().unwrap();
        let op_a = ok_op(a.submit(LatentUnit::new(1, 0, stops.clone()), t));
        assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let b = host.reload_unit(10, plugin_ref("v2")).unwrap();
        device.stop();
        let op_b = ok_op(b.submit(LatentUnit::new(2, 0, stops.clone()), t));
        let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };

        super::unit_host::service_if_idle(&host.core);

        let back_a = a.take_back().expect("A's box must be ready");
        let back_b = b.take_back().expect("B's box must be ready");

        assert!(
            matches!(back_a, Back::Superseded(_)),
            "replaced original A must receive Back::Superseded (does not retire)"
        );
        assert!(
            matches!(back_b, Back::Removed(_)),
            "applied then removed candidate B must receive Back::Removed (retires)"
        );

        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert_eq!(b.poll(op_b), OpState::Applied);
        assert_eq!(a.poll(op_rem), OpState::Applied);
        assert!(a.take_back().is_none(), "no duplicate terminal for A");
        assert!(b.take_back().is_none(), "no duplicate terminal for B");
        a.release();
        b.release();
    }
}

/// 34. Round D1 / Astra CR2: a failed reload C reserved during A->B's outgoing crossfade must not
///     clear A's superseded state or allow A to withdraw B.
#[test]
fn d1_cr2_failed_reload_during_crossfade_does_not_revive_older_owner() {
    let mut device = TestDevice::start(48_000, 64, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    let t = a.terms().unwrap();
    let op_a = ok_op(a.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let b = host.reload_unit(10, plugin_ref("v2")).unwrap();
    device.stop();
    let op_b = ok_op(b.submit(LatentUnit::new(2, 0, stops.clone()), t));

    // Render 2 blocks (128 < 480 frames): B applied, A still crossfading out.
    {
        let mut rt = host.core.rt.lock().unwrap();
        let engine = rt.engine.as_mut().unwrap();
        let (x, mut l, mut r) = ([0.0f32; 64], [0.0f32; 64], [0.0f32; 64]);
        for k in 0..2i64 {
            let ctx = lf_engine::ProcessContext { frame: k * 64, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            engine.process(&ctx, &x, &mut l, &mut r);
        }
    }
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(b.poll(op_b), OpState::Applied);
    assert!(a.take_back().is_none(), "A is still crossfading out");

    // Reserve C during the outgoing crossfade, fail C, and release C.
    let c = host.reload_unit(10, plugin_ref("v3")).unwrap();
    c.failed("load failed".into());
    c.release();

    // A must remain permanently superseded and must NOT be allowed to withdraw B!
    assert_eq!(a.withdraw(), Err(SubmitError::Superseded));

    device.resume();
    assert!(device.wait_blocks(15));
    assert!(matches!(a.take_back(), Some(Back::Superseded(_))));
    assert!(b.take_back().is_none(), "B must remain seated in the engine");
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)]);
    a.release();
    b.release();
}

/// 35. Round D1 / Astra CR3: `Pending { held }` adopts any unclaimed non-terminal mailbox return
///     on `remove_unit` entry, and a repeat `remove_unit` returns the existing removal op without
///     dropping the held box.
#[test]
fn d1_cr3_pending_held_adopts_unclaimed_refused_box_and_retry_reuses_op() {
    // Part A: refused initial install -> remove_unit reaps Refused box -> adopts into held until
    // Remove settles (Refused(Unknown) -> Applied), then removes the plate and delivers Removed.
    {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let u = host.unit(0, 10, Spec::Plugin(plugin_ref("bad")), 0, false).unwrap();
        let t = u.terms().unwrap();
        device.stop();
        let past = max_chain_latency(48_000) + 1;
        let op_in = ok_op(u.submit(LatentUnit::new(10, past, stops.clone()), t));
        // Service idle so engine refuses the install and puts the Refused box on the return ring,
        // WITHOUT draining Event::Seat (so has_pending_install is still true).
        super::unit_host::service_if_idle(&host.core);

        let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
        // While Remove is pending, the Refused box must stay held (not handed out before Remove settles).
        assert!(
            u.take_back().is_none(),
            "unclaimed Refused box must be adopted into held while Remove is pending"
        );

        // Now service and drain: Install is Refused(Latency), Remove gets Refused(Unknown) -> settles
        // as Applied, removes the plate, and delivers the held box as Back::Removed.
        super::unit_host::service_if_idle(&host.core);
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert_eq!(u.poll(op_in), OpState::Refused(SeatRefusal::Latency));
        assert_eq!(u.poll(op_rem), OpState::Applied);
        assert!(matches!(u.take_back(), Some(Back::Removed(_))));
        assert!(host.core.chain_snapshot().chains[0].units.is_empty());
        u.release();
    }

    // Part B: repeat remove_unit after a withdrawn box entered `held` during a take returns the
    // existing removal op and does not lose the held box.
    {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let u = host.unit(0, 31, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
        u.activated(Activated { name: "lat".into(), source: false, latency: 64 });
        let t = u.terms().unwrap();
        let op_in = ok_op(u.submit(LatentUnit::new(311, 64, stops.clone()), t));
        assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

        start_take(&device);
        let op_w = u.withdraw().unwrap();
        let Removal::Sent(op_rem1) = host.remove_unit(31).unwrap() else { panic!("expected Sent") };
        assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
        assert!(device.wait_blocks(15));

        // Reap Withdrawn into `held`.
        assert!(u.take_back().is_none());
        // Retry remove_unit while Remove is still Held and box is in `held`.
        let Removal::Sent(op_rem2) = host.remove_unit(31).unwrap() else { panic!("expected Sent on retry") };
        assert_eq!(op_rem2, op_rem1, "retry remove_unit must return existing removal op");

        stop_take(&device);
        assert_eq!(host.wait_op(op_rem1, Duration::from_secs(2)).unwrap(), OpState::Applied);
        let Back::Removed(box_r) = u.take_back().expect("held box must survive retry remove_unit") else {
            panic!("expected Back::Removed");
        };
        assert_eq!(box_r.into_any().downcast::<LatentUnit>().unwrap().tag, 311);
        u.release();
    }
}

/// 36. Round D1 / Astra CR4: a stopping owner can claim a box held in `Leave::Pending` through
///     `take_back_for_shutdown` while preserving the removal reservation until `Remove` settles.
#[test]
fn d1_cr4_shutdown_claims_held_box_while_preserving_removal_reservation() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 32, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
    u.activated(Activated { name: "lat".into(), source: false, latency: 64 });
    let t = u.terms().unwrap();
    let op_in = ok_op(u.submit(LatentUnit::new(322, 64, stops.clone()), t));
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);

    start_take(&device);
    let op_w = u.withdraw().unwrap();
    let Removal::Sent(op_rem) = host.remove_unit(32).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(15));

    // Normal take_back still hides the held box while Remove is Held.
    assert!(u.take_back().is_none());
    // Teardown claims the held box via take_back_for_shutdown.
    let Back::Withdrawn { unit: box_s, .. } = u.take_back_for_shutdown().expect("shutdown claims held box") else {
        panic!("expected held Withdrawn box from take_back_for_shutdown");
    };
    assert_eq!(box_s.into_any().downcast::<LatentUnit>().unwrap().tag, 322);
    assert!(u.take_back_for_shutdown().is_none(), "held box claimed once");

    // Removal reservation still stands (both before and after owner release) while Remove is Held.
    assert!(host.unit(0, 32, Spec::Plugin(plugin_ref("dup")), 0, false).is_err());
    u.release();
    assert!(host.unit(0, 32, Spec::Plugin(plugin_ref("dup2")), 0, false).is_err());

    stop_take(&device);
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    // Once Remove settles and plate is gone, id 32 is free again.
    let u_new = host.unit(0, 32, Spec::Plugin(plugin_ref("fresh")), 0, false).expect("id free after settle");
    u_new.release();
}

/// 37. Round D1 / Astra CR5: adding a new source B under another id that replaces a parked source A
///     delivers `Back::Dismissed` to A's owner on B's apply.
#[test]
fn d1_cr5_different_id_source_replacing_parked_source_delivers_dismissed() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("src_a")), 0, false).unwrap();
    a.activated(Activated { name: "src_a".into(), source: true, latency: 0 });
    let t = a.terms().unwrap();
    let op_a = ok_op(a.submit(Box::new(LatentUnit { tag: 1, latency: 0, source: true, stops: stops.clone() }), t));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Withdraw A and park it (reactivation failed).
    let op_w = a.withdraw().unwrap();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let box_a = loop {
        if let Some(Back::Withdrawn { unit, .. }) = a.take_back() {
            break unit;
        }
        assert!(Instant::now() < deadline, "withdrawn source box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    a.parked("reactivation failed".into());
    drop(box_a);

    // Add source B under another id 20: replaces A's empty seat and deletes A's plate.
    let b = host.unit(0, 20, Spec::Plugin(plugin_ref("src_b")), 0, false).unwrap();
    b.activated(Activated { name: "src_b".into(), source: true, latency: 0 });
    let op_b = ok_op(b.submit(Box::new(LatentUnit { tag: 2, latency: 0, source: true, stops: stops.clone() }), t));
    assert_eq!(host.wait_op(op_b, Duration::from_secs(2)).unwrap(), OpState::Applied);

    assert!(
        matches!(a.take_back(), Some(Back::Dismissed)),
        "parked source A must receive Back::Dismissed when source B replaces its seat"
    );
    assert!(a.take_back().is_none());
    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false)]);
    a.release();
    b.release();
}

/// 38. Round D2 / Astra 3: reserving an id drops any stale gated commands left behind by an
///     earlier immediate removal under the same id.
#[test]
fn d2_astra3_unit_reservation_drops_stale_gated_commands_for_same_id() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let applied = ParamRecorder::new();

    let u1 = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false).unwrap();
    let t1 = u1.terms().unwrap();
    let op1 = ok_op(u1.submit(ClampEcho::new(0.5, 0, applied.clone()), t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: ev_box, .. } = u1.take_back().expect("evicted") else { panic!("expected Evicted") };
    drop(ev_box);
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10));

    assert_eq!(host.remove_unit(10).unwrap(), Removal::Plate);
    assert!(matches!(u1.take_back(), Some(Back::Dismissed)));
    u1.release();

    let u2 = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false).unwrap();
    let t2 = u2.terms().unwrap();
    let op2 = ok_op(u2.submit(ClampEcho::new(0.5, 0, applied), t2));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(5));
    ev.clear();
    host.drain_feed(&mut ev);

    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)], "stale gated bypass must not reach new unit");
    assert_eq!(host.core.gate_dropped.load(SeqCst), 1, "stale gated command must be counted as dropped");
    u2.release();
}

/// 39. Round D2 / Astra 6(a) + counter-read `push_all` Gone: a withdrawn seat expires after
///     `GATE_DEADLINE`, drops its gated commands, marks the plate `Failed`, and drops further sends
///     instead of forwarding them to the still-withdrawn seat; a late submit still seats cleanly.
#[test]
fn d2_astra6a_withdrawn_seat_expires_after_deadline_and_push_all_drops_gone() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let applied = ParamRecorder::new();

    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false).unwrap();
    let t = u.terms().unwrap();
    let op1 = ok_op(u.submit(ClampEcho::new(0.5, 0, applied), t));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);
    let op_w = u.withdraw().unwrap();
    assert_eq!(host.wait_op(op_w, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let deadline = Instant::now() + Duration::from_secs(2);
    let box_w = loop {
        if let Some(Back::Withdrawn { unit, .. }) = u.take_back() {
            break unit;
        }
        assert!(Instant::now() < deadline, "withdrawn box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };

    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10), "bypass gated while withdrawn");

    host.core.state.lock().unwrap().model.advance_clock(GATE_DEADLINE + Duration::from_secs(1));
    let mut ev = Vec::new();
    let drain = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 1, "withdrawn gate expired and dropped");
    assert_eq!(drain.chains.chains[0].units[0].state, PlateState::Failed, "expired withdrawn plate reads Failed");
    assert!(!host.core.state.lock().unwrap().gate.has(10));

    // A further send after expiry must be dropped and counted (not gated, and not forwarded to the
    // still-withdrawn seat where the engine would retain the bypass).
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert_eq!(host.core.gate_dropped.load(SeqCst), 2, "post-expiry send counted as dropped");
    assert!(!host.core.state.lock().unwrap().gate.has(10), "post-expiry send must not enter gate");

    let op2 = ok_op(u.submit(box_w, t));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(device.wait_blocks(5));
    ev.clear();
    host.drain_feed(&mut ev);
    assert_eq!(host.core.chain_snapshot().chains[0].units[0].state, PlateState::Seated);
    assert_eq!(
        engine_chain_ids(&device, 0),
        vec![(10, false)],
        "post-expiry bypass must not have been retained by the withdrawn seat"
    );
    u.release();
}

/// 40. Round D2 / counter-read sharpening 1: delaying Withdraw `Applied` until after a timely
///     resubmit (held during a take) must not arm `reseating_since` and drop valid commands at 15 s.
#[test]
fn d2_cr_timely_resubmit_during_take_not_expired_by_delayed_withdraw_applied() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("p")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let op1 = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), t));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);
    start_take(&device);

    // Withdraw during the take, but do NOT drain events yet: the engine applies Withdraw and returns
    // the box while Event::Seat(Withdraw, Applied) waits in the event ring.
    let _op_w = u.withdraw().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let box_w = loop {
        if let Some(Back::Withdrawn { unit, .. }) = u.take_back() {
            break unit;
        }
        assert!(Instant::now() < deadline, "withdrawn box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    drop(box_w);

    // Timely resubmit with changed latency during the take: the engine holds it (`Waiting`).
    let op_in = ok_op(u.submit(LatentUnit::new(2, 64, stops), t));
    assert!(device.wait_blocks(10));

    // Now drain: Withdraw Applied arrives AFTER submit, followed by Install Waiting.
    let mut ev = Vec::new();
    let drain = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(host.core.state.lock().unwrap().poll(op_in), OpState::Held);
    assert_eq!(drain.chains.chains[0].units[0].state, PlateState::Waiting);

    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10));

    host.core.state.lock().unwrap().model.advance_clock(GATE_DEADLINE + Duration::from_secs(1));
    ev.clear();
    let after = host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 0, "timely held resubmit must not expire");
    assert_eq!(after.chains.chains[0].units[0].state, PlateState::Waiting);
    assert!(host.core.state.lock().unwrap().gate.has(10));

    stop_take(&device);
    assert_eq!(host.wait_op(op_in, Duration::from_secs(2)).unwrap(), OpState::Applied);
    ev.clear();
    host.drain_feed_taps(&mut ev, None, None, None);
    assert!(device.wait_blocks(5));
    ev.clear();
    host.drain_feed_taps(&mut ev, None, None, None);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)], "held resubmit receives its gated bypass");
    u.release();
}

/// 41. Round D2 / Astra 6(b): a submit arriving past `GATE_DEADLINE` with no intervening flush
///     expires the overdue gate before clearing the reseat timer.
#[test]
fn d2_astra6b_late_submit_expires_overdue_gate_before_clearing_timer() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("p")), 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let op1 = ok_op(u.submit(LatentUnit::new(1, 0, stops.clone()), t1));
    assert_eq!(host.wait_op(op1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);
    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: ev_box, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
    drop(ev_box);

    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10));

    // Start the take before advancing the clock so `start_take`'s drain does not expire the timer.
    start_take(&device);
    assert_eq!(host.core.gate_dropped.load(SeqCst), 0);
    assert!(host.core.state.lock().unwrap().gate.has(10));

    // Advance past GATE_DEADLINE with NO drain, then submit a latent unit (held by the take).
    host.core.state.lock().unwrap().model.advance_clock(GATE_DEADLINE + Duration::from_secs(1));
    let t2 = u.terms().unwrap();
    let op2 = ok_op(u.submit(LatentUnit::new(2, 64, stops), t2));
    assert_eq!(host.core.gate_dropped.load(SeqCst), 1, "late submit must expire overdue gate at once");
    assert!(!host.core.state.lock().unwrap().gate.has(10));

    stop_take(&device);
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)]);
    u.release();
}

/// 42. Round D2 / Astra 4: two pending installs of the same row across a rebuild (one refused and
///     reaped while the event ring is full, followed by an applied retry) settle independently.
#[test]
fn d2_astra4_rebuild_settles_refused_and_applied_installs_of_same_row_separately() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));
    fill_event_ring(&device);

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("retry")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let past = max_chain_latency(48_000) + 1;
    let op1 = ok_op(u.submit(LatentUnit::new(1, past, stops.clone()), t));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(Back::Refused { why, .. }) = u.take_back() {
            assert_eq!(why, SeatRefusal::Latency);
            break;
        }
        assert!(Instant::now() < deadline, "refused box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }

    let op2 = ok_op(u.submit(LatentUnit::new(2, 0, stops), t));
    assert!(device.wait_blocks(5));

    device.rebuild_at(44_100, 256);
    assert_eq!(u.poll(op1), OpState::Refused(SeatRefusal::Latency), "first install must settle Refused");
    assert_eq!(u.poll(op2), OpState::Applied, "second install must settle Applied");
    assert!(matches!(u.take_back(), Some(Back::Evicted { .. })));
    u.release();
}

/// 43. Round D2 / counter-read sharpening 2 (receipts): reaping a refused install while the event
///     ring is full and releasing its owner before a rebuild still settles that op `Refused`, not `Rebuilt`.
#[test]
fn d2_cr_refused_install_receipt_survives_release_across_rebuild() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));
    fill_event_ring(&device);

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("rel")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let past = max_chain_latency(48_000) + 1;
    let op1 = ok_op(u.submit(LatentUnit::new(1, past, stops), t));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(Back::Refused { why, .. }) = u.take_back() {
            assert_eq!(why, SeatRefusal::Latency);
            break;
        }
        assert!(Instant::now() < deadline, "refused box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }
    u.release();

    device.rebuild_at(44_100, 256);
    assert_eq!(
        host.wait_op(op1, Duration::from_millis(50)).unwrap(),
        OpState::Refused(SeatRefusal::Latency),
        "refused install receipt must outlive owner release until op settles"
    );
}

/// 44. Round D2 / Astra 5: two queued moves (`C->0`, then `B->0`) across a rebuild restore the
///     engine's order `[B, C, A]` in both the model and the rebuilt engine.
#[test]
fn d2_astra5_two_queued_moves_restore_engine_order_across_rebuild() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let uc = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 2, false).unwrap();
    let t1 = ua.terms().unwrap();
    for u in [&ua, &ub, &uc] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }

    device.stop();
    let op_m1 = host.move_unit(30, 0).unwrap();
    let op_m2 = host.move_unit(20, 0).unwrap();
    device.rebuild_at(44_100, 256);
    assert_eq!(uc.poll(op_m1), OpState::Applied);
    assert_eq!(ub.poll(op_m2), OpState::Applied);

    let model_ids: Vec<UnitId> = host.core.chain_snapshot().chains[0].units.iter().map(|u| u.id).collect();
    assert_eq!(model_ids, vec![20, 30, 10], "model must match engine order [B, C, A]");

    let t2 = ua.terms().unwrap();
    for u in [&ua, &ub, &uc] {
        let Back::Evicted { unit, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
        let op = ok_op(u.submit(unit, t2));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false), (30, false), (10, false)]);
    ua.release();
    ub.release();
    uc.release();
}

/// 45. Round D2 / counter-read sharpening 2 (moves): queueing an over-budget install X at index 0
///     ahead of `C->1` leaves `[A, C, B]` when idle service refuses X across a rebuild.
#[test]
fn d2_cr_move_behind_refused_overbudget_install_restores_engine_order_across_rebuild() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let ub = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let uc = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 2, false).unwrap();
    let t1 = ua.terms().unwrap();
    for u in [&ua, &ub, &uc] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), 0, stops.clone()), t1));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }

    device.stop();
    let ux = host.unit(0, 40, Spec::Plugin(plugin_ref("x")), 0, false).unwrap();
    let past = max_chain_latency(48_000) + 1;
    let op_x = ok_op(ux.submit(LatentUnit::new(40, past, stops.clone()), t1));
    let op_mv = host.move_unit(30, 1).unwrap();

    device.rebuild_at(44_100, 256);
    assert_eq!(ux.poll(op_x), OpState::Refused(SeatRefusal::Latency));
    assert_eq!(uc.poll(op_mv), OpState::Applied);
    assert!(matches!(ux.take_back(), Some(Back::Refused { why: SeatRefusal::Latency, .. })));
    ux.failed("latency".into());
    ux.release();

    let survivors: Vec<UnitId> = host
        .core
        .chain_snapshot()
        .chains[0]
        .units
        .iter()
        .filter(|u| u.id != 40)
        .map(|u| u.id)
        .collect();
    assert_eq!(survivors, vec![10, 30, 20], "survivor order must match engine [A, C, B]");

    let t2 = ua.terms().unwrap();
    for u in [&ua, &ub, &uc] {
        let Back::Evicted { unit, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
        let op = ok_op(u.submit(unit, t2));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false), (30, false), (20, false)]);
    ua.release();
    ub.release();
    uc.release();
}

/// 46. Round D2 / Astra 8: an unowned settled remove outcome survives >256 later completions within
///     `UNOWNED_KEEP` (60 s), and may evict once older than `UNOWNED_KEEP`.
#[test]
fn d2_astra8_unowned_remove_outcome_survives_count_past_256_until_age_deadline() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    device.stop();
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let u10 = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let u20 = host.unit(0, 20, Spec::Plugin(plugin_ref("b")), 1, false).unwrap();
    let u30 = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 2, false).unwrap();
    let t = u10.terms().unwrap();
    for u in [&u10, &u20, &u30] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), 0, stops.clone()), t));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }

    let t0 = Instant::now();
    host.core.state.lock().unwrap().model.set_clock(t0);

    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    super::unit_host::service_if_idle(&host.core);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(host.core.state.lock().unwrap().poll(op_rem), OpState::Applied);
    assert!(matches!(u10.take_back(), Some(Back::Removed(_))));
    u10.release();

    for i in 0..300usize {
        let target = if i % 2 == 0 { 1 } else { 0 };
        let op_mv = host.move_unit(20, target).unwrap();
        super::unit_host::service_if_idle(&host.core);
        ev.clear();
        host.drain_feed(&mut ev);
        assert_eq!(u20.poll(op_mv), OpState::Applied);
    }

    assert_eq!(
        host.wait_op(op_rem, Duration::from_millis(50)).unwrap(),
        OpState::Applied,
        "unowned remove outcome must survive >256 completions within 60 s"
    );

    // Once past UNOWNED_KEEP (60 s), one more completion allows the old entry to evict.
    host.core.state.lock().unwrap().model.advance_clock(UNOWNED_KEEP + Duration::from_secs(1));
    let op_mv = host.move_unit(20, 0).unwrap();
    super::unit_host::service_if_idle(&host.core);
    ev.clear();
    host.drain_feed(&mut ev);
    assert_eq!(u20.poll(op_mv), OpState::Applied);
    assert_eq!(host.core.state.lock().unwrap().poll(op_rem), OpState::Pending, "expired unowned outcome evicted");

    u20.release();
    u30.release();
}

/// 47. Round D2 / should-fix: moves mint with token 0 so `Row::settled` does not grow with moves.
#[test]
fn d2_should_fix_moves_do_not_grow_row_settled() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    device.stop();
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let ua = host.unit(0, 10, Spec::Plugin(plugin_ref("a")), 0, false).unwrap();
    let uc = host.unit(0, 30, Spec::Plugin(plugin_ref("c")), 1, false).unwrap();
    let t = ua.terms().unwrap();
    for u in [&ua, &uc] {
        let op = ok_op(u.submit(LatentUnit::new(u.id(), 0, stops.clone()), t));
        assert_eq!(host.wait_op(op, Duration::from_secs(2)).unwrap(), OpState::Applied);
    }

    let mut ev = Vec::new();
    for i in 0..100usize {
        let target = if i % 2 == 0 { 0 } else { 1 };
        let op_mv = host.move_unit(30, target).unwrap();
        super::unit_host::service_if_idle(&host.core);
        ev.clear();
        host.drain_feed(&mut ev);
        assert_eq!(uc.poll(op_mv), OpState::Applied);
    }

    let settled_len = host
        .core
        .state
        .lock()
        .unwrap()
        .table
        .active_row_for_id(30)
        .expect("row 30")
        .settled
        .len();
    assert!(settled_len <= 1, "moves must not accumulate in Row::settled (got {settled_len})");
    ua.release();
    uc.release();
}

/// 48. Round D2 / should-fix: a legacy install held during a take sets `model.slot_held(0)` true
///     while `chain_snapshot().chains[0].held` stays false (even when `snapshot()` recomputes).
#[test]
fn d2_should_fix_chain_state_held_ignores_legacy_held_op() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    start_take(&device);
    // Bump `rev` after `start_take`'s drain without taking a snapshot first, so the next
    // `chain_snapshot()` recomputes while the legacy op is held.
    let tmp = host.unit(0, 99, Spec::Plugin(plugin_ref("tmp")), 0, false).unwrap();
    assert_eq!(host.remove_unit(99).unwrap(), Removal::Plate);
    tmp.release();

    let slot = host.slot(0);
    let rate = slot.rate().unwrap();
    slot.install(LatentUnit::new(1, 64, stops), rate).unwrap_or_else(|(_, e)| panic!("legacy install: {e}"));
    assert!(device.wait_blocks(5));
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);

    assert!(host.core.state.lock().unwrap().model.slot_held(0), "live slot_held must see held legacy op");
    assert!(!host.core.chain_snapshot().chains[0].held, "ChainState.held must ignore legacy held ops");

    stop_take(&device);
    assert!(slot.remove(Duration::from_secs(2)).unwrap().is_some());
}

/// 49. Round D2 / D1 review 1: `failed()` and `release()` while a Remove is still pending must not
///     free the id before the Remove settles.
#[test]
fn d2_d1r1_failed_and_release_during_pending_remove_keeps_id_reserved() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));
    fill_event_ring(&device);

    let u = host.unit(0, 10, Spec::Plugin(plugin_ref("lat")), 0, false).unwrap();
    let t = u.terms().unwrap();
    let past = max_chain_latency(48_000) + 1;
    let _op_in = ok_op(u.submit(LatentUnit::new(1, past, stops), t));

    // Reap Back::Refused before Event::Seat(Install, Refused) drains.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(Back::Refused { why, .. }) = u.take_back() {
            assert_eq!(why, SeatRefusal::Latency);
            break;
        }
        assert!(Instant::now() < deadline, "refused box never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }

    // remove_unit still sees the pending install, sends Remove, marks plate Leaving.
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else { panic!("expected Sent") };
    u.failed("latency".into());
    u.release();

    // Id 10 must stay reserved while op_rem is pending.
    assert!(
        host.unit(0, 10, Spec::Plugin(plugin_ref("dup")), 0, false).is_err(),
        "id must stay reserved until pending Remove settles"
    );

    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let fresh = host.unit(0, 10, Spec::Plugin(plugin_ref("fresh")), 0, false).expect("id free after Remove settles");
    fresh.release();
}

/// 50. Round D2 / D1 review 2: reserving a same-id reload candidate without submitting it must not
///     turn a different-id source replacement into `Back::Superseded`.
#[test]
fn d2_d1r2_unsubmitted_reload_does_not_turn_different_id_source_replace_into_superseded() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("src_a")), 0, false).unwrap();
    a.activated(Activated { name: "src_a".into(), source: true, latency: 0 });
    let t = a.terms().unwrap();
    let op_a = ok_op(a.submit(Box::new(LatentUnit { tag: 1, latency: 0, source: true, stops: stops.clone() }), t));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Reserve same-id reload B without submitting it.
    let b = host.reload_unit(10, plugin_ref("src_b")).unwrap();

    // Fill event ring so C's Applied event does not drain before A reaps its box.
    fill_event_ring(&device);

    let c = host.unit(0, 20, Spec::Plugin(plugin_ref("src_c")), 0, false).unwrap();
    c.activated(Activated { name: "src_c".into(), source: true, latency: 0 });
    let op_c = ok_op(c.submit(Box::new(LatentUnit { tag: 2, latency: 0, source: true, stops }), t));

    let deadline = Instant::now() + Duration::from_secs(2);
    let back_a = loop {
        if let Some(back) = a.take_back() {
            break back;
        }
        assert!(Instant::now() < deadline, "replaced source A never returned");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        matches!(back_a, Back::Removed(_)),
        "different-id replacement with only an unsubmitted reload candidate must deliver Removed, not Superseded"
    );

    assert_eq!(host.wait_op(op_c, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(a.take_back().is_none());
    a.release();
    b.release();
    c.release();
}

/// 51. Round D2 / D1 review 3: legacy `SlotHost::remove` recovers a late-refused box immediately
///     without holding it under `Leave::Pending`.
#[test]
fn d2_d1r3_legacy_remove_recovers_refused_box_without_holding() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));
    let slot = host.slot(0);
    let past = max_chain_latency(48_000) + 1;

    // Stall audio callbacks by holding `core.rt` while `running` is true.
    let mut rt = host.core.rt.lock().unwrap();
    slot.install(LatentUnit::new(77, past, stops), 48_000).unwrap_or_else(|(_, e)| panic!("timed-out install stands: {e}"));
    // Restart's remove queues Remove (sets Leave::Pending) and times out while callbacks are stalled.
    assert!(slot.remove(Duration::from_millis(50)).is_err(), "remove times out while callbacks are stalled");

    // Let the engine process one block (refuses Install and pushes box to return ring), then keep
    // callbacks stalled (rt still held) before teardown's remove runs.
    let (x, mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256], vec![0.0f32; 256]);
    let ctx = lf_engine::ProcessContext { frame: 0, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
    rt.engine.as_mut().unwrap().process(&ctx, &x, &mut l, &mut r);

    // Teardown's remove must recover the refused box immediately without waiting on another Remove.
    let back = slot
        .remove(Duration::from_millis(50))
        .expect("teardown remove must recover refused box immediately")
        .expect("a unit");
    assert_eq!(back.into_any().downcast::<LatentUnit>().unwrap().tag, 77);
    drop(rt);
}

/// 52. Round D2 / D1 review 4: when a different-id source replacement's `Left::Replaced` box is
///     reaped before `Event::Seat(Applied)` drains, `finish_replaced` must not queue `Dismissed`
///     behind the already-delivered `Removed`.
#[test]
fn d2_d1r4_different_id_replacement_reaped_first_delivers_no_second_terminal() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("src_a")), 0, false).unwrap();
    a.activated(Activated { name: "src_a".into(), source: true, latency: 0 });
    let t = a.terms().unwrap();
    let op_a = ok_op(a.submit(Box::new(LatentUnit { tag: 1, latency: 0, source: true, stops: stops.clone() }), t));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    fill_event_ring(&device);
    let c = host.unit(0, 20, Spec::Plugin(plugin_ref("src_c")), 0, false).unwrap();
    c.activated(Activated { name: "src_c".into(), source: true, latency: 0 });
    let op_c = ok_op(c.submit(Box::new(LatentUnit { tag: 2, latency: 0, source: true, stops }), t));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(back) = a.take_back() {
            assert!(matches!(back, Back::Removed(_)), "first terminal must be Removed");
            break;
        }
        assert!(Instant::now() < deadline, "replaced source A never returned");
        std::thread::sleep(Duration::from_millis(2));
    }

    // Now let C's Applied event drain (which calls finish_replaced(10)).
    assert_eq!(host.wait_op(op_c, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(a.take_back().is_none(), "one terminal per row, ever: no Dismissed behind Removed");
    a.release();
    c.release();
}

/// 53. Round D2 / D1 review 5: a reload candidate cancelled by a rebuild calling `activated()`
///     later must not overwrite the next candidate's activation metadata.
#[test]
fn d2_d1r5_cancelled_reload_activated_does_not_overwrite_next_candidate() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("v1")), 0, false).unwrap();
    a.activated(Activated { name: "v1".into(), source: false, latency: 0 });
    let t1 = a.terms().unwrap();
    let op_a = ok_op(a.submit(LatentUnit::new(1, 0, stops.clone()), t1));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Reserve reload candidate B1, then rebuild (cancels B1).
    let b1 = host.reload_unit(10, plugin_ref("v2")).unwrap();
    device.rebuild_at(44_100, 256);

    // Reseat A on the new engine.
    let Back::Evicted { unit: box_a, .. } = a.take_back().expect("evicted A") else { panic!("expected Evicted") };
    let t2 = a.terms().unwrap();
    let op_a2 = ok_op(a.submit(box_a, t2));
    assert_eq!(host.wait_op(op_a2, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Start a fresh reload candidate B2 and publish its activation metadata.
    let b2 = host.reload_unit(10, plugin_ref("v3")).unwrap();
    b2.activated(Activated { name: "v3".into(), source: false, latency: 0 });

    // Stale cancelled B1 now calls activated(): must touch nothing on the plate.
    b1.activated(Activated { name: "stale-v2".into(), source: false, latency: 128 });
    b1.release();

    let op_b2 = ok_op(b2.submit(LatentUnit::new(3, 0, stops), t2));
    assert_eq!(host.wait_op(op_b2, Duration::from_secs(2)).unwrap(), OpState::Applied);

    let unit_state = &host.core.chain_snapshot().chains[0].units[0];
    assert_eq!(unit_state.name, "v3", "stale cancelled reload must not overwrite next candidate's name");
    assert_eq!(unit_state.latency, 0, "stale cancelled reload must not overwrite next candidate's latency");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(back) = a.take_back() {
            assert!(matches!(back, Back::Superseded(_)));
            break;
        }
        assert!(Instant::now() < deadline, "superseded box A never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }
    a.release();
    b2.release();
}

/// 54. Round D4 finding 1: when a different-id source replacement C is queued ahead of A's
///     submitted same-id reload B and A's `Left::Replaced` box is reaped before C's `Applied`
///     event drains, A must receive `Back::Removed` (retire) and candidate B must end; conversely,
///     if C ahead of B was refused and B replaced A, A must still receive `Back::Superseded`.
#[test]
fn d4_f1_different_id_replace_ahead_of_submitted_reload_delivers_removed() {
    // Part A: C (valid) queued ahead of same-id reload B -> C replaces A, B is refused Unknown.
    {
        let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));

        let a = host.unit(0, 10, Spec::Plugin(plugin_ref("src_a")), 0, false).unwrap();
        a.activated(Activated { name: "src_a".into(), source: true, latency: 0 });
        let t = a.terms().unwrap();
        let op_a = ok_op(a.submit(Box::new(LatentUnit { tag: 1, latency: 0, source: true, stops: stops.clone() }), t));
        assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let b = host.reload_unit(10, plugin_ref("src_b")).unwrap();
        b.activated(Activated { name: "src_b".into(), source: true, latency: 0 });

        // Stop callbacks so C and B are both queued in push order before the engine services them,
        // and leave the event ring undrained when the return ring is reaped.
        device.stop();
        let c = host.unit(0, 20, Spec::Plugin(plugin_ref("src_c")), 0, false).unwrap();
        c.activated(Activated { name: "src_c".into(), source: true, latency: 0 });
        let op_c = ok_op(c.submit(Box::new(LatentUnit { tag: 2, latency: 0, source: true, stops: stops.clone() }), t));
        let op_b = ok_op(b.submit(Box::new(LatentUnit { tag: 3, latency: 0, source: true, stops }), t));

        // Service idle: C replaces A (pushes A Left::Replaced), then B is refused Unknown (behind A).
        super::unit_host::service_if_idle(&host.core);

        // Reap return ring before C's Applied / B's Refused events drain: A is filed before B's refusal.
        let back_a = a.take_back().expect("replaced source A must return");
        assert!(
            matches!(back_a, Back::Removed(_)),
            "different-id C queued ahead of submitted reload B must deliver Removed (retire), not Superseded"
        );
        assert!(
            !host.core.chain_snapshot().chains[0].units.iter().any(|u| u.id == 10 && u.reloading),
            "doomed reload candidate B must be ended once A is replaced by C"
        );
        assert!(
            matches!(b.take_back(), Some(Back::Refused { why: SeatRefusal::Unknown, .. })),
            "reload candidate B still surfaces its own install refusal"
        );

        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert_eq!(a.poll(op_c), OpState::Applied);
        assert_eq!(b.poll(op_b), OpState::Refused(SeatRefusal::Unknown));
        assert!(a.take_back().is_none(), "no second terminal for A after C's Applied drains");
        let units = &host.core.chain_snapshot().chains[0].units;
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].id, 20);
        a.release();
        b.release();
        c.release();
    }

    // Part B: C (over-budget, refused) queued ahead of reload B -> B replaces A -> Superseded.
    {
        let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 0.0);
        let host = device.host().clone();
        let stops = Arc::new(AtomicUsize::new(0));
        let past = max_chain_latency(48_000) + 1;

        let a = host.unit(0, 10, Spec::Plugin(plugin_ref("src_a")), 0, false).unwrap();
        a.activated(Activated { name: "src_a".into(), source: true, latency: 0 });
        let t = a.terms().unwrap();
        let op_a = ok_op(a.submit(Box::new(LatentUnit { tag: 1, latency: 0, source: true, stops: stops.clone() }), t));
        assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

        let b = host.reload_unit(10, plugin_ref("src_b")).unwrap();
        b.activated(Activated { name: "src_b".into(), source: true, latency: 0 });

        device.stop();
        let c = host.unit(0, 20, Spec::Plugin(plugin_ref("src_c")), 0, false).unwrap();
        c.activated(Activated { name: "src_c".into(), source: true, latency: past });
        let op_c = ok_op(c.submit(Box::new(LatentUnit { tag: 2, latency: past, source: true, stops: stops.clone() }), t));
        let op_b = ok_op(b.submit(Box::new(LatentUnit { tag: 3, latency: 0, source: true, stops }), t));

        super::unit_host::service_if_idle(&host.core);

        let back_a = a.take_back().expect("replaced source A must return");
        assert!(
            matches!(back_a, Back::Superseded(_)),
            "when C ahead of B was refused and reload B replaced A, A must receive Superseded"
        );
        assert!(matches!(c.take_back(), Some(Back::Refused { why: SeatRefusal::Latency, .. })));

        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        assert_eq!(c.poll(op_c), OpState::Refused(SeatRefusal::Latency));
        assert_eq!(b.poll(op_b), OpState::Applied);
        a.release();
        c.failed("latency".into());
        c.release();
        b.release();
    }
}

/// 55. Round D4 finding 2: after a rebuild, an over-budget reseat is refused, the owner takes the
///     box and parks before the install refusal event drains, `remove_unit` queues a Remove, and a
///     plugin restart retry receives `SubmitError::Removing`. When the engine refuses the Remove
///     with `SeatRefusal::Unknown`, the removal must settle as done (`Applied`), remove the plate,
///     and deliver `Back::Dismissed` so the owner retires and releases instead of stranding in
///     `Seat::Removing`.
#[test]
fn d4_f2_refused_remove_after_rebuild_settles_and_delivers_terminal() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    let a = host.unit(0, 10, Spec::Plugin(plugin_ref("fx")), 0, false).unwrap();
    a.activated(Activated { name: "fx".into(), source: false, latency: 0 });
    let t1 = a.terms().unwrap();
    let op_a = ok_op(a.submit(LatentUnit::new(1, 0, stops.clone()), t1));
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Rebuild to a new engine; take the evicted box.
    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: _evicted, .. } = a.take_back().expect("evicted box") else {
        panic!("expected Back::Evicted");
    };
    let t2 = a.terms().unwrap();

    // Stop callbacks so Event::Seat(Install, Refused(Latency)) does not drain before the owner
    // takes the refused box, parks, and remove_unit runs.
    device.stop();
    let past = max_chain_latency(44_100) + 1;
    let op_reseat = ok_op(a.submit(LatentUnit::new(2, past, stops), t2));
    super::unit_host::service_if_idle(&host.core);

    // Owner takes the refused box and parks before the install refusal event drains.
    let Back::Refused { unit: parked_box, why: SeatRefusal::Latency } =
        a.take_back().expect("refused reseat box")
    else {
        panic!("expected Back::Refused(Latency)");
    };
    a.parked("latency".into());

    // remove_unit sees the install still pending in the model and queues a Remove.
    let Removal::Sent(op_rem) = host.remove_unit(10).unwrap() else {
        panic!("expected Removal::Sent");
    };

    // Plugin restart retries submit and receives SubmitError::Removing (owner enters Seat::Removing).
    let (held_by_owner, err) = a.submit(parked_box, t2).expect_err("retry during pending Remove must return Removing");
    assert_eq!(err, SubmitError::Removing);
    drop(held_by_owner);

    // Engine services Remove -> no seat exists -> Refused(Unknown); now drain events.
    super::unit_host::service_if_idle(&host.core);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);

    assert_eq!(a.poll(op_reseat), OpState::Refused(SeatRefusal::Latency));
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert!(
        matches!(a.take_back(), Some(Back::Dismissed)),
        "refused Remove(Unknown) must deliver Back::Dismissed to wake Seat::Removing"
    );
    assert!(
        host.core.chain_snapshot().chains[0].units.is_empty(),
        "plate must be removed once Remove(Unknown) settles"
    );
    a.release();

    // Id 10 must be reusable once the owner releases.
    let fresh = host.unit(0, 10, Spec::Plugin(plugin_ref("fresh")), 0, false).expect("id 10 free");
    fresh.release();
}

/// P4 fix r1 finding 3: `active_row_for_id` must ignore rows whose id was freed by `failed()` so a
/// concurrent add in another slot reserving the same id resolves its own row, never the freed one.
#[test]
fn p4f1_3a_active_row_for_id_skips_freed_row() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
    let host = device.host();

    let u1 = host
        .unit(0, 300, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false)
        .expect("reserve u1 in slot 0");
    let u1_tok = {
        let st = host.core.state.lock().unwrap();
        st.table.active_row_for_id(300).expect("u1 active").token
    };
    // `failed()` frees the id before `release()`.
    u1.failed("first add failed".into());

    // A concurrent add in slot 1 reserves the same freed id before `u1.release()` runs.
    let u2 = host
        .unit(1, 300, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false)
        .expect("reserve u2 in slot 1 after u1.failed");

    {
        let st = host.core.state.lock().unwrap();
        let active = st.table.active_row_for_id(300).expect("u2 active row");
        assert_ne!(active.token, u1_tok, "active_row_for_id must not return freed u1 row");
        assert_eq!(active.slot, 1, "active_row_for_id must return slot 1's new row");
    }

    u2.release();
    u1.release();
}

/// P4 fix r1 finding 5: after a rebuild, an evicted source retrying `submit()` after a newer source
/// has queued (or seated) in the new engine must never replace that newer source.
#[test]
fn p4f1_5b_evicted_source_retrying_after_rebuild_never_replaces_newer_source() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
    let host = device.host().clone();
    let stops = Arc::new(AtomicUsize::new(0));

    // Seat Source A (id 600) in slot 0.
    let u_a = host
        .unit(0, 600, Spec::Builtin { kind: UnitKind::Synth, params: vec![] }, 0, false)
        .expect("reserve source A");
    u_a.activated(Activated { name: "SynthA".into(), source: true, latency: 0 });
    let t1 = u_a.terms().unwrap();
    let mut box_a = LatentUnit::new(1, 0, stops.clone());
    box_a.source = true;
    let op_a1 = ok_op(u_a.submit(box_a, t1));
    assert_eq!(host.wait_op(op_a1, Duration::from_secs(2)).unwrap(), OpState::Applied);

    // Rebuild: Source A is evicted into its mailbox.
    device.rebuild_at(96_000, 512);
    let Back::Evicted { unit: evicted_a, .. } = u_a.take_back().expect("A evicted on rebuild") else {
        panic!("expected Back::Evicted for A");
    };
    let t2 = u_a.terms().unwrap();

    // Pause audio callbacks so Source B is queued (still pending) when Source A retries!
    device.stop();
    let u_b = host
        .unit(0, 601, Spec::Builtin { kind: UnitKind::Synth, params: vec![] }, 0, false)
        .expect("reserve newer source B");
    u_b.activated(Activated { name: "SynthB".into(), source: true, latency: 0 });
    let mut box_b = LatentUnit::new(2, 0, stops.clone());
    box_b.source = true;
    let op_b = ok_op(u_b.submit(box_b, t2));

    // Evicted Source A now retries `submit()` after newer Source B has already queued.
    let retry = u_a.submit(evicted_a, t2);
    match retry {
        Err((returned_a, SubmitError::Replaced)) => {
            drop(returned_a);
        }
        Ok(op) => panic!("evicted source A must not enqueue to replace newer source B (got Ok({op:?}))"),
        Err((_, other)) => panic!("expected SubmitError::Replaced for evicted source A, got {other:?}"),
    }

    // Service the port: Source B seats cleanly and remains the sole source in slot 0.
    super::unit_host::service_if_idle(&host.core);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(u_b.poll(op_b), OpState::Applied);

    let chain_ids: Vec<UnitId> = {
        let rt = host.core.rt.lock().unwrap();
        rt.engine.as_ref().unwrap().chain(0).map(|s| s.unit).collect()
    };
    assert_eq!(chain_ids, vec![601], "newer source B must stand alone in slot 0");
    let snap = host.core.chain_snapshot();
    assert_eq!(
        snap.chains[0].units.iter().map(|u| u.id).collect::<Vec<_>>(),
        vec![601],
        "evicted source A's plate must be gone, leaving only newer source B"
    );

    u_a.release();
    let Removal::Sent(rem_b) = host.remove_unit(601).unwrap() else { panic!("expected Sent") };
    super::unit_host::service_if_idle(&host.core);
    host.drain_feed(&mut ev);
    assert_eq!(host.wait_op(rem_b, Duration::from_secs(2)).unwrap(), OpState::Applied);
    drop(u_b.take_back());
    u_b.release();
}

/// 56. A kind toggle (`Toggle::Units`) survives a rebuild in send order (`docs/unit-contract.md` §2): a
///     per-unit command and the toggle the stopped engine never took, then a per-unit command sent while
///     the unit reseats, apply after the reseat in that order. A: bypass on; K: none of the kind on, so
///     all come on (`Toggled` on = true); B: the lamp flips it back to bypassed. Any other order ends
///     unbypassed or answers K with false.
#[test]
fn a_kind_toggle_survives_a_rebuild_between_unit_commands_in_send_order() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let factory = ClampFactory { applied: ParamRecorder::new() };
    let init = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.5)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    let t1 = u.terms().unwrap();
    let (unit1, clamped) = factory.build(UnitKind::Echo, &init, t1).unwrap();
    u.committed(clamped);
    assert_eq!(host.wait_op(ok_op(u.submit(unit1, t1)), Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)]);

    let kind = Command::Action(Action::Toggle(lf_engine::Toggle::Units(UnitKind::Echo)));
    device.stop();
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    host.send(TimedCommand { frame: None, command: kind }).unwrap();
    device.rebuild_at(44_100, 256);
    let Back::Evicted { unit: old_box, bypassed } = u.take_back().expect("evicted") else { panic!("evicted") };
    drop(old_box);
    assert!(!bypassed, "the stopped engine applied neither");
    host.send(TimedCommand { frame: None, command: Command::Action(Action::ToggleUnit(10)) }).unwrap();
    {
        let state = host.core.state.lock().unwrap();
        assert!(state.gate.holds_kind_toggle(), "the old engine's unapplied kind toggle is in the gate");
        assert!(state.gate.has(10));
    }
    device.resume();
    ev.clear();
    host.drain_feed(&mut ev);
    assert!(host.core.state.lock().unwrap().gate.holds_kind_toggle(), "a flush before the reseat keeps the kind toggle");

    let t2 = u.terms().unwrap();
    let params = host.core.chain_snapshot().chains[0].units[0].params.as_ref().unwrap().0.clone();
    let (unit2, _) = factory.build(UnitKind::Echo, &params, t2).unwrap();
    assert_eq!(host.wait_op(ok_op(u.submit(unit2, t2)), Duration::from_secs(2)).unwrap(), OpState::Applied);
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);
    assert!(host.core.state.lock().unwrap().gate.is_empty(), "all three sent");
    let answers: Vec<bool> = ev
        .iter()
        .filter_map(|e| match *e {
            lf_engine::Event::Toggled { toggle: lf_engine::Toggle::Units(UnitKind::Echo), on, .. } => Some(on),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [true], "the kind toggle applied once, after the bypass-on: all came on");
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)], "then the lamp: bypassed");
    assert!(host.core.chain_snapshot().chains[0].units[0].bypass);
    u.release();
}

/// 57. With the gate empty, a kind toggle sent while a unit of its kind reseats waits for it, and a
///     command for a seated unit sent after it waits behind it, then follows it. ECHO 10 reseats, ECHO 30
///     is back: the kind toggle (both on, so all go off) then 30's lamp (30 back on) ends 10 bypassed,
///     30 not. Reversed, the lamp turns 30 off first and the toggle (10 still on) turns both off: 30 ends
///     bypassed, so the last assertion fails.
#[test]
fn a_kind_toggle_waits_for_its_kinds_reseat_and_holds_later_unit_commands() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let factory = ClampFactory { applied: ParamRecorder::new() };
    let init = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.5)];
    let seat = |slot: usize, id: UnitId| {
        let u = host.unit(slot, id, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
        let t = u.terms().unwrap();
        let (unit, clamped) = factory.build(UnitKind::Echo, &init, t).unwrap();
        u.committed(clamped);
        assert_eq!(host.wait_op(ok_op(u.submit(unit, t)), Duration::from_secs(2)).unwrap(), OpState::Applied);
        u
    };
    let u = seat(0, 10);
    let w = seat(1, 30);

    device.rebuild_at(44_100, 256);
    for h in [&u, &w] {
        let Back::Evicted { unit: old, bypassed } = h.take_back().expect("evicted") else { panic!("evicted") };
        assert!(!bypassed);
        drop(old);
    }
    let reseat = |h: &super::unit_host::UnitHost, slot: usize| {
        let t = h.terms().unwrap();
        let params = host.core.chain_snapshot().chains[slot].units[0].params.as_ref().unwrap().0.clone();
        let (unit, _) = factory.build(UnitKind::Echo, &params, t).unwrap();
        assert_eq!(host.wait_op(ok_op(h.submit(unit, t)), Duration::from_secs(2)).unwrap(), OpState::Applied);
    };
    reseat(&w, 1);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert!(host.core.state.lock().unwrap().gate.is_empty());

    host.send(TimedCommand { frame: None, command: Command::Action(Action::Toggle(lf_engine::Toggle::Units(UnitKind::Echo))) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::Action(Action::ToggleUnit(30)) }).unwrap();
    {
        let state = host.core.state.lock().unwrap();
        assert!(state.gate.holds_kind_toggle(), "its kind reseats: the toggle waits");
        assert!(state.gate.has(30), "a seated unit's command waits behind the held kind toggle");
    }
    host.drain_feed(&mut ev);
    assert!(host.core.state.lock().unwrap().gate.holds_kind_toggle(), "still reseating: still held");
    assert_eq!(engine_chain_ids(&device, 1), vec![(30, false)], "nothing reached the seated ECHO yet");

    ev.clear();
    reseat(&u, 0);
    host.drain_feed(&mut ev);
    assert!(device.wait_blocks(10));
    host.drain_feed(&mut ev);
    assert!(host.core.state.lock().unwrap().gate.is_empty());
    let answers: Vec<bool> = ev
        .iter()
        .filter_map(|e| match *e {
            lf_engine::Event::Toggled { toggle: lf_engine::Toggle::Units(UnitKind::Echo), on, .. } => Some(on),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [false], "the kind toggle applied once: an ECHO was on, so all went off");
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, true)], "the toggle reached the reseated ECHO: on → bypassed");
    assert_eq!(engine_chain_ids(&device, 1), vec![(30, false)], "then 30's lamp, after the toggle: back on");
    u.release();
    w.release();
}

/// 58. A held entry that may go takes a freed ring slot before a new send: with the ring one slot short
///     of full and an eligible command in the gate (its unit seated, nothing before it), an ordinary
///     send flushes the gate first, so the held command takes the slot and the send is refused. Without
///     the flush in `push_all`, the send would take the slot and leave the held command waiting, which
///     sends that refill the ring after every block could do forever.
#[test]
fn a_new_send_lets_an_eligible_held_command_take_the_free_slot_first() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let factory = ClampFactory { applied: ParamRecorder::new() };
    let init = vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.5)];
    let u = host.unit(0, 10, Spec::Builtin { kind: UnitKind::Echo, params: init.clone() }, 0, false).unwrap();
    let t = u.terms().unwrap();
    let (unit, clamped) = factory.build(UnitKind::Echo, &init, t).unwrap();
    u.committed(clamped);
    assert_eq!(host.wait_op(ok_op(u.submit(unit, t)), Duration::from_secs(2)).unwrap(), OpState::Applied);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(engine_chain_ids(&device, 0), vec![(10, false)]);

    device.stop();
    {
        let mut ends = host.core.ends.lock().unwrap();
        let ends = ends.as_mut().unwrap();
        while ends.commands.slots() > 1 {
            ends.commands.push(TimedCommand { frame: None, command: Command::SetMasterVolume(1.0) }).unwrap();
        }
    }
    host.core.state.lock().unwrap().gate.push(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) });

    let full_before = host.core.counters.commands_full.load(Relaxed);
    let sent = host.send(TimedCommand { frame: None, command: Command::SetClickVolume(0.25) });
    assert!(sent.is_err(), "the held command took the one free slot, so the new send finds the ring full");
    assert_eq!(host.core.counters.commands_full.load(Relaxed), full_before + 1);
    assert!(host.core.state.lock().unwrap().gate.is_empty(), "the eligible held command went first");
    assert_eq!(host.core.ends.lock().unwrap().as_ref().unwrap().commands.slots(), 0);

    device.resume();
    let deadline = Instant::now() + PATIENCE;
    while engine_chain_ids(&device, 0) != vec![(10, true)] {
        assert!(Instant::now() < deadline, "the held bypass never reached the engine");
        assert!(device.wait_blocks(1));
        host.drain_feed(&mut ev);
    }
    u.release();
}
