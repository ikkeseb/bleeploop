//! Tests for E2 P2: the table, the model, the gate, the rebuild handoff, and legacy `SlotHost`.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{
    max_chain_latency, Action, Command, InputSendParam, LaneState, SeatRefusal, SlotEvent,
    TimedCommand, UnitCx, UnitId, UnitInfo, UnitKind, UnitParam, UnitProcessor,
};

use super::chain::{PlateState, PluginRef};
use super::fake_driver::{Fake, FakeDriver};
use super::test_rig::TestDevice;
use super::unit_host::{Activated, Back, BuiltinFactory, Op, OpState, Params, Removal, Spec, SubmitError, Terms, Unit};
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

/// A clamping built-in fake (E2 test-only `BuiltinFactory`): clamps `EchoLevel` to `0.0..=1.0`
/// (so 2.0 -> 1.0) and optionally logs every `set_param` value applied on the audio thread.
struct ClampEcho {
    level: f64,
    applied: Arc<Mutex<Vec<f64>>>,
}

impl UnitProcessor for ClampEcho {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: 0 }
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
            self.applied.lock().unwrap().push(c);
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
    applied: Arc<Mutex<Vec<f64>>>,
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
        Ok((Box::new(ClampEcho { level, applied: self.applied.clone() }), clamped))
    }
}

fn start_take(device: &TestDevice) {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
    // From candidate's submit on, the outgoing row may send nothing.
    assert_eq!(old_host.withdraw(), Err(SubmitError::Superseded));

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
        let drain = host.drain_feed_taps(&mut ev, None, None);
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
///    and the rebuilt clamping fake read the clamped applied value (1.0 for sent 2.0).
#[test]
fn full_event_ring_keeps_last_applied_param_across_rebuild() {
    let device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(1), 8, |_| 1.0);
    let host = device.host();
    let applied_log = Arc::new(Mutex::new(Vec::new()));
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
    assert_eq!(*applied_log.lock().unwrap(), vec![1.0], "engine applied clamped 1.0");

    // Rebuild WITHOUT draining the old event ring.
    device.rebuild_at(44_100, 256);

    // The model must read 1.0 from `unsent_unit_params`!
    let snap = host.core.chain_snapshot();
    let model_params = snap.chains[0].units[0].params.as_ref().expect("built-in params").0.clone();
    assert_eq!(model_params, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);

    // Rebuild the fake from the model's params at the new terms and verify it reads 1.0 too.
    let Back::Evicted { unit: old_unit, .. } = u.take_back().expect("evicted") else { panic!("expected Evicted") };
    drop(old_unit);
    let t2 = u.terms().unwrap();
    let (rebuilt_unit, clamped2) = factory.build(UnitKind::Echo, &model_params, t2).unwrap();
    assert_eq!(clamped2, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);
    let op2 = ok_op(u.submit(rebuilt_unit, t2));
    assert_eq!(host.wait_op(op2, Duration::from_secs(2)).unwrap(), OpState::Applied);
    let snap2 = host.core.chain_snapshot();
    assert_eq!(snap2.chains[0].units[0].params.as_ref().unwrap().0, vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]);
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
    let applied_log = Arc::new(Mutex::new(Vec::new()));
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
    let applied_log = Arc::new(Mutex::new(Vec::new()));
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

    assert_eq!(*applied_log.lock().unwrap(), vec![0.3, 0.8, 0.9], "gate order preserved across second rebuild");
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

    // Rebuild at the SAME rate (48_000) -> generation bumps, rate stays 48_000.
    device.rebuild_at(48_000, 256);
    let Back::Evicted { unit: back, .. } = u.take_back().expect("evicted") else { panic!("evicted") };
    host.send(TimedCommand { frame: None, command: Command::SetUnitBypass(10, true) }).unwrap();
    assert!(host.core.state.lock().unwrap().gate.has(10), "command entered the gate while reseating");

    // Advance past the gate deadline deterministically, then flush gate -> plate fails and gated command drops.
    host.core.state.lock().unwrap().gate_deadline = Duration::from_millis(5);
    std::thread::sleep(Duration::from_millis(20));
    let mut ev = Vec::new();
    let drain = host.drain_feed_taps(&mut ev, None, None);
    assert_eq!(drain.chains.chains[0].units[0].state, PlateState::Failed);
    assert!(host.core.gate_dropped.load(SeqCst) >= 1, "gated command dropped after deadline");

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
    stop_take(&device);
    assert_eq!(host.wait_op(op_rem, Duration::from_secs(2)).unwrap(), OpState::Applied);
    assert_eq!(host.wait_op(op_mv, Duration::from_secs(2)).unwrap(), OpState::Applied);

    assert_eq!(engine_chain_ids(&device, 0), vec![(20, false), (10, false)]);
    let snap = host.core.chain_snapshot();
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
fn shutdown_with_take_open_and_feed_stopped_ends_inside_3s() {
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
    assert!(elapsed < Duration::from_secs(3), "shutdown took {} ms (must be < 3000 ms)", elapsed.as_millis());
    let back = u.take_back().expect("shutdown evicted unit to its owner");
    assert!(matches!(back, Back::Removed(_)));
    assert_eq!(stops.load(SeqCst), 1);
    u.release();
}
