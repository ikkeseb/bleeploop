//! The app's restore host (CP3b, `host.rs`) on a real [`EngineHost`] over a [`TestDevice`] and a real
//! [`Chains`]: the add's place and match (D9), the player's source (D9), the seed and its barrier (D1,
//! D2), the forget, the refusals once closing, and a whole restore through a [`Rig`] that holds its
//! add while the player moves a level. Plugin units are the fake owners of `unit_owner::test_support`
//! (no plugin code); built-ins are the engine's own (`Builtins`) unless a test gates their build.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use lf_engine::dsp::fx::{FxKind, FxParam};
use lf_engine::grid::Frame;
use lf_engine::{max_chain_latency, Command, InputSend, LaneState, ProcessContext, SeatRefusal, SlotEvent, TimedCommand, UnitCx, UnitId, UnitInfo, UnitKind, UnitProcessor, SLOT_COUNT, TRACK_COUNT};
use lf_rig::{AddFail, AddOk, AddState, Place, PluginRecord, PluginRefView, Retry, RigRecord, RigStatus, UnitBody, UnitRecord, SAVE_QUIET};

use super::host::{AppHost, Loader};
use super::tests::{echo_unit, record, wait_until, Folder};
use super::{files, writer, Forgot, RestoreHost, Rig, SeedTaken};
use crate::engine_io::builtins::Builtins;
use crate::engine_io::chain::{PlateState, PluginRef, UnitSpec};
use crate::engine_io::chain_cmds::tests::{empty_slots, ClampingBuiltinFactory};
use crate::engine_io::chain_cmds::Chains;
use crate::engine_io::plugins::EngineSlot;
use crate::engine_io::test_rig::{TestDevice, PATIENCE};
use crate::engine_io::unit_host::{Activated, Back, BuiltinFactory, OpState, Params, Removal, Spec, Terms, Unit, UnitHost, BEFORE_RESERVE_STATE};
use crate::engine_io::EngineHost;
use crate::host::engine_slot::{EventSink, PluginFormat};
use crate::host::tone::{ToneBinding, ToneIdentity, ToneStore};
use crate::host::unit_owner::test_support::{spawn_fake_unit_typed, FakePluginConfig};
use crate::host::unit_owner::{Answer, LoadFail, UnitOwner};

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────────────

/// Fake plugin owners as `cfg` says; the next load fails with `fail` once, when set.
struct FakeLoader {
    cfg: FakePluginConfig,
    fail: Arc<Mutex<Option<String>>>,
}

impl Loader for FakeLoader {
    fn open(&self, _: PluginFormat, _: &PluginRef, unit: UnitHost, sink: EventSink, tone: Option<ToneBinding>) -> Result<(UnitOwner, Answer), LoadFail> {
        let mut cfg = self.cfg.clone();
        cfg.fail_load = self.fail.lock().unwrap().take();
        spawn_fake_unit_typed(unit, sink, tone, cfg)
    }

    fn sink(&self, _: UnitId) -> EventSink {
        noop_sink()
    }
}

fn noop_sink() -> EventSink {
    Arc::new(|_| {})
}

/// A device, its chain host and the app's restore host over both.
struct Bench {
    device: TestDevice,
    chains: Arc<Chains>,
    slots: Arc<Mutex<[EngineSlot; SLOT_COUNT]>>,
    /// The fake loader's next load fails with this reason, once.
    fail: Arc<Mutex<Option<String>>>,
    app: Arc<AppHost>,
}

impl Bench {
    fn new(device: TestDevice) -> Bench {
        Bench::with(device, Chains::new(), FakePluginConfig::default(), None)
    }

    fn with(device: TestDevice, chains: Chains, cfg: FakePluginConfig, tones: Option<ToneStore>) -> Bench {
        let chains = Arc::new(chains);
        let slots = Arc::new(empty_slots());
        let fail = Arc::new(Mutex::new(None));
        let loader = Box::new(FakeLoader { cfg, fail: fail.clone() });
        let app = Arc::new(AppHost::new(device.host().clone(), chains.clone(), slots.clone(), tones, None, loader));
        Bench { device, chains, slots, fail, app }
    }

    fn host(&self) -> &EngineHost {
        self.device.host()
    }

    /// The player's add (`chain_add`'s typed path) of a built-in at plate index `at`.
    fn player_add(&self, slot: usize, id: UnitId, kind: UnitKind, at: usize) -> Result<AddOk, AddFail> {
        let spec = UnitSpec { kind, plugin: None, params: None, bypass: false };
        let open = |_: PluginFormat, _: &PluginRef, _: UnitHost, _: EventSink, _: Option<ToneBinding>| -> Result<(UnitOwner, Answer), LoadFail> {
            Err(LoadFail::from("no plugin here".to_string()))
        };
        self.chains.add_unit(self.host(), &self.slots, None, slot, id, spec, at, None, noop_sink(), &open)
    }

    /// The plate ids of `slot`, in order (failed adds' included).
    fn ids(&self, slot: usize) -> Vec<UnitId> {
        settle(self.host());
        writer::view(self.host()).slots[slot].iter().map(|p| p.id).collect()
    }

    fn applied(&self, id: UnitId) -> bool {
        settle(self.host());
        writer::view(self.host()).slots.iter().flatten().any(|p| p.id == id && p.applied && !p.failed_add)
    }

    fn remove(&self, id: UnitId) {
        let Removal::Sent(op) = self.host().remove_unit(id).expect("remove") else { panic!("unit {id}: expected a sent removal") };
        assert_eq!(self.host().wait_op(op, PATIENCE), Ok(OpState::Applied));
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        self.chains.unload_all(self.device.host());
        self.chains.join_all();
    }
}

fn settle(host: &EngineHost) {
    host.drain_feed(&mut Vec::new());
}

/// 256-frame blocks every millisecond.
fn fast_device() -> TestDevice {
    TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0)
}

fn builtin(id: UnitId, kind: UnitKind) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind, params: None }, bypass: None }
}

fn plugin_ref(tag: &str) -> PluginRef {
    plugin_at(tag, tag)
}

/// The plugin `host.<id_tag>` in the file `C:\plugins\<file>.clap`.
fn plugin_at(file: &str, id_tag: &str) -> PluginRef {
    PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{file}.clap"), id: format!("host.{id_tag}") }
}

fn amp(id: UnitId, tag: &str) -> UnitRecord {
    amp_at(id, tag, tag)
}

/// A record plugin unit of [`plugin_at`]'s identity.
fn amp_at(id: UnitId, file: &str, id_tag: &str) -> UnitRecord {
    let p = plugin_at(file, id_tag);
    let plugin = PluginRefView { format: p.format, path: p.path, id: p.id };
    UnitRecord { id, body: UnitBody::Plugin(PluginRecord { plugin, name: None }), bypass: None }
}

fn seated(added: Result<AddOk, AddFail>) -> lf_engine::InstallToken {
    match added {
        Ok(AddOk { state: AddState::Seated, token }) => token,
        other => panic!("expected a seated add, got {other:?}"),
    }
}

fn host_refusal(added: Result<AddOk, AddFail>) -> (Retry, Option<lf_engine::InstallToken>) {
    match added {
        Err(AddFail::Host { retry, token, .. }) => (retry, token),
        other => panic!("expected a host refusal, got {other:?}"),
    }
}

/// The slot gains the settings memory holds.
fn gains(host: &EngineHost) -> [Option<f32>; 2] {
    let mut held = [None; 2];
    for command in host.settings() {
        if let Command::SetSlotGain(slot, gain) = command {
            held[usize::from(slot)] = Some(gain);
        }
    }
    held
}

/// Whether the settings memory holds all four input sends off (the seed's send-offs).
fn sends_off(host: &EngineHost) -> bool {
    let kept = host.settings();
    InputSend::ALL.into_iter().all(|send| kept.contains(&Command::SetInputSend(send, false)))
}

fn send(host: &EngineHost, command: Command) {
    host.send(TimedCommand { frame: None, command }).expect("the ring takes it");
}

/// Render one 256-frame block at device frame `frame` on this thread, as the device's callback does:
/// for a stopped test device whose `running` the test raised, so nothing else renders a block or
/// services a port.
fn render_block(host: &EngineHost, frame: Frame) {
    let (x, mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256], vec![0.0f32; 256]);
    let entry = Instant::now();
    {
        let mut rt = host.core.rt.lock().unwrap();
        let engine = rt.engine.as_mut().expect("an engine");
        engine.process(&ProcessContext { frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 }, &x, &mut l, &mut r);
    }
    host.core.clock.publish(entry, frame, 256, host.core.rate.load(Relaxed));
}

/// A pass-through plugin stand-in, an instrument (a source) when `source`, reporting `latency`.
struct Thru {
    source: bool,
    latency: Frame,
}

impl UnitProcessor for Thru {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: self.source, latency: self.latency }
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

// ── the add's place (D9) ─────────────────────────────────────────────────────────────────────────────

/// C17: RING MOD (1) is still in its add (its plate loading, not applied) when ECHO (2) and REVERB (3)
/// are added after it: each lands after its nearest predecessor present by id, ahead of the player's
/// unit at the end, and RING MOD seating later keeps the order.
#[test]
fn anchor_resolves_after_late_seating_unit() {
    let b = Bench::new(fast_device());
    let host = b.host();
    let ring = host.unit(0, 1, Spec::Builtin { kind: UnitKind::Ring, params: Vec::new() }, 0, false).expect("reserve RING MOD");
    seated(b.player_add(0, 9, UnitKind::Echo, 1));
    seated(b.app.add(0, 2, &builtin(2, UnitKind::Echo), &Place::After(vec![1])));
    seated(b.app.add(0, 3, &builtin(3, UnitKind::Reverb), &Place::After(vec![1, 2])));
    assert_eq!(b.ids(0), vec![1, 2, 3, 9], "after their predecessors by id, the player's unit after them");

    let terms: Terms = ring.terms().expect("engine terms");
    let (unit, _): (Unit, Params) = Builtins.build(UnitKind::Ring, &Vec::new(), terms).expect("a RING MOD box");
    ring.activated(Activated { name: "RING MOD".into(), source: false, latency: 0 });
    let op = ring.submit(unit, terms).map_err(|(_, e)| e).expect("submit RING MOD");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    assert_eq!(b.ids(0), vec![1, 2, 3, 9], "RING MOD seats where its plate stood");
    assert!([1, 2, 3, 9].iter().all(|id| b.applied(*id)), "every unit applied");
}

/// The player puts a unit at the head and removes a record unit between the restore's adds: the next
/// add still goes right after its nearest predecessor present (by id, read under `state` at its mint),
/// never at an index an earlier view gave.
#[test]
fn player_edit_meanwhile_does_not_shift_anchor() {
    let b = Bench::new(fast_device());
    seated(b.app.add(0, 1, &builtin(1, UnitKind::Echo), &Place::After(vec![])));
    seated(b.app.add(0, 2, &builtin(2, UnitKind::Reverb), &Place::After(vec![1])));
    seated(b.player_add(0, 9, UnitKind::Ring, 0));
    b.remove(2);
    assert_eq!(b.ids(0), vec![9, 1]);
    seated(b.app.add(0, 3, &builtin(3, UnitKind::Echo), &Place::After(vec![1, 2])));
    assert_eq!(b.ids(0), vec![9, 1, 3], "after 1, the nearest predecessor left");
    seated(b.app.add(0, 4, &builtin(4, UnitKind::Reverb), &Place::End));
    assert_eq!(b.ids(0), vec![9, 1, 3, 4], "End is the slot's end");
}

// ── the match (D9) ───────────────────────────────────────────────────────────────────────────────────

/// A live, applied plate of that slot under the id, of that kind and plugin identity, is `Matched`
/// (its own token, nothing added); the same id as another plugin (another id in the same file, or the
/// same plugin id in another file), another kind or in the other slot refuses (`Host`, no token).
#[test]
fn matched_by_identity_not_id() {
    let b = Bench::new(fast_device());
    let token = seated(b.app.add(0, 5, &amp(5, "amp"), &Place::After(vec![])));
    assert_eq!(b.app.add(0, 5, &amp(5, "amp"), &Place::End), Ok(AddOk { state: AddState::Matched, token }));
    assert_eq!(host_refusal(b.app.add(0, 5, &amp_at(5, "amp", "other"), &Place::End)).1, None, "the same file, another plugin id in it");
    assert_eq!(host_refusal(b.app.add(0, 5, &amp_at(5, "other", "amp"), &Place::End)).1, None, "the same plugin id, another file");
    assert_eq!(host_refusal(b.app.add(1, 5, &amp(5, "amp"), &Place::End)).1, None, "the id in the other slot");
    let echo = seated(b.app.add(0, 6, &builtin(6, UnitKind::Echo), &Place::End));
    assert_eq!(host_refusal(b.app.add(0, 6, &builtin(6, UnitKind::Reverb), &Place::End)).1, None, "another kind under the id");
    assert_eq!(b.app.add(0, 6, &builtin(6, UnitKind::Echo), &Place::End), Ok(AddOk { state: AddState::Matched, token: echo }));
    assert_eq!(b.ids(0), vec![5, 6], "nothing added by a match or a refusal");
}

/// A plate that has not applied yet (its add may still fail) and a `Leaving` one are no match: the add
/// is refused with no token. A failed add's plate is no match either: the add replaces it, a new
/// attempt with its own token. The `Leaving` plate is a SYNTH at its slot's head whose removal a take
/// holds (it changes the slot's role, so the engine answers it `Waiting` until the take ends).
#[test]
fn leaving_or_failed_plate_is_no_match() {
    let b = Bench::new(TestDevice::start(48_000, 64, Duration::from_millis(5), |_| 0.0));
    let host = b.host();
    let _loading = host.unit(0, 7, Spec::Builtin { kind: UnitKind::Echo, params: Vec::new() }, 0, false).expect("reserve 7");
    assert_eq!(host_refusal(b.app.add(0, 7, &builtin(7, UnitKind::Echo), &Place::End)), (Retry::Launch, None), "not applied yet");

    *b.fail.lock().unwrap() = Some("its setup failed".into());
    let failed = match b.app.add(0, 8, &amp(8, "amp"), &Place::End) {
        Err(AddFail::Plugin { token, .. }) => token,
        other => panic!("expected the plugin's own failure, got {other:?}"),
    };
    let again = seated(b.app.add(0, 8, &amp(8, "amp"), &Place::End));
    assert_ne!(again, failed, "a new attempt replaces the failed plate");

    seated(b.app.add(1, 10, &builtin(10, UnitKind::Synth), &Place::End));
    send(host, Command::SetBpm(240.0));
    send(host, Command::SetFixedLength(false));
    send(host, Command::RecDub(0));
    wait_until("the take records", || {
        let (_, ov) = host.drain_feed(&mut Vec::new());
        ov.is_some_and(|ov| ov.lane(0).state == LaneState::Recording && ov.lane(0).frames > 0)
    });
    let Removal::Sent(op) = host.remove_unit(10).expect("remove") else { panic!("expected a sent removal") };
    wait_until("the take holds the removal", || {
        settle(host);
        host.core.state.lock().unwrap().poll(op) == OpState::Held
    });
    {
        let state = host.core.state.lock().unwrap();
        assert!(state.model.find_plate(10).is_some_and(|(_, p)| p.state == PlateState::Leaving && p.applied), "an applied plate, leaving");
    }
    assert_eq!(host_refusal(b.app.add(1, 10, &builtin(10, UnitKind::Synth), &Place::End)).1, None, "a leaving plate");
}

// ── the player's source (D9) ─────────────────────────────────────────────────────────────────────────

/// A source the player put in heads the slot: a restore add that comes out a source (a SYNTH, a plugin
/// that activates as an instrument) is `SourceTaken`, and the player's source stays, applied. The
/// player's own add keeps replacing (`chain_cmds_tests`).
#[test]
fn restore_never_replaces_players_source() {
    let cfg = FakePluginConfig { source: true, ..FakePluginConfig::default() };
    let b = Bench::with(fast_device(), Chains::new(), cfg, None);
    seated(b.player_add(0, 9, UnitKind::Synth, 0));
    match b.app.add(0, 7, &builtin(7, UnitKind::Synth), &Place::After(vec![])) {
        Err(AddFail::SourceTaken { .. }) => {}
        other => panic!("a restore SYNTH under the player's: expected SourceTaken, got {other:?}"),
    }
    match b.app.add(0, 11, &amp(11, "instrument"), &Place::After(vec![])) {
        Err(AddFail::SourceTaken { .. }) => {}
        other => panic!("a restore instrument plugin under the player's SYNTH: expected SourceTaken, got {other:?}"),
    }
    assert_eq!(b.ids(0).first(), Some(&9), "the player's SYNTH heads the slot");
    assert!(b.applied(9), "the player's SYNTH stays applied");
    assert!(!b.applied(7) && !b.applied(11), "neither restore source applied");
}

/// A source the player put in that a rebuild evicted and that has not reseated yet: its row is live,
/// and no projected seat shows it (`Model::head_source_id`). A restore SYNTH added meanwhile is
/// `SourceTaken` (applied, it would take that source out: `Model::take_older_sources_out_of_engine`),
/// and the player's source reseats, applied.
#[test]
fn restore_never_replaces_a_source_awaiting_its_reseat() {
    let b = Bench::new(fast_device());
    let host = b.host().clone();
    let player = host.unit(0, 9, Spec::Plugin(plugin_ref("player")), 0, false).expect("reserve the player's source");
    player.activated(Activated { name: "player".into(), source: true, latency: 0 });
    let terms = player.terms().expect("engine terms");
    let op = player.submit(Box::new(Thru { source: true, latency: 0 }), terms).map_err(|(_, e)| e).expect("submit");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    b.device.rebuild_at(44_100, 256);
    {
        let state = host.core.state.lock().unwrap();
        let (_, plate) = state.model.find_plate(9).expect("its plate stays");
        assert!(!plate.seated_in_engine && plate.reseating_since.is_some(), "it awaits its reseat");
        assert_eq!(state.model.head_source_id(0, None), None, "no projected seat shows it");
    }
    match b.app.add(0, 7, &builtin(7, UnitKind::Synth), &Place::After(vec![])) {
        Err(AddFail::SourceTaken { .. }) => {}
        other => panic!("a restore SYNTH while the player's source awaits its reseat: expected SourceTaken, got {other:?}"),
    }
    assert_eq!(b.ids(0).first(), Some(&9), "the player's source keeps its plate at the head");
    assert!(!b.applied(7), "the restore's SYNTH never applied");
    let Some(Back::Evicted { unit, .. }) = player.take_back() else { panic!("the rebuild handed the player's source back") };
    let terms = player.terms().expect("the new engine's terms");
    let op = player.submit(unit, terms).map_err(|(_, e)| e).expect("reseat");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    assert!(b.applied(9), "the player's source reseats");
}

/// The player's source, back from a rebuild, re-activated as an effect with a latency past the most a
/// slot compensates, queues its reseat in that role: the projection shows an effect at the head, and
/// its row has an op queued. It keeps the source role until an install in another role applies, so a
/// restore SYNTH added meanwhile is `SourceTaken`. The engine refuses the reseat (`Latency`) and the
/// player's source keeps its plate: had the SYNTH queued behind the reseat, it would have applied at
/// the head and taken the source out as replaced (`Model::take_older_sources_out_of_engine`). Once
/// its reseat as an effect applies, it is no source: a restore SYNTH heads the slot ahead of it.
#[test]
fn restore_never_replaces_a_source_whose_reseat_as_an_effect_is_refused() {
    let mut b = Bench::new(fast_device());
    let host = b.host().clone();
    let player = host.unit(0, 9, Spec::Plugin(plugin_ref("player")), 0, false).expect("reserve the player's source");
    player.activated(Activated { name: "player".into(), source: true, latency: 0 });
    let terms = player.terms().expect("engine terms");
    let op = player.submit(Box::new(Thru { source: true, latency: 0 }), terms).map_err(|(_, e)| e).expect("submit");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    b.device.stop();
    b.device.rebuild_at(48_000, 256);
    {
        // As a running device: what is pushed waits in its port until the device resumes.
        let _rt = host.core.rt.lock().unwrap();
        host.core.running.store(true, Release);
    }
    let Some(Back::Evicted { .. }) = player.take_back() else { panic!("the rebuild handed the player's source back") };
    let past = max_chain_latency(48_000) + 1;
    player.activated(Activated { name: "player".into(), source: false, latency: past });
    let terms = player.terms().expect("the new engine's terms");
    let reseat = player.submit(Box::new(Thru { source: false, latency: past }), terms).map_err(|(_, e)| e).expect("reseat as an effect");
    {
        let state = host.core.state.lock().unwrap();
        assert!(state.model.has_pending_install(0, 9), "its reseat is queued");
        assert_eq!(state.model.head_source_id(0, None), None, "the projection shows an effect at the head");
    }
    let app = b.app.clone();
    let adding = std::thread::spawn(move || app.add(0, 7, &builtin(7, UnitKind::Synth), &Place::After(vec![])));
    wait_until("the restore's SYNTH is answered or queued behind the reseat", || {
        adding.is_finished() || host.core.state.lock().unwrap().model.has_pending_install(0, 7)
    });
    b.device.resume();
    match adding.join().expect("the add's thread") {
        Err(AddFail::SourceTaken { .. }) => {}
        other => panic!("a restore SYNTH while the player's source reseats as an effect: expected SourceTaken, got {other:?}"),
    }
    assert_eq!(host.wait_op(reseat, PATIENCE), Ok(OpState::Refused(SeatRefusal::Latency)), "the engine refuses the reseat");
    assert!(b.ids(0).contains(&9), "the player's source keeps its plate");
    assert!(!b.applied(7), "the restore's SYNTH never applied");

    let Some(Back::Refused { .. }) = player.take_back() else { panic!("the refused reseat's unit came back") };
    player.activated(Activated { name: "player".into(), source: false, latency: 0 });
    let terms = player.terms().expect("engine terms");
    let op = player.submit(Box::new(Thru { source: false, latency: 0 }), terms).map_err(|(_, e)| e).expect("reseat as an effect");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    seated(b.app.add(0, 7, &builtin(7, UnitKind::Synth), &Place::After(vec![])));
    assert_eq!(b.ids(0).first(), Some(&7), "the restore's SYNTH heads the slot");
    assert!(b.applied(7) && b.applied(9), "the player's unit, an effect now, stays behind it");
}

// ── the seed and its barrier (D1, D2) ────────────────────────────────────────────────────────────────

/// D2: a slot whose gain the player set (a gesture) keeps it; the other takes the record's level; a
/// second seed changes neither. The four input sends go off with it. The seed is taken by the running
/// engine.
#[test]
fn seed_skips_a_slot_gain_set_by_gesture() {
    let b = Bench::new(fast_device());
    send(b.host(), Command::SetSlotGain(0, 0.3));
    assert!(!sends_off(b.host()), "no send-off before the seed");
    assert!(b.app.seed([0.8, 0.6]), "the ring takes the seed whole");
    assert_eq!(gains(b.host()), [Some(0.3), Some(0.6)]);
    assert!(sends_off(b.host()), "the four send-offs go with the levels");
    assert!(b.app.seed([0.9, 0.9]));
    assert_eq!(gains(b.host()), [Some(0.3), Some(0.6)], "both held now");
    assert!(matches!(b.app.wait_taken(PATIENCE), SeedTaken::Taken { .. }));
}

/// C18: the ring fills midway through the seed (its first level taken, the rest refused): the seed is
/// not taken whole (sent again); a gesture meanwhile on either slot wins over the retry.
#[test]
fn c18_partial_take_on_full_ring_retry_keeps_gesture() {
    let mut b = Bench::new(fast_device());
    b.device.stop();
    let free = b.host().core.ends.lock().unwrap().as_ref().expect("an engine").commands.slots();
    let fill: Vec<TimedCommand> = (0..free - 1).map(|_| TimedCommand { frame: None, command: Command::SetBpm(120.0) }).collect();
    b.host().send_all(fill).expect("the ring takes the fill");
    assert!(!b.app.seed([0.8, 0.6]), "the ring took one level and refused the rest");
    assert_eq!(gains(b.host()), [Some(0.8), None], "the part it took is kept");
    assert!(!sends_off(b.host()), "the send-offs were refused with the second level");
    b.device.resume();
    wait_until("the ring drains", || b.host().core.ends.lock().unwrap().as_ref().unwrap().commands.slots() >= free);
    send(b.host(), Command::SetSlotGain(0, 0.3));
    send(b.host(), Command::SetSlotGain(1, 0.25));
    assert!(b.app.seed([0.8, 0.6]), "the retry is taken whole");
    assert_eq!(gains(b.host()), [Some(0.3), Some(0.25)], "the gestures win over the retry");
    assert!(sends_off(b.host()), "the retry's send-offs");
    assert!(matches!(b.app.wait_taken(PATIENCE), SeedTaken::Taken { .. }));
}

/// D1, D2: a seed sent while no engine exists (before a device's first open) is kept by the settings
/// memory (the levels and the four send-offs) with no mark; the first engine's replay carries it, and
/// the barrier, set at that first engine, answers `Taken` once its blocks took the replay.
#[test]
fn seed_with_no_engine_is_replayed_by_the_first_engine() {
    let mut b = Bench::new(fast_device());
    b.device.stop();
    // No engine yet: the engine, its ring and its slot ports go (none holds a unit).
    let old = b.host().core.rt.lock().unwrap().engine.take();
    *b.host().core.ends.lock().unwrap() = None;
    for port in &b.host().core.ports {
        *port.lock().unwrap() = None;
    }
    drop(old);
    assert_eq!(b.app.device(), None);
    assert!(b.app.seed([0.8, 0.6]), "kept for the first engine");
    assert_eq!(gains(b.host()), [Some(0.8), Some(0.6)]);
    assert!(sends_off(b.host()));
    assert_eq!(b.app.wait_taken(Duration::from_millis(20)), SeedTaken::Pending, "no engine takes it");
    b.device.rebuild_at(48_000, 256);
    let g = b.host().core.engine_gen.load(Acquire);
    let replayed = b.host().core.ends.lock().unwrap().as_ref().expect("the first engine").pushed;
    assert!(replayed >= 6, "the first engine's replay carries the two levels and the four send-offs ({replayed})");
    b.device.resume();
    assert_eq!(b.app.device(), Some(g));
    assert_eq!(b.app.wait_taken(PATIENCE), SeedTaken::Taken { gen: g });
    let taken = b.host().core.ends.lock().unwrap().as_ref().unwrap().overview.commands_taken();
    assert!(taken >= replayed, "the first engine took its whole replay");
    assert_eq!(gains(b.host()), [Some(0.8), Some(0.6)]);
    assert!(sends_off(b.host()));
}

/// D1: the barrier is the engine generation and the ring's count through the seed; a rebuild moves
/// the generation, the wait answers `Moved` at once and re-targets to the new ring (whose replay
/// carries the seed), and `Taken` comes from the new engine once a device runs it.
#[test]
fn barrier_retargets_on_rebuild() {
    let mut b = Bench::new(fast_device());
    let g0 = b.app.device().expect("a device runs");
    assert!(b.app.seed([0.8, 0.6]));
    assert_eq!(b.app.wait_taken(PATIENCE), SeedTaken::Taken { gen: g0 });
    b.device.stop();
    assert_eq!(b.app.device(), None, "no device runs");
    assert!(b.app.seed([0.8, 0.6]), "sent again (the levels held, the send-offs again)");
    b.device.rebuild_at(48_000, 256);
    let g1 = b.host().core.engine_gen.load(Acquire);
    assert_ne!(g1, g0);
    assert_eq!(b.app.wait_taken(Duration::from_millis(20)), SeedTaken::Moved { gen: g1 });
    assert_eq!(b.app.wait_taken(Duration::from_millis(20)), SeedTaken::Pending, "no device runs the new engine");
    b.device.resume();
    assert_eq!(b.app.device(), Some(g1));
    assert_eq!(b.app.wait_taken(PATIENCE), SeedTaken::Taken { gen: g1 });
}

/// D1's open question, settled: a new engine's first block takes at most `MAX_PENDING` (64) commands
/// off its ring, then services the slot ports, so an install already in a port seats in that block
/// while the rest of a longer replay and the seed behind it wait for the next ones. The replay here is
/// the settings memory's (each lane's mix off its defaults, which the old engine applied), more than
/// 64 commands. The barrier sees it: `Pending` after that block, `Taken` later. So the restore
/// re-checks the barrier before each add, and an install already queued across a rebuild is the
/// residual window (`OPEN-THREADS.md`). The device stays stopped after the rebuild and the test renders
/// the new engine's blocks itself, one at a time ([`render_block`]): the first block's reads are made
/// before any second block renders.
#[test]
fn first_block_can_seat_an_install_ahead_of_a_long_replay() {
    let mut b = Bench::new(fast_device());
    let host = b.host().clone();
    let mut long = Vec::new();
    for lane in 0..TRACK_COUNT as u8 {
        long.extend([Command::SetVolume(lane, 0.5), Command::SetPan(lane, 0.5), Command::SetDubFeedback(lane, 0.5), Command::SetMute(lane, true)]);
        for kind in FxKind::ALL {
            long.push(Command::SetFxBypass(lane, kind, false));
            for def in kind.params() {
                let param = FxParam::from_key(kind, def.key).expect("a listed param");
                long.push(Command::SetFxParam(lane, param, if def.min != def.default { def.min } else { def.max }));
            }
        }
    }
    host.send_all(long.into_iter().map(|command| TimedCommand { frame: None, command })).expect("the ring takes them");
    let sent = host.core.ends.lock().unwrap().as_ref().expect("an engine").pushed;
    wait_until("the old engine takes them", || host.core.ends.lock().unwrap().as_ref().unwrap().overview.commands_taken() >= sent);
    assert!(b.device.wait_blocks(1), "and applies them");
    settle(&host);
    b.device.stop();
    b.device.rebuild_at(48_000, 256);
    let replayed = host.core.ends.lock().unwrap().as_ref().expect("the new engine").pushed;
    assert!(replayed > 64, "the new engine's replay is longer than one block takes ({replayed})");
    assert!(b.app.seed([0.8, 0.6]));
    let uh = host.unit(0, 1, Spec::Plugin(plugin_ref("ahead")), 0, false).expect("reserve");
    uh.activated(Activated { name: "ahead".into(), source: false, latency: 0 });
    {
        // As a running device whose blocks this test renders: the install waits in the port for a
        // block (no idle service).
        let _rt = host.core.rt.lock().unwrap();
        host.core.running.store(true, Release);
    }
    let terms = uh.terms().expect("engine terms");
    let op = uh.submit(Box::new(Thru { source: false, latency: 0 }), terms).map_err(|(_, e)| e).expect("submit");
    // The stopped device's next frame: each of its blocks moved the frame by 256 from 0.
    let mut frame = b.device.blocks.load(Relaxed) as Frame * 256;
    render_block(&host, frame);
    assert_eq!(b.app.wait_taken(Duration::ZERO), SeedTaken::Pending, "the seed is still in the ring after the first block");
    assert!(b.applied(1), "the install seated in the first block, ahead of the seed");
    let mut rendered = 1;
    while !matches!(b.app.wait_taken(Duration::ZERO), SeedTaken::Taken { .. }) {
        assert!(rendered < 8, "the replay and the seed are taken within a few blocks");
        frame += 256;
        render_block(&host, frame);
        rendered += 1;
    }
    assert!(rendered > 1, "the seed took a later block than the install");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    // As the device's stop: the bench's teardown services the ports idle.
    host.core.running.store(false, Release);
    host.core.clock.clear();
}

// ── forget and closing ───────────────────────────────────────────────────────────────────────────────

/// `forget` deletes a unit's tone files only while no unit holds its id (decided under the id's
/// reservation), and once the host is closing it answers `Held` (the shutdown keeps each removed
/// unit's tone for the next launch).
#[test]
fn forget_is_held_while_a_unit_holds_the_id_and_once_closing() {
    let folder = Folder::new("host-forget");
    let tones = folder.0.join("tones");
    std::fs::create_dir_all(&tones).unwrap();
    let files_of = |id: UnitId| [tones.join(ToneIdentity::unit_file_name(id)), tones.join(ToneIdentity::unit_pin_name(id))];
    for id in [4, 5] {
        for f in files_of(id) {
            std::fs::write(f, b"tone").unwrap();
        }
    }
    let b = Bench::with(fast_device(), Chains::new(), FakePluginConfig::default(), Some(ToneStore::new(tones.clone())));
    seated(b.app.add(0, 4, &builtin(4, UnitKind::Echo), &Place::End));
    assert_eq!(b.app.forget(4), Forgot::Held, "a unit holds 4");
    assert!(files_of(4).iter().all(|f| f.exists()), "nothing deleted");
    b.remove(4);
    wait_until("4's row is released", || b.app.forget(4) == Forgot::Deleted);
    assert!(files_of(4).iter().all(|f| !f.exists()), "4's files are gone");
    b.host().set_closing();
    assert_eq!(b.app.forget(5), Forgot::Held, "closing");
    assert!(files_of(5).iter().all(|f| f.exists()), "5's files kept for the next launch");
}

/// `forget` answers `Deleted` only once no file of the id is left: a tone file another handle holds
/// with no delete sharing (a sharing violation) is no delete, and it answers `Held`, so the restore asks
/// again; once the handle is closed, the next forget deletes it.
#[test]
fn forget_is_held_while_a_file_cannot_be_deleted() {
    use std::os::windows::fs::OpenOptionsExt;
    let folder = Folder::new("host-forget-locked");
    let tones = folder.0.join("tones");
    std::fs::create_dir_all(&tones).unwrap();
    let tone = tones.join(ToneIdentity::unit_file_name(6));
    let pin = tones.join(ToneIdentity::unit_pin_name(6));
    for f in [&tone, &pin] {
        std::fs::write(f, b"tone").unwrap();
    }
    let b = Bench::with(fast_device(), Chains::new(), FakePluginConfig::default(), Some(ToneStore::new(tones.clone())));
    let open = std::fs::OpenOptions::new().read(true).share_mode(0).open(&tone).expect("hold the tone file");
    assert_eq!(b.app.forget(6), Forgot::Held, "a file is left");
    assert!(tone.exists(), "the held file is left");
    drop(open);
    assert_eq!(b.app.forget(6), Forgot::Deleted);
    assert!(!tone.exists() && !pin.exists(), "6's files are gone");
}

/// An add is refused before the mint (`Host`, no token, nothing on the chain) while no device runs
/// (`Device`) and once the host is closing (`Launch`).
#[test]
fn add_refused_before_the_mint_without_a_device_and_once_closing() {
    let mut b = Bench::new(fast_device());
    b.device.stop();
    assert_eq!(host_refusal(b.app.add(0, 3, &builtin(3, UnitKind::Echo), &Place::End)), (Retry::Device, None));
    b.device.resume();
    b.host().set_closing();
    assert_eq!(host_refusal(b.app.add(0, 3, &builtin(3, UnitKind::Echo), &Place::End)), (Retry::Launch, None));
    assert!(b.ids(0).is_empty(), "no plate");
}

/// The shutdown marks the host closing while an add is on its way to `state` for its reservation (past
/// its first closing checks, its plugin slots' lock held: `Chains::mint`): the add reserves nothing
/// (`Host`, no token, no plate), so no unit escapes the shutdown's removal of the units it sees. The
/// host closes at the reservation's last step before the lock (`BEFORE_RESERVE_STATE`), so a closing
/// read made anywhere ahead of the lock misses it.
#[test]
fn add_waiting_on_state_as_the_host_closes_reserves_nothing() {
    let b = Bench::new(fast_device());
    let host = b.host().clone();
    BEFORE_RESERVE_STATE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || host.set_closing())));
    let added = b.app.add(0, 3, &builtin(3, UnitKind::Echo), &Place::End);
    assert!(BEFORE_RESERVE_STATE.with(|hook| hook.borrow().is_none()), "the reservation reached its lock");
    assert_eq!(host_refusal(added), (Retry::Launch, None), "refused with no token");
    assert!(b.ids(0).is_empty(), "no plate");
}

/// [`RestoreHost::exited`] over the chain host's latches: a live attempt's token reads not ended; a
/// token no add minted reads ended (the trait's rule), where `Chains::exited` reads it not ended.
#[test]
fn exited_reads_an_unknown_token_ended_and_a_live_attempt_not() {
    let b = Bench::new(fast_device());
    let token = seated(b.app.add(0, 5, &amp(5, "amp"), &Place::End));
    assert!(!b.app.exited(token), "its owner runs");
    let unknown = token + 1_000;
    assert!(b.app.exited(unknown), "a token no add minted reads ended");
    assert!(!b.chains.exited(unknown), "the chain host's own read differs: the adapter maps it");
}

// ── a whole restore through the rig ──────────────────────────────────────────────────────────────────

/// Built-ins as the clamping factory makes them, each build held until the test opens the gate.
struct Gated {
    inner: ClampingBuiltinFactory,
    entered: AtomicBool,
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gated {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl BuiltinFactory for Gated {
    fn build(&self, kind: UnitKind, params: &Params, terms: Terms) -> Result<(Unit, Params), String> {
        self.entered.store(true, Release);
        let deadline = Instant::now() + PATIENCE;
        let mut open = self.open.lock().unwrap();
        while !*open && Instant::now() < deadline {
            open = self.cv.wait_timeout(open, Duration::from_millis(50)).unwrap().0;
        }
        drop(open);
        self.inner.build(kind, params, terms)
    }
}

fn read(folder: &Folder) -> RigRecord {
    RigRecord::read(&folder.bytes(files())).expect("a readable record")
}

/// C1, C19, C24: while the restore's add is held, the player's level gesture reaches the record file
/// (every write reads the settings' gain), stays through the add and the close, and the next launch
/// (a fresh engine, its settings empty) answers it and seeds it.
#[test]
fn c1_c19_c24_file_holds_gesture_while_restore_held_and_after_restart() {
    let folder = Folder::new("host-gesture");
    let rec = record(vec![echo_unit(1)], Vec::new());
    folder.write(files(), &rec);
    {
        let gated = Arc::new(Gated { inner: ClampingBuiltinFactory::new(), entered: AtomicBool::new(false), open: Mutex::new(false), cv: Condvar::new() });
        let b = Bench::with(fast_device(), Chains::with_factory(gated.clone()), FakePluginConfig::default(), None);
        let rig = Rig::with_restore(Some(folder.0.clone()), files(), b.app.clone());
        assert_eq!(rig.open(&*b.app).status, RigStatus::Present);
        let t0 = Instant::now();
        wait_until("the restore's add is held in its build", || {
            assert!(rig.tick(&*b.app, t0), "the writer goes on");
            gated.entered.load(Acquire)
        });
        send(b.host(), Command::SetSlotGain(0, 0.3));
        // A level is a value change: written once quiet for `SAVE_QUIET` from the tick that saw it.
        let t1 = Instant::now();
        assert!(rig.tick(&*b.app, t1), "the writer goes on");
        let later = t1 + SAVE_QUIET + Duration::from_millis(100);
        wait_until("the gesture is in the file while the add is held", || {
            assert!(rig.tick(&*b.app, later), "the writer goes on");
            read(&folder).slots[0].volume == f64::from(0.3f32)
        });
        assert!(!b.applied(1), "the add is still held");
        gated.release();
        wait_until("the restore's unit is in", || {
            assert!(rig.tick(&*b.app, later), "the writer goes on");
            b.applied(1)
        });
        rig.close(&*b.app);
        let after = read(&folder);
        assert_eq!(after.slots[0].volume, f64::from(0.3f32), "the close keeps the gesture");
        assert_eq!(folder.ids(0), vec![1]);
    }
    let b = Bench::new(fast_device());
    assert_eq!(gains(b.host()), [None, None], "a new process: no level held");
    let rig = Rig::with_restore(Some(folder.0.clone()), files(), b.app.clone());
    let answer = rig.open(&*b.app);
    assert_eq!(answer.volumes, Some([f64::from(0.3f32), 1.0]), "the next launch answers the gesture's level");
    assert_eq!(gains(b.host()), [Some(0.3), Some(1.0)], "and seeds it");
    let t0 = Instant::now();
    wait_until("the unit is restored", || {
        assert!(rig.tick(&*b.app, t0), "the writer goes on");
        b.applied(1)
    });
    rig.close(&*b.app);
}
