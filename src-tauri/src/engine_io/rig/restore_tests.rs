//! The restore's driver (CP3a, `restore.rs`) over a scripted [`RestoreHost`] and files on disk: what
//! a launch adds, under which ids and where, what the record and the crash marker hold after it, and
//! what the next launch makes of them. The fake host is the model too ([`RigModel`]): its plates are
//! what each view shows. Each test ticks the writer by hand at a fixed instant (`at`) and moves it on
//! only to run the restore's timers out (the settle); the blocking calls run on the driver's own thread
//! as in the app. A "crash" drops the `Rig` with no close; the next launch is a new `Rig` and a new
//! fake host on the same folder.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::InputSendParam;
use lf_rig::marker::{self, MarkerRead};
use lf_rig::{
    AddFail, AddOk, AddState, Disposition, InstallToken, Notice, Place, PlateState, PlateView, PluginRecord, PluginRefView, Retry, RigRecord,
    RigStatus, RigView, UnitBody, UnitId, UnitKind, UnitParam, UnitRecord, Volumes, SETTLE,
};

use super::tests::{echo_unit, record, wait_until, Folder};
use super::{files, lock, Forgot, RestoreHost, Rig, RigModel, SeedTaken};

// ── the fake host ───────────────────────────────────────────────────────────────────────────────────

/// How the fake host answers one add of a unit (the default: it seats, not a source).
#[derive(Clone, Copy, Debug)]
enum Script {
    /// Applied; `source` as the plugin's activation said.
    Seat { source: bool },
    /// A held install (a take holds the alignment): its plate `Waiting`, not applied.
    Waiting,
    /// No answer to trust (a setup timeout, a built-in that did not answer): its plate loading, its
    /// owner still running.
    Unresolved,
    /// Refused before any plugin code ran (no token); `Device` also stops the device (no engine).
    Host(Retry),
    /// The owner is in setup: the add blocks, its plate loading, until the test releases it; then it
    /// seats.
    Block,
    /// The add blocks with no plate until the test releases it; then the unit is in the slot, applied
    /// (a plate the host found under its lock), and the add answers `Matched` with its token.
    Appears,
}

#[derive(Default)]
struct State {
    /// The model's revision, bumped at every change of the plates.
    rev: u64,
    slots: [Vec<PlateView>; 2],
    next_token: InstallToken,
    /// The running engine's generation; `None`: no device.
    device: Option<u64>,
    /// The engine has not taken the seed: every barrier wait is `Pending`.
    hold_taken: bool,
    /// The next barrier wait finds the generation moved here (a rebuild meanwhile).
    moved: Option<u64>,
    /// The plugins the scan finds, of those asked for.
    installed: Vec<PluginRefView>,
    scripts: BTreeMap<UnitId, VecDeque<Script>>,
    /// Every add, in order: its slot, id, place and unit.
    adds: Vec<(usize, UnitId, Place, UnitRecord)>,
    /// How many adds have answered (returned to the driver's call thread).
    answered: usize,
    seeds: Vec<[f64; 2]>,
    released: BTreeSet<UnitId>,
    release_all: bool,
    /// The next view releases this unit's add and waits for its answer, after taking its copy.
    release_on_view: Option<UnitId>,
    exited: BTreeSet<InstallToken>,
    forgot: Vec<UnitId>,
}

struct FakeHost {
    dir: PathBuf,
    state: Mutex<State>,
    cv: Condvar,
}

impl FakeHost {
    /// A host whose device runs (generation 1) and whose scan finds every plugin `record` names.
    fn new(folder: &Folder, record: &RigRecord) -> Arc<FakeHost> {
        let installed = record
            .slots
            .iter()
            .flat_map(|s| s.units.iter().chain(&s.pending))
            .filter_map(|u| match &u.body {
                UnitBody::Plugin(p) => Some(p.plugin.clone()),
                UnitBody::Builtin { .. } => None,
            })
            .collect();
        let state = State { next_token: 100, device: Some(1), installed, ..State::default() };
        Arc::new(FakeHost { dir: folder.0.clone(), state: Mutex::new(state), cv: Condvar::new() })
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn script(&self, id: UnitId, answers: &[Script]) {
        self.st().scripts.insert(id, answers.iter().copied().collect());
    }

    /// The plugin file of `unit` is gone: the scan no longer finds it.
    fn lose(&self, unit: &UnitRecord) {
        let UnitBody::Plugin(p) = &unit.body else { panic!("unit {} is a built-in", unit.id) };
        self.st().installed.retain(|i| *i != p.plugin);
    }

    fn release(&self, id: UnitId) {
        self.st().released.insert(id);
        self.cv.notify_all();
    }

    fn release_all(&self) {
        self.st().release_all = true;
        self.cv.notify_all();
    }

    fn set_device(&self, device: Option<u64>) {
        self.st().device = device;
    }

    fn adds(&self) -> Vec<UnitId> {
        self.st().adds.iter().map(|a| a.1).collect()
    }

    fn add_of(&self, id: UnitId) -> (usize, Place) {
        let st = self.st();
        st.adds.iter().find(|a| a.1 == id).map(|a| (a.0, a.2.clone())).unwrap_or_else(|| panic!("no add of {id}"))
    }

    fn answered(&self) -> usize {
        self.st().answered
    }

    fn plate(&self, id: UnitId) -> Option<PlateView> {
        self.st().slots.iter().flatten().find(|p| p.id == id).cloned()
    }

    fn applied(&self, id: UnitId) -> bool {
        self.plate(id).is_some_and(|p| p.applied)
    }

    fn token(&self, id: UnitId) -> InstallToken {
        self.plate(id).unwrap_or_else(|| panic!("no plate {id}")).added_by
    }

    /// Set plate `id`'s state as the model would (a late seat, a failure).
    fn set(&self, id: UnitId, state: PlateState) {
        let mut st = self.st();
        for p in st.slots.iter_mut().flatten().filter(|p| p.id == id) {
            p.state = state;
            p.applied |= state == PlateState::Seated;
            p.failed_add = state == PlateState::Failed;
        }
        st.rev += 1;
    }

    /// The owner of attempt `token` ended (its exit latch).
    fn exit(&self, token: InstallToken) {
        self.st().exited.insert(token);
    }
}

/// A record unit as the model's plate shows it: the record's name, values and bypass, so a projection of
/// it is the record's unit itself.
fn plate_of(unit: &UnitRecord, token: InstallToken, state: PlateState, applied: bool, source: bool) -> PlateView {
    let (kind, plugin, name, params) = match &unit.body {
        UnitBody::Plugin(p) => (UnitKind::Plugin, Some(p.plugin.clone()), p.name.clone().unwrap_or_default(), None),
        UnitBody::Builtin { kind, params } => (*kind, None, format!("{kind:?}"), params.clone()),
    };
    PlateView { id: unit.id, added_by: token, kind, plugin, name, params, bypass: unit.bypassed(), source, state, applied, failed_add: false }
}

/// `place` in `plates`, as the app's host resolves it: right after the nearest listed unit present,
/// else at the head; `End` at the end.
fn index(plates: &[PlateView], place: &Place) -> usize {
    match place {
        Place::End => plates.len(),
        Place::After(ids) => ids.iter().rev().find_map(|id| plates.iter().position(|p| p.id == *id)).map_or(0, |i| i + 1),
    }
}

fn put(st: &mut State, slot: usize, place: &Place, plate: PlateView) {
    let at = index(&st.slots[slot], place);
    st.slots[slot].insert(at, plate);
    st.rev += 1;
}

impl RigModel for FakeHost {
    fn gains(&self) -> Volumes {
        [None, None]
    }

    fn view(&self) -> RigView {
        let (view, release) = {
            let mut st = self.st();
            (RigView { rev: st.rev, slots: st.slots.clone() }, st.release_on_view.take())
        };
        // The copy is taken; the add answers now, before the caller goes on with that copy.
        if let Some(id) = release {
            let before = self.answered();
            self.release(id);
            let st = self.st();
            drop(self.cv.wait_while(st, |s| s.answered == before).unwrap_or_else(|e| e.into_inner()));
            // The call thread sends the answer right after the add returns.
            std::thread::sleep(Duration::from_millis(50));
        }
        view
    }
}

impl RestoreHost for FakeHost {
    fn seed(&self, volumes: [f64; 2]) -> bool {
        self.st().seeds.push(volumes);
        true
    }

    fn wait_taken(&self, _within: Duration) -> SeedTaken {
        let mut st = self.st();
        if let Some(gen) = st.moved.take() {
            st.device = Some(gen);
            return SeedTaken::Moved { gen };
        }
        match st.device {
            Some(gen) if !st.hold_taken => SeedTaken::Taken { gen },
            _ => SeedTaken::Pending,
        }
    }

    fn device(&self) -> Option<u64> {
        self.st().device
    }

    fn scan(&self, paths: &[String]) -> Vec<PluginRefView> {
        self.st().installed.iter().filter(|p| paths.contains(&p.path)).cloned().collect()
    }

    fn add(&self, slot: usize, id: UnitId, unit: &UnitRecord, place: &Place) -> Result<AddOk, AddFail> {
        let mut st = self.st();
        let script = st.scripts.get_mut(&id).and_then(VecDeque::pop_front).unwrap_or(Script::Seat { source: false });
        st.adds.push((slot, id, place.clone(), unit.clone()));
        let token = st.next_token;
        st.next_token += 1;
        // The add replaces a failed add's plate under its id.
        st.slots[slot].retain(|p| !(p.id == id && p.failed_add));
        let answer = match script {
            Script::Seat { source } => {
                put(&mut st, slot, place, plate_of(unit, token, PlateState::Seated, true, source));
                Ok(AddOk { state: AddState::Seated, token })
            }
            Script::Waiting => {
                put(&mut st, slot, place, plate_of(unit, token, PlateState::Waiting, false, false));
                Ok(AddOk { state: AddState::Waiting, token })
            }
            Script::Unresolved => {
                put(&mut st, slot, place, plate_of(unit, token, PlateState::Loading, false, false));
                Err(AddFail::Unresolved { why: "setup timed out".into(), token, exited: false })
            }
            Script::Host(retry) => {
                if retry == Retry::Device {
                    st.device = None;
                }
                Err(AddFail::Host { why: "refused".into(), retry, token: None })
            }
            Script::Block | Script::Appears => {
                let block = matches!(script, Script::Block);
                if block {
                    put(&mut st, slot, place, plate_of(unit, token, PlateState::Loading, false, false));
                }
                st = self.cv.wait_while(st, |s| !s.release_all && !s.released.contains(&id)).unwrap_or_else(|e| e.into_inner());
                if block {
                    for p in st.slots.iter_mut().flatten().filter(|p| p.added_by == token) {
                        (p.state, p.applied) = (PlateState::Seated, true);
                    }
                    st.rev += 1;
                    Ok(AddOk { state: AddState::Seated, token })
                } else {
                    put(&mut st, slot, place, plate_of(unit, token, PlateState::Seated, true, false));
                    Ok(AddOk { state: AddState::Matched, token })
                }
            }
        };
        st.answered += 1;
        self.cv.notify_all();
        answer
    }

    fn exited(&self, token: InstallToken) -> bool {
        self.st().exited.contains(&token)
    }

    /// Under the fake's one lock: the id's check and the deletion are one step.
    fn forget(&self, id: UnitId) -> Forgot {
        let mut st = self.st();
        if st.slots.iter().flatten().any(|p| p.id == id) {
            return Forgot::Held;
        }
        st.forgot.push(id);
        for name in [format!("unit-{id}.tone"), format!("unit-{id}.pin")] {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
        Forgot::Deleted
    }
}

// ── a launch ────────────────────────────────────────────────────────────────────────────────────────

/// One process: its rig, driving its own fake host, over the folder. Dropping it without
/// [`Launch::close`] is a crash (a blocked add is released into a rig that is gone).
struct Launch {
    fake: Arc<FakeHost>,
    rig: Rig,
}

impl Launch {
    fn new(folder: &Folder, record: &RigRecord) -> Launch {
        let fake = FakeHost::new(folder, record);
        let rig = Rig::with_restore(Some(folder.0.clone()), files(), fake.clone());
        Launch { fake, rig }
    }

    fn open(&self) -> Option<Notice> {
        let answer = self.rig.open(&*self.fake);
        assert_eq!(answer.status, RigStatus::Present);
        answer.notice
    }

    /// Writer ticks at `at` until `done`.
    fn until(&self, at: Instant, what: &str, mut done: impl FnMut() -> bool) {
        wait_until(what, || {
            assert!(self.rig.tick(&*self.fake, at), "the writer goes on");
            done()
        });
    }

    /// `n` writer ticks at `at`, a little apart, for what must not happen.
    fn turns(&self, at: Instant, n: usize) {
        for _ in 0..n {
            assert!(self.rig.tick(&*self.fake, at), "the writer goes on");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn disposition(&self, id: UnitId) -> Option<Disposition> {
        lock(&self.rig.inner).restore.plan().dispositions.get(&id).cloned()
    }

    fn close(&self) {
        self.rig.close(&*self.fake);
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.fake.release_all();
    }
}

// ── fixtures ────────────────────────────────────────────────────────────────────────────────────────

/// A plugin unit as the record keeps it, its scan name `tag`.
fn amp(id: UnitId, tag: &str) -> UnitRecord {
    let plugin = PluginRefView { format: "clap".into(), path: format!("C:\\plugins\\{tag}.clap"), id: format!("rig.{tag}") };
    UnitRecord { id, body: UnitBody::Plugin(PluginRecord { plugin, name: Some(tag.into()) }), bypass: Some(false) }
}

fn ott_on(id: UnitId) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind: UnitKind::Ott, params: None }, bypass: Some(false) }
}

/// The crash marker on disk: `None` when there is no file, else the ids it names.
fn on_disk(folder: &Folder) -> Option<Vec<UnitId>> {
    let bytes = std::fs::read(folder.0.join(files().marker)).ok()?;
    match marker::read(Some(&bytes)) {
        MarkerRead::None => Some(Vec::new()),
        MarkerRead::Names(ids) => Some(ids),
        MarkerRead::Unreadable => panic!("an unreadable marker"),
    }
}

fn write_marker(folder: &Folder, ids: &[UnitId]) {
    std::fs::write(folder.0.join(files().marker), marker::text(ids)).expect("write the marker");
}

fn tone(folder: &Folder, id: UnitId) -> PathBuf {
    folder.0.join(format!("unit-{id}.tone"))
}

fn write_tone(folder: &Folder, id: UnitId) {
    std::fs::write(tone(folder, id), format!("tone {id}")).expect("write a tone file");
}

// ── a launch's restore (C2, C4, C6, C7, C12, C15, C16) ──────────────────────────────────────────────

/// C2: each launch brings every record unit back under its id, at its place, with its values, and the
/// record file and the plugin's tone file come out of it byte for byte as they went in. The marker
/// names the plugin while its load settles, and goes after.
#[test]
fn restart_brings_same_ids_values_and_file() {
    let folder = Folder::new("restart");
    let mut echo = echo_unit(2);
    echo.body = UnitBody::Builtin { kind: UnitKind::Echo, params: Some(vec![(UnitParam::Send(InputSendParam::EchoTime), 0.25)]) };
    let mut rec = record(vec![amp(1, "restart"), echo], vec![echo_unit(3)]);
    rec.slots[0].volume = 0.8;
    folder.write(files(), &rec);
    write_tone(&folder, 1);
    let before = folder.bytes(files());

    for launch in 0..2 {
        let l = Launch::new(&folder, &rec);
        let t0 = Instant::now();
        l.open();
        l.until(t0, "every unit is in", || [1, 2, 3].iter().all(|id| l.fake.applied(*id)));
        let adds = l.fake.st().adds.clone();
        let want = [(0, 1, Place::After(vec![])), (0, 2, Place::After(vec![1])), (1, 3, Place::After(vec![]))];
        assert_eq!(adds.iter().map(|a| (a.0, a.1, a.2.clone())).collect::<Vec<_>>(), want, "launch {launch}: ids, slots, places");
        for (slot, id, _, unit) in &adds {
            assert_eq!(Some(unit), rec.slots[*slot].units.iter().find(|u| u.id == *id), "launch {launch}: unit {id} as recorded");
        }
        assert_eq!(l.fake.st().seeds, vec![[0.8, 1.0]], "launch {launch}: one seed, the record's levels");
        assert_eq!(on_disk(&folder), Some(vec![1]), "launch {launch}: the plugin's load settles");
        l.until(t0 + SETTLE, "the marker goes", || on_disk(&folder).is_none());
        l.close();
        assert_eq!(folder.bytes(files()), before, "launch {launch}: the record file as it was");
        assert!(tone(&folder, 1).exists(), "launch {launch}: the tone file kept");
    }
}

/// C4, C15: a reload while an add is in its plugin's setup, and again after its settle, starts
/// nothing: one seed, each unit added once.
#[test]
fn reload_under_load_and_settle_adds_nothing() {
    let folder = Folder::new("reload");
    let rec = record(vec![amp(1, "reload"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the plugin is in setup", || l.fake.adds() == [1] && on_disk(&folder) == Some(vec![1]));

    l.open();
    l.turns(t0, 20);
    assert_eq!(l.fake.adds(), [1], "a reload under load adds nothing");
    l.fake.release(1);
    l.until(t0, "both are in", || l.fake.applied(1) && l.fake.applied(2));
    l.until(t0 + SETTLE, "the load settles", || on_disk(&folder).is_none());
    l.open();
    l.turns(t0 + SETTLE, 20);
    assert_eq!(l.fake.adds(), [1, 2], "each unit once");
    assert_eq!(l.fake.st().seeds.len(), 1, "one seed");
    assert_eq!(folder.ids(0), vec![1, 2]);
}

/// C16's in-process half: an add with no answer to trust keeps its unit and the marker; its plate
/// seating later restores it, and the settle runs from that seat, not from the answer.
#[test]
fn unanswered_then_seated_settles_after_seat() {
    let folder = Folder::new("unanswered");
    let rec = record(vec![amp(1, "unanswered")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Unresolved]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "its answer is in", || l.disposition(1).is_some_and(|d| matches!(d, Disposition::Issued { token: Some(_) })));
    let t1 = t0 + 2 * SETTLE;
    l.turns(t1, 10);
    assert_eq!(on_disk(&folder), Some(vec![1]), "no settle without a seat");
    assert_eq!(folder.ids(0), vec![1], "kept at its place");

    l.fake.set(1, PlateState::Seated);
    l.until(t1, "restored", || l.disposition(1).is_some_and(|d| matches!(d, Disposition::Restored { .. })));
    l.turns(t1 + SETTLE - Duration::from_millis(1), 10);
    assert_eq!(on_disk(&folder), Some(vec![1]), "its settle runs from the seat");
    l.until(t1 + SETTLE, "the marker goes", || on_disk(&folder).is_none());
    assert_eq!(folder.ids(0), vec![1]);
}

/// A setup that timed out with its owner still in plugin code keeps the marker past a settle, even
/// once its plate failed; the owner's exit makes the load terminal: the marker goes, the unit is held
/// at its place, and the next unit went after it.
#[test]
fn setup_timeout_holds_marker_until_owner_exits() {
    let folder = Folder::new("timeout");
    let rec = record(vec![amp(1, "timeout"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Unresolved]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert_eq!(l.fake.add_of(2), (0, Place::After(vec![1])));

    l.fake.set(1, PlateState::Failed);
    let t1 = t0 + 2 * SETTLE;
    l.turns(t1, 10);
    assert_eq!(on_disk(&folder), Some(vec![1]), "its owner may still be in plugin code");
    l.fake.exit(l.fake.token(1));
    l.until(t1, "the marker goes", || on_disk(&folder).is_none());
    assert!(matches!(l.disposition(1), Some(Disposition::Held { .. })));
    assert_eq!(folder.ids(0), vec![1, 2], "held at its place");
}

/// C22: a built-in that did not answer keeps its place in the record, and the next unit goes after it;
/// the marker names no built-in.
#[test]
fn builtin_timeout_holds_place() {
    let folder = Folder::new("builtin-timeout");
    let rec = record(vec![echo_unit(1), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Unresolved]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert_eq!(l.fake.add_of(2), (0, Place::After(vec![1])), "after the unit that did not answer");
    l.turns(t0 + 2 * SETTLE, 10);
    assert_eq!(folder.ids(0), vec![1, 2], "held at its place");
    assert!(matches!(l.disposition(1), Some(Disposition::Issued { token: Some(_) })));
    assert_eq!(on_disk(&folder), Some(Vec::new()), "the marker names nothing");
}

/// C6: a plugin the scan does not find is held at its place, its tone file kept; the next launch whose
/// scan finds it adds it under the same id.
#[test]
fn missing_then_found_next_launch_same_id() {
    let folder = Folder::new("missing");
    let rec = record(vec![amp(1, "missing")], Vec::new());
    folder.write(files(), &rec);
    write_tone(&folder, 1);
    {
        let l = Launch::new(&folder, &rec);
        l.fake.lose(&rec.slots[0].units[0]);
        let t0 = Instant::now();
        l.open();
        l.until(t0, "held", || matches!(l.disposition(1), Some(Disposition::Held { .. })));
        l.turns(t0, 10);
        assert_eq!(l.fake.adds(), Vec::<UnitId>::new());
        l.close();
    }
    assert_eq!(folder.ids(0), vec![1], "kept in the record");
    assert!(tone(&folder, 1).exists(), "and its tone file");

    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "found and added", || l.fake.applied(1));
    assert_eq!(l.fake.add_of(1), (0, Place::After(vec![])), "under its id");
    assert_eq!(folder.ids(0), vec![1]);
}

/// C7: a slot's role is its applied head's: a plugin the record holds as an effect that activates as a
/// source places only the OTT (on) of the slot's `pending`, at the chain's end; the rest leaves.
#[test]
fn scan_effect_activates_as_source_places_by_source() {
    let folder = Folder::new("source");
    let mut rec = record(vec![amp(1, "keys")], Vec::new());
    rec.slots[0].pending = vec![ott_on(10), echo_unit(11)];
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Seat { source: true }]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the OTT is in", || l.fake.applied(10));
    l.turns(t0, 10);
    assert_eq!(l.fake.adds(), [1, 10], "the ECHO is not added");
    assert_eq!(l.fake.add_of(10), (0, Place::End));
    let stored = RigRecord::read(&folder.bytes(files())).expect("a record");
    assert_eq!(stored.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1, 10]);
    assert!(stored.slots[0].pending.is_empty(), "placed");
}

/// C12: with no device nothing is added; with one whose engine has not taken the seed (and then was
/// rebuilt) neither; a refusal for no engine holds the unit until a device runs again, and it is added
/// again under its id.
#[test]
fn no_engine_waits_for_device_then_retries() {
    let folder = Folder::new("no-engine");
    let rec = record(vec![amp(1, "no-engine")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Host(Retry::Device), Script::Seat { source: false }]);
    l.fake.set_device(None);
    let t0 = Instant::now();
    l.open();
    l.turns(t0, 20);
    assert_eq!(l.fake.adds(), Vec::<UnitId>::new(), "no device");

    {
        let mut st = l.fake.st();
        st.device = Some(1);
        st.hold_taken = true;
    }
    l.turns(t0, 20);
    assert_eq!(l.fake.adds(), Vec::<UnitId>::new(), "the seed not taken");
    {
        let mut st = l.fake.st();
        st.hold_taken = false;
        st.moved = Some(2);
    }
    l.until(t0, "the add, refused for no engine", || l.fake.answered() == 1);
    l.turns(t0, 20);
    assert_eq!(l.fake.adds(), [1], "held until a device runs");
    assert_eq!(folder.ids(0), vec![1]);

    l.fake.set_device(Some(3));
    l.until(t0, "added again", || l.fake.applied(1));
    assert_eq!(l.fake.adds(), [1, 1], "under its id");
}

// ── the crash marker (C5, C23, C26) ─────────────────────────────────────────────────────────────────

/// C5: a leftover marker's plugin is forgotten (the record without it, its tone file gone) and told:
/// every open answers the notice, also after a reply lost to a reload, until the page confirms it.
#[test]
fn notice_survives_a_lost_reply_until_seen() {
    let folder = Folder::new("notice");
    let rec = record(vec![amp(1, "crashed"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    write_marker(&folder, &[1]);
    write_tone(&folder, 1);
    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the echo is in", || l.fake.applied(2));
    assert_eq!(folder.ids(0), vec![2], "forgotten");
    assert!(!tone(&folder, 1).exists(), "its tone file gone");

    let notice = l.open().expect("the notice");
    assert_eq!((notice.forgotten.clone(), notice.plugins_held), (vec!["crashed".to_string()], false));
    assert_eq!(l.open(), Some(notice.clone()), "a reply lost to a reload: the next open answers it again");
    l.rig.seen(notice.id + 1);
    assert_eq!(l.open(), Some(notice.clone()), "another id confirms nothing");
    l.rig.seen(notice.id);
    assert_eq!(l.open(), None, "confirmed");
    assert_eq!(l.fake.adds(), [2]);
}

/// C26: a host refusal in A (no plugin code ran) leaves the marker before B loads, so a crash in B's
/// load forgets B alone at the next launch, and A is restored.
#[test]
fn c26_refuse_a_then_crash_b_reopen_forgets_b_only() {
    let folder = Folder::new("c26");
    let rec = record(vec![amp(1, "a"), amp(2, "b")], Vec::new());
    folder.write(files(), &rec);
    write_tone(&folder, 1);
    write_tone(&folder, 2);
    {
        let l = Launch::new(&folder, &rec);
        l.fake.script(1, &[Script::Host(Retry::Launch)]);
        l.fake.script(2, &[Script::Block]);
        let t0 = Instant::now();
        l.open();
        l.until(t0, "B in setup, the marker naming it alone", || l.fake.adds() == [1, 2] && on_disk(&folder) == Some(vec![2]));
        // The crash: no close.
    }

    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "A is in", || l.fake.applied(1));
    assert_eq!(l.fake.adds(), [1], "B is not loaded again");
    assert_eq!(l.fake.st().forgot, vec![2]);
    assert_eq!(folder.ids(0), vec![1]);
    assert!(!tone(&folder, 2).exists() && tone(&folder, 1).exists(), "B's tone file goes, A's stays");
    let notice = l.open().expect("told");
    assert_eq!(notice.forgotten, vec!["b".to_string()]);
    l.until(t0 + SETTLE, "the marker goes", || on_disk(&folder).is_none());
}

/// C23: the marker names nothing before the first add of any kind, so a crash while a built-in hangs
/// forgets nothing: the next launch restores the plugin after it.
#[test]
fn c23_builtin_hang_then_crash_restores_b() {
    let folder = Folder::new("c23");
    let rec = record(vec![echo_unit(1), amp(2, "b")], Vec::new());
    folder.write(files(), &rec);
    write_tone(&folder, 2);
    {
        let l = Launch::new(&folder, &rec);
        l.fake.script(1, &[Script::Block]);
        let t0 = Instant::now();
        l.open();
        l.until(t0, "the built-in hangs", || l.fake.adds() == [1] && on_disk(&folder) == Some(Vec::new()));
        l.turns(t0, 10);
        assert_eq!(l.fake.adds(), [1], "no plugin load while it hangs");
    }

    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    assert_eq!(l.open(), None, "nothing to tell");
    l.until(t0, "both are in", || l.fake.applied(1) && l.fake.applied(2));
    assert_eq!(l.fake.st().forgot, Vec::<UnitId>::new());
    assert!(tone(&folder, 2).exists());
    assert_eq!(folder.ids(0), vec![1, 2]);
}

/// The side lane: a load that settled leaves the marker by a rewrite while the next plugin's add is
/// still in its setup (the add blocks on its own thread, not the rig's).
#[test]
fn a_settled_load_leaves_the_marker_while_the_next_add_blocks() {
    let folder = Folder::new("side-lane");
    let rec = record(vec![amp(1, "settles"), amp(2, "blocks")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(2, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the second plugin in setup", || l.fake.adds() == [1, 2] && on_disk(&folder) == Some(vec![1, 2]));
    l.until(t0 + SETTLE, "the first leaves the marker", || on_disk(&folder) == Some(vec![2]));
    assert_eq!(l.fake.answered(), 1, "the second add still blocks");
}

/// The glue rule (Answer 1): a turn feeds an add's answer before it takes its view, and a view taken
/// before the answer came is never fed after it. Here the answer (a matched unit, whose plate is in no
/// earlier view) arrives while a turn takes its copy: fed after that older copy, the plate would read
/// as removed and the unit leave the record.
#[test]
fn a_view_taken_before_an_answer_is_never_fed_after_it() {
    let folder = Folder::new("view-order");
    let rec = record(vec![amp(1, "appears")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Appears]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add is out", || l.fake.adds() == [1]);
    l.fake.st().release_on_view = Some(1);
    l.until(t0, "matched", || {
        assert_eq!(folder.ids(0), vec![1], "the unit stays in the record");
        matches!(l.disposition(1), Some(Disposition::Matched { .. }))
    });
    assert_eq!(l.fake.answered(), 1);
    assert_eq!(folder.ids(0), vec![1]);
}

// ── the close (D7, SEAT-C) ──────────────────────────────────────────────────────────────────────────

/// SEAT-C: a clean close inside a load's settle removes the marker (the process lived through it).
#[test]
fn close_inside_a_settle_removes_the_marker() {
    let folder = Folder::new("close-settle");
    let rec = record(vec![amp(1, "quick")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "seated, settling", || l.fake.applied(1) && on_disk(&folder) == Some(vec![1]));
    l.close();
    assert_eq!(on_disk(&folder), None);
    assert_eq!(folder.ids(0), vec![1]);
}

/// A close while a plugin's owner is in setup keeps it in the marker and the record (and drops the
/// one settling beside it), without waiting for the add; its answer after the close writes nothing.
#[test]
fn close_while_owner_in_setup_keeps_marker() {
    let folder = Folder::new("close-setup");
    let rec = record(vec![amp(1, "settling"), amp(2, "setup")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(2, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the second plugin in setup", || l.fake.adds() == [1, 2] && on_disk(&folder) == Some(vec![1, 2]));
    l.close();
    assert_eq!(l.fake.answered(), 1, "close did not wait for the add");
    assert_eq!(on_disk(&folder), Some(vec![2]), "the load in setup stays, the settling one goes");
    assert_eq!(folder.ids(0), vec![1, 2]);

    let record_bytes = folder.bytes(files());
    l.fake.release(2);
    wait_until("the add answers", || l.fake.answered() == 2);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!l.rig.tick(&*l.fake, t0 + SETTLE), "the writer stops at close");
    assert_eq!(on_disk(&folder), Some(vec![2]));
    assert_eq!(folder.bytes(files()), record_bytes, "nothing written after close");
}

/// CP2b's: a close while a restore add is held (`Waiting`: a take holds its install) keeps its entry in
/// the record and its load in the marker, and drops the load settling beside it.
#[test]
fn close_while_a_restore_add_is_held_keeps_its_entry_and_marker() {
    let folder = Folder::new("close-held");
    let rec = record(vec![amp(1, "held"), amp(2, "next")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Waiting]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next is in", || l.fake.applied(2) && on_disk(&folder) == Some(vec![1, 2]));
    l.close();
    assert_eq!(on_disk(&folder), Some(vec![1]));
    assert_eq!(folder.ids(0), vec![1, 2], "the held unit keeps its entry at its place");
}
