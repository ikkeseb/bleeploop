//! The restore's driver (D10): it feeds [`lf_rig::Restore`] its events and runs the actions it answers,
//! on the rig thread, behind [`RestoreHost`], the one seam to the engine and the chain host. The machine
//! decides everything (its module doc is the contract); this is a loop of I/O.
//!
//! **One turn per writer tick** (`writer.rs` `Rig::tick`): the inputs first ([`Rig::inputs`]), then
//! one fresh view fed to the restore and projected with its plan under one `inner`, then the store's
//! tick (skipped while the turn queued a `Commit`, which writes that projection itself), then the
//! actions ([`Rig::drive`]). **The inputs' order:** the answer of a blocking call that ended is taken
//! from its channel first (not fed yet), then the device is read (its generation, a device that runs,
//! the seed's barrier, D1), and the restore is fed the device's events, then that answer, then the
//! exit latches of the attempts it follows and the timers due. So an add's answer that arrives with a
//! rebuild is fed after the new generation, and the next add waits for its barrier; a rebuild after
//! the turn's read is seen at the next turn. **The glue rule:** the answer is received before the
//! turn's view is taken and fed before that view, which is what [`lf_rig::Plan::fate`] asks; an answer
//! that arrives while a turn takes its copy waits in its channel for the next turn (fed after an older
//! copy, its plate would read as removed: `restore_tests.rs`
//! `a_view_taken_before_an_answer_is_never_fed_after_it`), and the machine's next add, issued at its
//! last view, is checked again by the host's D9 under its locks.
//!
//! **The actions.** A seed goes out at once (`rig_open` sends the first one before it answers, D1),
//! a refused one again on the machine's backoff. The waits for the device and the seed's barrier are
//! polled once per turn, bounded ([`TAKEN_WAIT`]). A scan and an add block for seconds (a scan child, a
//! plugin's setup): each runs on its own thread (`lf-rig-call`, detached: it ends by the host's own
//! bounds, and no shutdown joins it), one at a time as the machine issues them, and its answer is taken
//! at the next turn; meanwhile the rig thread keeps its ticks and runs the writes the machine allows
//! beside an add (a load that turned terminal leaves the marker at once, `restore.rs`'s side lane in
//! `lf-rig`). A call whose thread ended with no answer (a panic): a scan found nothing; an add that
//! started answers nothing, and the restore waits on it for the rest of the launch (its load stays in
//! the marker); one that never started is a host refusal. `Commit` projects a fresh view (fed to the
//! restore first) and commits it through the store; `CommitMarker` and `RemoveMarker` write the marker;
//! each is one write under `disk`, refused once close began, and answered `Committed`/`CommitFailed`.
//! `Settle` and `Backoff` are the loop's timers, counted from the turn's view (`now` moves on by the
//! real time the turn took before its actions). `Forget` asks the host to delete the unit's tone files
//! under the id's reservation; one a unit holds is asked again at each later turn (the files are that
//! unit's until it leaves). Nothing here runs under `inner` or `disk` but the store's write.
//!
//! **Close** feeds `Closing` itself (`writer.rs` `Rig::close`) and never waits on the driver, a host
//! call or an add. The driver's blocking call, its answer and the latches it follows sit behind their
//! own small lock (`Rig.calls`, never held across a write, nor a host call but a forget's), so close
//! reads, right after it sets `closing` and before its feed wait and its view, what is already
//! answered: the call's answer if one is in its channel (taken out of `calls`: close feeds that answer
//! and no later one, which a tick past its check may still receive; fed after close's older view, its
//! plate would read as removed), and each followed attempt's latch ([`RestoreHost::exited`], a read).
//! It feeds them, in order, before its view, in the same `judge`. An add not started by then is
//! cancelled: one still in the queue, or one whose `lf-rig-call` thread has not passed its start gate
//! (one atomic: queued, then started or cancelled, whichever comes first; a scan's too); close answers
//! it `AddFail::Host` (the restore withdraws its load and holds the unit at its place). An add that
//! started stays as SEAT-C keeps one in flight. **Once `closing` is set the driver makes no host call
//! and feeds nothing:** [`Rig::inputs`] returns at once, [`Rig::drive`] runs no action, and an answer
//! in hand is fed only under `inner` with `closing` checked there (close's `judge` takes `inner` after
//! `closing`, so a feed after it, which could queue an add `Closing` never sees, is refused); only
//! `Closing`'s own actions run (the marker's, on the store before it drops). A `Forget` runs under
//! `calls` with `closing` checked there (`Rig::forget`): close's inputs wait for one in progress,
//! two file deletes at most, and none runs after them; one not run by then is not run, and that unit's
//! files stay. Lock order: `driver`, `inner`, `calls`, then the host's id reservation and its `state`.
//! A call's log line is written once `calls` is released.

use std::sync::atomic::{AtomicU8, Ordering::{AcqRel, Acquire}};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
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
    /// document check), resolving `place` under the slot's order lock and the model's `state` (D9),
    /// by `lf_rig::place::find_match`'s rule: an applied live plate that is the unit answers `Matched`
    /// with its token; any other live plate holding the id refuses (`Host`), one of the same unit that
    /// has not applied too; a source the player put in answers `SourceTaken`. Typed as D5 (each answer past the
    /// mint names the attempt's token; `exited` read from its latch at the answer). Blocks for the
    /// load's first answer; called on a thread of its own, never under a rig lock.
    fn add(&self, slot: usize, id: UnitId, unit: &UnitRecord, place: &Place) -> Result<AddOk, AddFail>;

    /// Whether the owner of attempt `token` has ended (its exit latch, D5), read at any time, also
    /// after the add answered. True for an attempt no owner ran for.
    fn exited(&self, token: InstallToken) -> bool;

    /// Delete unit `id`'s tone files (`unit-<id>.tone`, `unit-<id>.pin`) under the id's reservation:
    /// atomically with the check that no unit holds the id, so a unit added under it after this call
    /// keeps its files. `Held` when a unit holds it (nothing deleted; the driver asks again later).
    /// Called under the rig's `calls` lock, which close waits for: it takes no rig lock, and it never
    /// waits on more than the reservation and the two deletes.
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
    /// A unit holds the id (its files are that unit's, and nothing was deleted), the host is closing,
    /// or a file could not be deleted and is left: the driver asks again at a later turn.
    Held,
}

/// The driver's own state, the rig thread's: taken by a turn's inputs and its actions, never by close.
#[derive(Default)]
pub(super) struct Driver {
    /// The timers due at their instant (`Settle`, `Backoff`, a seed sent again).
    timers: Vec<(Instant, Timer)>,
    want_taken: bool,
    want_device: bool,
    /// The last engine generation the restore was told.
    gen: Option<u64>,
    /// Forgotten ids a unit held when their files were to go: asked again each turn.
    held_forgets: Vec<UnitId>,
}

/// The driver's blocking call and what it answered, which close reads too: behind its own lock, taken
/// alone or inside `driver` or `inner`, never across a write or a host call but a forget's (the module
/// doc's close).
#[derive(Default)]
pub(super) struct Calls {
    /// The blocking call out (a scan or an add), one at a time.
    out: Option<Out>,
    /// The answer received from `out`'s channel and not fed yet: fed under `inner`, by the turn that
    /// received it or by close.
    answer: Option<Event>,
    /// The attempts whose owner may still run: their latch is read each turn, and once more by close.
    /// A token leaves once its `LatchSet` was fed.
    watch: Vec<InstallToken>,
    /// Test only: run on the call's thread before its gate.
    #[cfg(test)]
    pub(super) before_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Test only: the next call gets no thread, as when the spawn fails.
    #[cfg(test)]
    pub(super) no_thread: bool,
    /// Test only: run at each [`Pause`], outside every rig lock but the driver's.
    #[cfg(test)]
    pub(super) pause: Option<Arc<dyn Fn(Pause) + Send + Sync>>,
}

/// Test only: where the driver and close run [`Calls::pause`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pause {
    /// A tick's inputs, past their `closing` check, before the call's answer is received.
    BeforeReceive,
    /// `drive`, an action's answer in hand, before it takes `inner` to feed it.
    BeforeFeed,
    /// Close, after its `judge`, before it feeds `Closing`.
    AfterJudge,
}

/// A blocking call out: its kind, its answer's channel and its start gate.
struct Out {
    kind: Call,
    answers: Receiver<Event>,
    /// [`QUEUED`] until the call's thread moves it to [`STARTED`] right before the host call, or close
    /// moves it to [`CANCELLED`] first, and then nothing is called.
    gate: Arc<AtomicU8>,
}

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

/// A call queued by [`Rig::drive`], for [`spawn`].
struct Start {
    answer: Sender<Event>,
    gate: Arc<AtomicU8>,
    #[cfg(test)]
    before_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    no_thread: bool,
}

/// One turn's inputs, in the order `Rig::judge` (`writer.rs`) feeds them (the module doc).
#[derive(Default)]
pub(super) struct Turn {
    /// The device: its generation, a device that runs, the seed's barrier (D1).
    device: Vec<Event>,
    /// A tick's: the call's answer (`Calls::answer`) was received before the turn's view: fed after
    /// `device`.
    answered: bool,
    /// Close's: the answer it took out of `Calls` in its own inputs, before its feed wait and its view.
    /// An answer received after that (by a tick past its `closing` check) is not fed.
    answer: Option<Event>,
    /// Close's: its inputs cancelled the add behind its start gate (answered `AddFail::Host`).
    cancelled: bool,
    /// The latches set, then the timers due.
    after: Vec<Event>,
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
            // Nothing is fed once close began (`Rig::drive`'s rule).
            if self.closing.load(Acquire) {
                return;
            }
            let more = inner.restore.handle(answer);
            inner.actions.extend(more);
        }
    }

    /// This turn's inputs to the restore (the module doc): empty without a host, and once close began
    /// (no host call after it; close reads what it needs itself).
    pub(super) fn inputs(&self, now: Instant) -> Turn {
        let mut turn = Turn::default();
        let Some(host) = &self.host else { return turn };
        if self.closing.load(Acquire) {
            return turn;
        }
        let mut driver = lock(&self.driver);
        let d = &mut *driver;
        #[cfg(test)]
        self.paused(Pause::BeforeReceive);
        // The answer first, received and not fed: the device's events go to the restore before it.
        let (watch, line) = {
            let mut calls = lock(&self.calls);
            let line = receive(&mut calls);
            turn.answered = calls.answer.is_some();
            (calls.watch.clone(), line)
        };
        if let Some(line) = line {
            log::error!("{line}");
        }
        if let Some(gen) = host.device() {
            if d.gen != Some(gen) {
                d.gen = Some(gen);
                turn.device.push(Event::GenMoved { gen });
            }
            if std::mem::take(&mut d.want_device) {
                turn.device.push(Event::DeviceRuns);
            }
        }
        if d.want_taken {
            match host.wait_taken(TAKEN_WAIT) {
                SeedTaken::Taken { gen } => {
                    d.want_taken = false;
                    turn.device.push(Event::Taken { gen });
                }
                SeedTaken::Moved { gen } => {
                    d.gen = Some(gen);
                    turn.device.push(Event::GenMoved { gen });
                }
                SeedTaken::Pending => {}
            }
        }
        turn.after.extend(watch.into_iter().filter(|&token| host.exited(token)).map(|token| Event::LatchSet { token }));
        // Close may have begun during the barrier's wait: the timers (a seed) and the forgets call the
        // host, so they wait for a turn that never comes.
        if self.closing.load(Acquire) {
            return turn;
        }
        d.timers.sort_by_key(|(at, _)| *at);
        let due = d.timers.iter().take_while(|(at, _)| *at <= now).count();
        for (_, timer) in d.timers.drain(..due) {
            turn.after.push(match timer {
                Timer::Seed(volumes) if host.seed(volumes) => Event::Seeded,
                Timer::Seed(_) => Event::Refused,
                Timer::Event(event) => event,
            });
        }
        d.held_forgets.retain(|&id| self.forget(&**host, id) != Some(Forgot::Deleted));
        turn
    }

    /// Close's inputs, read right after it set `closing` (before its feed wait and its view) with no
    /// host call that can block: the call's answer if one is in its channel or was received and not
    /// fed (taken out of `Calls`: close feeds this one and no later one), the call cancelled if its
    /// thread has not passed its start gate, and the latch of each attempt followed (a read). Taking
    /// `calls` waits for a forget in progress (one at most, `Rig::forget`); none starts after.
    pub(super) fn closing_inputs(&self) -> Turn {
        let Some(host) = &self.host else { return Turn::default() };
        let (answer, cancelled, watch, line) = {
            let mut calls = lock(&self.calls);
            let line = receive(&mut calls);
            let cancelled = cancel(&mut calls);
            (calls.answer.take(), cancelled, calls.watch.clone(), line)
        };
        if let Some(line) = line {
            log::error!("{line}");
        }
        let after = watch.into_iter().filter(|&token| host.exited(token)).map(|token| Event::LatchSet { token }).collect();
        Turn { answer, cancelled, after, ..Turn::default() }
    }

    /// `turn`'s events in the order they are fed, under `inner` (`actions` is its queue): the device's,
    /// the answer received (a tick's from `Calls`, close's its own), then the latches and the timers.
    /// At close, an add that never started is answered (`AddFail::Host`, before the latches): the one
    /// close's inputs cancelled, one still queued in `actions`, or one `drive` registered after those
    /// inputs (its pop and its gate under `inner`, raced by them), cancelled here. The fed latches stop
    /// being followed.
    pub(super) fn fed(&self, turn: Turn, actions: &mut Vec<Action>, at_close: bool) -> Vec<Event> {
        let mut calls = lock(&self.calls);
        let mut events = turn.device;
        if turn.answered {
            events.extend(calls.answer.take());
        }
        events.extend(turn.answer);
        if at_close {
            let queued = actions.iter().any(|a| matches!(a, Action::Add { .. }));
            actions.retain(|a| !matches!(a, Action::Add { .. }));
            if turn.cancelled || queued || cancel(&mut calls) {
                let why = "the app closed before the add started".to_string();
                events.push(Event::AddAnswered(Err(AddFail::Host { why, retry: Retry::Launch, token: None })));
            }
        }
        calls.watch.retain(|t| !turn.after.contains(&Event::LatchSet { token: *t }));
        events.extend(turn.after);
        events
    }

    /// Whether the restore's queued actions hold a `Commit`: that commit writes this turn's projection,
    /// so the tick does not hand the store one ahead of it.
    pub(super) fn commit_queued(&self) -> bool {
        lock(&self.inner).actions.contains(&Action::Commit)
    }

    /// Run the restore's queued actions, and those their answers bring, one at a time, until none is
    /// left or close began. Without a host they are dropped (the restore is never seeded, so it asks for
    /// nothing but its first seed).
    pub(super) fn drive(&self, model: &(impl RigModel + ?Sized), now: Instant) {
        let Some(host) = &self.host else {
            lock(&self.inner).actions.clear();
            return;
        };
        let mut driver = lock(&self.driver);
        loop {
            let (action, start) = {
                let mut inner = lock(&self.inner);
                if self.closing.load(Acquire) || inner.actions.is_empty() {
                    return;
                }
                let action = inner.actions.remove(0);
                // A call is registered as it leaves the queue, under `inner`: close finds an add either
                // still queued or behind its gate.
                let start = match &action {
                    Action::Scan(_) => Some(self.queue(Call::Scan)),
                    Action::Add { .. } => Some(self.queue(Call::Add)),
                    _ => None,
                };
                (action, start)
            };
            // No host call once close began; a call registered stays queued, and close cancels it.
            if self.closing.load(Acquire) {
                return;
            }
            let Some(answer) = self.run(host, model, &mut driver, action, start, now) else { continue };
            #[cfg(test)]
            self.paused(Pause::BeforeFeed);
            let mut inner = lock(&self.inner);
            // Close's judge takes `inner` after it set `closing`: an answer fed after that judge would
            // queue what close never sees (a load's add after its marker write, kept by `Closing` as
            // started). One fed before it queued its add in `actions`, which close cancels.
            if self.closing.load(Acquire) {
                return;
            }
            let more = inner.restore.handle(answer);
            inner.actions.extend(more);
        }
    }

    /// How long until the driver's next timer is due (`None`: no timer).
    pub(super) fn next_due(&self, now: Instant) -> Option<Duration> {
        lock(&self.driver).timers.iter().map(|(at, _)| at.saturating_duration_since(now)).min()
    }

    /// [`RestoreHost::forget`] under `calls`, unless close began (`None`: not asked, the files stay).
    /// Close takes `calls` right after it sets `closing`, so no forget runs once close read its inputs,
    /// and close waits at most for one forget's two file deletes. Lock order: `calls`, then the host's
    /// id reservation, then the model's `state`.
    fn forget(&self, host: &dyn RestoreHost, id: UnitId) -> Option<Forgot> {
        let _calls = lock(&self.calls);
        if self.closing.load(Acquire) {
            return None;
        }
        Some(host.forget(id))
    }

    /// Test only: run the test's hook at `at`, outside the `calls` lock.
    #[cfg(test)]
    pub(super) fn paused(&self, at: Pause) {
        let hook = lock(&self.calls).pause.clone();
        if let Some(hook) = hook {
            hook(at);
        }
    }

    /// A blocking call out, its gate queued (under `inner`, from `drive`).
    fn queue(&self, kind: Call) -> Start {
        let (answer, answers) = mpsc::channel();
        let gate = Arc::new(AtomicU8::new(QUEUED));
        let mut calls = lock(&self.calls);
        calls.out = Some(Out { kind, answers, gate: gate.clone() });
        Start {
            answer,
            gate,
            #[cfg(test)]
            before_gate: calls.before_gate.clone(),
            #[cfg(test)]
            no_thread: std::mem::take(&mut calls.no_thread),
        }
    }

    /// One action; the answer to feed now, if it has one (a blocking call's comes at a later turn).
    fn run(&self, host: &Arc<dyn RestoreHost>, model: &(impl RigModel + ?Sized), d: &mut Driver, action: Action, start: Option<Start>, now: Instant) -> Option<Event> {
        match action {
            Action::Seed { volumes, after } => d.timers.push((now + after, Timer::Seed(volumes))),
            Action::WaitTaken => d.want_taken = true,
            Action::WaitDevice => d.want_device = true,
            Action::Scan(paths) => spawn(host, start, move |host| Event::Scanned(host.scan(&paths))),
            Action::Add { slot, id, spec, place } => spawn(host, start, move |host| Event::AddAnswered(host.add(slot, id, &spec, &place))),
            Action::Commit => return self.commit(model),
            Action::CommitMarker(ids) => return self.write(|store| store.write_marker(&marker::text(&ids))),
            Action::RemoveMarker => {
                return self.write(|store| match clear_marker(store) {
                    Cleared::Failed => Err(std::io::Error::other("neither the unlink nor the empty marker's write went through")),
                    Cleared::Removed | Cleared::Emptied => Ok(()),
                })
            }
            Action::Forget { id } => {
                if self.forget(&**host, id) == Some(Forgot::Held) {
                    d.held_forgets.push(id);
                }
            }
            Action::Settle { token } => d.timers.push((now + SETTLE, Timer::Event(Event::SettleElapsed { token }))),
            Action::Backoff { after } => d.timers.push((now + after, Timer::Event(Event::BackoffElapsed))),
            Action::Log(line) => log::info!("{line}"),
            // The restore follows no attempt's latch after it.
            Action::Done => lock(&self.calls).watch.clear(),
        }
        None
    }

    /// The machine's `Commit`: a fresh copy fed to the restore and projected with its plan (the same
    /// rule as a tick), committed now. `None` once close began (nothing is written; the restore is
    /// closing and waits for no answer).
    fn commit(&self, model: &(impl RigModel + ?Sized)) -> Option<Event> {
        let held = model.gains();
        let view = model.view();
        let record = self.judge(Turn::default(), held, view, false)?;
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

/// Run a blocking call on its own thread, `lf-rig-call` (detached: it ends by the host's own bounds,
/// and no shutdown joins it); its answer is taken at a later turn. The thread passes the gate right
/// before the host call and calls nothing if close cancelled it first. A thread that cannot start
/// drops its sender with its gate still queued, which the next turn reads as an add that never started.
fn spawn(host: &Arc<dyn RestoreHost>, start: Option<Start>, work: impl FnOnce(&dyn RestoreHost) -> Event + Send + 'static) {
    let Some(start) = start else { return };
    #[cfg(test)]
    if start.no_thread {
        log::error!("[rig] no thread for a restore call (a test's)");
        return;
    }
    let host = host.clone();
    let Start { answer, gate, .. } = start;
    #[cfg(test)]
    let before_gate = start.before_gate;
    let spawned = std::thread::Builder::new().name("lf-rig-call".into()).spawn(move || {
        #[cfg(test)]
        if let Some(hook) = before_gate {
            hook();
        }
        if gate.compare_exchange(QUEUED, STARTED, AcqRel, Acquire).is_ok() {
            let _ = answer.send(work(&*host));
        }
    });
    if let Err(e) = spawned {
        log::error!("[rig] no thread for a restore call ({e})");
    }
}

/// Take the call's answer, if one came, from its channel into `calls.answer` (not fed yet), and follow
/// the attempt it leaves running. A call whose thread ended with no answer: a scan answers that it
/// found nothing; an add that started answers nothing, and the restore waits on it for the rest of the
/// launch (its load stays in the marker, as at close); one that never started is a host refusal. The
/// line to log, if any, is the caller's to log once `calls` is released (a slow log never holds close).
fn receive(calls: &mut Calls) -> Option<&'static str> {
    if calls.answer.is_some() {
        return None;
    }
    let out = calls.out.as_ref()?;
    let (answer, line) = match out.answers.try_recv() {
        Ok(answer) => (Some(answer), None),
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => match out.kind {
            Call::Scan => (Some(Event::Scanned(Vec::new())), Some("[rig] the restore's plugin scan ended with no answer; its plugins are held for a later launch")),
            Call::Add if out.gate.load(Acquire) == STARTED => {
                (None, Some("[rig] a restore add ended with no answer after it started; nothing more is restored this launch, and its load stays in the crash marker"))
            }
            Call::Add => {
                let why = "the add never started".to_string();
                let answer = Event::AddAnswered(Err(AddFail::Host { why, retry: Retry::Launch, token: None }));
                (Some(answer), Some("[rig] a restore add never started; the unit is held at its place"))
            }
        },
    };
    calls.out = None;
    if let Some(answer) = answer {
        if let Some(token) = followed(&answer) {
            calls.watch.push(token);
        }
        calls.answer = Some(answer);
    }
    line
}

/// Close's cancellation of the call out, if its thread has not passed its start gate: nothing is called
/// then. True for an add (close answers it `AddFail::Host`); a scan answers nothing (the restore is
/// closing and waits for no scan).
fn cancel(calls: &mut Calls) -> bool {
    let Some(out) = &calls.out else { return false };
    if out.gate.compare_exchange(QUEUED, CANCELLED, AcqRel, Acquire).is_err() {
        return false;
    }
    let add = matches!(out.kind, Call::Add);
    calls.out = None;
    add
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
