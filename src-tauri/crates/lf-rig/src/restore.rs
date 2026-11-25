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
//! **Blocking actions** are answered by one event each, and the machine issues the next only after
//! that answer: `Seed` (`Seeded`/`Refused`), `WaitTaken` (`Taken`), `WaitDevice` (`DeviceRuns`), `Scan`
//! (`Scanned`), `Commit`, `CommitMarker` and `RemoveMarker` (`Committed`/`CommitFailed`), `Add`
//! (`AddAnswered`). The others (`Forget`, `Settle`, `Log`, `Done`) are run as they come. `ViewChanged`,
//! `LatchSet`, `GenMoved`, `DeviceRuns` and `SettleElapsed` may come at any time.
//!
//! **Dispositions** (D3, D5): each record unit's in the [`Plan`], which the projection reads for every
//! write. A plugin's own failure forgets it (`Forget`: its tone files go too); a host or seat refusal
//! holds it at its place (a refusal for no engine is tried again under its id once a device runs); a
//! source the player put in yields it; an add with no answer to trust stays issued until its plate
//! applies or fails with its owner gone. A plate that was seen and left the model is a confirmed
//! removal: the unit leaves the plan.
//!
//! **Closing** stops every action: the machine answers only the marker's close (SEAT-C) and `Done`, and
//! nothing after.

use std::collections::BTreeSet;
use std::time::Duration;

use lf_engine::{InstallToken, UnitId};

use crate::add::{AddFail, AddOk, AddState, Place, Retry};
use crate::answer::{Notice, NoticeId};
use crate::marker::{Loads, MarkerRead};
use crate::place::{self, HeadRole, Match};
use crate::plan::{Disposition, Plan};
use crate::record::{RigRecord, UnitBody, UnitRecord};
use crate::view::{PlateState, PluginRefView, RigView};

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
    /// The last `Commit`, `CommitMarker` or `RemoveMarker` is durable.
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
    /// Delete unit `id`'s tone files (`unit-<id>.tone`, `unit-<id>.pin`): it is forgotten.
    Forget { id: UnitId },
    /// Feed `SettleElapsed { token }` after [`crate::marker::SETTLE`].
    Settle { token: InstallToken },
    /// One log line.
    Log(String),
    /// The restore has nothing left to do or wait for. Comes once.
    Done,
}

/// What a blocking action waits for.
#[derive(Clone, Debug, PartialEq)]
enum Await {
    Seed,
    Taken,
    Device,
    Scan,
    /// The record without the leftover marker's units.
    ForgetCommit(Vec<UnitId>),
    /// A slot's placement: what it moved from `pending`.
    PlaceCommit { slot: usize, pending: Vec<UnitRecord> },
    /// The marker naming nothing, before the first add or placement.
    FirstMarker(Vec<UnitId>),
    /// The marker naming a plugin before its load.
    LoadMarker { id: UnitId, ids: Vec<UnitId> },
    /// A rewrite after a load went terminal.
    Rewrite(Vec<UnitId>),
    /// This launch's marker, removed at the end.
    Remove,
    /// The leftover marker, removed once handled.
    RemoveLeftover,
    Add { slot: usize, id: UnitId },
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
    /// Every install token a view showed: a token seen and then gone is a confirmed removal.
    seen: BTreeSet<InstallToken>,
    /// Tokens whose owner ended.
    exited: BTreeSet<InstallToken>,
    waiting: Option<Await>,
    loads: Loads,
    /// What this launch's marker on disk names; `None` when this launch wrote none (or removed it).
    marker: Option<Vec<UnitId>>,
    first_written: bool,
    rewrite_owed: bool,
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
                self.rewrite_owed |= self.loads.exited(token);
                self.resolve_issued(&mut out);
            }
            Event::SettleElapsed { token } => self.rewrite_owed |= self.loads.settled(token),
            Event::Seen(_) => {}
        }
        self.drive(&mut out);
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
        if self.closing || self.done || !self.seeded || self.waiting.is_some() {
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
        // This launch's marker, when it names more than it must.
        if let Some(on_disk) = &self.marker {
            let named = self.loads.named();
            if named.is_empty() && !self.more_to_issue() {
                self.waiting = Some(Await::Remove);
                out.push(Action::RemoveMarker);
                return;
            }
            if std::mem::take(&mut self.rewrite_owed) && named != *on_disk {
                self.waiting = Some(Await::Rewrite(named.clone()));
                out.push(Action::CommitMarker(named));
                return;
            }
        }
        if !self.stopped {
            for slot in 0..2 {
                if self.placement_ready(slot) {
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
                    return self.first_marker(out);
                }
                let unit = self.plan.units[slot][index].clone();
                match place::find_match(slot, &unit, &self.view) {
                    Match::Matched(token) => {
                        self.loads.withdraw(unit.id);
                        self.plan.dispositions.insert(unit.id, Disposition::Matched { token });
                        continue;
                    }
                    Match::Taken => {
                        self.loads.withdraw(unit.id);
                        self.hold(unit.id, "another unit holds its id", None);
                        out.push(log(&format!("slot {}: {} not restored: another unit holds its id", slot + 1, label(&unit))));
                        continue;
                    }
                    Match::Free => {}
                }
                if matches!(unit.body, UnitBody::Plugin(_)) && !self.loads.contains(unit.id) {
                    self.loads.issue(unit.id);
                    let ids = self.loads.named();
                    self.waiting = Some(Await::LoadMarker { id: unit.id, ids: ids.clone() });
                    out.push(Action::CommitMarker(ids));
                    return;
                }
                let place =
                    if self.placed.contains(&unit.id) { Place::End } else { place::anchor(&self.plan.units[slot], &self.plan.dispositions, index) };
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
            self.waiting = Some(Await::RemoveLeftover);
            out.push(Action::RemoveMarker);
            return;
        }
        for id in &named {
            self.plan.dispositions.insert(*id, Disposition::Forgotten);
        }
        self.waiting = Some(Await::ForgetCommit(named));
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
        self.waiting = Some(Await::RemoveLeftover);
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
                    self.plan.dispositions.insert(unit.id, Disposition::Matched { token });
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
        let Some(waiting) = self.waiting.take() else { return };
        match waiting {
            Await::ForgetCommit(ids) if ok => {
                let names = ids.iter().filter_map(|id| self.unit(*id)).map(label).collect::<Vec<_>>();
                out.extend(ids.iter().map(|id| Action::Forget { id: *id }));
                out.push(log(&format!("the last launch stopped while restoring {}; forgotten", names.join(", "))));
                self.tell(names, false);
                self.waiting = Some(Await::RemoveLeftover);
                out.push(Action::RemoveMarker);
            }
            Await::ForgetCommit(ids) => {
                for id in ids {
                    self.hold(id, "the record could not be written without it", None);
                }
                self.stop("the record could not be written; nothing restored this launch");
                out.push(log("could not write the rig record without the units the crash marker names; nothing restored, the marker kept"));
            }
            Await::PlaceCommit { .. } if ok => {}
            Await::PlaceCommit { slot, pending } => {
                self.plan.units[slot].retain(|u| pending.iter().all(|p| p.id != u.id));
                for id in pending.iter().map(|u| u.id) {
                    self.plan.dispositions.remove(&id);
                    self.placed.remove(&id);
                }
                self.plan.pending[slot] = pending;
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
                self.loads.withdraw(id);
                self.stop("the crash marker could not be written");
                out.push(log("could not write the crash marker; the rest is not restored this launch"));
            }
            Await::Rewrite(ids) if ok => self.marker = Some(ids),
            Await::Rewrite(_) => out.push(log("could not rewrite the crash marker; a crash before the next write forgets a unit that loaded")),
            Await::Remove => {
                self.marker = None;
                if !ok {
                    out.push(log("could not remove the crash marker; the next launch forgets the units it names"));
                }
            }
            Await::RemoveLeftover if !ok => out.push(log("could not remove the last launch's crash marker")),
            Await::RemoveLeftover => {}
            other => self.waiting = Some(other), // not a commit's answer
        }
    }

    fn answered(&mut self, slot: usize, id: UnitId, answer: Result<AddOk, AddFail>, out: &mut Vec<Action>) {
        let plugin = self.is_plugin(id);
        let name = self.unit(id).map(label).unwrap_or_default();
        let at = format!("slot {}: {name}", slot + 1);
        match answer {
            Ok(AddOk { state, token }) => {
                self.loads.answered(id, token);
                if state == AddState::Seated {
                    self.restored(id, token, plugin, out);
                } else {
                    self.plan.dispositions.insert(id, Disposition::Issued { token: Some(token) });
                }
            }
            Err(AddFail::Host { why, retry, token }) => {
                self.loads.withdraw(id);
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
                self.plan.dispositions.insert(id, Disposition::Forgotten);
                self.failed_load(id, token, exited);
                out.push(Action::Forget { id });
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
        let present: BTreeSet<InstallToken> = self.view.slots.iter().flatten().map(|p| p.added_by).collect();
        // A plate seen before and gone now is a confirmed removal (D3): its unit leaves the plan.
        let gone: Vec<(UnitId, InstallToken)> = self
            .plan
            .dispositions
            .iter()
            .filter_map(|(id, d)| token_of(d).map(|t| (*id, t)))
            .filter(|(_, t)| self.seen.contains(t) && !present.contains(t))
            .collect();
        for (id, token) in gone {
            let exited = self.exited.contains(&token);
            self.rewrite_owed |= self.loads.failed(token, exited);
            self.plan.dispositions.remove(&id);
            self.device_retry.remove(&id);
            for units in &mut self.plan.units {
                units.retain(|u| u.id != id);
            }
        }
        self.seen.extend(present);
        self.resolve_issued(out);
    }

    /// An issued unit whose plate applied is restored; one whose plate failed with its owner gone is
    /// held (D5).
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
            let Some(plate) = self.view.slots.iter().flatten().find(|p| p.added_by == token) else { continue };
            if plate.applied {
                let plugin = self.is_plugin(id);
                self.restored(id, token, plugin, out);
            } else if plate.state == PlateState::Failed {
                let exited = self.exited.contains(&token);
                self.rewrite_owed |= self.loads.failed(token, exited);
                if exited {
                    self.hold(id, "its add failed with no answer to trust", Some(token));
                }
            }
        }
    }

    /// D7 (5) and SEAT-C: a load not terminal keeps the marker; one inside its settle goes.
    fn close(&mut self, out: &mut Vec<Action>) {
        self.closing = true;
        if !self.opened || self.done {
            return;
        }
        if let Some(on_disk) = &self.marker {
            let mut keep = self.loads.at_close();
            // A load whose marker write is out was never issued: the driver starts no add after close.
            if let Some(Await::LoadMarker { id, .. }) = &self.waiting {
                keep.retain(|k| k != id);
            }
            if keep.is_empty() {
                out.push(Action::RemoveMarker);
            } else if keep != *on_disk {
                out.push(Action::CommitMarker(keep));
            }
        }
        out.push(Action::Done);
    }
}

/// The token a disposition follows its plate by.
fn token_of(d: &Disposition) -> Option<InstallToken> {
    match d {
        Disposition::Issued { token } | Disposition::Held { token, .. } => *token,
        Disposition::Restored { token } | Disposition::Matched { token } => Some(*token),
        Disposition::Queued | Disposition::Forgotten | Disposition::Yielded => None,
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
