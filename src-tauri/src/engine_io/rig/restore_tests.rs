//! The restore's driver (CP3a, `restore.rs`) over a scripted [`RestoreHost`] and files on disk: what
//! a launch adds, under which ids and where, what the record and the crash marker hold after it, and
//! what the next launch makes of them. The fake host is the model too ([`RigModel`]): its plates are
//! what each view shows. Each test ticks the writer by hand at a fixed instant (`at`) and moves it on
//! only to run the restore's timers out (the settle); the blocking calls run on the driver's own thread
//! as in the app. A "crash" drops the `Rig` with no close; the next launch is a new `Rig` and a new
//! fake host on the same folder.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use lf_engine::InputSendParam;
use lf_rig::marker::{self, MarkerRead};
use lf_rig::{
    Action, AddFail, AddOk, AddState, Disposition, InstallToken, Notice, Place, PlateState, PlateView, PluginRecord, PluginRefView, Retry, RigRecord,
    RigStatus, RigView, UnitBody, UnitId, UnitKind, UnitParam, UnitRecord, Volumes, SETTLE,
};

use super::tests::{echo_unit, record, wait_until, Folder};
use crate::engine_io::test_rig::PATIENCE;
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
    /// [`Script::Block`], and the plugin activates as a source.
    BlockSource,
    /// The add blocks with no plate until the test releases it; then a host refusal (no plugin code).
    HostLate,
    /// The plugin's own setup failed: its failed plate stays, its owner gone.
    PluginFails,
    /// The seat was refused after the plugin ran: its failed plate stays, its owner gone.
    SeatRefused,
    /// A source the player put in heads the slot: no plate, its owner gone.
    SourceTaken,
    /// The call's thread panics inside the add (after its start gate).
    Panic,
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
    /// How many times a forget was asked.
    forget_asks: usize,
    /// Ids a unit holds for the forget (its files are that unit's), whether or not a plate shows it.
    reserved: BTreeSet<UnitId>,
    /// The next this many seeds are refused (the ring full).
    refuse_seeds: usize,
    /// The next view takes this long after its copy (a slow turn).
    slow_view: Option<Duration>,
    /// Slot A's gain as the settings hold it.
    gain: Option<f64>,
    /// At the n-th gain read from now: slot A's gain moves (0.5), the record file is copied into
    /// `trapped` and replaced by a directory, so the next record write fails.
    trap: Option<usize>,
    trapped: Option<Vec<u8>>,
}

struct FakeHost {
    /// Itself: the driver's call thread holds one more reference while it runs.
    me: Weak<FakeHost>,
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
        Arc::new_cyclic(|me| FakeHost { me: me.clone(), dir: folder.0.clone(), state: Mutex::new(state), cv: Condvar::new() })
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

    fn forget_asks(&self) -> usize {
        self.st().forget_asks
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
        let mut st = self.st();
        if let Some(n) = st.trap.as_mut() {
            *n -= 1;
            if *n == 0 {
                st.trap = None;
                st.gain = Some(0.5);
                let path = self.dir.join(files().record);
                st.trapped = Some(std::fs::read(&path).expect("read the record"));
                std::fs::remove_file(&path).expect("remove the record");
                std::fs::create_dir(&path).expect("an obstacle at the record's path");
            }
        }
        [st.gain, None]
    }

    fn view(&self) -> RigView {
        let (view, release, slow) = {
            let mut st = self.st();
            (RigView { rev: st.rev, slots: st.slots.clone() }, st.release_on_view.take(), st.slow_view.take())
        };
        // The copy is taken; the add answers now, before the caller goes on with that copy: its call
        // thread ends once its answer is in the channel.
        if let Some(id) = release {
            self.release(id);
            wait_until("the add's thread ended", || self.me.strong_count() == 2);
        }
        if let Some(slow) = slow {
            std::thread::sleep(slow);
        }
        view
    }
}

impl RestoreHost for FakeHost {
    fn seed(&self, volumes: [f64; 2]) -> bool {
        let mut st = self.st();
        st.seeds.push(volumes);
        if st.refuse_seeds > 0 {
            st.refuse_seeds -= 1;
            return false;
        }
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
        if matches!(script, Script::Panic) {
            drop(st);
            panic!("the fake's add of unit {id} panics (a test's)");
        }
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
            Script::PluginFails | Script::SeatRefused => {
                let mut plate = plate_of(unit, token, PlateState::Failed, false, false);
                plate.failed_add = true;
                put(&mut st, slot, place, plate);
                let why = "failed".to_string();
                match script {
                    Script::PluginFails => Err(AddFail::Plugin { why, token, exited: true }),
                    _ => Err(AddFail::Seat { why, token, exited: true }),
                }
            }
            Script::SourceTaken => Err(AddFail::SourceTaken { why: "a source heads the slot".into(), token, exited: true }),
            Script::Block | Script::BlockSource | Script::Appears | Script::HostLate => {
                let block = matches!(script, Script::Block | Script::BlockSource);
                if block {
                    put(&mut st, slot, place, plate_of(unit, token, PlateState::Loading, false, false));
                }
                st = self.cv.wait_while(st, |s| !s.release_all && !s.released.contains(&id)).unwrap_or_else(|e| e.into_inner());
                match script {
                    Script::Block | Script::BlockSource => {
                        let source = matches!(script, Script::BlockSource);
                        for p in st.slots.iter_mut().flatten().filter(|p| p.added_by == token) {
                            (p.state, p.applied, p.source) = (PlateState::Seated, true, source);
                        }
                        st.rev += 1;
                        Ok(AddOk { state: AddState::Seated, token })
                    }
                    Script::Appears => {
                        put(&mut st, slot, place, plate_of(unit, token, PlateState::Seated, true, false));
                        Ok(AddOk { state: AddState::Matched, token })
                    }
                    _ => Err(AddFail::Host { why: "refused late".into(), retry: Retry::Launch, token: None }),
                }
            }
            Script::Panic => unreachable!("panicked above"),
        };
        st.answered += 1;
        self.cv.notify_all();
        answer
    }

    fn exited(&self, token: InstallToken) -> bool {
        self.st().exited.contains(&token)
    }

    /// Under the fake's one lock: the id's check and the deletion are one step. A failed add's plate
    /// holds no files.
    fn forget(&self, id: UnitId) -> Forgot {
        let mut st = self.st();
        st.forget_asks += 1;
        if st.reserved.contains(&id) || st.slots.iter().flatten().any(|p| p.id == id && !p.failed_add) {
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

    /// [`Launch::close`], which must not wait on the driver or an add: a close that does fails the
    /// test by timeout, after its blocked add is let go (so the suite does not hang).
    fn close_within(&self) {
        let (done, finished) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(move || {
                self.close();
                let _ = done.send(());
            });
            if finished.recv_timeout(PATIENCE).is_err() {
                self.fake.release_all();
                panic!("close waited on the driver or an add");
            }
        });
    }

    /// Wait until the driver's call thread has ended (its answer, if any, is in its channel): the
    /// fake is then held by the launch and the rig alone.
    fn call_ended(&self) {
        wait_until("the call's thread ended", || Arc::strong_count(&self.fake) == 2);
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.fake.release_all();
    }
}

/// How far past a timer's instant a test ticks to run it out: a timer counts from its turn's view,
/// which the real time the turn took moves past the tick's instant (r1, decision 5a).
const LATE: Duration = Duration::from_secs(1);

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
        l.until(t0 + SETTLE + LATE, "the marker goes", || on_disk(&folder).is_none());
        l.close();
        assert_eq!(folder.bytes(files()), before, "launch {launch}: the record file as it was");
        assert_eq!(std::fs::read_to_string(tone(&folder, 1)).expect("the tone file"), "tone 1", "launch {launch}: the tone file kept");
    }
}

/// C4, C15: a reload while an add is in its plugin's setup, again during its settle, and again after
/// it, starts nothing: one seed, each unit added once.
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
    assert_eq!(on_disk(&folder), Some(vec![1]), "in its settle");
    l.open();
    l.turns(t0, 20);
    assert_eq!(l.fake.adds(), [1, 2], "a reload during the settle adds nothing");
    l.until(t0 + SETTLE + LATE, "the load settles", || on_disk(&folder).is_none());
    l.open();
    l.turns(t0 + SETTLE + LATE, 20);
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
    l.until(t1 + SETTLE + LATE, "the marker goes", || on_disk(&folder).is_none());
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
    // The marker names plugins only: with nothing left to issue it goes, the built-in still unresolved.
    l.until(t0 + 2 * SETTLE, "the marker goes: it never names a built-in", || on_disk(&folder).is_none());
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
    l.until(t0 + SETTLE + LATE, "the marker goes", || on_disk(&folder).is_none());
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
    l.until(t0 + SETTLE + LATE, "the first leaves the marker", || on_disk(&folder) == Some(vec![2]));
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
    l.close_within();
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

// ── review round 1's decisions (r1 D2-D7) ───────────────────────────────────────────────────────────

/// A flag a test's hook sets and waits on, bounded.
#[derive(Default)]
struct Flag(Mutex<bool>, Condvar);

impl Flag {
    fn set(&self) {
        *lock(&self.0) = true;
        self.1.notify_all();
    }

    fn is_set(&self) -> bool {
        *lock(&self.0)
    }

    fn wait(&self) {
        let set = lock(&self.0);
        drop(self.1.wait_timeout_while(set, PATIENCE, |s| !*s).unwrap_or_else(|e| e.into_inner()));
    }
}

/// D2: no host call after close. A forget a unit held is asked again at each turn, but not by a tick
/// after close began: the unit's files stay.
#[test]
fn a_held_forget_is_not_asked_again_after_close() {
    let folder = Folder::new("forget-close");
    let rec = record(vec![amp(1, "crashed"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    write_marker(&folder, &[1]);
    write_tone(&folder, 1);
    let l = Launch::new(&folder, &rec);
    l.fake.st().reserved.insert(1);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the forget asked again", || l.fake.forget_asks() >= 2 && l.fake.applied(2));
    l.close();
    l.fake.st().reserved.clear();
    assert!(!l.rig.tick(&*l.fake, t0), "the writer stops at close");
    assert!(tone(&folder, 1).exists(), "its tone file stays");
    assert_eq!(l.fake.st().forgot, Vec::<UnitId>::new());
}

/// D3: close reads the exit latch already set of an attempt it follows. A setup that timed out, its
/// plate failed, its owner gone just before close: its load is terminal, and the marker does not keep it.
#[test]
fn close_reads_an_owner_exit_already_set() {
    let folder = Folder::new("close-latch");
    let rec = record(vec![amp(1, "timeout")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Unresolved]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "its answer is in", || l.disposition(1).is_some_and(|d| matches!(d, Disposition::Issued { token: Some(_) })));
    l.fake.set(1, PlateState::Failed);
    l.turns(t0, 5);
    assert_eq!(on_disk(&folder), Some(vec![1]), "its owner may still be in plugin code");
    l.fake.exit(l.fake.token(1));
    l.close_within();
    assert!(on_disk(&folder).unwrap_or_default().is_empty(), "the marker does not keep a load whose owner is gone");
    assert_eq!(folder.ids(0), vec![1], "held at its place");
}

/// D3: close reads an answer already in its channel. A host refusal (no plugin code ran) that came
/// after the last turn: its load is withdrawn, and the marker does not keep it.
#[test]
fn close_reads_an_answer_already_in() {
    let folder = Folder::new("close-answer");
    let rec = record(vec![amp(1, "late")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::HostLate]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add is out, named", || l.fake.adds() == [1] && on_disk(&folder) == Some(vec![1]));
    l.fake.release(1);
    l.call_ended();
    l.close_within();
    assert!(on_disk(&folder).unwrap_or_default().is_empty(), "the marker does not keep a refused load");
    assert!(matches!(l.disposition(1), Some(Disposition::Held { .. })));
    assert_eq!(folder.ids(0), vec![1], "held at its place");
}

/// D4: an add the restore issued and the driver never dispatched is cancelled by close and answered
/// as a host refusal: the unit is held at its place, not left issued.
#[test]
fn close_cancels_an_add_never_dispatched() {
    let folder = Folder::new("close-queued");
    let rec = record(vec![echo_unit(1), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the first add is out", || l.fake.adds() == [1]);
    l.fake.release(1);
    l.call_ended();
    // One turn's inputs and judgement, without its actions: the next add is queued, never run.
    let turn = l.rig.inputs(t0);
    let _ = l.rig.judge(turn, l.fake.gains(), l.fake.view(), false);
    assert!(lock(&l.rig.inner).actions.iter().any(|a| matches!(a, Action::Add { id: 2, .. })), "the next add is queued");
    l.close_within();
    assert!(matches!(l.disposition(2), Some(Disposition::Held { .. })), "{:?}", l.disposition(2));
    assert_eq!(l.fake.adds(), [1]);
    assert_eq!(folder.ids(0), vec![1, 2]);
}

/// D4: an add whose call thread has not passed its start gate when close begins never starts: close
/// holds the unit, and the thread calls nothing after. Needs the test hook (`Calls::before_gate`).
#[test]
fn close_cancels_an_add_before_its_gate() {
    let folder = Folder::new("close-gate");
    let rec = record(vec![echo_unit(1)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    let (entered, go) = (Arc::new(Flag::default()), Arc::new(Flag::default()));
    let (e, g) = (entered.clone(), go.clone());
    lock(&l.rig.calls).before_gate = Some(Arc::new(move || {
        e.set();
        g.wait();
    }));
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add's thread at its gate", || entered.is_set());
    l.close_within();
    assert!(matches!(l.disposition(1), Some(Disposition::Held { .. })), "{:?}", l.disposition(1));
    go.set();
    l.call_ended();
    assert_eq!(l.fake.adds(), Vec::<UnitId>::new(), "nothing called after close");
    assert_eq!(folder.ids(0), vec![1]);
}

/// D5a: a load's settle counts from the turn's view, not the tick's instant: a slow turn moves it on.
#[test]
fn a_settle_counts_from_the_turns_view() {
    let folder = Folder::new("settle-view");
    let rec = record(vec![amp(1, "slow")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add is out", || l.fake.adds() == [1]);
    l.fake.release(1);
    l.call_ended();
    l.fake.st().slow_view = Some(Duration::from_millis(60));
    l.turns(t0, 1);
    assert!(l.fake.applied(1));
    l.turns(t0 + SETTLE + Duration::from_millis(30), 10);
    assert_eq!(on_disk(&folder), Some(vec![1]), "its settle began after the turn's view");
    l.until(t0 + SETTLE + LATE, "the marker goes", || on_disk(&folder).is_none());
}

/// D5b: an add whose thread ended with no answer after it started (a panic) is waited on for the rest
/// of the launch: no later add, its load kept in the marker, also by close.
#[test]
fn an_add_that_ended_unanswered_after_it_started_is_waited_on() {
    let folder = Folder::new("panic");
    let rec = record(vec![amp(1, "panics"), amp(2, "next")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Panic]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add is out", || l.fake.adds() == [1]);
    l.call_ended();
    l.turns(t0 + LATE, 20);
    assert_eq!(l.fake.adds(), [1], "no later add");
    assert_eq!(on_disk(&folder), Some(vec![1]), "its load stays named");
    l.close_within();
    assert_eq!(on_disk(&folder), Some(vec![1]), "close keeps it as an add in flight");
    assert_eq!(folder.ids(0), vec![1, 2]);
}

/// D5b: an add whose thread never started is a host refusal: the unit is held at its place, and the
/// next unit goes. Needs the test hook (`Calls::no_thread`).
#[test]
fn an_add_that_never_started_is_held() {
    let folder = Folder::new("no-thread");
    let rec = record(vec![echo_unit(1), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    lock(&l.rig.calls).no_thread = true;
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert!(matches!(l.disposition(1), Some(Disposition::Held { .. })), "{:?}", l.disposition(1));
    assert_eq!(l.fake.adds(), [2]);
    assert_eq!(folder.ids(0), vec![1, 2]);
}

/// D6: a tick never stores ahead of the restore's `Commit`. The source head places the OTT and drops
/// the ECHO from `pending`; a gain change makes that commit write, and the write fails: the file never
/// held the placement, and keeps the ECHO in `pending` after.
#[test]
fn a_tick_never_stores_a_placement_ahead_of_its_commit() {
    let folder = Folder::new("place-commit");
    let mut rec = record(vec![amp(1, "keys")], Vec::new());
    rec.slots[0].pending = vec![ott_on(10), echo_unit(11)];
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::BlockSource]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the add is out", || l.fake.adds() == [1]);
    l.fake.release(1);
    l.call_ended();
    // The tick's gain read, then the commit's: at the second, the file is copied and blocked.
    l.fake.st().trap = Some(2);
    l.turns(t0, 1);
    let trapped = l.fake.st().trapped.take().expect("the commit read the gains");
    let held = RigRecord::read(&trapped).expect("a record");
    let pending = |r: &RigRecord| r.slots[0].pending.iter().map(|u| u.id).collect::<Vec<_>>();
    assert_eq!(pending(&held), vec![10, 11], "the file did not hold the placement before its commit");
    assert_eq!(held.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1]);

    let path = folder.path(files());
    std::fs::remove_dir(&path).expect("lift the obstacle");
    std::fs::write(&path, &trapped).expect("put the record back");
    l.until(t0, "the gain is written", || RigRecord::read(&folder.bytes(files())).is_ok_and(|r| r.slots[0].volume == 0.5));
    let stored = RigRecord::read(&folder.bytes(files())).expect("a record");
    assert_eq!(pending(&stored), vec![10, 11], "the failed placement keeps the ECHO pending");
    assert_eq!(l.fake.adds(), [1], "nothing placed is added");
}

/// D7: an add's answer that comes with a rebuild is fed after the new generation: the next add waits
/// for the seed's barrier on it.
#[test]
fn an_answer_with_a_rebuild_waits_for_the_new_barrier() {
    let folder = Folder::new("rebuild-between");
    let rec = record(vec![echo_unit(1), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::Block]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the first add is out", || l.fake.adds() == [1]);
    {
        let mut st = l.fake.st();
        st.device = Some(2);
        st.hold_taken = true;
    }
    l.fake.release(1);
    l.call_ended();
    l.turns(t0, 10);
    assert_eq!(l.fake.adds(), [1], "the next add waits for the new engine's barrier");
    l.fake.st().hold_taken = false;
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert_eq!(l.fake.adds(), [1, 2]);
}

// ── coverage through the driver ─────────────────────────────────────────────────────────────────────

/// A seed the ring refuses is sent again on the restore's backoff; the adds wait for it.
#[test]
fn a_refused_seed_is_sent_again() {
    let folder = Folder::new("seed-refused");
    let rec = record(vec![echo_unit(1)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.st().refuse_seeds = 2;
    let t0 = Instant::now();
    l.open();
    l.turns(t0, 10);
    assert_eq!(l.fake.adds(), Vec::<UnitId>::new(), "not seeded yet");
    // The first retry is refused too; its backoff runs from that turn.
    l.turns(t0 + LATE, 5);
    assert_eq!(l.fake.adds(), Vec::<UnitId>::new(), "refused again");
    l.until(t0 + 2 * LATE, "seeded and added", || l.fake.applied(1));
    assert_eq!(l.fake.st().seeds.len(), 3, "two refused, then taken");
}

/// A plugin whose own setup failed is forgotten: out of the record, its tone file gone, the next unit
/// added.
#[test]
fn a_plugin_failure_forgets_the_unit() {
    let folder = Folder::new("plugin-fails");
    let rec = record(vec![amp(1, "fails"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    write_tone(&folder, 1);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::PluginFails]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "forgotten, the next unit in", || l.fake.applied(2) && !tone(&folder, 1).exists());
    assert!(matches!(l.disposition(1), Some(Disposition::Forgotten)));
    assert_eq!(l.fake.st().forgot, vec![1]);
    assert_eq!(folder.ids(0), vec![2]);
    l.until(t0, "the marker goes", || on_disk(&folder).is_none());
}

/// A seat refused after the plugin ran holds the unit at its place, its tone file kept.
#[test]
fn a_seat_refusal_holds_the_unit() {
    let folder = Folder::new("seat-refused");
    let rec = record(vec![amp(1, "refused"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    write_tone(&folder, 1);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::SeatRefused]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert!(matches!(l.disposition(1), Some(Disposition::Held { .. })), "{:?}", l.disposition(1));
    assert!(tone(&folder, 1).exists());
    assert_eq!(folder.ids(0), vec![1, 2]);
    l.until(t0, "the marker goes", || on_disk(&folder).is_none());
}

/// A source the player put in heads the slot: the record's unit yields to it.
#[test]
fn a_source_taken_yields_the_unit() {
    let folder = Folder::new("source-taken");
    let rec = record(vec![amp(1, "synth"), echo_unit(2)], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    l.fake.script(1, &[Script::SourceTaken]);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "the next unit is in", || l.fake.applied(2));
    assert!(matches!(l.disposition(1), Some(Disposition::Yielded)), "{:?}", l.disposition(1));
    assert_eq!(l.fake.adds(), [1, 2]);
    l.until(t0, "the marker goes", || on_disk(&folder).is_none());
}

/// A marker write that fails is tried again on the restore's backoff, not at once: the settle's
/// removal fails behind an obstacle, waits its backoff, and goes once it may.
#[test]
fn a_failed_marker_write_waits_its_backoff() {
    let folder = Folder::new("marker-fault");
    let rec = record(vec![amp(1, "settles")], Vec::new());
    folder.write(files(), &rec);
    let l = Launch::new(&folder, &rec);
    let t0 = Instant::now();
    l.open();
    l.until(t0, "seated, settling", || l.fake.applied(1) && on_disk(&folder) == Some(vec![1]));
    let path = folder.0.join(files().marker);
    std::fs::remove_file(&path).expect("remove the marker");
    std::fs::create_dir(&path).expect("an obstacle at the marker's path");
    let t1 = t0 + SETTLE + LATE;
    // The settle's write failed once a timer later than this instant waits: the backoff.
    l.until(t1, "the write failed, its backoff armed", || l.rig.next_due(t1).is_some_and(|due| !due.is_zero()));
    std::fs::remove_dir(&path).expect("lift the obstacle");
    write_marker(&folder, &[1]);
    l.turns(t1, 10);
    assert_eq!(on_disk(&folder), Some(vec![1]), "not tried again before its backoff");
    l.until(t1 + LATE, "the marker goes", || on_disk(&folder).is_none());
}
