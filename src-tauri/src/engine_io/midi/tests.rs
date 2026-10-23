use super::*;
use std::cell::Cell;
use std::sync::atomic::AtomicBool;

use lf_engine::{Action, Instrument, Toggle};

use super::bindings::Kind;
use super::learn::RELEASE_WAIT;
use super::store::{Origin, FILE_NAME};

const A: u32 = 1;
const B: u32 = 2;
const LEAD: NoteTarget = NoteTarget::Builtin(Instrument::Lead);
const PAD: NoteTarget = NoteTarget::Builtin(Instrument::Pad);
const MS: Duration = Duration::from_millis(1);

/// The engine host as these tests need it: what it was sent, whether a device runs, a ring that
/// can be made to refuse, and the rebuild hook registered on it.
#[derive(Default)]
struct TestEngine {
    sent: Mutex<Vec<Command>>,
    running: AtomicBool,
    full: AtomicBool,
    hook: Mutex<Option<Arc<dyn RebuildHook>>>,
}

impl EngineSide for TestEngine {
    fn send(&self, command: TimedCommand) -> Result<(), String> {
        assert_eq!(command.frame, None, "nothing is stamped");
        if self.full.load(Relaxed) {
            return Err("the engine's command ring is full".into());
        }
        self.sent.lock().unwrap().push(command.command);
        Ok(())
    }

    fn running(&self) -> bool {
        self.running.load(Relaxed)
    }

    fn set_rebuild_hook(&self, hook: Option<Arc<dyn RebuildHook>>) {
        *self.hook.lock().unwrap() = hook;
    }
}

/// A port named `name` on its own device path.
fn port(name: &str, conn: Option<u32>) -> PortEntry {
    port_at(&format!(r"\\?\usb#{name}"), name, conn)
}

fn port_at(path: &str, name: &str, conn: Option<u32>) -> PortEntry {
    PortEntry::new(PortIdentity { path: path.into(), index: 0, name: name.into() }, conn, false)
}

fn id(name: &str) -> String {
    port(name, None).id
}

fn on(note: u8, velocity: u8) -> Command {
    Command::NoteOn(note, f32::from(velocity) / 127.0)
}

fn hold_pedal(port_name: &str, number: u8) -> Binding {
    Binding {
        port_id: id(port_name),
        port_name: port_name.into(),
        channel: 0,
        kind: Kind::Cc,
        number,
        action: ActionId::RecDub,
        target: None,
        press_high: true,
        momentary: true,
        hold: true,
    }
}

/// A store holding `list`, learned natively.
fn holding(list: Vec<Binding>) -> Store {
    let mut store = store::empty();
    store.replace(&[], list);
    store
}

/// A web record (`port`, as `midi-actions.ts` saves it): a momentary pedal on CC `number`.
fn legacy(port: &str, port_name: &str, number: u8, action: &str) -> serde_json::Value {
    serde_json::json!({
        "port": port, "portName": port_name, "channel": 0, "kind": "cc", "number": number,
        "action": action, "pressHigh": true, "momentary": true, "target": null, "hold": false,
    })
}

fn legacy_list(records: &[serde_json::Value]) -> String {
    serde_json::to_string(records).unwrap()
}

/// A scratch folder for `midi-bindings.json`, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("lf-midi-glue-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// The store as `MidiHost::start` loads it.
    fn load(&self) -> (Store, Option<StoreProblem>) {
        let (store, result) = store::load(&self.0);
        (store, loaded(result))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A host with no port thread (the tests run its timers: [`Rig::wait`]), a device running, and its
/// UI events recorded.
struct Rig {
    host: MidiHost,
    engine: Arc<TestEngine>,
    events: Arc<Mutex<Vec<MidiEvent>>>,
    t: Instant,
    /// The input epoch of the page the rig's listener stands for (its last subscribe's), and the age
    /// the next page subscribes with.
    epoch: Cell<u64>,
    origin: Cell<f64>,
}

impl Rig {
    /// Two ports, "Probe a" and "Probe b" on connections [`A`] and [`B`], as the browser probes'
    /// two virtual Web MIDI inputs.
    fn new() -> Rig {
        Rig::with((store::empty(), None), None, vec![port("Probe a", Some(A)), port("Probe b", Some(B))])
    }

    fn with((store, problem): (Store, Option<StoreProblem>), dir: Option<PathBuf>, ports: Vec<PortEntry>) -> Rig {
        let engine = Arc::new(TestEngine::default());
        engine.running.store(true, Relaxed);
        let core = hosted(engine.clone(), store, problem, dir, None);
        core.set_ports(ports);
        let rig = Rig {
            host: MidiHost { core, wake: None, port_thread: None },
            engine,
            events: Arc::default(),
            t: Instant::now(),
            epoch: Cell::new(0),
            origin: Cell::new(1_000.0),
        };
        rig.subscribe();
        rig.events();
        rig
    }

    /// A new page, younger than every one before it, subscribes: its listener hears what it needs to
    /// draw first. Its epoch, the rig's from now on.
    fn subscribe(&self) -> u64 {
        self.origin.set(self.origin.get() + 1.0);
        let events = self.events.clone();
        let epoch = self.host.subscribe(self.origin.get(), Box::new(move |e| events.lock().unwrap().push(e)));
        self.epoch.set(epoch);
        epoch
    }

    fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    /// The store revision the UI last heard with the list (what an edit names).
    fn revision(&self) -> u64 {
        self.host.core.lock().heard_revision
    }

    /// A note of the rig's document.
    fn key(&self, owner: &str, note: u8, on: bool) -> Option<Dropped> {
        self.host.ui_note(self.epoch(), owner.into(), note, if on { 100 } else { 0 }, on)
    }

    /// Messages arriving in one burst, at the rig's current time.
    fn send(&self, conn: u32, messages: &[[u8; 3]]) {
        for m in messages {
            self.host.core.message(conn, self.t, m);
        }
    }

    fn take(&self) -> Vec<Command> {
        std::mem::take(&mut *self.engine.sent.lock().unwrap())
    }

    fn events(&self) -> Vec<MidiEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    /// `d` later, the port thread runs the timers; when they next have work.
    fn wait(&mut self, d: Duration) -> Option<Instant> {
        self.t += d;
        self.host.core.tick(self.t)
    }

    fn running(&self, on: bool) {
        self.engine.running.store(on, Relaxed);
    }

    /// Learn `messages` (one burst from `conn`) onto `action`.
    fn learn(&self, action: ActionId, target: Target, conn: u32, messages: &[[u8; 3]]) {
        self.host.learn(self.epoch(), action, target);
        self.send(conn, messages);
    }

    /// What MIDI learn runs.
    fn live(&self) -> Vec<Binding> {
        self.host.core.lock().learn.bindings().to_vec()
    }

    fn held(&self) -> Vec<u8> {
        let held = self.host.held();
        (0..128).filter(|&n| held.contains(n)).collect()
    }

    fn states(&self) -> Vec<(String, bool, BindingState)> {
        self.host.listed().into_iter().map(|l| (l.listed.binding.port_name, l.listed.ordinal, l.state)).collect()
    }
}

// actions.ts runAction through a bound pedal: a named REC/DUB selects its lane, then runs there; a
// UI action is a `Press` and the UI's event; the stage view is an event only. Every looper press is
// also `Pressed` (onPress). A learn itself runs nothing.
#[test]
fn a_binding_runs_its_action_and_tells_the_ui() {
    let r = Rig::new();
    r.learn(ActionId::RecDub, Some(2), A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.learn(ActionId::GoLive, None, A, &[[0xb0, 21, 127], [0xb0, 21, 0]]);
    r.learn(ActionId::StageView, None, B, &[[0x91, 36, 100], [0x81, 36, 0]]);
    assert_eq!(r.take(), []);
    r.events();
    r.send(A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    assert_eq!(r.take(), [Command::SelectTrack(2), Command::ActionOn(2, Action::RecDub)]);
    assert_eq!(r.events(), [MidiEvent::Pressed]);
    r.send(A, &[[0xb0, 21, 127]]);
    assert_eq!(r.take(), [Command::Press]);
    assert_eq!(r.events(), [MidiEvent::Pressed, MidiEvent::Run { action: UiAction::GoLive }]);
    r.send(B, &[[0x91, 36, 100]]);
    assert_eq!(r.take(), []);
    assert_eq!(r.events(), [MidiEvent::Run { action: UiAction::StageView }]);
}

// midi-actions.ts and the learn UI: `learning` (a global action takes no target) and
// `awaitingRelease` as events, the list with its learned binding, and the wait ended by the port
// thread's timer, not by the next message.
#[test]
fn a_learn_is_heard_and_its_release_wait_ends_by_the_timer() {
    let mut r = Rig::new();
    r.host.learn(r.epoch(), ActionId::PlayAll, Some(3));
    assert_eq!(r.events(), [MidiEvent::Learning { learning: Some(LearnPick { action: ActionId::PlayAll, target: None }) }]);
    r.send(A, &[[0xb0, 20, 127]]);
    let b = Binding {
        port_id: id("Probe a"),
        port_name: "Probe a".into(),
        channel: 0,
        kind: Kind::Cc,
        number: 20,
        action: ActionId::PlayAll,
        target: None,
        press_high: true,
        momentary: false,
        hold: false,
    };
    let listed = Listed { binding: b.clone(), origin: Origin::Native, ordinal: false, blocked: None, display_name: "Probe a".into() };
    assert_eq!(
        r.events(),
        [
            MidiEvent::Learned { binding: b.clone() },
            MidiEvent::Bindings { bindings: vec![ListedBinding { listed, state: BindingState::Live }], revision: 1 },
            MidiEvent::Learning { learning: None },
            MidiEvent::AwaitingRelease { binding: Some(b) },
        ]
    );
    let learned_at = r.t;
    assert_eq!(r.wait(RELEASE_WAIT - MS), Some(learned_at + RELEASE_WAIT), "the port thread wakes for the deadline");
    assert_eq!(r.events(), []);
    assert_eq!(r.wait(MS), None);
    assert_eq!(r.events(), [MidiEvent::AwaitingRelease { binding: None }]);
    // Its wait over, the pedal reads latching: its release runs the action too.
    r.send(A, &[[0xb0, 20, 0]]);
    assert_eq!(r.take(), [Command::Action(Action::PlayAll)]);
}

// probe midi-learn "learning CC64 lets go of the pedal held down on its port", "a CC learned onto 64
// must not sustain", "the other port's unlearned CC64 still sustains" (consume-first).
#[test]
fn a_learned_cc64_never_sustains() {
    let r = Rig::new();
    r.send(B, &[[0xb0, 64, 127], [0x90, 50, 100], [0x80, 50, 0]]);
    assert_eq!(r.take(), [on(50, 100)], "port b's pedal holds its note");
    r.learn(ActionId::NextTrack, None, B, &[[0xb0, 64, 0], [0xb0, 64, 127]]);
    assert_eq!(r.take(), [Command::NoteOff(50)], "learning the pedal lets go of it");
    r.send(B, &[[0x90, 67, 100], [0x80, 67, 0]]);
    assert_eq!(r.take(), [on(67, 100), Command::NoteOff(67)], "a learned CC64 never sustains");
    r.send(B, &[[0xb0, 64, 0]]);
    assert_eq!(r.take(), [Command::Action(Action::NextTrack)], "a reversed pedal presses on 0");
    r.send(A, &[[0xb0, 64, 127], [0x90, 67, 100], [0x80, 67, 0]]);
    assert_eq!(r.take(), [on(67, 100)], "port a's unlearned CC64 still sustains");
}

// One note router for every source: a pointer, a key and a port on one note: one strike, one
// release when the last lets go; the held set reaches the UI once per change.
#[test]
fn ui_and_midi_owners_share_one_note() {
    let r = Rig::new();
    r.key("pointer:1", 60, true);
    assert_eq!(r.take(), [on(60, 100)]);
    r.send(A, &[[0x90, 60, 90]]);
    r.key("key:KeyA", 60, true);
    r.key("pointer:1", 60, false);
    r.key("key:KeyA", 60, false);
    assert_eq!(r.take(), [], "the port still holds it");
    assert_eq!(r.held(), [60]);
    r.send(A, &[[0x80, 60, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(60)]);
    let held: Vec<Vec<u8>> = r.events().into_iter().filter_map(|e| if let MidiEvent::Held { notes, .. } = e { Some(notes) } else { None }).collect();
    assert_eq!(held, [vec![60], vec![]]);
}

// A target switch then a note-on back to back sounds on the new target, also when both
// wait behind a ring that refused, which the port thread retries soon.
#[test]
fn a_target_switch_then_a_note_on_land_in_that_order() {
    let mut r = Rig::new();
    r.host.select_target(r.epoch(), Some(0), LEAD);
    r.send(A, &[[0x90, 60, 100]]);
    assert_eq!(r.take(), [Command::SelectInstrument(LEAD), on(60, 100)]);
    r.engine.full.store(true, Relaxed);
    r.host.select_target(r.epoch(), Some(1), PAD);
    r.send(B, &[[0x90, 64, 100]]);
    assert_eq!(r.take(), []);
    let now = r.t;
    assert_eq!(r.wait(Duration::ZERO), Some(now + RETRY), "a refused drain is retried soon");
    r.engine.full.store(false, Relaxed);
    assert_eq!(r.wait(RETRY), None, "nothing left to retry");
    assert_eq!(r.take(), [Command::NoteOff(60), Command::SelectInstrument(PAD), on(64, 100)]);
    assert!(r.host.diag().failed_sends > 0);
}

// One ordered path into the engine: the UI's input commands join the one queue, so a pedal and a
// click keep their order and fall under the same no-device rule (the UI hears why its press did
// nothing); a setting goes straight to the engine; a note goes through the router only, and a batch
// holding one is refused before any of it runs.
#[test]
fn the_uis_commands_join_the_queue_and_its_settings_go_straight_to_the_engine() {
    let mut r = Rig::new();
    let batch = vec![Command::Press, Command::SetBpm(100.0), Command::SelectTrack(1), Command::ActionOn(1, Action::RecDub)];
    assert_eq!(r.host.ui_commands(batch.clone()), Ok(None));
    assert_eq!(r.take(), batch);

    r.running(false);
    let dropped = r.host.ui_commands(vec![Command::Action(Action::Toggle(Toggle::Click)), Command::SetBpm(90.0)]);
    assert_eq!(dropped, Ok(Some(Dropped::NoDevice)), "the UI hears why");
    assert_eq!(r.take(), [Command::SetBpm(90.0)], "the toggle is dropped as a pedal's would be; the setting is kept");
    assert_eq!(r.host.diag().refused.no_device, 1);
    r.running(true);

    assert!(r.host.ui_commands(vec![Command::Press, Command::NoteOn(60, 0.5), Command::Press]).is_err());
    assert_eq!(r.take(), [], "nothing of a batch holding a note runs");

    r.learn(ActionId::PlayAll, None, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.engine.full.store(true, Relaxed);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.host.ui_commands(vec![Command::Action(Action::StopAll)]), Ok(None), "queued behind the pedal, not dropped");
    r.send(A, &[[0xb0, 20, 0], [0xb0, 20, 127]]);
    r.engine.full.store(false, Relaxed);
    r.wait(RETRY);
    let all = [Command::Action(Action::PlayAll), Command::Action(Action::StopAll), Command::Action(Action::PlayAll)];
    assert_eq!(r.take(), all, "pedal, click, pedal");
}

// Decisions 8 and 9: a legacy record activates on the one present port with its name, and the move
// is saved (its real id replaces the ordinal one).
#[test]
fn a_legacy_record_runs_on_the_one_port_with_its_name_and_the_move_is_saved() {
    let dir = Scratch::new("follow");
    let mut r = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Keys", Some(B)), port("Pedal", Some(A))]);
    let report = r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-3", "Pedal", 20, "recDub")])).unwrap();
    assert_eq!(report.imported, [0]);
    let listed = &r.host.listed()[0];
    assert_eq!((listed.state, listed.listed.ordinal, listed.listed.origin), (BindingState::Live, false, Origin::Legacy));
    assert_eq!(listed.listed.binding.port_id, id("Pedal"));
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::RecDub)]);
    assert_eq!(r.wait(Duration::ZERO), None);
    let (saved, problem) = dir.load();
    assert_eq!(problem, None);
    let saved = &saved.listed()[0];
    assert_eq!((saved.binding.port_id.as_str(), saved.ordinal), (id("Pedal").as_str(), false), "the move is on disk");
    assert!(!r.events().iter().any(|e| matches!(e, MidiEvent::Store { .. })));
}

// Decision 9: two present ports with its name leave a legacy record unrun and unmoved; once one of
// them goes, the other is the one port with the name.
#[test]
fn two_ports_with_its_name_leave_a_legacy_record_unrun() {
    let r = Rig::with((store::empty(), None), None, vec![port_at(r"\\?\usb#1", "Pedal", Some(A)), port_at(r"\\?\usb#2", "Pedal", Some(B))]);
    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-3", "Pedal", 20, "recDub")]));
    assert_eq!(r.states(), [("Pedal".into(), true, BindingState::SeveralPorts)]);
    r.send(A, &[[0xb0, 20, 127]]);
    r.send(B, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [], "neither port runs it");
    r.host.core.set_ports(vec![port_at(r"\\?\usb#2", "Pedal", Some(B))]);
    assert_eq!(r.states(), [("Pedal".into(), false, BindingState::Live)]);
    r.send(B, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::RecDub)]);
}

// Two legacy records on one ordinal id under two names are two controllers: neither reaches MIDI
// learn on that id (it would read them as one control); each runs once resolution gives it its
// port's real id.
#[test]
fn an_ordinal_pair_with_one_id_never_reaches_learn() {
    let r = Rig::with((store::empty(), None), None, vec![port("Keys", Some(B))]);
    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-1", "Pedal", 20, "playAll"), legacy("input-1", "Keys", 20, "stopAll")]));
    assert_eq!(r.states(), [("Pedal".into(), true, BindingState::NoPort), ("Keys".into(), false, BindingState::Live)]);
    assert_eq!(r.live().iter().map(|b| b.port_id.clone()).collect::<Vec<_>>(), [id("Keys")]);
    r.send(B, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::StopAll)]);

    r.host.core.set_ports(vec![port("Keys", Some(B)), port("Pedal", Some(A))]);
    assert!(r.live().iter().all(|b| !b.port_id.starts_with("input-")));
    assert_eq!(r.live().len(), 2);
    r.send(A, &[[0xb0, 20, 127]]);
    r.send(B, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::PlayAll), Command::Action(Action::StopAll)]);

    r.host.core.set_ports(Vec::new());
    assert!(r.live().is_empty(), "no port, nothing runs");
}

// Decision 8: the store is written on the port thread, off the lock; a file another instance wrote
// since is a conflict, told to the UI (and the release log), never a panic. The bindings run on.
#[test]
fn a_store_conflict_becomes_an_event() {
    let dir = Scratch::new("conflict");
    let mut r = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Probe a", Some(A))]);
    let (mut other, _) = store::load(&dir.0);
    other.import_legacy("[]");
    other.snapshot().write(&dir.0).unwrap();
    r.learn(ActionId::Undo, None, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.events();
    r.wait(Duration::ZERO);
    let problems: Vec<StoreProblem> = r.events().into_iter().filter_map(|e| if let MidiEvent::Store { problem } = e { Some(problem) } else { None }).collect();
    assert!(matches!(problems[..], [StoreProblem::Conflict { .. }]), "{problems:?}");
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Undo)]);
    // A listener that comes later hears it too.
    r.subscribe();
    assert!(r.events().iter().any(|e| matches!(e, MidiEvent::Store { problem: StoreProblem::Conflict { .. } })));
}

// Decision 8: a file this build cannot read is reported at once and never written over.
#[test]
fn an_unreadable_store_is_reported_and_left_as_it_is() {
    let dir = Scratch::new("torn");
    std::fs::write(dir.0.join(FILE_NAME), b"{ torn").unwrap();
    let mut r = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Probe a", Some(A))]);
    r.subscribe();
    assert!(r.events().iter().any(|e| matches!(e, MidiEvent::Store { problem: StoreProblem::ReadOnly { .. } })));
    r.learn(ActionId::Undo, None, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.wait(Duration::ZERO);
    assert!(r.events().iter().any(|e| matches!(e, MidiEvent::Store { problem: StoreProblem::ReadOnly { .. } })));
    assert_eq!(std::fs::read(dir.0.join(FILE_NAME)).unwrap(), b"{ torn");
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Undo)], "the binding works in memory");
}

// A failed write (the file held open without delete sharing, so the rename over it fails) is tried
// again with no change to start it, on a backoff that doubles up to its bound, told once; the write
// that lands clears the problem, so a later listener hears none.
#[test]
fn a_failed_store_write_is_retried_on_a_bounded_backoff_until_it_lands() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    let dir = Scratch::new("retry");
    let mut r = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Probe a", Some(A))]);
    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-1", "Probe a", 20, "undo")]));
    assert_eq!(r.wait(Duration::ZERO), None, "written");
    let store_events = |r: &Rig| r.events().into_iter().filter(|e| matches!(e, MidiEvent::Store { .. })).count();
    store_events(&r);

    let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(dir.0.join(FILE_NAME)).unwrap();
    assert_eq!(r.host.set_momentary(r.epoch(), r.revision(), 0, false), Ok(true));
    assert_eq!(r.wait(Duration::ZERO), Some(r.t + WRITE_RETRY_FIRST), "the failed write comes back");
    assert_eq!(store_events(&r), 1, "told once");
    assert!(matches!(r.host.core.lock().problem, Some(StoreProblem::Failed { .. })));
    assert_eq!(r.wait(WRITE_RETRY_FIRST / 2), Some(r.t + WRITE_RETRY_FIRST / 2), "not before it is due");
    let mut waited = WRITE_RETRY_FIRST / 2;
    let mut gaps = Vec::new();
    for _ in 0..8 {
        let next = r.wait(waited).expect("a retry is due again");
        waited = next - r.t;
        gaps.push(waited.as_secs());
    }
    assert_eq!(gaps, [2, 4, 8, 16, 32, 60, 60, 60], "doubling, bounded");
    assert_eq!(store_events(&r), 0, "a retry failing as before tells nothing new");

    drop(held);
    assert_eq!(r.wait(waited), None, "the retry lands");
    assert_eq!(r.host.core.lock().problem, None);
    assert!(!dir.load().0.listed()[0].binding.momentary, "the edit is on disk");
    r.subscribe();
    assert_eq!(store_events(&r), 0, "a later listener hears no problem");
}

// The port's name under WinMM may not be the one Web MIDI stored: a legacy record no present port
// answers to is assigned by the player like any unresolved one; it runs there, and the port's id
// and name are saved, so the next start resolves it by its own rules.
#[test]
fn a_record_no_port_answers_to_is_assigned_to_a_present_port() {
    let dir = Scratch::new("assign-noport");
    let mut r = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Pedal", Some(A))]);
    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-1", "Pedal (USB MIDI)", 20, "undo")]));
    assert_eq!(r.states(), [("Pedal (USB MIDI)".into(), true, BindingState::NoPort)]);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [], "nothing runs it");

    assert_eq!(r.host.assign(r.epoch(), r.revision(), 0, &id("Pedal")), Ok(true));
    assert_eq!(r.states(), [("Pedal".into(), false, BindingState::Live)]);
    assert_eq!(r.host.listed()[0].listed.origin, Origin::Legacy, "the origin stays");
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Undo)]);
    assert_eq!(r.wait(Duration::ZERO), None);

    let saved = dir.load().0.listed().remove(0);
    assert_eq!((saved.binding.port_id.as_str(), saved.binding.port_name.as_str(), saved.ordinal), (id("Pedal").as_str(), "Pedal", false));
    assert_eq!((saved.blocked.as_deref(), saved.origin), (None, Origin::Legacy));
    let next = Rig::with(dir.load(), Some(dir.0.clone()), vec![port("Pedal", Some(A))]);
    assert_eq!(next.states(), [("Pedal".into(), false, BindingState::Live)], "the next start runs it");
}

// The player's edits go to the store by listed index and reach MIDI learn through its hand-off: a
// HOLD an edit ends is released and its pedal's release spent (midi-actions.ts update); a forgotten
// binding hands its message back; an assignment runs a blocked record on the port picked.
#[test]
fn the_players_edits_reach_learn_and_release_what_they_end() {
    let r = Rig::with((holding(vec![hold_pedal("Probe a", 20)]), None), None, vec![port("Probe a", Some(A)), port("Probe b", Some(B))]);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Hold(0))]);
    assert_eq!(r.host.set_momentary(r.epoch(), r.revision(), 0, false), Ok(true));
    assert_eq!(r.take(), [Command::Action(Action::Release(0))]);
    assert_eq!((r.host.listed()[0].listed.binding.momentary, r.host.listed()[0].listed.binding.hold), (false, false));
    r.send(A, &[[0xb0, 20, 0]]);
    assert_eq!(r.take(), [], "the pedal's own release is spent");
    r.send(A, &[[0xb0, 20, 0]]);
    assert_eq!(r.take(), [Command::Action(Action::RecDub)], "latching now");
    assert!(r.host.set_hold(r.epoch(), r.revision(), 0, true).is_err(), "a latching pedal has no HOLD");
    assert_eq!(r.host.forget(r.epoch(), r.revision(), 0), Ok(true));
    assert_eq!(r.host.listed(), []);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [], "the message reaches the play path again, where CC20 does nothing");

    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-1", "Pedal", 20, "undo"), legacy("input-3", "Pedal", 20, "undo")]));
    assert!(r.states().iter().all(|s| s.2 == BindingState::Blocked));
    assert!(r.host.assign(r.epoch(), r.revision(), 0, "winmm:0:nowhere:Pedal").is_err(), "no such port");
    assert_eq!(r.host.assign(r.epoch(), r.revision(), 0, &id("Probe b")), Ok(true));
    assert_eq!(r.states()[0], ("Probe b".into(), false, BindingState::Live));
    r.send(B, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Undo)]);
}

// The engine's 16 HOLD controls held at once: a 17th HOLD press is consumed, runs nothing and is told
// to the UI.
#[test]
fn a_hold_press_past_the_engines_controls_is_refused_and_heard() {
    let r = Rig::with((holding((0..=16).map(|n| hold_pedal("Probe a", n)).collect()), None), None, vec![port("Probe a", Some(A))]);
    for n in 0..16 {
        r.send(A, &[[0xb0, n, 127]]);
    }
    assert_eq!(r.take().len(), 16);
    r.events();
    r.send(A, &[[0xb0, 16, 127]]);
    assert_eq!(r.take(), []);
    assert_eq!(r.events(), [MidiEvent::Refused { reason: LearnRefusal::HoldControlsTaken }]);
}

// A port that goes away releases its notes and pedal and its HOLD press; its late message finds no
// owner; the UI hears the list and the departure (midi.ts attachInputs, midi-actions.ts portGone).
#[test]
fn a_port_that_goes_away_releases_what_it_held_and_the_ui_hears_it() {
    let r = Rig::with((holding(vec![hold_pedal("Probe a", 20)]), None), None, vec![port("Probe a", Some(A)), port("Probe b", Some(B))]);
    r.host.core.publish_ports(Vec::new());
    r.send(A, &[[0xb0, 64, 127], [0x90, 60, 100], [0x80, 60, 0], [0xb0, 20, 127]]);
    r.send(B, &[[0x90, 62, 100]]);
    r.take();
    r.events();
    assert_eq!(r.host.core.port_gone(A), Some("Probe a".into()));
    assert_eq!(r.take(), [Command::NoteOff(60), Command::Action(Action::Release(0))]);
    r.send(A, &[[0x90, 61, 100]]);
    assert_eq!(r.take(), []);
    assert_eq!(r.held(), [62]);
    r.host.core.publish_ports(vec!["Probe a".into()]);
    let ports = vec![
        PortInfo { id: id("Probe a"), name: "Probe a".into(), state: PortState::Closed },
        PortInfo { id: id("Probe b"), name: "Probe b".into(), state: PortState::Open },
    ];
    let events: Vec<MidiEvent> = r.events().into_iter().filter(|e| !matches!(e, MidiEvent::Held { .. })).collect();
    assert_eq!(events, [MidiEvent::Ports { ports }, MidiEvent::Gone { names: vec!["Probe a".into()] }]);
}

// Decision 7, with no device: a bound press and a note-on are dropped (the note-on holds nothing);
// a note held from before gets its release once a device runs again, before what comes after it;
// learn still captures.
#[test]
fn with_no_device_fresh_input_is_dropped_and_releases_wait_for_it() {
    let mut r = Rig::new();
    r.learn(ActionId::RecDub, None, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.send(A, &[[0x90, 60, 100]]);
    assert_eq!(r.take(), [on(60, 100)]);
    r.running(false);
    r.send(A, &[[0xb0, 20, 127], [0xb0, 20, 0], [0x90, 64, 100], [0x80, 60, 0]]);
    assert_eq!(r.take(), []);
    assert_eq!(r.held(), [] as [u8; 0], "the dropped note-on holds nothing");
    r.learn(ActionId::Undo, None, A, &[[0xb0, 30, 127]]);
    assert!(r.live().iter().any(|b| b.number == 30), "learn captured");
    let now = r.t;
    assert_eq!(r.wait(Duration::ZERO), Some(now + IDLE_RETRY), "the port thread looks again for a device");
    r.running(true);
    r.wait(IDLE_RETRY);
    assert_eq!(r.take(), [Command::NoteOff(60)]);
}

// A retained-engine device gap with a HOLD press and its release (the no-device rule). The release
// of a HOLD pressed before the gap waits and reaches the engine once it runs again; a HOLD pressed in
// the gap is dropped and holds nothing, so its release runs nothing and its number is free.
#[test]
fn in_a_device_gap_a_hold_release_passes_and_a_hold_press_is_dropped() {
    let mut r = Rig::with((holding(vec![hold_pedal("Probe a", 20), hold_pedal("Probe a", 21)]), None), None, vec![port("Probe a", Some(A))]);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Hold(0))]);
    r.running(false);
    r.send(A, &[[0xb0, 21, 127], [0xb0, 20, 0], [0xb0, 21, 0]]);
    assert_eq!(r.take(), [], "nothing reaches a stopped engine");
    assert_eq!(r.host.diag().refused.no_device, 1);
    r.wait(IDLE_RETRY);
    assert_eq!(r.take(), []);
    r.running(true);
    r.wait(IDLE_RETRY);
    assert_eq!(r.take(), [Command::Action(Action::Release(0))]);
    r.send(A, &[[0xb0, 21, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Hold(0))]);
}

// Decision 7: a new document (a reload, a recovery) subscribes, which releases the old one's holds and
// refuses its late input of every kind (a note, a slot pick, a panic, a blur), and cancels a learn
// the old page left listening (an unload runs no cleanup); a blur releases the document's holds;
// MIDI holds stay through both.
#[test]
fn a_frontend_reload_releases_the_ui_holds_only_and_refuses_the_old_documents_input() {
    let r = Rig::new();
    let old = r.epoch();
    r.key("pointer:1", 60, true);
    r.key("key:KeyA", 62, true);
    r.send(A, &[[0x90, 64, 100]]);
    r.host.learn(r.epoch(), ActionId::Undo, None);
    r.take();
    r.events();
    let new = r.subscribe();
    assert!(new > old);
    assert_eq!(r.take(), [Command::NoteOff(60), Command::NoteOff(62)]);
    assert!(
        r.events().contains(&MidiEvent::Learning { learning: None }),
        "the learn the old page left listening is cancelled, and the new page hears it"
    );
    r.send(A, &[[0xb0, 30, 127]]);
    assert!(r.live().is_empty(), "nothing is learned unseen");

    let late = [
        r.host.ui_note(old, "key:KeyB".into(), 65, 100, true),
        r.host.select_target(old, Some(1), PAD),
        r.host.all_notes_off(old),
        r.host.ui_blur(old),
    ];
    assert_eq!(r.take(), [], "a late input of the old document, whatever its kind");
    assert_eq!(late, [Some(Dropped::Stale); 4], "and each is answered as stale");
    assert_eq!(r.held(), [64]);
    r.host.ui_note(new, "key:KeyA".into(), 67, 100, true);
    r.host.ui_blur(new);
    assert_eq!(r.take(), [on(67, 100), Command::NoteOff(67)]);
    assert_eq!(r.held(), [64]);
    r.host.select_target(new, Some(1), PAD);
    assert_eq!(r.take(), [Command::NoteOff(64), Command::SelectInstrument(PAD)], "the new document's slot pick runs");
}

// A subscribe of an older page that reaches native code after a newer page's (its call was late)
// changes nothing: the newer page keeps the events, its holds and its learn, and the older page's
// epoch is never current. Its age is the page's own (`performance.timeOrigin`), not the call's
// arrival.
#[test]
fn an_older_pages_late_subscribe_changes_nothing() {
    let r = Rig::new();
    let older_page = r.origin.get() + 1.0;
    r.origin.set(older_page + 1.0);
    let newer = r.subscribe();
    r.host.ui_note(newer, "pointer:1".into(), 60, 100, true);
    r.host.learn(newer, ActionId::Undo, None);
    r.take();
    r.events();
    let late: Arc<Mutex<Vec<MidiEvent>>> = Arc::default();
    let sink = late.clone();
    let older = r.host.subscribe(older_page, Box::new(move |e| sink.lock().unwrap().push(e)));
    assert!(older > newer, "its call came later");
    assert_eq!(r.take(), [], "the newer page's note holds");
    assert_eq!(r.host.ui_note(older, "pointer:1".into(), 61, 100, true), Some(Dropped::Stale), "the older page's input is refused");
    assert!(!r.host.learn(older, ActionId::PlayAll, None), "and its learn");
    assert_eq!(r.take(), []);
    assert!(r.host.cancel_learn(newer));
    assert_eq!(*late.lock().unwrap(), [], "the late subscriber hears nothing");
    assert_eq!(r.events(), [MidiEvent::Learning { learning: None }], "the newer page still does, and its learn ran until it cancelled it");
    // The same page subscribing again (a remount) is no older than itself.
    let again = r.host.subscribe(r.origin.get(), Box::new(|_| {}));
    assert_eq!(r.host.ui_note(again, "pointer:2".into(), 62, 100, true), None);
    assert_eq!(r.take(), [Command::NoteOff(60), on(62, 100)], "its remount releases the subscription before it, and plays");
}

// A learn call of a replaced page (its learn or cancel or edit arrived after the new page's subscribe)
// changes nothing: a learn the new page's subscribe cancelled stays cancelled.
#[test]
fn a_replaced_pages_learn_calls_change_nothing() {
    let r = Rig::with((holding(vec![hold_pedal("Probe a", 20)]), None), None, vec![port("Probe a", Some(A))]);
    let old = r.epoch();
    let revision = r.revision();
    let new = r.subscribe();
    r.events();
    assert!(!r.host.learn(old, ActionId::Undo, None), "an old page's late learn");
    assert_eq!(r.host.forget(old, revision, 0), Ok(false));
    assert_eq!(r.host.set_momentary(old, revision, 0, false), Ok(false));
    assert_eq!(r.host.set_hold(old, revision, 0, false), Ok(false));
    assert_eq!(r.host.assign(old, revision, 0, &id("Probe a")), Ok(false));
    assert_eq!(r.host.import_legacy(old, &legacy_list(&[legacy("input-1", "Pedal", 30, "undo")])), None);
    assert_eq!(r.events(), [], "nothing listens, nothing changed");
    assert_eq!(r.host.listed().len(), 1);
    r.send(A, &[[0xb0, 31, 127]]);
    assert!(r.live().iter().all(|b| b.number == 20), "nothing is learned unseen");
    assert!(r.host.learn(new, ActionId::Undo, None));
    assert!(!r.host.cancel_learn(old), "an old page's cancel ends no learn of the new page");
    assert_eq!(r.events(), [MidiEvent::Learning { learning: Some(LearnPick { action: ActionId::Undo, target: None }) }]);
    assert!(r.host.import_legacy(new, "[]").is_some());
}

// A record learn does not run, arriving while its control is held (the import landed after the port
// played): the release the router owes passes, a note-off and a pedal-up alike; the next press is
// consumed.
#[test]
fn an_unrun_record_arriving_mid_hold_lets_its_release_pass() {
    let r = Rig::with((store::empty(), None), None, vec![port("Pedal", Some(A))]);
    // Channel 2's key holds 36; channel 1's pedal sustains 40.
    r.send(A, &[[0x91, 36, 100], [0xb0, 64, 127], [0x90, 40, 100], [0x80, 40, 0]]);
    assert_eq!(r.take(), [on(36, 100), on(40, 100)], "36 held, 40 under the pedal");
    let note = |port: &str| {
        let mut record = legacy(port, "Pedal", 36, "playAll");
        record["kind"] = "note".into();
        record["channel"] = 1.into();
        record
    };
    let records = [note("input-1"), note("input-3"), legacy("input-1", "Pedal", 64, "undo"), legacy("input-3", "Pedal", 64, "undo")];
    r.host.import_legacy(r.epoch(), &legacy_list(&records));
    assert!(r.host.listed().iter().all(|l| l.state == BindingState::Blocked));
    r.send(A, &[[0x81, 36, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(36)], "the held note's release passes");
    r.send(A, &[[0xb0, 64, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(40)], "the pedal's release lets go of what it sustained");
    r.send(A, &[[0x91, 36, 100], [0x81, 36, 0], [0xb0, 64, 127], [0x90, 41, 100], [0x80, 41, 0], [0xb0, 64, 0]]);
    assert_eq!(r.take(), [on(41, 100), Command::NoteOff(41)], "the next press is consumed: no note, no sustain");
}

// A binding made live (here by the player's assignment) while its note or pedal is held through the
// router: learn takes the release from now on, so the router lets go then, of that port and
// channel's hold only.
#[test]
fn a_binding_made_live_mid_hold_releases_what_the_router_held_under_it() {
    let r = Rig::with((store::empty(), None), None, vec![port("Pedal", Some(A)), port("Keys", Some(B))]);
    // Channel 2's key holds 36, as Keys does; channel 1's pedal sustains 40.
    r.send(A, &[[0x91, 36, 100], [0xb0, 64, 127], [0x90, 40, 100], [0x80, 40, 0]]);
    r.send(B, &[[0x91, 36, 100]]);
    assert_eq!(r.take(), [on(36, 100), on(40, 100)]);
    let mut note = legacy("input-1", "Pedal (USB MIDI)", 36, "playAll");
    note["kind"] = "note".into();
    note["channel"] = 1.into();
    r.host.import_legacy(r.epoch(), &legacy_list(&[note, legacy("input-1", "Pedal (USB MIDI)", 64, "undo")]));
    assert!(r.states().iter().all(|s| s.2 == BindingState::NoPort));

    assert_eq!(r.host.assign(r.epoch(), r.revision(), 0, &id("Pedal")), Ok(true));
    assert_eq!((r.take(), r.held()), (vec![], vec![36]), "Keys still holds 36");
    r.send(B, &[[0x81, 36, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(36)], "Keys' release is its last");
    assert_eq!(r.host.assign(r.epoch(), r.revision(), 1, &id("Pedal")), Ok(true));
    assert_eq!(r.take(), [Command::NoteOff(40)], "the pedal lets go of what it sustained");
    r.send(A, &[[0x81, 36, 0], [0xb0, 64, 0]]);
    assert_eq!((r.take(), r.held()), (vec![], vec![]), "learn takes both releases, and nothing hangs");
    assert_eq!(r.states().iter().map(|s| s.2).collect::<Vec<_>>(), [BindingState::Live; 2]);
}

// The UI hears why its input did nothing: a fresh note or press while no device runs or the engine
// is being rebuilt; a release never is.
#[test]
fn the_uis_dropped_input_is_answered() {
    let r = Rig::new();
    assert_eq!(r.key("pointer:1", 60, true), None);
    r.running(false);
    assert_eq!(r.key("pointer:2", 62, true), Some(Dropped::NoDevice));
    assert_eq!(r.key("pointer:1", 60, false), None, "a release passes");
    assert_eq!(r.host.ui_commands(vec![Command::Press]), Ok(Some(Dropped::NoDevice)));
    r.running(true);
    let hook = r.engine.hook.lock().unwrap().clone().expect("registered at start");
    hook.pause();
    assert_eq!(r.key("pointer:2", 62, true), Some(Dropped::Rebuilding));
    assert_eq!(r.host.ui_commands(vec![Command::SetBpm(90.0), Command::Press]), Ok(Some(Dropped::Rebuilding)));
    hook.resume();
    assert_eq!(r.key("pointer:2", 62, true), None);
}

// An edit by index names the list it was made against: once the list changed (forgetting the first
// binding), an edit made against the older one is refused and changes nothing, since its index now
// names another binding. The list goes out with each revision.
#[test]
fn an_edit_made_against_an_older_list_is_refused() {
    let r = Rig::with((holding(vec![hold_pedal("Probe a", 20), hold_pedal("Probe a", 21), hold_pedal("Probe a", 22)]), None), None, vec![port("Probe a", Some(A))]);
    let seen = r.revision();
    assert_eq!(r.host.forget(r.epoch(), seen, 0), Ok(true));
    let revision = r.revision();
    assert!(revision > seen);
    let heard: Vec<u64> = r.events().into_iter().filter_map(|e| if let MidiEvent::Bindings { revision, .. } = e { Some(revision) } else { None }).collect();
    assert_eq!(heard, [revision], "the new list went out with its revision");
    let numbers = |r: &Rig| r.host.listed().iter().map(|l| (l.listed.binding.number, l.listed.binding.momentary)).collect::<Vec<_>>();
    assert_eq!(r.host.set_momentary(r.epoch(), seen, 1, false), Ok(false), "made against the list before the forget");
    assert_eq!(r.host.forget(r.epoch(), seen, 1), Ok(false));
    assert_eq!(r.host.assign(r.epoch(), seen, 0, &id("Probe a")), Ok(false));
    assert_eq!(r.host.set_hold(r.epoch(), seen, 0, false), Ok(false));
    assert_eq!(numbers(&r), [(21, true), (22, true)], "nothing changed");
    assert_eq!(r.revision(), revision);
    assert_eq!(r.host.set_momentary(r.epoch(), revision, 1, false), Ok(true), "made against the list as it is");
    assert_eq!(numbers(&r), [(21, true), (22, false)]);
}

// Without the app's data folder the bindings live in memory, and the UI hears it at its subscribe.
#[test]
fn with_no_data_folder_the_ui_hears_the_bindings_are_not_kept() {
    let r = Rig::with(open_store(None), None, vec![port("Probe a", Some(A))]);
    r.subscribe();
    let problems: Vec<StoreProblem> = r.events().into_iter().filter_map(|e| if let MidiEvent::Store { problem } = e { Some(problem) } else { None }).collect();
    assert_eq!(problems, [StoreProblem::ReadOnly { why: NO_FOLDER.into() }]);
    r.learn(ActionId::Undo, None, A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Undo)], "the binding works in memory");
}

// A record learn does not run (blocked, or still on its legacy ordinal id) owns its control on a port
// of its name: a pedal waiting for the player's assignment never plays the synth, its note neither
// strikes nor releases, and the same message on a port of another name plays as before.
#[test]
fn an_unrun_records_control_runs_nothing_on_a_port_of_its_name() {
    let both = |n: u8| legacy("input-1", "Pedal", n, "undo");
    let note = |port: &str, number: u8, action: &str| {
        let mut record = legacy(port, "Pedal", number, action);
        record["kind"] = "note".into();
        record
    };
    // CC 20 and note 36 blocked (two legacy ids bind them); note 40 ordinal, two ports named Pedal.
    let records = [both(20), legacy("input-3", "Pedal", 20, "undo"), note("input-1", 36, "playAll"), note("input-3", 36, "playAll"), note("input-1", 40, "stopAll")];
    let ports = vec![port_at(r"\\?\usb#1", "Pedal", Some(A)), port_at(r"\\?\usb#2", "Pedal", Some(B)), port_at(r"\\?\usb#3", "Keys", Some(3))];
    let r = Rig::with((store::empty(), None), None, ports);
    r.host.import_legacy(r.epoch(), &legacy_list(&records));
    let states: Vec<BindingState> = r.host.listed().iter().map(|l| l.state).collect();
    assert_eq!(states, [BindingState::Blocked, BindingState::Blocked, BindingState::Blocked, BindingState::Blocked, BindingState::SeveralPorts]);
    r.send(A, &[[0xb0, 20, 127], [0x90, 36, 100], [0x80, 36, 0], [0x90, 40, 100]]);
    r.send(B, &[[0x90, 40, 100], [0x80, 40, 0], [0x90, 36, 100]]);
    assert_eq!(r.take(), [], "no action, no note");
    assert_eq!(r.held(), [] as [u8; 0]);
    r.send(A, &[[0x90, 41, 100], [0x91, 36, 100], [0x81, 36, 0]]);
    r.send(3, &[[0x90, 36, 100]]);
    assert_eq!(r.take(), [on(41, 100), on(36, 100), Command::NoteOff(36), on(36, 100)], "another note, another channel, another port's name: played");
    // Assigned, a record runs on its port; the other still owns its control on the rest.
    assert_eq!(r.host.assign(r.epoch(), r.revision(), 2, &port_at(r"\\?\usb#1", "Pedal", None).id), Ok(true));
    r.send(A, &[[0x90, 36, 100]]);
    assert_eq!(r.take(), [Command::Action(Action::PlayAll)]);
    r.send(B, &[[0x90, 36, 100]]);
    assert_eq!(r.take(), [], "the blocked record still owns it there");
}

// An ambiguous record (its port absent, and several present ports, or several stored ports, carry
// its name) runs nothing, and neither does its control on a port of its name: a note and a CC64
// learned on one of two identical pedals never play the synth once both come back on new paths.
// A note or a pedal held before the record turned ambiguous still gets its release.
#[test]
fn an_ambiguous_records_control_runs_nothing_on_a_port_of_its_name() {
    let on_pedal = |path: &str, channel: u8, kind: Kind, number: u8, action: ActionId| Binding {
        port_id: port_at(path, "Pedal", None).id,
        port_name: "Pedal".into(),
        channel,
        kind,
        number,
        action,
        target: None,
        press_high: true,
        momentary: false,
        hold: false,
    };
    let usb = |n: u32| format!(r"\\?\usb#{n}");
    // The claimed port 1 binds CC 20 exactly; port 0, where channel 2's note 36 and channel 1's CC64
    // were learned, is gone.
    let store = holding(vec![
        on_pedal(&usb(1), 0, Kind::Cc, 20, ActionId::Undo),
        on_pedal(&usb(0), 1, Kind::Note, 36, ActionId::PlayAll),
        on_pedal(&usb(0), 0, Kind::Cc, 64, ActionId::StopAll),
    ]);
    let r = Rig::with((store, None), None, vec![port_at(&usb(1), "Pedal", Some(A))]);
    let states = |r: &Rig| r.host.listed().iter().map(|l| l.state).collect::<Vec<_>>();
    assert_eq!(states(&r), [BindingState::Live, BindingState::NoPort, BindingState::NoPort]);
    r.send(A, &[[0x91, 36, 100], [0xb0, 64, 127], [0x90, 40, 100], [0x80, 40, 0]]);
    assert_eq!(r.take(), [on(36, 100), on(40, 100)], "36 held, 40 under the pedal");

    r.host.core.set_ports(vec![port_at(&usb(1), "Pedal", Some(A)), port_at(&usb(2), "Pedal", Some(B)), port_at(&usb(3), "Pedal", Some(3))]);
    assert_eq!(states(&r), [BindingState::Live, BindingState::SeveralPorts, BindingState::SeveralPorts]);
    r.send(A, &[[0x81, 36, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(36)], "the held note's release passes");
    r.send(A, &[[0xb0, 64, 0]]);
    assert_eq!(r.take(), [Command::NoteOff(40)], "the pedal's release lets go of what it sustained");
    for conn in [A, B, 3] {
        r.send(conn, &[[0x91, 36, 100], [0x81, 36, 0], [0xb0, 64, 127], [0x90, 41, 100], [0x80, 41, 0], [0xb0, 64, 0]]);
        assert_eq!(r.take(), [on(41, 100), Command::NoteOff(41)], "port {conn}: no action, no note 36, no sustain");
    }
    assert_eq!(r.held(), [] as [u8; 0]);

    // One present port, two stored ports of its name, neither present: none is guessed.
    let store = holding(vec![on_pedal(&usb(0), 0, Kind::Note, 36, ActionId::PlayAll), on_pedal(&usb(9), 0, Kind::Note, 37, ActionId::StopAll)]);
    let r = Rig::with((store, None), None, vec![port_at(&usb(1), "Pedal", Some(A))]);
    assert_eq!(states(&r), [BindingState::SeveralAbsent, BindingState::SeveralAbsent]);
    r.send(A, &[[0x90, 36, 100], [0x90, 37, 100], [0x90, 41, 100]]);
    assert_eq!(r.take(), [on(41, 100)]);
}

// Review fix: a HOLD pedal's press-side message repeated while it holds (a lost release) keeps its
// number; refused (no device), it leaves the hold its first press made, so the release still ends
// the capture.
#[test]
fn a_refused_repeat_of_a_held_hold_press_still_gets_its_release() {
    let mut r = Rig::with((holding(vec![hold_pedal("Probe a", 20)]), None), None, vec![port("Probe a", Some(A))]);
    r.send(A, &[[0xb0, 20, 127]]);
    assert_eq!(r.take(), [Command::Action(Action::Hold(0))]);
    r.running(false);
    r.send(A, &[[0xb0, 20, 127], [0xb0, 20, 0]]);
    r.running(true);
    r.wait(IDLE_RETRY);
    assert_eq!(r.take(), [Command::Action(Action::Release(0))]);
}

// The import never guesses: two present controllers named Pedal, one legacy binding assigned to the
// first. The other stays unresolved: two present ports carry its name, whichever is claimed.
#[test]
fn a_legacy_record_never_follows_a_name_two_present_ports_carry() {
    let (a, b) = (port_at(r"\\?\usb#1", "Pedal", Some(A)), port_at(r"\\?\usb#2", "Pedal", Some(B)));
    let a_id = a.id.clone();
    let r = Rig::with((store::empty(), None), None, vec![a, b]);
    r.host.import_legacy(r.epoch(), &legacy_list(&[legacy("input-1", "Pedal", 20, "playAll"), legacy("input-1", "Pedal", 21, "stopAll")]));
    assert_eq!(r.host.assign(r.epoch(), r.revision(), 0, &a_id), Ok(true));
    assert_eq!(r.states(), [("Pedal".into(), false, BindingState::Live), ("Pedal".into(), true, BindingState::SeveralPorts)]);
    r.send(B, &[[0xb0, 21, 127]]);
    assert_eq!(r.take(), [], "port b does not run it");
}

// Review fix: a listener replaced while events are being handed to the old one (a reload whose
// `subscribe` lands inside the old sink's callback) gets its first events and the held set; the old
// one gets none of them.
#[test]
fn a_new_listener_gets_its_first_events_even_mid_flush() {
    let r = Rig::new();
    let (old, new): (Arc<Mutex<Vec<MidiEvent>>>, Arc<Mutex<Vec<MidiEvent>>>) = Default::default();
    {
        let (old, new, core) = (old.clone(), new.clone(), Arc::downgrade(&r.host.core));
        let replaced = AtomicBool::new(false);
        r.host.subscribe(5_000.0, Box::new(move |e| {
            old.lock().unwrap().push(e);
            if !replaced.swap(true, Relaxed) {
                let (new, core) = (new.clone(), core.upgrade().unwrap());
                assert!(core.subscribe(f64::MAX, Arc::new(move |e| new.lock().unwrap().push(e))).1);
            }
        }));
    }
    assert!(matches!(old.lock().unwrap()[..], [MidiEvent::Ports { .. }]), "only what came before the reload");
    let first = std::mem::take(&mut *new.lock().unwrap());
    assert!(
        matches!(
            first[..],
            [MidiEvent::Ports { .. }, MidiEvent::Bindings { .. }, MidiEvent::Learning { .. }, MidiEvent::AwaitingRelease { .. }, MidiEvent::Held { .. }]
        ),
        "{first:?}"
    );
    let current = r.host.core.lock().router.epoch();
    r.host.learn(current, ActionId::Undo, None);
    assert_eq!(old.lock().unwrap().len(), 1);
    assert!(matches!(new.lock().unwrap()[..], [MidiEvent::Learning { .. }]));
}

/// The engine host as the app has it, its sends recorded: every command reaches the real
/// `EngineHost::send` (its ring and its settings memory) unless the ring is made to refuse.
struct Recorded {
    host: EngineHost,
    sent: Mutex<Vec<Command>>,
    full: AtomicBool,
}

impl EngineSide for Recorded {
    fn send(&self, command: TimedCommand) -> Result<(), String> {
        if self.full.load(Relaxed) {
            return Err("the engine's command ring is full".into());
        }
        self.sent.lock().unwrap().push(command.command);
        self.host.send(command)
    }

    fn running(&self) -> bool {
        EngineSide::running(&self.host)
    }

    fn set_rebuild_hook(&self, hook: Option<Arc<dyn RebuildHook>>) {
        self.host.set_rebuild_hook(hook);
    }
}

/// The device owner's rebuild: a new engine in place of the old, through `swap_engine`.
fn rebuild(io: &super::super::Core) {
    let config = lf_engine::EngineConfig { max_loop_seconds: 1.0, ..lf_engine::EngineConfig::new(48_000) };
    let (engine, handle) = lf_engine::Engine::new(config);
    drop(super::super::owner::swap_engine(io, engine, handle, config));
}

// Review fix: the rebuild's handshake runs while its caller holds every slot port, so it hands the UI
// nothing (a sink that reached `SlotHost::remove` would wait on that thread's own lock); an event
// still waiting at the rebuild goes out from the port thread, with no slot port held.
#[test]
fn a_rebuild_hands_the_ui_nothing_under_the_slot_ports() {
    let io = Arc::new(super::super::Core::new());
    rebuild(&io);
    let engine = Arc::new(Recorded { host: EngineHost { core: io.clone() }, sent: Mutex::default(), full: AtomicBool::new(false) });
    let midi = MidiHost { core: hosted(engine, store::empty(), None, None, None), wake: None, port_thread: None };
    // For each event handed over: whether the sink could take a slot port then.
    let calls: Arc<Mutex<Vec<bool>>> = Arc::default();
    {
        let (calls, io) = (calls.clone(), io.clone());
        midi.subscribe(1.0, Box::new(move |_| calls.lock().unwrap().push(io.ports[0].try_lock().is_ok())));
    }
    calls.lock().unwrap().clear();
    // An event a flush on another thread has not handed over yet.
    midi.core.emit(MidiEvent::Pressed);
    rebuild(&io);
    assert_eq!(*calls.lock().unwrap(), [] as [bool; 0], "nothing during the swap");
    midi.core.tick(Instant::now());
    assert_eq!(*calls.lock().unwrap(), [true], "the port thread hands it over afterwards");
}

// Every live operation takes one ordered path into the engine: GO LIVE's toggle goes straight to
// the ring as the live setters do. A press the full ring refuses is answered at once and never
// applied after a later live-off (a source change's); an accepted one lands before an unload's
// live-off. Rendered, the slot stays off and the settings a rebuild replays agree.
#[test]
fn go_lives_toggle_never_lands_after_a_later_live_off() {
    let io = Arc::new(super::super::Core::new());
    let host = EngineHost { core: io.clone() };
    rebuild(&io);
    io.clock.publish(Instant::now(), 0, 128, 48_000);
    let engine = Arc::new(Recorded { host: host.clone(), sent: Mutex::default(), full: AtomicBool::new(false) });
    let midi = MidiHost { core: hosted(engine.clone(), store::empty(), None, None, None), wake: None, port_thread: None };
    let press = || midi.ui_commands(vec![Command::Action(Action::Toggle(Toggle::Live(0)))]);
    let frame = Cell::new(0);
    // The device's blocks, rendered here (no device runs), then the feed's drain into the settings.
    let render = || {
        let mut rt = io.rt.lock().unwrap();
        let engine = rt.engine.as_mut().expect("an engine");
        let (x, mut l, mut r) = ([0.0f32; 128], [0.0f32; 128], [0.0f32; 128]);
        for _ in 0..4 {
            let ctx = lf_engine::ProcessContext { frame: frame.get(), xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
            engine.process(&ctx, &x, &mut l, &mut r);
            frame.set(frame.get() + 128);
        }
        drop(rt);
        host.drain_events(&mut Vec::new());
        let ends = io.ends.lock().unwrap();
        ends.as_ref().expect("an engine").overview.slot_live(0)
    };
    let live_off = Command::SetSlotLive(0, false);

    engine.full.store(true, Relaxed);
    assert_eq!(press(), Ok(Some(Dropped::Full)), "refused at once, never queued behind the ring");
    engine.full.store(false, Relaxed);
    assert_eq!(midi.ui_commands(vec![live_off]), Ok(None), "a source change's live-off");
    midi.core.tick(Instant::now() + IDLE_RETRY);
    assert!(!render(), "the refused press never lands");

    assert_eq!(press(), Ok(None));
    host.silence_slot(0, Duration::ZERO).expect("an unload's live-off");
    midi.core.tick(Instant::now() + IDLE_RETRY);
    assert!(!render(), "the press, then the live-off");
    let toggle = Command::Action(Action::Toggle(Toggle::Live(0)));
    assert_eq!(*engine.sent.lock().unwrap(), [live_off, toggle], "the unload's live-off goes through the host, after the press");
    let replay = host.settings();
    assert!(replay.contains(&live_off) && !replay.contains(&Command::SetSlotLive(0, true)), "{replay:?}");
}

// A rebuild needs no WebView: an engine rebuild with notes and HOLD held, a refused attack, a HOLD
// and a wheel update queued, and the WebView stalled (nothing here calls the UI's API). The new
// engine gets the target and the wheel once, through the settings replay; no stale one-shot reaches
// it; a later release is harmless and a fresh press attacks again.
#[test]
fn an_engine_rebuild_needs_no_webview() {
    let io = Arc::new(super::super::Core::new());
    let host = EngineHost { core: io.clone() };
    rebuild(&io);
    io.clock.publish(Instant::now(), 0, 128, 48_000);
    let engine = Arc::new(Recorded { host: host.clone(), sent: Mutex::default(), full: AtomicBool::new(false) });
    let core = hosted(engine.clone(), holding(vec![hold_pedal("Probe a", 20), hold_pedal("Probe a", 21)]), None, None, None);
    core.set_ports(vec![port("Probe a", Some(A)), port("Probe b", Some(B))]);
    let midi = MidiHost { core, wake: None, port_thread: None };
    let t = Instant::now();
    let send = |conn: u32, m: [u8; 3]| midi.core.message(conn, t, &m);
    let take = || std::mem::take(&mut *engine.sent.lock().unwrap());

    let epoch = midi.subscribe(1.0, Box::new(|_| {}));
    midi.select_target(epoch, Some(0), LEAD);
    send(A, [0x90, 60, 100]);
    send(A, [0xb0, 20, 127]);
    assert_eq!(take(), [Command::SelectInstrument(LEAD), on(60, 100), Command::Action(Action::Hold(0))]);

    // The ring stalls: an attack, a HOLD press and a wheel update wait in the queue.
    engine.full.store(true, Relaxed);
    send(B, [0x90, 64, 100]);
    send(A, [0xb0, 21, 127]);
    send(A, [0xe0, 0x00, 0x60]);
    // The rebuild begins: an attack now is refused, and no owner recorded.
    let hook = io.rebuild_hook.lock().unwrap().clone().expect("registered at start");
    hook.pause();
    send(B, [0x90, 65, 100]);
    rebuild(&io);
    engine.full.store(false, Relaxed);
    midi.core.tick(t);
    assert_eq!(take(), [], "no stale one-shot, and nothing a second time");
    let bend = (f64::from(0x60u16 << 7) - 8192.0) / 8192.0 * 2.0;
    let replay = host.settings();
    let picked: Vec<&Command> = replay.iter().filter(|c| matches!(c, Command::SelectInstrument(_) | Command::PitchBend(_))).collect();
    assert_eq!(picked, [&Command::SelectInstrument(LEAD), &Command::PitchBend(bend)], "the replay hands them over");

    // Releases of what the old engine held are harmless; the wheel left where it is sends nothing.
    send(A, [0x80, 60, 0]);
    send(B, [0x80, 64, 0]);
    send(A, [0xb0, 20, 0]);
    send(A, [0xb0, 21, 0]);
    send(A, [0xe0, 0x00, 0x60]);
    assert_eq!(take(), []);
    // A fresh press attacks again; HOLD starts from its first control.
    send(B, [0x90, 60, 100]);
    send(A, [0xb0, 20, 127]);
    assert_eq!(take(), [on(60, 100), Command::Action(Action::Hold(0))]);
    assert_eq!(midi.diag().refused.paused, 1);
    assert_eq!(midi.diag().queue.discarded, 2, "the queued attack and HOLD press");
}
