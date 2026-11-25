//! The restore (D1, D5, D6, D9, D10): once per process, it brings each record unit the model lacks back
//! under its id, at its place, and places the slots' `pending`. A sans-IO state machine: the app's
//! driver (`engine_io/rig/restore.rs`, CP3a) feeds it [`Event`]s and runs the [`Action`]s it answers,
//! so every rule here is tested on Linux, and no clock is read inside (time enters as events and as
//! the waits an action names).
//!
//! **The order.** `Opened` seeds each slot's level and the legacy sends' send-off at once ([`Action::Seed`],
//! sent again 20 ms doubling to 500 ms while the ring refuses it, one log line at 10 s). Once seeded: a
//! leftover crash marker is dealt with ([`crate::marker`]; its units forgotten through a record
//! commit first), the record's plugins are scanned, and the adds wait for a running device and for
//! the engine to have taken the seed ([`Action::WaitDevice`], [`Action::WaitTaken`]), checked again
//! before each add. The crash marker is written naming nothing before the first add or placement of
//! any kind, and naming each plugin before its load. Adds go one at a time, slot A then slot B, in
//! record order, each after the record units before it ([`crate::place`]).
//!
//! **Blocking actions** are answered by one event each: `Seed` (`Seeded`/`Refused`), `WaitTaken`
//! (`Taken`), `WaitDevice` (`DeviceRuns`), `Scan` (`Scanned`), `Add` (`AddAnswered`), and the writes,
//! `Commit`, `CommitMarker` and `RemoveMarker` (`Committed`/`CommitFailed`). The restore's steps run
//! one at a time, and **at most one write is out at a time**, so `Committed` always answers the write
//! that is out. Beside the steps runs one more lane, for the writes nothing else may wait on: a load
//! that turned terminal leaves the marker by a rewrite at once, also while an add or a device wait is
//! out (the glue runs that write while the add blocks), and a plugin's forget commits the record
//! before its effects. A failed write on that lane is tried again after a wait ([`Action::Backoff`],
//! D1's 20 ms doubling to 500 ms), and while it waits no other write goes out either; the machine
//! never reaches `Done` while a marker write is owed or failed, and the marker goes only by a removal
//! that succeeded (a failed one is tried again the same way, the last launch's too). The others
//! (`Forget`, `Settle`, `Backoff`, `Log`, `Done`) are run as they come. `ViewChanged`, `LatchSet`, `GenMoved`, `DeviceRuns`, `SettleElapsed` and
//! `BackoffElapsed` may come at any time.
//!
//! **Dispositions** (D3, D5): each record unit's in the [`Plan`], which the projection reads for every
//! write. A plugin's own failure forgets it (`Forget`: its tone files go too); a host or seat refusal
//! holds it at its place (a refusal for no engine is tried again under its id once a device runs); a
//! source the player put in yields it; an add with no answer to trust stays issued until its plate
//! applies or fails with its owner gone (whether or not a view showed its plate). A plugin's forget is
//! durable before its effects: the record without the unit commits first, then its tone files go
//! (while a plate of another attempt holds the id they wait, the files being that plate's, and go at
//! the first view without one; still owed at close, they stay) and its load leaves the marker.
//!
//! **Each id once:** at each view the machine matches each queued unit the model already holds (D9),
//! then drops from the plan every id whose [`Plan::fate`] is [`Fate::Ended`], the rule the projection
//! writes by. A load no unit can use then leaves the marker by a rewrite: one whose add was never
//! issued once its unit left the plan, one whose id the player's applied unit won, and one withdrawn
//! (a match, a host refusal) after a marker write named it. What stays open: the app's writer may
//! store the record with the player's unit before that rewrite lands, and a crash between the two
//! writes forgets the player's unit at the next launch.
//!
//! **Closing** stops every action: the machine answers only the marker's close (SEAT-C), a log line for
//! each forget whose tone files still wait, and `Done`, and nothing after. Close reads the marker as written or in flight: with a marker write out, it writes
//! its own unless both the old and the new content already say what close keeps. It keeps each load
//! whose add was issued and has not applied; a load whose add never started goes, in any phase.

use std::collections::BTreeSet;
use std::time::Duration;

use lf_engine::{InstallToken, UnitId};

use crate::add::{AddFail, AddOk, AddState, Place, Retry};
use crate::answer::{Notice, NoticeId};
use crate::marker::{Loads, MarkerRead};
use crate::place::{self, HeadRole, Match};
use crate::plan::{Disposition, Fate, Plan};
use crate::record::{RigRecord, UnitBody, UnitRecord};
use crate::view::{PlateState, PlateView, PluginRefView, RigView};

/// The first wait before a refused seed is sent again (D1).
pub const SEED_BACKOFF_FIRST: Duration = Duration::from_millis(20);
/// The longest wait between two sends of a refused seed (D1).
pub const SEED_BACKOFF_MAX: Duration = Duration::from_millis(500);
/// After this long refused, one log line (D1).
pub const SEED_LOG_AFTER: Duration = Duration::from_secs(10);

/// What the driver tells the machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A page opened the rig with a record present (read, or stored by its import), and the marker as
    /// the process found it. The first starts the restore; a later one (a reload) starts nothing.
    Opened { record: RigRecord, marker: MarkerRead },
    /// The seed batch went in whole (`EngineHost::seed`).
    Seeded,
    /// The ring refused the seed batch, or took part of it.
    Refused,
    /// The engine of generation `gen` took the seed (the barrier, `wait_taken`).
    Taken { gen: u64 },
    /// The engine generation moved to `gen`: the seed must be taken again (the new engine replays it).
    GenMoved { gen: u64 },
    /// The scan of [`Action::Scan`]'s paths: the plugins it found.
    Scanned(Vec<PluginRefView>),
    /// The write out (`Commit`, `CommitMarker` or `RemoveMarker`; one at a time) is durable.
    Committed,
    /// It failed.
    CommitFailed,
    /// The last [`Action::Add`]'s answer.
    AddAnswered(Result<AddOk, AddFail>),
    /// A fresh copy of the model (`Model::rig_view`).
    ViewChanged(RigView),
    /// The owner of the attempt `token` ended (its exit latch).
    LatchSet { token: InstallToken },
    /// A device runs.
    DeviceRuns,
    /// [`crate::marker::SETTLE`] passed since [`Action::Settle`] named `token`.
    SettleElapsed { token: InstallToken },
    /// The app closes (D7).
    Closing,
    /// The page showed the notice `id` (`rig_seen`).
    Seen(NoticeId),
    /// The wait of the last [`Action::Backoff`] passed.
    BackoffElapsed,
}

/// What the machine asks the driver to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// After `after`, push each slot's level (A, B; one the settings memory holds is skipped, D2) and
    /// the four `SetInputSend false` as one batch.
    Seed { volumes: [f64; 2], after: Duration },
    /// Wait until the current engine took the seed; answered `Taken`.
    WaitTaken,
    /// Wait until a device runs; answered `DeviceRuns`.
    WaitDevice,
    /// Scan these plugin paths (`scan_paths`); answered `Scanned`.
    Scan(Vec<String>),
    /// Project the record with the plan and write it (`Rig::commit`).
    Commit,
    /// Write the crash marker naming these ids (D6).
    CommitMarker(Vec<UnitId>),
    /// Remove the crash marker ([`crate::marker::clear`]); answered `Committed` unless both the unlink
    /// and the empty write failed.
    RemoveMarker,
    /// Add `spec` to slot `slot` under `id`, at `place` (`add_as_host`).
    Add { slot: usize, id: UnitId, spec: UnitRecord, place: Place },
    /// Delete unit `id`'s tone files (`unit-<id>.tone`, `unit-<id>.pin`): it is forgotten. The machine
    /// emits it only while no plate of another attempt holds the id (else it waits for a view without
    /// one); **the glue deletes under the id's reservation:** it checks that no unit holds the id and
    /// deletes atomically with that check, so a unit added under the id after this action keeps its files.
    Forget { id: UnitId },
    /// Feed `SettleElapsed { token }` after [`crate::marker::SETTLE`].
    Settle { token: InstallToken },
    /// A write beside the steps, or the marker's removal, failed: feed `BackoffElapsed` after `after`,
    /// and it is tried again.
    Backoff { after: Duration },
    /// One log line.
    Log(String),
    /// The restore has nothing left to do or wait for. Comes once. A `Forget` that waits for another
    /// plate to leave its id may still come after it, at a later view.
    Done,
}

/// What a blocking action waits for.
#[derive(Clone, Debug, PartialEq)]
enum Await {
    Seed,
    Taken,
    Device,
    Scan,
    /// The record without the leftover marker's units the record holds (`forgotten`); `leftover` is
    /// what the marker names.
    ForgetCommit { forgotten: Vec<UnitId>, leftover: Vec<UnitId> },
    /// A slot's placement: what it moved from `pending`.
    PlaceCommit { slot: usize, pending: Vec<UnitRecord> },
    /// The marker naming nothing, before the first add or placement.
    FirstMarker(Vec<UnitId>),
    /// The marker naming a plugin before its load.
    LoadMarker { id: UnitId, ids: Vec<UnitId> },
    /// This launch's marker, removed at the end.
    Remove,
    /// The leftover marker, naming these ids (none for an unreadable one), removed once handled.
    RemoveLeftover(Vec<UnitId>),
    Add { slot: usize, id: UnitId },
}

impl Await {
    /// Whether this step is a write (one write is out at a time).
    fn writes(&self) -> bool {
        matches!(
            self,
            Await::ForgetCommit { .. }
                | Await::PlaceCommit { .. }
                | Await::FirstMarker(_)
                | Await::LoadMarker { .. }
                | Await::Remove
                | Await::RemoveLeftover(_)
        )
    }
}

/// A write beside the steps (the module doc).
#[derive(Clone, Debug, PartialEq)]
enum Side {
    /// The record without these forgotten plugin units (D5), with each one's attempt token.
    Forget(Vec<(UnitId, InstallToken)>),
    /// The marker naming these ids, after a load went terminal.
    Rewrite(Vec<UnitId>),
}

/// The restore machine. One per process (the app's `Rig`).
#[derive(Debug, Default)]
pub struct Restore {
    opened: bool,
    plan: Plan,
    volumes: [f64; 2],
    /// The marker the open found, until it is dealt with.
    leftover: Option<MarkerRead>,
    seeded: bool,
    backoff: Duration,
    refused_for: Duration,
    refusal_logged: bool,
    scanned: bool,
    device: bool,
    taken_gen: Option<u64>,
    moved_gen: u64,
    view: RigView,
    /// Tokens whose owner ended.
    exited: BTreeSet<InstallToken>,
    waiting: Option<Await>,
    loads: Loads,
    /// What this launch's marker on disk names; `None` when this launch wrote none (or removed it).
    marker: Option<Vec<UnitId>>,
    first_written: bool,
    /// A load went terminal: the marker is rewritten without it.
    rewrite_owed: bool,
    /// The write out beside the steps.
    side: Option<Side>,
    /// Plugin units forgotten whose record write is still owed, with their attempt token.
    forgets: Vec<(UnitId, InstallToken)>,
    /// Forgotten units whose tone files wait for no other plate to hold their id, with their own
    /// attempt's token (none for a leftover marker's unit).
    deferred: Vec<(UnitId, Option<InstallToken>)>,
    /// A failed write beside the steps waits for `BackoffElapsed`.
    backoff_armed: bool,
    backoff_next: Duration,
    backoff_logged: bool,
    /// The tokens under the in-flight add's id when it was issued: a plate under the id with another
    /// token is the add's own, read by close before the add answers.
    add_over: BTreeSet<InstallToken>,
    /// Why nothing more is issued this launch.
    stopped: bool,
    /// Units held for no engine, tried again once a device runs (D5's `Device`).
    device_retry: BTreeSet<UnitId>,
    /// The units a placement moved in: they go to their chain's end.
    placed: BTreeSet<UnitId>,
    /// A slot whose placement failed its commit: it waits for the next launch.
    placement_stopped: [bool; 2],
    notice: Option<Notice>,
    notices: NoticeId,
    done: bool,
    closing: bool,
}

impl Restore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The plan the projection reads (D3).
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// What every open answers until the page confirms it (D13).
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// Feed one event; the actions to run, in order.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        if let Event::Seen(id) = event {
            if self.notice.as_ref().is_some_and(|n| n.id == id) {
                self.notice = None;
            }
            return out;
        }
        if self.closing {
            return out;
        }
        match event {
            Event::Opened { record, marker } => {
                if self.opened {
                    return out; // a reload: the restore runs once per process (D1)
                }
                self.open(record, marker);
                self.waiting = Some(Await::Seed);
                out.push(Action::Seed { volumes: self.volumes, after: Duration::ZERO });
                return out;
            }
            Event::Closing => {
                self.close(&mut out);
                return out;
            }
            // Before the open, and once done: only what the model and the device are.
            _ if !self.opened || self.done => {
                match event {
                    Event::ViewChanged(view) => self.view_changed(view, &mut out),
                    Event::GenMoved { gen } => self.moved_gen = self.moved_gen.max(gen),
                    Event::DeviceRuns => self.device = true,
                    _ => {}
                }
                return out;
            }
            Event::Seeded => {
                if self.waiting == Some(Await::Seed) {
                    self.waiting = None;
                    self.seeded = true;
                }
            }
            Event::Refused => {
                if self.waiting == Some(Await::Seed) {
                    self.refused_for += self.backoff;
                    if self.refused_for >= SEED_LOG_AFTER && !self.refusal_logged {
                        self.refusal_logged = true;
                        out.push(log("the engine has refused the slot levels and the send-off for 10 s; still sending them, nothing added meanwhile"));
                    }
                    self.backoff = (self.backoff * 2).clamp(SEED_BACKOFF_FIRST, SEED_BACKOFF_MAX);
                    out.push(Action::Seed { volumes: self.volumes, after: self.backoff });
                    return out;
                }
            }
            Event::Taken { gen } => {
                if self.waiting == Some(Await::Taken) {
                    self.taken_gen = Some(gen);
                    self.waiting = None;
                }
            }
            Event::GenMoved { gen } => self.moved_gen = self.moved_gen.max(gen),
            Event::DeviceRuns => {
                self.device = true;
                for id in std::mem::take(&mut self.device_retry) {
                    if let Some(d @ Disposition::Held { .. }) = self.plan.dispositions.get_mut(&id) {
                        *d = Disposition::Queued;
                    }
                }
                if self.waiting == Some(Await::Device) {
                    self.waiting = None;
                }
            }
            Event::Scanned(found) => {
                if self.waiting == Some(Await::Scan) {
                    self.waiting = None;
                    self.scanned_found(&found, &mut out);
                }
            }
            Event::Committed => self.committed(true, &mut out),
            Event::CommitFailed => self.committed(false, &mut out),
            Event::AddAnswered(answer) => {
                if let Some(Await::Add { slot, id }) = self.waiting.clone() {
                    self.waiting = None;
                    self.answered(slot, id, answer, &mut out);
                }
            }
            Event::ViewChanged(view) => self.view_changed(view, &mut out),
            Event::LatchSet { token } => {
                self.exited.insert(token);
                // A plate gone from the view (also one that came and went between two views) did not
                // apply: with its owner ended, its load failed.
                self.rewrite_owed |=
                    if self.plate(token).is_none() { self.loads.failed(token, true) } else { self.loads.exited(token) };
                self.resolve_issued(&mut out);
            }
            Event::SettleElapsed { token } => self.rewrite_owed |= self.loads.settled(token),
            Event::BackoffElapsed => self.backoff_armed = false,
            Event::Seen(_) => {}
        }
        self.drive(&mut out);
        self.side_write(&mut out);
        out
    }

    fn open(&mut self, record: RigRecord, marker: MarkerRead) {
        self.opened = true;
        self.volumes = [record.slots[0].volume, record.slots[1].volume];
        for (slot, s) in record.slots.into_iter().enumerate() {
            for unit in &s.units {
                self.plan.dispositions.insert(unit.id, Disposition::Queued);
            }
            self.plan.units[slot] = s.units;
            self.plan.pending[slot] = s.pending;
        }
        self.leftover = Some(marker);
    }

    // ── the next step ──────────────────────────────────────────────────────────────────────────────

    /// Issue the next blocking action, if nothing is awaited.
    fn drive(&mut self, out: &mut Vec<Action>) {
        // A write beside the steps is out: the next step may be a write too, so it waits for the answer.
        if self.closing || self.done || !self.seeded || self.waiting.is_some() || self.side.is_some() {
            return;
        }
        match self.leftover.take() {
            Some(MarkerRead::Names(ids)) => return self.forget_leftover(&ids, out),
            Some(MarkerRead::Unreadable) => return self.hold_plugins(out),
            _ => {}
        }
        if !self.scanned {
            self.scanned = true;
            if let Some(paths) = self.scan_paths() {
                self.waiting = Some(Await::Scan);
                out.push(Action::Scan(paths));
                return;
            }
        }
        // This launch's marker, once nothing is left to name or issue.
        if self.marker.is_some() && self.loads.is_empty() && self.forgets.is_empty() && !self.more_to_issue() {
            if self.backoff_armed {
                return; // a failed write waits its backoff: no write goes out meanwhile
            }
            self.waiting = Some(Await::Remove);
            out.push(Action::RemoveMarker);
            return;
        }
        if !self.stopped {
            for slot in 0..2 {
                if self.placement_ready(slot) {
                    if self.backoff_armed {
                        return;
                    }
                    if !self.first_written {
                        return self.first_marker(out);
                    }
                    return self.place(slot, out);
                }
            }
            while let Some((slot, index)) = self.next_queued() {
                if !self.device {
                    self.waiting = Some(Await::Device);
                    out.push(Action::WaitDevice);
                    return;
                }
                if !self.taken() {
                    self.waiting = Some(Await::Taken);
                    out.push(Action::WaitTaken);
                    return;
                }
                if !self.first_written {
                    if self.backoff_armed {
                        return; // a failed write waits its backoff: no write goes out meanwhile
                    }
                    return self.first_marker(out);
                }
                let unit = self.plan.units[slot][index].clone();
                match place::find_match(slot, &unit, &self.view) {
                    Match::Matched(token) => {
                        self.matched(unit.id, token);
                        continue;
                    }
                    Match::Taken => {
                        self.withdraw(unit.id);
                        self.hold(unit.id, "another unit holds its id", None);
                        out.push(log(&format!("slot {}: {} not restored: another unit holds its id", slot + 1, label(&unit))));
                        continue;
                    }
                    Match::Free => {}
                }
                if matches!(unit.body, UnitBody::Plugin(_)) && !self.loads.contains(unit.id) {
                    if self.backoff_armed {
                        return;
                    }
                    self.loads.issue(unit.id);
                    let ids = self.loads.named();
                    self.waiting = Some(Await::LoadMarker { id: unit.id, ids: ids.clone() });
                    out.push(Action::CommitMarker(ids));
                    return;
                }
                let place =
                    if self.placed.contains(&unit.id) { Place::End } else { place::anchor(&self.plan.units[slot], &self.plan.dispositions, index) };
                self.loads.start(unit.id);
                self.add_over = self.view.slots.iter().flatten().filter(|p| p.id == unit.id).map(|p| p.added_by).collect();
                self.plan.dispositions.insert(unit.id, Disposition::Issued { token: None });
                self.waiting = Some(Await::Add { slot, id: unit.id });
                out.push(Action::Add { slot, id: unit.id, spec: unit, place });
                return;
            }
            if !self.device_retry.is_empty() && !self.device {
                self.waiting = Some(Await::Device);
                out.push(Action::WaitDevice);
                return;
            }
        }
        if self.finished() {
            self.done = true;
            out.push(Action::Done);
        }
    }

    /// The lane beside the steps (the module doc): a forget's record write, then a marker rewrite a
    /// terminal load owes, each once no other write is out and no failed one waits its backoff.
    fn side_write(&mut self, out: &mut Vec<Action>) {
        if self.closing || self.side.is_some() || self.backoff_armed || self.waiting.as_ref().is_some_and(Await::writes) {
            return;
        }
        if !self.forgets.is_empty() {
            self.side = Some(Side::Forget(std::mem::take(&mut self.forgets)));
            out.push(Action::Commit);
            return;
        }
        let Some(on_disk) = &self.marker else { return };
        if std::mem::take(&mut self.rewrite_owed) {
            let named = self.loads.named();
            if named != *on_disk {
                self.side = Some(Side::Rewrite(named.clone()));
                out.push(Action::CommitMarker(named));
            }
        }
    }

    fn side_answered(&mut self, side: Side, ok: bool, out: &mut Vec<Action>) {
        if ok {
            self.backoff_next = Duration::ZERO;
            self.backoff_logged = false;
        }
        match side {
            Side::Forget(forgets) if ok => {
                // Durable: only now its tone files go and its load may leave the marker.
                for (id, token) in forgets {
                    self.forget(id, Some(token), out);
                    self.loads.answered(id, token);
                    let exited = self.exited.contains(&token);
                    self.rewrite_owed |= self.loads.failed(token, exited);
                }
            }
            Side::Forget(mut forgets) => {
                forgets.append(&mut self.forgets);
                self.forgets = forgets;
                self.back_off("could not write the rig record without a plugin that failed to load; trying again", out);
            }
            Side::Rewrite(ids) if ok => self.marker = Some(ids),
            Side::Rewrite(_) => {
                self.rewrite_owed = true;
                self.back_off("could not rewrite the crash marker; trying again (a crash meanwhile forgets a unit that loaded)", out);
            }
        }
    }

    /// Unit `id`'s tone files go once no plate of another attempt than `own` holds its id (the files
    /// are that plate's, seat r2): now, or at the first view after that plate left (seat r3). Owed at
    /// close, they stay, with a log line.
    fn forget(&mut self, id: UnitId, own: Option<InstallToken>, out: &mut Vec<Action>) {
        if self.view.slots.iter().flatten().any(|p| p.id == id && Some(p.added_by) != own) {
            self.deferred.push((id, own));
        } else {
            out.push(Action::Forget { id });
        }
    }

    /// A failed write: tried again after D1's backoff, one log line per failure run.
    fn back_off(&mut self, line: &str, out: &mut Vec<Action>) {
        if !self.backoff_logged {
            self.backoff_logged = true;
            out.push(log(line));
        }
        self.backoff_next = (self.backoff_next * 2).clamp(SEED_BACKOFF_FIRST, SEED_BACKOFF_MAX);
        self.backoff_armed = true;
        out.push(Action::Backoff { after: self.backoff_next });
    }

    fn first_marker(&mut self, out: &mut Vec<Action>) {
        let ids = self.loads.named();
        self.waiting = Some(Await::FirstMarker(ids.clone()));
        out.push(Action::CommitMarker(ids));
    }

    /// The seed was taken by the engine generation now running.
    fn taken(&self) -> bool {
        self.taken_gen.is_some_and(|gen| gen >= self.moved_gen)
    }

    /// The first record unit still to add, slot A then slot B, in record order.
    fn next_queued(&self) -> Option<(usize, usize)> {
        (0..2).find_map(|slot| {
            self.plan.units[slot]
                .iter()
                .position(|u| self.plan.dispositions.get(&u.id) == Some(&Disposition::Queued))
                .map(|index| (slot, index))
        })
    }

    fn more_to_issue(&self) -> bool {
        !self.stopped && (self.next_queued().is_some() || !self.device_retry.is_empty() || (0..2).any(|slot| self.placement_open(slot)))
    }

    /// Slot `slot`'s `pending` may still be placed this launch.
    fn placement_open(&self, slot: usize) -> bool {
        !self.plan.pending[slot].is_empty() && !self.placement_stopped[slot] && self.head_role(slot) != HeadRole::Keep
    }

    fn placement_ready(&self, slot: usize) -> bool {
        self.placement_open(slot) && matches!(self.head_role(slot), HeadRole::Known(_))
    }

    fn head_role(&self, slot: usize) -> HeadRole {
        place::head_role(&self.plan.units[slot], &self.plan.dispositions, &self.view.slots[slot])
    }

    fn finished(&self) -> bool {
        self.waiting.is_none()
            && self.side.is_none()
            && !self.backoff_armed
            && self.forgets.is_empty()
            && !self.more_to_issue()
            && self.loads.is_empty()
            && self.marker.is_none()
            && !self.plan.dispositions.values().any(|d| matches!(d, Disposition::Issued { .. }))
    }

    // ── the steps ─────────────────────────────────────────────────────────────────────────────────

    /// A leftover marker (D6): its units the record holds are forgotten, the record written first.
    fn forget_leftover(&mut self, ids: &[UnitId], out: &mut Vec<Action>) {
        let named: Vec<UnitId> = self.plugin_units().filter(|u| ids.contains(&u.id)).map(|u| u.id).collect();
        if named.is_empty() {
            self.waiting = Some(Await::RemoveLeftover(ids.to_vec()));
            out.push(Action::RemoveMarker);
            return;
        }
        for id in &named {
            self.plan.dispositions.insert(*id, Disposition::Forgotten);
        }
        self.waiting = Some(Await::ForgetCommit { forgotten: named, leftover: ids.to_vec() });
        out.push(Action::Commit);
    }

    /// An unreadable marker (D6): no plugin is restored this launch, the record untouched, told once.
    fn hold_plugins(&mut self, out: &mut Vec<Action>) {
        let ids: Vec<UnitId> = self.plugin_units().map(|u| u.id).collect();
        for id in ids {
            self.hold(id, "the crash marker could not be read", None);
        }
        self.tell(Vec::new(), true);
        out.push(log("the crash marker could not be read; no plugin is restored this launch"));
        self.waiting = Some(Await::RemoveLeftover(Vec::new()));
        out.push(Action::RemoveMarker);
    }

    /// The paths of the plugins still to add that the model does not already hold.
    fn scan_paths(&mut self) -> Option<Vec<String>> {
        let mut paths: Vec<String> = Vec::new();
        for slot in 0..2 {
            for unit in self.plan.units[slot].clone() {
                let UnitBody::Plugin(plugin) = &unit.body else { continue };
                if self.plan.dispositions.get(&unit.id) != Some(&Disposition::Queued) {
                    continue;
                }
                if let Match::Matched(token) = place::find_match(slot, &unit, &self.view) {
                    self.matched(unit.id, token);
                } else if !paths.contains(&plugin.plugin.path) {
                    paths.push(plugin.plugin.path.clone());
                }
            }
        }
        (!paths.is_empty()).then_some(paths)
    }

    fn scanned_found(&mut self, found: &[PluginRefView], out: &mut Vec<Action>) {
        let missing: Vec<UnitRecord> = self
            .plugin_units()
            .filter(|u| matches!(&u.body, UnitBody::Plugin(p) if !found.contains(&p.plugin)))
            .filter(|u| self.plan.dispositions.get(&u.id) == Some(&Disposition::Queued))
            .cloned()
            .collect();
        for unit in missing {
            self.hold(unit.id, "not in this launch's plugin scan", None);
            out.push(log(&format!("{} is not in this launch's plugin scan (file missing, plugin changed or no plugin host); kept for a later launch", label(&unit))));
        }
    }

    /// Slot `slot`'s placement (D9): one commit moves what its head's role takes from `pending` to the
    /// units' end, each `Queued`; its adds come after the commit.
    fn place(&mut self, slot: usize, out: &mut Vec<Action>) {
        let HeadRole::Known(role) = self.head_role(slot) else { return };
        let pending = std::mem::take(&mut self.plan.pending[slot]);
        for unit in place::placed(&pending, role) {
            self.plan.dispositions.insert(unit.id, Disposition::Queued);
            self.placed.insert(unit.id);
            self.plan.units[slot].push(unit);
        }
        self.waiting = Some(Await::PlaceCommit { slot, pending });
        out.push(Action::Commit);
    }

    /// Nothing more is issued this launch: what is still queued is held at its place.
    fn stop(&mut self, why: &str) {
        self.stopped = true;
        self.device_retry.clear();
        let queued: Vec<UnitId> = self.plan.dispositions.iter().filter(|(_, d)| **d == Disposition::Queued).map(|(id, _)| *id).collect();
        for id in queued {
            self.hold(id, why, None);
        }
    }

    fn hold(&mut self, id: UnitId, why: &str, token: Option<InstallToken>) {
        self.plan.dispositions.insert(id, Disposition::Held { why: why.to_owned(), token });
    }

    fn tell(&mut self, forgotten: Vec<String>, plugins_held: bool) {
        self.notices += 1;
        self.notice = Some(Notice { id: self.notices, forgotten, plugins_held });
    }

    fn plugin_units(&self) -> impl Iterator<Item = &UnitRecord> {
        self.plan.units.iter().flatten().filter(|u| matches!(u.body, UnitBody::Plugin(_)))
    }

    fn unit(&self, id: UnitId) -> Option<&UnitRecord> {
        self.plan.units.iter().flatten().find(|u| u.id == id)
    }

    fn is_plugin(&self, id: UnitId) -> bool {
        self.unit(id).is_some_and(|u| matches!(u.body, UnitBody::Plugin(_)))
    }

    // ── the answers ───────────────────────────────────────────────────────────────────────────────

    fn committed(&mut self, ok: bool, out: &mut Vec<Action>) {
        if let Some(side) = self.side.take() {
            return self.side_answered(side, ok, out);
        }
        let Some(waiting) = self.waiting.take() else { return };
        match waiting {
            Await::ForgetCommit { forgotten, leftover } if ok => {
                let names = forgotten.iter().filter_map(|id| self.unit(*id)).map(label).collect::<Vec<_>>();
                for id in forgotten {
                    self.forget(id, None, out);
                }
                out.push(log(&format!("the last launch stopped while restoring {}; forgotten", names.join(", "))));
                self.tell(names, false);
                self.waiting = Some(Await::RemoveLeftover(leftover));
                out.push(Action::RemoveMarker);
            }
            Await::ForgetCommit { forgotten, .. } => {
                for id in forgotten {
                    self.hold(id, "the record could not be written without it", None);
                }
                self.stop("the record could not be written; nothing restored this launch");
                out.push(log("could not write the rig record without the units the crash marker names; nothing restored, the marker kept"));
            }
            Await::PlaceCommit { .. } if ok => {}
            Await::PlaceCommit { slot, pending } => {
                // The record keeps `pending` as it was, less a unit placement moved that left the plan
                // meanwhile (its id won by the player's unit, or a confirmed removal; seat r3); the id
                // rule then judges the rest at the current view.
                let held = |u: &UnitRecord| self.plan.units[slot].iter().any(|p| p.id == u.id);
                let back: Vec<UnitRecord> = pending.iter().filter(|u| !self.placed.contains(&u.id) || held(u)).cloned().collect();
                self.plan.units[slot].retain(|u| pending.iter().all(|p| p.id != u.id));
                for id in pending.iter().map(|u| u.id) {
                    self.plan.dispositions.remove(&id);
                    self.placed.remove(&id);
                }
                self.plan.pending[slot] = back;
                self.drop_ended();
                self.placement_stopped[slot] = true;
                out.push(log(&format!("slot {}: could not store the placed send units; none added, the next launch places them", slot + 1)));
            }
            Await::FirstMarker(ids) if ok => {
                self.first_written = true;
                self.marker = Some(ids);
            }
            Await::FirstMarker(_) => {
                self.stop("the crash marker could not be written");
                out.push(log("could not write the crash marker; nothing restored or placed this launch"));
            }
            Await::LoadMarker { ids, .. } if ok => self.marker = Some(ids),
            Await::LoadMarker { id, .. } => {
                self.withdraw(id);
                self.stop("the crash marker could not be written");
                out.push(log("could not write the crash marker; the rest is not restored this launch"));
            }
            Await::Remove if ok => {
                self.marker = None;
                self.rewrite_owed = false;
                self.backoff_next = Duration::ZERO;
                self.backoff_logged = false;
            }
            // The marker goes only by a removal that succeeded: until then it names what it named.
            Await::Remove => self.back_off("could not remove the crash marker; trying again (a crash meanwhile forgets the units it names)", out),
            Await::RemoveLeftover(_) if ok => {}
            // The same rule as this launch's marker (seat r3): until a removal succeeds it stands on disk
            // as this launch's marker, tried again after D1's backoff (the first marker write replaces
            // it), and the machine is not `Done`.
            Await::RemoveLeftover(ids) => {
                self.marker = Some(ids);
                self.back_off("could not remove the last launch's crash marker; trying again (a crash meanwhile forgets the units it names)", out);
            }
            other => self.waiting = Some(other), // not a commit's answer
        }
    }

    fn answered(&mut self, slot: usize, id: UnitId, answer: Result<AddOk, AddFail>, out: &mut Vec<Action>) {
        let plugin = self.is_plugin(id);
        let name = self.unit(id).map(label).unwrap_or_default();
        let at = format!("slot {}: {name}", slot + 1);
        // The answer saw its plate in the model: a view without it is a confirmed removal (seat r3). A
        // failed add's token is not: its plate may never have been there.
        if let Ok(ok) = &answer { self.plan.seen.insert(ok.token); }
        match answer {
            // The host found the unit under its slot lock: no plugin code ran for this attempt.
            Ok(AddOk { state: AddState::Matched, token }) => self.matched(id, token),
            Ok(AddOk { state, token }) => {
                self.loads.answered(id, token);
                if state == AddState::Seated {
                    self.restored(id, token, plugin, out);
                } else {
                    self.plan.dispositions.insert(id, Disposition::Issued { token: Some(token) });
                }
            }
            Err(AddFail::Host { why, retry, token }) => {
                self.withdraw(id);
                self.hold(id, &why, token);
                if retry == Retry::Device {
                    self.device_retry.insert(id);
                    self.device = false;
                    self.taken_gen = None;
                }
                out.push(log(&format!("{at} not restored ({why}); held at its place")));
            }
            Err(AddFail::SourceTaken { why, token, exited }) => {
                self.plan.dispositions.insert(id, Disposition::Yielded);
                self.failed_load(id, token, exited);
                out.push(log(&format!("{at} yields to the player's source ({why})")));
            }
            Err(AddFail::Seat { why, token, exited }) => {
                self.hold(id, &why, Some(token));
                self.failed_load(id, token, exited);
                out.push(log(&format!("{at} not seated ({why}); held at its place")));
            }
            Err(AddFail::Plugin { why, token, exited }) => {
                // Durable before its effects: the record without it commits first (`side_write`); its
                // tone files and its marker entry go after. Until then the marker keeps naming it.
                self.plan.dispositions.insert(id, Disposition::Forgotten);
                if exited {
                    self.exited.insert(token);
                }
                self.forgets.push((id, token));
                out.push(log(&format!("{at} failed to load ({why}); forgotten")));
            }
            Err(AddFail::Unresolved { why, token, exited }) => {
                self.loads.answered(id, token);
                if exited {
                    self.exited.insert(token);
                }
                self.plan.dispositions.insert(id, Disposition::Issued { token: Some(token) });
                out.push(log(&format!("{at}: no answer to trust ({why}); its place is kept while it may still seat")));
            }
        }
        self.follow_loads(out);
        self.resolve_issued(out);
    }

    fn failed_load(&mut self, id: UnitId, token: InstallToken, exited: bool) {
        self.loads.answered(id, token);
        let exited = exited || self.exited.contains(&token);
        self.rewrite_owed |= self.loads.failed(token, exited);
    }

    fn restored(&mut self, id: UnitId, token: InstallToken, plugin: bool, out: &mut Vec<Action>) {
        self.plan.dispositions.insert(id, Disposition::Restored { token });
        if plugin && self.loads.applied(token) {
            out.push(Action::Settle { token });
        }
    }

    fn view_changed(&mut self, view: RigView, out: &mut Vec<Action>) {
        if view.rev < self.view.rev {
            return; // an older copy than the one held
        }
        self.view = view;
        self.plan.seen.extend(self.view.slots.iter().flatten().map(|p| p.added_by));
        self.follow_loads(out);
        // A queued unit the model already holds is matched at the view that shows it (D9), not when a
        // blocking step lets the next add come: the player's removal of that plate then ends it (seat r3).
        let matches: Vec<(UnitId, InstallToken)> = (0..2)
            .flat_map(|slot| self.plan.units[slot].iter().map(move |u| (slot, u)))
            .filter(|(_, u)| self.plan.dispositions.get(&u.id) == Some(&Disposition::Queued))
            .filter_map(|(slot, u)| match place::find_match(slot, u, &self.view) {
                Match::Matched(token) => Some((u.id, token)),
                _ => None,
            })
            .collect();
        for (id, token) in matches {
            self.matched(id, token);
        }
        self.drop_ended();
        self.release_loads();
        self.resolve_issued(out);
        for (id, own) in std::mem::take(&mut self.deferred) {
            self.forget(id, own, out);
        }
    }

    /// Every id whose [`Plan::fate`] is [`Fate::Ended`] at the latest view leaves the plan.
    fn drop_ended(&mut self) {
        let ended: Vec<UnitId> = self.plan.ids().into_iter().filter(|id| self.plan.fate(*id, &self.view) == Some(Fate::Ended)).collect();
        for id in ended {
            self.plan.dispositions.remove(&id);
            self.device_retry.remove(&id);
            for units in self.plan.units.iter_mut().chain(&mut self.plan.pending) {
                units.retain(|u| u.id != id);
            }
        }
    }

    /// Unit `id` is the plate `token` the model already holds (D9): never added, and its load, if
    /// issued, leaves the marker.
    fn matched(&mut self, id: UnitId, token: InstallToken) {
        self.withdraw(id);
        self.plan.seen.insert(token);
        self.plan.dispositions.insert(id, Disposition::Matched { token });
    }

    /// `id`'s load leaves the marker's set. A rewrite is owed (seat r3): the next write drops it anyway
    /// (no rewrite goes out when the marker on disk already says the same), but while another load
    /// stays unresolved no other write may come, and an entry under the id could forget the unit, or a
    /// later one under its id, at the next launch.
    fn withdraw(&mut self, id: UnitId) {
        self.loads.withdraw(id);
        self.rewrite_owed = true;
    }

    /// The marker's loads no record unit can use any more leave it by a rewrite (D6, seat r2): one
    /// whose add was never issued once its unit is no longer queued (it left the plan), and one whose
    /// id an applied plate of another attempt holds (the player's unit won it; an entry under the id
    /// could only forget that unit at the next launch). A load whose add is out unanswered is kept:
    /// a plate under its id may be its own.
    fn release_loads(&mut self) {
        for id in self.loads.named() {
            let release = if !self.loads.started(id) {
                self.plan.dispositions.get(&id) != Some(&Disposition::Queued)
            } else {
                let own = self.loads.token(id).or_else(|| self.forget_token(id));
                own.is_some_and(|t| self.view.slots.iter().flatten().any(|p| p.id == id && p.applied && p.added_by != t))
            };
            if release {
                self.withdraw(id);
            }
        }
    }

    /// The attempt token of a forgotten unit whose record write is still owed or out.
    fn forget_token(&self, id: UnitId) -> Option<InstallToken> {
        let out = match &self.side {
            Some(Side::Forget(f)) => f.as_slice(),
            _ => &[],
        };
        self.forgets.iter().chain(out).find(|(i, _)| *i == id).map(|(_, t)| *t)
    }

    /// The marker's loads by their own tokens, whatever the plan holds for their units: a plate that
    /// applied starts its settle; one that failed, or was seen and left before it applied, failed.
    fn follow_loads(&mut self, out: &mut Vec<Action>) {
        for token in self.loads.tokens() {
            let exited = self.exited.contains(&token);
            match self.plate(token) {
                Some(p) if p.applied => {
                    if self.loads.applied(token) {
                        out.push(Action::Settle { token });
                    }
                }
                Some(p) if p.state == PlateState::Failed => self.rewrite_owed |= self.loads.failed(token, exited),
                Some(_) => {}
                // Not in the view: failed if an earlier view showed it (a view older than its add shows
                // nothing yet).
                None if self.plan.seen.contains(&token) => self.rewrite_owed |= self.loads.failed(token, exited),
                None => {}
            }
        }
    }

    fn plate(&self, token: InstallToken) -> Option<&PlateView> {
        self.view.slots.iter().flatten().find(|p| p.added_by == token)
    }

    /// An issued unit whose plate applied is restored; one whose plate failed, or is gone, with its
    /// owner ended is held (D5).
    fn resolve_issued(&mut self, out: &mut Vec<Action>) {
        let issued: Vec<(UnitId, InstallToken)> = self
            .plan
            .dispositions
            .iter()
            .filter_map(|(id, d)| match d {
                Disposition::Issued { token: Some(t) } => Some((*id, *t)),
                _ => None,
            })
            .collect();
        for (id, token) in issued {
            let exited = self.exited.contains(&token);
            match self.plate(token) {
                Some(p) if p.applied => {
                    let plugin = self.is_plugin(id);
                    self.restored(id, token, plugin, out);
                }
                Some(p) if p.state != PlateState::Failed => {}
                // Failed with its owner gone: terminal, whether or not a view showed its plate (D6).
                _ if exited => {
                    self.hold(id, "its add failed with no answer to trust", Some(token));
                    self.rewrite_owed |= self.loads.failed(token, true);
                }
                _ => {}
            }
        }
    }

    /// D7 (5) and SEAT-C: a load not terminal keeps the marker; one inside its settle goes.
    fn close(&mut self, out: &mut Vec<Action>) {
        self.closing = true;
        for (id, _) in std::mem::take(&mut self.deferred) {
            out.push(log(&format!("unit {id} was forgotten but its tone files stay: another unit held its id until the app closed")));
        }
        if !self.opened || self.done {
            return;
        }
        // What the disk may hold: the last write acknowledged, and the marker write out, if any (the
        // glue lands close's own write after it).
        let in_flight = match (&self.waiting, &self.side) {
            (Some(Await::FirstMarker(ids) | Await::LoadMarker { ids, .. }), _) | (_, Some(Side::Rewrite(ids))) => Some(Some(ids.clone())),
            (Some(Await::Remove), _) => Some(None),
            // The last launch's marker: the removal out may fail, and close removes it again.
            (Some(Await::RemoveLeftover(ids)), _) => Some(Some(ids.clone())),
            _ => None,
        };
        let disk: Vec<Option<Vec<UnitId>>> = std::iter::once(self.marker.clone()).chain(in_flight).collect();
        if disk.iter().all(Option::is_none) {
            out.push(Action::Done);
            return;
        }
        // A load whose add never started goes, in any phase: no add follows a close.
        let mut keep = self.loads.at_close();
        // Its add is out: its own plate (a token the id did not have at the issue) applied, so it is
        // terminal under SEAT-C, its answer not needed.
        if let Some(Await::Add { slot, id }) = &self.waiting {
            let applied = self.view.slots[*slot].iter().any(|p| p.id == *id && p.applied && !self.add_over.contains(&p.added_by));
            if applied {
                keep.retain(|k| k != id);
            }
        }
        if keep.is_empty() {
            out.push(Action::RemoveMarker);
        } else if disk.iter().any(|d| d.as_ref() != Some(&keep)) {
            out.push(Action::CommitMarker(keep));
        }
        out.push(Action::Done);
    }
}

/// A unit's name for the notice and the logs: a plugin's scan name, else its file's name.
fn label(unit: &UnitRecord) -> String {
    match &unit.body {
        UnitBody::Plugin(p) => p.name.clone().unwrap_or_else(|| p.plugin.path.rsplit(['/', '\\']).next().unwrap_or_default().to_owned()),
        UnitBody::Builtin { kind, .. } => format!("{kind:?}").to_uppercase(),
    }
}

fn log(line: &str) -> Action {
    Action::Log(format!("[rig] {line}"))
}
