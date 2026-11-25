//! The restore's driver (D10): it feeds [`lf_rig::Restore`] its events and runs the actions it answers,
//! on the rig thread, behind [`RestoreHost`], the one seam to the engine and the chain host. The machine
//! decides everything (its module doc is the contract); this is a loop of I/O.
//!
//! **One turn per writer tick** (`writer.rs` `Rig::tick`): the inputs first ([`Rig::inputs`]: the
//! answer of a blocking call that ended, the timers due, the exit latches of the attempts the restore
//! follows, the device and its engine generation, the seed's barrier), then one fresh view fed to the
//! restore and projected with its plan under one `inner`, then the store's tick, then the actions
//! ([`Rig::drive`]). **The glue rule:** a turn feeds an add's answer before it takes its view, and a
//! view taken before an answer was received is never fed after it: an answer that arrives while a turn
//! takes its copy waits in its channel for the next turn. Fed after an older copy, the answer's plate
//! would read as removed (`restore_tests.rs` `a_view_taken_before_an_answer_is_never_fed_after_it`).
//!
//! **The actions.** A seed goes out at once (`rig_open` sends the first one before it answers, D1),
//! a refused one again on the machine's backoff. The waits for the device and the seed's barrier are
//! polled once per turn, bounded ([`TAKEN_WAIT`]). A scan and an add block for seconds (a scan child, a
//! plugin's setup): each runs on its own thread (`lf-rig-call`), one at a time as the machine issues
//! them, and its answer is taken at the next turn; meanwhile the rig thread keeps its ticks and runs
//! the writes the machine allows beside an add (a load that turned terminal leaves the marker at once,
//! `restore.rs`'s side lane in `lf-rig`). `Commit` projects a fresh view (fed to the restore first)
//! and commits it through the store; `CommitMarker` and `RemoveMarker` write the marker; each is one
//! write under `disk`, refused once close began, and answered `Committed`/`CommitFailed`. `Settle` and
//! `Backoff` are the loop's timers. `Forget` asks the host to delete the unit's tone files under the
//! id's reservation; one a unit holds is asked again at each later turn (the files are that unit's
//! until it leaves). Nothing here runs under `inner` or `disk` but the store's write.
//!
//! **Close** feeds `Closing` itself (`writer.rs` `Rig::close`) and never waits on the driver: a call
//! in flight answers into a restore that is closing, and the driver runs no action after `closing`.
//! Only `Closing`'s own actions run (the marker's, on the store before it drops): a `Forget` the
//! restore emitted and the driver had not run by then is not run, and that unit's tone files stay.

use std::sync::atomic::Ordering::Acquire;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_rig::marker::{self, Cleared};
use lf_rig::{Action, AddFail, AddOk, AddState, Commit, Event, InstallToken, Place, PluginRefView, Refusal, Retry, Store, UnitId, UnitRecord, SETTLE};

use super::disk::FileDisk;
use super::writer::{clear_marker, RigModel};
use super::{lock, Rig};

/// How long one turn waits for the seed's barrier before it goes on (the next turn asks again).
const TAKEN_WAIT: Duration = Duration::from_millis(20);

/// What the restore drives (D1, D2, D5, D9, D10): the engine and the chain host, one method per need.
/// The app's implementor (CP3b) wraps `EngineHost` and `Chains`; the tests' is a scripted fake. Its
/// [`RigModel`] half is the model the writer and the restore read, so one turn's view is one copy.
pub(crate) trait RestoreHost: RigModel + Send + Sync {
    /// Push the seed as one batch (D1, D2): each slot's level from `volumes` (A, B), skipping a slot
    /// whose gain the settings memory already holds (decided under `settings`), and the four
    /// `SetInputSend false`. True when the ring took the batch whole; false when it refused it or took
    /// part of it (the machine sends it again on its backoff). Never blocks.
    fn seed(&self, volumes: [f64; 2]) -> bool;

    /// Wait, at most `within`, until the engine now running took the last seed (its replay of the
    /// settings, for an engine built after it), or its generation moved past the one the wait began
    /// on. `Pending` when neither happened in time (asked again at the next turn).
    fn wait_taken(&self, within: Duration) -> SeedTaken;

    /// The running engine's generation, `None` while no device runs. A new generation is a rebuild:
    /// the seed must be taken again before the next add.
    fn device(&self) -> Option<u64>;

    /// Scan the plugin files at `paths` (`scan_paths`; no document needed): the plugins found, by
    /// identity. Blocks for the scan; called on a thread of its own.
    fn scan(&self, paths: &[String]) -> Vec<PluginRefView>;

    /// Add record unit `unit` to slot `slot` under `id` at `place` (`add_as_host`: `add` minus the
    /// document check), resolving `place` under the slot's order lock and the model's `state` (D9):
    /// a live plate that is the unit answers `Matched` with its token; another holding the id refuses
    /// (`Host`); a source the player put in answers `SourceTaken`. Typed as D5 (each answer past the
    /// mint names the attempt's token; `exited` read from its latch at the answer). Blocks for the
    /// load's first answer; called on a thread of its own, never under a rig lock.
    fn add(&self, slot: usize, id: UnitId, unit: &UnitRecord, place: &Place) -> Result<AddOk, AddFail>;

    /// Whether the owner of attempt `token` has ended (its exit latch, D5), read at any time, also
    /// after the add answered. True for an attempt no owner ran for.
    fn exited(&self, token: InstallToken) -> bool;

    /// Delete unit `id`'s tone files (`unit-<id>.tone`, `unit-<id>.pin`) under the id's reservation:
    /// atomically with the check that no unit holds the id, so a unit added under it after this call
    /// keeps its files. `Held` when a unit holds it (nothing deleted; the driver asks again later).
    fn forget(&self, id: UnitId) -> Forgot;
}

/// [`RestoreHost::wait_taken`]'s answer.
#[cfg_attr(not(test), allow(dead_code))] // made by the real host (CP3b) and the tests' fake
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeedTaken {
    /// The engine of generation `gen` took the seed.
    Taken { gen: u64 },
    /// The generation moved to `gen` first; its engine replays the seed, and the next wait is for it.
    Moved { gen: u64 },
    /// Neither, within the wait.
    Pending,
}

/// [`RestoreHost::forget`]'s answer.
#[cfg_attr(not(test), allow(dead_code))] // made by the real host (CP3b) and the tests' fake
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Forgot {
    /// No unit held the id: its tone files are gone (or there were none).
    Deleted,
    /// A unit holds the id: its files are that unit's, and nothing was deleted.
    Held,
}

/// The driver's own state, the rig thread's.
#[derive(Default)]
pub(super) struct Driver {
    /// The timers due at their instant (`Settle`, `Backoff`, a seed sent again).
    timers: Vec<(Instant, Timer)>,
    /// The blocking call out (a scan or an add), its answer taken at the next turn.
    call: Option<(Call, Receiver<Event>)>,
    want_taken: bool,
    want_device: bool,
    /// The last engine generation the restore was told.
    gen: Option<u64>,
    /// The attempts whose owner may still run: their latch is read each turn until it is set.
    watch: Vec<InstallToken>,
    /// Forgotten ids a unit held when their files were to go: asked again each turn.
    held_forgets: Vec<UnitId>,
}

enum Timer {
    Seed([f64; 2]),
    Event(Event),
}

#[derive(Clone, Copy)]
enum Call {
    Scan,
    Add,
}

impl Rig {
    /// The first actions of a restore an open or the import started (`Opened`, on the caller's thread):
    /// the seed goes out now, before the page's answer (D1); the rest wait for the rig thread. Without
    /// a host, or once close began, nothing runs.
    pub(super) fn start(&self, actions: Vec<Action>) {
        let Some(host) = &self.host else { return };
        for action in actions {
            let answer = match action {
                Action::Seed { volumes, after } if after.is_zero() && !self.closing.load(Acquire) => {
                    if host.seed(volumes) {
                        Event::Seeded
                    } else {
                        Event::Refused
                    }
                }
                other => {
                    lock(&self.inner).actions.push(other);
                    continue;
                }
            };
            let mut inner = lock(&self.inner);
            let more = inner.restore.handle(answer);
            inner.actions.extend(more);
        }
    }

    /// This turn's inputs to the restore, in order (the module doc). Empty without a host.
    pub(super) fn inputs(&self, now: Instant) -> Vec<Event> {
        let Some(host) = &self.host else { return Vec::new() };
        let mut driver = lock(&self.driver);
        let d = &mut *driver;
        let mut events = Vec::new();
        if let Some((call, answers)) = &d.call {
            let answer = match answers.try_recv() {
                Ok(answer) => Some(answer),
                Err(TryRecvError::Empty) => None,
                // The call's thread ended with no answer (it panicked, or never started).
                Err(TryRecvError::Disconnected) => Some(match call {
                    Call::Scan => {
                        log::error!("[rig] the restore's plugin scan ended with no answer; its plugins are held for a later launch");
                        Event::Scanned(Vec::new())
                    }
                    Call::Add => {
                        log::error!("[rig] a restore add ended with no answer; the unit is held at its place");
                        let why = "the add ended with no answer".to_string();
                        Event::AddAnswered(Err(AddFail::Host { why, retry: Retry::Launch, token: None }))
                    }
                }),
            };
            if let Some(answer) = answer {
                d.call = None;
                if let Some(token) = followed(&answer) {
                    d.watch.push(token);
                }
                events.push(answer);
            }
        }
        d.timers.sort_by_key(|(at, _)| *at);
        let due = d.timers.iter().take_while(|(at, _)| *at <= now).count();
        for (_, timer) in d.timers.drain(..due) {
            events.push(match timer {
                Timer::Seed(volumes) if host.seed(volumes) => Event::Seeded,
                Timer::Seed(_) => Event::Refused,
                Timer::Event(event) => event,
            });
        }
        d.watch.retain(|&token| {
            let exited = host.exited(token);
            if exited {
                events.push(Event::LatchSet { token });
            }
            !exited
        });
        if let Some(gen) = host.device() {
            if d.gen != Some(gen) {
                d.gen = Some(gen);
                events.push(Event::GenMoved { gen });
            }
            if std::mem::take(&mut d.want_device) {
                events.push(Event::DeviceRuns);
            }
        }
        if d.want_taken {
            match host.wait_taken(TAKEN_WAIT) {
                SeedTaken::Taken { gen } => {
                    d.want_taken = false;
                    events.push(Event::Taken { gen });
                }
                SeedTaken::Moved { gen } => {
                    d.gen = Some(gen);
                    events.push(Event::GenMoved { gen });
                }
                SeedTaken::Pending => {}
            }
        }
        d.held_forgets.retain(|&id| host.forget(id) == Forgot::Held);
        events
    }

    /// Run the restore's queued actions, and those their answers bring, until none is left or close
    /// began. Without a host they are dropped (the restore is never seeded, so it asks for nothing but
    /// its first seed).
    pub(super) fn drive(&self, model: &(impl RigModel + ?Sized), now: Instant) {
        let Some(host) = &self.host else {
            lock(&self.inner).actions.clear();
            return;
        };
        let mut driver = lock(&self.driver);
        loop {
            let actions = std::mem::take(&mut lock(&self.inner).actions);
            if actions.is_empty() {
                return;
            }
            for action in actions {
                if self.closing.load(Acquire) {
                    return;
                }
                let Some(answer) = self.run(host, model, &mut driver, action, now) else { continue };
                let mut inner = lock(&self.inner);
                let more = inner.restore.handle(answer);
                inner.actions.extend(more);
            }
        }
    }

    /// How long until the driver's next timer is due (`None`: no timer).
    pub(super) fn next_due(&self, now: Instant) -> Option<Duration> {
        lock(&self.driver).timers.iter().map(|(at, _)| at.saturating_duration_since(now)).min()
    }

    /// One action; the answer to feed now, if it has one (a blocking call's comes at a later turn).
    fn run(&self, host: &Arc<dyn RestoreHost>, model: &(impl RigModel + ?Sized), d: &mut Driver, action: Action, now: Instant) -> Option<Event> {
        match action {
            Action::Seed { volumes, after } => d.timers.push((now + after, Timer::Seed(volumes))),
            Action::WaitTaken => d.want_taken = true,
            Action::WaitDevice => d.want_device = true,
            Action::Scan(paths) => call(host, d, Call::Scan, move |host| Event::Scanned(host.scan(&paths))),
            Action::Add { slot, id, spec, place } => call(host, d, Call::Add, move |host| Event::AddAnswered(host.add(slot, id, &spec, &place))),
            Action::Commit => return self.commit(model),
            Action::CommitMarker(ids) => return self.write(|store| store.write_marker(&marker::text(&ids))),
            Action::RemoveMarker => {
                return self.write(|store| match clear_marker(store) {
                    Cleared::Failed => Err(std::io::Error::other("neither the unlink nor the empty marker's write went through")),
                    Cleared::Removed | Cleared::Emptied => Ok(()),
                })
            }
            Action::Forget { id } => {
                if host.forget(id) == Forgot::Held {
                    d.held_forgets.push(id);
                }
            }
            Action::Settle { token } => d.timers.push((now + SETTLE, Timer::Event(Event::SettleElapsed { token }))),
            Action::Backoff { after } => d.timers.push((now + after, Timer::Event(Event::BackoffElapsed))),
            Action::Log(line) => log::info!("{line}"),
            // The restore follows no attempt's latch after it.
            Action::Done => d.watch.clear(),
        }
        None
    }

    /// The machine's `Commit`: a fresh copy fed to the restore and projected with its plan (the same
    /// rule as a tick), committed now. `None` once close began (nothing is written; the restore is
    /// closing and waits for no answer).
    fn commit(&self, model: &(impl RigModel + ?Sized)) -> Option<Event> {
        let held = model.gains();
        let view = model.view();
        let record = self.judge(Vec::new(), held, view, false)?;
        let (commit, seen) = {
            let mut disk = lock(&self.disk);
            if self.closing.load(Acquire) || disk.closed {
                return None;
            }
            let commit = disk.store.as_mut()?.commit(&record);
            if commit == Commit::Committed {
                disk.at += 1;
            }
            (commit, Rig::found(&disk))
        };
        match commit {
            Commit::Committed => {
                self.keep(&seen, None);
                Some(Event::Committed)
            }
            Commit::Failed { why, log } => {
                if log {
                    log::warn!("[rig] the restore's record write failed ({why})");
                }
                Some(Event::CommitFailed)
            }
            Commit::Refused(Refusal::Closed) => None,
            Commit::Refused(why) => {
                log::warn!("[rig] the restore's record write was refused ({why:?})");
                Some(Event::CommitFailed)
            }
        }
    }

    /// One marker write under `disk`, answered; `None` once close began.
    fn write(&self, op: impl FnOnce(&mut Store<FileDisk>) -> std::io::Result<()>) -> Option<Event> {
        let mut disk = lock(&self.disk);
        if self.closing.load(Acquire) || disk.closed {
            return None;
        }
        match op(disk.store.as_mut()?) {
            Ok(()) => Some(Event::Committed),
            Err(e) => {
                log::debug!("[rig] a crash marker write failed: {e}");
                Some(Event::CommitFailed)
            }
        }
    }
}

/// Run a blocking call on its own thread; its answer is taken at a later turn. A thread that cannot
/// start drops its sender, which the next turn reads as no answer.
fn call(host: &Arc<dyn RestoreHost>, d: &mut Driver, kind: Call, work: impl FnOnce(&dyn RestoreHost) -> Event + Send + 'static) {
    let (answer, answers) = mpsc::channel();
    let host = host.clone();
    let spawned = std::thread::Builder::new().name("lf-rig-call".into()).spawn(move || {
        let _ = answer.send(work(&*host));
    });
    if let Err(e) = spawned {
        log::error!("[rig] no thread for a restore call ({e})");
    }
    d.call = Some((kind, answers));
}

/// The attempt an add's answer leaves running: its owner's latch is followed until it is set (D5, D6).
fn followed(answer: &Event) -> Option<InstallToken> {
    match answer {
        Event::AddAnswered(Ok(AddOk { state: AddState::Seated | AddState::Waiting, token })) => Some(*token),
        Event::AddAnswered(Err(
            AddFail::SourceTaken { token, exited: false, .. }
            | AddFail::Seat { token, exited: false, .. }
            | AddFail::Plugin { token, exited: false, .. }
            | AddFail::Unresolved { token, exited: false, .. },
        )) => Some(*token),
        _ => None,
    }
}
