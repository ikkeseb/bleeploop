use std::any::Any;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::instruments::LEAD;
use lf_engine::units::{OttUnit, SynthUnit, SYNTH_GAIN_MAX};
use lf_engine::{
    Command, InputSendParam, Instrument, LaneState, NoteTarget, SlotEvent, TimedCommand, UnitCx, UnitId,
    UnitInfo, UnitKind, UnitParam, UnitProcessor, MAX_UNITS, SLOT_COUNT,
};

use super::{chain_event, chain_units_exist, Added, Affinity, Chains, Reloaded, EDITOR_CLOSED, PARAM_CHANGED, PARAMS_CHANGED};
use crate::audio_output::AudioBackend;
use crate::engine_io::builtins::Builtins;
use crate::engine_io::chain::{PlateState, PluginRef, UnitSpec, UnitState, WireParams};
use crate::engine_io::fake_driver::{Fake, FakeDriver};
use crate::engine_io::plugins::EngineSlot;
use crate::engine_io::test_rig::{start_take, stop_take, TestDevice, PATIENCE};
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

/// `pub(crate)` with the two below for the CLAP fixture's chain tests (`host/clap_restart_fixture.rs`
/// `mod chain_cmds`), which cannot name `EngineSlot` or reach `Chains`' `pub(super)` shutdown steps.
pub(crate) fn empty_slots() -> Mutex<[EngineSlot; SLOT_COUNT]> {
    Mutex::new(std::array::from_fn(|_| EngineSlot::Empty))
}

pub(crate) fn unload_all(chains: &Chains, host: &EngineHost) {
    chains.unload_all(host);
}

pub(crate) fn join_all(chains: &Chains) {
    chains.join_all();
}

fn start_device() -> TestDevice {
    TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0)
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

fn spec_with(kind: UnitKind, params: Params) -> UnitSpec {
    UnitSpec { kind, plugin: None, params: Some(WireParams(params)), bypass: false }
}

/// The plate of `unit` in the model's snapshot.
fn plate_state(host: &EngineHost, unit: UnitId) -> UnitState {
    let rev = host.core.state.lock().unwrap().model.snapshot();
    rev.chains.iter().flat_map(|c| c.units.iter()).find(|u| u.id == unit).cloned().expect("plate")
}

/// `unit`'s params as its plate reads them, as a map (the answer's order is `UnitParam::ALL`'s).
fn plate_params(host: &EngineHost, unit: UnitId) -> BTreeMap<usize, f64> {
    let params = plate_state(host, unit).params.expect("a built-in's params").0;
    let map: BTreeMap<usize, f64> = params.iter().map(|&(p, v)| (p.index(), v)).collect();
    assert_eq!(map.len(), params.len(), "each param answered once");
    let mut order: Vec<usize> = params.iter().map(|&(p, _)| p.index()).collect();
    order.sort_unstable();
    assert_eq!(order, params.iter().map(|&(p, _)| p.index()).collect::<Vec<_>>(), "in UnitParam::ALL's order");
    map
}

fn send_param(p: InputSendParam) -> usize {
    UnitParam::Send(p).index()
}

fn engine_kinds(device: &TestDevice, slot: usize) -> Vec<(UnitId, UnitKind)> {
    let rt = device.host().core.rt.lock().unwrap();
    rt.engine.as_ref().expect("engine present").chain(slot).map(|s| (s.unit, s.kind)).collect()
}

/// The left channel's peak over the last `frames` the device rendered.
fn recent_peak(device: &TestDevice, frames: usize) -> f32 {
    let out = device.output.lock().unwrap();
    out.iter().rev().take(frames).fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// Whether a note played to `slot` sounds: silence first, then heard within 32 blocks of the NoteOn.
fn note_sounds(device: &TestDevice, slot: u8) -> bool {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::SetSlotLive(slot, true) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SelectInstrument(NoteTarget::Slot(slot)) }).unwrap();
    wait_until(PATIENCE, "silence before the note (an earlier note's tail rung out)", || recent_peak(device, 1024) < 1e-4);
    host.send(TimedCommand { frame: None, command: Command::NoteOn(60, 0.9) }).unwrap();
    assert!(device.wait_blocks(32));
    let heard = recent_peak(device, 4096) > 1e-3;
    host.send(TimedCommand { frame: None, command: Command::AllNotesOff }).unwrap();
    assert!(device.wait_blocks(64));
    heard
}

/// Wraps the production factory and keeps each build's kind, asked params, rate and answer, and what a
/// twin built by the same call renders of one note ([`render_note`]).
struct RecordingBuiltins {
    builds: Mutex<Vec<(UnitKind, Params, u32, Params, Vec<f32>)>>,
}

impl BuiltinFactory for RecordingBuiltins {
    fn build(&self, kind: UnitKind, params: &Params, terms: Terms) -> Result<(Unit, Params), String> {
        let (unit, answer) = Builtins.build(kind, params, terms)?;
        let (mut twin, _) = Builtins.build(kind, params, terms)?;
        let heard = render_note(twin.as_mut());
        self.builds.lock().unwrap().push((kind, params.clone(), terms.rate, answer.clone(), heard));
        Ok((unit, answer))
    }
}

/// A `UnitCx` for a block from `frame`, at 120 bpm in 4/4.
fn unit_cx(frame: usize) -> UnitCx {
    UnitCx { frame: frame as Frame, dsp_frame: frame as Frame, bpm: 120.0, meter: lf_engine::grid::Meter::FOUR_FOUR }
}

/// What a SYNTH renders of one A4 played at its first frame, 12 000 frames in blocks of 256, left channel.
fn render_note(unit: &mut dyn UnitProcessor) -> Vec<f32> {
    let mut heard = Vec::with_capacity(12_000);
    let note = [SlotEvent { offset: 0, kind: lf_engine::SlotEventKind::NoteOn { key: 69, velocity: 0.8 } }];
    let silence = [0.0f32; 256];
    for frame in (0..12_000).step_by(256) {
        let n = 256.min(12_000 - frame);
        let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
        let events: &[SlotEvent] = if frame == 0 { &note } else { &[] };
        unit.process(&unit_cx(frame), [&silence[..n], &silence[..n]], events, [&mut l, &mut r]);
        heard.extend(l);
    }
    heard
}

/// How long [`render_effect`] renders, in frames at 48 kHz: 1.2 s, past ECHO's second repeat at 1/4 and
/// 120 bpm (the first at 0.5 s, the feedback's at 1 s).
const EFFECT_FRAMES: usize = 57_600;

/// What an effect renders of a 10 ms burst at its first frame, [`EFFECT_FRAMES`] in blocks of 256: the
/// left channel's frames, then the right's.
fn render_effect(unit: &mut dyn UnitProcessor) -> Vec<f32> {
    let (mut left, mut right) = (Vec::with_capacity(EFFECT_FRAMES * 2), Vec::with_capacity(EFFECT_FRAMES));
    for frame in (0..EFFECT_FRAMES).step_by(256) {
        let n = 256.min(EFFECT_FRAMES - frame);
        let input: Vec<f32> = (frame..frame + n).map(|f| if f < 480 { 0.5 * (f as f32 * 0.07).sin() } else { 0.0 }).collect();
        let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
        unit.process(&unit_cx(frame), [&input, &input], &[], [&mut l, &mut r]);
        left.extend(l);
        right.extend(r);
    }
    left.extend(right);
    left
}

fn terms_at(rate: u32) -> Terms {
    Terms { generation: 1, rate, max_block: 256 }
}

/// (g) The production factory (`Chains::new`) seats ECHO, REVERB, RING MOD and OTT through `add`, each
/// built at the engine's rate with its asked params applied in order as the engine would: clamped (ECHO
/// level 2 → 1, REVERB level −1 → 0, RING freq 5000 → 1500), refused and so at its default (ECHO time
/// NaN, an ECHO level asked of RING MOD, which answers no ECHO param), and every param it was not asked
/// at its default. Plate names, roles and latencies are the units' own; the engine's chain holds the four
/// kinds in order.
#[test]
fn g_production_factory_seats_each_effect_with_its_params_applied_and_answered() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();
    let send = |p: InputSendParam, v: f64| (UnitParam::Send(p), v);
    let def = |p: InputSendParam| p.range().2;

    let echo = vec![send(InputSendParam::EchoLevel, 2.0), send(InputSendParam::EchoFeedback, 0.3), send(InputSendParam::EchoTime, f64::NAN)];
    let reverb = vec![send(InputSendParam::ReverbLevel, -1.0)];
    let ring = vec![send(InputSendParam::RingFreq, 5000.0), send(InputSendParam::EchoLevel, 0.9)];
    let ott = vec![send(InputSendParam::OttDepth, 0.25)];
    for (k, (kind, params)) in [(UnitKind::Echo, echo), (UnitKind::Reverb, reverb), (UnitKind::Ring, ring), (UnitKind::Ott, ott)].into_iter().enumerate() {
        let id = 90 + k as UnitId;
        assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, id, spec_with(kind, params), k), Ok(Added::Seated), "{kind:?}");
    }
    assert_eq!(engine_kinds(&device, 0), vec![(90, UnitKind::Echo), (91, UnitKind::Reverb), (92, UnitKind::Ring), (93, UnitKind::Ott)]);

    let expect = |id: UnitId, want: &[(InputSendParam, f64)]| {
        let want: BTreeMap<usize, f64> = want.iter().map(|&(p, v)| (send_param(p), v)).collect();
        assert_eq!(plate_params(host, id), want, "unit {id}'s params");
    };
    expect(90, &[(InputSendParam::EchoTime, def(InputSendParam::EchoTime)), (InputSendParam::EchoFeedback, 0.3), (InputSendParam::EchoLevel, 1.0)]);
    expect(91, &[(InputSendParam::ReverbLevel, 0.0)]);
    expect(92, &[(InputSendParam::RingFreq, 1500.0), (InputSendParam::RingLevel, def(InputSendParam::RingLevel))]);
    expect(
        93,
        &[
            (InputSendParam::OttDepth, 0.25),
            (InputSendParam::OttTime, def(InputSendParam::OttTime)),
            (InputSendParam::OttUpward, def(InputSendParam::OttUpward)),
            (InputSendParam::OttDownward, def(InputSendParam::OttDownward)),
        ],
    );
    for (id, name) in [(90, "ECHO"), (91, "REVERB"), (92, "RING MOD"), (93, "OTT")] {
        let plate = plate_state(host, id);
        assert_eq!((plate.name.as_str(), plate.source, plate.latency, plate.state), (name, false, 0, PlateState::Seated));
    }

    // The boxes themselves: built at the asked rate, holding what they answered.
    let (ott_box, _) = Builtins.build(UnitKind::Ott, &vec![send(InputSendParam::OttDepth, 0.25), send(InputSendParam::OttUpward, 7.0)], terms_at(44_100)).unwrap();
    let ott_box = ott_box.into_any().downcast::<OttUnit>().expect("an OttUnit");
    assert_eq!(ott_box.controls(), [0.25, def(InputSendParam::OttTime), 1.0, def(InputSendParam::OttDownward)]);
    for kind in [UnitKind::Echo, UnitKind::Reverb, UnitKind::Ring] {
        let (unit, _) = Builtins.build(kind, &Params::new(), terms_at(44_100)).unwrap();
        assert_eq!(unit.info().kind, kind);
    }
    assert!(Builtins.build(UnitKind::Plugin, &Params::new(), terms_at(48_000)).is_err(), "a plugin is not the factory's");

    chains.unload_all(host);
    chains.join_all();
}

/// (g1) The production factory applies what it answers: ECHO, REVERB and RING MOD built with every param
/// of theirs away from its default render bit for bit what the same unit built directly and set to those
/// params renders, and not what a build with no params (every default) renders. A factory that answered
/// the params but built a unit at its defaults, or left one of them out, fails here: the render runs past
/// ECHO's feedback repeat.
#[test]
fn g1_production_factory_builds_render_their_params_as_the_unit_set_directly() {
    use lf_engine::units::{EchoUnit, ReverbUnit, RingUnit};
    let send = |p: InputSendParam, v: f64| (UnitParam::Send(p), v);
    let cases: [(UnitKind, Params, Box<dyn UnitProcessor>); 3] = [
        (
            UnitKind::Echo,
            vec![send(InputSendParam::EchoTime, 0.0), send(InputSendParam::EchoFeedback, 0.7), send(InputSendParam::EchoLevel, 0.9)],
            Box::new(EchoUnit::new(48_000)),
        ),
        (UnitKind::Reverb, vec![send(InputSendParam::ReverbLevel, 0.9)], Box::new(ReverbUnit::new(48_000))),
        (UnitKind::Ring, vec![send(InputSendParam::RingFreq, 1000.0), send(InputSendParam::RingLevel, 0.9)], Box::new(RingUnit::new(48_000))),
    ];
    for (kind, params, mut direct) in cases {
        for &(param, value) in &params {
            if let UnitParam::Send(p) = param {
                assert_ne!(value, p.range().2, "{kind:?}: {} away from its default", param.key());
            }
            assert_eq!(direct.set_param(&unit_cx(0), param, value), Some(value), "{kind:?} takes {}", param.key());
        }
        let (mut built, answer) = Builtins.build(kind, &params, terms_at(48_000)).unwrap();
        assert_eq!(answer, params, "{kind:?}: every param of its own, asked, answered as taken");
        let (mut defaults, _) = Builtins.build(kind, &Params::new(), terms_at(48_000)).unwrap();
        let heard = render_effect(built.as_mut());
        assert!(heard.iter().any(|&x| x != 0.0), "{kind:?} renders something");
        if kind == UnitKind::Echo {
            // 0.9 to 1.1 s, left: the feedback's repeat, which only the applied feedback makes.
            assert!(heard[43_200..52_800].iter().any(|&x| x.abs() > 1e-4), "the render reaches ECHO's feedback repeat");
        }
        assert!(heard == render_effect(direct.as_mut()), "{kind:?}: the factory's box renders as the unit set directly");
        assert!(heard != render_effect(defaults.as_mut()), "{kind:?}: its params change what it renders");
    }
}

/// (g2) SYNTH through the production factory: one seated with its pick in its params (2.4 → 2, Pad, in the
/// UI's `SYNTHS` order) and its levels clamped (Lead 9 → `SYNTH_GAIN_MAX`), one with no params, which
/// gets pick 0 (Lead), every level 1. Each is a source with `instruments::LEAD` latency, and each plays
/// a note sent to its slot (an unpicked SYNTH is silent). Built directly: the box holds the answered pick,
/// and a pick out of range is refused, so it too plays Lead.
#[test]
fn g2_production_factory_seats_synth_with_its_pick_and_without_one_plays_lead() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();

    let picked = vec![(UnitParam::Instrument, 2.4), (UnitParam::Gain(Instrument::Pad), 0.7), (UnitParam::Gain(Instrument::Lead), 9.0)];
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 100, spec_with(UnitKind::Synth, picked), 0), Ok(Added::Seated));
    let bare = UnitSpec { kind: UnitKind::Synth, plugin: None, params: None, bypass: false };
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 1, 101, bare, 0), Ok(Added::Seated));
    assert_eq!(engine_kinds(&device, 0), vec![(100, UnitKind::Synth)]);
    assert_eq!(engine_kinds(&device, 1), vec![(101, UnitKind::Synth)]);

    let synth = |pick: f64, levels: &[(Instrument, f64)]| -> BTreeMap<usize, f64> {
        let mut m: BTreeMap<usize, f64> = Instrument::ALL.iter().map(|&i| (UnitParam::Gain(i).index(), 1.0)).collect();
        for &(i, v) in levels {
            m.insert(UnitParam::Gain(i).index(), v);
        }
        m.insert(UnitParam::Instrument.index(), pick);
        m
    };
    assert_eq!(plate_params(host, 100), synth(2.0, &[(Instrument::Pad, 0.7), (Instrument::Lead, SYNTH_GAIN_MAX)]));
    assert_eq!(plate_params(host, 101), synth(0.0, &[]));
    for id in [100, 101] {
        let plate = plate_state(host, id);
        assert_eq!((plate.name.as_str(), plate.source, plate.latency), ("SYNTH", true, LEAD), "unit {id}");
    }
    assert!(note_sounds(&device, 0), "the picked SYNTH plays");
    assert!(note_sounds(&device, 1), "the SYNTH added with no pick plays Lead");

    let picked_box = |params: Params| {
        let (unit, answer) = Builtins.build(UnitKind::Synth, &params, terms_at(48_000)).unwrap();
        (unit.into_any().downcast::<SynthUnit>().expect("a SynthUnit").instrument(), answer)
    };
    let (pick, _) = picked_box(vec![(UnitParam::Instrument, 2.4)]);
    assert_eq!(pick, Some(Instrument::Pad), "index 2 is Pad in the UI's order, not the enum's");
    let (pick, answer) = picked_box(Params::new());
    assert_eq!(pick, Some(Instrument::Lead));
    assert!(answer.contains(&(UnitParam::Instrument, 0.0)));
    let (pick, answer) = picked_box(vec![(UnitParam::Instrument, 5.6)]);
    assert_eq!(pick, Some(Instrument::Lead), "a pick out of range is refused, as the engine refuses it");
    assert!(answer.contains(&(UnitParam::Instrument, 0.0)));

    chains.unload_all(host);
    chains.join_all();
}

/// (g3) An engine at a new rate builds a seated SYNTH again at that rate from its applied params: the
/// pick and the levels it was added with and a level set live after on the instrument it plays (Piano),
/// answered by the engine. The plate reads the same params after and the SYNTH plays at the new rate: a
/// twin of the rebuilt box renders a note bit for bit as a SYNTH built directly at 44.1 kHz and set to
/// those params, and neither as one at 48 kHz, nor as one on pick 0 (Lead), nor as one without the live
/// level (a rebuild that dropped it fails). A rebuild at the same rate keeps the box (no build).
#[test]
fn g3_rebuild_at_a_new_rate_builds_synth_again_with_its_applied_pick_and_levels() {
    let device = start_device();
    let host = device.host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let factory = Arc::new(RecordingBuiltins { builds: Mutex::new(Vec::new()) });
    let chains = Chains::with_factory(factory.clone());

    // Pick 3 is Piano (`SYNTH_IDS`): the live level is the played instrument's, so the render hears it.
    let asked = vec![(UnitParam::Instrument, 3.0), (UnitParam::Gain(Instrument::Organ), 0.6)];
    assert_eq!(add_builtin(&chains, host, (&doc_state, epoch), &slots, 0, 110, spec_with(UnitKind::Synth, asked), 0), Ok(Added::Seated));
    host.send(TimedCommand { frame: None, command: Command::SetUnitParam(110, UnitParam::Gain(Instrument::Piano), 0.4) }).unwrap();
    let piano = UnitParam::Gain(Instrument::Piano).index();
    wait_until(PATIENCE, "the live level answered into the model", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        plate_params(host, 110).get(&piano) == Some(&0.4)
    });
    let applied = plate_params(host, 110);
    assert_eq!(applied[&UnitParam::Instrument.index()], 3.0);
    assert_eq!(applied[&UnitParam::Gain(Instrument::Organ).index()], 0.6);
    assert_eq!(factory.builds.lock().unwrap().len(), 1);

    let reseated = |what: &str| {
        wait_until(PATIENCE, what, || {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            let st = host.core.state.lock().unwrap();
            let seated = st.model.find_plate(110).is_some_and(|(_, p)| p.state == PlateState::Seated && p.seated_in_engine);
            drop(st);
            seated && engine_kinds(&device, 0) == vec![(110, UnitKind::Synth)]
        });
    };
    device.rebuild_at(44_100, 256);
    reseated("SYNTH reseated at 44.1 kHz");
    {
        let builds = factory.builds.lock().unwrap();
        assert_eq!(builds.len(), 2, "a new rate builds the box again");
        let (kind, params, rate, answer, heard) = &builds[1];
        assert_eq!((*kind, *rate), (UnitKind::Synth, 44_100));
        let asked: BTreeMap<usize, f64> = params.iter().map(|&(p, v)| (p.index(), v)).collect();
        assert_eq!(asked, applied, "built from the params as applied (the live level included)");
        let answered: BTreeMap<usize, f64> = answer.iter().map(|&(p, v)| (p.index(), v)).collect();
        assert_eq!(answered, applied, "the rebuilt box took the same pick and levels");
        // A SYNTH built directly at `rate`, set to the applied params in `UnitParam::ALL`'s order, but for
        // the ones `unlike` changes (pick 0, Lead, in the applied pick's place; the asked level, 1, in the
        // live one's).
        let direct = |rate: u32, unlike: &[(UnitParam, f64)]| {
            let mut unit = SynthUnit::new(rate);
            for param in UnitParam::ALL {
                if let Some(&value) = applied.get(&param.index()) {
                    let value = unlike.iter().find(|(p, _)| *p == param).map_or(value, |&(_, v)| v);
                    assert!(unit.set_param(&unit_cx(0), param, value).is_some(), "{}", param.key());
                }
            }
            render_note(&mut unit)
        };
        assert!(heard.iter().any(|&x| x != 0.0), "the rebuilt box sounds");
        assert!(*heard == direct(44_100, &[]), "the rebuilt box renders as a SYNTH at 44.1 kHz with the applied pick and levels");
        assert!(*heard != direct(48_000, &[]), "not as one at the old rate");
        assert!(*heard != direct(44_100, &[(UnitParam::Instrument, 0.0)]), "not as one with pick 0 (Lead)");
        assert!(*heard != direct(44_100, &[(UnitParam::Gain(Instrument::Piano), 1.0)]), "not as one without the live level");
    }
    assert_eq!(plate_params(host, 110), applied, "the plate reads the same params");
    assert!(note_sounds(&device, 0), "the rebuilt SYNTH plays at the new rate");

    device.rebuild_at(44_100, 512);
    reseated("SYNTH reseated at the same rate");
    assert_eq!(factory.builds.lock().unwrap().len(), 2, "the same rate resubmits the same box");

    chains.unload_all(host);
    chains.join_all();
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

/// (i) A rebuild (`TestDevice::rebuild_at`): each built-in comes back seated with its bypass and level.
/// At the same rate the owner resubmits the same box; at another rate it builds a new one from the
/// model's applied params (a built-in's DSP is built for one rate) and drops the old.
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

    let reseated = |what: &str| {
        wait_until(Duration::from_secs(3), what, || {
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
    };

    // The same rate: the same two boxes, evicted once each, keep processing in the new engine.
    device.rebuild_at(48_000, 512);
    reseated("both units reseated after a rebuild at the same rate");
    assert!(device.wait_blocks(8));
    assert_eq!(tracker.builds.load(Ordering::SeqCst), 2, "the same rate resubmits the same boxes");
    assert_eq!(tracker.drops.load(Ordering::SeqCst), 0, "original boxes survive a same-rate rebuild");
    assert_eq!(tracker.stops(1), 1, "original box 1 stopped once during eviction");
    assert_eq!(tracker.stops(2), 1, "original box 2 stopped once during eviction");
    assert!(tracker.calls(1) > calls_before_1, "original box 1 keeps processing in the rebuilt engine");
    assert!(tracker.calls(2) > calls_before_2, "original box 2 (bypassed, warm) keeps processing in the rebuilt engine");

    // Another rate: each box built again (instances 3 and 4, in either order), the old ones stopped once
    // more and dropped.
    device.rebuild_at(96_000, 512);
    reseated("both units reseated after a rebuild at another rate");
    assert!(device.wait_blocks(8));
    assert_eq!(tracker.builds.load(Ordering::SeqCst), 4, "another rate builds both boxes again");
    assert_eq!(tracker.drops.load(Ordering::SeqCst), 2, "the old boxes are dropped");
    assert_eq!(tracker.stops(1), 2);
    assert_eq!(tracker.stops(2), 2);
    assert!(tracker.calls(3) > 0 && tracker.calls(4) > 0, "both rebuilt boxes (one bypassed, warm) process");

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
    let (rem_tx, rem_rx) = std::sync::mpsc::sync_channel(1);
    let rem_thread = std::thread::spawn(move || {
        let res = rem_chains.remove(&rem_host, (&rem_doc, epoch), 90);
        let _ = rem_tx.try_send(res);
    });

    wait_until(Duration::from_secs(3), "removal held before shutdown", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 90).is_some() && st.model.slot_held(0)
    });

    let sd_chains = chains.clone();
    let sd_host = host.clone();
    let (sd_tx, sd_rx) = std::sync::mpsc::sync_channel(1);
    let sd_thread = std::thread::spawn(move || {
        let t0 = Instant::now();
        sd_chains.unload_all(&sd_host);
        sd_host.shutdown();
        sd_chains.join_all();
        let _ = sd_tx.try_send(t0.elapsed());
    });
    let elapsed = sd_rx
        .recv_timeout(PATIENCE)
        .expect("shutdown hung");
    sd_thread.join().unwrap();
    // Room for a loaded run (CI, a full suite); the shutdown bound itself is 8 s.
    assert!(elapsed < Duration::from_millis(6500), "shutdown took {elapsed:?} (must be < 6.5 s)");

    // Background held removal unblocked on closing and finished too.
    let _ = rem_rx
        .recv_timeout(PATIENCE)
        .expect("background held remove hung after shutdown");
    rem_thread.join().unwrap();
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

pub(crate) mod test_log {
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Mutex, Once};
    use std::thread::ThreadId;

    struct SharedTestLogger {
        watchers: Mutex<Vec<(u64, ThreadId, Sender<(log::Level, String)>)>>,
        next_id: std::sync::atomic::AtomicU64,
    }

    impl log::Log for SharedTestLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let me = std::thread::current().id();
            let msg = format!("{}", record.args());
            let level = record.level();
            let watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
            for (_, tid, tx) in watchers.iter() {
                if *tid == me {
                    let _ = tx.send((level, msg.clone()));
                }
            }
        }

        fn flush(&self) {}
    }

    static LOGGER: SharedTestLogger = SharedTestLogger {
        watchers: Mutex::new(Vec::new()),
        next_id: std::sync::atomic::AtomicU64::new(1),
    };
    static INSTALL: Once = Once::new();

    pub(crate) fn ensure_installed() {
        INSTALL.call_once(|| {
            log::set_logger(&LOGGER).expect("shared test logger must install cleanly");
            log::set_max_level(log::LevelFilter::Trace);
        });
    }

    pub(crate) struct ThreadLogGuard {
        id: u64,
        rx: Receiver<(log::Level, String)>,
    }

    impl ThreadLogGuard {
        pub(crate) fn drain_all(&self) -> Vec<(log::Level, String)> {
            let mut out = Vec::new();
            while let Ok(item) = self.rx.try_recv() {
                out.push(item);
            }
            out
        }

        pub(crate) fn warns(&self) -> Vec<String> {
            self.drain_all()
                .into_iter()
                .filter_map(|(lvl, msg)| (lvl == log::Level::Warn).then_some(msg))
                .collect()
        }
    }

    impl Drop for ThreadLogGuard {
        fn drop(&mut self) {
            let mut watchers = LOGGER.watchers.lock().unwrap_or_else(|e| e.into_inner());
            watchers.retain(|(id, _, _)| *id != self.id);
        }
    }

    pub(crate) fn watch_thread(tid: ThreadId) -> ThreadLogGuard {
        ensure_installed();
        let id = LOGGER.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        LOGGER
            .watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, tid, tx));
        ThreadLogGuard { id, rx }
    }

    pub(crate) fn watch_current_thread() -> ThreadLogGuard {
        watch_thread(std::thread::current().id())
    }
}

/// R2 (a): The affinity table: two instances of one path, the second refused; `unordered` refuses both;
/// `orphaned` refuses both; last release deletes the entry and the next claim owns afresh; a `leak`ed
/// holder keeps it even when release is attempted on it.
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

    // A leaked holder keeps the entry even when release is tried on both holders (release of the
    // leaked holder is refused/ignored; replacing `leak` with a no-op fails this assertion).
    let l1 = aff.claim(path);
    aff.landed(l1, path, 60);
    let l2 = aff.claim(path);
    aff.leak(l2, path);
    aff.release(l1, path);
    aff.release(l2, path);
    assert!(aff.is_held(path), "leaked holder must refuse release and keep the entry held");
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

    // Slot 0 chain order: amp (105), delay (102), amp (101) — numeric order differs from chain order.
    // Slot 1: amp (201, hinted).
    let u105 = seat_stand_in_plugin(host, 0, 105, &amp, 0);
    let u102 = seat_stand_in_plugin(host, 0, 102, &delay, 1);
    let u101 = seat_stand_in_plugin(host, 0, 101, &amp, 2);
    let u201 = seat_stand_in_plugin(host, 1, 201, &amp, 0);

    let bytes = make_tone_bytes(&amp, "Amp One", b"amp-state-v2");
    let imported = chains
        .tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(201))
        .unwrap();
    let tok = imported.token.expect("matches -> parked token");
    assert_eq!(imported.name, "Amp One");
    assert_eq!(imported.matches, vec![201, 105, 101]);
    // Nothing written to slot file when units matched.
    assert_eq!(store.load(0, &amp).unwrap(), None);

    // Wrong-identity lookup returns None AND must NOT consume the token: the matching identity still
    // redeems `tok` once.
    assert!(chains.take_parked_tone(tok, &delay).is_none());
    let taken = chains
        .take_parked_tone(tok, &amp)
        .expect("wrong-identity lookup must leave token redeemable for matching identity");
    assert_eq!(taken.state, b"amp-state-v2");
    assert!(chains.take_parked_tone(tok, &amp).is_none());

    // Re-import and verify `forget` drops the parked token.
    let imported2 = chains
        .tone_import(host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(201))
        .unwrap();
    let tok2 = imported2.token.expect("second parked token");
    assert_eq!(chains.tone_forget((&doc_state, epoch), tok2), Ok(()));
    assert!(chains.take_parked_tone(tok2, &amp).is_none());

    host.shutdown();
    u105.release();
    u102.release();
    u101.release();
    u201.release();
    let _ = std::fs::remove_dir_all(&dir);
}

/// R2 (f): Reload `moved` (untold, `since` older than `told`, another plugin, plate not seated), and
/// reload `failed` when `reload_unit` refuses after `rebuild_at` (not yet resubmitted), with reason logged.
#[test]
fn r2_f_reload_moved_and_failed_with_warn_log() {
    let log_guard = test_log::watch_current_thread();
    // Self-check: Error records are never accepted as Warn.
    log::error!("r2f error control record");
    assert!(
        log_guard.warns().is_empty(),
        "warn capture must not accept Error records as Warn"
    );

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
    let _ = log_guard.drain_all();
    assert_eq!(
        chains.reload(host, (&doc_state, epoch), Some(&store), 200, amp.clone(), 5, 1, noop_sink(), &no_open),
        Ok(Reloaded::Failed)
    );
    let logs = log_guard.warns();
    assert!(
        logs.iter().any(|line| line.contains("200") && line.contains("not seated")),
        "expected reload failure reason logged strictly at Warn, got: {logs:?}"
    );

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

    let err_params = chains.list_params(host, (&doc_state, old_epoch), 1).unwrap_err();
    assert!(err_params.contains("stale frontend epoch"), "{err_params}");

    let err_take = chains.tone_take(host, (&doc_state, old_epoch), 1).unwrap_err();
    assert!(err_take.contains("stale frontend epoch"), "{err_take}");
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
    assert!(chains.list_params(host, (&doc_state, epoch), 1).is_err(), "chain_list_params must refuse legacy plate");
    assert!(chains.set_param(host, (&doc_state, epoch), 1, 0, 0.5).is_err(), "chain_set_param must refuse legacy plate");
    assert!(chains.tone_take(host, (&doc_state, epoch), 1).is_err(), "chain_tone_take must refuse legacy plate");

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

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    {
        *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || {
            let _ = entered_tx.try_send(());
            let _ = resume_rx.recv_timeout(PATIENCE);
        }));
    }

    let add_chains = chains.clone();
    let add_host = host.clone();
    let add_doc = doc_state.clone();
    let add_slots = slots.clone();
    let (add_tx, add_rx) = std::sync::mpsc::sync_channel(1);
    let add_thread = std::thread::spawn(move || {
        let res = add_builtin(&add_chains, &add_host, (&add_doc, epoch), &add_slots, 0, 202, echo_spec(0.5, false), 0);
        let _ = add_tx.try_send(res);
    });

    // Wait until `add_builtin` reaches the point right before registry insertion, then run `unload_all`
    // while the device is still playing (BEFORE `host.shutdown()`).
    entered_rx
        .recv_timeout(PATIENCE)
        .expect("add_builtin never reached before_register_hook");
    chains.unload_all(&host);
    let _ = resume_tx.try_send(());

    let res = add_rx
        .recv_timeout(PATIENCE)
        .expect("add_builtin did not finish within 5 s");
    add_thread.join().unwrap();
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
        in_build_tx: std::sync::mpsc::SyncSender<()>,
        release_build_rx: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
    }
    impl BuiltinFactory for PausingFactory {
        fn build(&self, kind: UnitKind, params: &Params, _terms: Terms) -> Result<(Unit, Params), String> {
            if kind == UnitKind::Reverb {
                return Err("fail slot 0 plate".into());
            }
            if self.pause_slot1.swap(false, Ordering::AcqRel) {
                let _ = self.in_build_tx.try_send(());
                let rx = self.release_build_rx.lock().unwrap_or_else(|e| e.into_inner());
                let _ = rx.recv_timeout(PATIENCE);
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
    let (in_build_tx, in_build_rx) = std::sync::mpsc::sync_channel::<()>(2);
    let (release_build_tx, release_build_rx) = std::sync::mpsc::sync_channel::<()>(2);
    let pause_slot1 = Arc::new(AtomicBool::new(true));
    let chains = Arc::new(Chains::with_factory(Arc::new(PausingFactory {
        pause_slot1: pause_slot1.clone(),
        in_build_tx,
        release_build_rx: Arc::new(Mutex::new(release_build_rx)),
    })));

    // 1. Create a failed-add plate for id 400 in slot 0 (id is freed).
    let bad = UnitSpec { kind: UnitKind::Reverb, plugin: None, params: None, bypass: false };
    assert!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 400, bad, 0).is_err());

    // 2. Arm `after_slot_lookup_hook`: pause `remove(400)` right after it resolves slot 0, then
    //    report the second lookup (slot 1) once it re-checks under `order[0]` and loops.
    let (looked_tx, looked_rx) = std::sync::mpsc::sync_channel::<usize>(4);
    let (resume_lookup_tx, resume_lookup_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_lookup_rx = Arc::new(Mutex::new(Some(resume_lookup_rx)));
    *chains.after_slot_lookup_hook.lock().unwrap() = Some(Arc::new(move |slot| {
        let _ = looked_tx.try_send(slot);
        if let Some(rx) = resume_lookup_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = rx.recv_timeout(PATIENCE);
        }
    }));

    let rem_chains = chains.clone();
    let rem_host = host.clone();
    let rem_doc = doc_state.clone();
    let rem_done = Arc::new(AtomicBool::new(false));
    let rem_done_flag = rem_done.clone();
    let (rem_tx, rem_rx) = std::sync::mpsc::sync_channel(1);
    let rem_thread = std::thread::spawn(move || {
        let r = rem_chains.remove(&rem_host, (&rem_doc, epoch), 400);
        rem_done_flag.store(true, Ordering::Release);
        let _ = rem_tx.try_send(r);
    });
    assert_eq!(
        looked_rx.recv_timeout(PATIENCE),
        Ok(0),
        "remove(400) did not resolve slot 0 before pausing"
    );

    // 3. While `rem_thread` is paused right after resolving slot 0, start an add for id 400 in
    //    SLOT 1 that pauses in flight (holding `order[1]` with plate 400 now in slot 1).
    let add_chains = chains.clone();
    let add_host = host.clone();
    let add_doc = doc_state.clone();
    let add_slots = slots.clone();
    let (add_tx, add_rx) = std::sync::mpsc::sync_channel(1);
    let add_thread = std::thread::spawn(move || {
        let r = add_builtin(&add_chains, &add_host, (&add_doc, epoch), &add_slots, 1, 400, echo_spec(0.5, false), 0);
        let _ = add_tx.try_send(r);
    });
    in_build_rx
        .recv_timeout(PATIENCE)
        .expect("slot 1 add(400) never reached build pause");

    // 4. Resume `rem_thread` while slot 1's add is STILL IN FLIGHT. Under `order[0]`, it must see
    //    plate 400 moved to slot 1, loop to look up slot 1, and block on `order[1]`.
    let _ = resume_lookup_tx.try_send(());
    let retried_slot = looked_rx.recv_timeout(PATIENCE);

    let rem_ran_early = rem_done.load(Ordering::Acquire);
    let slot1_still_has_400 = {
        let st = host.core.state.lock().unwrap();
        st.model.find_plate(400).is_some_and(|(s, _)| s == 1)
    };

    // Release slot 1's build so neither thread hangs regardless of assertion outcome.
    let _ = release_build_tx.try_send(());
    let add_res = add_rx.recv_timeout(PATIENCE).expect("slot 1 add(400) hung");
    let rem_res = rem_rx.recv_timeout(PATIENCE).expect("remove(400) hung");
    add_thread.join().unwrap();
    rem_thread.join().unwrap();

    assert_eq!(retried_slot, Ok(1), "remove must re-resolve to slot 1 instead of mutating under order[0]");
    assert!(!rem_ran_early, "remove must not finish while slot 1's add of reused id 400 is still in flight");
    assert!(slot1_still_has_400, "slot 1's in-flight plate 400 must not be removed under slot 0's lock");
    assert_eq!(add_res, Ok(Added::Seated), "slot 1's add must seat cleanly before remove acts");
    assert_eq!(rem_res, Ok(()), "remove must then proceed under slot 1's order");

    // Repeat for `move_unit`: failed plate 401 in slot 0, reused in slot 1 (alongside unit 402).
    *chains.after_slot_lookup_hook.lock().unwrap() = None;
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 1, 402, echo_spec(0.4, false), 0), Ok(Added::Seated));
    let bad2 = UnitSpec { kind: UnitKind::Reverb, plugin: None, params: None, bypass: false };
    assert!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 401, bad2, 0).is_err());

    let (looked2_tx, looked2_rx) = std::sync::mpsc::sync_channel::<usize>(4);
    let (resume2_tx, resume2_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume2_rx = Arc::new(Mutex::new(Some(resume2_rx)));
    *chains.after_slot_lookup_hook.lock().unwrap() = Some(Arc::new(move |slot| {
        let _ = looked2_tx.try_send(slot);
        if let Some(rx) = resume2_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = rx.recv_timeout(PATIENCE);
        }
    }));

    pause_slot1.store(true, Ordering::Release);
    let mv_chains = chains.clone();
    let mv_host = host.clone();
    let mv_doc = doc_state.clone();
    let mv_done = Arc::new(AtomicBool::new(false));
    let mv_done_flag = mv_done.clone();
    let (mv_tx, mv_rx) = std::sync::mpsc::sync_channel(1);
    let mv_thread = std::thread::spawn(move || {
        let r = mv_chains.move_unit(&mv_host, (&mv_doc, epoch), 401, 0);
        mv_done_flag.store(true, Ordering::Release);
        let _ = mv_tx.try_send(r);
    });
    assert_eq!(
        looked2_rx.recv_timeout(PATIENCE),
        Ok(0),
        "move_unit(401) did not resolve slot 0 before pausing"
    );

    let add2_chains = chains.clone();
    let add2_host = host.clone();
    let add2_doc = doc_state.clone();
    let add2_slots = slots.clone();
    let (add2_tx, add2_rx) = std::sync::mpsc::sync_channel(1);
    let add2_thread = std::thread::spawn(move || {
        let r = add_builtin(&add2_chains, &add2_host, (&add2_doc, epoch), &add2_slots, 1, 401, echo_spec(0.5, false), 1);
        let _ = add2_tx.try_send(r);
    });
    in_build_rx
        .recv_timeout(PATIENCE)
        .expect("slot 1 add(401) never reached build pause");

    let _ = resume2_tx.try_send(());
    let retried_mv_slot = looked2_rx.recv_timeout(PATIENCE);
    let mv_ran_early = mv_done.load(Ordering::Acquire);
    let _ = release_build_tx.try_send(());

    let add2_res = add2_rx.recv_timeout(PATIENCE).expect("slot 1 add(401) hung");
    let mv_res = mv_rx.recv_timeout(PATIENCE).expect("move_unit(401) hung");
    add2_thread.join().unwrap();
    mv_thread.join().unwrap();
    *chains.after_slot_lookup_hook.lock().unwrap() = None;

    assert_eq!(retried_mv_slot, Ok(1), "move_unit must re-resolve to slot 1 instead of mutating under order[0]");
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

// ── P4 fix round 2 tests: shutdown, re-add, reload outcome, epochs ──

/// P4 fix r2 item 1a: `join_all` stops and waits for every plugin owner thread to exit even while an
/// in-flight command (`tone_take`) holds an `Arc<Entry>` clone, and no command thread ends up doing
/// an unbounded join after shutdown.
#[test]
fn p4f2_1a_join_all_waits_for_plugin_owner_even_with_held_arc() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let host = start_fake_driver_host();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("p4f2-1a");

    let (in_state_tx, in_state_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_state_tx, resume_state_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_state_rx = Arc::new(Mutex::new(Some(resume_state_rx)));
    let owner_exited = Arc::new(AtomicBool::new(false));
    let paused_once = Arc::new(AtomicBool::new(false));

    let p_once = paused_once.clone();
    let exited = owner_exited.clone();
    let fake_open = move |_fmt: PluginFormat,
                          _plugin: &PluginRef,
                          unit_host: UnitHost,
                          sink: EventSink,
                          tone: Option<ToneBinding>| {
        let in_tx = in_state_tx.clone();
        let res_rx = resume_state_rx.clone();
        let p_once = p_once.clone();
        spawn_fake_unit(
            unit_host,
            sink,
            tone,
            FakePluginConfig {
                name: "SlowSave".into(),
                on_state: Some(Arc::new(move || {
                    if !p_once.swap(true, Ordering::AcqRel) {
                        let _ = in_tx.try_send(());
                        if let Some(rx) = res_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                            let _ = rx.recv_timeout(Duration::from_secs(10));
                        }
                    }
                })),
                exited_flag: Some(exited.clone()),
                ..Default::default()
            },
        )
    };

    let pref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\SlowSave.clap".into(),
        id: "slow.save".into(),
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(pref), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 701, spec, 0, None, noop_sink(), &fake_open),
        Ok(Added::Seated)
    );

    // Start an in-flight `tone_take` that clones `Arc<Entry>` and blocks inside the plugin's `state()`.
    let take_chains = chains.clone();
    let take_host = host.clone();
    let take_doc = doc_state.clone();
    let (take_done_tx, take_done_rx) = std::sync::mpsc::sync_channel(1);
    let take_thread = std::thread::spawn(move || {
        let res = take_chains.tone_take(&take_host, (&take_doc, epoch), 701);
        let _ = take_done_tx.try_send(res);
    });
    in_state_rx
        .recv_timeout(PATIENCE)
        .expect("tone_take never entered plugin state()");

    // Shutdown: unload, close device, then start `join_all` while `tone_take` still holds `Arc<Entry>`.
    chains.unload_all(&host);
    host.shutdown();
    let (join_done_tx, join_done_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let jc = chains.clone();
    let join_thread = std::thread::spawn(move || {
        jc.join_all();
        let _ = join_done_tx.try_send(());
    });

    let returned_early = join_done_rx.recv_timeout(Duration::from_millis(80)).is_ok();
    let still_alive_while_blocked = !owner_exited.load(Ordering::Acquire);

    // Release the slow save so both threads finish regardless of assertion result.
    let _ = resume_state_tx.try_send(());
    if !returned_early {
        join_done_rx
            .recv_timeout(PATIENCE)
            .expect("join_all did not finish after unblocking plugin state()");
    }
    join_thread.join().unwrap();
    let exited_when_join_returned = owner_exited.load(Ordering::Acquire);
    let _ = take_done_rx
        .recv_timeout(PATIENCE)
        .expect("tone_take did not finish after unblocking plugin state()");
    take_thread.join().unwrap();

    assert!(still_alive_while_blocked, "owner should still be blocked in SaveTone");
    assert!(
        !returned_early,
        "join_all must NOT return while a plugin owner thread is still running (even with a held Arc<Entry>)"
    );
    assert!(exited_when_join_returned, "plugin owner thread must have exited before join_all returned");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P4 fix r2 item 1b (Sol finding 1): a reload candidate that seats just before `closing` is set
/// (before `unload_all` removes the unit) must be handed to `gone` instead of dropped early on the
/// command thread, so shutdown tears it down cleanly without abandoning/leaking the plugin.
#[test]
fn p4f2_1b_seated_reload_racing_closing_does_not_leak_plugin() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let host = start_fake_driver_host();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("p4f2-1b");

    let released = Arc::new(AtomicUsize::new(0));
    let amp = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\ReloadClose.clap".into(),
        id: "reload.close".into(),
    };

    let rel1 = released.clone();
    let open1 = move |_fmt: PluginFormat,
                      _plugin: &PluginRef,
                      unit_host: UnitHost,
                      sink: EventSink,
                      tone: Option<ToneBinding>| {
        spawn_fake_unit(
            unit_host,
            sink,
            tone,
            FakePluginConfig {
                name: "ReloadClose".into(),
                released_count: Some(rel1.clone()),
                ..Default::default()
            },
        )
    };

    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 702, spec, 0, None, noop_sink(), &open1),
        Ok(Added::Seated)
    );
    let rev = host.core.state.lock().unwrap().model.snapshot().rev;
    host.chain_told(rev, 1);

    let bytes = make_tone_bytes(&amp, "ReloadClose", b"tone-v2");
    let tok = chains
        .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(702))
        .unwrap()
        .token
        .expect("parked tone token");

    let seated_barrier = Arc::new(Barrier::new(2));
    let resume_barrier = Arc::new(Barrier::new(2));
    let sb = seated_barrier.clone();
    let rb = resume_barrier.clone();
    let rel2 = released.clone();
    let reload_chains = chains.clone();
    let reload_host = host.clone();
    let reload_doc = doc_state.clone();
    let reload_store = store.clone();
    let reload_amp = amp.clone();

    let reload_thread = std::thread::spawn(move || {
        let open2 = move |_fmt: PluginFormat,
                          _plugin: &PluginRef,
                          unit_host: UnitHost,
                          sink: EventSink,
                          tone: Option<ToneBinding>| {
            let res = spawn_fake_unit(
                unit_host,
                sink,
                tone,
                FakePluginConfig {
                    name: "ReloadClose".into(),
                    released_count: Some(rel2.clone()),
                    ..Default::default()
                },
            );
            // Candidate is now seated in the engine; pause right before `reload` locks `registry`.
            sb.wait();
            rb.wait();
            res
        };
        reload_chains.reload(
            &reload_host,
            (&reload_doc, epoch),
            Some(&reload_store),
            702,
            reload_amp,
            1,
            tok,
            noop_sink(),
            &open2,
        )
    });

    // Candidate is seated in the engine; flip `closing` BEFORE `remove_unit` has run.
    seated_barrier.wait();
    host.set_closing();
    chains.closing.store(true, Ordering::Release);
    resume_barrier.wait();

    let res = reload_thread.join().unwrap();
    assert!(res.is_err(), "reload racing closing must return Err, got {res:?}");

    // Complete shutdown: remove units while device plays, close device, join all owners.
    chains.unload_all(&host);
    host.shutdown();
    chains.join_all();

    assert_eq!(chains.active_entries(), 0, "no entries left after join_all");
    assert_eq!(
        released.load(Ordering::SeqCst),
        2,
        "both original and seated reload candidate must take ordered_release (neither abandoned/leaked)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// P4 fix r2 item 2: re-adding a failed plugin id whose first owner is still blocked in foreign
/// teardown never drops or joins the displaced entry under the registry mutex, and releases the old
/// affinity claim once the old owner exits (before shutdown).
#[test]
fn p4f2_2_readd_failed_plugin_id_never_blocks_registry_and_releases_affinity() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    // 20 ms per 256-frame block keeps the recording take open cleanly without hitting max_loop_seconds.
    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = Arc::new(empty_slots());
    let chains = Arc::new(Chains::new());

    let src_ref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\HeadSrc.clap".into(),
        id: "head.src".into(),
    };
    let p1 = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\FailFirst.clap".into(),
        id: "fail.first".into(),
    };
    let p2 = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\ReaddSecond.clap".into(),
        id: "readd.second".into(),
    };

    // Open a take FIRST, then queue a source (710) whose role change (false -> true) holds slot 0's
    // port head. Effect 711 queued at index 0 behind it answers `Waiting`, then is refused
    // (`Placement`, ahead of source 710) once the take ends.
    start_take(&device);

    let open_src = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "HeadSrc".into(), source: true, ..Default::default() })
    };
    let spec_src = UnitSpec { kind: UnitKind::Plugin, plugin: Some(src_ref), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, None, 0, 710, spec_src, 0, None, noop_sink(), &open_src),
        Ok(Added::Waiting)
    );

    let (in_release_tx, in_release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_release_tx, resume_release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_release_rx = Arc::new(Mutex::new(Some(resume_release_rx)));
    let owner1_exited = Arc::new(AtomicBool::new(false));
    let oe = owner1_exited.clone();
    let open_p1 = move |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let in_tx = in_release_tx.clone();
        let res_rx = resume_release_rx.clone();
        spawn_fake_unit(
            uh,
            sink,
            tone,
            FakePluginConfig {
                name: "FailFirst".into(),
                source: false,
                latency: 0,
                on_release: Some(Arc::new(move || {
                    let _ = in_tx.try_send(());
                    if let Some(rx) = res_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = rx.recv_timeout(Duration::from_secs(10));
                    }
                })),
                exited_flag: Some(oe.clone()),
                ..Default::default()
            },
        )
    };
    let spec_p1 = UnitSpec { kind: UnitKind::Plugin, plugin: Some(p1.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, None, 0, 711, spec_p1, 0, None, noop_sink(), &open_p1),
        Ok(Added::Waiting)
    );
    assert!(chains.affinity_held(&p1.path));

    // End the take: source 710 seats at index 0, then engine refuses effect 711 (`Placement`).
    // Owner1 calls `host.failed` (freeing id 711) and pauses inside `plugin.release()`.
    stop_take(&device);
    in_release_rx
        .recv_timeout(PATIENCE)
        .expect("owner1 did not enter plugin.release() after post-take refusal");
    wait_until(Duration::from_secs(3), "slot 0 unheld after take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        !host.core.state.lock().unwrap().model.slot_held(0)
    });

    // Re-add id 711 (at valid index 1 behind source 710) on a spawned thread with a strict bound:
    // if re-add ever drops/joins displaced owner1 under `registry`, recv_timeout fails promptly.
    let readd_chains = chains.clone();
    let readd_host = host.clone();
    let readd_doc = doc_state.clone();
    let readd_slots = slots.clone();
    let (readd_tx, readd_rx) = std::sync::mpsc::sync_channel(1);
    let readd_thread = std::thread::spawn(move || {
        let open_p2 = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "ReaddSecond".into(), source: false, ..Default::default() })
        };
        let spec_p2 = UnitSpec { kind: UnitKind::Plugin, plugin: Some(p2), params: None, bypass: false };
        let res = readd_chains.add(&readd_host, (&readd_doc, epoch), &readd_slots, None, 0, 711, spec_p2, 1, None, noop_sink(), &open_p2);
        let _ = readd_tx.try_send(res);
    });

    let readd_res = readd_rx.recv_timeout(Duration::from_secs(3));
    let reg_unlocked = chains.registry.try_lock().is_ok();
    let held_while_running = chains.affinity_held(&p1.path);

    // Always unblock owner1 before asserting so even a failure never hangs on drop.
    let _ = resume_release_tx.try_send(());
    let readd_res = readd_res.expect("re-add blocked waiting on displaced owner1 (must never drop/join under registry)");
    readd_thread.join().unwrap();

    assert_eq!(readd_res, Ok(Added::Seated));
    assert!(reg_unlocked, "registry mutex must not be held");
    assert!(held_while_running, "p1 affinity stays held while owner1 is still running");

    // Once owner1's OS thread has actually finished (not just set its pre-return flag), sweep
    // BEFORE shutdown and verify p1's affinity releases.
    wait_until(Duration::from_secs(3), "owner1 thread finishes", || {
        owner1_exited.load(Ordering::Acquire)
            && chains.gone.lock().unwrap().entries.iter().all(|e| e.owner.is_finished())
    });
    assert_eq!(chains.close_editor(&host, (&doc_state, epoch), 711), Ok(()));
    assert!(
        !chains.affinity_held(&p1.path),
        "displaced owner1's affinity must be released once its owner thread exits (not only at shutdown)"
    );

    chains.unload_all(&host);
    chains.join_all();
}

/// P4 fix r2 item 4: a waiting reload cancelled by an engine rebuild answers `Failed` (and logs the
/// cause at `Warn`) even if a later reload of the same unit seats before the first waiter polls.
#[test]
fn p4f2_4_cancelled_waiting_reload_answers_failed_even_if_later_reload_seats_and_logs_cause() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("p4f2-4");

    let amp = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\ReloadRace.clap".into(),
        id: "reload.race".into(),
    };
    let open_lat0 = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "ReloadRace".into(), latency: 0, ..Default::default() })
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 720, spec, 0, None, noop_sink(), &open_lat0),
        Ok(Added::Seated)
    );
    let rev = host.core.state.lock().unwrap().model.snapshot().rev;
    host.chain_told(rev, 5);

    let bytes = make_tone_bytes(&amp, "ReloadRace", b"tone-race");
    let tok1 = chains
        .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(720))
        .unwrap()
        .token
        .expect("tok1");
    let tok2 = chains
        .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(720))
        .unwrap()
        .token
        .expect("tok2");

    // Open a take so reload 1 (with changed latency 64) enters `Answer::Waiting` and polls.
    start_take(&device);
    let (in_poll_tx, in_poll_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_poll_tx, resume_poll_rx) = std::sync::mpsc::sync_channel::<()>(1);
    {
        *chains.before_reload_poll_hook.lock().unwrap() = Some(Box::new(move || {
            let _ = in_poll_tx.try_send(());
            let _ = resume_poll_rx.recv_timeout(Duration::from_secs(10));
        }));
    }

    let r1_chains = chains.clone();
    let r1_host = host.clone();
    let r1_doc = doc_state.clone();
    let r1_store = store.clone();
    let r1_amp = amp.clone();
    let r1_thread = std::thread::spawn(move || {
        let log_guard = test_log::watch_current_thread();
        let open_lat64 = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "ReloadRace".into(), latency: 64, ..Default::default() })
        };
        let res = r1_chains.reload(
            &r1_host,
            (&r1_doc, epoch),
            Some(&r1_store),
            720,
            r1_amp,
            5,
            tok1,
            noop_sink(),
            &open_lat64,
        );
        (res, log_guard.warns())
    });

    // Wait until reload 1 is parked right before its first `reload_outcome` poll.
    in_poll_rx
        .recv_timeout(PATIENCE)
        .expect("reload 1 never reached its poll hook");

    // Rebuild cancels reload 1 in flight and evicts the original unit, which auto-reseats in the new engine.
    let gen_before = host.core.engine_gen.load(Ordering::Acquire);
    device.rebuild_at(96_000, 512);
    wait_until(PATIENCE, "original 720 reseated after rebuild", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let st = host.core.state.lock().unwrap();
        let new_gen = host.core.engine_gen.load(Ordering::Acquire);
        new_gen > gen_before
            && st.model.find_plate(720).is_some_and(|(_, p)| p.seated_in_engine && !p.reloading)
            && !st.table.has_reload_row(720)
    });

    // A later reload (reload 2) seats BEFORE reload 1 polls again.
    assert_eq!(
        chains.reload(&host, (&doc_state, epoch), Some(&store), 720, amp.clone(), 5, tok2, noop_sink(), &open_lat0),
        Ok(Reloaded::Reloaded)
    );
    let r2_token = host.core.state.lock().unwrap().table.active_row_for_id(720).expect("r2 row").token;

    // Now resume reload 1's poll: it MUST answer `Failed` (never `Reloaded`) and log the cause at Warn.
    let _ = resume_poll_tx.try_send(());
    let (r1_res, r1_warns) = r1_thread.join().unwrap();

    assert_eq!(
        r1_res,
        Ok(Reloaded::Failed),
        "cancelled reload 1 must report Failed even after reload 2 seated"
    );
    assert!(
        r1_warns.iter().any(|w| w.contains("720") && w.contains("rebuild")),
        "failed waiting reload must log its cause at Warn, got: {r1_warns:?}"
    );
    assert_eq!(
        chains.registry.lock().unwrap().get(&720).expect("r2 registered").token,
        r2_token,
        "reload 2's owner must stay registered"
    );

    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// P4 fix r3 item A: `join_all` must never miss an owner moving out of `registry` into `gone`
/// (in `retire_to_gone`) or moving from `pending_reloads` into `registry`/`gone` (in promotion).
#[test]
fn p4f3_a_shutdown_never_misses_owner_moving_between_tables() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let host = start_fake_driver_host();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());

    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let owner_exited = Arc::new(AtomicBool::new(false));
    let exited_flag = owner_exited.clone();

    let open = move |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let release_rx = release_rx.clone();
        spawn_fake_unit(
            uh,
            sink,
            tone,
            FakePluginConfig {
                name: "GapOwner".into(),
                on_release: Some(Arc::new(move || {
                    if let Some(rx) = release_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let _ = rx.recv_timeout(PATIENCE);
                    }
                })),
                exited_flag: Some(exited_flag.clone()),
                ..Default::default()
            },
        )
    };
    let pref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\GapOwner.clap".into(),
        id: "gap.owner".into(),
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(pref), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, None, 0, 730, spec, 0, None, noop_sink(), &open),
        Ok(Added::Seated)
    );

    // Pause `retire_to_gone` right after removing from `registry` (and hold the pause briefly so
    // `join_all` races against the move).
    let (in_gap_tx, in_gap_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_gap_tx, resume_gap_rx) = std::sync::mpsc::sync_channel::<()>(1);
    *chains.after_retire_remove_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = in_gap_tx.try_send(());
        let _ = resume_gap_rx.recv_timeout(Duration::from_millis(120));
    }));

    let retire_chains = chains.clone();
    let (retire_done_tx, retire_done_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let retire_thread = std::thread::spawn(move || {
        retire_chains.retire_to_gone(730, None);
        let _ = retire_done_tx.try_send(());
    });

    in_gap_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("retire_to_gone never reached hook");

    // Shut down engine so owner can receive its evicted box and block in `on_release`.
    host.shutdown();

    let join_chains = chains.clone();
    let (join_done_tx, join_done_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let join_thread = std::thread::spawn(move || {
        join_chains.join_all();
        let _ = join_done_tx.try_send(());
    });

    // While `on_release` is blocked, `join_all` MUST NOT return early.
    let returned_early = join_done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
    let alive_when_checked = !owner_exited.load(Ordering::Acquire);

    // Always unblock both threads before asserting so the test never hangs.
    let _ = resume_gap_tx.try_send(());
    let _ = release_tx.try_send(());
    retire_done_rx
        .recv_timeout(PATIENCE)
        .expect("retire_to_gone did not finish within 5 s");
    if !returned_early {
        join_done_rx
            .recv_timeout(PATIENCE)
            .expect("join_all did not finish within 5 s");
    }
    let _ = retire_thread.join();
    let _ = join_thread.join();

    assert!(alive_when_checked, "owner should still have been blocked in on_release");
    assert!(
        !returned_early,
        "join_all must wait for an owner moving from registry to gone (never miss it in a lock gap)"
    );
    assert!(owner_exited.load(Ordering::Acquire), "owner must have exited before join_all returned");
}

/// P4 fix r3 item B: a waiting reload answers `Reloaded` only once its candidate's owner is in
/// `registry`, so an immediate `tone_take` after `Reloaded` reaches the new owner even if a
/// concurrent sweep took the candidate out of `pending_reloads` first.
#[test]
fn p4f3_b_reload_answers_reloaded_only_after_new_owner_is_in_registry() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("p4f3-b");

    let amp = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\ReloadPromote.clap".into(),
        id: "reload.promote".into(),
    };
    let open_old = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(
            uh,
            sink,
            tone,
            FakePluginConfig {
                name: "ReloadPromote".into(),
                state_bytes: b"old-tone".to_vec(),
                ..Default::default()
            },
        )
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 735, spec, 0, None, noop_sink(), &open_old),
        Ok(Added::Seated)
    );
    let rev = host.core.state.lock().unwrap().model.snapshot().rev;
    host.chain_told(rev, 1);

    let bytes = make_tone_bytes(&amp, "ReloadPromote", b"new-tone");
    let tok = chains
        .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(735))
        .unwrap()
        .token
        .expect("parked tone token");

    // Hold slot 0 with a take so reload enters `pending_reloads` and polls.
    start_take(&device);
    let (in_poll_tx, in_poll_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_poll_tx, resume_poll_rx) = std::sync::mpsc::sync_channel::<()>(1);
    *chains.before_reload_poll_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = in_poll_tx.try_send(());
        let _ = resume_poll_rx.recv_timeout(PATIENCE);
    }));

    let r_chains = chains.clone();
    let r_host = host.clone();
    let r_doc = doc_state.clone();
    let r_store = store.clone();
    let r_amp = amp.clone();
    let (reload_done_tx, reload_done_rx) = std::sync::mpsc::sync_channel(1);
    let reload_thread = std::thread::spawn(move || {
        let open_new = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(
                uh,
                sink,
                tone,
                FakePluginConfig {
                    name: "ReloadPromote".into(),
                    latency: 64,
                    state_bytes: b"new-tone".to_vec(),
                    ..Default::default()
                },
            )
        };
        let res = r_chains.reload(
            &r_host,
            (&r_doc, epoch),
            Some(&r_store),
            735,
            r_amp,
            1,
            tok,
            noop_sink(),
            &open_new,
        );
        let _ = reload_done_tx.try_send(res);
    });

    in_poll_rx
        .recv_timeout(PATIENCE)
        .expect("reload never reached before_reload_poll_hook");

    // End take so candidate seats in engine while reload waiter is paused at poll hook.
    stop_take(&device);
    wait_until(PATIENCE, "reload candidate seated in table", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let st = host.core.state.lock().unwrap();
        !st.table.has_reload_row(735) && st.model.find_plate(735).is_some_and(|(_, p)| p.seated_in_engine)
    });

    // Trigger a concurrent sweep that takes the candidate out of `pending_reloads` and pauses 150 ms
    // before finishing promotion.
    let (sweep_took_tx, sweep_took_rx) = std::sync::mpsc::sync_channel::<()>(1);
    *chains.after_take_pending_reload_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = sweep_took_tx.try_send(());
        std::thread::sleep(Duration::from_millis(150));
    }));

    let s_chains = chains.clone();
    let s_host = host.clone();
    let s_doc = doc_state.clone();
    let (sweep_done_tx, sweep_done_rx) = std::sync::mpsc::sync_channel(1);
    let sweep_thread = std::thread::spawn(move || {
        let r = s_chains.close_editor(&s_host, (&s_doc, epoch), 735);
        let _ = sweep_done_tx.try_send(r);
    });

    sweep_took_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("sweep never entered promote_pending_reload");

    // Release reload's waiter while sweep is paused inside promotion.
    let _ = resume_poll_tx.try_send(());
    let reload_res = reload_done_rx
        .recv_timeout(PATIENCE)
        .expect("reload did not finish within 5 s");

    // Immediately call `tone_take` as soon as `reload` returns `Reloaded`!
    let taken_res = chains.tone_take(&host, (&doc_state, epoch), 735);

    let _ = sweep_done_rx.recv_timeout(PATIENCE);
    let _ = reload_thread.join();
    let _ = sweep_thread.join();

    assert_eq!(reload_res, Ok(Reloaded::Reloaded));
    let taken_bytes = taken_res.expect("immediate tone_take after Reloaded must find the new owner");
    let decoded = crate::host::tone::decode(&taken_bytes).expect("valid tone payload");
    assert_eq!(
        decoded.state, b"new-tone",
        "immediate tone_take after Reloaded must reach the new candidate owner, not the predecessor"
    );

    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// P4 fix r3 item C: a late add whose `Waiting` install was refused and freed its id must never
/// overwrite a newer registration of that id in the other slot; it retires its late owner and
/// answers its refusal.
#[test]
fn p4f3_c_late_refused_add_never_overwrites_newer_registration_in_other_slot() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let doc_state = Arc::new(PluginHostState::default());
    let epoch = doc_state.begin_frontend_session();
    let slots = Arc::new(empty_slots());
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("p4f3-c");

    let src_ref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\LateAddSrc.clap".into(),
        id: "late.add.src".into(),
    };
    let p_slot0 = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\LateSlot0.clap".into(),
        id: "late.slot0".into(),
    };
    let p_slot1 = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\LiveSlot1.clap".into(),
        id: "live.slot1".into(),
    };

    // Hold slot 0 with a take + pending source (740) so effect 741 at index 0 answers `Waiting`
    // and then gets refused (`Placement`) once the take ends.
    start_take(&device);
    let open_src = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "LateAddSrc".into(), source: true, ..Default::default() })
    };
    let spec_src = UnitSpec { kind: UnitKind::Plugin, plugin: Some(src_ref), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 740, spec_src, 0, None, noop_sink(), &open_src),
        Ok(Added::Waiting)
    );

    let (in_reg_tx, in_reg_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (resume_reg_tx, resume_reg_rx) = std::sync::mpsc::sync_channel::<()>(1);
    *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = in_reg_tx.try_send(());
        let _ = resume_reg_rx.recv_timeout(PATIENCE);
    }));

    let c0 = chains.clone();
    let h0 = host.clone();
    let d0 = doc_state.clone();
    let s0 = slots.clone();
    let st0 = store.clone();
    let p0 = p_slot0.clone();
    let (add0_tx, add0_rx) = std::sync::mpsc::sync_channel(1);
    let add0_thread = std::thread::spawn(move || {
        let open0 = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(
                uh,
                sink,
                tone,
                FakePluginConfig {
                    name: "LateSlot0".into(),
                    source: false,
                    state_bytes: b"dead-slot0".to_vec(),
                    ..Default::default()
                },
            )
        };
        let spec0 = UnitSpec { kind: UnitKind::Plugin, plugin: Some(p0), params: None, bypass: false };
        let res = c0.add(&h0, (&d0, epoch), &s0, Some(&st0), 0, 741, spec0, 0, None, noop_sink(), &open0);
        let _ = add0_tx.try_send(res);
    });

    in_reg_rx
        .recv_timeout(PATIENCE)
        .expect("slot 0 add never reached before_register_hook");

    // End the take: slot 0's 741 is refused (`Placement`) and its owner frees id 741 and exits.
    stop_take(&device);
    wait_until(PATIENCE, "slot 0 refused 741 and freed its id", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let st = host.core.state.lock().unwrap();
        st.table.active_row_for_id(741).is_none()
    });

    // Slot 1 re-adds id 741 and seats cleanly.
    let open1 = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(
            uh,
            sink,
            tone,
            FakePluginConfig {
                name: "LiveSlot1".into(),
                source: false,
                state_bytes: b"live-slot1".to_vec(),
                ..Default::default()
            },
        )
    };
    let spec1 = UnitSpec { kind: UnitKind::Plugin, plugin: Some(p_slot1.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 1, 741, spec1, 0, None, noop_sink(), &open1),
        Ok(Added::Seated)
    );

    // Resume slot 0's late add.
    let _ = resume_reg_tx.try_send(());
    let add0_res = add0_rx
        .recv_timeout(PATIENCE)
        .expect("slot 0 add did not finish within 5 s");
    let _ = add0_thread.join();

    assert!(add0_res.is_err(), "late refused add in slot 0 must answer Err, got: {add0_res:?}");

    let taken = chains
        .tone_take(&host, (&doc_state, epoch), 741)
        .expect("slot 1's live registration for id 741 must stay intact");
    let decoded = crate::host::tone::decode(&taken).expect("valid tone payload");
    assert_eq!(decoded.state, b"live-slot1", "registry entry for 741 must still be slot 1's owner");
    assert_eq!(engine_chain_summary(&device, 1), vec![(741, false)]);

    // Once slot 0's refused owner finishes and is swept, slot 0's affinity releases while slot 1's stays.
    wait_until(Duration::from_secs(3), "late slot 0 owner finishes", || {
        chains.gone.lock().unwrap().entries.iter().all(|e| e.owner.is_finished())
    });
    assert_eq!(chains.close_editor(&host, (&doc_state, epoch), 741), Ok(()));
    assert!(!chains.affinity_held(&p_slot0.path), "retired late owner's affinity must release");
    assert!(chains.affinity_held(&p_slot1.path), "live slot 1 owner's affinity must stay held");

    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// P4 fix r3 item D: if spawning `lf-chain-late-drop` fails, `dispose_late_entry` must leak the
/// entry, mark its affinity leaked, and log an error — never dropping/joining on the caller thread.
#[test]
fn p4f3_d_dispose_late_entry_spawn_failure_leaks_without_joining_caller() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let host = start_fake_driver_host();
    let chains = Arc::new(Chains::new());
    let pref = PluginRef {
        format: "clap".into(),
        path: "C:\\plugins\\LateDropFail.clap".into(),
        id: "late.drop.fail".into(),
    };
    let unit_host = host
        .unit(0, 745, Spec::Plugin(pref.clone()), 0, false)
        .expect("reserve unit");
    let token = unit_host.install_token();

    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let owner_exited = Arc::new(AtomicBool::new(false));
    let exited_flag = owner_exited.clone();
    let (owner, answer) = spawn_fake_unit(
        unit_host,
        noop_sink(),
        None,
        FakePluginConfig {
            name: "LateDropFail".into(),
            on_release: Some(Arc::new(move || {
                if let Some(rx) = release_rx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = rx.recv_timeout(PATIENCE);
                }
            })),
            exited_flag: Some(exited_flag),
            ..Default::default()
        },
    )
    .expect("spawn fake unit");
    assert!(matches!(answer, Answer::Seated { .. }));

    // Shut down the device so the owner receives its box and blocks inside `on_release`.
    host.shutdown();

    let inst = {
        let mut aff = chains.affinity.lock().unwrap();
        let inst = aff.claim(&pref.path);
        aff.landed(inst, &pref.path, 745);
        inst
    };
    let entry = Arc::new(super::Entry {
        slot: 0,
        token,
        owner: super::Owner::Plugin(super::PluginOwner { plugin: pref.clone(), inst, owner }),
    });

    chains.fail_next_late_drop_spawn.store(true, Ordering::Release);
    let c = chains.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let caller_thread = std::thread::spawn(move || {
        let log_guard = test_log::watch_current_thread();
        c.dispose_late_entry(entry);
        let errors: Vec<String> = log_guard
            .drain_all()
            .into_iter()
            .filter_map(|(lvl, msg)| (lvl == log::Level::Error).then_some(msg))
            .collect();
        let _ = done_tx.try_send(errors);
    });

    // Must return promptly even while the owner thread is blocked inside `on_release`.
    let errors_res = done_rx.recv_timeout(Duration::from_millis(500));

    // Unblock the owner thread so it exits cleanly regardless of assertion outcome.
    let _ = release_tx.try_send(());
    let errors = errors_res.expect("dispose_late_entry must never join on the caller thread when spawn fails");
    let _ = caller_thread.join();

    assert!(
        !errors.is_empty(),
        "dispose_late_entry spawn failure must log at error level"
    );
    wait_until(Duration::from_secs(3), "leaked owner thread eventually exits", || {
        owner_exited.load(Ordering::Acquire)
    });
    chains.sweep(None);
    assert!(
        chains.affinity_held(&pref.path),
        "leaked entry's affinity must remain leaked even after sweep"
    );
    // Attempting a direct release on the leaked instance must stay a no-op.
    chains.affinity.lock().unwrap().release(inst, &pref.path);
    assert!(chains.affinity_held(&pref.path), "leaked affinity instance must never release");
}

/// P4 fix r3 item F (built-in ECHO): shutdown (`unload_all` + `join_all`) while a removed ECHO is
/// still finishing its fade-out must collect and drop its box rather than abandoning/leaking it.
#[test]
fn p4f3_f_builtin_shutdown_does_not_abandon_removed_echo() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(10), |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let tracker = BoxTracker::new();
    let chains = Chains::with_factory(Arc::new(
        ClampingBuiltinFactory::new().with_tracker(tracker.clone()),
    ));

    assert_eq!(
        add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 750, echo_spec(0.4, false), 0),
        Ok(Added::Seated)
    );

    chains.unload_all(&host);
    chains.join_all();

    assert_eq!(
        tracker.drops.load(Ordering::SeqCst),
        1,
        "removed built-in ECHO box must be collected and dropped during shutdown, never abandoned/leaked"
    );
}

/// E3f (`docs/unit-contract.md` § 1, § 6): the host's shutdown (`unload_all`) removes every unit but keeps
/// a plugin unit's own tone, `unit-<id>.tone`, where it is, so the next launch's restore under that id
/// finds it, and leaves the slot's file of that plugin as it was: two instances of one plugin in one slot
/// each keep their own. A player's removal before shutdown still retires: its unit file becomes the
/// slot's file.
#[test]
fn e3f_shutdown_keeps_a_plugin_units_tone_and_a_players_removal_retires_it() {
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let host = start_fake_driver_host();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("e3f-shutdown");
    std::fs::create_dir_all(&dir).unwrap();

    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\KeepAmp.clap".into(), id: "keep.amp".into() };
    let cab = PluginRef { format: "clap".into(), path: "C:\\plugins\\RetireCab.clap".into(), id: "retire.cab".into() };
    // Each plugin's slot file as an earlier unit (or v0.8.9) left it.
    let amp_slot = dir.join(amp.file_name(0));
    let cab_slot = dir.join(cab.file_name(0));
    let amp_before = make_tone_bytes(&amp, "KeepAmp", b"amp-slot-tone");
    std::fs::write(&amp_slot, &amp_before).unwrap();
    std::fs::write(&cab_slot, make_tone_bytes(&cab, "RetireCab", b"cab-slot-tone")).unwrap();

    let open = |_fmt: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let state_bytes = format!("{}-{}-dialed-in", plugin.id, unit_host.id()).into_bytes();
        spawn_fake_unit(unit_host, sink, tone, FakePluginConfig { name: plugin.id.clone(), state_bytes, ..Default::default() })
    };
    for (id, plugin, at) in [(801, &cab, 0), (802, &amp, 1), (803, &amp, 2)] {
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(plugin.clone()), params: None, bypass: false };
        assert_eq!(chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, id, spec, at, None, noop_sink(), &open), Ok(Added::Seated));
        // The unit's first save lands in its own file; the slot's file stays as it was.
        assert!(chains.tone_take(&host, (&doc_state, epoch), id).is_ok());
        assert!(dir.join(format!("unit-{id}.tone")).exists(), "unit {id} saved its own tone");
    }
    assert_eq!(std::fs::read(&amp_slot).unwrap(), amp_before, "a unit's save leaves the slot file alone");

    // The player removes the cab: its tone retires into the slot's file.
    assert_eq!(chains.remove(&host, (&doc_state, epoch), 801), Ok(()));
    wait_until(PATIENCE, "the removed unit's tone retires", || !dir.join("unit-801.tone").exists());
    let retired = decode_tone(&std::fs::read(&cab_slot).unwrap()).unwrap();
    assert_eq!(retired.state, b"retire.cab-801-dialed-in", "a player's removal retires the unit's tone into the slot file");

    // The shutdown removes both amps: each one's own tone stays where it is, the slot file as it was.
    chains.unload_all(&host);
    host.shutdown();
    chains.join_all();
    assert_eq!(chains.active_entries(), 0, "every owner has ended");
    for id in [802, 803] {
        let kept = std::fs::read(dir.join(format!("unit-{id}.tone"))).unwrap_or_else(|e| panic!("the shutdown keeps unit-{id}.tone: {e}"));
        assert_eq!(decode_tone(&kept).unwrap().state, format!("keep.amp-{id}-dialed-in").into_bytes());
    }
    assert_eq!(std::fs::read(&amp_slot).unwrap(), amp_before, "the shutdown leaves the slot file unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── E3f CP1 review: the shutdown's keep is bound to the removal that ends the unit ──

/// A one-shot pause in a fake plugin's hook (`FakePluginConfig::on_state` / `on_reactivate`): armed, its
/// next call reports in and waits, bounded, until the test resumes it. Holds a unit's owner thread
/// where the test needs it.
struct HookPause {
    armed: AtomicBool,
    entered_tx: std::sync::mpsc::SyncSender<()>,
    entered_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    resume_tx: std::sync::mpsc::SyncSender<()>,
    resume_rx: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl HookPause {
    fn new() -> Arc<HookPause> {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        Arc::new(HookPause {
            armed: AtomicBool::new(false),
            entered_tx,
            entered_rx: Mutex::new(entered_rx),
            resume_tx,
            resume_rx: Mutex::new(resume_rx),
        })
    }

    fn hook(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let me = self.clone();
        Arc::new(move || {
            if me.armed.swap(false, Ordering::AcqRel) {
                let _ = me.entered_tx.try_send(());
                let _ = me.resume_rx.lock().unwrap().recv_timeout(PATIENCE);
            }
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Whether the hook was entered (since the last look), without waiting.
    fn entered(&self) -> bool {
        self.entered_rx.lock().unwrap().try_recv().is_ok()
    }

    fn wait_entered(&self, what: &str) {
        self.entered_rx.lock().unwrap().recv_timeout(PATIENCE).unwrap_or_else(|_| panic!("{what}"));
    }

    fn resume(&self) {
        let _ = self.resume_tx.try_send(());
    }
}

/// A source stand-in seated beside the chain host's own units (a newer source that replaces the head).
struct StandInSource;

impl UnitProcessor for StandInSource {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: true, latency: 0 }
    }
    fn process(&mut self, _cx: &UnitCx, _input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        for o in out {
            o.fill(0.0);
        }
    }
    fn stop(&mut self) {}
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// One plugin unit in slot 0 of a test device's chain host, with a tone store: the unit saved its own
/// `unit-<id>.tone` (`<plugin id>-<id>-dialed-in`) and the slot's file holds an older tone.
struct KeepRig {
    device: TestDevice,
    doc: Arc<PluginHostState>,
    epoch: u32,
    chains: Arc<Chains>,
    store: ToneStore,
    dir: std::path::PathBuf,
    plugin: PluginRef,
    id: UnitId,
    slot_before: Vec<u8>,
}

impl KeepRig {
    fn new(tag: &str, id: UnitId, cfg: crate::host::unit_owner::test_support::FakePluginConfig) -> KeepRig {
        use crate::host::unit_owner::test_support::spawn_fake_unit;

        let device = start_device();
        let doc = Arc::new(PluginHostState::default());
        let epoch = doc.begin_frontend_session();
        let chains = Arc::new(Chains::new());
        let (store, dir) = temp_tone_store(tag);
        std::fs::create_dir_all(&dir).unwrap();
        let plugin = PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{tag}.clap"), id: format!("{tag}.plugin") };
        let slot_before = make_tone_bytes(&plugin, tag, b"an-older-slot-tone");
        std::fs::write(dir.join(plugin.file_name(0)), &slot_before).unwrap();
        let state_bytes = format!("{}-{id}-dialed-in", plugin.id).into_bytes();
        let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(unit_host, sink, tone, crate::host::unit_owner::test_support::FakePluginConfig { state_bytes: state_bytes.clone(), ..cfg.clone() })
        };
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(plugin.clone()), params: None, bypass: false };
        let slots = empty_slots();
        assert_eq!(chains.add(device.host(), (&doc, epoch), &slots, Some(&store), 0, id, spec, 0, None, noop_sink(), &open), Ok(Added::Seated));
        assert!(chains.tone_take(device.host(), (&doc, epoch), id).is_ok());
        assert!(dir.join(format!("unit-{id}.tone")).exists(), "unit {id} saved its own tone");
        KeepRig { device, doc, epoch, chains, store, dir, plugin, id, slot_before }
    }

    fn host(&self) -> &EngineHost {
        self.device.host()
    }

    fn unit_file(&self) -> std::path::PathBuf {
        self.dir.join(format!("unit-{}.tone", self.id))
    }

    fn own_state(&self) -> Vec<u8> {
        format!("{}-{}-dialed-in", self.plugin.id, self.id).into_bytes()
    }

    /// A save request to the unit's owner on a thread of its own (its `on_state` hook may hold it).
    fn take_tone_on_a_thread(&self) -> std::thread::JoinHandle<()> {
        let (chains, host, doc, epoch, id) = (self.chains.clone(), self.host().clone(), self.doc.clone(), self.epoch, self.id);
        std::thread::spawn(move || {
            let _ = chains.tone_take(&host, (&doc, epoch), id);
        })
    }

    /// The tone a reload of the unit redeems (`start_reload`).
    fn reloaded_state(&self) -> Vec<u8> {
        format!("{}-{}-reloaded", self.plugin.id, self.id).into_bytes()
    }

    /// A same-id reload of the unit, as `Chains::reload` makes one: its tone redeemed into
    /// `unit-<id>.tone` (`reloaded_state`), a candidate row under the id, and its owner (a fake whose load
    /// restores the redeemed tone) on a thread of its own, returned once the candidate's install is
    /// queued. The caller holds the engine, so that install stays queued. The thread answers when the
    /// owner does (an error once the candidate leaves before seating, after its release).
    fn start_reload(
        &self,
        cfg: crate::host::unit_owner::test_support::FakePluginConfig,
    ) -> (lf_engine::InstallToken, std::thread::JoinHandle<Result<(UnitOwner, Answer), String>>) {
        use crate::host::tone::ToneKey;
        use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

        let key = ToneKey::Unit { unit: self.id, slot: 0, identity: self.plugin.clone() };
        let tone = Tone { identity: self.plugin.clone(), name: "Reloaded".into(), state: self.reloaded_state() };
        let imported = self.store.redeem(&key, tone).expect("redeem the reload's tone");
        let candidate = self.host().reload_unit(self.id, self.plugin.clone()).expect("a reload candidate");
        let token = candidate.install_token();
        let binding = ToneBinding { store: self.store.clone(), key, imported: Some(imported) };
        let cfg = FakePluginConfig { restores: Some(Arc::new(Mutex::new(Vec::new()))), ..cfg };
        let owner = std::thread::spawn(move || spawn_fake_unit(candidate, noop_sink(), Some(binding), cfg));
        wait_until(PATIENCE, "the reload's install is queued", || {
            self.host().core.state.lock().unwrap().model.has_pending_install(0, self.id)
        });
        (token, owner)
    }

    /// The reload's candidate ended by the shutdown's removal kept its tone: `unit-<id>.tone` holds the
    /// redeemed tone, and the slot's file is as it was.
    fn assert_reload_kept(&self, what: &str) {
        use crate::host::tone::decode as decode_tone;
        let kept = std::fs::read(self.unit_file()).unwrap_or_else(|e| panic!("{what}: unit-{}.tone is kept: {e}", self.id));
        assert_eq!(decode_tone(&kept).unwrap().state, self.reloaded_state(), "{what}: the reload's tone");
        assert_eq!(std::fs::read(self.dir.join(self.plugin.file_name(0))).unwrap(), self.slot_before, "{what}: the slot file unchanged");
    }

    /// The reload's candidate retired: `unit-<id>.tone` is gone, the slot's file holds the redeemed tone.
    fn assert_reload_retired(&self, what: &str) {
        use crate::host::tone::decode as decode_tone;
        assert!(!self.unit_file().exists(), "{what}: unit-{}.tone retired", self.id);
        let slot = std::fs::read(self.dir.join(self.plugin.file_name(0))).unwrap();
        assert_eq!(decode_tone(&slot).unwrap().state, self.reloaded_state(), "{what}: the slot file holds the reload's tone");
    }

    fn owner_finished(&self) -> bool {
        let reg = self.chains.registry.lock().unwrap();
        reg.get(&self.id).is_none_or(|e| e.owner.is_finished())
    }

    /// The shutdown kept the unit's own tone where it is, and the slot's file as it was.
    fn assert_kept(&self) {
        use crate::host::tone::decode as decode_tone;
        let kept = std::fs::read(self.unit_file()).unwrap_or_else(|e| panic!("the shutdown keeps unit-{}.tone: {e}", self.id));
        assert_eq!(decode_tone(&kept).unwrap().state, self.own_state(), "the unit's own tone, as it saved it");
        assert_eq!(std::fs::read(self.dir.join(self.plugin.file_name(0))).unwrap(), self.slot_before, "the slot file unchanged");
    }

    /// The unit's tone retired: its own file is gone, the slot's file holds it.
    fn wait_retired(&self, what: &str) {
        use crate::host::tone::decode as decode_tone;
        wait_until(PATIENCE, what, || !self.unit_file().exists());
        let slot = std::fs::read(self.dir.join(self.plugin.file_name(0))).unwrap();
        assert_eq!(decode_tone(&slot).unwrap().state, self.own_state(), "{what}: the slot file holds the unit's tone");
    }

    fn end(self) {
        self.chains.unload_all(self.host());
        self.chains.join_all();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The player's removal still pending (held by an open take) when the shutdown runs: the shutdown's
/// removal finds it under way, so the player's removal ends the unit and the tone retires.
#[test]
fn e3f_r1_a_players_removal_pending_at_shutdown_retires_the_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    // Latency 64: its removal is an edit, held while a take records.
    let rig = KeepRig::new("e3f-r1-a", 861, FakePluginConfig { latency: 64, ..Default::default() });
    let host = rig.host().clone();
    start_take(&rig.device);
    let rem = {
        let (chains, host, doc, epoch) = (rig.chains.clone(), host.clone(), rig.doc.clone(), rig.epoch);
        std::thread::spawn(move || chains.remove(&host, (&doc, epoch), 861))
    };
    wait_until(PATIENCE, "the player's removal held by the take", || {
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 861).is_some() && st.model.slot_held(0)
    });

    rig.chains.unload_all(&host);
    assert!(rig.unit_file().exists(), "nothing retires while the removal is held");
    stop_take(&rig.device);
    rig.wait_retired("the player's removal, pending at shutdown, retires the tone");
    let _ = rem.join();
    rig.end();
}

/// Finding 1's race: a newer source replaced the unit in the engine (its box came back as
/// `Left::Replaced`, the `Event::Seat` not yet drained, so the plate still stands) before the shutdown's
/// removal: the replacement ended the unit, so its tone retires.
#[test]
fn e3f_r1_b_a_source_replaced_before_the_shutdowns_removal_retires_the_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    let pause = HookPause::new();
    let rig = KeepRig::new("e3f-r1-b", 871, FakePluginConfig { source: true, on_state: Some(pause.hook()), ..Default::default() });
    let host = rig.host().clone();
    // Hold the unit's owner in a save, so the replacement's box waits in its mailbox.
    pause.arm();
    let save = rig.take_tone_on_a_thread();
    pause.wait_entered("the owner never entered its save");

    let other = PluginRef { format: "clap".into(), path: "C:\\plugins\\NewerSource.clap".into(), id: "newer.source".into() };
    let newer = host.unit(0, 872, Spec::Plugin(other.clone()), 0, false).expect("reserve the newer source");
    let terms = newer.terms().expect("engine terms");
    newer.activated(Activated { name: other.id.clone(), source: true, latency: 0 });
    newer.submit(Box::new(StandInSource), terms).map_err(|(_, e)| e).expect("submit the newer source");
    wait_until(PATIENCE, "the newer source replaced the unit in the engine", || {
        let rt = host.core.rt.lock().unwrap();
        rt.engine.as_ref().unwrap().chain(0).map(|s| s.unit).collect::<Vec<_>>() == vec![872]
    });
    assert!(
        host.core.state.lock().unwrap().model.find_plate(871).is_some(),
        "the replacement's Event::Seat is not drained yet: the shutdown still finds the unit's plate"
    );

    rig.chains.unload_all(&host);
    pause.resume();
    save.join().unwrap();
    rig.wait_retired("a unit a source replaced before the shutdown retires its tone");
    newer.release();
    rig.end();
}

/// A source replacement queued ahead of the shutdown's removal (the engine held still while both are
/// queued): the shutdown's removal is accepted, the replacement applies first and ends the unit, so its
/// tone retires.
#[test]
fn e3f_r1_c_a_source_replacement_queued_ahead_of_the_shutdowns_removal_retires_the_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    let rig = KeepRig::new("e3f-r1-c", 881, FakePluginConfig { source: true, ..Default::default() });
    let host = rig.host().clone();
    let other = PluginRef { format: "clap".into(), path: "C:\\plugins\\NewerSource.clap".into(), id: "newer.source".into() };
    let newer = host.unit(0, 882, Spec::Plugin(other.clone()), 0, false).expect("reserve the newer source");
    let terms = newer.terms().expect("engine terms");
    newer.activated(Activated { name: other.id.clone(), source: true, latency: 0 });

    let op = {
        // The test device's render `try_lock`s the engine: held here, it takes no message.
        let _engine = host.core.rt.lock().unwrap();
        newer.submit(Box::new(StandInSource), terms).map_err(|(_, e)| e).expect("submit the newer source");
        let Ok(Removal::Sent(op)) = host.remove_unit_at_shutdown(881) else { panic!("the shutdown's removal is sent") };
        let st = host.core.state.lock().unwrap();
        assert!(st.table.active_row_for_id(881).is_some_and(|r| r.keep_tone), "accepted, the shutdown's removal marks the keep");
        op
    };
    assert!(host.wait_op(op, PATIENCE).is_ok());
    rig.wait_retired("a replacement queued ahead of the shutdown's removal retires the tone");
    newer.release();
    rig.end();
}

/// The shutdown removes a unit whose box is out of the engine (evicted by a rebuild, its owner holding
/// it): the removal ends it at once with `Back::Dismissed`, and the unit keeps its own tone.
#[test]
fn e3f_r1_d_a_shutdown_dismissed_unit_keeps_its_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    let pause = HookPause::new();
    let rig = KeepRig::new("e3f-r1-d", 891, FakePluginConfig { on_reactivate: Some(pause.hook()), ..Default::default() });
    let host = rig.host().clone();
    pause.arm();
    rig.device.rebuild_at(44_100, 256);
    pause.wait_entered("the evicted unit's owner never re-activated");
    assert!(
        host.core.state.lock().unwrap().model.find_plate(891).is_some_and(|(_, p)| !p.seated_in_engine),
        "the plate stands unseated while its owner holds the box"
    );

    rig.chains.unload_all(&host);
    pause.resume();
    wait_until(PATIENCE, "the dismissed unit's owner ends on its own", || rig.owner_finished());
    rig.assert_kept();
    rig.end();
}

/// The shutdown's removal applies while the unit's owner is busy, and the owner is stopped before its
/// loop takes the terminal `Back::Removed`: the stopping owner's final drain takes it, and the unit
/// keeps its own tone.
#[test]
fn e3f_r1_e_a_shutdown_terminal_taken_by_the_final_drain_keeps_the_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    let pause = HookPause::new();
    let rig = KeepRig::new("e3f-r1-e", 901, FakePluginConfig { on_state: Some(pause.hook()), ..Default::default() });
    let host = rig.host().clone();
    pause.arm();
    let save = rig.take_tone_on_a_thread();
    pause.wait_entered("the owner never entered its save");

    rig.chains.unload_all(&host);
    assert!(host.core.state.lock().unwrap().model.find_plate(901).is_none(), "the shutdown's removal applied");
    let entry = rig.chains.registry.lock().unwrap().get(&901).cloned().expect("the unit's owner is registered");
    entry.owner.request_stop();
    pause.resume();
    save.join().unwrap();
    wait_until(PATIENCE, "the stopped owner ends", || entry.owner.is_finished());
    rig.assert_kept();
    drop(entry);
    rig.end();
}

// ── E3-4: a clean unit's tone is pinned; the shutdown's keep and a reload in flight ──

/// No wait drains the engine's events for itself from here on, as beside a feed that ticks but has not
/// drained yet: an `Event::Seat` stays undrained until the test drains the feed (`drain_feed`).
fn hold_self_drains(host: &EngineHost) {
    host.core.stash.lock().unwrap().mark_feed_drain(Instant::now() + Duration::from_secs(3600));
}

/// Two launches over one tone folder, each with a store of its own (`ToneStore::new`, as the app makes one
/// per launch): in the first, two units of one plugin in slot 0, `first` never saving and `second` saving
/// its own tone before the player removes it (it retires into the slot's file), then the shutdown; in the
/// next, `first` re-added under its id. Answers what each load restored (`first`, `second`, `first` again),
/// and whether `first`'s pin (`unit-<first>.pin`) stood once it seated in the first launch.
fn restores_across_two_launches(tag: &str, first: UnitId, second: UnitId, slot_tone: Option<&[u8]>) -> (Vec<Option<Vec<u8>>>, bool) {
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let (store, dir) = temp_tone_store(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{tag}.clap"), id: format!("{tag}.amp") };
    let slot_file = dir.join(amp.file_name(0));
    if let Some(tone) = slot_tone {
        std::fs::write(&slot_file, make_tone_bytes(&amp, tag, tone)).unwrap();
    }
    let restores: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::new(Mutex::new(Vec::new()));
    let open = |_fmt: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let state_bytes = format!("{}-{}-dialed-in", plugin.id, unit_host.id()).into_bytes();
        spawn_fake_unit(unit_host, sink, tone, FakePluginConfig { state_bytes, restores: Some(restores.clone()), ..Default::default() })
    };
    let spec = || UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    let slots = empty_slots();

    let pinned = {
        let device = start_device();
        let host = device.host();
        let doc = PluginHostState::default();
        let epoch = doc.begin_frontend_session();
        let chains = Chains::new();
        assert_eq!(chains.add(host, (&doc, epoch), &slots, Some(&store), 0, first, spec(), 0, None, noop_sink(), &open), Ok(Added::Seated));
        let pinned = dir.join(format!("unit-{first}.pin")).exists();
        assert_eq!(chains.add(host, (&doc, epoch), &slots, Some(&store), 0, second, spec(), 1, None, noop_sink(), &open), Ok(Added::Seated));
        assert!(chains.tone_take(host, (&doc, epoch), second).is_ok());
        assert_eq!(chains.remove(host, (&doc, epoch), second), Ok(()));
        wait_until(PATIENCE, "the removed unit's tone retires", || !dir.join(format!("unit-{second}.tone")).exists());
        let retired = decode_tone(&std::fs::read(&slot_file).unwrap()).unwrap();
        assert_eq!(retired.state, format!("{}-{second}-dialed-in", amp.id).into_bytes(), "unit {second}'s tone is the slot's now");
        chains.unload_all(host);
        chains.join_all();
        pinned
    };

    // The next launch: a store of its own over the same folder; the rig's restore re-adds `first` under its id.
    let store = ToneStore::new(dir.clone());
    let device = start_device();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let chains = Chains::new();
    assert_eq!(chains.add(device.host(), (&doc, epoch), &slots, Some(&store), 0, first, spec(), 0, None, noop_sink(), &open), Ok(Added::Seated));
    chains.unload_all(device.host());
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
    let restored = restores.lock().unwrap().clone();
    assert_eq!(restored.len(), 3, "three loads: {first}, {second}, {first} again");
    (restored, pinned)
}

/// A clean unit (one that never saved a tone of its own) pins the slot's tone it starts from at its first
/// load. Another instance of its plugin in its slot saves a new tone and the player removes it, so that
/// tone retires into the slot's file; at the next launch, the clean unit re-added under its id restores
/// the tone it played, not the retired one.
#[test]
fn e3_4_a_clean_units_tone_is_pinned_against_another_instance_retiring_into_the_slot_file() {
    let (restored, pinned) = restores_across_two_launches("e3-4-pin", 931, 932, Some(b"the-slot-tone"));
    assert_eq!(restored[0].as_deref(), Some(&b"the-slot-tone"[..]), "the clean unit started from the slot's tone");
    assert_eq!(
        restored[2].as_deref(),
        Some(&b"the-slot-tone"[..]),
        "re-added under its id, the clean unit restores the tone it played, not the one another instance retired"
    );
    assert!(pinned, "seated, the clean unit had pinned the tone it started from as unit-931.pin, not as a tone of its own");
}

/// The same with no slot file at the clean unit's first load: it started at its plugin's defaults, and
/// re-added under its id at the next launch it starts there again, not from the tone another instance
/// retired into the slot's file meanwhile.
#[test]
fn e3_4_a_clean_unit_that_started_at_the_plugins_defaults_keeps_them() {
    let (restored, pinned) = restores_across_two_launches("e3-4-pin-defaults", 936, 937, None);
    assert_eq!(restored[0], None, "the clean unit started at the plugin's defaults");
    assert_eq!(restored[2], None, "re-added under its id, the clean unit starts at its defaults again");
    assert!(pinned, "seated, the clean unit had pinned its defaults (an empty unit-936.pin)");
}

/// A unit whose add the engine refuses (its latency past the chain's bound) never seats: its load read
/// the slot's tone, and the owner, ending, leaves no file of the unit's behind (the failed plate's removal
/// has no owner to retire one).
#[test]
fn e3_4_an_add_the_engine_refuses_leaves_no_file_of_the_unit() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("e3-4-refused");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\RefusedAmp.clap".into(), id: "refused.amp".into() };
    std::fs::write(dir.join(amp.file_name(0)), make_tone_bytes(&amp, "RefusedAmp", b"the-slot-tone")).unwrap();
    let restores: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::new(Mutex::new(Vec::new()));
    let exited = Arc::new(AtomicBool::new(false));
    let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let cfg = FakePluginConfig {
            latency: lf_engine::max_chain_latency(48_000) + 1,
            restores: Some(restores.clone()),
            exited_flag: Some(exited.clone()),
            ..Default::default()
        };
        spawn_fake_unit(unit_host, sink, tone, cfg)
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    let added = chains.add(host, (&doc, epoch), &empty_slots(), Some(&store), 0, 981, spec, 0, None, noop_sink(), &open);
    assert!(added.is_err(), "the engine refuses the unit on latency: {added:?}");
    wait_until(PATIENCE, "the refused unit's owner ends", || exited.load(Ordering::Acquire));
    assert_eq!(restores.lock().unwrap().as_slice(), &[Some(b"the-slot-tone".to_vec())], "its load read the slot's tone");
    assert!(!dir.join("unit-981.tone").exists(), "the refused unit leaves no unit-981.tone");
    assert!(!dir.join("unit-981.pin").exists(), "the refused unit leaves no unit-981.pin");
    chains.unload_all(host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A unit that never changed its tone is removed by the player while a take holds that removal; its plate
/// leaving, a session import of its plugin finds no seated match and lands in the slot's file. When the
/// take ends and the removal applies, the unit's retire leaves the slot's file alone: the import stands.
#[test]
fn e3_4_an_untouched_units_removal_held_by_a_take_leaves_a_session_import_in_the_slot_file() {
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host().clone();
    let doc = Arc::new(PluginHostState::default());
    let epoch = doc.begin_frontend_session();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("e3-4-import");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\ImportAmp.clap".into(), id: "import.amp".into() };
    let slot_file = dir.join(amp.file_name(0));
    std::fs::write(&slot_file, make_tone_bytes(&amp, "ImportAmp", b"the-slot-tone")).unwrap();
    let exited = Arc::new(AtomicBool::new(false));
    // Latency 64: its removal is an edit, held while a take records. Its load reads its tone, as a real
    // plugin's does.
    let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let cfg = FakePluginConfig {
            latency: 64,
            restores: Some(Arc::new(Mutex::new(Vec::new()))),
            exited_flag: Some(exited.clone()),
            ..Default::default()
        };
        spawn_fake_unit(unit_host, sink, tone, cfg)
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    assert_eq!(chains.add(&host, (&doc, epoch), &empty_slots(), Some(&store), 0, 971, spec, 0, None, noop_sink(), &open), Ok(Added::Seated));

    start_take(&device);
    let rem = {
        let (chains, host, doc) = (chains.clone(), host.clone(), doc.clone());
        std::thread::spawn(move || chains.remove(&host, (&doc, epoch), 971))
    };
    wait_until(PATIENCE, "the player's removal held by the take", || {
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 971).is_some() && st.model.slot_held(0)
    });
    let imported = make_tone_bytes(&amp, "ImportAmp", b"the-imported-tone");
    let answer = chains.tone_import(&host, (&doc, epoch), Some(&store), 0, &imported, &amp, None).unwrap();
    assert!(answer.matches.is_empty() && answer.token.is_none(), "the leaving plate is no seated match");
    assert_eq!(std::fs::read(&slot_file).unwrap(), imported, "the import landed in the slot's file");

    stop_take(&device);
    let _ = rem.join();
    wait_until(PATIENCE, "the removed unit's owner ends", || exited.load(Ordering::Acquire));
    let slot = decode_tone(&std::fs::read(&slot_file).unwrap()).unwrap();
    assert_eq!(slot.state, b"the-imported-tone", "the untouched unit's retire leaves the session import in the slot's file");
    assert!(!dir.join("unit-971.tone").exists(), "the untouched unit leaves no unit-971.tone");
    assert!(!dir.join("unit-971.pin").exists(), "the untouched unit leaves no unit-971.pin");
    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Chains::unload_all` on a thread of its own, held inside its removal of `id` the moment that call lets
/// go of the host's locks, before any later line of it runs (`AFTER_REMOVE_UNLOCKED`). Answers when it is
/// held there and its removal stands sent (`pending_remove_op`), with the sender that lets it go on and its
/// thread.
fn unload_all_held_in_removal(rig: &KeepRig, id: UnitId) -> (std::sync::mpsc::SyncSender<()>, std::thread::JoinHandle<()>) {
    use crate::engine_io::unit_host::AFTER_REMOVE_UNLOCKED;

    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (go_tx, go_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (chains, host) = (rig.chains.clone(), rig.host().clone());
    let unloading = std::thread::spawn(move || {
        AFTER_REMOVE_UNLOCKED.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |removed| {
                if removed == id {
                    let _ = reached_tx.try_send(());
                    let _ = go_rx.recv_timeout(PATIENCE);
                }
            }));
        });
        chains.unload_all(&host);
    });
    reached_rx.recv_timeout(PATIENCE).expect("the shutdown never sent its removal");
    assert!(
        rig.host().core.state.lock().unwrap().model.pending_remove_op(0, id).is_some(),
        "the shutdown's removal of unit {id} was sent, not refused"
    );
    (go_tx, unloading)
}

/// A same-id reload queued ahead of the shutdown's removal (`Chains::unload_all`, the engine held still
/// while both are queued), its `Event::Seat` undrained when the candidate's box comes back removed: the
/// shutdown's removal is what ends the candidate, so its owner keeps `unit-<id>.tone` (the reload's tone)
/// and the slot's file stays as it was. The engine runs and the candidate's owner ends while the removal
/// call has released the host's locks and not yet returned, so the keep must be bound where it is sent.
#[test]
fn e3_4_a_reload_queued_ahead_of_the_shutdowns_removal_keeps_the_candidates_tone() {
    use crate::host::unit_owner::test_support::FakePluginConfig;

    let rig = KeepRig::new("e3-4-reload-keep", 941, FakePluginConfig::default());
    let host = rig.host().clone();
    hold_self_drains(&host);
    // The test device's render `try_lock`s the engine: held here, it takes no message.
    let engine = host.core.rt.lock().unwrap();
    let (_, owner) = rig.start_reload(FakePluginConfig::default());
    let (go, unloading) = unload_all_held_in_removal(&rig, 941);
    // The engine applies the reload, then the removal; the candidate's owner takes its box back removed.
    drop(engine);
    wait_until(PATIENCE, "the candidate's owner ends on its removed box", || owner.is_finished());
    assert!(
        host.core.state.lock().unwrap().model.has_pending_install(0, 941),
        "the reload's Event::Seat was still undrained when the candidate's owner ended"
    );
    assert!(owner.join().unwrap().is_err(), "the candidate left before its seat was told");
    rig.assert_reload_kept("a reload queued ahead of the shutdown's removal");

    host.drain_feed(&mut Vec::new());
    go.send(()).unwrap();
    unloading.join().unwrap();
    rig.assert_reload_kept("after the Seat drains");
    rig.end();
}

/// A same-id reload, then a newer source (another id) replacing the head, then the shutdown's removal
/// (`Chains::unload_all`), all queued with the engine held still: the newer source's replacement ends the
/// reload's candidate (its box filed before its promotion, the Seats undrained), so it retires its tone
/// even after the Seats drain and promote it: the promotion does not revive a keep the replacement
/// cleared. The predecessor's owner is held until after the promotion, so its row still stands there.
#[test]
fn e3_4_a_source_replacing_a_reload_candidate_ahead_of_the_shutdowns_removal_retires_the_tone() {
    use crate::engine_io::unit_host::Leave;
    use crate::host::unit_owner::test_support::FakePluginConfig;

    // Holds the predecessor's owner at a turn, so its row stands (superseded, not released) until the
    // test lets it take its box back after the promotion.
    let held_predecessor = HookPause::new();
    let rig = KeepRig::new(
        "e3-4-reload-replaced",
        951,
        FakePluginConfig { source: true, on_turn: Some(held_predecessor.hook()), ..Default::default() },
    );
    let host = rig.host().clone();
    let other = PluginRef { format: "clap".into(), path: "C:\\plugins\\NewerSource.clap".into(), id: "newer.source".into() };
    let newer = host.unit(0, 952, Spec::Plugin(other.clone()), 0, false).expect("reserve the newer source");
    let terms = newer.terms().expect("engine terms");
    newer.activated(Activated { name: other.id.clone(), source: true, latency: 0 });
    hold_self_drains(&host);
    let predecessor = host.core.state.lock().unwrap().table.active_row_for_id(951).map(|r| r.token).expect("the seated row");
    held_predecessor.arm();
    held_predecessor.wait_entered("the predecessor's owner never took a turn");
    // Holds the candidate's owner at its first turn after its install is queued, so the test decides
    // when it takes its box back.
    let held_candidate = HookPause::new();
    held_candidate.arm();
    let engine = host.core.rt.lock().unwrap();
    let (candidate, owner) =
        rig.start_reload(FakePluginConfig { source: true, on_turn: Some(held_candidate.hook()), ..Default::default() });
    held_candidate.wait_entered("the candidate's owner never took a turn");
    newer.submit(Box::new(StandInSource), terms).map_err(|(_, e)| e).expect("submit the newer source");
    let (go, unloading) = unload_all_held_in_removal(&rig, 951);
    drop(engine);
    wait_until(PATIENCE, "the candidate's box is filed, replaced, before its promotion", || {
        let mut port = host.core.ports[0].lock().unwrap();
        let mut st = host.core.state.lock().unwrap();
        if let Some(port) = port.as_mut() {
            st.reap_port(0, port);
        }
        st.table.row(candidate).is_some_and(|r| matches!(r.leave, Leave::Done))
    });
    {
        let st = host.core.state.lock().unwrap();
        assert!(st.model.has_pending_install(0, 951), "the reload's Event::Seat is still undrained");
        assert!(
            st.table.row(predecessor).is_some_and(|r| r.superseded && !r.released),
            "the predecessor's row still stands, superseded"
        );
    }

    // The Seats drain: the reload's promotion, the replacement, the removal.
    host.drain_feed(&mut Vec::new());
    go.send(()).unwrap();
    unloading.join().unwrap();
    held_predecessor.resume();
    held_candidate.resume();
    wait_until(PATIENCE, "the candidate's owner ends", || owner.is_finished());
    assert!(owner.join().unwrap().is_err(), "the candidate never answered seated");
    rig.assert_reload_retired("a reload candidate a newer source replaced ahead of the shutdown's removal");
    newer.release();
    rig.end();
}

// ── E3-4 review round 3: a failed add's files go before its id is free; an add that answers `Err` keeps
// nothing, whatever the shutdown did; a refused add's import goes back under its token ──

/// A unit whose add the engine refuses in slot 0 deletes its pin while its id is still reserved: held
/// the moment its owner frees the id (`ON_ID_FREED`, `UnitHost::failed` or `release`, whichever comes
/// first), its pin is already gone, and a same-id add in slot 1 made there seats, starts from slot 1's
/// tone and pins that, its pin standing once the refused owner has ended.
#[test]
fn e3_4_a_refused_adds_files_go_before_its_id_is_free() {
    use crate::engine_io::unit_host::ON_ID_FREED;
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host().clone();
    let doc = Arc::new(PluginHostState::default());
    let epoch = doc.begin_frontend_session();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("e3-4-id-reuse");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\ReuseAmp.clap".into(), id: "reuse.amp".into() };
    std::fs::write(dir.join(amp.file_name(0)), make_tone_bytes(&amp, "ReuseAmp", b"slot-0-tone")).unwrap();
    std::fs::write(dir.join(amp.file_name(1)), make_tone_bytes(&amp, "ReuseAmp", b"slot-1-tone")).unwrap();
    let freeing = HookPause::new();
    let exited = Arc::new(AtomicBool::new(false));
    let restores_0: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::default();
    let restores_1: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::default();
    let spec = || UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };

    // Slot 0's add: refused on latency, its owner held where it frees the id.
    freeing.arm();
    ON_ID_FREED.lock().unwrap().push((991, freeing.hook()));
    let refused = {
        let (chains, host, doc, store, spec) = (chains.clone(), host.clone(), doc.clone(), store.clone(), spec());
        let (restores_0, exited) = (restores_0.clone(), exited.clone());
        std::thread::spawn(move || {
            let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
                let cfg = FakePluginConfig {
                    latency: lf_engine::max_chain_latency(48_000) + 1,
                    restores: Some(restores_0.clone()),
                    exited_flag: Some(exited.clone()),
                    ..Default::default()
                };
                spawn_fake_unit(unit_host, sink, tone, cfg)
            };
            chains.add(&host, (&doc, epoch), &empty_slots(), Some(&store), 0, 991, spec, 0, None, noop_sink(), &open)
        })
    };
    freeing.wait_entered("the refused unit's owner never freed its id");
    assert_eq!(restores_0.lock().unwrap().as_slice(), &[Some(b"slot-0-tone".to_vec())], "the refused unit's load read (and pinned) slot 0's tone");
    assert!(!dir.join("unit-991.pin").exists(), "the refused unit's pin is gone before its id is free");

    let open_1 = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(unit_host, sink, tone, FakePluginConfig { restores: Some(restores_1.clone()), ..Default::default() })
    };
    let during = chains.add(&host, (&doc, epoch), &empty_slots(), Some(&store), 1, 991, spec(), 0, None, noop_sink(), &open_1);
    freeing.resume();
    assert_eq!(during, Ok(Added::Seated), "the id is free: slot 1's unit 991 seats while the refused owner is held");
    assert!(refused.join().unwrap().is_err(), "the engine refuses slot 0's unit on latency");
    wait_until(PATIENCE, "the refused unit's owner ends", || exited.load(Ordering::Acquire));
    assert_eq!(
        restores_1.lock().unwrap().as_slice(),
        &[Some(b"slot-1-tone".to_vec())],
        "slot 1's unit 991 starts from slot 1's tone, never from the refused unit's pin"
    );
    let pin = std::fs::read(dir.join("unit-991.pin")).expect("slot 1's unit 991 keeps its pin");
    assert_eq!(decode_tone(&pin).unwrap().state, b"slot-1-tone", "its pin holds slot 1's tone");
    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The shutdown's removal (`Chains::unload_all`) takes a unit's plate after its load pinned the slot's
/// tone and before its owner submits it: the owner takes `Back::Dismissed`, and its add answers `Err`. A
/// unit whose add answers `Err` leaves no file of its own (no report may have named its id), whatever the
/// shutdown's removal decided.
#[test]
fn e3_4_an_add_the_shutdown_dismisses_before_it_submits_leaves_no_pin() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host().clone();
    let doc = Arc::new(PluginHostState::default());
    let epoch = doc.begin_frontend_session();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("e3-4-dismissed-add");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\DismissedAmp.clap".into(), id: "dismissed.amp".into() };
    let slot_file = dir.join(amp.file_name(0));
    let slot_before = make_tone_bytes(&amp, "DismissedAmp", b"the-slot-tone");
    std::fs::write(&slot_file, &slot_before).unwrap();
    let loading = HookPause::new();
    let exited = Arc::new(AtomicBool::new(false));

    loading.arm();
    let adding = {
        let (chains, host, doc, store) = (chains.clone(), host.clone(), doc.clone(), store.clone());
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
        let (on_load, exited) = (loading.hook(), exited.clone());
        std::thread::spawn(move || {
            let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
                let cfg = FakePluginConfig {
                    restores: Some(Arc::default()),
                    on_load: Some(on_load.clone()),
                    exited_flag: Some(exited.clone()),
                    ..Default::default()
                };
                spawn_fake_unit(unit_host, sink, tone, cfg)
            };
            chains.add(&host, (&doc, epoch), &empty_slots(), Some(&store), 0, 996, spec, 0, None, noop_sink(), &open)
        })
    };
    loading.wait_entered("the unit's load never read its tone");
    assert!(dir.join("unit-996.pin").exists(), "its load pinned the slot's tone");

    chains.unload_all(&host);
    assert!(host.core.state.lock().unwrap().model.find_plate(996).is_none(), "the shutdown's removal took the loading plate");
    loading.resume();
    assert!(adding.join().unwrap().is_err(), "the dismissed add answers Err");
    wait_until(PATIENCE, "the dismissed unit's owner ends", || exited.load(Ordering::Acquire));
    assert!(!dir.join("unit-996.pin").exists(), "an add that answered Err leaves no unit-996.pin");
    assert!(!dir.join("unit-996.tone").exists(), "an add that answered Err leaves no unit-996.tone");
    assert_eq!(std::fs::read(&slot_file).unwrap(), slot_before, "the slot file unchanged");
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An add passing a session import's token redeems the import into `unit-<id>.tone` before the owner
/// opens the plugin; the engine then refuses the unit. Its add leaves no file of the unit's, and the
/// import is not lost: a later add passing the same token restores it.
#[test]
fn e3_4_an_add_with_an_import_token_the_engine_refuses_leaves_no_file_and_the_token_redeemable() {
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("e3-4-refused-import");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\ImportedAmp.clap".into(), id: "imported.amp".into() };
    let slot_file = dir.join(amp.file_name(0));
    let slot_before = make_tone_bytes(&amp, "ImportedAmp", b"the-slot-tone");
    std::fs::write(&slot_file, &slot_before).unwrap();
    let token = chains.tokens.lock().unwrap().park(Tone {
        identity: amp.clone(),
        name: "Imported".into(),
        state: b"the-imported-tone".to_vec(),
    });
    let restores: Arc<Mutex<Vec<Option<Vec<u8>>>>> = Arc::default();
    let exited = Arc::new(AtomicBool::new(false));
    // Unit 993 is refused on latency; unit 994 seats.
    let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        let refused = unit_host.id() == 993;
        let cfg = FakePluginConfig {
            latency: if refused { lf_engine::max_chain_latency(48_000) + 1 } else { 0 },
            restores: Some(restores.clone()),
            exited_flag: refused.then(|| exited.clone()),
            ..Default::default()
        };
        spawn_fake_unit(unit_host, sink, tone, cfg)
    };
    let spec = || UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
    let slots = empty_slots();

    let added = chains.add(host, (&doc, epoch), &slots, Some(&store), 0, 993, spec(), 0, Some(token), noop_sink(), &open);
    assert!(added.is_err(), "the engine refuses the unit on latency: {added:?}");
    wait_until(PATIENCE, "the refused unit's owner ends", || exited.load(Ordering::Acquire));
    assert_eq!(restores.lock().unwrap().as_slice(), &[Some(b"the-imported-tone".to_vec())], "its load restored the import");
    assert!(!dir.join("unit-993.tone").exists(), "the refused add leaves no unit-993.tone");
    assert!(!dir.join("unit-993.pin").exists(), "the refused add leaves no unit-993.pin");

    assert_eq!(
        chains.add(host, (&doc, epoch), &slots, Some(&store), 0, 994, spec(), 0, Some(token), noop_sink(), &open),
        Ok(Added::Seated)
    );
    assert_eq!(restores.lock().unwrap().get(1), Some(&Some(b"the-imported-tone".to_vec())), "the token still redeems the import");
    let kept = std::fs::read(dir.join("unit-994.tone")).expect("the redeeming unit's own tone");
    assert_eq!(decode_tone(&kept).unwrap().state, b"the-imported-tone");
    assert_eq!(std::fs::read(&slot_file).unwrap(), slot_before, "the slot file unchanged");
    chains.unload_all(host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── E3-4 review round 3 fix: an add's undo is for a unit that never applied a seat ──

/// A fresh add whose install applies with its `Event::Seat` undrained, and which the shutdown's removal
/// (queued behind it while the engine was held still) ends before its owner's first answer: its add
/// answers `Err`, but its unit applied a seat, so its owner takes the shutdown's keep, not an add's undo.
/// Its pin stays for the next launch's restore under its id, and the slot's file as it was.
#[test]
fn e3_4_an_add_that_applied_and_left_before_its_answer_keeps_its_tone() {
    use crate::engine_io::unit_host::Leave;
    use crate::host::tone::decode as decode_tone;
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = start_device();
    let host = device.host().clone();
    let doc = Arc::new(PluginHostState::default());
    let epoch = doc.begin_frontend_session();
    let chains = Arc::new(Chains::new());
    let (store, dir) = temp_tone_store("e3-4-applied-add");
    std::fs::create_dir_all(&dir).unwrap();
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\AppliedAmp.clap".into(), id: "applied.amp".into() };
    let slot_file = dir.join(amp.file_name(0));
    let slot_before = make_tone_bytes(&amp, "AppliedAmp", b"the-slot-tone");
    std::fs::write(&slot_file, &slot_before).unwrap();
    let turning = HookPause::new();
    let exited = Arc::new(AtomicBool::new(false));
    hold_self_drains(&host);
    // The test device's render `try_lock`s the engine: held here, it takes no message.
    let engine = host.core.rt.lock().unwrap();

    // The add's owner is held at its first turn, its install queued.
    turning.arm();
    let adding = {
        let (chains, host, doc, store) = (chains.clone(), host.clone(), doc.clone(), store.clone());
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
        let (on_turn, exited) = (turning.hook(), exited.clone());
        std::thread::spawn(move || {
            let open = |_fmt: PluginFormat, _plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
                let cfg = FakePluginConfig {
                    restores: Some(Arc::default()),
                    on_turn: Some(on_turn.clone()),
                    exited_flag: Some(exited.clone()),
                    ..Default::default()
                };
                spawn_fake_unit(unit_host, sink, tone, cfg)
            };
            chains.add(&host, (&doc, epoch), &empty_slots(), Some(&store), 0, 997, spec, 0, None, noop_sink(), &open)
        })
    };
    turning.wait_entered("the add's owner never took a turn");
    let token = {
        let st = host.core.state.lock().unwrap();
        assert!(st.model.has_pending_install(0, 997), "the add's install is queued");
        st.table.active_row_for_id(997).map(|r| r.token).expect("the add's row")
    };
    assert!(dir.join("unit-997.pin").exists(), "its load pinned the slot's tone");

    let unloading = {
        let (chains, host) = (chains.clone(), host.clone());
        std::thread::spawn(move || chains.unload_all(&host))
    };
    wait_until(PATIENCE, "the shutdown's removal of unit 997 is sent", || {
        host.core.state.lock().unwrap().model.pending_remove_op(0, 997).is_some()
    });
    // The engine applies the install, then the removal: the unit's box comes back removed.
    drop(engine);
    wait_until(PATIENCE, "the unit's box is filed, removed", || {
        let mut port = host.core.ports[0].lock().unwrap();
        let mut st = host.core.state.lock().unwrap();
        if let Some(port) = port.as_mut() {
            st.reap_port(0, port);
        }
        st.table.row(token).is_some_and(|r| matches!(r.leave, Leave::Done))
    });
    assert!(
        host.core.state.lock().unwrap().model.has_pending_install(0, 997),
        "the install's Event::Seat is still undrained: the owner cannot have been told it seated"
    );

    turning.resume();
    assert!(adding.join().unwrap().is_err(), "the add answers Err: its unit left before its owner's first answer");
    wait_until(PATIENCE, "the add's owner ends", || exited.load(Ordering::Acquire));
    let pin = std::fs::read(dir.join("unit-997.pin")).expect("a unit that applied a seat keeps its pin at the shutdown");
    assert_eq!(decode_tone(&pin).unwrap().state, b"the-slot-tone", "its pin holds the tone it started from");
    assert!(!dir.join("unit-997.tone").exists(), "it never saved a tone of its own");
    assert_eq!(std::fs::read(&slot_file).unwrap(), slot_before, "the slot file unchanged");

    host.drain_feed(&mut Vec::new());
    unloading.join().unwrap();
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Chain host edges (a review of the chain commands, settled before chains reach players) ──

/// A stand-in plugin unit with a set latency and role (a reload candidate or a newer source the engine
/// refuses).
struct LatentStandIn(Frame, bool);

impl UnitProcessor for LatentStandIn {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: self.1, latency: self.0 }
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

/// A fake plugin owner whose re-activation (after a rebuild) waits until `gate` opens, with
/// `reactivating` raised as it starts waiting.
fn gated_fake_plugin(
    name: &str,
    source: bool,
    gate: &Arc<AtomicBool>,
    reactivating: &Arc<AtomicBool>,
    released: &Arc<AtomicUsize>,
) -> crate::host::unit_owner::test_support::FakePluginConfig {
    let (gate, reactivating) = (gate.clone(), reactivating.clone());
    crate::host::unit_owner::test_support::FakePluginConfig {
        name: name.to_string(),
        source,
        on_reactivate: Some(Arc::new(move || {
            reactivating.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + PATIENCE;
            while !gate.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        })),
        released_count: Some(released.clone()),
        ..Default::default()
    }
}

/// Fill the engine's event ring with nothing draining it (the device must be rendering).
fn fill_event_ring_undrained(device: &TestDevice) {
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

/// Chain host edge a, through a real plugin owner (review round 2, finding 7): after a rebuild, an
/// evicted source whose owner retries while a newer source B is only queued keeps its plugin, plate and
/// tone (its own unit file, never retired into the slot's); the engine refuses B (latency), and the owner
/// seats the source again. The owner is seen retrying, twice, before B is answered: each time it serves
/// a tone save sent before that retry's submit (the save's hook holds it there), its box and plate kept
/// and nothing of it queued. Red before the fix (276dc95): the first retry was answered `Replaced`, so
/// the owner ended without serving the save, retired the tone into the slot's file and tore the unit
/// down.
#[test]
fn edge_a_a_plugin_owner_keeps_an_evicted_source_while_a_newer_one_is_queued_and_reseats_it_when_refused() {
    use crate::engine_io::unit_host::Back;
    use crate::host::unit_owner::test_support::spawn_fake_unit;
    use lf_engine::{max_chain_latency, SeatRefusal};

    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));
    let (store, dir) = temp_tone_store("edge-a-owner");
    let (gate, reactivating, released) =
        (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    let save = HookPause::new();
    let mut cfg = gated_fake_plugin("EdgeA", true, &gate, &reactivating, &released);
    cfg.state_bytes = b"edge-a-dialed-in".to_vec();
    cfg.on_state = Some(save.hook());
    let cfg = Mutex::new(Some(cfg));
    let open = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, cfg.lock().unwrap().take().expect("one open"))
    };
    let src = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeA.clap".into(), id: "edge.a".into() };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(src.clone()), params: None, bypass: false };
    let added = chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 650, spec, 0, None, noop_sink(), &open);
    assert_eq!(added, Ok(Added::Seated));
    let entry = chains.registry.lock().unwrap().get(&650).cloned().expect("A's owner is registered");
    let super::Owner::Plugin(a) = &entry.owner else { panic!("A is a plugin") };
    let saved = crate::host::tone::decode(&a.owner.take_tone().expect("A's tone saved")).expect("A's tone decodes");
    assert_eq!(saved.state, b"edge-a-dialed-in".to_vec());
    assert!(dir.join("unit-650.tone").exists(), "A's own tone file is written");
    assert_eq!(store.load(0, &src).unwrap(), None, "the slot's tone file is empty");

    device.rebuild_at(96_000, 512);
    wait_until(PATIENCE, "A's owner re-activating", || reactivating.load(Ordering::SeqCst));
    // B, newer and past the latency budget, queued while the engine is still.
    device.stop();
    let past = max_chain_latency(96_000) + 1;
    let b_ref = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeB.clap".into(), id: "edge.b".into() };
    let b = host.unit(0, 651, Spec::Plugin(b_ref), 0, false).expect("reserve newer source B");
    b.activated(Activated { name: "EdgeB".into(), source: true, latency: past });
    let op_b = b.submit(Box::new(LatentStandIn(past, true)), b.terms().unwrap()).map_err(|(_, e)| e).expect("queue B");

    // Two retries while B is only queued (nothing services the port or polls B): a save sent before each
    // retry's submit is served after it, so the owner held its box through the retry's answer.
    for retry in 1..=2 {
        save.arm();
        let reply = a.owner.start_tone_save().expect("A's owner takes the save");
        if retry == 1 {
            gate.store(true, Ordering::SeqCst);
        }
        let deadline = Instant::now() + PATIENCE;
        loop {
            if save.entered() {
                break;
            }
            if matches!(reply.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)) {
                panic!("A's owner ended at retry {retry} instead of serving the save behind it (its retry was answered Replaced)");
            }
            assert!(Instant::now() < deadline, "A's owner never served the save behind retry {retry}");
            std::thread::sleep(Duration::from_millis(1));
        }
        {
            let state = host.core.state.lock().unwrap();
            assert!(state.model.has_pending_install(0, 651), "B is still only queued at retry {retry}");
            assert!(!state.model.has_pending_install(0, 650), "A's retry {retry} queued nothing behind B");
            assert!(state.model.find_plate(650).is_some(), "A keeps its plate at retry {retry}");
        }
        assert_eq!(released.load(Ordering::SeqCst), 0, "A's owner keeps its plugin at retry {retry}");
        save.resume();
        assert!(reply.recv_timeout(PATIENCE).expect("the save answers").is_ok());
    }

    // The engine refuses B; its owner fails and releases.
    crate::engine_io::unit_host::service_if_idle(&host.core);
    let mut ev = Vec::new();
    host.drain_feed(&mut ev);
    assert_eq!(b.poll(op_b), OpState::Refused(SeatRefusal::Latency));
    assert!(matches!(b.take_back(), Some(Back::Refused { why: SeatRefusal::Latency, .. })));
    b.failed("refused".into());
    b.release();

    wait_until(PATIENCE, "A seated again by its owner", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        plate_state(&host, 650).state == PlateState::Seated
            && engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 650)
    });
    assert_eq!(released.load(Ordering::SeqCst), 0, "A's owner never tore its plugin down");
    assert!(dir.join("unit-650.tone").exists(), "A keeps its own tone file");
    assert_eq!(store.load(0, &src).unwrap(), None, "A's tone was never retired into the slot's file");

    drop(entry);
    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Chain host edge b, through a real plugin owner (review round 2, findings 2 and 8): an effect E whose
/// plugin re-activates as an instrument after a rebuild is judged where it stands in the plates. Behind a
/// live seat A it is refused its place (`Placement`) and the owner parks it: the owner keeps its plugin
/// and serves on, the plate stays parked, A keeps the slot. Behind a seat whose removal is queued ahead
/// of it, the same owner seats it at the head. Red on 6d629e4 in the queued-removal pass: that seat still
/// counted as ahead (`Model::live_plate_ahead`), so the owner parked E. The live-seat pass is red on
/// b992d39 (E took the head ahead of A), and fails an owner that ends the unit instead of parking it.
#[test]
fn edge_b_a_plugin_owner_parks_an_effect_back_as_a_source_behind_a_live_seat_and_seats_it_behind_a_queued_removal() {
    use crate::engine_io::unit_host::Back;
    use crate::host::unit_owner::test_support::spawn_fake_unit;

    for removal_ahead in [false, true] {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
        let host = device.host().clone();
        let lead = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeBLead.clap".into(), id: "edge.b.lead".into() };
        let a = seat_stand_in_plugin(&host, 0, 750, &lead, 0);
        let (gate, reactivating, released) =
            (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let exited = Arc::new(AtomicBool::new(false));
        let mut cfg = gated_fake_plugin("EdgeBFx", false, &gate, &reactivating, &released);
        cfg.source_after_reactivate = Some(true);
        cfg.exited_flag = Some(exited.clone());
        let fx = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeBFx.clap".into(), id: "edge.b.fx".into() };
        let uh_e = host.unit(0, 751, Spec::Plugin(fx), 1, false).expect("reserve effect E");
        let (owner_e, answer) = spawn_fake_unit(uh_e, noop_sink(), None, cfg).expect("E's owner starts");
        assert!(matches!(answer, Answer::Seated { .. }), "got {answer:?}");

        device.rebuild_at(96_000, 512);
        wait_until(PATIENCE, "E's owner re-activating", || reactivating.load(Ordering::SeqCst));
        let Some(Back::Evicted { unit: evicted_a, .. }) = a.take_back() else { panic!("expected A evicted") };
        let op_a = a.submit(evicted_a, a.terms().unwrap()).map_err(|(_, e)| e).expect("A reseats");
        assert_eq!(host.wait_op(op_a, Duration::from_secs(2)), Ok(OpState::Applied));
        if removal_ahead {
            device.stop();
            let Removal::Sent(_) = host.remove_unit(750).expect("queue A's removal") else { panic!("expected Sent") };
        }
        // E re-activates as an instrument and submits.
        gate.store(true, Ordering::SeqCst);

        if removal_ahead {
            wait_until(PATIENCE, "E seated at the head once A's removal applied", || {
                crate::engine_io::unit_host::service_if_idle(&host.core);
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                plate_state(&host, 751).state == PlateState::Seated && engine_chain_summary(&device, 0) == vec![(751, false)]
            });
            let units = host.core.chain_snapshot().chains[0].units.clone();
            assert!(units.iter().all(|u| u.id != 750 && !u.parked), "E alone, not parked: {units:?}");
            let mut back = None;
            wait_until(PATIENCE, "A's removed box", || {
                back = a.take_back();
                back.is_some()
            });
            assert!(matches!(back, Some(Back::Removed(_))));
            a.release();
            let Removal::Sent(rem_e) = host.remove_unit(751).unwrap() else { panic!("expected Sent") };
            crate::engine_io::unit_host::service_if_idle(&host.core);
            assert_eq!(host.wait_op(rem_e, Duration::from_secs(2)), Ok(OpState::Applied));
        } else {
            wait_until(PATIENCE, "E parked behind A", || {
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                host.core.chain_snapshot().chains[0].units.iter().any(|u| u.id == 751 && u.parked)
            });
            // A request answered after the park: the owner serves on with its plugin.
            owner_e.take_tone().expect("E's owner serves on, parked");
            assert!(!exited.load(Ordering::SeqCst), "E's owner keeps its unit parked");
            assert_eq!(released.load(Ordering::SeqCst), 0, "E's owner keeps its plugin");
            assert_eq!(engine_chain_summary(&device, 0), vec![(750, false)], "A keeps the slot");
            let units = host.core.chain_snapshot().chains[0].units.clone();
            assert_eq!(units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![750, 751], "E keeps its place, parked");
            assert!(units[1].parked);
            assert_eq!(host.remove_unit(751).unwrap(), Removal::Plate);
            wait_until(PATIENCE, "E's owner ends with its plate", || exited.load(Ordering::SeqCst));
            let Removal::Sent(rem_a) = host.remove_unit(750).unwrap() else { panic!("expected Sent") };
            assert_eq!(host.wait_op(rem_a, Duration::from_secs(2)), Ok(OpState::Applied));
            drop(a.take_back());
            a.release();
        }
        let _ = owner_e.stop();
    }
}

/// Chain host edge c: a plugin owner retrying its reseat into a full port (a rebuild took the unit out)
/// keeps the place it needs: a later edit on that slot enters only after the owner's install. The run is
/// pinned by signals, not sleeps: a tone save sent before the owner's first submit is served right after
/// it, and the save's hook holds the owner there. With the owner held, its plate unseated and nothing of
/// it queued, that submit found the port full; the port is then served (the model takes no answer yet)
/// and the later move must be refused while the claim stands (well inside its lease). `waiting`: the
/// add's first answer was `waiting` (queued behind a removal a take held), not `seated`, so a claim
/// given only to owners that answered `seated` fails it. Red on b992d39: the owner retried `PortFull`
/// with no place kept, and the move entered the freed port first.
fn edge_c_owner_retrying_a_full_port_keeps_its_place(waiting: bool) {
    use crate::host::unit_owner::test_support::spawn_fake_unit;

    // 20 ms per block keeps the take below `max_loop_seconds` until the rebuild ends it.
    let period = Duration::from_millis(if waiting { 20 } else { 2 });
    let mut device = TestDevice::start(48_000, 256, period, |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_latency(64)));
    let mut at = 0;
    for id in [530, 540, 545] {
        if id == 530 && !waiting {
            continue;
        }
        assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, id, echo_spec(0.5, false), at), Ok(Added::Seated));
        at += 1;
    }
    if waiting {
        // A latent echo's removal during a take is held: the model reads slot 0 held.
        start_take(&device);
        let Removal::Sent(_held) = host.remove_unit(530).expect("queue the held removal") else {
            panic!("expected a sent removal");
        };
        wait_until(PATIENCE, "slot 0 held during the take", || {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            host.core.state.lock().unwrap().model.slot_held(0)
        });
    }
    let (gate, reactivating, released) =
        (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    let save = HookPause::new();
    let mut cfg = gated_fake_plugin("EdgeC", false, &gate, &reactivating, &released);
    cfg.on_state = Some(save.hook());
    let cfg = Mutex::new(Some(cfg));
    let open = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, cfg.lock().unwrap().take().expect("one open"))
    };
    let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeC.clap".into(), id: "edge.c".into() };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp), params: None, bypass: false };
    let added = chains.add(&host, (&doc_state, epoch), &slots, None, 0, 541, spec, at, None, noop_sink(), &open);
    let first = if waiting { Added::Waiting } else { Added::Seated };
    assert_eq!(added, Ok(first), "the add answered; its caller moved on");
    let entry = chains.registry.lock().unwrap().get(&541).cloned().expect("the plugin's owner is registered");
    let super::Owner::Plugin(p) = &entry.owner else { panic!("541 is a plugin") };

    // A rebuild takes the plugin out (and ends the take); its owner waits at the gate to re-activate, the
    // echoes reseat.
    device.rebuild_at(48_000, 512);
    wait_until(PATIENCE, "the plugin's owner re-activating", || reactivating.load(Ordering::SeqCst));
    wait_until(PATIENCE, "the echoes reseated", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        let ids: Vec<UnitId> = engine_chain_summary(&device, 0).iter().map(|&(id, _)| id).collect();
        ids.contains(&540) && ids.contains(&545)
    });
    // With the engine still nothing serves the port: fill it.
    device.stop();
    let mut pushed = 0;
    loop {
        match host.move_unit(545, 0) {
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

    // The owner re-activates, submits into the full port, then serves the save and is held there.
    save.arm();
    let reply = p.owner.start_tone_save().expect("the owner takes the save");
    gate.store(true, Ordering::SeqCst);
    save.wait_entered("the owner never served the save behind its submit");
    {
        let state = host.core.state.lock().unwrap();
        assert!(
            !state.model.has_pending_install(0, 541) && !state.model.find_plate(541).is_some_and(|(_, p)| p.seated_in_engine),
            "the owner's submit found the port full"
        );
    }
    // The port is served; the owner cannot retry yet.
    crate::engine_io::unit_host::service_if_idle(&host.core);
    let refused = host.move_unit(540, 1).expect_err("a later move must not take the place the owner's retry needs");
    assert!(refused.contains("port is full"), "got: {refused}");
    save.resume();
    assert!(reply.recv_timeout(PATIENCE).expect("the save answers").is_ok());

    // The owner retries and gets in; the move enters after it.
    let deadline = Instant::now() + PATIENCE;
    loop {
        match host.move_unit(540, 1) {
            Ok(_) => {
                let state = host.core.state.lock().unwrap();
                let owner_in = state.model.has_pending_install(0, 541)
                    || state.model.find_plate(541).is_some_and(|(_, p)| p.seated_in_engine);
                assert!(owner_in, "a later edit entered the port ahead of the owner's queued reseat");
                break;
            }
            Err(why) => assert!(why.contains("port is full"), "unexpected error: {why}"),
        }
        assert!(Instant::now() < deadline, "the later edit never entered the port");
        std::thread::sleep(Duration::from_millis(1));
    }
    wait_until(PATIENCE, "the plugin seated again", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        plate_state(&host, 541).state == PlateState::Seated
    });
    assert_eq!(released.load(Ordering::SeqCst), 0);

    drop(entry);
    chains.unload_all(&host);
    chains.join_all();
}

#[test]
fn edge_c_a_plugin_owner_retrying_a_full_port_keeps_its_place_ahead_of_a_later_edit() {
    edge_c_owner_retrying_a_full_port_keeps_its_place(false);
}

#[test]
fn edge_c_a_plugin_owner_whose_add_answered_waiting_keeps_its_place_retrying_a_full_port() {
    edge_c_owner_retrying_a_full_port_keeps_its_place(true);
}

/// Chain host edge d: a built-in add answered `waiting` (queued behind a held edit) whose install a
/// rebuild hands back unsent (the engine had no place for its answer) after the 10 s first-answer bound
/// (`BUILTIN_INITIAL_SUBMIT_TIMEOUT`) is offered to the new engine and seats. No drain runs from the
/// held edit's `waiting` on, so the engine's answers stay owed and the slot takes nothing more: the
/// install comes back `Unsent`, never applied. Red before the fix: `serve_builtin` failed any box that
/// never seated once past that bound ("could not submit before timeout"), though its caller had long
/// been answered and moved on.
#[test]
fn edge_d_a_waiting_builtin_add_handed_back_unsent_after_the_first_answer_bound_seats() {
    use crate::engine_io::unit_host::Back;

    // 20 ms per 256-frame block keeps the take below `max_loop_seconds` until the test stops it.
    let mut device = TestDevice::start_with_events(48_000, 256, Duration::from_millis(20), 8, |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_latency(64)));
    let v1 = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeD.clap".into(), id: "edge.d".into() };
    let helper = seat_stand_in_plugin(&host, 0, 10, &v1, 0);
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 530, echo_spec(0.5, false), 1), Ok(Added::Seated));

    // A latent echo's removal during a take is held: the model reads slot 0 held.
    start_take(&device);
    let Removal::Sent(_held) = host.remove_unit(530).expect("queue the held removal") else {
        panic!("expected a sent removal");
    };
    wait_until(Duration::from_secs(3), "slot 0 held during the take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        host.core.state.lock().unwrap().model.slot_held(0)
    });

    // From here nothing drains (a feed that ticks keeps every wait's own drain idle).
    let ticking = Arc::new(AtomicBool::new(true));
    let ticker = {
        let (core, ticking) = (host.core.clone(), ticking.clone());
        std::thread::spawn(move || {
            while ticking.load(Ordering::SeqCst) {
                core.stash.lock().unwrap().mark_feed_drain(Instant::now());
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    fill_event_ring_undrained(&device);
    // The take ends: the removal applies, its answers owed (the model still reads the slot held).
    host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
    wait_until(PATIENCE, "the held removal applied", || !engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 530));
    device.stop();
    // The helper's withdrawals and reseats fill what slot 0 owes, until it takes no more.
    let t1 = helper.terms().unwrap();
    for _ in 0..32 {
        if helper.withdraw().is_err() {
            break;
        }
        crate::engine_io::unit_host::service_if_idle(&host.core);
        let Some(Back::Withdrawn { unit, .. }) = helper.take_back() else { break };
        helper.submit(unit, t1).map_err(|(_, e)| e).expect("the helper reseats");
        crate::engine_io::unit_host::service_if_idle(&host.core);
    }

    // The add answers `waiting` (queued behind what the model reads held), and the slot takes nothing.
    assert_eq!(add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 531, echo_spec(0.5, false), 1), Ok(Added::Waiting));
    std::thread::sleep(super::BUILTIN_INITIAL_SUBMIT_TIMEOUT + Duration::from_millis(500));
    assert!(!engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 531), "the install was never applied");

    device.rebuild_at(48_000, 512);
    ticking.store(false, Ordering::SeqCst);
    ticker.join().unwrap();
    wait_until(PATIENCE, "the waiting add seated in the new engine", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        plate_state(&host, 531).state == PlateState::Seated
            && engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 531)
    });
    assert_eq!(plate_state(&host, 531).error, None);

    chains.unload_all(&host);
    chains.join_all();
    while helper.take_back().is_some() {}
    helper.release();
}

/// Chain host edge e: a reload candidate the engine refused (latency) whose owner released its row before
/// the reload's waiter polls still reads the engine's cause (`reload_outcome`, which the waiter and the
/// sweep log), in both orders (its box taken before the refusal's `Seat` drained, and after), however
/// many later reloads were refused for another cause meanwhile (here 70, past a port's 64 places, for
/// `Placement`), each reading its own. Red before the fix: `release` cleared the row's `settled`, so the
/// poll fell back to the generic "reload candidate was refused or cancelled before seating"; a record
/// bounded by count (32, or 64) lost the first cause, and one keeping only the latest read `Placement`.
#[test]
fn edge_e_a_refused_reload_released_before_its_waiter_polls_keeps_the_engines_cause() {
    use crate::engine_io::unit_host::Back;
    use lf_engine::{max_chain_latency, SeatRefusal};

    for drain_first in [false, true] {
        let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
        let host = device.host().clone();
        let v1 = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE.clap".into(), id: "edge.e".into() };
        let a = seat_stand_in_plugin(&host, 0, 10, &v1, 0);
        let old = a.install_token();
        let start_gen = host.core.engine_gen.load(Ordering::Acquire);
        device.stop();

        // Reload a candidate of `unit` the engine refuses for `why`; its owner takes the refusal and
        // ends (`fail_before_seat`) before any poll.
        let refuse = |stand_in: LatentStandIn, why: SeatRefusal| {
            let b = host.reload_unit(10, v1.clone()).expect("reserve the reload candidate");
            let candidate = b.install_token();
            b.activated(Activated { name: "edge.e".into(), source: stand_in.1, latency: stand_in.0 });
            let terms = b.terms().expect("engine terms");
            let op = b.submit(Box::new(stand_in), terms).map_err(|(_, e)| e).expect("submit candidate");
            crate::engine_io::unit_host::service_if_idle(&host.core);
            if drain_first {
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                assert_eq!(b.poll(op), OpState::Refused(why));
            }
            let back = b.take_back();
            assert!(matches!(back, Some(Back::Refused { why: w, .. }) if w == why), "refused for {why:?}");
            b.failed("refused".into());
            b.release();
            candidate
        };
        let read = |candidate| match super::reload_outcome(&host, 10, old, candidate, start_gen) {
            super::ReloadOutcome::Failed(why) => why,
            other => panic!("expected Failed, got {other:?}"),
        };

        let past = max_chain_latency(48_000) + 1;
        let first = refuse(LatentStandIn(past, false), SeatRefusal::Latency);
        // Later reloads refused for another cause (an effect's seat taken by an instrument).
        let mut last = first;
        for _ in 0..70 {
            last = refuse(LatentStandIn(0, true), SeatRefusal::Placement);
        }

        let why = read(first);
        assert!(why.contains("Latency"), "the first waiter reads its own cause (drain first: {drain_first}), got: {why}");
        let why = read(last);
        assert!(why.contains("Placement"), "the last waiter reads its own cause (drain first: {drain_first}), got: {why}");
        a.release();
    }
}

/// Review round 2, finding 6: a reload candidate whose unit another id's source replaces first (its
/// install queued ahead of the candidate's) ends with that replacement, and the engine's later refusal
/// of the candidate, which no reload call reads any more, leaves no cause behind: the record of causes
/// cannot grow under ordinary edits. Red on 6d629e4: the refusal was noted after the candidate had
/// ended, and stayed.
#[test]
fn edge_e_a_reload_refused_after_another_source_replaced_its_unit_keeps_no_cause() {
    let mut device = TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0);
    let host = device.host().clone();
    let v1 = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE6.clap".into(), id: "edge.e6".into() };
    let a = host.unit(0, 20, Spec::Plugin(v1.clone()), 0, false).expect("reserve source A");
    a.activated(Activated { name: "edge.e6".into(), source: true, latency: 0 });
    let op_a = a.submit(Box::new(LatentStandIn(0, true)), a.terms().unwrap()).map_err(|(_, e)| e).expect("seat A");
    assert_eq!(host.wait_op(op_a, Duration::from_secs(2)), Ok(OpState::Applied));
    device.stop();

    let b_ref = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE6b.clap".into(), id: "edge.e6b".into() };
    let b = host.unit(0, 21, Spec::Plugin(b_ref), 0, false).expect("reserve newer source B");
    b.activated(Activated { name: "edge.e6b".into(), source: true, latency: 0 });
    let op_b = b.submit(Box::new(LatentStandIn(0, true)), b.terms().unwrap()).map_err(|(_, e)| e).expect("queue B");
    let c = host.reload_unit(20, v1.clone()).expect("reserve A's reload candidate");
    let candidate = c.install_token();
    c.activated(Activated { name: "edge.e6".into(), source: true, latency: 0 });
    c.submit(Box::new(LatentStandIn(0, true)), c.terms().unwrap()).map_err(|(_, e)| e).expect("queue the candidate behind B");

    let mut ev = Vec::new();
    for _ in 0..4 {
        crate::engine_io::unit_host::service_if_idle(&host.core);
        host.drain_feed(&mut ev);
    }
    assert_eq!(b.poll(op_b), OpState::Applied, "B replaced A");
    assert!(
        host.core.state.lock().unwrap().table.reload_refusal(candidate).is_none(),
        "a refusal of a candidate already ended keeps no cause"
    );

    while c.take_back().is_some() {}
    c.release();
    while a.take_back().is_some() {}
    a.release();
    let Removal::Sent(rem_b) = host.remove_unit(21).unwrap() else { panic!("expected Sent") };
    crate::engine_io::unit_host::service_if_idle(&host.core);
    host.drain_feed(&mut ev);
    assert_eq!(host.wait_op(rem_b, Duration::from_secs(2)), Ok(OpState::Applied));
    drop(b.take_back());
    b.release();
}

/// Review round 2, finding 11, through `Chains::reload`: a reload the engine refuses (its latency, behind
/// a held edit that raised the chain's sum) keeps its cause while a reader is left, and no longer. With
/// its call waiting (`waiter`), a sweep that retires the candidate first leaves the cause to that call,
/// which logs it and forgets it. With its call gone (`Unanswered`), the sweep that retires it forgets it.
/// Red on 6d629e4 in the waiting pass: the sweep forgot the cause, so the waiting call read none (and
/// logged nothing). A record that never forgets fails both passes.
#[test]
fn edge_e_a_refused_reloads_cause_is_kept_for_its_waiter_past_a_sweep_and_forgotten_by_its_last_reader() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};
    use lf_engine::{max_chain_latency, SeatRefusal};

    for waiter in [true, false] {
        // 20 ms per block keeps the take below `max_loop_seconds` until the test ends it.
        let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
        let host = device.host().clone();
        let doc_state = PluginHostState::default();
        let epoch = doc_state.begin_frontend_session();
        let slots = empty_slots();
        let chains = Chains::new();
        let (store, dir) = temp_tone_store("edge-e-forget");
        let amp = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE11.clap".into(), id: "edge.e11".into() };
        let open_a = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(uh, sink, tone, FakePluginConfig { name: "EdgeE11".into(), ..Default::default() })
        };
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
        assert_eq!(
            chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 780, spec, 0, None, noop_sink(), &open_a),
            Ok(Added::Seated)
        );
        let rev = host.core.state.lock().unwrap().model.snapshot().rev;
        host.chain_told(rev, 5);
        let bytes = make_tone_bytes(&amp, "EdgeE11", b"tone-e11");
        let tok = chains
            .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &amp, Some(780))
            .unwrap()
            .token
            .expect("a tone token");

        // During a take an effect B that takes all but 32 frames of the budget is held; the reload of A
        // (64 frames) queues behind it and is refused once the take ends and B seats.
        start_take(&device);
        let room = max_chain_latency(48_000) - 32;
        let b_ref = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE11b.clap".into(), id: "edge.e11b".into() };
        let b = host.unit(0, 781, Spec::Plugin(b_ref), 1, false).expect("reserve effect B");
        b.activated(Activated { name: "EdgeE11b".into(), source: false, latency: room });
        b.submit(Box::new(LatentStandIn(room, false)), b.terms().unwrap()).map_err(|(_, e)| e).expect("queue B");
        wait_until(PATIENCE, "slot 0 held during the take", || {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            host.core.state.lock().unwrap().model.slot_held(0)
        });

        let candidate = Mutex::new(None);
        let open_c = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| -> Result<(UnitOwner, Answer), String> {
            *candidate.lock().unwrap() = Some(uh.install_token());
            let cfg = FakePluginConfig { name: "EdgeE11".into(), latency: 64, ..Default::default() };
            let (owner, answer) = spawn_fake_unit(uh, sink, tone, cfg)?;
            assert!(matches!(answer, Answer::Waiting { .. }), "the candidate waits behind B, got {answer:?}");
            // Without a waiter: the call stopped waiting before the engine answered.
            Ok((owner, if waiter { answer } else { Answer::Unanswered }))
        };
        let poll = HookPause::new();
        if waiter {
            poll.arm();
            let hook = poll.hook();
            *chains.before_reload_poll_hook.lock().unwrap() = Some(Box::new(move || hook()));
        }

        std::thread::scope(|scope| {
            let mut call = Some(scope.spawn(|| {
                let log = test_log::watch_current_thread();
                let res = chains.reload(&host, (&doc_state, epoch), Some(&store), 780, amp.clone(), 5, tok, noop_sink(), &open_c);
                (res, log.warns())
            }));
            if waiter {
                poll.wait_entered("the waiting reload call never reached its first poll");
            } else {
                let (res, _) = call.take().unwrap().join().unwrap();
                let why = res.expect_err("the call gave up its wait");
                assert!(why.contains("did not answer"), "got: {why}");
            }
            let candidate = candidate.lock().unwrap().expect("the candidate was opened");

            // The take ends: B seats, the candidate is refused, its owner takes the box and releases.
            host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
            wait_until(PATIENCE, "the candidate refused and its row released", || {
                let mut ev = Vec::new();
                host.drain_feed(&mut ev);
                host.core.state.lock().unwrap().table.row(candidate).is_none_or(|r| r.released)
            });
            let noted = || host.core.state.lock().unwrap().table.reload_refusal(candidate);
            assert_eq!(noted(), Some(SeatRefusal::Latency), "the cause is kept for its reader (waiter: {waiter})");

            chains.sweep(Some(&host));
            let after_sweep = noted();
            poll.resume();
            assert!(chains.pending_reloads.lock().unwrap().is_empty(), "the sweep retired the candidate");
            if let Some(call) = call.take() {
                assert_eq!(after_sweep, Some(SeatRefusal::Latency), "a sweep leaves the cause to the call that waits on it");
                let (res, warns) = call.join().unwrap();
                assert_eq!(res, Ok(Reloaded::Failed));
                assert!(
                    warns.iter().any(|w| w.contains("780") && w.contains("Latency")),
                    "the waiting call logs the engine's cause, got: {warns:?}"
                );
            }
            assert_eq!(noted(), None, "the reload's last reader forgot its cause (waiter: {waiter})");
        });

        let Removal::Sent(rem_b) = host.remove_unit(781).unwrap() else { panic!("expected Sent") };
        assert_eq!(host.wait_op(rem_b, Duration::from_secs(2)), Ok(OpState::Applied));
        drop(b.take_back());
        b.release();
        chains.unload_all(&host);
        chains.join_all();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Review round 2, finding 6, through `Chains::reload`: a newer source B replaces A while A's reload waits
/// behind B in the port (both held by a take). The waiting call reads the replacement and returns, so
/// the reload has no reader left; the engine then refuses the candidate (its seat is gone), and that
/// late refusal leaves no cause behind. Every owner on the slot is held meanwhile (A's in a tone save,
/// the candidate's in its turn), so the candidate's box is taken only after the call returned. Red on
/// 6d629e4: the refusal was noted whatever ended the candidate, so the box taken last left a cause no
/// one reads (`Table::file_returned_inner`).
#[test]
fn edge_e_a_waiting_reload_whose_unit_a_newer_source_replaces_keeps_no_cause_of_its_late_refusal() {
    use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};

    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::new();
    let (store, dir) = temp_tone_store("edge-e-late");
    let synth = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE12.clap".into(), id: "edge.e12".into() };
    let a_save = HookPause::new();
    let a_cfg = FakePluginConfig { name: "EdgeE12".into(), source: true, on_state: Some(a_save.hook()), ..Default::default() };
    let open_a = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        spawn_fake_unit(uh, sink, tone, a_cfg.clone())
    };
    let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(synth.clone()), params: None, bypass: false };
    assert_eq!(
        chains.add(&host, (&doc_state, epoch), &slots, Some(&store), 0, 790, spec, 0, None, noop_sink(), &open_a),
        Ok(Added::Seated)
    );
    let rev = host.core.state.lock().unwrap().model.snapshot().rev;
    host.chain_told(rev, 5);
    let bytes = make_tone_bytes(&synth, "EdgeE12", b"tone-e12");
    let tok = chains
        .tone_import(&host, (&doc_state, epoch), Some(&store), 0, &bytes, &synth, Some(790))
        .unwrap()
        .token
        .expect("a tone token");

    // During a take, B (a newer source whose latency changes the chain's sum) is held; it replaces A.
    start_take(&device);
    let b_ref = PluginRef { format: "clap".into(), path: "C:\\plugins\\EdgeE12b.clap".into(), id: "edge.e12b".into() };
    let b = host.unit(0, 791, Spec::Plugin(b_ref), 0, false).expect("reserve newer source B");
    b.activated(Activated { name: "EdgeE12b".into(), source: true, latency: 64 });
    let op_b = b.submit(Box::new(LatentStandIn(64, true)), b.terms().unwrap()).map_err(|(_, e)| e).expect("queue B");
    wait_until(PATIENCE, "slot 0 held during the take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        host.core.state.lock().unwrap().model.slot_held(0)
    });

    // A's reload queues behind B; its owner answers `waiting`, then is held in its next turn.
    let candidate = Mutex::new(None);
    let turn = HookPause::new();
    turn.arm();
    let open_c = |_fmt: PluginFormat, _p: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        *candidate.lock().unwrap() = Some(uh.install_token());
        let cfg = FakePluginConfig { name: "EdgeE12".into(), source: true, latency: 64, on_turn: Some(turn.hook()), ..Default::default() };
        spawn_fake_unit(uh, sink, tone, cfg)
    };
    let entry = chains.registry.lock().unwrap().get(&790).cloned().expect("A's owner is registered");
    let super::Owner::Plugin(a) = &entry.owner else { panic!("A is a plugin") };
    std::thread::scope(|scope| {
        let call = scope.spawn(|| chains.reload(&host, (&doc_state, epoch), Some(&store), 790, synth.clone(), 5, tok, noop_sink(), &open_c));
        turn.wait_entered("the candidate's owner never turned");
        wait_until(PATIENCE, "the reload call waiting on its candidate", || !chains.pending_reloads.lock().unwrap().is_empty());
        let candidate = candidate.lock().unwrap().expect("the candidate was opened");
        a_save.arm();
        let a_reply = a.owner.start_tone_save().expect("A's owner takes the save");
        a_save.wait_entered("A's owner never served the save");

        // The take ends: B replaces A, and the call reads that and returns.
        host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !call.is_finished() {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            assert!(Instant::now() < deadline, "the reload call never answered");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(call.join().unwrap(), Ok(Reloaded::Failed));
        assert_eq!(b.poll(op_b), OpState::Applied, "B replaced A");

        // The owners take the port's returns again: the candidate's refused box among them.
        turn.resume();
        a_save.resume();
        let _ = a_reply.recv_timeout(PATIENCE);
        wait_until(PATIENCE, "the candidate's owner took its box and released", || {
            let mut ev = Vec::new();
            host.drain_feed(&mut ev);
            host.core.state.lock().unwrap().table.row(candidate).is_none_or(|r| r.released)
        });
        assert!(
            host.core.state.lock().unwrap().table.reload_refusal(candidate).is_none(),
            "a refusal that came after the reload call returned keeps no cause"
        );
    });

    drop(entry);
    let Removal::Sent(rem_b) = host.remove_unit(791).unwrap() else { panic!("expected Sent") };
    assert_eq!(host.wait_op(rem_b, Duration::from_secs(2)), Ok(OpState::Applied));
    drop(b.take_back());
    b.release();
    chains.unload_all(&host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review round 2, finding 5: a built-in add's first push is bounded like its retries, from where its
/// caller's wait starts. Its owner reaches the push in time but waits on the slot's port past the 10 s
/// first-answer bound: it sends nothing and answers the timeout, so nothing enters after the caller
/// may have released the slot's order. Red on 6d629e4: the first push was an unbounded `submit`, which
/// pushed once the port came free and answered `Seated`.
#[test]
fn edge_d_a_builtin_adds_first_push_held_past_its_bound_sends_nothing() {
    let device = TestDevice::start(48_000, 256, Duration::from_millis(2), |_| 0.0);
    let host = device.host().clone();
    let doc_state = PluginHostState::default();
    let epoch = doc_state.begin_frontend_session();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    // Just before the add hands its box to the owner, this thread takes slot 0's port and keeps it.
    let (take_tx, take_rx) = std::sync::mpsc::channel::<()>();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || {
        take_tx.send(()).unwrap();
        held_rx.recv_timeout(PATIENCE).expect("the port is held");
    }));
    let added = std::thread::scope(|scope| {
        let add = scope.spawn(|| add_builtin(&chains, &host, (&doc_state, epoch), &slots, 0, 780, echo_spec(0.5, false), 0));
        take_rx.recv_timeout(PATIENCE).expect("the add reached its hook");
        let port = host.core.ports[0].lock().unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(super::BUILTIN_INITIAL_SUBMIT_TIMEOUT + Duration::from_millis(500));
        drop(port);
        add.join().unwrap()
    });
    let why = added.expect_err("an add whose first push came past its bound fails");
    assert!(why.contains("timeout"), "got: {why}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(!engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 780), "nothing entered the engine");
    assert!(!host.core.state.lock().unwrap().model.has_pending_install(0, 780), "nothing was queued");

    chains.unload_all(&host);
    chains.join_all();
}

// ── D5: the typed add (`Chains::add_unit`) and the owner's exit latch (`Chains::exited`) ─────────────

/// The typed add with no tone store and no tone token.
#[allow(clippy::too_many_arguments)]
fn typed_add<E: Into<unit_owner::LoadFail>>(
    chains: &Chains,
    host: &EngineHost,
    slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
    slot: usize,
    unit: UnitId,
    spec: UnitSpec,
    at: usize,
    open: super::Open<'_, E>,
) -> Result<lf_rig::AddOk, lf_rig::AddFail> {
    chains.add_unit(host, slots, None, slot, unit, spec, at, None, noop_sink(), open)
}

/// The install token on `unit`'s plate (`added_by`): the row its add minted.
fn added_by(host: &EngineHost, unit: UnitId) -> lf_engine::InstallToken {
    let state = host.core.state.lock().unwrap();
    state.model.find_plate(unit).expect("a plate").1.added_by
}

fn plugin_spec(plugin: &PluginRef) -> UnitSpec {
    UnitSpec { kind: UnitKind::Plugin, plugin: Some(plugin.clone()), params: None, bypass: false }
}

fn fake_ref(name: &str) -> PluginRef {
    PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{name}.clap"), id: name.to_lowercase() }
}

/// A typed opener: the fake owner `cfg` builds, answering as `load_unit` does.
fn typed_open(
    cfg: unit_owner::test_support::FakePluginConfig,
) -> impl Fn(PluginFormat, &PluginRef, UnitHost, EventSink, Option<ToneBinding>) -> Result<(UnitOwner, Answer), unit_owner::LoadFail> {
    move |_: PluginFormat, _: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        unit_owner::test_support::spawn_fake_unit_typed(uh, sink, tone, cfg.clone())
    }
}

/// D5 `Host` before the mint: a refusal no unit code met carries no token (a reserved id, an invalid id,
/// a format the path does not have), tried again at the next launch.
#[test]
fn d5_a_refusal_before_the_mint_is_host_with_no_token() {
    use lf_rig::{AddFail, Retry};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    assert!(typed_add(&chains, host, &slots, 0, 300, echo_spec(0.5, false), 0, &no_open).is_ok());
    let host_launch = |res: Result<lf_rig::AddOk, AddFail>| match res {
        Err(AddFail::Host { retry: Retry::Launch, token: None, why }) => why,
        other => panic!("expected a host refusal before the mint, got {other:?}"),
    };
    assert!(host_launch(typed_add(&chains, host, &slots, 1, 300, echo_spec(0.5, false), 0, &no_open)).contains("reserved"));
    assert!(host_launch(typed_add(&chains, host, &slots, 0, 0, echo_spec(0.5, false), 0, &no_open)).contains("invalid"));
    let wrong = PluginRef { format: "vst3".into(), path: "C:\\plugins\\Amp.clap".into(), id: "amp".into() };
    assert!(host_launch(typed_add(&chains, host, &slots, 0, 301, plugin_spec(&wrong), 0, &no_open)).contains("does not match"));

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Ok`: a seated built-in's and a seated plugin's answer carry the token on their plate (`added_by`);
/// both owners live, so their latches read unset.
#[test]
fn d5_ok_carries_the_plates_added_by() {
    use lf_rig::{AddOk, AddState};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    let echo = typed_add(&chains, host, &slots, 0, 310, echo_spec(0.5, false), 0, &no_open).expect("the echo seats");
    assert_eq!(echo, AddOk { state: AddState::Seated, token: added_by(host, 310) });
    let amp = fake_ref("OkAmp");
    let open = typed_open(unit_owner::test_support::FakePluginConfig::default());
    let plugin = typed_add(&chains, host, &slots, 0, 311, plugin_spec(&amp), 1, &open).expect("the plugin seats");
    assert_eq!(plugin, AddOk { state: AddState::Seated, token: added_by(host, 311) });
    assert_ne!(echo.token, plugin.token);
    assert!(!chains.exited(echo.token) && !chains.exited(plugin.token), "both owners serve");

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Host` with `Retry::Device`: no engine runs, so the add is refused after the mint and before any
/// unit code (a built-in's and a plugin owner's), with the attempt's token.
#[test]
fn d5_no_engine_is_host_device_with_the_attempts_token() {
    use lf_rig::{AddFail, Retry};
    let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 2.0 }, FakeDriver(Fake::new())).unwrap();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    match typed_add(&chains, &host, &slots, 0, 320, echo_spec(0.5, false), 0, &no_open) {
        Err(AddFail::Host { retry: Retry::Device, token: Some(token), .. }) => assert_eq!(token, added_by(&host, 320)),
        other => panic!("expected a no-engine refusal, got {other:?}"),
    }
    let loaded = Arc::new(AtomicBool::new(false));
    let seen = loaded.clone();
    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_load: Some(Arc::new(move || seen.store(true, Ordering::SeqCst))),
        ..Default::default()
    });
    match typed_add(&chains, &host, &slots, 1, 321, plugin_spec(&fake_ref("NoEngine")), 0, &open) {
        Err(AddFail::Host { retry: Retry::Device, token: Some(token), .. }) => assert_eq!(token, added_by(&host, 321)),
        other => panic!("expected a no-engine refusal, got {other:?}"),
    }
    assert!(!loaded.load(Ordering::SeqCst), "no plugin code ran");

    chains.unload_all(&host);
    chains.join_all();
}

/// D5 `Host` after the mint: the owner's spawn failed; the failed plate carries the token answered.
#[test]
fn d5_an_owner_spawn_failure_is_host_launch_with_the_attempts_token() {
    use lf_rig::{AddFail, Retry};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    chains.fail_next_spawn.store(true, Ordering::Release);
    match typed_add(&chains, host, &slots, 0, 330, echo_spec(0.5, false), 0, &no_open) {
        Err(AddFail::Host { retry: Retry::Launch, token: Some(token), why }) => {
            assert!(why.contains("spawn"), "got: {why}");
            assert_eq!(token, added_by(host, 330));
        }
        other => panic!("expected a spawn refusal, got {other:?}"),
    }

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Seat`: the engine refuses the seat (latency) after the unit was built, a plugin's and a
/// built-in's; each owner had ended at the answer, and the token is the attempt's.
#[test]
fn d5_a_seat_refusal_is_seat_with_the_owner_ended() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let too_late = lf_engine::max_chain_latency(48_000) + 1;
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_latency(too_late)));

    let open = typed_open(unit_owner::test_support::FakePluginConfig { latency: too_late, ..Default::default() });
    match typed_add(&chains, host, &slots, 0, 340, plugin_spec(&fake_ref("SeatAmp")), 0, &open) {
        Err(AddFail::Seat { token, exited, why }) => {
            assert!(why.contains("Latency"), "got: {why}");
            assert_eq!(token, added_by(host, 340));
            assert!(exited, "the plugin owner was joined before the answer");
            assert!(chains.exited(token));
        }
        other => panic!("expected a seat refusal, got {other:?}"),
    }
    match typed_add(&chains, host, &slots, 0, 341, echo_spec(0.5, false), 0, &no_open) {
        Err(AddFail::Seat { token, exited, why }) => {
            assert!(why.contains("Latency"), "got: {why}");
            assert_eq!(token, added_by(host, 341));
            assert!(exited, "the built-in owner was joined before the answer");
        }
        other => panic!("expected a seat refusal, got {other:?}"),
    }

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Plugin`: the unit's own setup failed (a plugin's load, a built-in's build); nothing of it runs at
/// the answer.
#[test]
fn d5_a_failed_setup_is_plugin_with_the_owner_ended() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().failing_kind(UnitKind::Echo, "echo build failed")));

    let open = typed_open(unit_owner::test_support::FakePluginConfig { fail_load: Some("load failed: no such id".into()), ..Default::default() });
    match typed_add(&chains, host, &slots, 0, 350, plugin_spec(&fake_ref("BadLoad")), 0, &open) {
        Err(AddFail::Plugin { token, exited: true, why }) => {
            assert!(why.contains("no such id"), "got: {why}");
            assert_eq!(token, added_by(host, 350));
        }
        other => panic!("expected a plugin failure, got {other:?}"),
    }
    match typed_add(&chains, host, &slots, 0, 351, echo_spec(0.5, false), 0, &no_open) {
        Err(AddFail::Plugin { token, exited: true, why }) => {
            assert_eq!(why, "echo build failed");
            assert_eq!(token, added_by(host, 351));
        }
        other => panic!("expected a plugin failure, got {other:?}"),
    }

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Unresolved`, D6: a setup that outlasts its wait before it claims answers with the owner still in
/// plugin code (`exited: false`); the latch for that token sets once the owner leaves it and ends.
#[test]
fn d5_a_setup_timeout_is_unresolved_and_its_latch_sets_when_the_owner_ends() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();
    unit_owner::set_load_wait(Duration::from_millis(300));

    let gate = Arc::new(AtomicBool::new(false));
    let held = gate.clone();
    // Where the owner stands: in its setup from `entered` until `left`.
    let (entered, left) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (in_setup, out_of_setup) = (entered.clone(), left.clone());
    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_load: Some(Arc::new(move || {
            in_setup.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + PATIENCE;
            while !held.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            out_of_setup.store(true, Ordering::SeqCst);
        })),
        ..Default::default()
    });
    let amp = fake_ref("SlowSetup");
    let token = match typed_add(&chains, host, &slots, 0, 360, plugin_spec(&amp), 0, &open) {
        Err(AddFail::Unresolved { token, exited: false, why }) => {
            assert_eq!(why, unit_owner::SETUP_TIMEOUT);
            token
        }
        other => panic!("expected an unresolved setup, got {other:?}"),
    };
    assert!(!left.load(Ordering::SeqCst), "the owner has not left its setup at the answer");
    // A starved runner can answer the 300 ms wait before the owner reaches the setup at all (CI
    // 45718706541): the answer is the same, so the test waits for the setup rather than assume it.
    wait_until(PATIENCE, "the owner enters its setup", || entered.load(Ordering::SeqCst));
    assert_eq!(token, added_by(host, 360));
    assert!(!chains.exited(token), "the owner is still in its setup");
    assert!(chains.affinity_held(&amp.path), "a setup timeout leaks its plugin file's affinity");

    gate.store(true, Ordering::SeqCst);
    wait_until(PATIENCE, "the owner's latch sets once it ends", || chains.exited(token));

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Unresolved`: the owner claimed and submitted, and the engine did not answer within the second
/// wait (`Answer::Unanswered`): the owner serves on, unexited, and its plate seats once the engine runs,
/// carrying the token answered.
#[test]
fn d5_an_unanswered_add_is_unresolved_and_seats_later_under_its_token() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();
    // Long enough for the owner to claim on a loaded machine; then as long again without an answer.
    unit_owner::set_load_wait(Duration::from_secs(1));

    let open = typed_open(unit_owner::test_support::FakePluginConfig::default());
    // The engine renders nothing while this lock is held: the install stays pending.
    let rt = host.core.rt.lock().unwrap();
    let res = typed_add(&chains, host, &slots, 0, 370, plugin_spec(&fake_ref("Unanswered")), 0, &open);
    drop(rt);
    let token = match res {
        Err(AddFail::Unresolved { token, exited: false, why }) => {
            assert!(why.contains("did not answer"), "got: {why}");
            token
        }
        other => panic!("expected an unanswered add, got {other:?}"),
    };
    wait_until(PATIENCE, "the unanswered unit seats", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        plate_state(host, 370).state == PlateState::Seated
    });
    assert_eq!(added_by(host, 370), token);
    assert!(!chains.exited(token), "its owner serves the seated unit");

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Unresolved`: a built-in whose owner does not answer within `BUILTIN_ADD_TIMEOUT` (its first push
/// waits on the slot's port, held here) answers unexited; the latch sets once the owner gets the port,
/// finds its bound passed, and ends.
#[test]
fn d5_a_builtin_that_does_not_answer_is_unresolved_and_its_latch_sets_later() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host().clone();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    let (take_tx, take_rx) = std::sync::mpsc::channel::<()>();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || {
        take_tx.send(()).unwrap();
        held_rx.recv_timeout(PATIENCE).expect("the port is held");
    }));
    // The port is held while the add answers and dropped before any join: an add that waited on its owner
    // fails this test by its bound, never hangs it.
    let (res_tx, res_rx) = std::sync::mpsc::channel();
    let (res, still_running) = std::thread::scope(|scope| {
        scope.spawn(|| res_tx.send(typed_add(&chains, &host, &slots, 0, 380, echo_spec(0.5, false), 0, &no_open)));
        take_rx.recv_timeout(PATIENCE).expect("the add reached its hook");
        let port = host.core.ports[0].lock().unwrap();
        held_tx.send(()).unwrap();
        let res = res_rx.recv_timeout(super::BUILTIN_ADD_TIMEOUT + Duration::from_secs(10));
        let still_running = match &res {
            Ok(Err(AddFail::Unresolved { token, .. })) => !chains.exited(*token),
            _ => false,
        };
        drop(port);
        (res, still_running)
    });
    let token = match res.expect("the add answers while its owner waits on the port") {
        Err(AddFail::Unresolved { token, exited: false, why }) => {
            assert!(why.contains("did not answer"), "got: {why}");
            token
        }
        other => panic!("expected an unanswered built-in, got {other:?}"),
    };
    assert_eq!(token, added_by(&host, 380));
    assert!(still_running, "the owner waits on the port");
    wait_until(PATIENCE, "the built-in owner's latch sets once it ends", || chains.exited(token));

    chains.unload_all(&host);
    chains.join_all();
}

/// D5, D6: an add answered `Waiting` whose unit the engine refuses later (`Placement`, once the take
/// ends) has its owner end on its own; another thread reads that attempt's latch set by its token. While
/// that owner is still in its plugin's release (the id already free), a later add under the same id is
/// another attempt: both latches are live at once, each read by its own token, and only the first sets
/// when its owner ends.
#[test]
fn d5_a_waiting_add_that_fails_later_sets_its_own_latch_not_a_later_plates() {
    use lf_rig::{AddOk, AddState};
    use unit_owner::test_support::FakePluginConfig;
    // 20 ms per 256-frame block keeps the recording take open cleanly without hitting max_loop_seconds.
    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());

    // A take holds the source's role change at slot 0's port head; an effect queued at index 0 behind
    // it answers `Waiting`, then is refused (`Placement`, ahead of the source) once the take ends.
    start_take(&device);
    let open_src = typed_open(FakePluginConfig { name: "HeadSrc".into(), source: true, ..Default::default() });
    let src = typed_add(&chains, &host, &slots, 0, 390, plugin_spec(&fake_ref("HeadSrc")), 0, &open_src);
    assert!(matches!(src, Ok(AddOk { state: AddState::Waiting, .. })), "got {src:?}");
    let ended = Arc::new(AtomicBool::new(false));
    // The refused effect's owner stops in its plugin's release until the gate opens.
    let (releasing_tx, releasing_rx) = std::sync::mpsc::channel::<()>();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    let open_a = typed_open(FakePluginConfig {
        name: "FailsLate".into(),
        exited_flag: Some(ended.clone()),
        on_release: Some(Arc::new(move || {
            let _ = releasing_tx.send(());
            let _ = gate_rx.lock().unwrap().recv_timeout(PATIENCE);
        })),
        ..Default::default()
    });
    let first = typed_add(&chains, &host, &slots, 0, 391, plugin_spec(&fake_ref("FailsLate")), 0, &open_a).expect("the effect waits");
    assert_eq!(first, AddOk { state: AddState::Waiting, token: added_by(&host, 391) });
    assert!(!chains.exited(first.token), "a waiting add's owner serves");

    stop_take(&device);
    releasing_rx.recv_timeout(PATIENCE).expect("the refused effect's owner reaches its release");
    assert!(!chains.exited(first.token), "its owner is still in its plugin's release");

    wait_until(Duration::from_secs(3), "slot 0 unheld after the take", || {
        let mut ev = Vec::new();
        host.drain_feed(&mut ev);
        !host.core.state.lock().unwrap().model.slot_held(0)
    });
    let open_b = typed_open(FakePluginConfig { name: "Again".into(), ..Default::default() });
    let again = typed_add(&chains, &host, &slots, 0, 391, plugin_spec(&fake_ref("Again")), 1, &open_b).expect("the id is free again");
    assert_eq!(again, AddOk { state: AddState::Seated, token: added_by(&host, 391) });
    assert_ne!(again.token, first.token);
    assert!(
        !chains.exited(first.token) && !chains.exited(again.token),
        "two attempts under one id, both latches live at once"
    );

    gate_tx.send(()).unwrap();
    let reader = chains.clone();
    let seen = std::thread::spawn(move || {
        let deadline = Instant::now() + PATIENCE;
        while !reader.exited(first.token) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    })
    .join()
    .unwrap();
    assert!(seen, "another thread reads the refused attempt's latch set");
    assert!(ended.load(Ordering::SeqCst), "the latch sets after the owner's own last step");
    assert!(!chains.exited(again.token), "the later plate's owner serves");

    chains.unload_all(&host);
    chains.join_all();
}

/// D5, D6 (the latch's meaning): an owner whose loop unwinds while the engine still holds its seated unit
/// leaves the plugin loaded (`abandon_in_engine`); the plugin may still process, so that attempt's latch
/// never sets, its thread's end included.
#[test]
fn d5_an_owner_that_leaves_its_unit_in_the_engine_never_sets_its_latch() {
    use lf_rig::{AddOk, AddState};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();

    let panic_now = Arc::new(AtomicBool::new(false));
    let armed = panic_now.clone();
    let ended = Arc::new(AtomicBool::new(false));
    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_turn: Some(Arc::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                panic!("an owner turn panics with its unit seated");
            }
        })),
        exited_flag: Some(ended.clone()),
        ..Default::default()
    });
    let seated = typed_add(&chains, host, &slots, 0, 400, plugin_spec(&fake_ref("Unwinds")), 0, &open).expect("the plugin seats");
    assert_eq!(seated.state, AddState::Seated);
    let AddOk { token, .. } = seated;

    panic_now.store(true, Ordering::SeqCst);
    wait_until(PATIENCE, "the owner's serving ends after its panic", || ended.load(Ordering::SeqCst));
    // The runner returned; its thread ends right after (`exited_flag` is its last step but the latch's guard).
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 400),
        "the unit is still in the engine"
    );
    assert!(!chains.exited(token), "an owner that left its unit in the engine never reads ended");

    chains.unload_all(host);
    chains.join_all();
}

/// D5, D6: the attempt's latch is registered at the mint, before any unit code runs, so a reader that
/// finds the token on its plate (`added_by`) while the plugin's setup still runs reads it unset. A token
/// this host has no latch for reads unended; one whose owner ended reads ended.
#[test]
fn d5_the_latch_is_known_from_the_mint_while_the_setup_runs() {
    use lf_rig::{AddOk, AddState};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let gate_rx = Mutex::new(gate_rx);
    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_load: Some(Arc::new(move || {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = gate_rx.lock().unwrap().recv_timeout(PATIENCE);
        })),
        ..Default::default()
    });
    let (added, token) = std::thread::scope(|scope| {
        let add = scope.spawn(|| typed_add(&chains, host, &slots, 0, 410, plugin_spec(&fake_ref("HeldSetup")), 0, &open));
        entered_rx.recv_timeout(PATIENCE).expect("the owner enters its setup");
        let token = added_by(host, 410);
        let held = !chains.exited(token);
        gate_tx.send(()).unwrap();
        assert!(held, "a setup still running reads its attempt unended");
        (add.join().unwrap(), token)
    });
    assert_eq!(added, Ok(AddOk { state: AddState::Seated, token }));
    assert!(!chains.exited(token), "its owner serves");
    assert!(!chains.exited(token + 1_000), "a token with no latch here reads unended");

    chains.unload_all(host);
    chains.join_all();
    assert!(chains.exited(token), "its owner ended");
}

/// D5: a shutdown that begins after the owner answered `Waiting` and before the add registers it refuses
/// the seat, not the load: the plugin's code ran, its owner still runs (`exited: false`), never `Host`.
#[test]
fn d5_closing_after_a_waiting_answer_is_seat_with_the_owner_running() {
    use lf_rig::AddFail;
    use unit_owner::test_support::FakePluginConfig;
    let device = TestDevice::start(48_000, 256, Duration::from_millis(20), |_| 0.0);
    let host = device.host().clone();
    let slots = empty_slots();
    let chains = Arc::new(Chains::new());

    start_take(&device);
    let closing = chains.clone();
    *chains.before_register_hook.lock().unwrap() = Some(Box::new(move || closing.closing.store(true, Ordering::Release)));
    let open = typed_open(FakePluginConfig { name: "ClosingSrc".into(), source: true, ..Default::default() });
    match typed_add(&chains, &host, &slots, 0, 420, plugin_spec(&fake_ref("ClosingSrc")), 0, &open) {
        Err(AddFail::Seat { token, exited, why }) => {
            assert!(why.contains("closing"), "got: {why}");
            assert_eq!(token, added_by(&host, 420));
            assert!(!exited, "its owner serves until the shutdown joins it");
            assert!(!chains.exited(token));
        }
        other => panic!("expected a seat refusal, got {other:?}"),
    }

    stop_take(&device);
    chains.unload_all(&host);
    chains.join_all();
}

/// D5 `Host` after the mint, through the real `load_unit`: a load this host refuses before any of the
/// plugin's code runs (the binary gone since the scan, an id no plugin of its format can have) is the
/// host's, tried again at the next launch, in each format.
#[test]
fn d5_a_load_refused_before_plugin_code_is_host() {
    use lf_rig::{AddFail, Retry};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();

    let dir = std::env::temp_dir().join(format!(
        "bleeploop-d5-host-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Files that exist, so only their ids are refused (neither is ever loaded).
    let vst3_file = dir.join("Present.vst3");
    let vst2_file = dir.join("Present.dll");
    std::fs::write(&vst3_file, b"").unwrap();
    std::fs::write(&vst2_file, b"").unwrap();
    let missing = "C:\\__bleeploop_nonexistent_plugin_dir__";
    let class_id = "0123456789abcdef0123456789abcdef";
    let cases = [
        (430, "vst3", format!("{missing}\\Missing.vst3"), class_id.to_string(), "no loadable VST3 binary"),
        (431, "vst3", vst3_file.to_string_lossy().into_owned(), "not-a-class-id".to_string(), "bad VST3 class id"),
        (432, "clap", format!("{missing}\\Missing.clap"), "missing.amp".to_string(), "no CLAP file"),
        (433, "vst2", format!("{missing}\\Missing.dll"), "0000beef".to_string(), "no VST2 file"),
        (434, "vst2", vst2_file.to_string_lossy().into_owned(), "beef".to_string(), "bad VST2 plugin id"),
    ];
    let real_open = |format: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit_host, 0, sink, tone)
    };
    for (unit, format, path, id, words) in cases {
        let plugin = PluginRef { format: format.into(), path: path.clone(), id };
        match typed_add(&chains, host, &slots, 0, unit, plugin_spec(&plugin), 0, &real_open) {
            Err(AddFail::Host { retry: Retry::Launch, token: Some(token), why }) => {
                assert!(why.contains(words), "{path}: got {why}");
                assert_eq!(token, added_by(host, unit), "{path}");
                assert!(chains.exited(token), "{path}: its owner ended");
            }
            other => panic!("{path}: expected a host refusal, got {other:?}"),
        }
        assert!(!chains.affinity_held(&path), "{path}: the refused load holds no affinity");
        assert_eq!(plate_state(host, unit).state, PlateState::Failed, "{path}");
    }

    chains.unload_all(host);
    chains.join_all();
    let _ = std::fs::remove_dir_all(&dir);
}

/// D5 `Unresolved`: an owner that ends without answering (a panic in the host's own code during the load)
/// says nothing of the plugin: unresolved, with the latch as it reads once the owner was joined.
#[test]
fn d5_an_owner_that_ends_without_answering_is_unresolved() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();

    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_load: Some(Arc::new(|| panic!("a host bug in the load"))),
        ..Default::default()
    });
    let amp = fake_ref("NoAnswer");
    match typed_add(&chains, host, &slots, 0, 440, plugin_spec(&amp), 0, &open) {
        Err(AddFail::Unresolved { token, exited, why }) => {
            assert!(why.contains("before answering"), "got: {why}");
            assert_eq!(token, added_by(host, 440));
            assert!(exited, "the owner was joined, and it left no unit in the engine");
            assert!(chains.exited(token));
        }
        other => panic!("expected an unresolved add, got {other:?}"),
    }
    assert!(!chains.affinity_held(&amp.path), "an ended owner holds no affinity");

    chains.unload_all(host);
    chains.join_all();
}

/// D5: a failed built-in add answers within its bound (`BUILTIN_EXIT_WAIT`) while its owner's thread is
/// held after its answer, never joining it under the slot's order lock; the latch reads as it stands
/// (unset) and sets once the thread ends.
#[test]
fn d5_a_failed_builtin_answers_within_its_bound_while_its_owner_ends() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let too_late = lf_engine::max_chain_latency(48_000) + 1;
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().with_latency(too_late)));

    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    *chains.builtin_end_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = gate_rx.recv_timeout(PATIENCE);
    }));
    let asked = Instant::now();
    let res = typed_add(&chains, host, &slots, 0, 450, echo_spec(0.5, false), 0, &no_open);
    let took = asked.elapsed();
    let token = match res {
        Err(AddFail::Seat { token, exited: false, why }) => {
            assert!(why.contains("Latency"), "got: {why}");
            token
        }
        other => panic!("expected a seat refusal with the owner still ending, got {other:?}"),
    };
    assert!(took < Duration::from_secs(5), "the add answered in {took:?}");
    assert!(!chains.exited(token), "the owner's thread is held");
    // The slot's next call is not held up either.
    assert!(typed_add(&chains, host, &slots, 0, 451, echo_spec(0.5, false), 0, &no_open).is_err());

    gate_tx.send(()).unwrap();
    wait_until(PATIENCE, "the latch sets once the owner's thread ends", || chains.exited(token));

    chains.unload_all(host);
    chains.join_all();
}

/// D5, D6: a token with no latch here reads unended (a plate's read before its latch's registration, a
/// token never minted for an add), and an ended attempt reads ended for good, a sweep after it included.
#[test]
fn d5_a_token_with_no_latch_reads_unended_and_an_ended_one_stays_ended() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new().failing_kind(UnitKind::Echo, "echo build failed")));

    let token = match typed_add(&chains, host, &slots, 0, 460, echo_spec(0.5, false), 0, &no_open) {
        Err(AddFail::Plugin { token, exited: true, .. }) => token,
        other => panic!("expected a plugin failure, got {other:?}"),
    };
    assert!(chains.exited(token), "its owner never ran");
    chains.sweep(Some(host));
    assert!(chains.exited(token), "a sweep keeps an ended attempt ended");
    assert!(!chains.exited(token + 1_000), "a token with no latch here reads unended");

    chains.unload_all(host);
    chains.join_all();
}

/// D5, D6: the plate is in the host's view from the mint on, before the add registers its latch; a reader
/// that finds its token there (a `rig_view`) in between reads it unended, a plugin's and a built-in's.
#[test]
fn d5_a_token_read_between_its_plates_insert_and_its_latch_reads_unended() {
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Arc::new(Chains::with_factory(Arc::new(ClampingBuiltinFactory::new())));

    let open = typed_open(unit_owner::test_support::FakePluginConfig::default());
    for (unit, spec) in [(470, plugin_spec(&fake_ref("Window"))), (471, echo_spec(0.5, false))] {
        let read = Arc::new(Mutex::new(None));
        let (reader, seen, view) = (chains.clone(), read.clone(), host.clone());
        *chains.after_mint_hook.lock().unwrap() = Some(Box::new(move |token| {
            *seen.lock().unwrap() = Some((added_by(&view, unit) == token, reader.exited(token)));
        }));
        let added = typed_add(&chains, host, &slots, 0, unit, spec, 0, &open).expect("the unit seats");
        assert_eq!(*read.lock().unwrap(), Some((true, false)), "unit {unit}: the plate holds the token, read unended");
        assert!(!chains.exited(added.token), "unit {unit}: its owner serves");
    }

    chains.unload_all(host);
    chains.join_all();
}

/// D5, D6: a built-in owner that panics while the engine holds its seated box abandons its latch, as a
/// plugin owner's caught panic does: the engine may still run the box, so the attempt never reads ended.
#[test]
fn d5_a_builtin_that_panics_with_its_box_in_the_engine_never_sets_its_latch() {
    use lf_rig::{AddOk, AddState};
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));

    let panic_now = Arc::new(AtomicBool::new(false));
    let armed = panic_now.clone();
    *chains.builtin_turn_hook.lock().unwrap() = Some(Arc::new(move || {
        if armed.swap(false, Ordering::SeqCst) {
            panic!("a built-in owner's turn panics with its box seated");
        }
    }));
    let AddOk { state, token } = typed_add(&chains, host, &slots, 0, 480, echo_spec(0.5, false), 0, &no_open).expect("the echo seats");
    assert_eq!(state, AddState::Seated);

    panic_now.store(true, Ordering::SeqCst);
    let finished = || chains.registry.lock().unwrap().get(&480).is_none_or(|e| e.owner.is_finished());
    wait_until(PATIENCE, "the owner's thread ends after its panic", finished);
    assert!(engine_chain_summary(&device, 0).iter().any(|&(id, _)| id == 480), "the box is still in the engine");
    assert!(!chains.exited(token), "an owner that left its box in the engine never reads ended");

    chains.unload_all(host);
    chains.join_all();
}

/// D5 `Host`: no device and a binary gone since the scan: the device's refusal comes first, as it did
/// before the file check, through the real `load_unit`.
#[test]
fn d5_no_device_is_refused_before_a_missing_binary() {
    use lf_rig::{AddFail, Retry};
    let host = EngineHost::with_driver(HostConfig { max_loop_seconds: 2.0 }, FakeDriver(Fake::new())).unwrap();
    let slots = empty_slots();
    let chains = Chains::new();

    let real_open = |format: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit_host, 0, sink, tone)
    };
    let gone = PluginRef {
        format: "clap".into(),
        path: "C:\\__bleeploop_nonexistent_plugin_dir__\\Missing.clap".into(),
        id: "missing.amp".into(),
    };
    match typed_add(&chains, &host, &slots, 0, 490, plugin_spec(&gone), 0, &real_open) {
        Err(AddFail::Host { retry: Retry::Device, token: Some(token), why }) => {
            assert!(why.contains("no audio device"), "got: {why}");
            assert_eq!(token, added_by(&host, 490));
            assert!(chains.exited(token), "its owner ended");
        }
        other => panic!("expected the device's refusal, got {other:?}"),
    }

    chains.unload_all(&host);
    chains.join_all();
}

/// D5, D6: an owner that left its unit in the engine keeps its plugin file's editor affinity when a sweep
/// drops its finished entry, as the add's own failure does: the plugin stays loaded and may still process.
#[test]
fn d5_a_swept_owner_that_left_its_unit_in_the_engine_keeps_its_editor_affinity() {
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();

    let panic_now = Arc::new(AtomicBool::new(false));
    let armed = panic_now.clone();
    let open = typed_open(unit_owner::test_support::FakePluginConfig {
        on_turn: Some(Arc::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                panic!("an owner turn panics with its unit seated");
            }
        })),
        ..Default::default()
    });
    let amp = fake_ref("SweptAbandon");
    let token = typed_add(&chains, host, &slots, 0, 500, plugin_spec(&amp), 0, &open).expect("the plugin seats").token;

    panic_now.store(true, Ordering::SeqCst);
    let finished = || chains.registry.lock().unwrap().get(&500).is_none_or(|e| e.owner.is_finished());
    wait_until(PATIENCE, "the owner's thread ends after its panic", finished);
    assert!(!chains.exited(token), "its owner left its unit in the engine");
    chains.sweep(Some(host));
    assert!(chains.registry.lock().unwrap().get(&500).is_none(), "the sweep took the finished entry");
    assert!(chains.affinity_held(&amp.path), "the abandoned instance keeps its plugin file's affinity");

    chains.unload_all(host);
    chains.join_all();
}

/// D5, D6: a string-answering opener (`LoadFail::from`'s latch is no owner's) whose setup outlasts its wait
/// keeps the latch its owner took at the mint: the attempt reads unended while that owner is in its setup.
#[test]
fn d5_a_string_openers_setup_timeout_keeps_its_owners_latch() {
    use lf_rig::AddFail;
    let device = start_device();
    let host = device.host();
    let slots = empty_slots();
    let chains = Chains::new();
    unit_owner::set_load_wait(Duration::from_millis(300));

    let gate = Arc::new(AtomicBool::new(false));
    let held = gate.clone();
    let left = Arc::new(AtomicBool::new(false));
    let out_of_setup = left.clone();
    let cfg = unit_owner::test_support::FakePluginConfig {
        on_load: Some(Arc::new(move || {
            let deadline = Instant::now() + PATIENCE;
            while !held.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            out_of_setup.store(true, Ordering::SeqCst);
        })),
        ..Default::default()
    };
    let open = move |_: PluginFormat, _: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
        unit_owner::test_support::spawn_fake_unit(uh, sink, tone, cfg.clone())
    };
    let amp = fake_ref("StringSlow");
    let (token, exited) = match typed_add(&chains, host, &slots, 0, 510, plugin_spec(&amp), 0, &open) {
        Err(AddFail::Unresolved { token, exited, why }) => {
            assert_eq!(why, unit_owner::SETUP_TIMEOUT);
            (token, exited)
        }
        other => panic!("expected an unresolved setup, got {other:?}"),
    };
    let in_setup = !left.load(Ordering::SeqCst) && !chains.exited(token);
    gate.store(true, Ordering::SeqCst);
    assert!(!exited, "the answer reads the owner's latch, unset in its setup");
    assert!(in_setup, "the attempt reads unended while its owner is in its setup");
    assert!(chains.affinity_held(&amp.path), "a setup timeout leaks its plugin file's affinity");
    wait_until(PATIENCE, "the owner's latch sets once it ends", || chains.exited(token));

    chains.unload_all(host);
    chains.join_all();
}
