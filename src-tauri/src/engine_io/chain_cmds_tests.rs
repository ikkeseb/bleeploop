use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{
    Command, InputSendParam, LaneState, SlotEvent, TimedCommand, UnitCx, UnitId, UnitInfo,
    UnitKind, UnitParam, UnitProcessor, MAX_UNITS, SLOT_COUNT,
};

use super::{chain_event, chain_units_exist, Added, Affinity, Chains, Reloaded, EDITOR_CLOSED, PARAM_CHANGED, PARAMS_CHANGED};
use crate::audio_output::AudioBackend;
use crate::engine_io::chain::{PlateState, PluginRef, UnitSpec, WireParams};
use crate::engine_io::fake_driver::{Fake, FakeDriver};
use crate::engine_io::plugins::EngineSlot;
use crate::engine_io::test_rig::TestDevice;
use crate::engine_io::unit_host::{Activated, BuiltinFactory, OpState, Params, Removal, Spec, Terms, Unit, UnitHost};
use crate::engine_io::{DeviceRequest, EngineHost, HostConfig};
use crate::host::engine_slot::{EngineSlotEvent, EventSink, PluginFormat};
use crate::host::tone::{encode as encode_tone, Tone, ToneBinding, ToneIdentity, ToneStore};
use crate::host::unit_owner::{self, Answer, UnitOwner};
use crate::host::{engine_slot, PluginDescriptor, PluginHostState, PluginInfo};

fn noop_sink() -> EventSink {
    Arc::new(|_| {})
}

fn no_open(
    _format: PluginFormat,
    _plugin: &PluginRef,
    _unit_host: UnitHost,
    _sink: EventSink,
    _tone: Option<ToneBinding>,
) -> Result<(UnitOwner, Answer), String> {
    Err("opener should not be called in this test".to_string())
}

fn add_builtin(
    chains: &Chains,
    host: &EngineHost,
    doc: (&PluginHostState, u32),
    slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
    slot: usize,
    unit: UnitId,
    spec: UnitSpec,
    at: usize,
) -> Result<Added, String> {
    chains.add(host, doc, slots, None, slot, unit, spec, at, None, noop_sink(), &no_open)
}

/// Tracks box identity, construction count, drops, per-instance `process` calls, and `stop` counts
/// across rebuilds and shutdown.
#[derive(Default)]
pub(crate) struct BoxTracker {
    next_id: AtomicU64,
    pub(crate) builds: AtomicUsize,
    pub(crate) drops: AtomicUsize,
    calls: Mutex<BTreeMap<u64, Arc<AtomicUsize>>>,
    stops: Mutex<BTreeMap<u64, Arc<AtomicUsize>>>,
}

impl BoxTracker {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn alloc(&self) -> (u64, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let call_count = Arc::new(AtomicUsize::new(0));
        let stop_count = Arc::new(AtomicUsize::new(0));
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).insert(id, call_count.clone());
        self.stops.lock().unwrap_or_else(|e| e.into_inner()).insert(id, stop_count.clone());
        (id, call_count, stop_count)
    }

    pub(crate) fn calls(&self, instance_id: u64) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&instance_id)
            .map_or(0, |c| c.load(Ordering::SeqCst))
    }

    pub(crate) fn stops(&self, instance_id: u64) -> usize {
        self.stops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&instance_id)
            .map_or(0, |c| c.load(Ordering::SeqCst))
    }
}

/// A clamping built-in fake for E2 tests (`BuiltinFactory`): scales audio by `EchoLevel` clamped to
/// `0.0..=1.0`, reports configurable latency, and optionally refuses a configured kind.
pub(crate) struct ClampingEcho {
    level: f64,
    latency: Frame,
    calls: Option<Arc<AtomicUsize>>,
    stops: Option<Arc<AtomicUsize>>,
    tracker: Option<Arc<BoxTracker>>,
}

impl Drop for ClampingEcho {
    fn drop(&mut self) {
        if let Some(t) = &self.tracker {
            t.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl UnitProcessor for ClampingEcho {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: self.latency }
    }

    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        if let Some(c) = &self.calls {
            c.fetch_add(1, Ordering::Relaxed);
        }
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
            Some(c)
        } else {
            None
        }
    }

    fn stop(&mut self) {
        if let Some(s) = &self.stops {
            s.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

pub(crate) struct ClampingBuiltinFactory {
    latency: Frame,
    fail_kind: Option<(UnitKind, String)>,
    tracker: Option<Arc<BoxTracker>>,
}

impl ClampingBuiltinFactory {
    pub(crate) fn new() -> Self {
        Self { latency: 0, fail_kind: None, tracker: None }
    }

    pub(crate) fn with_latency(mut self, latency: Frame) -> Self {
        self.latency = latency;
        self
    }

    pub(crate) fn failing_kind(mut self, kind: UnitKind, msg: impl Into<String>) -> Self {
        self.fail_kind = Some((kind, msg.into()));
        self
    }

    pub(crate) fn with_tracker(mut self, tracker: Arc<BoxTracker>) -> Self {
        self.tracker = Some(tracker);
        self
    }
}

impl BuiltinFactory for ClampingBuiltinFactory {
    fn build(&self, kind: UnitKind, params: &Params, _terms: Terms) -> Result<(Unit, Params), String> {
        if let Some((fk, msg)) = &self.fail_kind {
            if kind == *fk {
                return Err(msg.clone());
            }
        }
        if kind != UnitKind::Echo {
            return Err(format!("clamping factory does not support {kind:?}"));
        }
        let mut level = 0.5;
        for &(p, v) in params {
            if p == UnitParam::Send(InputSendParam::EchoLevel) {
                level = v.clamp(0.0, 1.0);
            }
        }
        let clamped = vec![(UnitParam::Send(InputSendParam::EchoLevel), level)];
        let (calls, stops) = match &self.tracker {
            Some(t) => {
                let (_, c, s) = t.alloc();
                (Some(c), Some(s))
            }
            None => (None, None),
        };
        Ok((
            Box::new(ClampingEcho {
                level,
                latency: self.latency,
                calls,
                stops,
                tracker: self.tracker.clone(),
            }),
            clamped,
        ))
    }
}

fn echo_spec(level: f64, bypass: bool) -> UnitSpec {
    UnitSpec {
        kind: UnitKind::Echo,
        plugin: None,
        params: Some(WireParams(vec![(UnitParam::Send(InputSendParam::EchoLevel), level)])),
        bypass,
    }
}

fn empty_slots() -> Mutex<[EngineSlot; SLOT_COUNT]> {
    Mutex::new(std::array::from_fn(|_| EngineSlot::Empty))
}

fn start_device() -> TestDevice {
    TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0)
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

fn engine_chain_summary(device: &TestDevice, slot: usize) -> Vec<(UnitId, bool)> {
    let rt = device.host().core.rt.lock().unwrap();
    let engine = rt.engine.as_ref().expect("engine present");
    engine.chain(slot).map(|s| (s.unit, s.bypassed)).collect()
}

fn snapshot_chain_summary(device: &TestDevice, slot: usize) -> Vec<(UnitId, bool)> {
    let mut state = device.host().core.state.lock().unwrap();
    let rev = state.model.snapshot();
    rev.chains[slot].units.iter().map(|u| (u.id, u.bypass)).collect()
}

fn wait_until(bound: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + bound;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// (a) Two ECHOs at different settings, one bypassed, through `Chains::add` with our clamping factory
/// (levels 0.3 and 2.0 -> 1.0): both answer `Seated`. Engine's chain equals `model.snapshot()`'s units;
/// plates' `params` read 0.3 and 1.0.
#[test]
fn a_two_echos_seat_with_clamped_params_and_matching_engine_chain() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    let r1 = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 10, echo_spec(0.3, false), 0);
    assert_eq!(r1, Ok(Added::Seated));
    let r2 = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 11, echo_spec(2.0, true), 1);
    assert_eq!(r2, Ok(Added::Seated));

    let eng = engine_chain_summary(&device, 0);
    let snap = snapshot_chain_summary(&device, 0);
    assert_eq!(eng, vec![(10, false), (11, true)]);
    assert_eq!(eng, snap);

    let rev = {
        let mut st = host.core.state.lock().unwrap();
        st.model.snapshot()
    };
    let units = &rev.chains[0].units;
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].name, "ECHO");
    assert_eq!(units[0].params.as_ref().map(|p| &p.0), Some(&vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.3)]));
    assert_eq!(units[1].params.as_ref().map(|p| &p.0), Some(&vec![(UnitParam::Send(InputSendParam::EchoLevel), 1.0)]));

    chains.unload_all(host);
    chains.join_all();
}

/// (b) (g) A reserved id is refused at once, and a ninth unit in a full slot is refused at once
/// (< 100 ms, no plate added).
#[test]
fn b_reserved_id_and_full_slot_refused_at_once() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    for i in 0..MAX_UNITS {
        let id = (i + 1) as UnitId;
        assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, id, echo_spec(0.5, false), i), Ok(Added::Seated));
    }

    let t0 = Instant::now();
    let dup = add_builtin(&chains, host, (&doc_state, epoch), &slots, 1, 1, echo_spec(0.5, false), 0);
    let dup_elapsed = t0.elapsed();
    assert!(dup.is_err(), "reserved id must be refused: {dup:?}");
    assert!(dup_elapsed < Duration::from_millis(100), "reserved id took {dup_elapsed:?}");

    let t1 = Instant::now();
    let full = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 99, echo_spec(0.5, false), MAX_UNITS);
    let full_elapsed = t1.elapsed();
    assert!(full.is_err(), "ninth unit in full slot must be refused: {full:?}");
    assert!(full_elapsed < Duration::from_millis(100), "full slot refusal took {full_elapsed:?}");

    let rev = {
        let mut st = host.core.state.lock().unwrap();
        st.model.snapshot()
    };
    assert_eq!(rev.chains[0].units.len(), MAX_UNITS);
    assert!(rev.chains[1].units.is_empty());

    chains.unload_all(host);
    chains.join_all();
}

/// (c) A failed add (the factory errs for a kind): `Err` with the factory's words, the plate `failed`
/// with that error, the id free. `Chains::remove` takes the plate (`Ok`), and an add under the same id
/// then seats.
#[test]
fn c_failed_factory_build_marks_plate_failed_and_allows_remove_and_readd() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(
        ClampingBuiltinFactory::new().failing_kind(UnitKind::Reverb, "reverb dsp unavailable"),
    ));

    let bad_spec = UnitSpec { kind: UnitKind::Reverb, plugin: None, params: None, bypass: false };
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 20, bad_spec, 0).unwrap_err();
    assert!(err.contains("reverb dsp unavailable"), "{err}");

    {
        let mut st = host.core.state.lock().unwrap();
        assert!(!st.table.is_id_reserved(20), "id must be free after failed add");
        let rev = st.model.snapshot();
        assert_eq!(rev.chains[0].units.len(), 1);
        assert_eq!(rev.chains[0].units[0].id, 20);
        assert_eq!(rev.chains[0].units[0].state, PlateState::Failed);
        assert_eq!(rev.chains[0].units[0].error.as_deref(), Some("reverb dsp unavailable"));
    }

    assert_eq!(chains.remove(host, (&doc_state, epoch), 20), Ok(()));
    {
        let mut st = host.core.state.lock().unwrap();
        assert!(st.model.snapshot().chains[0].units.is_empty());
    }

    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 20, echo_spec(0.4, false), 0), Ok(Added::Seated));

    chains.unload_all(host);
    chains.join_all();
}

/// (d) (g) A replaced document's add, remove and move are refused ("stale frontend epoch"), and nothing
/// is reserved.
#[test]
fn d_stale_frontend_epoch_refused_for_add_remove_move() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let old_epoch = doc_state.begin_frontend_session();
    let new_epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    let err_add = add_builtin(&chains, host, (&doc_state, old_epoch), &slots, 0, 30, echo_spec(0.5, false), 0).unwrap_err();
    assert!(err_add.contains("stale frontend epoch"), "{err_add}");
    assert!(!chain_units_exist(host), "stale add must reserve nothing");

    // Seed two real units under new_epoch so remove and move would otherwise succeed.
    assert_eq!(add_builtin(&chains, host, (&doc_state, new_epoch), &slots, 0, 31, echo_spec(0.5, false), 0), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, host, (&doc_state, new_epoch), &slots, 0, 32, echo_spec(0.5, false), 1), Ok(Added::Seated));

    let err_move = chains.move_unit(host, (&doc_state, old_epoch), 32, 0).unwrap_err();
    assert!(err_move.contains("stale frontend epoch"), "{err_move}");
    let err_rem = chains.remove(host, (&doc_state, old_epoch), 31).unwrap_err();
    assert!(err_rem.contains("stale frontend epoch"), "{err_rem}");

    assert_eq!(engine_chain_summary(&device, 0), vec![(31, false), (32, false)]);

    chains.unload_all(host);
    chains.join_all();
}

/// (e) (g) The run exclusion: an add is refused while the legacy slots hold `EngineSlot::Loading`/
/// `Loaded` (built in the test). `chain_units_exist` is true after an add and false after its removal.
#[test]
fn e_run_exclusion_with_legacy_slots_and_chain_units_exist() {
    struct Silent;
    impl UnitProcessor for Silent {
        fn info(&self) -> UnitInfo {
            UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 }
        }
        fn process(&mut self, _cx: &UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
            for ch in out {
                ch.fill(0.0);
            }
        }
        fn stop(&mut self) {}
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    // Loading in slot 1 refuses chain add in slot 0.
    let slots = Mutex::new([
        EngineSlot::Empty,
        EngineSlot::Loading { epoch, cancelled: false },
    ]);
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 40, echo_spec(0.5, false), 0).unwrap_err();
    assert!(err.contains("slot 1") && err.contains("plugin slot"), "{err}");
    assert!(!chain_units_exist(host));

    // Loaded in slot 0 (handle backed by a separate TestDevice so `host` itself has no legacy row,
    // ensuring `Chains::add`'s own `slots` guard fires).
    let other_device = start_device();
    let handle = Arc::new(
        engine_slot::stand_in(other_device.host().slot(0), Box::new(Silent), || {}, || {}).unwrap(),
    );
    let info = PluginInfo {
        slot: 0,
        descriptor: PluginDescriptor {
            id: "test.amp".into(),
            name: "Stand-in".into(),
            format: "clap".into(),
            path: "C:\\plugins\\Amp.clap".into(),
            is_effect: Some(true),
        },
        tone: None,
    };
    {
        let mut s = slots.lock().unwrap();
        s[0] = EngineSlot::Loaded { handle: handle.clone(), info };
        s[1] = EngineSlot::Empty;
    }
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 1, 41, echo_spec(0.5, false), 0).unwrap_err();
    assert!(err.contains("slot 0") && err.contains("plugin slot"), "{err}");
    assert!(!chain_units_exist(host));

    // Clear legacy slots and unload the stand-in cleanly.
    {
        let mut s = slots.lock().unwrap();
        s[0] = EngineSlot::Empty;
    }
    let _ = Arc::try_unwrap(handle).ok().expect("sole handle").unload();

    // Now chain add succeeds, `chain_units_exist` becomes true, legacy `reserve` refuses before calling
    // the legacy opener, and `chain_units_exist` turns false after removal releases.
    assert!(!chain_units_exist(host));
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 42, echo_spec(0.5, false), 0), Ok(Added::Seated));
    assert!(chain_units_exist(host));

    let legacy_opener_called = Arc::new(AtomicBool::new(false));
    let called_flag = legacy_opener_called.clone();
    let legacy_err = crate::engine_io::plugins::load_with_opener(
        &slots,
        host,
        &doc_state,
        0,
        "C:\\plugins\\Amp.clap".into(),
        "test.amp".into(),
        epoch,
        &move |_, _, _, _, _| {
            called_flag.store(true, Ordering::SeqCst);
            Err("legacy opener must not be reached when chain units exist".into())
        },
    )
    .unwrap_err();
    assert!(
        legacy_err.contains("chain units are in this run"),
        "expected legacy reserve exclusion error, got: {legacy_err}"
    );
    assert!(
        !legacy_opener_called.load(Ordering::SeqCst),
        "legacy reserve must refuse before invoking the legacy opener"
    );
    assert!(matches!(slots.lock().unwrap()[0], EngineSlot::Empty));

    assert_eq!(chains.remove(host, (&doc_state, epoch), 42), Ok(()));
    wait_until(Duration::from_secs(2), "chain_units_exist becomes false after removal", || {
        !chain_units_exist(host)
    });

    chains.unload_all(host);
    chains.join_all();
}

/// (f) (g) Round D's answer to the UI: an ECHO with latency 64 seated, a take open (`start_take`). The
/// remove (on a thread) is held, and an add under the same id is refused with the "still leaving" words.
/// End the take: the remove answers `Ok`, and the same id then adds.
#[test]
fn f_held_remove_during_take_refuses_readd_with_still_leaving_then_succeeds() {
    let device = start_device();
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_latency(64))));

    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 50, echo_spec(0.5, false), 0), Ok(Added::Seated));
    start_take(&device);

    let rem_chains = chains.clone();
    let rem_host = host.clone();
    let rem_doc = doc_state.clone();
    let rem_thread = std::thread::spawn(move || rem_chains.remove(&rem_host, (&rem_doc, epoch), 50));

    // Wait until the removal is held in the engine/model.
    wait_until(Duration::from_secs(3), "removal held in model", || {
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 50).is_some() && st.model.slot_held(0)
    });
    assert!(!rem_thread.is_finished(), "remove must remain held during the open take");

    // Adding under the same id must NOT block on the take and must fail with "still leaving".
    let err = add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 50, echo_spec(0.5, false), 0).unwrap_err();
    assert!(err.contains("still leaving"), "expected 'still leaving' refusal, got: {err}");

    stop_take(&device);
    assert_eq!(rem_thread.join().unwrap(), Ok(()));

    // Wait for the crossfaded unit's owner to release the old reservation, then re-add the same id.
    wait_until(Duration::from_secs(2), "id 50 released after remove", || {
        let st = host.core.state.lock().unwrap();
        !st.table.is_id_reserved(50)
    });
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 50, echo_spec(0.7, false), 0), Ok(Added::Seated));

    chains.unload_all(&host);
    chains.join_all();
}

/// (g) (g) `factory: None`: a built-in add is refused at once, and nothing is reserved.
#[test]
fn g_no_factory_refuses_builtin_without_reserving() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();

    let t0 = Instant::now();
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 60, echo_spec(0.5, false), 0).unwrap_err();
    assert!(t0.elapsed() < Duration::from_millis(100));
    assert!(err.contains("ECHO") && err.contains("later build"), "{err}");
    assert!(!chain_units_exist(host), "refused add must reserve nothing");
}

/// (h) A move: three ECHOs, `move(c, 0)` -> `Ok`, and the engine's order equals the snapshot's.
#[test]
fn h_move_reorders_engine_and_snapshot() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 70, echo_spec(0.2, false), 0), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 71, echo_spec(0.4, true), 1), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 72, echo_spec(0.6, false), 2), Ok(Added::Seated));

    assert_eq!(chains.move_unit(host, (&doc_state, epoch), 72, 0), Ok(()));

    let eng = engine_chain_summary(&device, 0);
    let snap = snapshot_chain_summary(&device, 0);
    assert_eq!(eng, vec![(72, false), (70, false), (71, true)]);
    assert_eq!(eng, snap);

    chains.unload_all(host);
    chains.join_all();
}

/// (i) A rebuild (`TestDevice::rebuild_at` at another rate): each built-in comes back seated with its
/// bypass and level (the owner resubmits the same box instances, never rebuilding equivalent boxes).
#[test]
fn i_rebuild_reseats_builtins_with_bypass_and_level() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.5);
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let tracker = BoxTracker::new();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_tracker(tracker.clone())));

    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 80, echo_spec(0.25, false), 0), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 81, echo_spec(0.8, true), 1), Ok(Added::Seated));
    host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
    assert!(device.wait_blocks(4));

    assert_eq!(tracker.builds.load(Ordering::SeqCst), 2, "two original boxes built");
    assert_eq!(tracker.drops.load(Ordering::SeqCst), 0, "neither box dropped before rebuild");
    let calls_before_1 = tracker.calls(1);
    let calls_before_2 = tracker.calls(2);
    assert!(calls_before_1 > 0, "original box 1 processed blocks before rebuild");
    assert!(calls_before_2 > 0, "original box 2 (bypassed, warm) processed blocks before rebuild");
    assert_eq!(tracker.stops(1), 0);
    assert_eq!(tracker.stops(2), 0);

    device.rebuild_at(96_000, 512);

    wait_until(Duration::from_secs(3), "both units reseated after rebuild", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let eng = engine_chain_summary(&device, 0);
        let st = host.core.state.lock().unwrap();
        let all_seated = [80u32, 81u32].into_iter().all(|id| {
            st.model
                .find_plate(id)
                .is_some_and(|(_, p)| p.state == PlateState::Seated && p.seated_in_engine)
        });
        eng == vec![(80, false), (81, true)] && all_seated
    });

    // Verify rendered output reflects level 0.25 (unit 80 active, unit 81 bypassed) on 0.5 input -> ~0.125,
    // AND verify the exact same two box instances (1 and 2) were evicted once and keep processing in the rebuilt engine.
    assert!(device.wait_blocks(8));
    assert_eq!(tracker.builds.load(Ordering::SeqCst), 2, "rebuild must resubmit same boxes, not build new ones");
    assert_eq!(tracker.drops.load(Ordering::SeqCst), 0, "original boxes must survive rebuild intact");
    assert_eq!(tracker.stops(1), 1, "original box 1 stopped once during eviction");
    assert_eq!(tracker.stops(2), 1, "original box 2 stopped once during eviction");
    assert!(
        tracker.calls(1) > calls_before_1,
        "original box 1 must keep processing in rebuilt engine ({} vs {})",
        tracker.calls(1),
        calls_before_1
    );
    assert!(
        tracker.calls(2) > calls_before_2,
        "original box 2 (bypassed, warm) must keep processing in rebuilt engine ({} vs {})",
        tracker.calls(2),
        calls_before_2
    );

    let tail_peak = {
        let out = device.output.lock().unwrap();
        out.iter().rev().take(256).fold(0.0f32, |m, &s| m.max(s.abs()))
    };
    assert!((tail_peak - 0.125).abs() < 0.03, "expected ~0.125 after rebuild, got {tail_peak}");

    let rev = {
        let mut st = host.core.state.lock().unwrap();
        st.model.snapshot()
    };
    assert_eq!(rev.chains[0].units[0].bypass, false);
    assert_eq!(rev.chains[0].units[1].bypass, true);
    assert_eq!(
        rev.chains[0].units[0].params.as_ref().map(|p| &p.0),
        Some(&vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.25)])
    );
    assert_eq!(
        rev.chains[0].units[1].params.as_ref().map(|p| &p.0),
        Some(&vec![(UnitParam::Send(InputSendParam::EchoLevel), 0.8)])
    );

    chains.unload_all(host);
    chains.join_all();
}

/// (j) Shutdown: two ECHOs and a held removal in an open take over a real fake-driver device owner.
/// `unload_all` + `host.shutdown()` + `join_all` ends within 3 s, every owner thread finishes, and
/// both boxes are evicted and dropped (not abandoned).
#[test]
fn j_shutdown_with_held_removal_in_open_take_finishes_within_3s() {
    let host = start_fake_driver_host();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let tracker = BoxTracker::new();
    let chains = Arc::new(Chains::with_factory(Arc::new(
        ClampingBuiltinFactory::new().with_latency(64).with_tracker(tracker.clone()),
    )));

    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 90, echo_spec(0.3, false), 0), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 91, echo_spec(0.6, false), 1), Ok(Added::Seated));
    start_take_on_host(&host);

    let rem_chains = chains.clone();
    let rem_host = host.clone();
    let rem_doc = doc_state.clone();
    let rem_thread = std::thread::spawn(move || rem_chains.remove(&rem_host, (&rem_doc, epoch), 90));

    wait_until(Duration::from_secs(3), "removal held before shutdown", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 90).is_some() && st.model.slot_held(0)
    });

    let t0 = Instant::now();
    chains.unload_all(&host);
    host.shutdown();
    chains.join_all();
    let elapsed = t0.elapsed();
    assert!(elapsed < Duration::from_secs(3), "shutdown took {elapsed:?} (must be < 3 s)");

    // Background held removal unblocked on closing and finished too.
    let _ = rem_thread.join().unwrap();
    assert_eq!(chains.active_entries(), 0, "no owner entries remain after join_all");
    assert_eq!(
        tracker.drops.load(Ordering::SeqCst),
        2,
        "both boxes must be returned by device shutdown and dropped, not abandoned"
    );
    let st = host.core.state.lock().unwrap();
    assert!(!st.table.has_any_chain(), "every owner thread released its row and exited");
}

/// (k) `into_spec`: each refusal and each accepted shape (bypass carried, params kept in order).
#[test]
fn k_unit_spec_into_spec_accepts_and_refuses() {
    let pref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\Amp.clap".into(),
        id: "amp.one".into(),
    };
    let ordered_params = vec![
        (UnitParam::Send(InputSendParam::EchoLevel), 0.7),
        (UnitParam::Send(InputSendParam::EchoFeedback), 0.3),
    ];

    // Refusals
    let err = UnitSpec { kind: UnitKind::Plugin, plugin: None, params: None, bypass: false }
        .into_spec()
        .unwrap_err();
    assert!(err.contains("plugin"), "{err}");

    let err = UnitSpec {
        kind: UnitKind::Plugin,
        plugin: Some(pref.clone()),
        params: Some(WireParams(ordered_params.clone())),
        bypass: false,
    }
    .into_spec()
    .unwrap_err();
    assert!(err.contains("param"), "{err}");

    let err = UnitSpec {
        kind: UnitKind::Echo,
        plugin: Some(pref.clone()),
        params: None,
        bypass: false,
    }
    .into_spec()
    .unwrap_err();
    assert!(err.contains("plugin"), "{err}");

    // Accepted shapes
    let (spec, bypass) = UnitSpec {
        kind: UnitKind::Plugin,
        plugin: Some(pref.clone()),
        params: None,
        bypass: true,
    }
    .into_spec()
    .unwrap();
    assert_eq!(spec, Spec::Plugin(pref));
    assert!(bypass);

    let (spec, bypass) = UnitSpec {
        kind: UnitKind::Ott,
        plugin: None,
        params: None,
        bypass: true,
    }
    .into_spec()
    .unwrap();
    assert_eq!(spec, Spec::Builtin { kind: UnitKind::Ott, params: vec![] });
    assert!(bypass);

    let (spec, bypass) = UnitSpec {
        kind: UnitKind::Echo,
        plugin: None,
        params: Some(WireParams(ordered_params.clone())),
        bypass: false,
    }
    .into_spec()
    .unwrap();
    assert_eq!(spec, Spec::Builtin { kind: UnitKind::Echo, params: ordered_params });
    assert!(!bypass);
}

// ── Round 2 tests: plugin path, affinity, events, tone import/tokens, reload, stale epochs ──

struct StandInPlugin;

impl UnitProcessor for StandInPlugin {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: 0 }
    }
    fn process(&mut self, _cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        for (o, i) in out.into_iter().zip(input) {
            o.copy_from_slice(i);
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

fn seat_stand_in_plugin(host: &EngineHost, slot: usize, id: UnitId, plugin: &PluginRef, at: usize) -> UnitHost {
    let uh = host.unit(slot, id, Spec::Plugin(plugin.clone()), at, false).expect("reserve stand-in");
    let terms = uh.terms().expect("engine terms");
    uh.activated(Activated { name: plugin.id.clone(), source: false, latency: 0 });
    let op = uh.submit(Box::new(StandInPlugin), terms).map_err(|(_, e)| e).expect("submit stand-in");
    assert_eq!(host.wait_op(op, Duration::from_secs(2)), Ok(OpState::Applied));
    uh
}

fn temp_tone_store(tag: &str) -> (ToneStore, std::path::PathBuf) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bleeploop-p4r2-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (ToneStore::new(dir.clone()), dir)
}

fn make_tone_bytes(identity: &ToneIdentity, name: &str, state: &[u8]) -> Vec<u8> {
    encode_tone(&Tone {
        identity: identity.clone(),
        name: name.to_string(),
        state: state.to_vec(),
    })
}

struct WarnCapture;
static WARN_LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static WARN_LOGGER_INSTALLED: AtomicBool = AtomicBool::new(false);

impl log::Log for WarnCapture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            WARN_LOGS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{}", record.args()));
        }
    }
    fn flush(&self) {}
}

fn install_warn_capture() -> bool {
    static LOGGER: WarnCapture = WarnCapture;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
        WARN_LOGGER_INSTALLED.store(true, Ordering::Release);
    }
    WARN_LOGGER_INSTALLED.load(Ordering::Acquire)
}

/// R2 (a): The affinity table: two instances of one path, the second refused; `unordered` refuses both;
/// `orphaned` refuses both; last release deletes the entry and the next claim owns afresh; a `leak`ed
/// holder keeps it.
#[test]
fn r2_a_affinity_table_rules() {
    let mut aff = Affinity::default();
    let path = "C:\\plugins\\Amp.clap";

    // Two instances of one path (slot-blind): owner allowed, second refused with owner's unit id.
    let i1 = aff.claim(path);
    aff.landed(i1, path, 10);
    let i2 = aff.claim(path);
    aff.landed(i2, path, 20);
    assert_eq!(aff.check(path, i1), Ok(()));
    let err = aff.check(path, i2).unwrap_err();
    assert!(err.contains("belongs to unit 10"), "{err}");

    // Owner releases while second still holds -> orphaned; refuses both existing holder and new loads.
    aff.release(i1, path);
    let err = aff.check(path, i2).unwrap_err();
    assert!(err.contains("bound to unit 10's previous load"), "{err}");
    let i3 = aff.claim(path);
    aff.landed(i3, path, 30);
    let err = aff.check(path, i3).unwrap_err();
    assert!(err.contains("bound to unit 10's previous load"), "{err}");

    // Last release deletes the entry; next claim owns afresh.
    aff.release(i2, path);
    aff.release(i3, path);
    assert!(!aff.is_held(path));
    let i4 = aff.claim(path);
    aff.landed(i4, path, 40);
    assert_eq!(aff.check(path, i4), Ok(()));
    aff.release(i4, path);
    assert!(!aff.is_held(path));

    // Unordered: a second claim while the owner's load is still in flight refuses both.
    let u1 = aff.claim(path);
    let u2 = aff.claim(path);
    aff.landed(u1, path, 50);
    aff.landed(u2, path, 51);
    let e1 = aff.check(path, u1).unwrap_err();
    let e2 = aff.check(path, u2).unwrap_err();
    assert!(e1.contains("belongs to an unknown unit (two loaded it at once)"), "{e1}");
    assert!(e2.contains("belongs to an unknown unit (two loaded it at once)"), "{e2}");
    aff.release(u1, path);
    aff.release(u2, path);
    assert!(!aff.is_held(path));

    // A leaked holder keeps the entry even after the other holder releases.
    let l1 = aff.claim(path);
    aff.landed(l1, path, 60);
    let l2 = aff.claim(path);
    aff.leak(l2, path);
    aff.release(l1, path);
    assert!(aff.is_held(path));
    let l3 = aff.claim(path);
    aff.landed(l3, path, 61);
    let err = aff.check(path, l3).unwrap_err();
    assert!(err.contains("bound to unit 60's previous load"), "{err}");
}

/// R2 (b): A plugin add whose load fails, through the real `load_unit` with a missing `.clap` path:
/// `Err` with the load's words, the plate `failed`, the id free, no affinity holder left; `Chains::remove`
/// takes the plate.
#[test]
fn r2_b_failed_plugin_add_via_real_load_unit_marks_plate_failed_and_clears_affinity() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();

    let missing_path = "C:\\__bleeploop_nonexistent_plugin_dir__\\MissingAmp.clap";
    let pref = PluginRef {
        format: "clap".into(),
        path: missing_path.into(),
        id: "missing.amp".into(),
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(pref), params: None, bypass: false };
    let real_open = |format: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit_host, 0, sink, tone)
    };

    let err = chains
        .add(host, (&doc_state, epoch), &slots, None, 0, 110, spec, 0, None, noop_sink(), &real_open)
        .unwrap_err();
    assert!(err.contains("load failed"), "expected real load_unit failure words, got: {err}");
    assert!(!chains.affinity_held(missing_path), "failed add must release its affinity holder");

    {
        let mut st = host.core.state.lock().unwrap();
        assert!(!st.table.is_id_reserved(110), "id must be free after failed plugin add");
        let rev = st.model.snapshot();
        assert_eq!(rev.chains[0].units.len(), 1);
        assert_eq!(rev.chains[0].units[0].id, 110);
        assert_eq!(rev.chains[0].units[0].state, PlateState::Failed);
        assert_eq!(rev.chains[0].units[0].error.as_deref(), Some(err.as_str()));
    }

    assert_eq!(chains.remove(host, (&doc_state, epoch), 110), Ok(()));
    {
        let mut st = host.core.state.lock().unwrap();
        assert!(st.model.snapshot().chains[0].units.is_empty());
    }
}

/// R2 (c): `chain_event`'s three mappings, names and payload keys exact (`unit`, `id`, `value`).
#[test]
fn r2_c_chain_event_mappings_exact() {
    let (name, val) = chain_event(7, EngineSlotEvent::EditorClosed);
    assert_eq!(name, EDITOR_CLOSED);
    assert_eq!(name, "chain:editor-closed");
    assert_eq!(val, serde_json::json!({ "unit": 7 }));

    let (name, val) = chain_event(9, EngineSlotEvent::ParamChanged { id: 42, value: 0.75 });
    assert_eq!(name, PARAM_CHANGED);
    assert_eq!(name, "chain:param-changed");
    assert_eq!(val, serde_json::json!({ "unit": 9, "id": 42, "value": 0.75 }));

    let (name, val) = chain_event(12, EngineSlotEvent::ParamsChanged);
    assert_eq!(name, PARAMS_CHANGED);
    assert_eq!(name, "chain:params-changed");
    assert_eq!(val, serde_json::json!({ "unit": 12 }));
}

/// R2 (d): Tone import with no seated match: `token: None`, slot file holds the tone. Another plugin's
/// tone is refused and nothing is written. A stale `epoch` is refused.
#[test]
fn r2_d_tone_import_no_match_wrong_plugin_and_stale_epoch() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let old_epoch = doc_state.begin_frontend_session();
    let epoch = doc_state.begin_frontend_session();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("r2d");

    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp.one".into() };
    let other = PluginRef { format: "clap".into(), path: "C:\\plugins\\Other.clap".into(), id: "other.one".into() };
    let bytes = make_tone_bytes(&amp, "Amp One", b"amp-state-v1");

    // Stale epoch refused, nothing written.
    let err = chains.tone_import(host, (&doc_state, old_epoch), Some(&store), 0, &bytes, &amp, None).unwrap_err();
    assert!(err.contains("stale frontend epoch"), "{err}");
    assert_eq!(store.load(0, &amp).unwrap(), None);

    // Another plugin's tone refused, nothing written.
    let err = chains.tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &other, None).unwrap_err();
    assert!(err.contains("Amp One"), "{err}");
    assert_eq!(store.load(0, &amp).unwrap(), None);
    assert_eq!(store.load(0, &other).unwrap(), None);

    // No seated match: stored as slot file, token is None.
    let imported = chains.tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, None).unwrap();
    assert_eq!(imported.token, None);
    assert_eq!(imported.name, "Amp One");
    assert!(imported.matches.is_empty());
    let loaded = store.load(0, &amp).unwrap().expect("slot file written");
    assert_eq!(loaded.state, b"amp-state-v1");

    let _ = std::fs::remove_dir_all(&dir);
}

/// R2 (e): Tone import matches with stand-in seated plugin plates: hinted unit in the other slot first,
/// then the import slot's others of that identity in chain order; another identity left out. Token takes
/// only for that identity; `forget` drops it.
#[test]
fn r2_e_tone_import_matches_order_take_and_forget() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("r2e");

    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp.one".into() };
    let delay = PluginRef { format: "clap".into(), path: "C:\\plugins\\Delay.clap".into(), id: "delay.one".into() };

    // Slot 0: amp (101), delay (102), amp (103). Slot 1: amp (201, hinted).
    let u101 = seat_stand_in_plugin(host, 0, 101, &amp, 0);
    let u102 = seat_stand_in_plugin(host, 0, 102, &delay, 1);
    let u103 = seat_stand_in_plugin(host, 0, 103, &amp, 2);
    let u201 = seat_stand_in_plugin(host, 1, 201, &amp, 0);

    let bytes = make_tone_bytes(&amp, "Amp One", b"amp-state-v2");
    let imported = chains
        .tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(201))
        .unwrap();
    let tok = imported.token.expect("matches -> parked token");
    assert_eq!(imported.name, "Amp One");
    assert_eq!(imported.matches, vec![201, 101, 103]);
    // Nothing written to slot file when units matched.
    assert_eq!(store.load(0, &amp).unwrap(), None);

    // Token does not take for another identity, and forget drops it.
    assert!(chains.take_parked_tone(tok, &delay).is_none());
    assert_eq!(chains.tone_forget((&doc_state, epoch), tok), Ok(()));
    assert!(chains.take_parked_tone(tok, &amp).is_none());

    // Re-import and take with the right identity succeeds once.
    let imported2 = chains
        .tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(201))
        .unwrap();
    let tok2 = imported2.token.expect("second parked token");
    let taken = chains.take_parked_tone(tok2, &amp).expect("takes for matching identity");
    assert_eq!(taken.state, b"amp-state-v2");
    assert!(chains.take_parked_tone(tok2, &amp).is_none());

    host.shutdown();
    u101.release();
    u102.release();
    u103.release();
    u201.release();
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2 (f): Reload `moved` (untold, `since` older than `told`, another plugin, plate not seated), and
/// reload `failed` when `reload_unit` refuses after `rebuild_at` (not yet resubmitted), with reason logged.
#[test]
fn r2_f_reload_moved_and_failed_with_warn_log() {
    let captured_logs = install_warn_capture();
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("r2f");

    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp.one".into() };
    let other = PluginRef { format: "clap".into(), path: "C:\\plugins\\Other.clap".into(), id: "other.one".into() };

    let u200 = seat_stand_in_plugin(host, 0, 200, &amp, 0);
    // Unsubmitted stand-in plate (Loading, not Seated).
    let u201 = host.unit(0, 201, Spec::Plugin(amp.clone()), 1, false).unwrap();

    // Untold -> moved.
    assert_eq!(host.told(200), None);
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 200, amp.clone(), 10, 1, noop_sink(), &no_open),
        Ok(Reloaded::Moved)
    );

    // Mark plates told at seq 5.
    let rev = {
        let mut st = host.core.state.lock().unwrap();
        st.model.snapshot().rev
    };
    host.chain_told(rev, 5);
    assert_eq!(host.told(200), Some(5));

    // `since` older than `told` -> moved.
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 200, amp.clone(), 4, 1, noop_sink(), &no_open),
        Ok(Reloaded::Moved)
    );
    // Another plugin -> moved.
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 200, other.clone(), 5, 1, noop_sink(), &no_open),
        Ok(Reloaded::Moved)
    );
    // Plate not seated (Loading) -> moved.
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 201, amp.clone(), 5, 1, noop_sink(), &no_open),
        Ok(Reloaded::Moved)
    );

    // After rebuild_at (not yet resubmitted), plate stays Seated in UI state while seated_in_engine=false,
    // so `host.reload_unit` refuses -> `Reloaded::Failed` with reason logged at warn.
    device.rebuild_at(96_000, 512);
    if captured_logs {
        WARN_LOGS.get_or_init(|| Mutex::new(Vec::new())).lock().unwrap().clear();
    }
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 200, amp.clone(), 5, 1, noop_sink(), &no_open),
        Ok(Reloaded::Failed)
    );
    if captured_logs {
        let logs = WARN_LOGS.get().unwrap().lock().unwrap();
        assert!(
            logs.iter().any(|line| line.contains("200") && line.contains("not seated")),
            "expected reload failure reason logged at warn, got: {logs:?}"
        );
    }

    host.shutdown();
    u200.release();
    u201.release();
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2 (g): A replaced document's reload, open_editor, set_param and tone_forget are refused.
#[test]
fn r2_g_stale_frontend_epoch_refused_for_reload_editor_param_and_forget() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let old_epoch = doc_state.begin_frontend_session();
    let _new_epoch = doc_state.begin_frontend_session();
    let chains = Chains::new();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp.one".into() };

    let err_reload = chains
        .reload(host, (&doc_state, old_epoch), None, 1, amp, 0, 1, noop_sink(), &no_open)
        .unwrap_err();
    assert!(err_reload.contains("stale frontend epoch"), "{err_reload}");

    let err_open = chains.open_editor(host, (&doc_state, old_epoch), 1).unwrap_err();
    assert!(err_open.contains("stale frontend epoch"), "{err_open}");

    let err_set = chains.set_param(host, (&doc_state, old_epoch), 1, 0, 0.5).unwrap_err();
    assert!(err_set.contains("stale frontend epoch"), "{err_set}");

    let err_forget = chains.tone_forget((&doc_state, old_epoch), 1).unwrap_err();
    assert!(err_forget.contains("stale frontend epoch"), "{err_forget}");
}

// ── P4 fix round 1 tests: ownership and order in chain commands ──

fn start_fake_driver_host() -> EngineHost {
    let fake = FakeDriver(Fake::new());
    let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 2.0 }, fake).unwrap();
    host.open(
        DeviceRequest {
            backend: AudioBackend::Asio,
            input: None,
            output: None,
            input_channels: [None; SLOT_COUNT],
            buffer: Some(256),
            sample_rate: None,
        },
        false,
    )
    .unwrap();
    host
}

fn start_take_on_host(host: &EngineHost) {
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
        assert!(Instant::now() < deadline, "recording take never started");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Finding 1: chain commands never act on a legacy plate (reachable even with app factory `None`).
#[test]
fn p4f1_1_chain_commands_refuse_legacy_plate() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let chains = Chains::new();

    // Seat a legacy plate in slot 0 with id 1.
    let legacy_uh = host.unit(0, 1, Spec::Legacy, 0, false).expect("reserve legacy plate");
    let terms = legacy_uh.terms().expect("terms");
    let op = legacy_uh.submit(Box::new(StandInPlugin), terms).map_err(|(_, e)| e).expect("submit legacy");
    assert_eq!(host.wait_op(op, Duration::from_secs(2)), Ok(OpState::Applied));

    let pref = PluginRef { format: "clap".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp.one".into() };
    assert!(chains.remove(host, (&doc_state, epoch), 1).is_err(), "chain_remove must refuse legacy plate");
    assert!(chains.move_unit(host, (&doc_state, epoch), 1, 0).is_err(), "chain_move must refuse legacy plate");
    assert!(
        chains.reload(host, (&doc_state, epoch), None, 1, pref, 10, 1, noop_sink(), &no_open).is_err(),
        "chain_reload must refuse legacy plate"
    );
    assert!(chains.open_editor(host, (&doc_state, epoch), 1).is_err(), "chain_open_editor must refuse legacy plate");
    assert!(chains.close_editor(host, (&doc_state, epoch), 1).is_err(), "chain_close_editor must refuse legacy plate");
    assert!(chains.list_params(host, 1).is_err(), "chain_list_params must refuse legacy plate");
    assert!(chains.set_param(host, (&doc_state, epoch), 1, 0, 0.5).is_err(), "chain_set_param must refuse legacy plate");
    assert!(chains.tone_take(host, 1).is_err(), "chain_tone_take must refuse legacy plate");

    // Legacy unit must remain untouched in the engine.
    assert_eq!(engine_chain_summary(&device, 0), vec![(1, false)]);

    let Removal::Sent(rem) = host.remove_unit(1).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(rem, Duration::from_secs(2)), Ok(OpState::Applied));
    drop(legacy_uh.take_back());
    legacy_uh.release();
}

/// Finding 2a: owner spawn failure drops the box and releases the reservation row cleanly.
#[test]
fn p4f1_2a_spawn_failure_releases_row_and_drops_box() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let tracker = BoxTracker::new();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_tracker(tracker.clone())));

    chains.fail_next_spawn.store(true, Ordering::Release);
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 201, echo_spec(0.5, false), 0).unwrap_err();
    assert!(err.contains("spawn"), "expected spawn error, got: {err}");

    assert_eq!(tracker.drops.load(Ordering::SeqCst), 1, "box must be dropped on spawn failure");
    {
        let st = host.core.state.lock().unwrap();
        assert!(!st.table.is_id_reserved(201), "row must be released on spawn failure");
        assert!(!st.table.has_any_chain(), "no stranded chain row on spawn failure");
    }
    // Same id can now be added cleanly.
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 201, echo_spec(0.5, false), 0), Ok(Added::Seated));
    chains.unload_all(host);
    chains.join_all();
}

/// Finding 2b: shutdown taking its registry snapshot during owner startup must not leak the box or
/// leave a stranded owner.
#[test]
fn p4f1_2b_shutdown_during_owner_startup_drops_box() {
    let host = start_fake_driver_host();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = Arc::new(empty_slots());
    let tracker = BoxTracker::new();
    let chains = Arc::new(Chains::with_factory(Arc::new(
        ClampingBuiltinFactory::new().with_tracker(tracker.clone()),
    )));

    let entered = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    {
        let entered = entered.clone();
        let resume = resume.clone();
        *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || {
            entered.wait();
            resume.wait();
        }));
    }

    let add_chains = chains.clone();
    let add_host = host.clone();
    let add_doc = doc_state.clone();
    let add_slots = slots.clone();
    let add_thread = std::thread::spawn(move || {
        add_builtin(&add_chains, &add_host, (&add_doc, epoch), &add_slots, 0, 202, echo_spec(0.5, false), 0)
    });

    // Wait until `add_builtin` reaches the point right before registry insertion, then run `unload_all`
    // while the device is still playing (BEFORE `host.shutdown()`).
    entered.wait();
    chains.unload_all(&host);
    resume.wait();

    let res = add_thread.join().unwrap();
    assert!(res.is_err(), "add racing with closing must return Err, got: {res:?}");

    host.shutdown();
    chains.join_all();

    assert_eq!(chains.active_entries(), 0, "no owner entries left after join_all");
    assert_eq!(
        tracker.drops.load(Ordering::SeqCst),
        1,
        "box must be dropped cleanly, never abandoned/leaked across shutdown gap"
    );
    let st = host.core.state.lock().unwrap();
    assert!(!st.table.has_any_chain(), "every owner released its row");
}

/// Finding 3b: a refused add's cleanup resolves by its own install token and never stops a newer
/// owner that reused the freed id in the other slot.
#[test]
fn p4f1_3b_refused_add_cleanup_does_not_stop_newer_owner() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    // Simulate a first add in slot 0 that called `failed()` (freeing id 301) before `release()`.
    let u1 = host
        .unit(0, 301, Spec::Builtin { kind: UnitKind::Echo, params: vec![] }, 0, false)
        .expect("reserve first row");
    let u1_token = {
        let st = host.core.state.lock().unwrap();
        st.table.active_row_for_id(301).expect("first row").token
    };
    u1.failed("first add refused".into());

    // Before `u1.release()`, a concurrent add in slot 1 reserves id 301 and seats a healthy unit.
    assert_eq!(
        add_builtin(&chains, host, (&doc_state, epoch), &slots, 1, 301, echo_spec(0.6, false), 0),
        Ok(Added::Seated)
    );

    let reg_token = chains
        .registry
        .lock()
        .unwrap()
        .get(&301)
        .expect("slot 1 entry registered")
        .token;
    assert_ne!(reg_token, u1_token, "slot 1's entry must record its own install token, not freed u1's token");

    // Simulate first add's error cleanup running with `u1_token`.
    chains.retire_to_gone(301, Some(u1_token));
    assert!(
        chains.registry.lock().unwrap().contains_key(&301),
        "first add's cleanup must not remove slot 1's newer owner"
    );
    assert_eq!(engine_chain_summary(&device, 1), vec![(301, false)]);

    u1.release();
    chains.unload_all(host);
    chains.join_all();
}

/// Finding 4: `remove` and `move` resolve the slot and unit under that slot's order mutex so an id
/// reused in the other slot cannot be mutated while its add is still in flight.
#[test]
fn p4f1_4_remove_and_move_do_not_mutate_id_reused_in_other_slot() {
    struct PausingFactory {
        pause_slot1: Arc<AtomicBool>,
        in_build: Arc<Barrier>,
        release_build: Arc<Barrier>,
    }
    impl BuiltinFactory for PausingFactory {
        fn build(&self, kind: UnitKind, params: &Params, _terms: Terms) -> Result<(Unit, Params), String> {
            if kind == UnitKind::Reverb {
                return Err("fail slot 0 plate".into());
            }
            if self.pause_slot1.swap(false, Ordering::AcqRel) {
                self.in_build.wait();
                self.release_build.wait();
            }
            Ok((
                Box::new(ClampingEcho {
                    level: 0.5,
                    latency: 0,
                    calls: None,
                    stops: None,
                    tracker: None,
                }),
                params.clone(),
            ))
        }
    }

    let device = start_device();
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = Arc::new(empty_slots());
    let in_build = Arc::new(Barrier::new(2));
    let release_build = Arc::new(Barrier::new(2));
    let pause_slot1 = Arc::new(AtomicBool::new(true));
    let chains = Arc::new(Chains::with_factory(Arc::new(PausingFactory {
        pause_slot1: pause_slot1.clone(),
        in_build: in_build.clone(),
        release_build: release_build.clone(),
    })));

    // 1. Create a failed-add plate for id 400 in slot 0 (id is freed).
    let bad = UnitSpec { kind: UnitKind::Reverb, plugin: None, params: None, bypass: false };
    assert!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 400, bad, 0).is_err());

    // 2. Hold slot 0's order mutex and start `remove(400)` on a thread (so it finds slot 0 and waits).
    let hold_slot0 = chains.order[0].lock().unwrap();
    let rem_chains = chains.clone();
    let rem_host = host.clone();
    let rem_doc = doc_state.clone();
    let rem_done = Arc::new(AtomicBool::new(false));
    let rem_done_flag = rem_done.clone();
    let rem_thread = std::thread::spawn(move || {
        let r = rem_chains.remove(&rem_host, (&rem_doc, epoch), 400);
        rem_done_flag.store(true, Ordering::Release);
        r
    });
    // Give `rem_thread` a moment to look up slot 0 and block on `order[0]`.
    std::thread::sleep(Duration::from_millis(30));

    // 3. While `rem_thread` is waiting on slot 0, start an add for id 400 in SLOT 1 that pauses in flight
    //    (holding `order[1]` with plate 400 now in slot 1).
    let add_chains = chains.clone();
    let add_host = host.clone();
    let add_doc = doc_state.clone();
    let add_slots = slots.clone();
    let add_thread = std::thread::spawn(move || {
        add_builtin(&add_chains, &add_host, (&add_doc, epoch), &add_slots, 1, 400, echo_spec(0.5, false), 0)
    });
    in_build.wait();

    // 4. Release `order[0]` while slot 1's add is STILL IN FLIGHT.
    drop(hold_slot0);
    std::thread::sleep(Duration::from_millis(40));

    // `rem_thread` MUST NOT have mutated or removed slot 1's in-flight plate!
    let rem_ran_early = rem_done.load(Ordering::Acquire);
    let slot1_still_has_400 = {
        let st = host.core.state.lock().unwrap();
        st.model.find_plate(400).is_some_and(|(s, _)| s == 1)
    };

    // Release slot 1's build so neither thread hangs regardless of assertion outcome.
    release_build.wait();
    let add_res = add_thread.join().unwrap();
    let rem_res = rem_thread.join().unwrap();

    assert!(!rem_ran_early, "remove must not finish while slot 1's add of reused id 400 is still in flight");
    assert!(slot1_still_has_400, "slot 1's in-flight plate 400 must not be removed under slot 0's lock");
    assert_eq!(add_res, Ok(Added::Seated), "slot 1's add must seat cleanly before remove acts");
    assert_eq!(rem_res, Ok(()), "remove must then proceed under slot 1's order");

    // Repeat for `move_unit`: failed plate 401 in slot 0, reused in slot 1 (alongside unit 402).
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 1, 402, echo_spec(0.4, false), 0), Ok(Added::Seated));
    let bad2 = UnitSpec { kind: UnitKind::Reverb, plugin: None, params: None, bypass: false };
    assert!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 401, bad2, 0).is_err());

    pause_slot1.store(true, Ordering::Release);
    let hold_slot0 = chains.order[0].lock().unwrap();
    let mv_chains = chains.clone();
    let mv_host = host.clone();
    let mv_doc = doc_state.clone();
    let mv_done = Arc::new(AtomicBool::new(false));
    let mv_done_flag = mv_done.clone();
    let mv_thread = std::thread::spawn(move || {
        let r = mv_chains.move_unit(&mv_host, (&mv_doc, epoch), 401, 0);
        mv_done_flag.store(true, Ordering::Release);
        r
    });
    std::thread::sleep(Duration::from_millis(30));

    let add2_chains = chains.clone();
    let add2_host = host.clone();
    let add2_doc = doc_state.clone();
    let add2_slots = slots.clone();
    let add2_thread = std::thread::spawn(move || {
        add_builtin(&add2_chains, &add2_host, (&add2_doc, epoch), &add2_slots, 1, 401, echo_spec(0.5, false), 1)
    });
    in_build.wait();

    drop(hold_slot0);
    std::thread::sleep(Duration::from_millis(40));
    let mv_ran_early = mv_done.load(Ordering::Acquire);
    release_build.wait();

    let add2_res = add2_thread.join().unwrap();
    let mv_res = mv_thread.join().unwrap();
    assert!(!mv_ran_early, "move_unit must not run while slot 1's add of reused id 401 is still in flight");
    assert_eq!(add2_res, Ok(Added::Seated));
    assert_eq!(mv_res, Ok(()), "move_unit must then succeed under slot 1's order");
    assert_eq!(engine_chain_summary(&device, 1), vec![(401, false), (402, false)]);

    chains.unload_all(&host);
    chains.join_all();
}

/// Finding 5a: a full port behind a held edit must not hold the slot's order mutex for 15 s or leave
/// an owner retrying to enqueue after a later command.
#[test]
fn p4f1_5a_full_port_add_fails_promptly_and_never_enqueues_after_later_command() {
    // 20 ms per 256-frame block keeps the recording take open well past any timeout without hitting max_loop_seconds.
    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let tracker = BoxTracker::new();
    let chains = Chains::with_factory(Arc::new(
        ClampingBuiltinFactory::new().with_latency(64).with_tracker(tracker.clone()),
    ));

    // Seat a latent unit (500) and a helper unit (505), open a take so a latency-changing removal
    // holds the slot's port head, then fill the remaining port capacity behind that held op.
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 500, echo_spec(0.5, false), 0), Ok(Added::Seated));
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 505, echo_spec(0.5, false), 1), Ok(Added::Seated));
    start_take(&device);

    // Queue a held removal of latent unit 500 first, and wait until the model sees slot 0 held.
    let Removal::Sent(_held_rem) = host.remove_unit(500).expect("queue held removal") else {
        panic!("expected Sent removal");
    };
    wait_until(Duration::from_secs(3), "slot 0 held during take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        host.core.state.lock().unwrap().model.slot_held(0)
    });

    // Push moves behind the held removal until slot 0's port is completely full.
    let mut pushed = 0;
    loop {
        match host.move_unit(505, 0) {
            Ok(_) => {
                pushed += 1;
                assert!(pushed <= 64, "slot port did not fill as expected");
            }
            Err(why) => {
                assert!(why.contains("port is full"), "unexpected error filling port: {why}");
                break;
            }
        }
    }

    // Adding unit 501 into the full port must fail promptly (NOT hold order for 15 s) and exit cleanly.
    let t0 = Instant::now();
    let err = add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 501, echo_spec(0.5, false), 2).unwrap_err();
    let elapsed = t0.elapsed();
    assert!(elapsed < Duration::from_secs(2), "full-port add held order too long: {elapsed:?} ({err})");
    assert!(err.contains("port is full"), "expected port-full error, got: {err}");
    assert_eq!(tracker.drops.load(Ordering::SeqCst), 1, "failed full-port add must drop its box");
    assert!(chains.order[0].try_lock().is_ok(), "order mutex must be released immediately on failure");

    // End the take so the port drains, then run a later add (502). Unit 501 must NOT sneak in!
    stop_take(&device);
    wait_until(Duration::from_secs(3), "port drained after take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        !host.core.state.lock().unwrap().model.slot_held(0)
    });

    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 502, echo_spec(0.5, false), 1), Ok(Added::Seated));
    std::thread::sleep(Duration::from_millis(80));
    let eng = engine_chain_summary(&device, 0);
    assert_eq!(eng, vec![(505, false), (502, false)], "failed full-port add 501 must never enqueue after later add 502");

    chains.unload_all(host);
    chains.join_all();
}
