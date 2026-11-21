//! OWNS: the chain units' handshake with the engine (E2, `docs/unit-contract.md` §2, §3): the table of
//! installs (one row per [`UnitHost`], its install token, its mailbox), each install's port messages as
//! [`Op`]s settled from the slot's `Event::Seat`s, the gate that holds a unit's commands while it
//! reseats, and the rebuild's handoff of every row to the new engine. The model of the chains (what the
//! UI reads) is `chain.rs`; the Tauri commands are `chain_cmds.rs`.
//!
//! Identity is per install, completion per operation: every [`UnitHost`] has an install token the
//! engine carries and hands back with the box and why it left (`lf_engine::Left`); every port message
//! is an [`Op`] settled from the slot's seat answers, which come in message order. A rebuild is settled
//! from the old engine's own state (its chains, its owed param answers, its unapplied unit commands),
//! never from its event ring. The legacy slot path keeps its code: [`super::SlotHost`] stays, an
//! adapter over this table (`Spec::Legacy`), until activation deletes it.

// Some public seam items are called by sibling E2 pieces (P3/P4/P6) in parallel worktrees.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{
    ChainSeat, Command, Engine, InstallToken, Left, Returned, SeatOp, SeatRefusal, SlotPort,
    TimedCommand, UnitId, UnitKind, UnitParam, UnitProcessor, MAX_UNITS, SLOT_COUNT,
};

use super::chain::{GateStatus, Model, PendingKind, PendingOp, PlateState, PluginRef};
use super::settings::Settings;
use super::{lock_at, Core, Ends, EngineHost, LockSite};

/// A unit on its way into or out of the engine.
pub type Unit = Box<dyn UnitProcessor>;

/// The engine terms a unit is activated at, read under the slot's port lock (a rebuild writes all
/// three with every port held). [`UnitHost::submit`] refuses stale terms ([`SubmitError::Stale`]) and
/// hands the box back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    pub generation: u64,
    pub rate: u32,
    pub max_block: usize,
}

/// One port message of one install: settled from the slot's `Event::Seat`s, in push order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    install: InstallToken,
    n: u64,
    generation: u64,
    slot: u8,
}

impl Op {
    pub(crate) fn new(install: InstallToken, n: u64, generation: u64, slot: u8) -> Op {
        Op { install, n, generation, slot }
    }

    pub(crate) fn install(self) -> InstallToken {
        self.install
    }

    pub(crate) fn n(self) -> u64 {
        self.n
    }

    pub(crate) fn generation(self) -> u64 {
        self.generation
    }

    pub(crate) fn slot(self) -> u8 {
        self.slot
    }
}

/// Where an [`Op`] stands ([`UnitHost::poll`], [`EngineHost::wait_op`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpState {
    Pending,
    /// The engine answered `Waiting`: held in its port until no capture holds the alignment; a second
    /// answer resolves it.
    Held,
    Applied,
    Refused(SeatRefusal),
    /// An install the old engine never applied: its box is in the mailbox as [`Back::Evicted`], and the
    /// owner submits again at the new [`Terms`].
    Rebuilt,
}

/// A box in a row's mailbox ([`UnitHost::take_back`]), every owner turn.
pub enum Back {
    /// The id ended: a removal, or a source another id replaced. Arrives unasked: straight to teardown.
    Removed(Unit),
    /// A reload took its seat. Arrives unasked: straight to teardown.
    Superseded(Unit),
    /// A withdrawal took it; its seat stays. Re-activate at [`UnitHost::terms`] and submit again.
    Withdrawn { unit: Unit, bypassed: bool },
    /// A rebuild took it. Re-activate at [`UnitHost::terms`] and submit again.
    Evicted { unit: Unit, bypassed: bool },
    /// Its install was refused (never started or stopped).
    Refused { unit: Unit, why: SeatRefusal },
    /// A removal of a seat with no unit in the engine (parked): nothing comes back; release what the
    /// owner holds.
    Dismissed,
}

/// Leak any unit inside `back` instead of dropping it (a drop calls into a plugin's DLL).
pub(crate) fn forget_back(back: Back) {
    match back {
        Back::Removed(unit)
        | Back::Superseded(unit)
        | Back::Withdrawn { unit, .. }
        | Back::Evicted { unit, .. }
        | Back::Refused { unit, .. } => std::mem::forget(unit),
        Back::Dismissed => {}
    }
}

/// Why [`UnitHost::submit`] or [`UnitHost::withdraw`] sent nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The terms belong to an engine a rebuild has replaced.
    Stale,
    PortFull,
    /// A reload candidate's `submit` took over: this row may send nothing and only receives.
    Superseded,
    Closing,
    NoEngine,
}

/// What a reserved id holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    Plugin(PluginRef),
    Builtin { kind: UnitKind, params: Params },
    /// The legacy slot path ([`super::SlotHost`]): a device-minted id per install, as today. In the
    /// table and the model's index arithmetic, never in `chains`.
    Legacy,
}

/// A built-in unit's parameters, in the order they apply.
pub type Params = Vec<(UnitParam, f64)>;

/// What a loaded unit reports onto its plate ([`UnitHost::activated`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Activated {
    pub name: String,
    /// Its role: a source heads a chain and takes no input.
    pub source: bool,
    pub latency: Frame,
}

/// How a removal goes ([`EngineHost::remove_unit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// A failed add's plate: gone at once, nothing sent.
    Plate,
    /// Sent; the plate leaves at its `Applied`.
    Sent(Op),
}

/// Where a row's unit box sits with respect to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowEngineState {
    Out,
    Queued { generation: u64 },
    Seated { generation: u64 },
    Withdrawing { generation: u64 },
    Removing { generation: u64 },
}

impl RowEngineState {
    pub(crate) fn generation(self) -> Option<u64> {
        match self {
            RowEngineState::Out => None,
            RowEngineState::Queued { generation }
            | RowEngineState::Seated { generation }
            | RowEngineState::Withdrawing { generation }
            | RowEngineState::Removing { generation } => Some(generation),
        }
    }
}

pub(crate) struct Row {
    pub(crate) token: InstallToken,
    pub(crate) id: UnitId,
    pub(crate) slot: usize,
    pub(crate) spec: Spec,
    pub(crate) is_reload: bool,
    pub(crate) superseded: bool,
    pub(crate) abandoned: bool,
    pub(crate) released: bool,
    /// True after `UnitHost::failed` freed `id` on a failed add (before `release`).
    pub(crate) id_freed: bool,
    /// True while a removal for this row has been queued and its terminal box/`Dismissed` has not
    /// yet been filed into `mailbox`.
    pub(crate) removing: bool,
    pub(crate) in_engine: RowEngineState,
    pub(crate) mailbox: VecDeque<Back>,
    /// Terminal outcomes of ops belonging to this row, kept until the row is released so `poll` stays idempotent.
    pub(crate) settled: BTreeMap<u64, OpState>,
    /// Last settled `Remove` op for this row, handed off on `release` so a concurrent `wait_op` still reads it.
    pub(crate) last_remove_settled: Option<(u64, OpState)>,
    /// Legacy `SlotHost` owner id (0 for chain rows).
    pub(crate) legacy_owner: u64,
}

/// The table: one row per install (two under one id while a reload's candidate stands beside the
/// seated instance), each with its install token and its mailbox. P2 builds it into `Core`.
///
/// - The port's reader, whoever holds its lock, files every `Returned` by token into its row's mailbox.
///   A row with no owner left, or abandoned, leaks its box. Nothing here drops a unit.
/// - The reservation lasts from [`EngineHost::unit`] to [`UnitHost::release`]; the plate leaves at the
///   removal's `Applied`.
/// - Legacy rows (`Spec::Legacy`, a device-minted id per install) are rows like any other.
pub(crate) struct Table {
    rows: Vec<Row>,
    next_token: InstallToken,
    next_legacy_id: UnitId,
    next_legacy_owner: u64,
    /// What each live row last received from the engine since its last submit (its box's cause and
    /// bypass), by install token: a rebuild settles a pending install from it when the reader filed
    /// its box before its `Seat` was drained (`Model::settle_rebuild`).
    received: BTreeMap<InstallToken, (Left, bool)>,
}

impl Table {
    pub(crate) fn new() -> Table {
        Table {
            rows: Vec::new(),
            next_token: 1,
            next_legacy_id: 1,
            next_legacy_owner: 1,
            received: BTreeMap::new(),
        }
    }

    pub(crate) fn mint_token(&mut self) -> InstallToken {
        loop {
            let t = self.next_token;
            self.next_token = self.next_token.wrapping_add(1);
            if t != 0 {
                return t;
            }
        }
    }

    pub(crate) fn mint_legacy_owner(&mut self) -> u64 {
        loop {
            let o = self.next_legacy_owner;
            self.next_legacy_owner = self.next_legacy_owner.wrapping_add(1);
            if o != 0 {
                return o;
            }
        }
    }

    pub(crate) fn mint_legacy_id(&mut self) -> UnitId {
        loop {
            let id = self.next_legacy_id;
            self.next_legacy_id = self.next_legacy_id.wrapping_add(1);
            if id != 0 && !self.rows.iter().any(|r| r.id == id) {
                return id;
            }
        }
    }

    pub(crate) fn has_any_chain(&self) -> bool {
        self.rows.iter().any(|r| !matches!(r.spec, Spec::Legacy) && !r.released)
    }

    pub(crate) fn has_any_legacy(&self) -> bool {
        self.rows
            .iter()
            .any(|r| matches!(r.spec, Spec::Legacy) && (!r.released || r.in_engine != RowEngineState::Out))
    }

    pub(crate) fn is_id_reserved(&self, id: UnitId) -> bool {
        self.rows.iter().any(|r| r.id == id && !r.released && !r.id_freed)
    }

    pub(crate) fn has_active_row(&self, id: UnitId) -> bool {
        self.rows
            .iter()
            .any(|r| r.id == id && !r.released && !r.abandoned && !r.superseded && !r.id_freed)
    }

    pub(crate) fn has_reload_row(&self, id: UnitId) -> bool {
        self.rows.iter().any(|r| r.id == id && r.is_reload && !r.released)
    }

    pub(crate) fn is_superseded(&self, token: InstallToken) -> bool {
        self.row(token).is_some_and(|r| r.superseded)
    }

    pub(crate) fn insert(&mut self, row: Row) {
        self.rows.push(row);
    }

    pub(crate) fn row(&self, token: InstallToken) -> Option<&Row> {
        self.rows.iter().find(|r| r.token == token)
    }

    pub(crate) fn row_mut(&mut self, token: InstallToken) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.token == token)
    }

    pub(crate) fn active_row_for_id(&self, id: UnitId) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| r.id == id && !r.released && !r.superseded)
    }

    pub(crate) fn active_row_for_id_mut(&mut self, id: UnitId) -> Option<&mut Row> {
        self.rows
            .iter_mut()
            .find(|r| r.id == id && !r.released && !r.superseded)
    }

    /// Row `token` was submitted to the engine of generation `gen`: it awaits a new answer.
    pub(crate) fn mark_queued(&mut self, token: InstallToken, gen: u64) {
        if let Some(row) = self.row_mut(token) {
            row.in_engine = RowEngineState::Queued { generation: gen };
        }
        self.received.remove(&token);
    }

    /// What row `token` received since its last submit ([`Table::received`]'s field).
    pub(crate) fn received(&self, token: InstallToken) -> Option<(Left, bool)> {
        self.received.get(&token).copied()
    }

    pub(crate) fn mark_seated(&mut self, token: InstallToken, gen: u64) {
        if let Some(row) = self.row_mut(token) {
            if row.in_engine == (RowEngineState::Queued { generation: gen }) {
                row.in_engine = RowEngineState::Seated { generation: gen };
            }
            row.is_reload = false;
        }
    }

    pub(crate) fn promote_reload(&mut self, token: InstallToken) {
        if let Some(row) = self.row_mut(token) {
            row.is_reload = false;
        }
    }

    pub(crate) fn mark_outgoing_superseded(&mut self, id: UnitId, candidate_token: InstallToken) {
        for row in &mut self.rows {
            if row.id == id && row.token != candidate_token && !row.released {
                row.superseded = true;
            }
        }
    }

    pub(crate) fn clear_superseded_for_failed_reload(&mut self, id: UnitId, candidate_token: InstallToken) {
        for row in &mut self.rows {
            if row.id == id && row.token != candidate_token && !row.released {
                row.superseded = false;
            }
        }
    }

    pub(crate) fn record_settled(&mut self, op: Op, state: OpState) -> bool {
        if let Some(row) = self.row_mut(op.install()) {
            if !row.released {
                row.settled.insert(op.n(), state);
                return true;
            }
        }
        false
    }

    pub(crate) fn note_remove_settled(&mut self, token: InstallToken, op_n: u64, state: OpState) {
        if let Some(row) = self.row_mut(token) {
            row.last_remove_settled = Some((op_n, state));
        }
    }

    pub(crate) fn settled(&self, op: Op) -> Option<OpState> {
        self.row(op.install()).and_then(|r| r.settled.get(&op.n()).copied())
    }

    /// Settle a row whose `Remove` op applied: if its box has already left the engine (`Out`) and was
    /// not yet turned into a terminal `Back::Removed`, convert any unclaimed non-terminal box in its
    /// mailbox to `Back::Removed`, else post `Back::Dismissed`. If the box is still in the engine or
    /// port (`in_engine != Out`), leave `row.removing = true` so `file_returned` converts it on arrival.
    pub(crate) fn finish_remove(&mut self, token: InstallToken) {
        let Some(row) = self.row_mut(token) else { return };
        if !row.removing || row.in_engine != RowEngineState::Out {
            return;
        }
        row.removing = false;
        if row.abandoned || row.released || row.superseded {
            return;
        }
        for back in &mut row.mailbox {
            if matches!(back, Back::Withdrawn { .. } | Back::Evicted { .. } | Back::Refused { .. }) {
                let old = std::mem::replace(back, Back::Dismissed);
                match old {
                    Back::Withdrawn { unit, .. }
                    | Back::Evicted { unit, .. }
                    | Back::Refused { unit, .. } => {
                        *back = Back::Removed(unit);
                        return;
                    }
                    _ => unreachable!(),
                }
            }
        }
        row.mailbox.push_back(Back::Dismissed);
    }

    pub(crate) fn cancel_remove(&mut self, token: InstallToken) {
        if let Some(row) = self.row_mut(token) {
            row.removing = false;
        }
    }

    pub(crate) fn post_back(&mut self, token: InstallToken, back: Back) {
        if let Some(row) = self.row_mut(token) {
            if row.abandoned || row.released {
                forget_back(back);
            } else {
                row.mailbox.push_back(back);
            }
        } else {
            forget_back(back);
        }
    }

    /// File a box returned by `SlotPort::returned_unit` during normal operation.
    pub(crate) fn file_returned(&mut self, slot: usize, ret: Returned) {
        self.file_returned_rebuild(slot, ret, false);
    }

    /// File a box returned by `SlotPort::returned_unit` (normal or during rebuild).
    pub(crate) fn file_returned_rebuild(&mut self, slot: usize, ret: Returned, pending_remove: bool) {
        let Some(pos) = self.rows.iter().position(|r| r.token == ret.token) else {
            log::warn!("[engine_io] slot {slot}: returned unit id={} token={} has no row; leaking it", ret.id, ret.token);
            std::mem::forget(ret.unit);
            return;
        };
        let row = &mut self.rows[pos];
        row.in_engine = RowEngineState::Out;
        if row.abandoned || row.released {
            log::warn!("[engine_io] slot {slot}: an abandoned or released unit (id={}) came back; leaking it", ret.id);
            std::mem::forget(ret.unit);
            if row.released {
                self.rows.remove(pos);
                self.received.remove(&ret.token);
            }
            return;
        }
        self.received.insert(ret.token, (ret.left, ret.bypassed));
        let row = &mut self.rows[pos];
        let back = if row.superseded {
            // The outgoing row of a same-id reload never re-activates.
            Back::Superseded(ret.unit)
        } else if pending_remove || row.removing {
            row.removing = false;
            Back::Removed(ret.unit)
        } else {
            match ret.left {
                Left::Removed => Back::Removed(ret.unit),
                Left::Replaced => Back::Removed(ret.unit),
                Left::Withdrawn => Back::Withdrawn { unit: ret.unit, bypassed: ret.bypassed },
                Left::Evicted | Left::Unsent => Back::Evicted { unit: ret.unit, bypassed: ret.bypassed },
                Left::Refused(why) => Back::Refused { unit: ret.unit, why },
            }
        };
        row.mailbox.push_back(back);
    }

    /// Drain every returned box on `port` into the table's mailboxes.
    pub(crate) fn reap_port(&mut self, slot: usize, port: &mut SlotPort) {
        while let Some(ret) = port.returned_unit() {
            self.file_returned(slot, ret);
        }
    }

    /// How many rows the table says are still in `gen` (or queued for `gen`), excluding tokens
    /// already collected in `returned`.
    pub(crate) fn remaining_in_engine(&self, gen: u64, returned: &[(usize, Returned)]) -> usize {
        let seen: BTreeSet<InstallToken> = returned.iter().map(|(_, r)| r.token).collect();
        self.rows
            .iter()
            .filter(|r| r.in_engine.generation() == Some(gen) && !seen.contains(&r.token))
            .count()
    }

    /// Release row `token`: leak any unclaimed boxes in its mailbox, drop its settled op outcomes,
    /// and remove the row immediately if its box is already out of the engine (otherwise keep as a
    /// tombstone so a late box leaks). Returns the row's last settled `Remove` op, if any, so a
    /// concurrent `wait_op` caller can still read it.
    pub(crate) fn release_row(&mut self, token: InstallToken) -> Option<(u64, OpState)> {
        let pos = self.rows.iter().position(|r| r.token == token)?;
        let row = &mut self.rows[pos];
        row.released = true;
        row.settled.clear();
        let remove_settled = row.last_remove_settled.take();
        while let Some(back) = row.mailbox.pop_front() {
            forget_back(back);
        }
        if row.in_engine == RowEngineState::Out {
            self.rows.remove(pos);
            self.received.remove(&token);
        }
        remove_settled
    }

    /// Abandon row `token`: leak any boxes already waiting and mark it so any future box leaks.
    pub(crate) fn abandon_row(&mut self, token: InstallToken) {
        let Some(pos) = self.rows.iter().position(|r| r.token == token) else { return };
        let row = &mut self.rows[pos];
        row.abandoned = true;
        while let Some(back) = row.mailbox.pop_front() {
            forget_back(back);
        }
        if row.released && row.in_engine == RowEngineState::Out {
            self.rows.remove(pos);
            self.received.remove(&token);
        }
    }

    // --- Legacy SlotHost helpers ---

    /// Whether `slot` holds any live legacy row (including an abandoned unit still in the engine).
    pub(crate) fn legacy_slot_occupied(&self, slot: usize) -> bool {
        self.rows.iter().any(|r| {
            r.slot == slot
                && matches!(r.spec, Spec::Legacy)
                && (!r.released || r.in_engine != RowEngineState::Out)
        })
    }

    /// Whether `slot` currently has any seated/queued row (legacy or chain), used by `probe.rs`.
    pub(crate) fn slot_occupied(&self, slot: usize) -> bool {
        self.rows.iter().any(|r| {
            r.slot == slot && !r.abandoned && !r.released && r.in_engine != RowEngineState::Out
        })
    }

    pub(crate) fn legacy_row_of_owner(&self, slot: usize, owner: u64) -> Option<&Row> {
        self.rows.iter().find(|r| {
            r.slot == slot
                && matches!(r.spec, Spec::Legacy)
                && r.legacy_owner == owner
                && !r.abandoned
                && !r.released
        })
    }

    pub(crate) fn legacy_row_of_owner_mut(&mut self, slot: usize, owner: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| {
            r.slot == slot
                && matches!(r.spec, Spec::Legacy)
                && r.legacy_owner == owner
                && !r.abandoned
                && !r.released
        })
    }

    #[cfg(test)]
    pub(crate) fn legacy_has_evicted(&self, slot: usize) -> bool {
        self.rows.iter().any(|r| {
            r.slot == slot && matches!(r.spec, Spec::Legacy) && !r.abandoned && !r.released && !r.mailbox.is_empty()
        })
    }

    #[cfg(test)]
    pub(crate) fn legacy_unit_id(&self, slot: usize) -> UnitId {
        self.rows
            .iter()
            .find(|r| r.slot == slot && matches!(r.spec, Spec::Legacy) && !r.abandoned && !r.released)
            .map_or(0, |r| r.id)
    }
}

/// A unit command held back from the ring (`SetUnitBypass`, `SetUnitParam`, `ToggleUnit`:
/// `Command::unit`). A unit command goes to the ring unless its id is reseating or the gate already
/// holds one for it. The gate is per unit id, survives a second rebuild, and is flushed only by
/// [`flush_gate`]. A row not submitted [`GATE_DEADLINE`] after a swap reads `failed`, and its commands
/// drop; a later submit still seats it (its terms fence it).
pub(crate) struct Gate {
    queues: BTreeMap<UnitId, VecDeque<TimedCommand>>,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate { queues: BTreeMap::new() }
    }

    pub(crate) fn has(&self, id: UnitId) -> bool {
        self.queues.get(&id).is_some_and(|q| !q.is_empty())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    pub(crate) fn push(&mut self, id: UnitId, command: TimedCommand) {
        self.queues.entry(id).or_default().push_back(command);
    }

    pub(crate) fn drop_id(&mut self, id: UnitId) -> u64 {
        self.queues.remove(&id).map_or(0, |q| q.len() as u64)
    }

    /// Rebuild the gate: the old engine's unapplied unit commands, the old gate, and the MIDI
    /// queue's unsent unit commands, in that order per unit id.
    pub(crate) fn on_rebuild(
        &mut self,
        old_unapplied: impl IntoIterator<Item = Command>,
        midi_unsent: impl IntoIterator<Item = Command>,
    ) {
        let old_gate = std::mem::take(&mut self.queues);
        for cmd in old_unapplied {
            if let Some(id) = cmd.unit() {
                self.queues.entry(id).or_default().push_back(TimedCommand { frame: None, command: cmd });
            }
        }
        for (id, cmds) in old_gate {
            if !cmds.is_empty() {
                self.queues.entry(id).or_default().extend(cmds);
            }
        }
        for cmd in midi_unsent {
            if let Some(id) = cmd.unit() {
                self.queues.entry(id).or_default().push_back(TimedCommand { frame: None, command: cmd });
            }
        }
    }
}

/// The table, model and gate under one lock (`Core::state`).
/// Lock order: ports → settings → ends → stash → state; nothing is taken under `state`.
pub(crate) struct HostState {
    pub(crate) table: Table,
    pub(crate) model: Model,
    pub(crate) gate: Gate,
    pub(crate) gate_deadline: Duration,
}

impl HostState {
    pub(crate) fn new() -> HostState {
        HostState {
            table: Table::new(),
            model: Model::new(),
            gate: Gate::new(),
            gate_deadline: GATE_DEADLINE,
        }
    }

    pub(crate) fn poll(&self, op: Op) -> OpState {
        self.model.poll(&self.table, op)
    }
}

/// How long after a swap a row may go unsubmitted before its plate reads `failed` and its gated
/// commands drop: the legacy load's bound (`host::engine_slot`'s `LOAD_TIMEOUT`).
pub(crate) const GATE_DEADLINE: Duration = Duration::from_secs(15);

/// Send the gate's commands whose id is seated, drop and count those whose id is gone, keep the rest;
/// a full ring leaves the rest for the next call. Called where `settings` and `ends` are already held.
/// Lock order: ports → settings → ends → model; the model is taken inside, and nothing is taken under
/// it.
pub(crate) fn flush_gate(core: &Core, settings: &mut Settings, ends: &mut Ends) {
    let mut state = core.state.lock().unwrap_or_else(|e| e.into_inner());
    flush_gate_locked(core, settings, ends, &mut state);
}

pub(crate) fn flush_gate_locked(core: &Core, settings: &mut Settings, ends: &mut Ends, state: &mut HostState) {
    let now = Instant::now();
    let expired = state.model.expire_reseats(now, state.gate_deadline);
    for id in expired {
        let n = state.gate.drop_id(id);
        if n > 0 {
            core.gate_dropped.fetch_add(n, Relaxed);
        }
    }
    if state.gate.is_empty() {
        return;
    }
    let ids: Vec<UnitId> = state.gate.queues.keys().copied().collect();
    let mut ring_full = false;
    for id in ids {
        match state.model.unit_gate_status(id) {
            GateStatus::Seated => {
                if ring_full {
                    continue;
                }
                let Some(q) = state.gate.queues.get_mut(&id) else { continue };
                while let Some(cmd) = q.pop_front() {
                    if ends.commands.push(cmd).is_err() {
                        q.push_front(cmd);
                        core.counters.commands_full.fetch_add(1, Relaxed);
                        ring_full = true;
                        break;
                    }
                    ends.pushed += 1;
                    settings.record(&cmd.command);
                    settings.pushed(&cmd.command);
                }
                if q.is_empty() {
                    state.gate.queues.remove(&id);
                }
            }
            GateStatus::Reseating => {}
            GateStatus::Gone => {
                let n = state.gate.drop_id(id);
                if n > 0 {
                    core.gate_dropped.fetch_add(n, Relaxed);
                }
            }
        }
    }
}

/// First engine ever (no old engine to evict): advance the model's generation to `next`.
pub(crate) fn begin_first_engine(core: &Core, next: u64) {
    let mut state = core.state.lock().unwrap_or_else(|e| e.into_inner());
    state.model.advance(next);
}

/// The rebuild handoff, in `owner.rs` `swap_engine` with every port held (P2 replaces `evict` with it):
/// read the old engine's chains (`Engine::chain`) and unsent params (`Engine::unsent_unit_params`);
/// begin the model's rebuild at `next` (from then on it ignores the old generation's events, its
/// eviction `Chain` included); `evict` until every row the table says is in this engine or queued is
/// back (else false: the caller leaks the engine); settle each pending op from the boxes (`Evicted` →
/// `Applied`, `Unsent` → `Rebuilt`, a pending removal → `Applied` with its box filed as
/// [`Back::Removed`], a pending move applied to the model's order); a withdrawn seat with no row
/// re-activating (parked) becomes `failed`, no seat. Then, in the section that replaces the ends, the
/// gate becomes: the old engine's unapplied unit commands (`Engine::unapplied_unit_commands`), the old
/// gate, the MIDI queue's (`RebuildHook::rebuild`'s return), in that order.
pub(crate) fn rebuild_handoff(
    core: &Core,
    ports: &mut [MutexGuard<'_, Option<SlotPort>>; SLOT_COUNT],
    old: &mut Engine,
    next: u64,
) -> bool {
    let old_gen = core.engine_gen.load(Acquire);
    let old_chains: [Vec<ChainSeat>; SLOT_COUNT] = std::array::from_fn(|s| old.chain(s).collect());
    let unsent_params: Vec<(UnitId, UnitParam, f64)> = old.unsent_unit_params().collect();

    {
        // Its event ring first, under `ends` (ports → settings → ends → model): a `Seat`, `Chain` or
        // `UnitParam` there is the only copy (the engine no longer owes it), and from `begin_rebuild`
        // on the model ignores this generation. Peeked, not popped (`peek_unit_events`).
        let mut ends = core.ends.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ends) = ends.as_mut() {
            super::peek_unit_events(&mut state, &mut ends.events, old_gen, &core.chain_protocol_errors);
        }
        state.model.begin_rebuild(next, &old_chains, &unsent_params);
    }

    // Drain any boxes already waiting on the ports before evicting, so the return rings start free.
    let mut returned: Vec<(usize, Returned)> = Vec::new();
    for (slot, port) in ports.iter_mut().enumerate() {
        if let Some(port) = port.as_mut() {
            while let Some(ret) = port.returned_unit() {
                returned.push((slot, ret));
            }
        }
    }

    let mut clean = true;
    loop {
        let ok = catch_unwind(AssertUnwindSafe(|| {
            old.service_slots_idle();
            old.evict_slots();
        }))
        .is_ok();
        if !ok {
            clean = false;
            core.counters.panics.fetch_add(1, Relaxed);
            log::error!("[engine_io] a unit panicked while it was evicted; leaking the old engine");
        }
        let mut back = 0usize;
        for (slot, port) in ports.iter_mut().enumerate() {
            if let Some(port) = port.as_mut() {
                while let Some(ret) = port.returned_unit() {
                    returned.push((slot, ret));
                    back += 1;
                }
            }
        }
        if !clean {
            break;
        }
        let remaining = {
            let state = core.state.lock().unwrap_or_else(|e| e.into_inner());
            state.table.remaining_in_engine(old_gen, &returned)
        };
        if remaining == 0 {
            break;
        }
        if back == 0 {
            log::error!("[engine_io] {remaining} unit(s) did not return during eviction; leaking the old engine");
            clean = false;
            break;
        }
    }

    {
        let mut state = core.state.lock().unwrap_or_else(|e| e.into_inner());
        let HostState { table, model, .. } = &mut *state;
        model.settle_rebuild(returned, table, Instant::now());
    }

    clean
}

/// While no device runs nothing services the port: do it here, under the engine lock. A unit that
/// panics here faults the engine as a callback panic does (the owner replaces it); a faulted engine
/// is left to that.
pub(crate) fn service_if_idle(core: &Core) {
    if core.running.load(Acquire) {
        return;
    }
    let mut rt = core.rt.lock().unwrap_or_else(|e| e.into_inner());
    if core.running.load(Acquire) || rt.faulted {
        return;
    }
    let rt = &mut *rt;
    let Some(engine) = rt.engine.as_mut() else { return };
    if catch_unwind(AssertUnwindSafe(|| engine.service_slots_idle())).is_err() {
        core.counters.panics.fetch_add(1, Relaxed);
        core.latch_fault(rt);
    }
}

/// If no feed drain has run within [`super::chain::STASH_AFTER`], drain the engine's event ring
/// into [`Core::stash`] so `Event::Seat`, `Event::Chain`, and `Event::UnitParam` reach the model
/// even when the feed is stopped. Never runs beside a ticking feed.
pub(crate) fn drain_if_stale(core: &Core) {
    {
        let stash = core.stash.lock().unwrap_or_else(|e| e.into_inner());
        if !stash.is_stale(Instant::now()) {
            return;
        }
    }
    let mut settings = lock_at(&core.settings, LockSite::Settings).unwrap_or_else(|e| e.into_inner());
    let Ok(mut ends) = lock_at(&core.ends, LockSite::Ends) else { return };
    let gen = core.engine_gen.load(Acquire);
    let Some(ends) = ends.as_mut() else { return };
    let mut stash = core.stash.lock().unwrap_or_else(|e| e.into_inner());
    if !stash.is_stale(Instant::now()) {
        return;
    }
    let mut state = core.state.lock().unwrap_or_else(|e| e.into_inner());
    super::drain(&mut settings, &mut state, &mut ends.events, gen, &core.chain_protocol_errors, |e| {
        stash.push(gen, e);
    });
    flush_gate_locked(core, &mut settings, ends, &mut state);
}

/// Wait for `op` to leave `Pending`/`Held`; `bound` counts only while the model shows `op`'s slot
/// holding nothing; with `closing` or `return_on_held` set, `Held` (or waiting behind a held op)
/// returns `Ok(OpState::Held)` at once.
pub(crate) fn wait_op_inner(core: &Core, op: Op, bound: Duration, return_on_held: bool) -> Result<OpState, String> {
    let mut unheld = Duration::ZERO;
    let mut last = Instant::now();
    loop {
        service_if_idle(core);
        drain_if_stale(core);
        let now = Instant::now();
        let dt = now.saturating_duration_since(last);
        last = now;
        let closing = core.closing.load(Acquire);
        let (state, slot_held) = {
            let st = core.state.lock().unwrap_or_else(|e| e.into_inner());
            (st.poll(op), st.model.slot_held(usize::from(op.slot())))
        };
        match state {
            OpState::Applied | OpState::Refused(_) | OpState::Rebuilt => return Ok(state),
            OpState::Held => {
                if closing || return_on_held {
                    return Ok(OpState::Held);
                }
            }
            OpState::Pending => {
                if slot_held {
                    if closing || return_on_held {
                        return Ok(OpState::Held);
                    }
                } else {
                    unheld += dt;
                    if unheld >= bound {
                        return Err(format!("slot {}'s op {} did not settle within {} ms", op.slot(), op.n(), bound.as_millis()));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// One install's handle: its owner (a chain unit's owner thread, or the legacy [`super::SlotHost`])
/// submits, polls and takes its boxes back through it. A clone is the same install.
#[derive(Clone)]
pub struct UnitHost {
    core: Arc<Core>,
    slot: usize,
    id: UnitId,
    install: InstallToken,
}

impl UnitHost {
    pub(crate) fn new(core: Arc<Core>, slot: usize, id: UnitId, install: InstallToken) -> UnitHost {
        UnitHost { core, slot, id, install }
    }

    pub fn id(&self) -> UnitId {
        self.id
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The engine's terms, read under the port lock; `None` while no engine exists.
    pub fn terms(&self) -> Option<Terms> {
        let port = self.core.ports[self.slot].lock().ok()?;
        port.as_ref()?;
        let rate = self.core.rate.load(Relaxed);
        if rate == 0 {
            return None;
        }
        Some(Terms {
            generation: self.core.engine_gen.load(Acquire),
            rate,
            max_block: self.core.max_block.load(Relaxed) as usize,
        })
    }

    /// The name, role and latency onto the plate.
    pub fn activated(&self, a: Activated) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let is_reload = state.table.row(self.install).is_some_and(|r| r.is_reload);
        state.model.set_activated(self.slot, self.id, is_reload, a);
    }

    /// A built-in's params as its build clamped them.
    pub fn committed(&self, params: Params) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.model.set_committed(self.slot, self.id, params);
    }

    /// Hand `unit`, activated at `terms`, to the engine. The index, `replaces` (a head source over a
    /// seated one; a reload candidate: its own id) and the bypass are the model's (a same-id reseat
    /// sends the model's latest bypass, never a stale one). Index and push happen in one port-locked
    /// section. Stale terms, a full port, a superseded row, closing, or no engine: the box comes back.
    pub fn submit(&self, unit: Unit, terms: Terms) -> Result<Op, (Unit, SubmitError)> {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
        if self.core.closing.load(Acquire) {
            return Err((unit, SubmitError::Closing));
        }
        let Some(port) = port_guard.as_mut() else {
            return Err((unit, SubmitError::NoEngine));
        };
        let cur_rate = self.core.rate.load(Relaxed);
        if cur_rate == 0 {
            return Err((unit, SubmitError::NoEngine));
        }
        let cur_terms = Terms {
            generation: self.core.engine_gen.load(Acquire),
            rate: cur_rate,
            max_block: self.core.max_block.load(Relaxed) as usize,
        };
        if cur_terms != terms {
            return Err((unit, SubmitError::Stale));
        }
        let Some(row) = state.table.row(self.install) else {
            return Err((unit, SubmitError::Superseded));
        };
        if row.superseded || row.released || row.abandoned || row.removing {
            return Err((unit, SubmitError::Superseded));
        }
        let is_reload = row.is_reload;

        let Some((_, plate)) = state.model.find_plate(self.id) else {
            return Err((unit, SubmitError::Superseded));
        };
        let bypassed = plate.bypass;
        let is_source = if is_reload {
            plate.reload_activated.as_ref().map_or(plate.source, |a| a.source)
        } else {
            plate.source
        };
        let seated_in_engine = plate.seated_in_engine;
        let withdrawn = plate.withdrawn;

        let (at, replaces) = if is_reload && seated_in_engine {
            (0, Some(self.id))
        } else if seated_in_engine && withdrawn {
            (0, None)
        } else if is_source {
            let head = state.model.head_source_id(self.slot, Some(self.id));
            (0, head)
        } else {
            (state.model.engine_index_for_insert(self.slot, self.id), None)
        };

        if let Err(unit) = port.install_at(self.id, self.install, at, bypassed, replaces, unit) {
            return Err((unit, SubmitError::PortFull));
        }

        if is_reload {
            state.table.mark_outgoing_superseded(self.id, self.install);
        }
        state.table.mark_queued(self.install, cur_terms.generation);
        if !is_reload {
            if let Some((_, plate)) = state.model.find_plate_mut(self.id) {
                // Submitting in time meets the reseat deadline; a submit held past it keeps its gate.
                plate.reseating_since = None;
                if plate.state == PlateState::Failed && !plate.parked {
                    // Late submit after GATE_DEADLINE still seats it.
                    plate.state = PlateState::Loading;
                    plate.error = None;
                }
            }
        }
        let op = state.model.mint_op(self.install, cur_terms.generation, self.slot);
        state.model.push_pending(
            self.slot,
            PendingOp {
                op,
                unit: self.id,
                seat_op: SeatOp::Install,
                state: OpState::Pending,
                kind: PendingKind::Install {
                    token: self.install,
                    is_reload,
                    replaces,
                    at,
                    source: is_source,
                },
            },
        );
        Ok(op)
    }

    /// Take the unit out and keep its seat (a restart): its box comes back as [`Back::Withdrawn`].
    pub fn withdraw(&self) -> Result<Op, SubmitError> {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
        if self.core.closing.load(Acquire) {
            return Err(SubmitError::Closing);
        }
        let Some(port) = port_guard.as_mut() else {
            return Err(SubmitError::NoEngine);
        };
        let gen = self.core.engine_gen.load(Acquire);
        let Some(row) = state.table.row(self.install) else {
            return Err(SubmitError::Superseded);
        };
        if row.superseded || row.released || row.abandoned || row.removing {
            return Err(SubmitError::Superseded);
        }
        if row.in_engine.generation() != Some(gen) {
            return Err(SubmitError::Stale);
        }
        if !port.withdraw(self.id) {
            return Err(SubmitError::PortFull);
        }
        if let Some(row) = state.table.row_mut(self.install) {
            row.in_engine = RowEngineState::Withdrawing { generation: gen };
        }
        let op = state.model.mint_op(self.install, gen, self.slot);
        state.model.push_pending(
            self.slot,
            PendingOp {
                op,
                unit: self.id,
                seat_op: SeatOp::Withdraw,
                state: OpState::Pending,
                kind: PendingKind::Withdraw,
            },
        );
        Ok(op)
    }

    /// Where `op` stands. Idempotent. While `op` has not settled, services the port if no device
    /// runs and drains the engine's event ring when the feed is stale, without sleeping.
    pub fn poll(&self, op: Op) -> OpState {
        let current = {
            let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            state.poll(op)
        };
        if !matches!(current, OpState::Pending | OpState::Held) {
            return current;
        }
        service_if_idle(&self.core);
        drain_if_stale(&self.core);
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.poll(op)
    }

    /// The next box in this row's mailbox; every owner turn.
    pub fn take_back(&self) -> Option<Back> {
        // Reap the port first so any unit the engine returned reaches its row's mailbox immediately.
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
        state.table.row_mut(self.install).and_then(|r| r.mailbox.pop_front())
    }

    /// The slot holds a held message (the model's state).
    pub fn held(&self) -> bool {
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.model.slot_held(self.slot)
    }

    /// The owner keeps the unit out (a refused reseat): the plate reads `failed` and `parked`, the seat
    /// stays.
    pub fn parked(&self, why: String) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.model.mark_parked(self.slot, self.id, why);
    }

    /// No seat: the plate reads `failed`, and the id is free.
    pub fn failed(&self, why: String) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let is_reload = state.table.row(self.install).is_some_and(|r| r.is_reload);
        if is_reload {
            state.table.clear_superseded_for_failed_reload(self.id, self.install);
            state.model.cancel_reloading(self.slot, self.id);
        } else {
            if let Some(row) = state.table.row_mut(self.install) {
                row.id_freed = true;
            }
            state.model.mark_failed(self.slot, self.id, why);
        }
    }

    /// The owner gives up on its unit: its box, if it ever comes, leaks.
    pub fn abandon(&self) {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.table.abandon_row(self.install);
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(self.slot, port);
        }
    }

    /// The owner is done: the reservation ends here.
    pub fn release(self) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let is_reload = state.table.row(self.install).is_some_and(|r| r.is_reload);
        if is_reload {
            state.model.cancel_reloading(self.slot, self.id);
        }
        if let Some((op_n, op_state)) = state.table.release_row(self.install) {
            state.model.record_unowned_settled(op_n, op_state);
        }
    }
}

/// Builds a built-in unit at the engine's terms and answers its params as it clamped them. E3:
/// `lf_engine::units`; E2: none in the app (a built-in add is refused), a clamping fake in the tests.
pub trait BuiltinFactory: Send + Sync {
    fn build(&self, kind: UnitKind, params: &Params, terms: Terms) -> Result<(Unit, Params), String>;
}

impl EngineHost {
    /// Reserve `id` across both slots and show its plate `loading` at plate index `at`. Refused at
    /// once: 0, reserved, a full chain unless it may replace the head source, the slot or the run held
    /// by the other mode (legacy / chain), closing.
    pub fn unit(&self, slot: usize, id: UnitId, spec: Spec, at: usize, bypassed: bool) -> Result<UnitHost, String> {
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }
        if self.core.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }
        if id == 0 {
            return Err("unit id 0 is invalid".to_string());
        }
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let is_legacy = matches!(spec, Spec::Legacy);
        if is_legacy {
            if state.table.has_any_chain() || state.model.has_non_legacy_plate() {
                return Err("chain units are active in this run".to_string());
            }
        } else if state.table.has_any_legacy() || state.model.has_legacy_plate() {
            return Err("legacy plugin slots are active in this run".to_string());
        }
        if state.table.is_id_reserved(id) {
            return Err(format!("unit id {id} is already reserved"));
        }
        if !is_legacy && state.model.active_seat_count(slot) >= MAX_UNITS {
            let may_replace_source = state.model.has_head_source_plate(slot)
                && matches!(spec, Spec::Plugin(_) | Spec::Builtin { kind: UnitKind::Synth, .. });
            if !may_replace_source {
                return Err(format!("slot {slot} seats no more units"));
            }
        }
        let token = state.table.mint_token();
        state.model.add_plate(slot, id, &spec, at, bypassed);
        state.table.insert(Row {
            token,
            id,
            slot,
            spec,
            is_reload: false,
            superseded: false,
            abandoned: false,
            released: false,
            id_freed: false,
            removing: false,
            in_engine: RowEngineState::Out,
            mailbox: VecDeque::new(),
            settled: BTreeMap::new(),
            last_remove_settled: None,
            legacy_owner: 0,
        });
        Ok(UnitHost::new(self.core.clone(), slot, id, token))
    }

    /// A reload candidate under a reserved id: a second row; the plate reads `reloading`. From the
    /// candidate's `submit` on, the outgoing row may send nothing and only receives.
    pub fn reload_unit(&self, id: UnitId, plugin: PluginRef) -> Result<UnitHost, String> {
        if self.core.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some((slot, plate)) = state.model.find_plate(id) else {
            return Err(format!("unit {id} not found"));
        };
        if plate.legacy || plate.kind != UnitKind::Plugin {
            return Err(format!("unit {id} is not a plugin unit"));
        }
        if !plate.seated_in_engine || plate.withdrawn || plate.state != PlateState::Seated {
            return Err(format!("unit {id} is not seated"));
        }
        if plate.reloading || state.table.has_reload_row(id) {
            return Err(format!("unit {id} is already reloading"));
        }
        let token = state.table.mint_token();
        state.model.mark_reloading(slot, id, plugin.clone());
        state.table.insert(Row {
            token,
            id,
            slot,
            spec: Spec::Plugin(plugin),
            is_reload: true,
            superseded: false,
            abandoned: false,
            released: false,
            id_freed: false,
            removing: false,
            in_engine: RowEngineState::Out,
            mailbox: VecDeque::new(),
            settled: BTreeMap::new(),
            last_remove_settled: None,
            legacy_owner: 0,
        });
        Ok(UnitHost::new(self.core.clone(), slot, id, token))
    }

    /// Remove `id`: a failed add's plate goes at once ([`Removal::Plate`]); otherwise the removal is
    /// sent, and the plate leaves at its `Applied`.
    pub fn remove_unit(&self, id: UnitId) -> Result<Removal, String> {
        let slot = {
            let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .model
                .find_plate(id)
                .map(|(s, _)| s)
                .ok_or_else(|| format!("unit {id} not found"))?
        };
        let mut port_guard = self.core.ports[slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(slot, port);
        }
        let Some((_, plate)) = state.model.find_plate(id) else {
            return Err(format!("unit {id} not found"));
        };
        let seated_in_engine = plate.seated_in_engine;
        let parked = plate.parked;
        let failed_no_seat = plate.state == PlateState::Failed && !parked && !seated_in_engine;
        let has_pending_install = state.model.has_pending_install(slot, id);
        let owner_token = state.table.active_row_for_id(id).map(|r| r.token);

        if failed_no_seat {
            state.model.remove_plate(slot, id);
            if let Some(tok) = owner_token {
                if !state.table.row(tok).is_some_and(|r| r.id_freed) {
                    state.table.post_back(tok, Back::Dismissed);
                }
            }
            return Ok(Removal::Plate);
        }

        // Reseating after a rebuild (not yet in the new engine) or no engine running: remove plate
        // at once and notify the owner.
        if (!seated_in_engine && !has_pending_install) || port_guard.is_none() {
            state.model.remove_plate(slot, id);
            if let Some(tok) = owner_token {
                if let Some(row) = state.table.row_mut(tok) {
                    row.superseded = true;
                    let mut converted = false;
                    for back in &mut row.mailbox {
                        if let Back::Evicted { .. } = back {
                            let old = std::mem::replace(back, Back::Dismissed);
                            if let Back::Evicted { unit, .. } = old {
                                *back = Back::Removed(unit);
                                converted = true;
                            }
                        }
                    }
                    if !converted {
                        row.mailbox.push_back(Back::Dismissed);
                    }
                }
            }
            return Ok(Removal::Plate);
        }

        let port = port_guard.as_mut().expect("checked above");
        if !port.remove(id) {
            return Err(format!("slot {slot}'s port is full"));
        }
        let gen = self.core.engine_gen.load(Acquire);
        if let Some(tok) = owner_token {
            if let Some(row) = state.table.row_mut(tok) {
                row.removing = true;
                if matches!(
                    row.in_engine,
                    RowEngineState::Queued { generation } | RowEngineState::Seated { generation } if generation == gen
                ) {
                    row.in_engine = RowEngineState::Removing { generation: gen };
                }
            }
        }
        state.model.mark_leaving(slot, id);
        let op = state.model.mint_op(owner_token.unwrap_or(0), gen, slot);
        state.model.push_pending(
            slot,
            PendingOp {
                op,
                unit: id,
                seat_op: SeatOp::Remove,
                state: OpState::Pending,
                kind: PendingKind::Remove { owner_token },
            },
        );
        Ok(Removal::Sent(op))
    }

    /// `to`: a plate index, translated to the engine's index as the op is pushed.
    pub fn move_unit(&self, id: UnitId, to: usize) -> Result<Op, String> {
        let slot = {
            let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .model
                .find_plate(id)
                .map(|(s, _)| s)
                .ok_or_else(|| format!("unit {id} not found"))?
        };
        let mut port_guard = self.core.ports[slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.table.reap_port(slot, port);
        }
        let Some(port) = port_guard.as_mut() else {
            return Err("no audio device is open".to_string());
        };
        let (to_plate, to_engine, before) = state.model.translate_move(slot, id, to)?;
        if !port.move_to(id, to_engine) {
            return Err(format!("slot {slot}'s port is full"));
        }
        let gen = self.core.engine_gen.load(Acquire);
        let owner_token = state.table.active_row_for_id(id).map_or(0, |r| r.token);
        let op = state.model.mint_op(owner_token, gen, slot);
        state.model.push_pending(
            slot,
            PendingOp {
                op,
                unit: id,
                seat_op: SeatOp::Move,
                state: OpState::Pending,
                kind: PendingKind::Move { to_plate, to_engine, before },
            },
        );
        Ok(op)
    }

    /// Block until `op` leaves `Pending`/`Held`; `bound` counts only while the model shows the slot
    /// holding nothing; with closing set, `Held` returns at once. Drains for itself when the feed is
    /// stale (the stash).
    pub fn wait_op(&self, op: Op, bound: Duration) -> Result<OpState, String> {
        wait_op_inner(&self.core, op, bound, false)
    }

    /// The feed: frame `seq` carried the model's revision `rev`.
    pub fn chain_told(&self, rev: u64, seq: u64) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.model.chain_told(rev, seq);
    }

    /// The seq of the first frame that told `id`'s plate: a reload's `since` is checked against it.
    pub fn told(&self, id: UnitId) -> Option<u64> {
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.model.told(id)
    }

    /// The app is closing: a wait that meets `Held` stops at once.
    pub fn set_closing(&self) {
        self.core.closing.store(true, Release);
    }
}
