//! The rig record's glue on the real chain model (CP2b): its files and lock, the writer's tick over each
//! transient plate state, the close (D7), the import (D12, D13), and the tone files' two outcomes through
//! the real teardown. Each test drives a [`TestDevice`]'s engine host (or a [`Chains`] over it), ticks the
//! writer by hand and reads the record file back. Until CP3a wires the restore, the plan is the record as
//! found, its units held at their place or matched to an applied plate (`held_plan`). No feed thread runs
//! here: a close with a record to write waits its bound for a feed pass (`FEED_PASS_WAIT`) unless a test
//! drains as the feed would.

use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{Command, LaneState, SlotEvent, TimedCommand, UnitCx, UnitId, UnitInfo, UnitKind, UnitProcessor};
use lf_rig::{
    Files, PluginRecord, PluginRefView, RigRecord, RigStatus, RigView, SlotRecord, UnitBody, UnitRecord, Version1, DEV_FILES,
    IMPORT_STALE, RELEASE_FILES,
};

use super::{files, lock, writer, Inner, Rig};
use crate::engine_io::chain::{PluginRef, UnitSpec};
use crate::engine_io::chain_cmds::tests::{empty_slots, ClampingBuiltinFactory};
use crate::engine_io::chain_cmds::{Added, Chains};
use crate::engine_io::test_rig::{TestDevice, PATIENCE};
use crate::engine_io::unit_host::{Activated, Back, OpState, Removal, Spec, UnitHost};
use crate::engine_io::EngineHost;
use crate::host::engine_slot::{EventSink, PluginFormat};
use crate::host::tone::{decode as decode_tone, encode as encode_tone, Tone, ToneBinding, ToneStore};
use crate::host::unit_owner::test_support::{spawn_fake_unit, FakePluginConfig};
use crate::host::PluginHostState;

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────────────

/// A fresh data folder under the system temp dir, removed on drop (declare it before the `Rig` over
/// it, so the rig and its lock file go first).
struct Folder(PathBuf);

impl Folder {
    fn new(tag: &str) -> Folder {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("bleeploop-rig-{tag}-{}-{}", std::process::id(), NEXT.fetch_add(1, Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp data folder");
        Folder(dir)
    }

    /// This build's rig over the folder (not opened).
    fn rig(&self) -> Rig {
        Rig::new(Some(self.0.clone()), files())
    }

    fn path(&self, files: Files) -> PathBuf {
        self.0.join(files.record)
    }

    fn write(&self, files: Files, record: &RigRecord) {
        std::fs::write(self.path(files), serde_json::to_vec(record).expect("a record serializes")).expect("write the record");
    }

    fn bytes(&self, files: Files) -> Vec<u8> {
        std::fs::read(self.path(files)).expect("the record file")
    }

    fn read(&self) -> RigRecord {
        RigRecord::read(&self.bytes(files())).expect("a readable record")
    }

    /// The record's unit ids in `slot`, in order.
    fn ids(&self, slot: usize) -> Vec<UnitId> {
        self.read().slots[slot].units.iter().map(|u| u.id).collect()
    }

    /// This build's rig over a record that holds `record`, opened (`present`).
    fn opened(&self, host: &EngineHost, record: &RigRecord) -> Rig {
        self.write(files(), record);
        let rig = self.rig();
        assert_eq!(rig.open(host).status, RigStatus::Present);
        rig
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn record(a: Vec<UnitRecord>, b: Vec<UnitRecord>) -> RigRecord {
    let slot = |units| SlotRecord { volume: 1.0, units, pending: Vec::new() };
    RigRecord { version: Version1, slots: [slot(a), slot(b)] }
}

fn empty() -> RigRecord {
    record(Vec::new(), Vec::new())
}

fn plugin(tag: &str) -> PluginRef {
    PluginRef { format: "clap".into(), path: format!("C:\\plugins\\{tag}.clap"), id: format!("rig.{tag}") }
}

fn plugin_unit(id: UnitId, plugin: &PluginRef) -> UnitRecord {
    let plugin = PluginRefView { format: plugin.format.clone(), path: plugin.path.clone(), id: plugin.id.clone() };
    UnitRecord { id, body: UnitBody::Plugin(PluginRecord { plugin, name: None }), bypass: Some(false) }
}

fn echo_unit(id: UnitId) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind: UnitKind::Echo, params: None }, bypass: None }
}

/// The plugin path a record unit names.
fn path_of(unit: &UnitRecord) -> &str {
    match &unit.body {
        UnitBody::Plugin(p) => &p.plugin.path,
        UnitBody::Builtin { .. } => panic!("unit {} is a built-in", unit.id),
    }
}

/// 256-frame blocks every millisecond.
fn fast_device() -> TestDevice {
    TestDevice::start(48_000, 256, Duration::from_millis(1), |_| 0.0)
}

/// 64-frame blocks every 5 ms: the 2-second take lasts about 7.5 s, long past what a test holds it.
fn slow_device() -> TestDevice {
    TestDevice::start(48_000, 64, Duration::from_millis(5), |_| 0.0)
}

fn start_take(device: &TestDevice) {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::SetBpm(240.0) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::SetFixedLength(false) }).unwrap();
    host.send(TimedCommand { frame: None, command: Command::RecDub(0) }).unwrap();
    wait_until("the take records", || {
        let (_, ov) = host.drain_feed(&mut Vec::new());
        ov.is_some_and(|ov| ov.lane(0).state == LaneState::Recording && ov.lane(0).frames > 0)
    });
}

fn stop_take(device: &TestDevice) {
    let host = device.host();
    host.send(TimedCommand { frame: None, command: Command::Stop(0) }).unwrap();
    wait_until("the take stops", || {
        let (_, ov) = host.drain_feed(&mut Vec::new());
        ov.is_some_and(|ov| ov.lane(0).state != LaneState::Recording)
    });
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The engine's events into the model (the feed's drain): the model moves only on a drain.
fn settle(host: &EngineHost) {
    host.drain_feed(&mut Vec::new());
}

/// Drain, then one writer tick now (a structure change is written at once).
fn tick(rig: &Rig, host: &EngineHost) {
    settle(host);
    assert!(rig.tick(host, Instant::now()), "the writer goes on");
}

fn view(host: &EngineHost) -> RigView {
    writer::view(host)
}

fn plate(host: &EngineHost, id: UnitId) -> lf_rig::PlateView {
    view(host).slots.iter().flatten().find(|p| p.id == id).cloned().unwrap_or_else(|| panic!("no plate {id}"))
}

/// A pass-through plugin stand-in with a latency.
struct StandIn(Frame);

impl UnitProcessor for StandIn {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Plugin, source: false, latency: self.0 }
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

/// Reserve `id` in slot 0 at `at` as `plugin`, activated with `latency` (nothing submitted).
fn reserve(host: &EngineHost, id: UnitId, plugin: &PluginRef, at: usize, latency: Frame) -> UnitHost {
    let uh = host.unit(0, id, Spec::Plugin(plugin.clone()), at, false).expect("reserve the unit");
    uh.activated(Activated { name: plugin.id.clone(), source: false, latency });
    uh
}

/// [`reserve`], then submit a stand-in and wait for it to apply.
fn seat(host: &EngineHost, id: UnitId, plugin: &PluginRef, at: usize, latency: Frame) -> UnitHost {
    let uh = reserve(host, id, plugin, at, latency);
    submit(host, &uh, Box::new(StandIn(latency)));
    uh
}

fn submit(host: &EngineHost, uh: &UnitHost, unit: Box<dyn UnitProcessor>) {
    let terms = uh.terms().expect("engine terms");
    let op = uh.submit(unit, terms).map_err(|(_, e)| e).expect("submit");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
}

fn take_back(uh: &UnitHost) -> Back {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(back) = uh.take_back() {
            return back;
        }
        assert!(Instant::now() < deadline, "unit {}'s box never came back", uh.id());
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Remove `id` and wait for the removal to apply.
fn remove(host: &EngineHost, id: UnitId) {
    let Removal::Sent(op) = host.remove_unit(id).expect("remove") else { panic!("unit {id}: expected a sent removal") };
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
}

/// Remove `id` during an open take and wait until the take holds the removal (its plate leaving).
fn remove_held(host: &EngineHost, id: UnitId) -> crate::engine_io::unit_host::Op {
    let Removal::Sent(op) = host.remove_unit(id).expect("remove") else { panic!("unit {id}: expected a sent removal") };
    wait_until("the take holds the removal", || {
        settle(host);
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, id).is_some() && st.model.slot_held(0)
    });
    op
}

// ── the files and the lock (D8) ──────────────────────────────────────────────────────────────────────

/// Only a missing file is no record: it answers `absent`, and nothing writes a first record but the
/// import. No data folder is `unavailable`. Text that is not a v1 record (garbage, a newer build's) and
/// a record that cannot be read at all are `unreadable`, kept byte for byte through ticks and the close.
#[test]
fn open_missing_absent_unreadable_kept() {
    let device = fast_device();
    let host = device.host();
    let _unit = seat(host, 1, &plugin("open"), 0, 0);

    let folder = Folder::new("absent");
    let rig = folder.rig();
    let answer = rig.open(host);
    assert_eq!((answer.status, answer.volumes), (RigStatus::Absent, None));
    tick(&rig, host);
    rig.close(host);
    assert!(!folder.path(files()).exists(), "no tick and no close writes a first record");

    let rig = Rig::new(None, files());
    assert_eq!(rig.open(host).status, RigStatus::Unavailable, "no data folder");
    assert!(!rig.tick(host, Instant::now()), "the writer stops: this process never writes the record");

    for (tag, text) in [("garbage", &b"not a rig record"[..]), ("newer", &br#"{"version":2,"slots":[{"volume":1,"units":[]},{"volume":1,"units":[]}]}"#[..])] {
        let folder = Folder::new(tag);
        std::fs::write(folder.path(files()), text).unwrap();
        let rig = folder.rig();
        let answer = rig.open(host);
        assert_eq!((answer.status, answer.volumes), (RigStatus::Unreadable, None), "{tag}");
        assert!(!rig.tick(host, Instant::now()), "{tag}: the writer stops");
        rig.close(host);
        assert_eq!(folder.bytes(files()), text, "{tag}: kept byte for byte");
    }

    // A record file that exists but cannot be read (here a folder in its place) is unreadable too.
    let folder = Folder::new("unread");
    std::fs::create_dir_all(folder.path(files())).unwrap();
    assert_eq!(folder.rig().open(host).status, RigStatus::Unreadable, "a read that failed is no missing file");
}

/// One process owns the file pair: a second rig on the folder answers `unavailable` and writes nothing;
/// once the first closes (its lock file released), a new rig opens the record the first wrote.
#[test]
fn second_rig_on_the_folder_is_unavailable() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("second");
    let first = folder.opened(host, &empty());
    let second = folder.rig();
    assert_eq!(second.open(host).status, RigStatus::Unavailable);

    let _unit = seat(host, 2, &plugin("second"), 0, 0);
    assert!(!second.tick(host, Instant::now()), "the second rig never writes");
    assert_eq!(folder.ids(0), Vec::<UnitId>::new());
    tick(&first, host);
    assert_eq!(folder.ids(0), vec![2]);

    first.close(host);
    assert_eq!(folder.rig().open(host).status, RigStatus::Present, "the close released the lock file");
}

/// A DEV build keeps its own file pair and lock beside the release's in the shared data folder
/// (Settled 6): both open on one folder, and each writes only its own record.
#[test]
fn dev_and_release_file_names() {
    assert_eq!(files(), if cfg!(debug_assertions) { DEV_FILES } else { RELEASE_FILES });
    assert_eq!((DEV_FILES.record, DEV_FILES.marker, DEV_FILES.lock), ("rig.dev.json", "rig-inflight.dev.json", "rig.dev.lock"));
    assert_eq!((RELEASE_FILES.record, RELEASE_FILES.marker, RELEASE_FILES.lock), ("rig.json", "rig-inflight.json", "rig.lock"));

    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("kinds");
    folder.write(DEV_FILES, &empty());
    folder.write(RELEASE_FILES, &empty());
    let release_before = folder.bytes(RELEASE_FILES);
    let dev = Rig::new(Some(folder.0.clone()), DEV_FILES);
    let release = Rig::new(Some(folder.0.clone()), RELEASE_FILES);
    assert_eq!(dev.open(host).status, RigStatus::Present);
    assert_eq!(release.open(host).status, RigStatus::Present, "the release's lock is its own");

    let _unit = seat(host, 3, &plugin("kinds"), 0, 0);
    tick(&dev, host);
    let written = RigRecord::read(&folder.bytes(DEV_FILES)).unwrap();
    assert_eq!(written.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![3]);
    dev.close(host);
    release.close(host);
    assert_eq!(folder.bytes(RELEASE_FILES), release_before, "the DEV rig never touched the release's record");
}

// ── one per transient plate state (D4: a unit enters once applied, and stays until it is gone) ──────

/// A first install held by a take (`Waiting`) has not applied: it is not recorded until the take ends
/// and it applies.
#[test]
fn a_first_install_waiting_on_a_take_is_recorded_once_it_applies() {
    let device = slow_device();
    let host = device.host();
    let folder = Folder::new("waiting");
    let rig = folder.opened(host, &empty());

    start_take(&device);
    // Latency 64 changes the chain's: the take holds the install.
    let uh = reserve(host, 11, &plugin("waiting"), 0, 64);
    let terms = uh.terms().unwrap();
    let op = uh.submit(Box::new(StandIn(64)), terms).map_err(|(_, e)| e).expect("submit");
    wait_until("the take holds the install", || {
        settle(host);
        uh.poll(op) == OpState::Held
    });
    let held = plate(host, 11);
    assert_eq!((held.state, held.applied), (lf_rig::PlateState::Waiting, false));
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "a waiting first install is not recorded");

    stop_take(&device);
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![11], "recorded once it applied");
    uh.release();
}

/// An install a rebuild finds never applied (reserved, its box not yet submitted) is reseating in the
/// new engine but still not recorded; it is once it applies there.
#[test]
fn a_rebuild_of_a_never_applied_install_leaves_it_unrecorded() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("never");
    let rig = folder.opened(host, &empty());

    let uh = reserve(host, 20, &plugin("never"), 0, 0);
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new());
    device.rebuild_at(44_100, 256);
    tick(&rig, host);
    assert!(!plate(host, 20).applied, "the rebuild applied nothing of it");
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "a never-applied install stays out across the rebuild");

    submit(host, &uh, Box::new(StandIn(0)));
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![20]);
    uh.release();
}

/// An applied unit a rebuild evicts stays recorded while it reseats; so does one the old engine applied
/// only at the rebuild's handoff (its idle service, the model settling it as applied: the second place
/// an install applies).
#[test]
fn a_rebuild_of_an_applied_install_keeps_it_recorded() {
    let mut device = fast_device();
    let host = device.host().clone();
    let folder = Folder::new("applied");
    let rig = folder.opened(&host, &empty());

    let a = seat(&host, 30, &plugin("applied-a"), 0, 0);
    tick(&rig, &host);
    assert_eq!(folder.ids(0), vec![30]);

    // B is queued on the port while nothing renders: not applied, not recorded.
    device.stop();
    let b = reserve(&host, 31, &plugin("applied-b"), 1, 0);
    let terms = b.terms().unwrap();
    let op_b = b.submit(Box::new(StandIn(0)), terms).map_err(|(_, e)| e).expect("submit B");
    assert!(rig.tick(&host, Instant::now()));
    assert_eq!(folder.ids(0), vec![30]);

    device.rebuild_at(44_100, 256);
    assert_eq!(b.poll(op_b), OpState::Applied, "the handoff's idle service applied B before the eviction");
    tick(&rig, &host);
    assert!(plate(&host, 30).applied && plate(&host, 31).applied);
    assert_eq!(folder.ids(0), vec![30, 31], "both stay recorded while they reseat");

    device.resume();
    for uh in [&a, &b] {
        let Back::Evicted { unit, .. } = take_back(uh) else { panic!("unit {}: expected its evicted box", uh.id()) };
        submit(&host, uh, unit);
    }
    tick(&rig, &host);
    assert_eq!(folder.ids(0), vec![30, 31]);
    a.release();
    b.release();
}

/// A withdrawn unit (its owner re-activating it) stays recorded until it reseats.
#[test]
fn a_withdrawn_unit_stays_recorded_until_it_reseats() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("withdrawn");
    let rig = folder.opened(host, &empty());

    let uh = seat(host, 40, &plugin("withdrawn"), 0, 0);
    tick(&rig, host);
    let op = uh.withdraw().expect("withdraw");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    let Back::Withdrawn { unit, .. } = take_back(&uh) else { panic!("expected the withdrawn box") };
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![40], "a withdrawn unit stays recorded");

    submit(host, &uh, unit);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![40]);
    uh.release();
}

/// A parked unit (its reseat refused, the seat kept) stays recorded until the player removes it.
#[test]
fn a_parked_unit_stays_recorded_until_its_removal() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("parked");
    let rig = folder.opened(host, &empty());

    let uh = seat(host, 45, &plugin("parked"), 0, 0);
    tick(&rig, host);
    let op = uh.withdraw().expect("withdraw");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    assert!(matches!(take_back(&uh), Back::Withdrawn { .. }));
    uh.parked("reseat refused".into());
    let parked = plate(host, 45);
    assert_eq!((parked.state, parked.failed_add), (lf_rig::PlateState::Failed, false));
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![45], "a parked unit stays recorded");

    remove(host, 45);
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new());
    assert!(matches!(take_back(&uh), Back::Dismissed));
    uh.release();
}

/// A removal a take holds (the plate `Leaving`) keeps the unit recorded: the engine refuses a removal
/// only as `Unknown`, which settles as applied, so the hold ends in the removal and the unit leaves the
/// record then.
#[test]
fn a_held_removal_stays_recorded_until_it_applies() {
    let device = slow_device();
    let host = device.host();
    let folder = Folder::new("held");
    let rig = folder.opened(host, &empty());

    let uh = seat(host, 50, &plugin("held"), 0, 64);
    tick(&rig, host);
    start_take(&device);
    let op = remove_held(host, 50);
    assert_eq!(plate(host, 50).state, lf_rig::PlateState::Leaving);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![50], "held: still recorded");

    stop_take(&device);
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "applied: gone");
    assert!(matches!(take_back(&uh), Back::Removed(_)));
    uh.release();
}

/// A same-id reload in flight keeps the seated plugin's identity in the record; the reloaded plugin's
/// replaces it once its install applies.
#[test]
fn a_reload_in_flight_keeps_the_seated_plugin_recorded() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("reload");
    let rig = folder.opened(host, &empty());
    let (v1, v2) = (plugin("reload-v1"), plugin("reload-v2"));

    let old = seat(host, 60, &v1, 0, 0);
    tick(&rig, host);
    let cand = host.reload_unit(60, v2.clone()).expect("a reload candidate");
    cand.activated(Activated { name: v2.id.clone(), source: false, latency: 0 });
    tick(&rig, host);
    assert!(view(host).slots[0][0].plugin.as_ref().is_some_and(|p| p.path == v1.path), "the plate is still v1's");
    assert_eq!(path_of(&folder.read().slots[0].units[0]), v1.path, "in flight: the seated plugin is recorded");

    submit(host, &cand, Box::new(StandIn(0)));
    tick(&rig, host);
    assert_eq!(path_of(&folder.read().slots[0].units[0]), v2.path, "applied: the reloaded plugin");
    assert!(matches!(take_back(&old), Back::Superseded(_)));
    old.release();
    cand.release();
}

/// Each plate carries the install token of the add that made it: a unit added again under a freed id
/// is a new plate with a new token (`PlateView::added_by`).
#[test]
fn a_plate_names_the_add_that_made_it() {
    let device = fast_device();
    let host = device.host();
    let first = seat(host, 70, &plugin("token"), 0, 0);
    let first_token = first.install_token();
    assert_eq!(plate(host, 70).added_by, first_token);

    remove(host, 70);
    assert!(matches!(take_back(&first), Back::Removed(_)));
    first.release();
    wait_until("id 70 is free", || !host.core.state.lock().unwrap().table.is_id_reserved(70));
    let again = seat(host, 70, &plugin("token"), 0, 0);
    assert_ne!(again.install_token(), first_token);
    assert_eq!(plate(host, 70).added_by, again.install_token(), "the new add's token, not the old plate's");
    again.release();
}

/// A record unit the player's unit won (an applied plate under its id) leaves the plan, so the player's
/// later removal of it is not undone by the plan writing it back at its place.
#[test]
fn a_record_unit_the_player_removed_is_not_written_back() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("ended");
    let amp = plugin("ended");
    let rig = folder.opened(host, &record(vec![plugin_unit(95, &amp)], Vec::new()));
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![95], "held at its place while the chain lacks it");

    let uh = seat(host, 95, &amp, 0, 0);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![95]);
    remove(host, 95);
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "the player's removal stands");
    assert!(matches!(take_back(&uh), Back::Removed(_)));
    uh.release();
}

// ── the close (D7) ───────────────────────────────────────────────────────────────────────────────────

/// The close takes the chain and the level as they stand, past the last tick: a unit removed, a fader
/// gesture, and a removal the take still holds (its plate `Leaving` is dropped: the shutdown retires its
/// tone). Nothing is written after it.
#[test]
fn close_takes_an_edit_made_just_before_it() {
    let device = slow_device();
    let host = device.host();
    let folder = Folder::new("close-edit");
    let rig = folder.opened(host, &empty());

    let a = seat(host, 80, &plugin("close-a"), 0, 0);
    let c = seat(host, 82, &plugin("close-c"), 1, 0);
    let b = seat(host, 81, &plugin("close-b"), 2, 64);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![80, 82, 81]);

    // After the last tick: A removed, the fader moved, B's removal held by a take.
    remove(host, 80);
    host.send(TimedCommand { frame: None, command: Command::SetSlotGain(0, 0.5) }).unwrap();
    start_take(&device);
    let op_b = remove_held(host, 81);
    rig.close(host);
    let closed = folder.read();
    assert_eq!(closed.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![82]);
    assert_eq!((closed.slots[0].volume, closed.slots[1].volume), (0.5, 1.0), "the fader's level");
    assert!(!rig.tick(host, Instant::now()), "the writer stops at close");

    stop_take(&device);
    assert_eq!(host.wait_op(op_b, PATIENCE), Ok(OpState::Applied));
    assert_eq!(folder.read(), closed);
    for uh in [a, b] {
        assert!(matches!(take_back(&uh), Back::Removed(_)));
        uh.release();
    }
    c.release();
}

/// The shutdown's own removals (`unload_all`, after the close) write nothing: the file lists every unit
/// the chain held, a late open answers no record to write, and the next launch's rig reads it all.
#[test]
fn nothing_written_after_close() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("after-close");
    let rig = folder.opened(host, &empty());
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let chains = Chains::with_factory(Arc::new(ClampingBuiltinFactory::new()));
    let open = |_: PluginFormat, _: &PluginRef, _: UnitHost, _: EventSink, _: Option<ToneBinding>| Err("no plugins here".to_string());
    for (id, at) in [(90, 0), (91, 1)] {
        let spec = UnitSpec { kind: UnitKind::Echo, plugin: None, params: None, bypass: false };
        assert_eq!(chains.add(host, (&doc, epoch), &empty_slots(), None, 0, id, spec, at, None, noop_sink(), &open), Ok(Added::Seated));
    }
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![90, 91]);

    rig.close(host);
    let closed = folder.bytes(files());
    chains.unload_all(host);
    host.shutdown();
    chains.join_all();
    assert_ne!(rig.open(host).status, RigStatus::Present, "a late open opens nothing again");
    assert!(!rig.tick(host, Instant::now()));
    assert_eq!(folder.bytes(files()), closed, "nothing was written after the close");
    assert_eq!(folder.ids(0), vec![90, 91], "the file lists every unit");
    drop(rig);
    assert_eq!(folder.rig().open(host).status, RigStatus::Present, "the next launch opens it");
}

// ── the tone files' two outcomes through the real teardown (`remove_unit_at_shutdown`) ───────────────

fn noop_sink() -> EventSink {
    Arc::new(|_| {})
}

/// One plugin unit `id` in slot 0 of a [`Chains`] over `device`, with a tone store in the data folder's
/// `tones/`: it saved its own `unit-<id>.tone`, and the slot's file holds an older tone.
struct ToneUnit {
    doc: Arc<PluginHostState>,
    epoch: u32,
    chains: Arc<Chains>,
    tones: PathBuf,
    amp: PluginRef,
    id: UnitId,
    slot_before: Vec<u8>,
}

impl ToneUnit {
    fn add(host: &EngineHost, folder: &Folder, amp: PluginRef, id: UnitId, latency: Frame) -> ToneUnit {
        let doc = Arc::new(PluginHostState::default());
        let epoch = doc.begin_frontend_session();
        let chains = Arc::new(Chains::new());
        let tones = folder.0.join("tones");
        std::fs::create_dir_all(&tones).unwrap();
        let store = ToneStore::new(tones.clone());
        let slot_before = encode_tone(&Tone { identity: amp.clone(), name: "Amp".into(), state: b"an-older-slot-tone".to_vec() });
        std::fs::write(tones.join(amp.file_name(0)), &slot_before).unwrap();
        let state_bytes = format!("{}-{id}-dialed-in", amp.id).into_bytes();
        let open = |_: PluginFormat, _: &PluginRef, uh: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            spawn_fake_unit(uh, sink, tone, FakePluginConfig { latency, state_bytes: state_bytes.clone(), ..Default::default() })
        };
        let spec = UnitSpec { kind: UnitKind::Plugin, plugin: Some(amp.clone()), params: None, bypass: false };
        let added = chains.add(host, (&doc, epoch), &empty_slots(), Some(&store), 0, id, spec, 0, None, noop_sink(), &open);
        assert_eq!(added, Ok(Added::Seated));
        assert!(chains.tone_take(host, (&doc, epoch), id).is_ok());
        let unit = ToneUnit { doc, epoch, chains, tones, amp, id, slot_before };
        assert!(unit.unit_file().exists(), "unit {id} saved its own tone");
        unit
    }

    fn unit_file(&self) -> PathBuf {
        self.tones.join(format!("unit-{}.tone", self.id))
    }

    fn slot_file(&self) -> Vec<u8> {
        std::fs::read(self.tones.join(self.amp.file_name(0))).unwrap()
    }

    fn own_state(&self) -> Vec<u8> {
        format!("{}-{}-dialed-in", self.amp.id, self.id).into_bytes()
    }

    /// The shutdown's teardown after the rig's close (`mode.rs`): unload, the device's close, the joins.
    fn teardown(&self, host: &EngineHost) {
        self.chains.unload_all(host);
        host.shutdown();
        self.chains.join_all();
    }
}

/// A unit the chain holds at shutdown (as the restore's add, or the player's, leaves it) stays in the
/// record the close writes, and the shutdown's removal keeps its `unit-<id>.tone` where it is, the
/// slot's file as it was: the next launch restores it under its id with its own tone.
#[test]
fn restored_unit_keeps_its_tone_through_teardown() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("tone-keep");
    let rig = folder.opened(host, &empty());
    let unit = ToneUnit::add(host, &folder, plugin("tone-keep"), 110, 0);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![110]);

    rig.close(host);
    unit.teardown(host);
    assert_eq!(folder.ids(0), vec![110], "the record keeps the unit");
    let kept = std::fs::read(unit.unit_file()).expect("the shutdown keeps unit-110.tone");
    assert_eq!(decode_tone(&kept).unwrap().state, unit.own_state());
    assert_eq!(unit.slot_file(), unit.slot_before, "the slot file unchanged");
}

/// A record unit, re-added under its id, that the player removes while a take holds the removal: the
/// close drops its leaving plate (the plan had let it go once the plate applied), and the shutdown
/// finds the player's removal under way, so the tone retires into the slot's file once the take ends.
/// The record and the tone files agree: no unit to restore, its tone the slot's.
#[test]
fn player_removed_unit_retires_through_teardown() {
    let device = slow_device();
    let host = device.host();
    let folder = Folder::new("tone-retire");
    let amp = plugin("tone-retire");
    let rig = folder.opened(host, &record(vec![plugin_unit(120, &amp)], Vec::new()));
    // Latency 64: its removal is an edit a take holds.
    let unit = ToneUnit::add(host, &folder, amp, 120, 64);
    tick(&rig, host);
    assert_eq!(folder.ids(0), vec![120]);

    start_take(&device);
    let removal = {
        let (chains, host, doc, epoch) = (unit.chains.clone(), host.clone(), unit.doc.clone(), unit.epoch);
        std::thread::spawn(move || chains.remove(&host, (&doc, epoch), 120))
    };
    wait_until("the take holds the player's removal", || {
        let st = host.core.state.lock().unwrap();
        st.model.pending_remove_op(0, 120).is_some() && st.model.slot_held(0)
    });
    rig.close(host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "the close drops the leaving unit");

    unit.chains.unload_all(host);
    assert!(unit.unit_file().exists(), "nothing retires while the removal is held");
    stop_take(&device);
    wait_until("the player's removal retires the tone", || !unit.unit_file().exists());
    assert_eq!(decode_tone(&unit.slot_file()).unwrap().state, unit.own_state(), "the slot file holds the unit's tone");
    let _ = removal.join();
    unit.teardown(host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new());
}

// ── the import (D12, D13) ────────────────────────────────────────────────────────────────────────────

fn import_text(record: &RigRecord) -> String {
    serde_json::to_string(record).unwrap()
}

/// A replaced page's import is refused `stale` and stores nothing; the current page's is stored.
#[test]
fn import_from_a_replaced_page_is_stale() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("stale");
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let old = doc.begin_frontend_session();
    let current = doc.begin_frontend_session();
    let text = import_text(&record(vec![echo_unit(5)], Vec::new()));

    assert_eq!(rig.import(host, (&doc, old), &text), Err(IMPORT_STALE.to_string()));
    assert!(!folder.path(files()).exists(), "nothing stored");
    assert_eq!(rig.open(host).status, RigStatus::Absent);
    let answer = rig.import(host, (&doc, current), &text).expect("the current page's import");
    assert_eq!(answer.status, RigStatus::Present);
    assert_eq!(folder.ids(0), vec![5]);
}

/// While a record exists the import stores nothing and answers `present`, without reading the import:
/// text that is no record, or one whose id the chain holds, changes nothing.
#[test]
fn import_over_a_present_record_answers_present() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("present");
    folder.write(files(), &record(vec![echo_unit(7)], Vec::new()));
    let before = folder.bytes(files());
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();

    let answer = rig.import(host, (&doc, epoch), "not a rig record").expect("answered, not refused");
    assert_eq!((answer.status, answer.volumes), (RigStatus::Present, Some([1.0, 1.0])));
    let _unit = seat(host, 8, &plugin("present"), 0, 0);
    let colliding = import_text(&record(vec![echo_unit(8)], Vec::new()));
    assert_eq!(rig.import(host, (&doc, epoch), &colliding).map(|a| a.status), Ok(RigStatus::Present));
    assert_eq!(folder.bytes(files()), before, "nothing stored over the record");
}

/// DEV adoption (D12): a slot whose chain holds applied units takes them, their ids and order, in place
/// of the import's, keeping the import's `pending`; a slot the chain holds nothing applied in keeps the
/// import's units. Each slot's volume per D2.
#[test]
fn import_adopts_the_chains_applied_units() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("adopt");
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let amp = plugin("adopt");
    let _seated = seat(host, 130, &amp, 0, 0);
    let _unapplied = reserve(host, 131, &plugin("adopt-loading"), 1, 0);

    let mut import = record(vec![echo_unit(201)], vec![echo_unit(202)]);
    import.slots[0].pending = vec![echo_unit(203)];
    import.slots[0].volume = 0.8;
    let answer = rig.import(host, (&doc, epoch), &import_text(&import)).expect("stored");
    assert_eq!((answer.status, answer.volumes), (RigStatus::Present, Some([0.8, 1.0])));

    let stored = folder.read();
    assert_eq!(stored.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![130], "the chain's applied unit, not the import's");
    assert_eq!(path_of(&stored.slots[0].units[0]), amp.path);
    assert_eq!(stored.slots[0].pending.iter().map(|u| u.id).collect::<Vec<_>>(), vec![203], "the import's pending kept");
    assert_eq!(stored.slots[1].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![202], "a never-applied plate adopts nothing");
    assert_eq!(stored.slots[0].volume, 0.8);
}

/// An import id a plate of the chain holds refuses the import: nothing is stored, and the next open
/// still answers `absent`.
#[test]
fn import_refuses_an_id_the_chain_holds() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("id-held");
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let _unit = seat(host, 140, &plugin("id-held"), 0, 0);

    let import = record(vec![echo_unit(141)], vec![echo_unit(140)]);
    assert!(rig.import(host, (&doc, epoch), &import_text(&import)).is_err());
    assert!(!folder.path(files()).exists(), "nothing stored");
    assert_eq!(rig.open(host).status, RigStatus::Absent);
}

// ── the plan's judgement at the adoption's view and at close's full view (D3, D7) ───────────────────

/// A DEV import's adopted unit is matched to its plate, so the player's removal of it before the
/// writer's next tick ends it: the record does not write it back.
#[test]
fn an_adopted_unit_the_player_removes_before_a_tick_is_not_written_back() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("adopt-removed");
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let uh = seat(host, 150, &plugin("adopt-removed"), 0, 0);
    rig.import(host, (&doc, epoch), &import_text(&record(vec![echo_unit(250)], Vec::new()))).expect("stored");
    assert_eq!(folder.ids(0), vec![150], "adopted");

    remove(host, 150);
    tick(&rig, host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "the player's removal stands");
    assert!(matches!(take_back(&uh), Back::Removed(_)));
    uh.release();
}

/// A record unit the player added again under its id and then removed while a take holds the removal
/// (its plate `Leaving`), with no tick between: close judges the plan at its full view first (the
/// player's applied plate wins the id), then drops the leaving plate, so the unit is not written back.
#[test]
fn a_record_unit_readded_then_leaving_at_close_is_not_written_back() {
    let device = slow_device();
    let host = device.host();
    let folder = Folder::new("leaving-close");
    let amp = plugin("leaving-close");
    let rig = folder.opened(host, &record(vec![plugin_unit(160, &amp)], Vec::new()));
    // Latency 64: its removal is an edit a take holds.
    let uh = seat(host, 160, &amp, 0, 64);
    start_take(&device);
    let op = remove_held(host, 160);
    rig.close(host);
    assert_eq!(folder.ids(0), Vec::<UnitId>::new(), "the player's removal stands");

    stop_take(&device);
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    assert!(matches!(take_back(&uh), Back::Removed(_)));
    uh.release();
}

// ── the close's order (D7) ───────────────────────────────────────────────────────────────────────────

/// Close decides under `disk` whether it writes: a record an import or a first open committed and has
/// not yet published to `inner` (that window, set here by hand) still takes close's last write.
#[test]
fn close_writes_a_record_committed_but_not_yet_published() {
    let device = fast_device();
    let host = device.host();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let imported = Folder::new("unpublished-import");
    let found = Folder::new("unpublished-open");
    let a = imported.rig();
    a.import(host, (&doc, epoch), &import_text(&record(vec![echo_unit(170)], Vec::new()))).expect("stored");
    let b = found.opened(host, &record(vec![echo_unit(171)], Vec::new()));
    for rig in [&a, &b] {
        *lock(&rig.inner) = Inner::default();
    }
    let _unit = seat(host, 172, &plugin("unpublished"), 0, 0);
    a.close(host);
    b.close(host);
    assert_eq!(imported.ids(0), vec![170, 172], "the import's record, then the unit applied after it");
    assert_eq!(found.ids(0), vec![171, 172], "the opened record, then the unit applied after it");
}

/// Close waits for one completed feed pass before it takes its copy: an install the engine applied,
/// its `Seat` still in the event ring, reaches the model first, so the record holds the unit.
#[test]
fn close_waits_for_a_feed_pass_to_take_an_applied_install() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("feed-pass");
    let rig = folder.opened(host, &empty());
    let uh = reserve(host, 180, &plugin("feed-pass"), 0, 0);
    let terms = uh.terms().expect("engine terms");
    let op = uh.submit(Box::new(StandIn(0)), terms).map_err(|(_, e)| e).expect("submit");
    // The engine applies it within a few blocks; nothing drains its `Seat` into the model.
    std::thread::sleep(Duration::from_millis(50));
    assert!(!plate(host, 180).applied, "the model has not heard the seat yet");

    let done = AtomicBool::new(false);
    let (rig_ref, done_ref) = (&rig, &done);
    std::thread::scope(|s| {
        // The feed, whose first pass comes well after close began.
        let feed_host = host.clone();
        s.spawn(move || {
            wait_until("close begins", || rig_ref.closing.load(Acquire));
            std::thread::sleep(Duration::from_millis(100));
            while !done_ref.load(Acquire) {
                settle(&feed_host);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        rig.close(host);
        done.store(true, Release);
    });
    assert_eq!(folder.ids(0), vec![180], "the applied install is in the record");
    assert_eq!(host.wait_op(op, PATIENCE), Ok(OpState::Applied));
    uh.release();
}

/// A second close (the updater's shutdown, then the exit's) returns only after the first has written
/// and released the lock file, and writes nothing of its own.
#[test]
fn a_second_close_returns_after_the_first_has_written() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("second-close");
    let rig = folder.opened(host, &empty());
    let _unit = seat(host, 190, &plugin("second-close"), 0, 0);
    let returned = AtomicBool::new(false);
    let (early, after) = std::thread::scope(|s| {
        // The first close stops at `inner`, past `closing`, while the test holds it.
        let inner = lock(&rig.inner);
        let first = s.spawn(|| rig.close(host));
        wait_until("the first close begins", || rig.closing.load(Acquire));
        let second = s.spawn(|| {
            rig.close(host);
            returned.store(true, Release);
            // What the second close leaves when it returns.
            let status = folder.rig().open(host).status;
            (folder.ids(0), status)
        });
        std::thread::sleep(Duration::from_millis(100));
        let early = returned.load(Acquire);
        drop(inner);
        first.join().unwrap();
        (early, second.join().unwrap())
    });
    assert!(!early, "the second close waits for the first");
    assert_eq!(after, (vec![190], RigStatus::Present), "the first had written and released the lock file");
}

/// Once close has begun nothing opens or reads the file: an open that comes before close's last step
/// answers `unavailable` and takes no lock file.
#[test]
fn an_open_after_close_began_opens_no_file() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("open-closing");
    folder.write(files(), &empty());
    let rig = folder.rig();
    let (status, opened, lock_file) = std::thread::scope(|s| {
        // Close stops at `inner` (if it gets there) while the test holds it, past `closing`.
        let inner = lock(&rig.inner);
        let close = s.spawn(|| rig.close(host));
        wait_until("close begins", || rig.closing.load(Acquire));
        let status = rig.opened().status;
        let opened = lock(&rig.disk).store.is_some();
        let lock_file = folder.0.join(files().lock).exists();
        drop(inner);
        close.join().unwrap();
        (status, opened, lock_file)
    });
    assert_eq!((status, opened, lock_file), (RigStatus::Unavailable, false, false));
}

/// The import checks its page's epoch again right before its commit: a `host_init` after the call's
/// own check (here while the import waits for the chains' copy) makes it `stale`, and nothing is stored.
#[test]
fn an_import_whose_page_was_replaced_before_its_commit_is_stale() {
    let device = fast_device();
    let host = device.host();
    let folder = Folder::new("stale-late");
    let rig = folder.rig();
    let doc = PluginHostState::default();
    let epoch = doc.begin_frontend_session();
    let text = import_text(&record(vec![echo_unit(200)], Vec::new()));
    let answer = std::thread::scope(|s| {
        let state = host.core.state.lock().unwrap();
        let import = s.spawn(|| rig.import(host, (&doc, epoch), &text));
        // Past its first check once the store is open; it then waits for `state` (the view).
        wait_until("the import opened the store", || lock(&rig.disk).store.is_some());
        doc.begin_frontend_session();
        drop(state);
        import.join().unwrap()
    });
    assert_eq!(answer, Err(IMPORT_STALE.to_string()));
    assert!(!folder.path(files()).exists(), "nothing stored");
}
