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
    /// A reload candidate took over, or this reload candidate was cancelled: send nothing and only receive.
    Superseded,
    /// A newer source already heads the slot after a rebuild: retire the unit's tone and tear down.
    Replaced,
    /// A removal is pending or settled for this row: keep the unit and wait for the removal's terminal.
    Removing,
    /// A reload candidate stands for this id: keep a pending restart and retry when the reload ends.
    Reloading,
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

/// Where a row stands with respect to a removal.
pub(crate) enum Leave {
    Staying,
    /// A `Remove` op is pending in the engine: any non-terminal return (`Withdrawn`, `Evicted`,
    /// `Refused`) is held here outside the mailbox until the removal settles (chain rows only;
    /// legacy `SlotHost` rows deliver returning boxes immediately).
    Pending { held: Option<Back> },
    /// Removal settled (or immediate removal / different-id source replacement) while the box was
    /// still in the engine: any late non-terminal box becomes `Back::Removed`, and
    /// `submit`/`withdraw` return `SubmitError::Removing`.
    Settled,
    /// A terminal `Back` (`Removed`, `Superseded`, or `Dismissed`) has already been delivered for
    /// this row: no second terminal may ever be posted.
    Done,
}

impl Leave {
    pub(crate) fn is_leaving(&self) -> bool {
        !matches!(self, Leave::Staying)
    }
}

fn take_nonterminal_unit(back: Back) -> Option<Unit> {
    match back {
        Back::Withdrawn { unit, .. } | Back::Evicted { unit, .. } | Back::Refused { unit, .. } => Some(unit),
        Back::Removed(unit) | Back::Superseded(unit) => Some(unit),
        Back::Dismissed => None,
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
    /// Removal state of this row (pending removal holds non-terminal returns outside `mailbox`).
    pub(crate) leave: Leave,
    pub(crate) in_engine: RowEngineState,
    /// Op number (`Op::n`) of the latest install submitted on this row while it awaits settlement.
    pub(crate) install_op: Option<u64>,
    pub(crate) mailbox: VecDeque<Back>,
    /// Terminal outcomes of ops belonging to this row, kept until the row is released so `poll` stays idempotent.
    pub(crate) settled: BTreeMap<u64, OpState>,
    /// Last settled `Remove` op for this row, handed off on `release` so a concurrent `wait_op` still reads it.
    pub(crate) last_remove_settled: Option<(u64, OpState)>,
    /// Legacy `SlotHost` owner id (0 for chain rows).
    pub(crate) legacy_owner: u64,
    /// The host's shutdown removed this row ([`EngineHost::remove_unit_at_shutdown`]) and its removal is
    /// the one that ends it: its owner keeps `unit-<id>.tone` where it is ([`UnitHost::keeps_tone`]), so
    /// the next launch's restore under the id finds the unit's own tone (`docs/unit-contract.md` § 1).
    /// Set under the state lock where that removal is accepted, before any terminal `Back` it causes, on
    /// every row it may end that no other removal or replacement has taken: the seated row and a live
    /// same-id reload candidate queued ahead of it (whose install, applied, the removal then ends). Cleared
    /// where another cause ends the row first (a source replacement, a refused removal), and never set
    /// later: a promotion carries nothing over. Every other end retires the tone into the slot's file.
    pub(crate) keep_tone: bool,
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
    /// Returned-box receipts keyed by install op number (`Op::n`): kept until that install op
    /// settles (`on_seat` or `Model::settle_rebuild`), even across owner release or a retry submit.
    received: BTreeMap<u64, (Left, bool)>,
    /// For each live reload candidate token, the specific predecessor row token it was reserved to
    /// replace (`EngineHost::reload_unit`): used on promotion and when `Left::Replaced` arrives
    /// before `Event::Seat` drains.
    reload_predecessor: BTreeMap<InstallToken, InstallToken>,
}

impl Table {
    pub(crate) fn new() -> Table {
        Table {
            rows: Vec::new(),
            next_token: 1,
            next_legacy_id: 1,
            next_legacy_owner: 1,
            received: BTreeMap::new(),
            reload_predecessor: BTreeMap::new(),
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
        self.live_reload_row_for_id(id).is_some()
    }

    /// The live same-id reload candidate for `id` (not released, not superseded), if one stands.
    pub(crate) fn live_reload_row_for_id(&self, id: UnitId) -> Option<InstallToken> {
        self.rows
            .iter()
            .find(|r| r.id == id && r.is_reload && !r.released && !r.superseded)
            .map(|r| r.token)
    }

    pub(crate) fn has_other_reload_row(&self, id: UnitId, exclude_token: InstallToken) -> bool {
        self.rows
            .iter()
            .any(|r| r.id == id && r.token != exclude_token && r.is_reload && !r.released && !r.superseded)
    }

    pub(crate) fn set_reload_predecessor(&mut self, candidate: InstallToken, predecessor: InstallToken) {
        self.reload_predecessor.insert(candidate, predecessor);
    }

    pub(crate) fn is_install_refused(&self, op_n: u64) -> bool {
        matches!(self.received.get(&op_n), Some((Left::Refused(_), _)))
    }

    fn is_reload_of(&self, candidate: InstallToken, predecessor: InstallToken) -> bool {
        self.reload_predecessor.get(&candidate) == Some(&predecessor)
            && self.row(candidate).is_some_and(|r| r.is_reload && !r.released && !r.superseded)
    }

    fn reload_candidate_for(&self, predecessor: InstallToken) -> Option<InstallToken> {
        self.reload_predecessor
            .iter()
            .find_map(|(&cand, &pred)| (pred == predecessor).then_some(cand))
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
            .find(|r| r.id == id && !r.released && !r.superseded && !r.is_reload && !r.id_freed)
    }

    pub(crate) fn active_row_for_id_mut(&mut self, id: UnitId) -> Option<&mut Row> {
        self.rows
            .iter_mut()
            .find(|r| r.id == id && !r.released && !r.superseded && !r.is_reload && !r.id_freed)
    }

    /// Row `token` was submitted to the engine of generation `gen` as `op_n`: it awaits a new answer.
    pub(crate) fn mark_queued(&mut self, token: InstallToken, gen: u64, op_n: u64) {
        if let Some(row) = self.row_mut(token) {
            row.in_engine = RowEngineState::Queued { generation: gen };
            row.install_op = Some(op_n);
        }
    }

    pub(crate) fn install_op(&self, token: InstallToken) -> Option<u64> {
        self.row(token).and_then(|r| r.install_op)
    }

    pub(crate) fn clear_install_op(&mut self, token: InstallToken, op_n: u64) {
        if let Some(row) = self.row_mut(token) {
            if row.install_op == Some(op_n) {
                row.install_op = None;
            }
        }
    }

    pub(crate) fn take_received(&mut self, op_n: u64) -> Option<(Left, bool)> {
        self.received.remove(&op_n)
    }

    pub(crate) fn clear_install_receipt(&mut self, token: InstallToken, op_n: u64) {
        self.received.remove(&op_n);
        self.clear_install_op(token, op_n);
    }

    pub(crate) fn mark_seated(&mut self, token: InstallToken, gen: u64) {
        if let Some(row) = self.row_mut(token) {
            if row.in_engine == (RowEngineState::Queued { generation: gen }) {
                row.in_engine = RowEngineState::Seated { generation: gen };
            }
        }
    }

    /// Promote reload candidate `token` to the active row and permanently mark its specific
    /// predecessor row superseded (clearing any pending removal state on that predecessor). A shutdown's
    /// removal pending on the predecessor moves to the candidate (`transfer_remove_to_promoted`); its keep
    /// does not: the candidate got its own where that removal was accepted ([`Row::keep_tone`]), and a
    /// copy here could revive one a replacement cleared after the candidate's terminal `Back`.
    pub(crate) fn promote_reload(&mut self, token: InstallToken) {
        if let Some(pred_tok) = self.reload_predecessor.remove(&token) {
            if let Some(pred) = self.row_mut(pred_tok) {
                pred.superseded = true;
                if let Leave::Pending { held: Some(old) } = std::mem::replace(&mut pred.leave, Leave::Staying) {
                    forget_back(old);
                }
            }
        }
        if let Some(row) = self.row_mut(token) {
            row.is_reload = false;
        }
    }

    /// End a live reload candidate without touching any predecessor row. Returns `true` if `token`
    /// was the active reload candidate (so the caller should cancel the plate's `reloading` state).
    pub(crate) fn fail_reload(&mut self, candidate_token: InstallToken) -> bool {
        self.reload_predecessor.remove(&candidate_token);
        let Some(row) = self.row_mut(candidate_token) else { return false };
        if row.is_reload && !row.released && !row.superseded {
            row.superseded = true;
            true
        } else {
            false
        }
    }

    /// Rule 4 (rebuild): end every reload candidate still in flight (not promoted by step 1) and
    /// return their `(slot, id)` pairs so the model clears `reloading`.
    pub(crate) fn cancel_in_flight_reloads(&mut self) -> Vec<(usize, UnitId)> {
        let mut cancelled = Vec::new();
        for row in &mut self.rows {
            if row.is_reload && !row.released && !row.superseded {
                row.superseded = true;
                self.reload_predecessor.remove(&row.token);
                cancelled.push((row.slot, row.id));
            }
        }
        cancelled
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

    /// Enter `Leave::Pending` on `token` for a newly sent `Remove` op: adopts any unclaimed
    /// non-terminal return already sitting in `mailbox` into `held` so the owner cannot take it
    /// while the removal is outstanding.
    pub(crate) fn begin_remove(&mut self, token: InstallToken, gen: u64) {
        let Some(row) = self.row_mut(token) else { return };
        if matches!(row.leave, Leave::Done) {
            return;
        }
        let mut held = match std::mem::replace(&mut row.leave, Leave::Staying) {
            Leave::Pending { held } => held,
            Leave::Staying | Leave::Settled | Leave::Done => None,
        };
        if held.is_none() {
            if let Some(idx) = row
                .mailbox
                .iter()
                .position(|b| matches!(b, Back::Withdrawn { .. } | Back::Evicted { .. } | Back::Refused { .. }))
            {
                held = row.mailbox.remove(idx);
            }
        }
        row.leave = Leave::Pending { held };
        if matches!(
            row.in_engine,
            RowEngineState::Queued { generation } | RowEngineState::Seated { generation } if generation == gen
        ) {
            row.in_engine = RowEngineState::Removing { generation: gen };
        }
    }

    /// Transfer pending removal ownership to a reload candidate `token` that just promoted while a
    /// `Remove` op stands behind its install. If `token` already received `Left::Removed` before
    /// `Event::Seat` drained, leave it `Done` so `finish_remove` will not post a duplicate terminal.
    pub(crate) fn transfer_remove_to_promoted(&mut self, token: InstallToken, gen: u64) {
        let already_removed = self.row(token).is_some_and(|r| {
            matches!(r.leave, Leave::Done)
                || (r.in_engine == RowEngineState::Out
                    && r.install_op
                        .and_then(|op_n| self.received.get(&op_n))
                        .is_some_and(|(left, _)| matches!(left, Left::Removed)))
        });
        if already_removed {
            return;
        }
        self.begin_remove(token, gen);
    }

    /// Settle a row whose `Remove` op applied: drains any box held in `Leave::Pending` as
    /// `Back::Removed`; if the box is already `Out` with nothing held, delivers `Back::Dismissed`;
    /// if the box is still in the engine (`in_engine != Out`), transitions to `Leave::Settled` so
    /// `file_returned` converts it to `Back::Removed` on arrival. Never posts a second terminal.
    pub(crate) fn finish_remove(&mut self, token: InstallToken) {
        let Some(row) = self.row_mut(token) else { return };
        let held = match std::mem::replace(&mut row.leave, Leave::Settled) {
            Leave::Pending { held } => held,
            other => {
                row.leave = other;
                return;
            }
        };
        if row.abandoned || row.released || row.superseded {
            row.leave = Leave::Done;
            if let Some(back) = held {
                forget_back(back);
            }
            return;
        }
        if let Some(back) = held {
            row.leave = Leave::Done;
            if let Some(unit) = take_nonterminal_unit(back) {
                row.mailbox.push_back(Back::Removed(unit));
            }
            return;
        }
        if row.in_engine != RowEngineState::Out {
            return;
        }
        row.leave = Leave::Done;
        for back in &mut row.mailbox {
            if matches!(back, Back::Withdrawn { .. } | Back::Evicted { .. } | Back::Refused { .. }) {
                let old = std::mem::replace(back, Back::Dismissed);
                if let Some(unit) = take_nonterminal_unit(old) {
                    *back = Back::Removed(unit);
                    return;
                }
            }
        }
        row.mailbox.push_back(Back::Dismissed);
    }

    /// Settle every live row of `old_id` when another id's install replaced its seat
    /// (`replaces: Some(old_id)`): a parked/withdrawn owner whose box is already `Out` gets
    /// `Back::Dismissed` (or its unclaimed box converted to `Back::Removed`); a row whose box is
    /// still in the engine enters `Leave::Settled` so arrival delivers `Back::Removed`. Never posts
    /// a second terminal once `Leave::Done` has been reached.
    pub(crate) fn finish_replaced(&mut self, old_id: UnitId) {
        for row in &mut self.rows {
            if row.id != old_id || row.released || row.abandoned || row.superseded {
                continue;
            }
            if row.is_reload {
                row.superseded = true;
                self.reload_predecessor.remove(&row.token);
                continue;
            }
            if matches!(row.leave, Leave::Done) {
                continue;
            }
            // The replacement ends this row, not a shutdown's removal queued behind it: it retires.
            row.keep_tone = false;
            let held = match std::mem::replace(&mut row.leave, Leave::Settled) {
                Leave::Pending { held } => held,
                Leave::Staying | Leave::Settled | Leave::Done => None,
            };
            if let Some(back) = held {
                row.leave = Leave::Done;
                if let Some(unit) = take_nonterminal_unit(back) {
                    row.mailbox.push_back(Back::Removed(unit));
                }
                continue;
            }
            if row.in_engine != RowEngineState::Out {
                continue;
            }
            row.leave = Leave::Done;
            let mut converted = false;
            for back in &mut row.mailbox {
                if matches!(back, Back::Withdrawn { .. } | Back::Evicted { .. } | Back::Refused { .. }) {
                    let old = std::mem::replace(back, Back::Dismissed);
                    if let Some(unit) = take_nonterminal_unit(old) {
                        *back = Back::Removed(unit);
                        converted = true;
                        break;
                    }
                }
            }
            if !converted {
                row.mailbox.push_back(Back::Dismissed);
            }
        }
    }

    pub(crate) fn cancel_remove(&mut self, token: InstallToken) {
        let Some(row) = self.row_mut(token) else { return };
        let held = match std::mem::replace(&mut row.leave, Leave::Staying) {
            Leave::Pending { held } => held,
            other => {
                row.leave = other;
                return;
            }
        };
        // A refused removal ended nothing: a shutdown's keep goes with it.
        row.keep_tone = false;
        if let Some(back) = held {
            if row.abandoned || row.released {
                forget_back(back);
            } else {
                row.mailbox.push_back(back);
            }
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
        self.file_returned_inner(slot, ret, None, pending_remove);
    }

    fn file_returned_inner(
        &mut self,
        slot: usize,
        ret: Returned,
        model: Option<&mut Model>,
        pending_remove: bool,
    ) {
        let Some(pos) = self.rows.iter().position(|r| r.token == ret.token) else {
            log::warn!("[engine_io] slot {slot}: returned unit id={} token={} has no row; leaking it", ret.id, ret.token);
            std::mem::forget(ret.unit);
            return;
        };
        let install_op = {
            let row = &mut self.rows[pos];
            row.in_engine = RowEngineState::Out;
            row.install_op
        };
        if let Some(op_n) = install_op {
            self.received.insert(op_n, (ret.left, ret.bypassed));
        }
        let row = &mut self.rows[pos];
        if row.abandoned || row.released {
            log::warn!("[engine_io] slot {slot}: an abandoned or released unit (id={}) came back; leaking it", ret.id);
            std::mem::forget(ret.unit);
            if row.released {
                self.rows.remove(pos);
                self.reload_predecessor.remove(&ret.token);
            }
            return;
        }

        // A reload candidate's install refusal always surfaces as `Back::Refused`, never `Superseded`
        // or `Removed`, even if `Event::Seat(Refused)` drained first or a removal was queued behind it.
        if self.rows[pos].is_reload {
            if let Left::Refused(why) = ret.left {
                self.rows[pos].mailbox.push_back(Back::Refused { unit: ret.unit, why });
                return;
            }
        }

        // `Left::Replaced` means "superseded by the reload" only when this row's reload candidate is
        // the first non-refused install replacing `ret.id` in the engine's order (or already promoted).
        // When a different-id install replacing `ret.id` is queued ahead of the candidate, it removed
        // this row: deliver `Back::Removed` and end the doomed reload candidate.
        let replaced_by_reload = if matches!(ret.left, Left::Replaced) {
            if self.rows[pos].superseded {
                true
            } else if let Some(m) = model.as_deref() {
                match m.first_pending_replacer(slot, ret.id, self) {
                    Some((cand_token, true)) => self.is_reload_of(cand_token, ret.token),
                    _ => false,
                }
            } else {
                false
            }
        } else {
            false
        };
        let row = &mut self.rows[pos];
        if row.superseded || replaced_by_reload {
            row.superseded = true;
            if matches!(row.leave, Leave::Done) {
                std::mem::forget(ret.unit);
                return;
            }
            if let Leave::Pending { held: Some(old) } = std::mem::replace(&mut row.leave, Leave::Done) {
                forget_back(old);
            }
            row.mailbox.push_back(Back::Superseded(ret.unit));
            return;
        }

        // Terminal engine causes deliver `Back::Removed` immediately and mark `Leave::Done`.
        if matches!(ret.left, Left::Removed | Left::Replaced) {
            let replaced = matches!(ret.left, Left::Replaced);
            if matches!(row.leave, Leave::Done) {
                std::mem::forget(ret.unit);
            } else {
                if replaced {
                    // A source replacement ahead of a shutdown's removal ends the row: it retires.
                    row.keep_tone = false;
                }
                if let Leave::Pending { held: Some(old) } = std::mem::replace(&mut row.leave, Leave::Done) {
                    forget_back(old);
                }
                row.mailbox.push_back(Back::Removed(ret.unit));
            }
            if replaced {
                if let Some(cand) = self.reload_candidate_for(ret.token) {
                    if self.fail_reload(cand) {
                        if let Some(m) = model {
                            m.cancel_reloading(slot, ret.id);
                        }
                    }
                }
            }
            return;
        }

        if pending_remove {
            if matches!(row.leave, Leave::Done) {
                std::mem::forget(ret.unit);
                return;
            }
            if let Leave::Pending { held: Some(old) } = std::mem::replace(&mut row.leave, Leave::Done) {
                forget_back(old);
            }
            row.mailbox.push_back(Back::Removed(ret.unit));
            return;
        }

        let nonterminal = match ret.left {
            Left::Withdrawn => Back::Withdrawn { unit: ret.unit, bypassed: ret.bypassed },
            Left::Evicted | Left::Unsent => Back::Evicted { unit: ret.unit, bypassed: ret.bypassed },
            Left::Refused(why) => Back::Refused { unit: ret.unit, why },
            Left::Removed | Left::Replaced => unreachable!(),
        };
        // Legacy `SlotHost` rows never hold boxes outside the mailbox: a returning box under a
        // pending removal is delivered immediately so teardown recovers it without waiting.
        if matches!(row.spec, Spec::Legacy) {
            match &mut row.leave {
                Leave::Pending { .. } | Leave::Settled => {
                    row.leave = Leave::Done;
                    if let Some(unit) = take_nonterminal_unit(nonterminal) {
                        row.mailbox.push_back(Back::Removed(unit));
                    }
                }
                Leave::Staying => {
                    row.mailbox.push_back(nonterminal);
                }
                Leave::Done => {
                    forget_back(nonterminal);
                }
            }
            return;
        }
        match &mut row.leave {
            Leave::Pending { held } => {
                if let Some(old) = held.replace(nonterminal) {
                    forget_back(old);
                }
            }
            Leave::Settled => {
                row.leave = Leave::Done;
                if let Some(unit) = take_nonterminal_unit(nonterminal) {
                    row.mailbox.push_back(Back::Removed(unit));
                }
            }
            Leave::Staying => {
                row.mailbox.push_back(nonterminal);
            }
            Leave::Done => {
                forget_back(nonterminal);
            }
        }
    }

    /// Drain every returned box on `port` into the table's mailboxes (legacy callers).
    pub(crate) fn reap_port(&mut self, slot: usize, port: &mut SlotPort) {
        while let Some(ret) = port.returned_unit() {
            self.file_returned(slot, ret);
        }
    }

    /// Drain every returned box on `port` into the table's mailboxes, reading `model`'s pending
    /// queue order to classify `Left::Replaced` against queued reload candidates.
    pub(crate) fn reap_port_with_model(&mut self, slot: usize, port: &mut SlotPort, model: &mut Model) {
        while let Some(ret) = port.returned_unit() {
            self.file_returned_inner(slot, ret, Some(model), false);
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
        self.reload_predecessor.remove(&token);
        let pos = self.rows.iter().position(|r| r.token == token)?;
        let row = &mut self.rows[pos];
        row.released = true;
        row.settled.clear();
        let remove_settled = row.last_remove_settled.take();
        if let Leave::Pending { held } = &mut row.leave {
            if let Some(back) = held.take() {
                forget_back(back);
            }
        }
        while let Some(back) = row.mailbox.pop_front() {
            forget_back(back);
        }
        if row.in_engine == RowEngineState::Out {
            self.rows.remove(pos);
        }
        remove_settled
    }

    /// Abandon row `token`: leak any boxes already waiting and mark it so any future box leaks.
    pub(crate) fn abandon_row(&mut self, token: InstallToken) {
        self.reload_predecessor.remove(&token);
        let Some(pos) = self.rows.iter().position(|r| r.token == token) else { return };
        let row = &mut self.rows[pos];
        row.abandoned = true;
        if let Leave::Pending { held } = &mut row.leave {
            if let Some(back) = held.take() {
                forget_back(back);
            }
        }
        while let Some(back) = row.mailbox.pop_front() {
            forget_back(back);
        }
        if row.released && row.in_engine == RowEngineState::Out {
            self.rows.remove(pos);
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
/// `Command::unit`), or a kind toggle (`Toggle::Units`: `Command::kind_toggle`), in one send order. A
/// unit command goes to the ring unless its id is reseating, or the gate already holds one for it or a
/// kind toggle; a kind toggle goes unless the gate holds anything, or a plate of its kind is reseating
/// (it switches every unit of the kind, so it waits until each is back: `docs/unit-contract.md` §2). So
/// no command overtakes a kind toggle sent before it, and a kind toggle overtakes nothing. The gate
/// survives a second rebuild and is flushed only by [`flush_gate`] (the feed's tick, and every send
/// while it holds anything, so a held entry that may go takes a freed ring slot before the send). A row not submitted
/// [`GATE_DEADLINE`] after a swap reads `failed`, and its commands drop; a later submit still seats it
/// (its terms fence it).
pub(crate) struct Gate {
    /// Every held command, oldest first.
    held: VecDeque<TimedCommand>,
}

/// What the gate holds: a unit command or a kind toggle.
fn gated(command: &Command) -> bool {
    command.unit().is_some() || command.kind_toggle().is_some()
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate { held: VecDeque::new() }
    }

    pub(crate) fn has(&self, id: UnitId) -> bool {
        self.held.iter().any(|c| c.command.unit() == Some(id))
    }

    /// Whether a kind toggle waits here: every unit command sent after it waits behind it.
    pub(crate) fn holds_kind_toggle(&self) -> bool {
        self.held.iter().any(|c| c.command.kind_toggle().is_some())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Hold `command` (a unit command or a kind toggle) behind everything held.
    pub(crate) fn push(&mut self, command: TimedCommand) {
        debug_assert!(gated(&command.command), "the gate holds unit commands and kind toggles only");
        self.held.push_back(command);
    }

    pub(crate) fn drop_id(&mut self, id: UnitId) -> u64 {
        let before = self.held.len();
        self.held.retain(|c| c.command.unit() != Some(id));
        (before - self.held.len()) as u64
    }

    /// Rebuild the gate: the old engine's unapplied unit commands and kind toggles (its table, then its
    /// ring), the old gate, and the input path's unsent ones, in that order, which is their send order:
    /// a command reached the old ring only when nothing it must follow was held.
    pub(crate) fn on_rebuild(
        &mut self,
        old_unapplied: impl IntoIterator<Item = Command>,
        midi_unsent: impl IntoIterator<Item = Command>,
    ) {
        let old_gate = std::mem::take(&mut self.held);
        let fresh = |command: Command| TimedCommand { frame: None, command };
        self.held.extend(old_unapplied.into_iter().filter(gated).map(fresh));
        self.held.extend(old_gate);
        self.held.extend(midi_unsent.into_iter().filter(gated).map(fresh));
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

    pub(crate) fn reap_port(&mut self, slot: usize, port: &mut SlotPort) {
        self.table.reap_port_with_model(slot, port, &mut self.model);
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

/// Returns true when an entry that may go found the ring full: the next free slot is its, so a caller
/// about to push something else refuses that instead (`push_all`).
pub(crate) fn flush_gate_locked(core: &Core, settings: &mut Settings, ends: &mut Ends, state: &mut HostState) -> bool {
    let now = Instant::now();
    let expired = state.model.expire_reseats(now, state.gate_deadline);
    for id in expired {
        let n = state.gate.drop_id(id);
        if n > 0 {
            core.gate_dropped.fetch_add(n, Relaxed);
        }
    }
    if state.gate.is_empty() {
        return false;
    }
    // In send order. Every entry before `i` is still held (a sent or dropped one leaves the gate), so a
    // kind toggle goes only from the front, and a unit command only past no held entry of its own id
    // and no held kind toggle.
    let HostState { gate, model, .. } = state;
    let mut held_ids: Vec<UnitId> = Vec::new();
    let mut toggle_held = false;
    let mut i = 0;
    while let Some(&cmd) = gate.held.get(i) {
        let send = match (cmd.command.unit(), cmd.command.kind_toggle()) {
            (Some(id), _) => match model.unit_gate_status(id) {
                GateStatus::Seated => !toggle_held && !held_ids.contains(&id),
                GateStatus::Reseating => false,
                GateStatus::Gone => {
                    // None of the id's entries is before `i`: the first one met drops them all.
                    let n = gate.drop_id(id);
                    core.gate_dropped.fetch_add(n, Relaxed);
                    continue;
                }
            },
            (None, Some(kind)) => i == 0 && !model.kind_reseating(kind),
            (None, None) => unreachable!("the gate holds unit commands and kind toggles only"),
        };
        if !send {
            match cmd.command.unit() {
                Some(id) => held_ids.push(id),
                None => toggle_held = true,
            }
            i += 1;
            continue;
        }
        if ends.commands.push(cmd).is_err() {
            core.counters.commands_full.fetch_add(1, Relaxed);
            return true;
        }
        gate.held.remove(i);
        ends.pushed += 1;
        settings.record(&cmd.command);
        settings.pushed(&cmd.command);
    }
    false
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

    pub(crate) fn install_token(&self) -> InstallToken {
        self.install
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
        let Some(row) = state.table.row(self.install) else { return };
        if row.superseded || row.released || row.abandoned {
            return;
        }
        let is_reload = row.is_reload;
        state.model.set_activated(self.slot, self.id, is_reload, a);
    }

    /// A built-in's params as its build clamped them.
    pub fn committed(&self, params: Params) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(row) = state.table.row(self.install) else { return };
        if row.superseded || row.released || row.abandoned || row.is_reload {
            return;
        }
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
            state.reap_port(self.slot, port);
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
        if row.superseded || row.released || row.abandoned {
            return Err((unit, SubmitError::Superseded));
        }
        if row.leave.is_leaving() {
            return Err((unit, SubmitError::Removing));
        }
        let is_reload = row.is_reload;

        let Some((_, plate)) = state.model.find_plate(self.id) else {
            return Err((unit, SubmitError::Superseded));
        };
        if is_reload {
            if !plate.seated_in_engine || plate.withdrawn || plate.state == PlateState::Leaving {
                if state.table.fail_reload(self.install) {
                    state.model.cancel_reloading(self.slot, self.id);
                }
                return Err((unit, SubmitError::Superseded));
            }
        } else if plate.state == PlateState::Leaving {
            return Err((unit, SubmitError::Removing));
        }
        let bypassed = plate.bypass;
        let is_source = if is_reload {
            plate.reload_activated.as_ref().map_or(plate.source, |a| a.source)
        } else {
            plate.source
        };
        let seated_in_engine = plate.seated_in_engine;
        let withdrawn = plate.withdrawn;
        let my_added_rev = plate.added_rev;

        let (at, replaces) = if is_reload {
            (0, Some(self.id))
        } else if seated_in_engine && withdrawn {
            (0, None)
        } else if is_source {
            let head = state.model.head_source_id(self.slot, Some(self.id));
            if let Some(head_id) = head {
                // An evicted/older source retrying after a rebuild must never replace a newer
                // source that has already queued or seated in the new engine.
                let head_is_newer = state
                    .model
                    .find_plate(head_id)
                    .is_some_and(|(_, hp)| hp.added_rev > my_added_rev);
                if head_is_newer {
                    if let Some(row) = state.table.row_mut(self.install) {
                        row.superseded = true;
                    }
                    state.model.remove_plate(self.slot, self.id);
                    return Err((unit, SubmitError::Replaced));
                }
            }
            (0, head)
        } else {
            (state.model.engine_index_for_insert(self.slot, self.id), None)
        };

        if let Err(unit) = port.install_at(self.id, self.install, at, bypassed, replaces, unit) {
            return Err((unit, SubmitError::PortFull));
        }

        // Expire any overdue reseat/withdrawal before clearing the timer so a late submit does not
        // rescue commands that already outlived `GATE_DEADLINE`.
        let deadline = state.gate_deadline;
        let expired = state.model.expire_reseats(Instant::now(), deadline);
        for exp in expired {
            let n = state.gate.drop_id(exp);
            if n > 0 {
                self.core.gate_dropped.fetch_add(n, Relaxed);
            }
        }
        let op = state.model.mint_op(self.install, cur_terms.generation, self.slot);
        state.table.mark_queued(self.install, cur_terms.generation, op.n());
        if let Some((_, plate)) = state.model.find_plate_mut(self.id) {
            // Submitting in time meets the reseat deadline; a submit held past it keeps its gate.
            plate.reseating_since = None;
            if plate.state == PlateState::Failed && !plate.parked {
                // Late submit after GATE_DEADLINE still seats it.
                plate.state = PlateState::Loading;
                plate.error = None;
            }
        }
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
    /// Only a plugin unit withdraws; a built-in's `SetUnitParam` never races a withdrawal.
    pub fn withdraw(&self) -> Result<Op, SubmitError> {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.reap_port(self.slot, port);
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
        if row.superseded || row.released || row.abandoned {
            return Err(SubmitError::Superseded);
        }
        if row.leave.is_leaving()
            || state
                .model
                .find_plate(self.id)
                .is_some_and(|(_, p)| p.state == PlateState::Leaving)
        {
            return Err(SubmitError::Removing);
        }
        if state.table.has_other_reload_row(self.id, self.install) {
            return Err(SubmitError::Reloading);
        }
        if row.is_reload || row.in_engine.generation() != Some(gen) {
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
            state.reap_port(self.slot, port);
        }
        state.table.row_mut(self.install).and_then(|r| r.mailbox.pop_front())
    }

    /// Whether the host's shutdown's removal is what ended this install ([`Row::keep_tone`]): its owner
    /// keeps `unit-<id>.tone` instead of retiring it into the slot's file. Read after the terminal
    /// `Back`, which is posted under the same lock as the disposition.
    pub fn keeps_tone(&self) -> bool {
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.table.row(self.install).is_some_and(|r| r.keep_tone)
    }

    /// Whether this row's removal is under way and its box not yet delivered ([`Leave::Pending`] or
    /// [`Leave::Settled`]): a stopping owner waits for that box, never for a seated unit's.
    pub fn removal_in_flight(&self) -> bool {
        let state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .table
            .row(self.install)
            .is_some_and(|r| matches!(r.leave, Leave::Pending { .. } | Leave::Settled))
    }

    /// Claim the next box in this row's mailbox or a box held in [`Leave::Pending`] during a
    /// pending removal, for a stopping owner's teardown, while preserving the removal reservation.
    pub fn take_back_for_shutdown(&self) -> Option<Back> {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = port_guard.as_mut() {
            state.reap_port(self.slot, port);
        }
        let row = state.table.row_mut(self.install)?;
        if let Some(back) = row.mailbox.pop_front() {
            return Some(back);
        }
        if let Leave::Pending { held } = &mut row.leave {
            return held.take();
        }
        None
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
        let Some(row) = state.table.row(self.install) else { return };
        if row.superseded || row.released || row.abandoned || row.is_reload || row.leave.is_leaving() {
            return;
        }
        state.model.mark_parked(self.slot, self.id, why);
    }

    /// No seat: the plate reads `failed`, and the id is free (unless a removal for it is still pending).
    pub fn failed(&self, why: String) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(row) = state.table.row(self.install) else { return };
        if row.superseded || row.released || row.abandoned {
            return;
        }
        if row.is_reload {
            if state.table.fail_reload(self.install) {
                state.model.cancel_reloading(self.slot, self.id);
            }
            return;
        }
        if row.leave.is_leaving() || state.model.has_pending_remove(self.id) {
            return;
        }
        if let Some(row) = state.table.row_mut(self.install) {
            row.id_freed = true;
        }
        state.model.mark_failed(self.slot, self.id, why);
    }

    /// The owner gives up on its unit: its box, if it ever comes, leaks.
    pub fn abandon(&self) {
        let mut port_guard = self.core.ports[self.slot].lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        state.table.abandon_row(self.install);
        if let Some(port) = port_guard.as_mut() {
            state.reap_port(self.slot, port);
        }
    }

    /// The owner is done: the reservation ends here.
    pub fn release(self) {
        let mut state = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.table.fail_reload(self.install) {
            state.model.cancel_reloading(self.slot, self.id);
        }
        if let Some((op_n, op_state)) = state.table.release_row(self.install) {
            state.model.record_unowned_settled(op_n, op_state);
        }
    }
}

/// Builds a built-in unit at the engine's terms and answers its params as it clamped them: in the app
/// `super::builtins::Builtins` over `lf_engine::units`; a clamping fake in the tests.
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
        if state.table.is_id_reserved(id) || state.model.has_live_plate(id) || state.model.has_pending_remove(id) {
            return Err(format!("unit id {id} is already reserved"));
        }
        if !is_legacy && state.model.active_seat_count(slot) >= MAX_UNITS {
            let may_replace_source = state.model.has_head_source_plate(slot)
                && matches!(spec, Spec::Plugin(_) | Spec::Builtin { kind: UnitKind::Synth, .. });
            if !may_replace_source {
                return Err(format!("slot {slot} seats no more units"));
            }
        }
        // Any gate entries still queued for `id` belong to a unit that is gone (e.g. an immediate
        // removal after a rebuild): drop and count them before reserving the id again.
        let dropped = state.gate.drop_id(id);
        if dropped > 0 {
            self.core.gate_dropped.fetch_add(dropped, Relaxed);
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
            leave: Leave::Staying,
            in_engine: RowEngineState::Out,
            install_op: None,
            mailbox: VecDeque::new(),
            settled: BTreeMap::new(),
            last_remove_settled: None,
            legacy_owner: 0,
            keep_tone: false,
        });
        Ok(UnitHost::new(self.core.clone(), slot, id, token))
    }

    /// A reload candidate under a reserved id: a second row; the plate reads `reloading`. While it
    /// stands, the seated row's `withdraw` returns [`SubmitError::Reloading`].
    pub fn reload_unit(&self, id: UnitId, plugin: PluginRef) -> Result<UnitHost, String> {
        if self.core.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }
        let gen = self.core.engine_gen.load(Acquire);
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
        let Some(active) = state.table.active_row_for_id(id) else {
            return Err(format!("unit {id} is not seated"));
        };
        if active.in_engine != (RowEngineState::Seated { generation: gen }) || active.leave.is_leaving() {
            return Err(format!("unit {id} is not seated"));
        }
        let predecessor = active.token;
        let token = state.table.mint_token();
        state.model.mark_reloading(slot, id, plugin.clone());
        state.table.set_reload_predecessor(token, predecessor);
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
            leave: Leave::Staying,
            in_engine: RowEngineState::Out,
            install_op: None,
            mailbox: VecDeque::new(),
            settled: BTreeMap::new(),
            last_remove_settled: None,
            legacy_owner: 0,
            keep_tone: false,
        });
        Ok(UnitHost::new(self.core.clone(), slot, id, token))
    }

    /// Remove `id`: a failed add's plate goes at once ([`Removal::Plate`]); otherwise the removal is
    /// sent, and the plate leaves at its `Applied`. Its owner retires the unit's tone into the slot's file.
    pub fn remove_unit(&self, id: UnitId) -> Result<Removal, String> {
        self.remove(id, false)
    }

    /// [`EngineHost::remove_unit`] for the host's shutdown (`Chains::unload_all`): when this removal is
    /// the one that ends the unit, its owner keeps `unit-<id>.tone` where it is ([`UnitHost::keeps_tone`]),
    /// so the next launch's restore of the unit under its id finds its own tone. A unit already leaving
    /// (the player's removal, a source replacement returned or queued ahead of this removal) retires as
    /// before, and so does one whose removal the port refuses: a unit the device's close then evicts
    /// keeps its file anyway (`Row::keep_tone`).
    pub fn remove_unit_at_shutdown(&self, id: UnitId) -> Result<Removal, String> {
        self.remove(id, true)
    }

    fn remove(&self, id: UnitId, keep_tone: bool) -> Result<Removal, String> {
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
            state.reap_port(slot, port);
        }
        let Some((_, plate)) = state.model.find_plate(id) else {
            return Err(format!("unit {id} not found"));
        };
        if let Some(existing_op) = state.model.pending_remove_op(slot, id) {
            return Ok(Removal::Sent(existing_op));
        }
        let seated_in_engine = plate.seated_in_engine;
        let parked = plate.parked;
        let failed_no_seat = plate.state == PlateState::Failed && !parked && !seated_in_engine;
        let has_pending_install = state.model.has_pending_install(slot, id);
        let owner_token = state.table.active_row_for_id(id).map(|r| r.token);
        // The shutdown's keep binds to its own removal, set below where it is accepted and under the same
        // lock as the terminal `Back`, and only on a row no other cause has begun to take: a row already
        // leaving (a player's removal, a replacement whose box came back) keeps its retire. A same-id reload
        // candidate queued ahead of this removal is a row it may end (the candidate's install applies
        // first): it binds there too, before the candidate's box can come back ahead of its promotion.
        let staying = |tok: InstallToken| state.table.row(tok).is_some_and(|r| matches!(r.leave, Leave::Staying));
        let candidate_keep = keep_tone
            .then(|| state.table.live_reload_row_for_id(id))
            .flatten()
            .filter(|&tok| staying(tok));
        let keep_tone = keep_tone && owner_token.is_some_and(staying);

        if failed_no_seat {
            state.model.remove_plate(slot, id);
            if let Some(tok) = owner_token {
                if !state.table.row(tok).is_some_and(|r| r.id_freed || matches!(r.leave, Leave::Done)) {
                    if let Some(row) = state.table.row_mut(tok) {
                        row.leave = Leave::Done;
                        row.keep_tone = keep_tone;
                    }
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
                    if !matches!(row.leave, Leave::Done) {
                        row.keep_tone = keep_tone;
                        let held = match std::mem::replace(&mut row.leave, Leave::Done) {
                            Leave::Pending { held } => held,
                            Leave::Staying | Leave::Settled | Leave::Done => None,
                        };
                        let mut converted = false;
                        if let Some(back) = held {
                            if let Some(unit) = take_nonterminal_unit(back) {
                                row.mailbox.push_back(Back::Removed(unit));
                                converted = true;
                            }
                        }
                        if !converted {
                            for back in &mut row.mailbox {
                                if matches!(back, Back::Evicted { .. } | Back::Withdrawn { .. } | Back::Refused { .. }) {
                                    let old = std::mem::replace(back, Back::Dismissed);
                                    if let Some(unit) = take_nonterminal_unit(old) {
                                        *back = Back::Removed(unit);
                                        converted = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if !converted {
                            row.mailbox.push_back(Back::Dismissed);
                        }
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
            state.table.begin_remove(tok, gen);
            if keep_tone {
                if let Some(row) = state.table.row_mut(tok) {
                    row.keep_tone = true;
                }
            }
        }
        if let Some(row) = candidate_keep.and_then(|tok| state.table.row_mut(tok)) {
            row.keep_tone = true;
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
            state.reap_port(slot, port);
        }
        let Some(port) = port_guard.as_mut() else {
            return Err("no audio device is open".to_string());
        };
        let (to_plate, to_engine) = state.model.translate_move(slot, id, to)?;
        if !port.move_to(id, to_engine) {
            return Err(format!("slot {slot}'s port is full"));
        }
        let gen = self.core.engine_gen.load(Acquire);
        // Moves mint with token 0 so their settled outcomes live in the unowned store instead of
        // growing `Row::settled` unboundedly over the unit's lifetime.
        let op = state.model.mint_op(0, gen, slot);
        state.model.push_pending(
            slot,
            PendingOp {
                op,
                unit: id,
                seat_op: SeatOp::Move,
                state: OpState::Pending,
                kind: PendingKind::Move { to_plate, to_engine },
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
