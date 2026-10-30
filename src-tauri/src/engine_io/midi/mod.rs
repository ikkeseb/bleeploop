//! OWNS: native MIDI for the engine, the app's only MIDI path (the WebView's Web MIDI is denied: WinMM
//! input ports may be exclusive, so one path owns them), started and dropped by engine mode
//! (`super::mode`) and driven by the UI through `super::midi_mode`'s commands: the glue that joins
//! the input ports, MIDI learn and its stored bindings, the one note router for every note source and
//! the one ordered queue into the engine ([`Core`]), what the host offers the UI ([`MidiHost`]) and
//! what it tells it ([`MidiEvent`]). This doc is the module's briefing; the decisions it carries out are
//! `docs/ARCHITECTURE.md` § Decided: native MIDI's, which comments here cite by their bold titles
//! (the open threads, and what only a device can settle: `src-tauri/OPEN-THREADS.md`).
//!
//! # Module map
//!
//! | Module | Owns | Ported from |
//! |---|---|---|
//! | this file | [`MidiHost`] and its UI calls, [`Core`] (the state every thread shares, under one lock), which stored bindings are live, the timers, the UI events | `src/ui/state/midi.ts` (glue) |
//! | `parse` | bytes to `parse::Message`, and what is ignored | `midi.ts` `parseMidiMessage` (removed) |
//! | `bindings` | [`Binding`], the 26 actions and their targets, the persisted list's fallible parse | `src/app/midi-actions.ts`, `src/app/actions.ts` |
//! | `learn` | learn capture, matching, consume-first, momentary vs latching, HOLD's control numbers, the release waits | `src/app/midi-actions.ts` |
//! | `actions` | what a binding runs, as engine commands and UI events; which of the UI's commands join the queue | `src/app/actions.ts` |
//! | `router` | note ownership for MIDI and UI owners, sustain, the wheels, the note target, the held-note set | `src/ui/state/input-router.ts` (removed), `instrument.ts` `routeEngine` |
//! | `queue` | the one bounded FIFO into the engine's ring: reserved releases, whole batches, coalesced wheels, no-device admission, what a rebuild empties and folds | (new: nothing is stamped, the no-device rule, a rebuild needs no WebView) |
//! | `store` | `midi-bindings.json` and the one-time import of the web's list | (new: bindings live native, the import never guesses) |
//! | `ports` | midir connections, port identities, which port a stored binding answers to, the port thread | `midi.ts` `attachInputs` (removed) |
//! | `liveness` | interface arrival and removal notifications, which open connections a removal ends, the arrival retry | (Web MIDI's `statechange`) |
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
//! - **What joins the queue** (the rules: **One ordered path into the engine**, **Nothing is
//!   stamped**): the router's batches, what a binding fires, and the UI's looper presses, `Press`,
//!   `SelectTrack` and toggles ([`MidiHost::ui_commands`]). A setting and GO LIVE's toggle go straight
//!   to `EngineHost::send` (**Every toggle has one owner, the engine**); a note, a wheel, the note
//!   target or a panic from the UI goes through the router's own calls, never past it (a UI batch that
//!   holds one is refused whole, before any of it runs).
//! - **No device here** (the rule: **The no-device rule**, `EngineSide::running`): the router records
//!   no owner for a refused attack, learn no HOLD for a refused press. Releases, pedals, wheels and the
//!   target wait in the queue until a device runs; the port thread looks again every `IDLE_RETRY`, and every `RETRY` after a
//!   drain the engine's full ring refused. A refusal of the UI's input is answered ([`Dropped`]: the
//!   player hears why a press did nothing); a pedal's is counted only.
//! - **The rebuild handshake** (the rule: **A rebuild needs no WebView**; [`super::RebuildHook`],
//!   registered at start): the queue pauses, folds its target and wheels into the settings memory
//!   before the replay and drops the old engine's one-shots and their releases; the router forgets
//!   what the old engine sounded, learn its HOLD presses; the queue resumes after the replay. The
//!   handshake itself sends nothing and calls no UI sink (its caller holds every slot port): the port
//!   thread, woken, does both afterwards.
//! - **Which stored bindings are live** is decided on one port snapshot (`ports::resolve`) at every
//!   change of the port list or the bindings: exact ids first, by name only where it is unambiguous,
//!   every ordinal (legacy) record of one name counting as one identity (the import never guesses). A
//!   move is saved (`Store::reanchor`) before learn is handed anything; learn runs only records that
//!   are neither blocked nor ordinal and whose id a present port has. An edit of learn's own (a learn,
//!   a pedal read as momentary) goes back to the store (`Store::replace`), which keeps the records
//!   learn does not run; the player's edits go to the store by listed index and reach learn through the
//!   same hand-off, which releases a HOLD the edit ends.
//! - **An edit by index names the list it was made against:** the `bindings` event carries the store's
//!   revision, and an edit made against another one is refused (nothing changes; the UI already has, or
//!   is about to get, the list as it is), since its index may name another binding by now.
//! - **A record learn does not run still owns its control on a port of its name** (a blocked one, one
//!   on its legacy ordinal id, or an ambiguous one: several ports carry its name): that message is
//!   consumed and runs nothing, so a pedal waiting for the player's assignment never plays the synth. A release the router still owes its owner passes (a
//!   note-off of a held note, a pedal-up of a pedal down): the record may have arrived while it was held.
//! - **The store is written on the port thread** (`Core::tick`), from a snapshot taken under the lock,
//!   after it is released. A write refused or failed is a [`MidiEvent::Store`] and a release-log line,
//!   never a panic; a failed one is tried again on a bounded backoff, and a later success clears it. Without the app's data folder nothing is written, and the UI hears it as read-only.
//! - **A port that goes away releases what it held:** its notes, its pedal, its wheels and its HOLD
//!   presses; the UI hears [`MidiEvent::Gone`] (the toast).
//! - **The subscription's locked step** (the rule: **A page is its subscription**;
//!   [`MidiHost::subscribe`]): the new epoch, the older pages' released holds, the cancelled learn (an
//!   unloaded page ran no cleanup of its own) and the replaced event sink happen under the one lock; an
//!   older page's subscribe gets an epoch that is never current. The epoch checks the UI's input
//!   events (a note, a blur, a slot pick, a panic) and learn calls; a refused input event is answered
//!   ([`Dropped::Stale`]), and the UI's engine commands carry no epoch. MIDI holds stay through a
//!   resubscribe and a blur.

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
#[cfg(test)]
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
/// The wait before a failed store write is tried again; it doubles with each failure, up to
/// [`WRITE_RETRY_MAX`].
const WRITE_RETRY_FIRST: Duration = Duration::from_secs(1);
const WRITE_RETRY_MAX: Duration = Duration::from_secs(60);
/// The stored id an ordinal (legacy) record resolves under: no port's id, one for every such record,
/// and of the legacy form, so its port name alone is its identity and every present port with that
/// name counts (the import never guesses, `ports::resolve`).
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

/// An action a binding fired that the UI runs (GO LIVE, TAP, the stage view: `actions`), by its
/// `actions.ts` id.
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
    /// A write failed; it is tried again on a backoff ([`WRITE_RETRY_FIRST`] doubling up to
    /// [`WRITE_RETRY_MAX`]) and at the next change. A later success clears it.
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
    /// No device runs: a fresh note-on or press would have fired at the next open (the no-device rule).
    NoDevice,
    /// The engine is being rebuilt (a device change or a recovery).
    Rebuilding,
    /// No room: the queue's unreserved room was gone, or the engine's ring refused a setting.
    Full,
    /// The batch's epoch is not the current page's: its input events were refused (a newer page
    /// subscribed, or this one's subscribe was refused as older).
    Stale,
}

impl Dropped {
    /// What the queue's answer to one batch tells the player.
    fn of(admitted: Result<(), Refused>) -> Option<Dropped> {
        match admitted {
            Ok(()) => None,
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

/// A stored record MIDI learn does not run (blocked, still on its legacy ordinal id, or ambiguous:
/// [`BindingState::SeveralPorts`], [`BindingState::SeveralAbsent`]), by the control it names: on a port
/// of its name, that message runs nothing.
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
    /// Fresh input while no device ran (the no-device rule): by design, counted only.
    pub no_device: u64,
    /// Fresh input during an engine rebuild: counted only.
    pub paused: u64,
    /// The unreserved room was gone: in the release log, at most a line a second.
    pub full: u64,
}

impl Refusals {
    fn count(&mut self, why: Refused) {
        match why {
            Refused::NoDevice => self.no_device += 1,
            Refused::Paused => self.paused += 1,
            Refused::Full => self.full += 1,
        }
    }
}

/// The counters the tests read (the release log names them: `Core::tick`).
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiDiag {
    /// Drains the engine refused (its ring was full): the refused command stayed at the queue's head.
    pub failed_sends: u64,
    pub refused: Refusals,
    pub queue: queue::QueueCounters,
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
    /// After a failed write: when the port thread tries again, and the wait that led there.
    write_retry: Option<(Instant, Duration)>,
    /// The port thread knows something is due (the queue holds input, or a write waits).
    armed: bool,
    /// The last drain stopped at a head the engine's ring refused.
    blocked: bool,
    refused: Refusals,
    /// The refusals and failed sends the release log last named, and when.
    logged: (Refusals, u64, Option<Instant>),
    /// The store's last problem, for a subscriber that comes later.
    problem: Option<StoreProblem>,
    /// The age (`performance.timeOrigin`) of the newest page that subscribed, and the last input epoch
    /// handed out (0: none yet).
    origin: Option<f64>,
    epochs: u64,
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
    /// Drains the engine refused (its ring was full): in the release log, at most a line a second.
    failed_sends: AtomicU64,
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
                queue: Queue::new(),
                learn: Learn::default(),
                store,
                ports: Vec::new(),
                live: Vec::new(),
                bindings: Vec::new(),
                heard_revision: 0,
                unrun: Vec::new(),
                heard_ports: Vec::new(),
                heard_learning: None,
                heard_awaiting: None,
                written,
                write_retry: None,
                armed: false,
                blocked: false,
                refused: Refusals::default(),
                logged: (Refusals::default(), 0, None),
                problem,
                origin: None,
                epochs: 0,
            }),
            engine,
            dir,
            wake,
            held,
            events: Events::default(),
            failed_sends: AtomicU64::new(0),
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

    /// The WebView page of age `origin` (its `performance.timeOrigin`) listens (`MidiHost::subscribe`).
    /// In one step under the lock: it gets a fresh input epoch, and when no page newer than it subscribed
    /// before (a late subscribe of an older page changes nothing more), the older pages' holds are
    /// released and their later input refused (`Router::ui_epoch`), a pending learn is cancelled, and
    /// `sink` replaces the listener. It hears first what it needs to draw, queued with the sink in that
    /// step, so no thread handing events over can come between them; the held set goes out after them,
    /// whatever its count. The epoch, and whether `sink` is the listener now.
    fn subscribe(&self, origin: f64, sink: Sink) -> (u64, bool) {
        let answer = {
            let mut guard = self.lock();
            let st = &mut *guard;
            st.epochs += 1;
            let epoch = st.epochs;
            // The same page again (a remount) is no older than itself.
            let newest = origin.is_finite() && st.origin.is_none_or(|o| origin >= o);
            if newest {
                st.origin = Some(origin);
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
            (epoch, newest)
        };
        self.flush();
        answer
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
        let admitted = st.queue.admit(out, self.engine.running());
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

    /// An input of the WebView page of `epoch`, run only while that is the current page (a replaced
    /// one's late input is refused, every kind of it: [`Dropped::Stale`]); what the queue dropped of its
    /// batch.
    fn ui(&self, epoch: u64, f: impl FnOnce(&mut State, &mut Vec<Out>)) -> Option<Dropped> {
        let (current, admitted) = self.input_admitted(|_, st, out| {
            let current = st.router.epoch() == epoch;
            if current {
                f(st, out);
            }
            current
        });
        if current { Dropped::of(admitted) } else { Some(Dropped::Stale) }
    }

    /// A learn call of the page of `epoch` (`MidiHost::learn` and the edits), run only while that is the
    /// current page, so an older page's call that arrives late changes nothing; `None` when refused.
    fn current<R>(&self, epoch: u64, f: impl FnOnce(&Core, &mut State) -> R) -> Option<R> {
        self.input(|core, st, _| (st.router.epoch() == epoch).then(|| f(core, st)))
    }

    /// A port callback: `bytes` arrived on connection `conn` at `at`.
    pub(crate) fn message(&self, conn: u32, at: Instant, bytes: &[u8]) {
        let Some(message) = parse(bytes) else { return };
        // DEV: where a note reaches native code, for the MIDI benchmark (one atomic load unless it runs).
        #[cfg(debug_assertions)]
        if let Message::NoteOn { note, velocity, .. } = message {
            super::midi_bench::arrived(&[Command::NoteOn(note, f32::from(velocity) / 127.0)], at);
        }
        // midir calls this from an `extern "system"` WinMM callback, where an unwind would abort the
        // process: a panic drops the message (the panic hook logs it).
        let _ = catch_unwind(AssertUnwindSafe(|| self.input(|core, st, out| core.route(st, out, conn, at, message))));
    }

    fn route(&self, st: &mut State, out: &mut Vec<Out>, conn: u32, at: Instant, message: Message) {
        // A connection already released (its port went away) has no owner left.
        let Some(port) = st.ports.iter().find(|p| p.conn == Some(conn)) else { return };
        let outcome = st.learn.consume(&port.id, &port.identity.name, &message, at);
        let owner = Owner::Midi { conn, channel: message.channel() };
        // Not learn's: a record waiting for the player may still own it on this port, but for a release
        // the router owes (its note or pedal was held before the record arrived).
        let unrun = !outcome.consumed
            && st.unrun.iter().any(|u| u.hears(&port.identity.name, &message))
            && !st.router.owes_release(&owner, &message);
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
    /// makes, hand MIDI learn the live ones (a HOLD whose binding left or changed is released, and
    /// what the router holds under a control that just went live: [`Core::release_bound`]), and tell
    /// the UI when the list changed.
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
        let before = st.learn.bindings().to_vec();
        let fires = st.learn.set_bindings(live.iter().map(|&i| listed[i].binding.clone()).collect());
        st.live = live;
        self.release_bound(st, &before);
        for fire in fires {
            self.fire(st, fire);
        }
        let bindings: Vec<ListedBinding> = listed.into_iter().zip(states).map(|(listed, state)| ListedBinding { listed, state }).collect();
        let unrun = |l: &ListedBinding| {
            l.listed.blocked.is_some() || l.listed.ordinal || matches!(l.state, BindingState::SeveralPorts | BindingState::SeveralAbsent)
        };
        st.unrun = bindings.iter().filter(|l| unrun(l)).map(|l| Unrun::of(&l.listed.binding)).collect();
        let revision = st.store.revision();
        if bindings != st.bindings || revision != st.heard_revision {
            st.bindings = bindings.clone();
            st.heard_revision = revision;
            self.emit(MidiEvent::Bindings { bindings, revision });
        }
    }

    /// The bindings live now that were not (`before`, learn's list before the change): learn consumes
    /// their messages from now on, so a note or pedal one's port and channel holds through the router
    /// would never hear its release. The router lets go of it now, for that owner only (another port
    /// or channel holding the same note keeps it): the note's release, the controller's (a pedal-up
    /// for CC64, as a learn of it does).
    fn release_bound(&self, st: &mut State, before: &[Binding]) {
        let control = |b: &Binding| (b.port_id.clone(), b.channel, b.kind, b.number);
        let was: Vec<_> = before.iter().map(control).collect();
        let bound: Vec<(Owner, Kind, u8)> = st
            .learn
            .bindings()
            .iter()
            .filter(|b| !was.contains(&control(b)))
            .filter_map(|b| {
                let conn = st.ports.iter().find(|p| p.id == b.port_id)?.conn?;
                Some((Owner::Midi { conn, channel: b.channel }, b.kind, b.number))
            })
            .collect();
        let mut out = Vec::new();
        for (owner, kind, number) in bound {
            match kind {
                Kind::Note => st.router.note_off(&owner, number, &mut out),
                Kind::Cc => st.router.release_controller(&owner, number, &mut out),
            }
        }
        let _ = self.admit(st, &mut out);
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

    /// The port thread's timers, at `now`: learn's release waits, the queue's retry, a store write (a
    /// change, or a failed write's retry once due) and the refusals' log line. Returns when it next has
    /// work.
    pub(crate) fn tick(&self, now: Instant) -> Option<Instant> {
        let (write, line, next) = {
            let mut guard = self.lock();
            let st = &mut *guard;
            st.learn.tick(now);
            let changed = st.store.revision() > st.written;
            let retry_due = st.write_retry.is_some_and(|(at, _)| now >= at);
            let write = (self.dir.is_some() && (changed || retry_due)).then(|| {
                st.written = st.store.revision();
                (st.store.snapshot(), !changed)
            });
            // A retry not yet due keeps its deadline; one written now gets its next from the write.
            let write_at = st.write_retry.filter(|_| write.is_none()).map(|(at, _)| at);
            // This is the port thread: nothing to wake.
            st.armed = true;
            self.settle(st);
            st.armed = !st.queue.is_empty();
            let retry = st.armed.then(|| now + if st.blocked { RETRY } else { IDLE_RETRY });
            let (logged, logged_sends, at) = st.logged;
            let sends = self.failed_sends.load(Relaxed);
            let moved = (st.refused.full, sends) != (logged.full, logged_sends);
            let line = (moved && at.is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY)).then(|| {
                st.logged = (st.refused, sends, Some(now));
                format!(
                    "since the last line: {} send(s) the engine's full ring refused (each retried), {} batch(es) refused for room; queue {:?}",
                    sends - logged_sends,
                    st.refused.full - logged.full,
                    st.queue.counters()
                )
            });
            (write, line, [st.learn.next_deadline(), retry, write_at].into_iter().flatten().min())
        };
        if let Some(line) = line {
            log::warn!("[midi] {line}");
        }
        let retry_at = write.and_then(|(snapshot, retry)| self.write(snapshot, retry, now));
        self.flush();
        [next, retry_at].into_iter().flatten().min()
    }

    /// Write `snapshot` (off the lock); `retry`: no change came since the failed write it repeats.
    /// Returns when a failed write is tried again. A refusal or a failure is the UI's to show and the
    /// release log's; a retry that fails as the last write did is neither, as they already have it.
    ///
    /// Only [`WriteError::Failed`] is retried: a read-only store refuses every write this session, and a
    /// conflict stands until another instance's file changes back, which it does not; each such write
    /// re-reads the file first, so even a retry could never save over another instance's file. Both wait
    /// for the next change.
    fn write(&self, snapshot: Snapshot, retry: bool, now: Instant) -> Option<Instant> {
        let dir = self.dir.as_ref()?;
        let result = snapshot.write(dir);
        let mut st = self.lock();
        let problem = match result {
            Ok(_) => {
                st.write_retry = None;
                if matches!(st.problem, Some(StoreProblem::Failed { .. })) {
                    log::info!("[midi] the MIDI bindings are saved");
                    st.problem = None;
                }
                return None;
            }
            Err(WriteError::Failed(why)) => {
                let wait = st.write_retry.map_or(WRITE_RETRY_FIRST, |(_, wait)| (wait * 2).min(WRITE_RETRY_MAX));
                st.write_retry = Some((now + wait, wait));
                StoreProblem::Failed { why }
            }
            Err(WriteError::ReadOnly(why)) => {
                st.write_retry = None;
                StoreProblem::ReadOnly { why }
            }
            Err(WriteError::Conflict(why)) => {
                st.write_retry = None;
                StoreProblem::Conflict { why }
            }
        };
        let retry_at = st.write_retry.map(|(at, _)| at);
        let known = st.problem.as_ref() == Some(&problem);
        if !known {
            log::error!("[midi] the MIDI bindings were not saved: {problem:?}");
        }
        if !(retry && known) {
            self.emit(MidiEvent::Store { problem: problem.clone() });
        }
        st.problem = Some(problem);
        retry_at
    }

    /// The rebuild handshake ([`RebuildHook`]).
    fn pause(&self) {
        self.lock().queue.pause();
    }

    /// The engine was replaced: the queue empties (no batch spans the two engines: each is built and
    /// admitted under this lock), and what the old engine sounded or held is forgotten.
    fn rebuild(&self) -> Vec<Command> {
        let mut st = self.lock();
        let fold = st.queue.rebuild();
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

    fn rebuild(&self, _generation: u64) -> Vec<Command> {
        self.0.upgrade().map(|core| core.rebuild()).unwrap_or_default()
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
    /// (`ports::POLL`) as the backstop. The bindings load before any input can run them (the import
    /// never guesses).
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

    /// A WebView page subscribes (`midi_subscribe`, first thing in its boot), naming its age `origin`
    /// (`performance.timeOrigin`): its input epoch, which every input and learn call of it presents. When
    /// no newer page subscribed before, the older pages' holds are released, a pending learn is
    /// cancelled, and `sink` gets the events from now on (on their own channel), its first ones what a
    /// new listener needs: the ports, the bindings, the learn's state, the store's last problem and the
    /// held notes. An older page's subscribe changes nothing, and its epoch is never current.
    pub fn subscribe(&self, origin: f64, sink: Box<dyn Fn(MidiEvent) + Send + Sync>) -> u64 {
        self.core.subscribe(origin, Arc::from(sink)).0
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
    /// overtake input still queued behind a full ring. GO LIVE's toggle goes straight there too, so no
    /// live operation overtakes another (`actions::UiRoute::Live`); dropped while no device runs or the
    /// ring is full. A note, a wheel, the note target or a panic is
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
            let route = actions::ui_route(&command);
            if route == UiRoute::Queue {
                run.push(Out::new(command));
                continue;
            }
            let queued = self.ui_run(&mut run);
            dropped = dropped.or(queued);
            if route == UiRoute::Live && !self.core.engine.running() {
                dropped = dropped.or(Some(Dropped::NoDevice));
            } else if self.core.engine.send(TimedCommand { frame: None, command }).is_err() {
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

    /// The page of `epoch` learns the next CC or note-on, from any port, onto `action` (on `target` for a
    /// lane action; a global one takes none). Answered by [`MidiEvent::Learned`]. False, and nothing
    /// done, when `epoch` is not the current page's; the same for every call below.
    pub fn learn(&self, epoch: u64, action: ActionId, target: Target) -> bool {
        self.core.current(epoch, |_, st| st.learn.learn(action, target)).is_some()
    }

    /// Stop listening (a learned pedal's wait for its release goes on). True when a learn was pending.
    pub fn cancel_learn(&self, epoch: u64) -> bool {
        self.core.current(epoch, |_, st| st.learn.cancel_learn()).unwrap_or(false)
    }

    /// Drop listed binding `index` of the list at `revision` (the `bindings` event's): its messages
    /// reach the play path again, and a HOLD it held is released. False, and nothing done, when the list
    /// changed since; the same for every edit below.
    pub fn forget(&self, epoch: u64, revision: u64, index: usize) -> Result<bool, String> {
        self.listed_edit(epoch, revision, |st| st.store.forget(index))
    }

    /// Read listed binding `index`'s pedal as momentary or latching; a latching pedal has no HOLD. A
    /// HOLD it held is released, and its pedal's release spent.
    pub fn set_momentary(&self, epoch: u64, revision: u64, index: usize, momentary: bool) -> Result<bool, String> {
        self.edit(epoch, revision, index, |b| Ok(Binding { momentary, hold: b.hold && momentary, ..b }))
    }

    /// HOLD on or off for listed binding `index`: on for a momentary REC/DUB pedal only.
    pub fn set_hold(&self, epoch: u64, revision: u64, index: usize, hold: bool) -> Result<bool, String> {
        self.edit(epoch, revision, index, |b| {
            if hold && !(b.momentary && b.action == ActionId::RecDub) {
                return Err("HOLD is for a momentary REC/DUB pedal".to_string());
            }
            Ok(Binding { hold, ..b })
        })
    }

    fn edit(&self, epoch: u64, revision: u64, index: usize, f: impl FnOnce(Binding) -> Result<Binding, String>) -> Result<bool, String> {
        self.listed_edit(epoch, revision, |st| {
            let b = st.store.listed().into_iter().nth(index).ok_or_else(|| format!("no binding {index}"))?.binding;
            st.store.edit(index, f(b)?)
        })
    }

    /// The player assigns listed binding `index` to the present port `port_id` ([`PortInfo::id`]): a
    /// blocked or unresolved record runs there from now on. Refused when that port already binds its
    /// message.
    pub fn assign(&self, epoch: u64, revision: u64, index: usize, port_id: &str) -> Result<bool, String> {
        self.listed_edit(epoch, revision, |st| {
            let name = st.ports.iter().find(|p| p.id == port_id).map(|p| p.identity.name.clone()).ok_or_else(|| format!("no port {port_id}"))?;
            st.store.assign(index, port_id, &name)
        })
    }

    /// One of the player's edits by listed index, made against the list of store revision `revision`:
    /// refused (false) when the store changed since, as the index may name another binding by now. The
    /// UI has the list as it is, or is about to: each change of it went out with its revision.
    fn listed_edit(&self, epoch: u64, revision: u64, f: impl FnOnce(&mut State) -> Result<(), String>) -> Result<bool, String> {
        let edited = self.core.current(epoch, |core, st| {
            if st.store.revision() != revision {
                return Ok(false);
            }
            f(st)?;
            core.refresh(st);
            Ok(true)
        });
        edited.unwrap_or(Ok(false))
    }

    /// Import the web's list (`lf.midiLearn` verbatim; `"[]"` when it has none), once; `None` from a page
    /// that is not the current one.
    pub fn import_legacy(&self, epoch: u64, json: &str) -> Option<ImportReport> {
        self.core.current(epoch, |core, st| {
            let report = st.store.import_legacy(json);
            core.refresh(st);
            report
        })
    }
}

#[cfg(test)]
impl MidiHost {
    /// Every stored binding, in list order, and how it stands (the UI hears it as `MidiEvent::Bindings`).
    pub fn listed(&self) -> Vec<ListedBinding> {
        self.core.lock().bindings.clone()
    }

    /// The notes held down now, read without the lock.
    pub fn held(&self) -> Held {
        self.core.held.read()
    }

    pub fn diag(&self) -> MidiDiag {
        let st = self.core.lock();
        MidiDiag { failed_sends: self.core.failed_sends.load(Relaxed), refused: st.refused, queue: st.queue.counters() }
    }

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
mod tests;
