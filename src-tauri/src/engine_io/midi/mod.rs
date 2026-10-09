//! OWNS: native MIDI for the engine, the app's only MIDI path (the WebView's Web MIDI is denied: WinMM
//! input ports may be exclusive, so one path owns them), started and dropped by engine mode
//! (`super::mode`) and driven by the UI through `super::midi_mode`'s commands: the glue that joins
//! the input ports, MIDI learn and its stored bindings, the one note router for every note source and
//! the one ordered queue into the engine ([`Core`]), what the host offers the UI ([`MidiHost`]) and
//! what it tells it ([`MidiEvent`]). This doc is the module's briefing; the decisions it carries out are
//! `docs/plans/native-midi.md`'s (§ Decided).
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | this file | [`MidiHost`] and its UI calls, [`Core`] (the state every thread shares, under one lock), which stored bindings are live, the timers, the UI events | `src/ui/state/midi.ts` (glue) |
//! | `parse` | bytes to `parse::Message`, and what is ignored | `midi.ts` `parseMidiMessage` |
//! | `bindings` | [`Binding`], the 25 actions and their targets, the persisted list's fallible parse | `src/app/midi-actions.ts`, `src/app/actions.ts` |
//! | `learn` | learn capture, matching, consume-first, momentary vs latching, HOLD's control numbers, the release waits | `src/app/midi-actions.ts` |
//! | `actions` | what a binding runs, as engine commands and UI events; which of the UI's commands join the queue | `src/app/actions.ts` |
//! | `router` | note ownership for MIDI and UI owners, sustain, the wheels, the note target, the held-note set | `src/ui/state/input-router.ts`, `instrument.ts` `routeEngine` |
//! | `queue` | the one bounded FIFO into the engine's ring: reserved releases, whole batches, coalesced wheels, no-device admission, the engine generation | plan decisions 4 to 7 |
//! | `store` | `midi-bindings.json` and the one-time import of the web's list | plan decisions 8, 9 |
//! | `ports` | midir connections, port identities, which port a stored binding answers to, the port thread | `midi.ts` `attachInputs` |
//! | `liveness` | interface arrival and removal notifications, connection generations, the arrival retry | (Web MIDI's `statechange`) |
//!
//! # Rules
//!
//! - **One lock orders everything.** The router, the queue, MIDI learn, the store and the port table
//!   sit in one `Mutex` that the midir callbacks, the port thread and the UI's calls take; each decides
//!   and drains the queue into `EngineHost::send` under it, so the engine gets the commands in the order
//!   they were decided. It comes before the engine host's `settings` and `ends`, and nothing holding
//!   those calls in here. UI events are queued under it and handed to the sink once it is released, in
//!   that order. None of these threads is an audio thread (invariant 5).
//! - **A message passes MIDI learn first.** A learned or bound message is consumed there and never
//!   reaches the router (a learned CC64 never sustains); learning a CC lets go of what that owner's
//!   controller set; what a binding fires goes through `actions`; only what learn leaves is played.
//! - **Every input command goes through the queue:** the router's batches, what a binding fires, and
//!   the UI's looper presses, `Press`, `SelectTrack` and toggles ([`MidiHost::ui_commands`]). A setting
//!   goes straight to `EngineHost::send`; a note, a wheel, the note target or a panic from the UI goes
//!   through the router's own calls, never past it (a UI batch that holds one is refused whole, before
//!   any of it runs). Nothing is stamped (decision 4).
//! - **While no device runs** (`EngineSide::running`), fresh one-shots are refused: the router records
//!   no owner for a refused attack, learn no HOLD for a refused press. Releases, pedals, wheels and the
//!   target wait in the queue until a device runs; the port thread looks again every `IDLE_RETRY`, and
//!   every `RETRY` after a drain the engine's full ring refused. A refusal of the UI's input is answered
//!   ([`Dropped`]: the player hears why a press did nothing); a pedal's is counted only.
//! - **An engine rebuild needs no WebView** ([`super::RebuildHook`], registered at start): the queue
//!   pauses, folds its target and wheels into the settings memory before the replay and drops the old
//!   engine's one-shots and their releases; the router forgets what the old engine sounded, learn its
//!   HOLD presses; the queue resumes after the replay. The handshake itself sends nothing and calls no
//!   UI sink (its caller holds every slot port): the port thread, woken, does both afterwards.
//! - **Which stored bindings are live** is decided on one port snapshot (`ports::resolve`) at every
//!   change of the port list or the bindings: exact ids first, by name only where it is unambiguous,
//!   every ordinal (legacy) record of one name counting as one identity (decision 9). A move is saved
//!   (`Store::reanchor`) before learn is handed anything; learn runs only records that are neither
//!   blocked nor ordinal and whose id a present port has. An edit of learn's own (a learn, a pedal read
//!   as momentary) goes back to the store (`Store::replace`), which keeps the records learn does not
//!   run; the player's edits go to the store by listed index and reach learn through the same hand-off,
//!   which releases a HOLD the edit ends.
//! - **An edit by index names the list it was made against:** the `bindings` event carries the store's
//!   revision, and an edit made against another one is refused (nothing changes; the UI already has, or
//!   is about to get, the list as it is), since its index may name another binding by now.
//! - **A record learn does not run still owns its control on a port of its name** (a blocked one, or
//!   one on its legacy ordinal id): that message is consumed and runs nothing, so a pedal waiting for
//!   the player's assignment never plays the synth.
//! - **The store is written on the port thread** (`Core::tick`), from a snapshot taken under the lock,
//!   after it is released. A write refused or failed is a [`MidiEvent::Store`] and a release-log line,
//!   never a panic. Without the app's data folder nothing is written, and the UI hears it as read-only.
//! - **A port that goes away releases what it held:** its notes, its pedal, its wheels and its HOLD
//!   presses; the UI hears [`MidiEvent::Gone`] (the toast).
//! - **A WebView document is its subscription** ([`MidiHost::subscribe`], first thing in its boot): it
//!   gets a fresh input epoch, and in the same locked step the older documents' holds are released, a
//!   pending learn is cancelled (an unloaded page ran no cleanup of its own) and the event sink is
//!   replaced, only when that epoch is the newest (a late subscribe of an older document changes
//!   nothing). Every input event of the UI (a note, a blur, a slot pick, a panic) presents its epoch
//!   and is refused under any other; its engine commands carry none. A blur releases the document's
//!   holds; MIDI holds stay through both.

mod actions;
pub mod bindings;
mod learn;
mod liveness;
mod parse;
mod ports;
mod queue;
mod router;
mod store;

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::{Command, NoteTarget, TimedCommand};
use serde::Serialize;

pub use bindings::{ActionId, Binding, Target};
pub use learn::LearnRefusal;
pub use queue::QueueCounters;
pub use router::Held;
pub use store::{ImportReport, Listed};
/// For the wire fixture's test (`super::wire`), which builds a listed binding and an import report.
#[cfg(test)]
pub(crate) use store::{Blocked, Origin};

use super::{EngineHost, RebuildHook};
use actions::UiRoute;
use bindings::Kind;
use learn::{Fire, Learn};
use liveness::Wake;
use parse::{parse, Message};
use ports::{PortIdentity, Resolution, Unresolved};
use queue::{Out, Queue, Refused};
use router::{HeldNotes, Owner, Router};
use store::{LoadResult, Snapshot, Store, WriteError};

/// How soon the port thread tries again a drain the engine's ring refused (it was full).
const RETRY: Duration = Duration::from_millis(2);
/// How often it looks again while input waits in the queue for a device (or a rebuild) to run it.
const IDLE_RETRY: Duration = Duration::from_millis(20);
/// The most often the engine's refused sends and the queue's refusals for room reach the release log.
const LOG_EVERY: Duration = Duration::from_secs(1);
/// The stored id an ordinal (legacy) record resolves under: no port's id, one for every such record,
/// and of the legacy form, so its port name alone is its identity and every present port with that
/// name counts (decision 9, `ports::resolve`).
const ORDINAL: &str = "input-*";
/// Why the bindings are not written without the app's data folder ([`StoreProblem::ReadOnly`]).
const NO_FOLDER: &str = "no folder to keep MIDI bindings in";

/// What native MIDI needs of the engine host: [`EngineHost`] in the app, a recorder in tests.
pub trait EngineSide: Send + Sync {
    /// Queue one command (`EngineHost::send`, which keeps a setting for the replay); Err when the ring
    /// is full.
    fn send(&self, command: TimedCommand) -> Result<(), String>;
    /// A device runs ([`super::FrameClock::running`]).
    fn running(&self) -> bool;
    /// Register what an engine rebuild tells native MIDI; `None` removes it.
    fn set_rebuild_hook(&self, hook: Option<Arc<dyn RebuildHook>>);
}

impl EngineSide for EngineHost {
    fn send(&self, command: TimedCommand) -> Result<(), String> {
        EngineHost::send(self, command)
    }

    fn running(&self) -> bool {
        self.core.clock.running()
    }

    fn set_rebuild_hook(&self, hook: Option<Arc<dyn RebuildHook>>) {
        EngineHost::set_rebuild_hook(self, hook);
    }
}

/// A present input port, as the device list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    /// What a binding learned on it stores, and what [`MidiHost::assign`] takes.
    pub id: String,
    pub name: String,
    pub state: PortState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PortState {
    Open,
    /// Its last open failed: most likely another program holds it (WinMM input is exclusive). Tried
    /// again every poll.
    Busy,
    /// Not open: a removal notice closed it while Windows still lists it, or it has not opened yet.
    Closed,
}

/// How a stored binding stands on the present ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BindingState {
    /// MIDI learn runs it: a present port has its id.
    Live,
    /// It waits for the player's assignment ([`Listed::blocked`] says why).
    Blocked,
    /// No present port answers to it.
    NoPort,
    /// Several present ports carry its name: none is guessed.
    SeveralPorts,
    /// Another stored port with no present port carries its name too: none is guessed.
    SeveralAbsent,
}

/// A stored binding as the UI lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedBinding {
    #[serde(flatten)]
    pub listed: Listed,
    pub state: BindingState,
}

/// What the next CC or note-on will be learned onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnPick {
    pub action: ActionId,
    pub target: Target,
}

/// An action a binding fired that the UI runs (decision 11), by its `actions.ts` id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UiAction {
    GoLive,
    StageView,
    StageNextView,
    TapTempo,
}

/// Why the bindings are not, or not all, on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum StoreProblem {
    /// The file cannot be read or a newer build wrote it: it is kept as it is, and nothing is written
    /// this session (the bindings work in memory).
    ReadOnly { why: String },
    /// Another instance of the app wrote the file since this one read it: this session's changes are
    /// not saved over it.
    Conflict { why: String },
    /// A write failed; the next change tries again.
    Failed { why: String },
    /// Stored records this build cannot read: kept in the file, not run.
    Rejected { count: usize },
}

/// What native MIDI tells the UI, on its own channel (never the feed's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MidiEvent {
    /// The input ports, as they are now.
    Ports { ports: Vec<PortInfo> },
    /// Ports that went away; their notes and HOLD presses were released (the toast).
    Gone { names: Vec<String> },
    /// Every stored binding, in list order, and how it stands. `revision` is the store's: an edit by
    /// index names the one its list came with ([`MidiHost::forget`] and the other edits).
    Bindings { bindings: Vec<ListedBinding>, revision: u64 },
    /// What a learn listens for, or `None` once it captured or was cancelled.
    Learning { learning: Option<LearnPick> },
    /// The latest learned binding still waiting for its release (the learn row's hint), or `None` once
    /// its wait ended (a native timer, not the next message).
    AwaitingRelease { binding: Option<Binding> },
    /// A learn captured this binding.
    Learned { binding: Binding },
    /// MIDI learn consumed a press and ran nothing.
    Refused { reason: LearnRefusal },
    /// A binding fired an action the UI runs.
    Run { action: UiAction },
    /// A binding fired a looper press (`onPress`: the lane cue goes).
    Pressed,
    Store { problem: StoreProblem },
    /// The notes held down now (not the sustained ones), at most one event per change.
    Held { notes: Vec<u8>, changes: u64 },
}

/// Why an input of the UI's did not reach the engine (`input_send`'s answer: the player hears why a
/// press did nothing). The rest of its batch still ran: a release always passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Dropped {
    /// No device runs: a fresh note-on or press would have fired at the next open (decision 7).
    NoDevice,
    /// The engine is being rebuilt (a device change or a recovery).
    Rebuilding,
    /// No room: the queue's unreserved room was gone, or the engine's ring refused a setting.
    Full,
}

impl Dropped {
    /// What the queue's answer to one batch tells the player. A batch made for another engine generation
    /// is none of the UI's (the UI's are made for the current one).
    fn of(admitted: Result<(), Refused>) -> Option<Dropped> {
        match admitted {
            Ok(()) | Err(Refused::Stale) => None,
            Err(Refused::NoDevice) => Some(Dropped::NoDevice),
            Err(Refused::Paused) => Some(Dropped::Rebuilding),
            Err(Refused::Full) => Some(Dropped::Full),
        }
    }
}

/// A command the UI sends only through the router's own calls (a note, a wheel, the note target, a
/// panic): refused as an engine command, the whole batch with it.
pub fn router_command(command: &Command) -> bool {
    actions::ui_route(command) == UiRoute::Router
}

/// A stored record MIDI learn does not run (blocked, or still on its legacy ordinal id), by the control it
/// names: on a port of its name, that message runs nothing.
struct Unrun {
    port_name: String,
    channel: u8,
    kind: Kind,
    number: u8,
}

impl Unrun {
    fn of(b: &Binding) -> Unrun {
        Unrun { port_name: b.port_name.clone(), channel: b.channel, kind: b.kind, number: b.number }
    }

    /// `message`, from a port named `port_name`, is this record's control (a note's on and off alike).
    fn hears(&self, port_name: &str, message: &Message) -> bool {
        let (kind, number) = match *message {
            Message::Cc { controller, .. } => (Kind::Cc, controller),
            Message::NoteOn { note, .. } | Message::NoteOff { note, .. } => (Kind::Note, note),
            Message::PitchBend { .. } => return false,
        };
        (self.kind, self.number, self.channel) == (kind, number, message.channel()) && self.port_name == port_name
    }
}

/// The queue's refused batches, by why (`queue::Refused`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refusals {
    /// Fresh input while no device ran (decision 7): by design, counted only.
    pub no_device: u64,
    /// Fresh input during an engine rebuild: counted only.
    pub paused: u64,
    /// The unreserved room was gone: in the release log, at most a line a second.
    pub full: u64,
    /// Made for another engine generation: logged as `full` is.
    pub stale: u64,
}

impl Refusals {
    fn count(&mut self, why: Refused) {
        match why {
            Refused::NoDevice => self.no_device += 1,
            Refused::Paused => self.paused += 1,
            Refused::Full => self.full += 1,
            Refused::Stale => self.stale += 1,
        }
    }
}

/// Counters for the DEV probe and the release log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiDiag {
    /// Drains the engine refused (its ring was full): the refused command stayed at the queue's head.
    /// In the release log, at most a line a second.
    pub failed_sends: u64,
    /// Panics caught in a port callback (the message is dropped; midir calls it from an
    /// `extern "system"` WinMM callback, where an unwind would abort the process).
    pub panics: u64,
    pub refused: Refusals,
    pub queue: QueueCounters,
}

/// A present port and its connection (`None` while it is not open).
pub(crate) struct PortEntry {
    pub(crate) identity: PortIdentity,
    /// `identity.id()`, made once.
    id: String,
    pub(crate) conn: Option<u32>,
    /// Its last open failed.
    busy: bool,
}

impl PortEntry {
    pub(crate) fn new(identity: PortIdentity, conn: Option<u32>, busy: bool) -> PortEntry {
        PortEntry { id: identity.id(), identity, conn, busy }
    }
}

fn port_infos(ports: &[PortEntry]) -> Vec<PortInfo> {
    let state = |p: &PortEntry| match (p.conn, p.busy) {
        (Some(_), _) => PortState::Open,
        (None, true) => PortState::Busy,
        (None, false) => PortState::Closed,
    };
    ports.iter().map(|p| PortInfo { id: p.id.clone(), name: p.identity.name.clone(), state: state(p) }).collect()
}

/// The bindings in `data_dir`, and what the UI first hears of them. With no folder they live in memory,
/// and the UI hears so (as read-only: nothing is written).
fn open_store(data_dir: Option<&Path>) -> (Store, Option<StoreProblem>) {
    match data_dir {
        Some(dir) => {
            let (store, result) = store::load(dir);
            (store, loaded(result))
        }
        None => (store::empty(), Some(StoreProblem::ReadOnly { why: NO_FOLDER.into() })),
    }
}

/// The store's load as the UI hears it, said once in the release log.
fn loaded(result: LoadResult) -> Option<StoreProblem> {
    match result {
        LoadResult::Missing => None,
        LoadResult::Loaded { rejected } if rejected.is_empty() => None,
        LoadResult::Loaded { rejected } => {
            log::warn!("[midi] {} stored binding(s) could not be read; the file keeps them", rejected.len());
            Some(StoreProblem::Rejected { count: rejected.len() })
        }
        LoadResult::Invalid(why) => {
            log::error!("[midi] the MIDI bindings cannot be read; nothing is written this session: {why}");
            Some(StoreProblem::ReadOnly { why })
        }
        LoadResult::UnsupportedVersion(version) => {
            let why = format!("a newer build wrote it (version {version})");
            log::error!("[midi] the MIDI bindings file is kept as it is: {why}");
            Some(StoreProblem::ReadOnly { why })
        }
    }
}

struct State {
    router: Router,
    queue: Queue,
    learn: Learn,
    store: Store,
    /// The engine generation the queue feeds, and every batch is made for.
    generation: u64,
    ports: Vec<PortEntry>,
    /// The listed bindings MIDI learn runs, in the order it was handed them.
    live: Vec<usize>,
    /// Every stored binding and how it stands, as the UI last heard it, and the store's revision then.
    bindings: Vec<ListedBinding>,
    heard_revision: u64,
    /// The stored records learn does not run, whose controls run nothing on a port of their name.
    unrun: Vec<Unrun>,
    /// What else the UI last heard.
    heard_ports: Vec<PortInfo>,
    heard_learning: Option<LearnPick>,
    heard_awaiting: Option<Binding>,
    /// The store's revision last handed to a write.
    written: u64,
    /// The port thread knows something is due (the queue holds input, or a write waits).
    armed: bool,
    /// The last drain stopped at a head the engine's ring refused.
    blocked: bool,
    refused: Refusals,
    /// The refusals and failed sends the release log last named, and when.
    logged: (Refusals, u64, Option<Instant>),
    /// The store's last problem, for a subscriber that comes later.
    problem: Option<StoreProblem>,
}

type Sink = Arc<dyn Fn(MidiEvent) + Send + Sync>;

/// The UI events on their way out. Each sink has a generation (raised by every `subscribe`), each
/// event the generation of the sink it was made for, so a sink replaced while another thread hands
/// events over (a reload) never gets the new listener's events, and the new one never misses them.
#[derive(Default)]
struct Events {
    /// The current sink and its generation. Taken before `pending` and `held`.
    sink: Mutex<Option<(u64, Sink)>>,
    /// Queued under the state lock, so they keep the order of what they report.
    pending: Mutex<VecDeque<(u64, MidiEvent)>>,
    /// One thread at a time hands them over.
    flushing: Mutex<()>,
    /// The held set's change count the sink of a generation last heard: (generation, count).
    held: Mutex<(u64, u64)>,
}

/// What the port callbacks, the port thread and [`MidiHost`] share.
pub(crate) struct Core {
    state: Mutex<State>,
    engine: Arc<dyn EngineSide>,
    /// The store's folder; `None`: nothing is written.
    dir: Option<PathBuf>,
    /// Wakes the port thread; `None` without one (the tests run its timers by hand).
    wake: Option<Sender<Wake>>,
    held: HeldNotes,
    events: Events,
    /// The last input epoch handed out (`MidiHost::subscribe`); 0: none yet.
    epochs: AtomicU64,
    failed_sends: AtomicU64,
    panics: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic on a port callback is caught, so a lock may be poisoned: the state stays usable.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    fn new(engine: Arc<dyn EngineSide>, store: Store, problem: Option<StoreProblem>, dir: Option<PathBuf>, wake: Option<Sender<Wake>>) -> Core {
        let router = Router::default();
        let held = router.held().clone();
        let written = store.revision();
        let core = Core {
            state: Mutex::new(State {
                router,
                queue: Queue::new(0),
                learn: Learn::default(),
                store,
                generation: 0,
                ports: Vec::new(),
                live: Vec::new(),
                bindings: Vec::new(),
                heard_revision: 0,
                unrun: Vec::new(),
                heard_ports: Vec::new(),
                heard_learning: None,
                heard_awaiting: None,
                written,
                armed: false,
                blocked: false,
                refused: Refusals::default(),
                logged: (Refusals::default(), 0, None),
                problem,
            }),
            engine,
            dir,
            wake,
            held,
            events: Events::default(),
            epochs: AtomicU64::new(0),
            failed_sends: AtomicU64::new(0),
            panics: AtomicU64::new(0),
        };
        core.refresh(&mut lock(&core.state));
        core
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Queue an event for the UI, under the state lock, for the sink of now; dropped while no one
    /// listens.
    fn emit(&self, event: MidiEvent) {
        let sink = lock(&self.events.sink);
        if let Some((generation, _)) = &*sink {
            lock(&self.events.pending).push_back((*generation, event));
        }
    }

    /// A fresh input epoch, newer than every one before it.
    fn next_epoch(&self) -> u64 {
        self.epochs.fetch_add(1, Relaxed) + 1
    }

    /// The WebView document of input epoch `epoch` listens (`MidiHost::subscribe`). In one step under
    /// the lock, and only when `epoch` is newer than the current document's (an older document's late
    /// subscribe changes nothing): the older documents' holds are released and their later input
    /// refused (`Router::ui_epoch`), a pending learn is cancelled, and `sink` replaces the listener. It
    /// hears first what it needs to draw, queued with the sink in that step, so no thread handing
    /// events over can come between them; the held set goes out after them, whatever its count. True
    /// when `sink` is the listener now.
    fn subscribe(&self, epoch: u64, sink: Sink) -> bool {
        let newest = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let newest = epoch > st.router.epoch();
            if newest {
                let mut out = Vec::new();
                st.router.ui_epoch(epoch, &mut out);
                let _ = self.admit(st, &mut out);
                st.learn.cancel_learn();
                // Drains the releases and settles what learn shows, before the new listener's first
                // events say it.
                self.settle(st);
                let mut current = lock(&self.events.sink);
                let generation = current.as_ref().map_or(1, |(g, _)| g + 1);
                *current = Some((generation, sink));
                let mut first = vec![
                    MidiEvent::Ports { ports: port_infos(&st.ports) },
                    MidiEvent::Bindings { bindings: st.bindings.clone(), revision: st.heard_revision },
                    MidiEvent::Learning { learning: st.heard_learning },
                    MidiEvent::AwaitingRelease { binding: st.heard_awaiting.clone() },
                ];
                first.extend(st.problem.clone().map(|problem| MidiEvent::Store { problem }));
                let mut pending = lock(&self.events.pending);
                pending.clear();
                pending.extend(first.into_iter().map(|event| (generation, event)));
                *lock(&self.events.held) = (generation, u64::MAX);
            }
            newest
        };
        self.flush();
        newest
    }

    /// What to hand over next, and the sink it was made for: the oldest queued event for the sink of
    /// now (one made for a replaced sink is dropped: its successor got a fresh start), else the held
    /// set, once that sink has not heard its count.
    fn next(&self) -> Option<(Sink, MidiEvent)> {
        let current = lock(&self.events.sink);
        let (generation, sink) = current.as_ref()?;
        let mut pending = lock(&self.events.pending);
        while let Some((made_for, event)) = pending.pop_front() {
            if made_for == *generation {
                return Some((sink.clone(), event));
            }
        }
        let held = self.held.read();
        let mut heard = lock(&self.events.held);
        (*heard != (*generation, held.changes)).then(|| {
            *heard = (*generation, held.changes);
            (sink.clone(), MidiEvent::Held { notes: (0..128).filter(|&n| held.contains(n)).collect(), changes: held.changes })
        })
    }

    /// Hand the queued events over, in order, each to the sink it was made for, and the held set when
    /// it changed. Never under the state lock nor an engine lock; a thread that finds another handing
    /// events over leaves its own to that one.
    fn flush(&self) {
        loop {
            let turn = match self.events.flushing.try_lock() {
                Ok(turn) => turn,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            while let Some((sink, event)) = self.next() {
                sink(event);
            }
            drop(turn);
            // Something queued while this thread held the turn, by a thread that left it here.
            let current = lock(&self.events.sink);
            let Some((generation, _)) = current.as_ref() else { return };
            let waiting = !lock(&self.events.pending).is_empty() || *lock(&self.events.held) != (*generation, self.held.read().changes);
            if !waiting {
                return;
            }
        }
    }

    fn wake(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(Wake::Tick);
        }
    }

    /// One input's batch into the queue, whole; a refused attack is undone in the router and the
    /// refusal counted.
    fn admit(&self, st: &mut State, out: &mut Vec<Out>) -> Result<(), Refused> {
        if out.is_empty() {
            return Ok(());
        }
        let admitted = st.queue.admit(out, st.generation, self.engine.running());
        if let Err(why) = admitted {
            st.router.attack_refused(out);
            st.refused.count(why);
        }
        out.clear();
        admitted
    }

    /// What a binding fired: its batch, then its UI events. A refused HOLD press holds nothing.
    fn fire(&self, st: &mut State, fire: Fire) {
        let run = actions::run(fire);
        let mut out = run.out;
        if self.admit(st, &mut out).is_err() {
            if let Fire::HoldPress { control, .. } = fire {
                st.learn.hold_refused(control);
            }
        }
        for event in run.events {
            self.emit(event);
        }
    }

    /// Send what the engine's ring takes, in order, while a device runs.
    fn drain(&self, st: &mut State) {
        st.blocked = false;
        if st.queue.is_empty() || !self.engine.running() {
            return;
        }
        let (engine, failed) = (&self.engine, &self.failed_sends);
        let drained = st.queue.drain(&mut |command| {
            engine.send(command).map_err(|_| {
                failed.fetch_add(1, Relaxed);
            })
        });
        st.blocked = drained.blocked;
    }

    /// The end of every locked call: drain, arm the port thread's timers, tell the UI what learn shows.
    fn settle(&self, st: &mut State) {
        self.drain(st);
        let write_due = self.dir.is_some() && st.store.revision() > st.written;
        if (write_due || !st.queue.is_empty()) && !st.armed {
            st.armed = true;
            self.wake();
        }
        let learning = st.learn.learning().map(|(action, target)| LearnPick { action, target });
        if learning != st.heard_learning {
            st.heard_learning = learning;
            self.emit(MidiEvent::Learning { learning });
        }
        let awaiting = st.learn.awaiting_release().cloned();
        if awaiting != st.heard_awaiting {
            st.heard_awaiting = awaiting.clone();
            self.emit(MidiEvent::AwaitingRelease { binding: awaiting });
        }
    }

    /// Run `f` under the lock with a batch for the queue, then settle and hand the events over.
    fn input<R>(&self, f: impl FnOnce(&Core, &mut State, &mut Vec<Out>) -> R) -> R {
        self.input_admitted(f).0
    }

    /// [`Core::input`], with the queue's answer to the batch `f` left.
    fn input_admitted<R>(&self, f: impl FnOnce(&Core, &mut State, &mut Vec<Out>) -> R) -> (R, Result<(), Refused>) {
        let result = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let mut out = Vec::new();
            let result = f(self, st, &mut out);
            let admitted = self.admit(st, &mut out);
            self.settle(st);
            (result, admitted)
        };
        self.flush();
        result
    }

    /// An input of the WebView document of `epoch`, run only while that is the current document (a
    /// replaced one's late input is refused, every kind of it); what the queue dropped of its batch.
    fn ui(&self, epoch: u64, f: impl FnOnce(&mut State, &mut Vec<Out>)) -> Option<Dropped> {
        let ((), admitted) = self.input_admitted(|_, st, out| {
            if st.router.epoch() == epoch {
                f(st, out);
            }
        });
        Dropped::of(admitted)
    }

    /// A port callback: `bytes` arrived on connection `conn` at `at`.
    pub(crate) fn message(&self, conn: u32, at: Instant, bytes: &[u8]) {
        let Some(message) = parse(bytes) else { return };
        // DEV: where a note reaches native code, for the MIDI benchmark (one atomic load unless it runs).
        #[cfg(debug_assertions)]
        if let Message::NoteOn { note, velocity, .. } = message {
            super::midi_bench::arrived(&[Command::NoteOn(note, f32::from(velocity) / 127.0)], at);
        }
        if catch_unwind(AssertUnwindSafe(|| self.input(|core, st, out| core.route(st, out, conn, at, message)))).is_err() {
            self.panics.fetch_add(1, Relaxed);
        }
    }

    fn route(&self, st: &mut State, out: &mut Vec<Out>, conn: u32, at: Instant, message: Message) {
        // A connection already released (its port went away) has no owner left.
        let Some(port) = st.ports.iter().find(|p| p.conn == Some(conn)) else { return };
        let outcome = st.learn.consume(&port.id, &port.identity.name, &message, at);
        // Not learn's: a record waiting for the player may still own it on this port.
        let unrun = !outcome.consumed && st.unrun.iter().any(|u| u.hears(&port.identity.name, &message));
        let owner = Owner::Midi { conn, channel: message.channel() };
        if let Some(controller) = outcome.release_controller {
            st.router.release_controller(&owner, controller, out);
            let _ = self.admit(st, out);
        }
        for fire in outcome.fire {
            self.fire(st, fire);
        }
        if let Some(reason) = outcome.refused {
            self.emit(MidiEvent::Refused { reason });
        }
        if let Some(binding) = outcome.learned {
            self.emit(MidiEvent::Learned { binding });
        }
        if outcome.changed {
            st.store.replace(&st.live, st.learn.bindings().to_vec());
            self.refresh(st);
        }
        if !(outcome.consumed || unrun) {
            st.router.message(&owner, message, out);
        }
    }

    /// Which stored bindings are live on the present ports (one snapshot): save the moves resolution
    /// makes, hand MIDI learn the live ones (a HOLD whose binding left or changed is released), and
    /// tell the UI when the list changed.
    fn refresh(&self, st: &mut State) {
        let present: Vec<PortIdentity> = st.ports.iter().map(|p| p.identity.clone()).collect();
        let listed = st.store.listed();
        // A blocked record names no identity anyone can trust: it takes no part.
        let candidates: Vec<usize> = (0..listed.len()).filter(|&i| listed[i].blocked.is_none()).collect();
        let stored: Vec<(&str, &str)> = candidates
            .iter()
            .map(|&i| {
                let l = &listed[i];
                (if l.ordinal { ORDINAL } else { l.binding.port_id.as_str() }, l.binding.port_name.as_str())
            })
            .collect();
        let mut resolved: Vec<Option<Resolution>> = vec![None; listed.len()];
        let mut moves: Vec<(String, String, usize)> = Vec::new();
        for (&i, r) in candidates.iter().zip(ports::resolve(&stored, &present)) {
            resolved[i] = Some(r);
            if let Resolution::Port { port, reanchor: true } = r {
                let found = (listed[i].binding.port_id.clone(), listed[i].binding.port_name.clone(), port);
                if !moves.contains(&found) {
                    moves.push(found);
                }
            }
        }
        for (old_id, old_name, port) in moves {
            let (new_id, new_name) = (&st.ports[port].id, &st.ports[port].identity.name);
            let moved = st.store.reanchor(&old_id, &old_name, new_id, new_name);
            if !moved.moved.is_empty() {
                log::info!("[midi] {} binding(s) of {old_name:?} follow it to {new_id}", moved.moved.len());
            }
            if !moved.blocked.is_empty() {
                log::warn!("[midi] {} binding(s) of {old_name:?} wait for the player: {new_id} binds their messages already", moved.blocked.len());
            }
        }

        let listed = st.store.listed();
        let mut live = Vec::new();
        let mut states = Vec::with_capacity(listed.len());
        for (i, l) in listed.iter().enumerate() {
            states.push(match resolved[i] {
                _ if l.blocked.is_some() => BindingState::Blocked,
                None => BindingState::Blocked,
                Some(Resolution::Port { port, .. }) if !l.ordinal && l.binding.port_id == st.ports[port].id => {
                    live.push(i);
                    BindingState::Live
                }
                Some(Resolution::Port { .. }) | Some(Resolution::Unresolved(Unresolved::NoPort)) => BindingState::NoPort,
                Some(Resolution::Unresolved(Unresolved::SeveralPorts)) => BindingState::SeveralPorts,
                Some(Resolution::Unresolved(Unresolved::SeveralAbsent)) => BindingState::SeveralAbsent,
            });
        }
        let fires = st.learn.set_bindings(live.iter().map(|&i| listed[i].binding.clone()).collect());
        st.live = live;
        for fire in fires {
            self.fire(st, fire);
        }
        let bindings: Vec<ListedBinding> = listed.into_iter().zip(states).map(|(listed, state)| ListedBinding { listed, state }).collect();
        st.unrun = bindings.iter().filter(|l| l.listed.blocked.is_some() || l.listed.ordinal).map(|l| Unrun::of(&l.listed.binding)).collect();
        let revision = st.store.revision();
        if bindings != st.bindings || revision != st.heard_revision {
            st.bindings = bindings.clone();
            st.heard_revision = revision;
            self.emit(MidiEvent::Bindings { bindings, revision });
        }
    }

    /// The port thread's table, after each enumeration (each port registered before it connects).
    pub(crate) fn set_ports(&self, ports: Vec<PortEntry>) {
        self.input(|core, st, _| {
            let same = st.ports.len() == ports.len() && st.ports.iter().zip(&ports).all(|(a, b)| a.identity == b.identity);
            st.ports = ports;
            if !same {
                core.refresh(st);
            }
        });
    }

    /// Connection `conn` did not open.
    pub(crate) fn open_failed(&self, conn: u32) {
        if let Some(port) = self.lock().ports.iter_mut().find(|p| p.conn == Some(conn)) {
            port.conn = None;
            port.busy = true;
        }
    }

    /// Connection `conn` is closing: release what it held (its notes, pedal and wheels, its HOLD
    /// presses) and stop taking its messages. Returns the port's name when it was in the table.
    pub(crate) fn port_gone(&self, conn: u32) -> Option<String> {
        self.input(|core, st, out| {
            st.router.release_conn(conn, out);
            let _ = core.admit(st, out);
            let port = st.ports.iter_mut().find(|p| p.conn == Some(conn))?;
            port.conn = None;
            let (id, name) = (port.id.clone(), port.identity.name.clone());
            for fire in st.learn.port_gone(&id) {
                core.fire(st, fire);
            }
            Some(name)
        })
    }

    /// Tell the UI when the port list changed, and which ports went away.
    pub(crate) fn publish_ports(&self, gone: Vec<String>) {
        self.input(|core, st, _| {
            let now = port_infos(&st.ports);
            if now != st.heard_ports {
                st.heard_ports = now.clone();
                core.emit(MidiEvent::Ports { ports: now });
            }
            if !gone.is_empty() {
                core.emit(MidiEvent::Gone { names: gone });
            }
        });
    }

    /// The port thread's timers, at `now`: learn's release waits, the queue's retry, a store write and
    /// the refusals' log line. Returns when it next has work.
    pub(crate) fn tick(&self, now: Instant) -> Option<Instant> {
        let (write, line, next) = {
            let mut guard = self.lock();
            let st = &mut *guard;
            st.learn.tick(now);
            let write = (self.dir.is_some() && st.store.revision() > st.written).then(|| {
                st.written = st.store.revision();
                st.store.snapshot()
            });
            // This is the port thread: nothing to wake.
            st.armed = true;
            self.settle(st);
            st.armed = !st.queue.is_empty();
            let retry = st.armed.then(|| now + if st.blocked { RETRY } else { IDLE_RETRY });
            let (logged, logged_sends, at) = st.logged;
            let sends = self.failed_sends.load(Relaxed);
            let moved = (st.refused.full, st.refused.stale, sends) != (logged.full, logged.stale, logged_sends);
            let line = (moved && at.is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY)).then(|| {
                st.logged = (st.refused, sends, Some(now));
                format!(
                    "since the last line: {} send(s) the engine's full ring refused (each retried), {} batch(es) refused for room, {} made for another engine; queue {:?}",
                    sends - logged_sends,
                    st.refused.full - logged.full,
                    st.refused.stale - logged.stale,
                    st.queue.counters()
                )
            });
            (write, line, [st.learn.next_deadline(), retry].into_iter().flatten().min())
        };
        if let Some(line) = line {
            log::warn!("[midi] {line}");
        }
        if let Some(snapshot) = write {
            self.write(snapshot);
        }
        self.flush();
        next
    }

    /// Write `snapshot` (off the lock); a refusal or a failure is the UI's to show and the release log's.
    fn write(&self, snapshot: Snapshot) {
        let Some(dir) = &self.dir else { return };
        let problem = match snapshot.write(dir) {
            Ok(_) => return,
            Err(WriteError::ReadOnly(why)) => StoreProblem::ReadOnly { why },
            Err(WriteError::Conflict(why)) => StoreProblem::Conflict { why },
            Err(WriteError::Failed(why)) => StoreProblem::Failed { why },
        };
        let mut st = self.lock();
        if st.problem.as_ref() != Some(&problem) {
            log::error!("[midi] the MIDI bindings were not saved: {problem:?}");
        }
        st.problem = Some(problem.clone());
        self.emit(MidiEvent::Store { problem });
    }

    /// The rebuild handshake ([`RebuildHook`]).
    fn pause(&self) {
        self.lock().queue.pause();
    }

    fn rebuild(&self, generation: u64) -> Vec<Command> {
        let mut st = self.lock();
        let fold = st.queue.rebuild(generation);
        st.generation = generation;
        st.router.engine_rebuilt();
        st.learn.clear_holds();
        fold.commands().collect()
    }

    /// The replay is queued. Like `pause` and `rebuild`, this changes the core's own state and
    /// nothing else: its caller holds every slot port, so it neither sends nor tells the UI (a sink that
    /// reached `SlotHost::remove` would wait on a lock its own thread holds). The port thread, woken,
    /// drains what waited and hands the events over (`tick`).
    fn resume(&self) {
        let mut st = self.lock();
        st.queue.resume();
        st.armed = true;
        self.wake();
    }
}

/// The engine host's way into [`Core`]'s rebuild handshake; it keeps no host alive.
struct Rebuild(Weak<Core>);

impl RebuildHook for Rebuild {
    fn pause(&self) {
        if let Some(core) = self.0.upgrade() {
            core.pause();
        }
    }

    fn rebuild(&self, generation: u64) -> Vec<Command> {
        self.0.upgrade().map(|core| core.rebuild(generation)).unwrap_or_default()
    }

    fn resume(&self) {
        if let Some(core) = self.0.upgrade() {
            core.resume();
        }
    }
}

/// A core on `engine`, its rebuild hook registered.
fn hosted(engine: Arc<dyn EngineSide>, store: Store, problem: Option<StoreProblem>, dir: Option<PathBuf>, wake: Option<Sender<Wake>>) -> Arc<Core> {
    let core = Arc::new(Core::new(engine.clone(), store, problem, dir, wake));
    engine.set_rebuild_hook(Some(Arc::new(Rebuild(Arc::downgrade(&core)))));
    core
}

/// Native MIDI input for the engine. Dropping it stops the port thread, which closes every port (their
/// notes and HOLD presses released) and saves what is due, then lets go of what the UI still holds.
pub struct MidiHost {
    core: Arc<Core>,
    /// [`Wake::Stop`] stops the port thread.
    wake: Option<Sender<Wake>>,
    port_thread: Option<JoinHandle<()>>,
}

impl MidiHost {
    /// Load the bindings from `data_dir` (`None`: they live in memory only), register the rebuild
    /// handshake on `engine` and start the port thread: every present input port opens now, and ports
    /// that come and go are followed by Windows' interface notifications, with a poll every second
    /// (`ports::POLL`) as the backstop. The bindings load before any input can run them (decision 9).
    pub fn start(engine: Arc<dyn EngineSide>, data_dir: Option<PathBuf>) -> MidiHost {
        let (store, problem) = open_store(data_dir.as_deref());
        let (wake, woken) = mpsc::channel();
        let core = hosted(engine, store, problem, data_dir, Some(wake.clone()));
        let port_thread = {
            let (core, wake) = (core.clone(), wake.clone());
            std::thread::Builder::new()
                .name("lf-midi-ports".into())
                .spawn(move || ports::run(core, wake, woken))
                .map_err(|e| log::error!("[midi] could not start the port thread: {e}"))
                .ok()
        };
        MidiHost { core, wake: Some(wake), port_thread }
    }

    /// A WebView document subscribes (`midi_subscribe`, first thing in its boot): its input epoch,
    /// which every input of it presents. The older documents' holds are released, a pending learn is
    /// cancelled, and `sink` gets the events from now on (on their own channel), its first ones what a
    /// new listener needs: the ports, the bindings, the learn's state, the store's last problem and the
    /// held notes. An older document's subscribe that runs after a newer one's changes nothing.
    pub fn subscribe(&self, sink: Box<dyn Fn(MidiEvent) + Send + Sync>) -> u64 {
        let epoch = self.core.next_epoch();
        self.core.subscribe(epoch, Arc::from(sink));
        epoch
    }

    /// A pointer or key of document `epoch` (`owner`: `pointer:<id>` or `key:<code>`) pressed or let go
    /// of `note` (velocity 0..127; 0 is a release).
    pub fn ui_note(&self, epoch: u64, owner: String, note: u8, velocity: u8, on: bool) -> Option<Dropped> {
        let owner = Owner::Ui { epoch, id: owner };
        self.core.ui(epoch, |st, out| {
            if on {
                st.router.note_on(&owner, note, velocity, out);
            } else {
                st.router.note_off(&owner, note, out);
            }
        })
    }

    /// The window of document `epoch` lost focus: its keys and pointers are up.
    pub fn ui_blur(&self, epoch: u64) -> Option<Dropped> {
        self.core.ui(epoch, |st, out| st.router.ui_blur(epoch, out))
    }

    /// Document `epoch` moves the notes to `target`, picked on `slot` (`None`: no slot): what sounds is
    /// released first. The same slot and target again change nothing.
    pub fn select_target(&self, epoch: u64, slot: Option<u8>, target: NoteTarget) -> Option<Dropped> {
        self.core.ui(epoch, |st, out| st.router.select_target(slot, target, out))
    }

    /// Document `epoch`'s panic: every note that sounds is released and forgotten, then
    /// `Command::AllNotesOff`.
    pub fn all_notes_off(&self, epoch: u64) -> Option<Dropped> {
        self.core.ui(epoch, |st, out| {
            st.router.release_all(out);
            out.push(Out::new(Command::AllNotesOff));
        })
    }

    /// A batch of the UI's engine commands (`input_send`'s), in order. Its input commands (looper
    /// presses, `Press`, `SelectTrack`, toggles) join the one queue, a run of them as one batch, and while
    /// no device runs they are dropped, as a pedal's are; a setting goes straight to the engine, and may
    /// overtake input still queued behind a full ring. A note, a wheel, the note target or a panic is
    /// refused, the whole batch with it before any of it runs: those go through [`MidiHost::ui_note`],
    /// [`MidiHost::select_target`] and [`MidiHost::all_notes_off`]. Answers why a command was dropped,
    /// the first one's reason; the rest of the batch ran.
    pub fn ui_commands(&self, commands: Vec<Command>) -> Result<Option<Dropped>, String> {
        if let Some(command) = commands.iter().find(|c| router_command(c)) {
            return Err(format!("{command:?} goes through the note router, not as an engine command"));
        }
        let mut dropped = None;
        let mut run: Vec<Out> = Vec::new();
        for command in commands {
            if actions::ui_route(&command) == UiRoute::Queue {
                run.push(Out::new(command));
                continue;
            }
            let queued = self.ui_run(&mut run);
            dropped = dropped.or(queued);
            if self.core.engine.send(TimedCommand { frame: None, command }).is_err() {
                dropped = dropped.or(Some(Dropped::Full));
            }
        }
        let queued = self.ui_run(&mut run);
        Ok(dropped.or(queued))
    }

    /// A run of the UI's input commands into the queue, as one batch.
    fn ui_run(&self, run: &mut Vec<Out>) -> Option<Dropped> {
        if run.is_empty() {
            return None;
        }
        let ((), admitted) = self.core.input_admitted(|_, _, out| out.append(run));
        Dropped::of(admitted)
    }

    /// Learn the next CC or note-on, from any port, onto `action` (on `target` for a lane action; a
    /// global one takes none). Answered by [`MidiEvent::Learned`].
    pub fn learn(&self, action: ActionId, target: Target) {
        self.core.input(|_, st, _| st.learn.learn(action, target));
    }

    /// Stop listening (a learned pedal's wait for its release goes on). True when a learn was pending.
    pub fn cancel_learn(&self) -> bool {
        self.core.input(|_, st, _| st.learn.cancel_learn())
    }

    /// Drop listed binding `index` of the list at `revision` (the `bindings` event's): its messages
    /// reach the play path again, and a HOLD it held is released. False, and nothing done, when the list
    /// changed since; the same for every edit below.
    pub fn forget(&self, revision: u64, index: usize) -> Result<bool, String> {
        self.listed_edit(revision, |st| st.store.forget(index))
    }

    /// Read listed binding `index`'s pedal as momentary or latching; a latching pedal has no HOLD. A
    /// HOLD it held is released, and its pedal's release spent.
    pub fn set_momentary(&self, revision: u64, index: usize, momentary: bool) -> Result<bool, String> {
        self.edit(revision, index, |b| Ok(Binding { momentary, hold: b.hold && momentary, ..b }))
    }

    /// HOLD on or off for listed binding `index`: on for a momentary REC/DUB pedal only.
    pub fn set_hold(&self, revision: u64, index: usize, hold: bool) -> Result<bool, String> {
        self.edit(revision, index, |b| {
            if hold && !(b.momentary && b.action == ActionId::RecDub) {
                return Err("HOLD is for a momentary REC/DUB pedal".to_string());
            }
            Ok(Binding { hold, ..b })
        })
    }

    fn edit(&self, revision: u64, index: usize, f: impl FnOnce(Binding) -> Result<Binding, String>) -> Result<bool, String> {
        self.listed_edit(revision, |st| {
            let b = st.store.listed().into_iter().nth(index).ok_or_else(|| format!("no binding {index}"))?.binding;
            st.store.edit(index, f(b)?)
        })
    }

    /// The player assigns listed binding `index` to the present port `port_id` ([`PortInfo::id`]): a
    /// blocked or unresolved record runs there from now on. Refused when that port already binds its
    /// message.
    pub fn assign(&self, revision: u64, index: usize, port_id: &str) -> Result<bool, String> {
        self.listed_edit(revision, |st| {
            let name = st.ports.iter().find(|p| p.id == port_id).map(|p| p.identity.name.clone()).ok_or_else(|| format!("no port {port_id}"))?;
            st.store.assign(index, port_id, &name)
        })
    }

    /// One of the player's edits by listed index, made against the list of store revision `revision`:
    /// refused (false) when the store changed since, as the index may name another binding by now. The
    /// UI has the list as it is, or is about to: each change of it went out with its revision.
    fn listed_edit(&self, revision: u64, f: impl FnOnce(&mut State) -> Result<(), String>) -> Result<bool, String> {
        self.core.input(|core, st, _| {
            if st.store.revision() != revision {
                return Ok(false);
            }
            f(st)?;
            core.refresh(st);
            Ok(true)
        })
    }

    /// Import the web's list (`lf.midiLearn` verbatim; `"[]"` when it has none), once.
    pub fn import_legacy(&self, json: &str) -> ImportReport {
        self.core.input(|core, st, _| {
            let report = st.store.import_legacy(json);
            core.refresh(st);
            report
        })
    }

    /// Every stored binding, in list order, and how it stands.
    pub fn listed(&self) -> Vec<ListedBinding> {
        self.core.lock().bindings.clone()
    }

    /// The present input ports, in the system's order.
    pub fn ports(&self) -> Vec<PortInfo> {
        port_infos(&self.core.lock().ports)
    }

    /// The notes held down now, read without the lock.
    pub fn held(&self) -> Held {
        self.core.held.read()
    }

    pub fn diag(&self) -> MidiDiag {
        let st = self.core.lock();
        MidiDiag {
            failed_sends: self.core.failed_sends.load(Relaxed),
            panics: self.core.panics.load(Relaxed),
            refused: st.refused,
            queue: st.queue.counters(),
        }
    }
}

#[cfg(test)]
impl MidiHost {
    /// A host on `engine` with no port thread (no port opens) and nothing stored: the command layer's
    /// tests (`super::midi_mode`).
    pub(crate) fn detached(engine: Arc<dyn EngineSide>) -> MidiHost {
        MidiHost { core: hosted(engine, store::empty(), None, None, None), wake: None, port_thread: None }
    }
}

impl Drop for MidiHost {
    fn drop(&mut self) {
        self.core.engine.set_rebuild_hook(None);
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(Wake::Stop);
        }
        if let Some(port_thread) = self.port_thread.take() {
            let _ = port_thread.join();
        }
        self.core.input(|_, st, out| st.router.release_all(out));
        self.core.tick(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        /// The input epoch of the document the rig's listener stands for (its first subscribe's).
        epoch: u64,
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
            let mut rig = Rig { host: MidiHost { core, wake: None, port_thread: None }, engine, events: Arc::default(), t: Instant::now(), epoch: 0 };
            rig.epoch = rig.subscribe();
            rig.events();
            rig
        }

        /// A new listener (a document's subscribe): it hears what it needs to draw first. Its epoch.
        fn subscribe(&self) -> u64 {
            let events = self.events.clone();
            self.host.subscribe(Box::new(move |e| events.lock().unwrap().push(e)))
        }

        /// The store revision the UI last heard with the list (what an edit names).
        fn revision(&self) -> u64 {
            self.host.core.lock().heard_revision
        }

        /// A note of the rig's document.
        fn key(&self, owner: &str, note: u8, on: bool) -> Option<Dropped> {
            self.host.ui_note(self.epoch, owner.into(), note, if on { 100 } else { 0 }, on)
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
            self.host.learn(action, target);
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
        r.host.learn(ActionId::PlayAll, Some(3));
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

    // Plan decision 1: one router for every note source. A pointer, a key and a port on one note: one
    // strike, one release when the last lets go; the held set reaches the UI once per change.
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

    // Step 4: "a target switch then a note-on back to back sounds on the new target", also when both
    // wait behind a ring that refused, which the port thread retries soon.
    #[test]
    fn a_target_switch_then_a_note_on_land_in_that_order() {
        let mut r = Rig::new();
        r.host.select_target(r.epoch, Some(0), LEAD);
        r.send(A, &[[0x90, 60, 100]]);
        assert_eq!(r.take(), [Command::SelectInstrument(LEAD), on(60, 100)]);
        r.engine.full.store(true, Relaxed);
        r.host.select_target(r.epoch, Some(1), PAD);
        r.send(B, &[[0x90, 64, 100]]);
        assert_eq!(r.take(), []);
        let now = r.t;
        assert_eq!(r.wait(Duration::ZERO), Some(now + RETRY), "a refused drain is retried soon");
        r.engine.full.store(false, Relaxed);
        assert_eq!(r.wait(RETRY), None, "nothing left to retry");
        assert_eq!(r.take(), [Command::NoteOff(60), Command::SelectInstrument(PAD), on(64, 100)]);
        assert!(r.host.diag().failed_sends > 0);
    }

    // Plan decision 5: the UI's input commands join the one queue, so a pedal and a click keep their
    // order and fall under the same no-device rule (the UI hears why its press did nothing); a setting
    // goes straight to the engine; a note goes through the router only, and a batch holding one is
    // refused before any of it runs.
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
        let report = r.host.import_legacy(&legacy_list(&[legacy("input-3", "Pedal", 20, "recDub")]));
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
        r.host.import_legacy(&legacy_list(&[legacy("input-3", "Pedal", 20, "recDub")]));
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
        r.host.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20, "playAll"), legacy("input-1", "Keys", 20, "stopAll")]));
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

    // The player's edits go to the store by listed index and reach MIDI learn through its hand-off: a
    // HOLD an edit ends is released and its pedal's release spent (midi-actions.ts update); a forgotten
    // binding hands its message back; an assignment runs a blocked record on the port picked.
    #[test]
    fn the_players_edits_reach_learn_and_release_what_they_end() {
        let r = Rig::with((holding(vec![hold_pedal("Probe a", 20)]), None), None, vec![port("Probe a", Some(A)), port("Probe b", Some(B))]);
        r.send(A, &[[0xb0, 20, 127]]);
        assert_eq!(r.take(), [Command::Action(Action::Hold(0))]);
        assert_eq!(r.host.set_momentary(r.revision(), 0, false), Ok(true));
        assert_eq!(r.take(), [Command::Action(Action::Release(0))]);
        assert_eq!((r.host.listed()[0].listed.binding.momentary, r.host.listed()[0].listed.binding.hold), (false, false));
        r.send(A, &[[0xb0, 20, 0]]);
        assert_eq!(r.take(), [], "the pedal's own release is spent");
        r.send(A, &[[0xb0, 20, 0]]);
        assert_eq!(r.take(), [Command::Action(Action::RecDub)], "latching now");
        assert!(r.host.set_hold(r.revision(), 0, true).is_err(), "a latching pedal has no HOLD");
        assert_eq!(r.host.forget(r.revision(), 0), Ok(true));
        assert_eq!(r.host.listed(), []);
        r.send(A, &[[0xb0, 20, 127]]);
        assert_eq!(r.take(), [], "the message reaches the play path again, where CC20 does nothing");

        r.host.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20, "undo"), legacy("input-3", "Pedal", 20, "undo")]));
        assert!(r.states().iter().all(|s| s.2 == BindingState::Blocked));
        assert!(r.host.assign(r.revision(), 0, "winmm:0:nowhere:Pedal").is_err(), "no such port");
        assert_eq!(r.host.assign(r.revision(), 0, &id("Probe b")), Ok(true));
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

    // Step 4: "a retained-engine device gap with a HOLD press and its release" (decision 7). The release
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
        let old = r.epoch;
        r.key("pointer:1", 60, true);
        r.key("key:KeyA", 62, true);
        r.send(A, &[[0x90, 64, 100]]);
        r.host.learn(ActionId::Undo, None);
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

        r.host.ui_note(old, "key:KeyB".into(), 65, 100, true);
        r.host.select_target(old, Some(1), PAD);
        r.host.all_notes_off(old);
        r.host.ui_blur(old);
        assert_eq!(r.take(), [], "a late input of the old document, whatever its kind");
        assert_eq!(r.held(), [64]);
        r.host.ui_note(new, "key:KeyA".into(), 67, 100, true);
        r.host.ui_blur(new);
        assert_eq!(r.take(), [on(67, 100), Command::NoteOff(67)]);
        assert_eq!(r.held(), [64]);
        r.host.select_target(new, Some(1), PAD);
        assert_eq!(r.take(), [Command::NoteOff(64), Command::SelectInstrument(PAD)], "the new document's slot pick runs");
    }

    // A subscribe of an older document that runs after a newer one's (its call was late) changes nothing:
    // the newer page keeps the events, its holds and its learn.
    #[test]
    fn an_older_documents_late_subscribe_changes_nothing() {
        let r = Rig::new();
        let older = r.host.core.next_epoch();
        let newer = r.subscribe();
        r.host.ui_note(newer, "pointer:1".into(), 60, 100, true);
        r.host.learn(ActionId::Undo, None);
        r.take();
        r.events();
        let late: Arc<Mutex<Vec<MidiEvent>>> = Arc::default();
        let sink = late.clone();
        assert!(!r.host.core.subscribe(older, Arc::new(move |e| sink.lock().unwrap().push(e))));
        assert_eq!(r.take(), [], "the newer page's note holds");
        r.host.ui_note(older, "pointer:1".into(), 61, 100, true);
        assert_eq!(r.take(), [], "the older document's input stays refused");
        r.host.cancel_learn();
        assert_eq!(*late.lock().unwrap(), [], "the late subscriber hears nothing");
        assert_eq!(r.events(), [MidiEvent::Learning { learning: None }], "the newer page still does, and its learn ran until it cancelled it");
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
        assert_eq!(r.host.forget(seen, 0), Ok(true));
        let revision = r.revision();
        assert!(revision > seen);
        let heard: Vec<u64> = r.events().into_iter().filter_map(|e| if let MidiEvent::Bindings { revision, .. } = e { Some(revision) } else { None }).collect();
        assert_eq!(heard, [revision], "the new list went out with its revision");
        let numbers = |r: &Rig| r.host.listed().iter().map(|l| (l.listed.binding.number, l.listed.binding.momentary)).collect::<Vec<_>>();
        assert_eq!(r.host.set_momentary(seen, 1, false), Ok(false), "made against the list before the forget");
        assert_eq!(r.host.forget(seen, 1), Ok(false));
        assert_eq!(r.host.assign(seen, 0, &id("Probe a")), Ok(false));
        assert_eq!(r.host.set_hold(seen, 0, false), Ok(false));
        assert_eq!(numbers(&r), [(21, true), (22, true)], "nothing changed");
        assert_eq!(r.revision(), revision);
        assert_eq!(r.host.set_momentary(revision, 1, false), Ok(true), "made against the list as it is");
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
        r.host.import_legacy(&legacy_list(&records));
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
        assert_eq!(r.host.assign(r.revision(), 2, &port_at(r"\\?\usb#1", "Pedal", None).id), Ok(true));
        r.send(A, &[[0x90, 36, 100]]);
        assert_eq!(r.take(), [Command::Action(Action::PlayAll)]);
        r.send(B, &[[0x90, 36, 100]]);
        assert_eq!(r.take(), [], "the blocked record still owns it there");
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

    // Review fix (decision 9): two present controllers named Pedal, one legacy binding assigned to the
    // first. The other stays unresolved: two present ports carry its name, whichever is claimed.
    #[test]
    fn a_legacy_record_never_follows_a_name_two_present_ports_carry() {
        let (a, b) = (port_at(r"\\?\usb#1", "Pedal", Some(A)), port_at(r"\\?\usb#2", "Pedal", Some(B)));
        let a_id = a.id.clone();
        let r = Rig::with((store::empty(), None), None, vec![a, b]);
        r.host.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20, "playAll"), legacy("input-1", "Pedal", 21, "stopAll")]));
        assert_eq!(r.host.assign(r.revision(), 0, &a_id), Ok(true));
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
            r.host.subscribe(Box::new(move |e| {
                old.lock().unwrap().push(e);
                if !replaced.swap(true, Relaxed) {
                    let (new, core) = (new.clone(), core.upgrade().unwrap());
                    assert!(core.subscribe(core.next_epoch(), Arc::new(move |e| new.lock().unwrap().push(e))));
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
        r.host.learn(ActionId::Undo, None);
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
            midi.subscribe(Box::new(move |_| calls.lock().unwrap().push(io.ports[0].try_lock().is_ok())));
        }
        calls.lock().unwrap().clear();
        // An event a flush on another thread has not handed over yet.
        midi.core.emit(MidiEvent::Pressed);
        rebuild(&io);
        assert_eq!(*calls.lock().unwrap(), [] as [bool; 0], "nothing during the swap");
        midi.core.tick(Instant::now());
        assert_eq!(*calls.lock().unwrap(), [true], "the port thread hands it over afterwards");
    }

    // Step 4 and decision 6: "an engine rebuild with notes and HOLD held, a refused attack, a HOLD and a
    // wheel update queued, and the WebView stalled" (nothing here calls the UI's API). The new engine
    // gets the target and the wheel once, through the settings replay; no stale one-shot reaches it; a
    // later release is harmless and a fresh press attacks again.
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

        let epoch = midi.subscribe(Box::new(|_| {}));
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
}
