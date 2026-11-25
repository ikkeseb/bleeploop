//! OWNS: the model of the slots' chains (E2, `docs/unit-contract.md` §2): each slot's plates as the
//! engine applied them, the pending ops per slot, the revision the feed tells the UI, and the wire's
//! shapes of it ([`ChainState`], [`UnitState`], [`UnitSpec`]). The UI draws the chain from the feed's
//! `chains` alone (invariant 3). The table and the handles are `unit_host.rs`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lf_engine::grid::Frame;
use lf_engine::{
    ChainSeat, Event, InstallToken, Left, Returned, SeatOp, SeatOutcome, UnitId,
    UnitKind, UnitParam, SLOT_COUNT,
};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::unit_host::{Activated, Op, OpState, Params, Spec, Table};

/// A plugin's identity, as the scan names it (`format`, `path`, `id`): the tone store's identity, so a
/// unit's plugin and its tone are one key.
pub(crate) type PluginRef = crate::host::tone::ToneIdentity;

/// Bound on settled op outcomes kept for ops without a live row (e.g. a `Remove` op whose owner
/// already called `release`, or a rowless test op). Live rows keep all their own outcomes until release.
const MAX_UNOWNED_SETTLED_OPS: usize = 256;
/// Minimum age on the model's clock before an unowned settled outcome may be evicted past
/// [`MAX_UNOWNED_SETTLED_OPS`]. Every `wait_op` caller is bounded well below this.
pub(crate) const UNOWNED_KEEP: Duration = Duration::from_secs(60);

/// Where a unit stands for the command gate ([`super::unit_host::Gate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateStatus {
    Seated,
    Reseating,
    Gone,
}

/// One plate in a slot's chain model. Legacy rows (`legacy == true`) participate in index arithmetic
/// and never appear in `ChainState`.
#[derive(Clone, Debug)]
pub(crate) struct Plate {
    pub(crate) id: UnitId,
    pub(crate) legacy: bool,
    pub(crate) kind: UnitKind,
    pub(crate) plugin: Option<PluginRef>,
    pub(crate) params: Option<Params>,
    pub(crate) bypass: bool,
    pub(crate) source: bool,
    pub(crate) name: String,
    pub(crate) latency: Frame,
    pub(crate) state: PlateState,
    pub(crate) error: Option<String>,
    pub(crate) parked: bool,
    pub(crate) held: bool,
    pub(crate) reloading: bool,
    /// True while the current engine generation has a seat for this id (withdrawn/parked included;
    /// leaving/failed-add/unsubmitted excluded).
    pub(crate) seated_in_engine: bool,
    /// True while the seat in the current engine is withdrawn (restart or parked).
    pub(crate) withdrawn: bool,
    /// Set at a rebuild while this unit has not yet reseated in the new engine (bounds `GATE_DEADLINE`).
    pub(crate) reseating_since: Option<Instant>,
    /// Model revision when this plate was added; used with `chain_told` to set `told_seq`.
    pub(crate) added_rev: u64,
    /// The first feed frame `seq` that carried this plate (`EngineHost::told`, reload's `since`).
    pub(crate) told_seq: Option<u64>,
    /// The role its last applied install seated it in (`Some(true)`: a source); `None` before any.
    /// An effect that comes back a source behind another seat is refused its place
    /// (`UnitHost::submit`), and never counts as an older source for a newer one to end.
    pub(crate) seated_source: Option<bool>,
    /// Reload candidate's pending plugin ref and activation info until its install applies.
    pub(crate) reload_plugin: Option<PluginRef>,
    pub(crate) reload_activated: Option<Activated>,
    /// The install token `EngineHost::unit` minted for the add that created this plate. A plate is
    /// created per add, so a reused id has a new token: the rig record never carries a fact of one
    /// unit over to the next under the same id (`lf_rig::PlateView::added_by`).
    pub(crate) added_by: InstallToken,
    /// Set at the plate's first applied install, in both places an install applies (`on_seat`'s
    /// `Applied`, a rebuild's settle), and never cleared: a reload, a rebuild, a withdrawal or a park
    /// keeps it. The rig record holds a unit only once it applied (`lf_rig::project`, D4).
    pub(crate) applied: bool,
}

impl Plate {
    fn to_unit_state(&self) -> UnitState {
        UnitState {
            id: self.id,
            kind: self.kind,
            plugin: self.plugin.clone(),
            params: self.params.clone().map(WireParams),
            bypass: self.bypass,
            source: self.source,
            name: self.name.clone(),
            latency: self.latency,
            state: self.state,
            error: self.error.clone(),
            parked: self.parked,
            held: self.held,
            reloading: self.reloading,
        }
    }

    fn is_failed_add(&self) -> bool {
        self.state == PlateState::Failed && !self.parked && !self.seated_in_engine && self.reseating_since.is_none()
    }
}

/// What a pending port message represents so its `Event::Seat` (or rebuild handoff) settles it cleanly.
#[derive(Clone, Debug)]
pub(crate) enum PendingKind {
    Install {
        token: InstallToken,
        is_reload: bool,
        replaces: Option<UnitId>,
        at: usize,
        source: bool,
    },
    Remove {
        owner_token: Option<InstallToken>,
    },
    Withdraw,
    Move {
        to_plate: usize,
        to_engine: usize,
    },
}

/// One in-flight port message in push order for a slot.
#[derive(Clone, Debug)]
pub(crate) struct PendingOp {
    pub(crate) op: Op,
    pub(crate) unit: UnitId,
    pub(crate) seat_op: SeatOp,
    pub(crate) state: OpState,
    pub(crate) kind: PendingKind,
}

#[derive(Default)]
struct SlotModel {
    plates: Vec<Plate>,
    pending: VecDeque<PendingOp>,
}

impl SlotModel {
    fn find(&self, id: UnitId) -> Option<usize> {
        self.plates.iter().position(|p| p.id == id)
    }

    fn held(&self) -> bool {
        self.pending.iter().any(|p| p.state == OpState::Held)
    }

    fn chain_held(&self) -> bool {
        self.pending
            .iter()
            .any(|p| p.state == OpState::Held && self.find(p.unit).is_some_and(|i| !self.plates[i].legacy))
    }

    fn retarget_pending_remove(&mut self, id: UnitId, new_owner: InstallToken) -> bool {
        let mut updated = false;
        for p in &mut self.pending {
            if p.unit == id {
                if let PendingKind::Remove { owner_token } = &mut p.kind {
                    *owner_token = Some(new_owner);
                    updated = true;
                }
            }
        }
        updated
    }

    /// Projected ordered list of unit ids that will hold seats in the current engine once every
    /// currently queued port message in `self.pending` has executed.
    fn projected_engine_ids(&self) -> Vec<(UnitId, bool)> {
        let mut list: Vec<(UnitId, bool)> = self
            .plates
            .iter()
            .filter(|p| p.seated_in_engine)
            .map(|p| (p.id, p.source))
            .collect();
        for p in &self.pending {
            match &p.kind {
                PendingKind::Install { replaces, at, source, .. } => {
                    if let Some(old_id) = *replaces {
                        if let Some(pos) = list.iter().position(|&(x, _)| x == old_id) {
                            list[pos] = (p.unit, *source);
                            continue;
                        }
                    }
                    if let Some(pos) = list.iter().position(|&(x, _)| x == p.unit) {
                        list[pos].1 = *source;
                    } else if *source {
                        list.insert(0, (p.unit, true));
                    } else {
                        let idx = (*at).min(list.len());
                        list.insert(idx, (p.unit, false));
                    }
                }
                PendingKind::Remove { .. } => {
                    list.retain(|&(x, _)| x != p.unit);
                }
                PendingKind::Withdraw => {}
                PendingKind::Move { to_engine, .. } => {
                    if let Some(pos) = list.iter().position(|&(x, _)| x == p.unit) {
                        let item = list.remove(pos);
                        let idx = (*to_engine).min(list.len());
                        list.insert(idx, item);
                    }
                }
            }
        }
        list
    }
}

/// The model: per slot, its plates in plate order and its pending ops, and the revision `chains`
/// carries. P2 builds it into `Core`.
///
/// - Lock order: ports → settings → ends → model; nothing is taken under the model. `drain` (`mod.rs`)
///   gains the model and projects `Seat`, `Chain` and `UnitParam` there.
/// - Per slot the pending ops are kept in push order; a `Seat` pops the head (`Waiting` marks it held,
///   its second answer settles it). A `Seat` whose unit or op differs from the head is a counted
///   protocol error, logged.
/// - From `begin_rebuild` on it ignores the old generation's events (its eviction `Chain` included).
/// - Legacy rows are in its index arithmetic and never in `chains`.
pub(crate) struct Model {
    pub(crate) generation: u64,
    pub(crate) rev: u64,
    next_op: u64,
    slots: [SlotModel; SLOT_COUNT],
    /// Settled outcomes for ops with no live row (a `Remove` whose owner already released, moves, or rowless test ops).
    settled_unowned: VecDeque<(u64, OpState, Instant)>,
    cached_rev: u64,
    cached_chains: Arc<[ChainState; SLOT_COUNT]>,
    /// The last snapshot of the generation before `generation`, frozen as the model advanced: the feed
    /// gets it while the swap has not yet published the new generation ([`Model::snapshot_for`]).
    outgoing: ChainRev,
    /// Controllable test clock for `reseating_since` and `expire_reseats`.
    #[cfg(test)]
    clock: Option<Instant>,
}

impl Model {
    pub(crate) fn new() -> Model {
        let empty = Arc::new(std::array::from_fn(|_| ChainState {
            units: Vec::new(),
            latency: 0,
            held: false,
        }));
        Model {
            generation: 0,
            rev: 0,
            next_op: 1,
            slots: std::array::from_fn(|_| SlotModel::default()),
            settled_unowned: VecDeque::new(),
            cached_rev: 0,
            outgoing: ChainRev { generation: 0, rev: 0, chains: empty.clone() },
            cached_chains: empty,
            #[cfg(test)]
            clock: None,
        }
    }

    fn now(&self, wall: Instant) -> Instant {
        #[cfg(test)]
        if let Some(t) = self.clock {
            return t;
        }
        wall
    }

    #[cfg(test)]
    pub(crate) fn set_clock(&mut self, now: Instant) {
        self.clock = Some(now);
    }

    #[cfg(test)]
    pub(crate) fn advance_clock(&mut self, by: Duration) {
        let base = self.clock.unwrap_or_else(Instant::now);
        self.clock = Some(base + by);
    }

    fn bump_rev(&mut self) {
        self.rev += 1;
    }

    fn record_settled(&mut self, table: &mut Table, op: Op, state: OpState) {
        if !table.record_settled(op, state) {
            self.record_unowned_settled(op.n(), state);
        }
    }

    pub(crate) fn record_unowned_settled(&mut self, op_n: u64, state: OpState) {
        let now = self.now(Instant::now());
        if let Some((_, existing, at)) = self.settled_unowned.iter_mut().find(|(n, _, _)| *n == op_n) {
            *existing = state;
            *at = now;
            return;
        }
        while self.settled_unowned.len() >= MAX_UNOWNED_SETTLED_OPS {
            let Some(&(_, _, oldest)) = self.settled_unowned.front() else { break };
            if now.saturating_duration_since(oldest) < UNOWNED_KEEP {
                break;
            }
            self.settled_unowned.pop_front();
        }
        self.settled_unowned.push_back((op_n, state, now));
    }

    pub(crate) fn mint_op(&mut self, install: InstallToken, generation: u64, slot: usize) -> Op {
        let n = self.next_op;
        self.next_op += 1;
        Op::new(install, n, generation, slot as u8)
    }

    pub(crate) fn push_pending(&mut self, slot: usize, pending: PendingOp) {
        self.slots[slot].pending.push_back(pending);
    }

    pub(crate) fn slot_held(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(SlotModel::held)
    }

    pub(crate) fn poll(&self, table: &Table, op: Op) -> OpState {
        if let Some(state) = table.settled(op) {
            return state;
        }
        if let Some(&(_, state, _)) = self.settled_unowned.iter().rev().find(|&&(n, _, _)| n == op.n()) {
            return state;
        }
        let slot = usize::from(op.slot());
        if let Some(pending) = self.slots.get(slot).and_then(|s| s.pending.iter().find(|p| p.op == op)) {
            return pending.state;
        }
        if op.generation() < self.generation {
            OpState::Rebuilt
        } else {
            OpState::Pending
        }
    }

    pub(crate) fn snapshot(&mut self) -> ChainRev {
        if self.cached_rev != self.rev {
            let chains: [ChainState; SLOT_COUNT] = std::array::from_fn(|s| {
                let slot = &self.slots[s];
                let mut units = Vec::new();
                let mut latency: Frame = 0;
                for plate in &slot.plates {
                    if plate.legacy {
                        continue;
                    }
                    if plate.seated_in_engine {
                        latency += plate.latency;
                    }
                    units.push(plate.to_unit_state());
                }
                ChainState { units, latency, held: slot.chain_held() }
            });
            self.cached_chains = Arc::new(chains);
            self.cached_rev = self.rev;
        }
        ChainRev { generation: self.generation, rev: self.rev, chains: self.cached_chains.clone() }
    }

    /// The snapshot for a drain that read engine generation `gen`: between the model's advance and the
    /// swap's publish of `gen + 1` (under `settings` + `ends`), the old generation's last snapshot, so
    /// a drain never pairs the new chains with the old engine's generation and overview.
    pub(crate) fn snapshot_for(&mut self, gen: u64) -> ChainRev {
        if gen != self.generation && gen == self.outgoing.generation {
            return self.outgoing.clone();
        }
        self.snapshot()
    }

    /// Move to generation `next` (from then on the old generation's events are ignored), freezing the
    /// old generation's last snapshot for the feed until the swap publishes `next`.
    pub(crate) fn advance(&mut self, next: u64) {
        self.outgoing = self.snapshot();
        self.generation = next;
    }

    pub(crate) fn chain_told(&mut self, rev: u64, seq: u64) {
        for slot in &mut self.slots {
            for plate in &mut slot.plates {
                if !plate.legacy && plate.added_rev <= rev && plate.told_seq.is_none() {
                    plate.told_seq = Some(seq);
                }
            }
        }
    }

    /// The seq of the first feed frame that told `id`'s plate (`EngineHost::told`).
    pub(crate) fn told(&self, id: UnitId) -> Option<u64> {
        self.find_plate(id).and_then(|(_, p)| p.told_seq)
    }

    pub(crate) fn find_plate(&self, id: UnitId) -> Option<(usize, &Plate)> {
        for (s, slot) in self.slots.iter().enumerate() {
            if let Some(p) = slot.plates.iter().find(|p| p.id == id) {
                return Some((s, p));
            }
        }
        None
    }

    pub(crate) fn find_plate_mut(&mut self, id: UnitId) -> Option<(usize, &mut Plate)> {
        for (s, slot) in self.slots.iter_mut().enumerate() {
            if let Some(p) = slot.plates.iter_mut().find(|p| p.id == id) {
                return Some((s, p));
            }
        }
        None
    }

    pub(crate) fn has_live_plate(&self, id: UnitId) -> bool {
        self.find_plate(id).is_some_and(|(_, p)| !p.is_failed_add())
    }

    pub(crate) fn has_non_legacy_plate(&self) -> bool {
        self.slots.iter().any(|s| s.plates.iter().any(|p| !p.legacy))
    }

    pub(crate) fn has_legacy_plate(&self) -> bool {
        self.slots.iter().any(|s| s.plates.iter().any(|p| p.legacy))
    }

    /// Number of occupied seats/reservations in `slot` (excluding unseated failed-add plates).
    pub(crate) fn active_seat_count(&self, slot: usize) -> usize {
        self.slots[slot].plates.iter().filter(|p| !p.is_failed_add()).count()
    }

    /// Whether `slot` currently has a head source (seated or pending install, not pending remove).
    pub(crate) fn head_source_id(&self, slot: usize, exclude: Option<UnitId>) -> Option<UnitId> {
        let projected = self.slots[slot].projected_engine_ids();
        projected
            .first()
            .copied()
            .filter(|&(id, source)| source && exclude != Some(id))
            .map(|(id, _)| id)
    }

    /// A source of `slot` newer than `id` (by its plate's `added_rev`): `Some(true)` when one is seated
    /// in this engine (its install applied as a source), `Some(false)` when one's install is still in
    /// the queue, unanswered or held, whatever is queued behind it. `None`: `id` may go to the head.
    pub(crate) fn newer_source(&self, slot: usize, id: UnitId) -> Option<bool> {
        let sm = &self.slots[slot];
        let rev = sm.plates[sm.find(id)?].added_rev;
        let newer = |other: UnitId| other != id && sm.find(other).is_some_and(|i| sm.plates[i].added_rev > rev);
        if sm.plates.iter().any(|p| p.seated_in_engine && p.seated_source == Some(true) && !p.legacy && newer(p.id)) {
            return Some(true);
        }
        sm.pending
            .iter()
            .any(|p| matches!(p.kind, PendingKind::Install { source: true, is_reload: false, .. }) && newer(p.unit))
            .then_some(false)
    }

    /// Whether a plate that holds or awaits a seat stands ahead of `id` in `slot`'s plate order: one
    /// seated in this engine, or one whose owner still holds its unit (out after a rebuild, parked or
    /// reseating, which a rebuild leaves `failed` with no seat). A failed add (its id freed) holds none,
    /// nor does one whose removal is queued: it is gone before an install pushed now is judged.
    pub(crate) fn live_plate_ahead(&self, slot: usize, id: UnitId, table: &Table) -> bool {
        let sm = &self.slots[slot];
        let leaving = |p: &Plate| {
            sm.pending.iter().any(|q| q.unit == p.id && matches!(q.kind, PendingKind::Remove { .. }))
        };
        sm.find(id).is_some_and(|pos| {
            sm.plates[..pos]
                .iter()
                .any(|p| (p.seated_in_engine || table.has_active_row(p.id)) && !leaving(p))
        })
    }

    /// Whether an install queued in `slot` takes `id`'s seat (`replaces: Some(id)`, another id's: a
    /// newer source taking the head) and the engine has not answered it yet.
    pub(crate) fn seat_taken_by_queued_install(&self, slot: usize, id: UnitId) -> bool {
        self.slots[slot].pending.iter().any(|p| {
            p.unit != id && matches!(p.kind, PendingKind::Install { replaces: Some(r), is_reload: false, .. } if r == id)
        })
    }

    /// The messages pushed on `slot`'s port whose answer the model has not taken yet: never fewer than
    /// the port holds ([`super::unit_host::Table::port_claimed_ahead`]).
    pub(crate) fn queued(&self, slot: usize) -> usize {
        self.slots[slot].pending.len()
    }

    /// Whether `slot` has any head source plate (even loading/reseating) for capacity check.
    pub(crate) fn has_head_source_plate(&self, slot: usize) -> bool {
        self.slots[slot]
            .plates
            .iter()
            .find(|p| !p.is_failed_add())
            .is_some_and(|p| p.source)
    }

    /// A host add's match (D9), under `state` before its mint: `Some(Ok(token))` when a live plate of
    /// `slot` already is the unit (id `id`, the kind and plugin identity `spec` names, applied, neither a
    /// failed add nor `Leaving`), with that plate's `added_by`; `Some(Err(why))` when another plate holds
    /// the id (the other slot, another kind or plugin, a `Leaving` one, or this unit's that has not
    /// applied yet: its own add may still fail, and adding beside it would double it); `None` when no
    /// plate holds it (a failed add's does not: the add replaces it).
    pub(crate) fn host_match(&self, slot: usize, id: UnitId, spec: &Spec) -> Option<Result<InstallToken, String>> {
        let (s, plate) = self.find_plate(id).filter(|(_, p)| !p.is_failed_add())?;
        let same = match spec {
            Spec::Plugin(plugin) => plate.kind == UnitKind::Plugin && plate.plugin.as_ref() == Some(plugin),
            Spec::Builtin { kind, .. } => plate.kind == *kind && plate.plugin.is_none(),
            Spec::Legacy => false,
        };
        if s == slot && !plate.legacy && same && plate.applied && plate.state != PlateState::Leaving {
            return Some(Ok(plate.added_by));
        }
        Some(Err(format!("unit id {id} is held by another unit, or by this one still loading or leaving")))
    }

    /// A host add's plate index (D9), under `state`: right after the nearest of `place`'s record units
    /// (nearest last) that is a plate of `slot` now, else at the head: after the slot's source, when its
    /// first plate is one (an effect never goes ahead of it). `End` is the slot's end. Counted as the
    /// plates will stand once `add_plate` has dropped a failed add's plate under `id`.
    pub(crate) fn host_place(&self, slot: usize, id: UnitId, place: &lf_rig::Place) -> usize {
        let plates: Vec<&Plate> = self.slots[slot].plates.iter().filter(|p| !(p.id == id && p.is_failed_add())).collect();
        let live = |p: &&Plate| !p.is_failed_add();
        match place {
            lf_rig::Place::End => plates.len(),
            lf_rig::Place::After(before) => before
                .iter()
                .rev()
                .find_map(|anchor| plates.iter().position(|p| p.id == *anchor && live(p)))
                .map(|i| i + 1)
                .unwrap_or_else(|| match plates.iter().position(live) {
                    Some(first) if plates[first].source => first + 1,
                    _ => 0,
                }),
        }
    }

    /// Add a new plate at plate index `at`, made by the add whose row `token` names. If an un-removed
    /// failed-add plate already carries `id`, it is cleared first (its id was freed by `failed()`).
    pub(crate) fn add_plate(&mut self, slot: usize, id: UnitId, spec: &Spec, at: usize, bypassed: bool, token: InstallToken) {
        // Clear any leftover failed-add plate with the same freed id.
        for s in &mut self.slots {
            s.plates.retain(|p| !(p.id == id && p.is_failed_add()));
        }
        let legacy = matches!(spec, Spec::Legacy);
        let (kind, plugin, params, source) = match spec {
            Spec::Plugin(p) => (UnitKind::Plugin, Some(p.clone()), None, false),
            Spec::Builtin { kind, params } => (*kind, None, Some(params.clone()), *kind == UnitKind::Synth),
            Spec::Legacy => (UnitKind::Plugin, None, None, false),
        };
        if !legacy {
            self.bump_rev();
        }
        let added_rev = self.rev;
        let idx = at.min(self.slots[slot].plates.len());
        self.slots[slot].plates.insert(
            idx,
            Plate {
                id,
                legacy,
                kind,
                plugin,
                params,
                bypass: bypassed,
                source,
                name: String::new(),
                latency: 0,
                state: PlateState::Loading,
                error: None,
                parked: false,
                held: false,
                reloading: false,
                seated_in_engine: false,
                withdrawn: false,
                reseating_since: None,
                added_rev,
                told_seq: None,
                seated_source: None,
                reload_plugin: None,
                reload_activated: None,
                added_by: token,
                applied: false,
            },
        );
    }

    /// The chains as the rig record reads them (`lf_rig::RigView`, D3): both slots' plates in chain
    /// order, legacy rows left out, copied with the revision the feed's `chains` carry. The caller takes
    /// it under `state` and releases the lock before it projects anything (`engine_io/rig`).
    pub(crate) fn rig_view(&self) -> lf_rig::RigView {
        let plate = |p: &Plate| lf_rig::PlateView {
            id: p.id,
            added_by: p.added_by,
            kind: p.kind,
            plugin: p.plugin.as_ref().map(|r| lf_rig::PluginRefView { format: r.format.clone(), path: r.path.clone(), id: r.id.clone() }),
            name: p.name.clone(),
            params: if p.kind == UnitKind::Plugin { None } else { p.params.clone() },
            bypass: p.bypass,
            source: p.source,
            state: match p.state {
                PlateState::Loading => lf_rig::PlateState::Loading,
                PlateState::Waiting => lf_rig::PlateState::Waiting,
                PlateState::Seated => lf_rig::PlateState::Seated,
                PlateState::Leaving => lf_rig::PlateState::Leaving,
                PlateState::Failed => lf_rig::PlateState::Failed,
            },
            applied: p.applied,
            failed_add: p.is_failed_add(),
        };
        let slot = |s: usize| self.slots.get(s).map(|m| m.plates.iter().filter(|p| !p.legacy).map(plate).collect()).unwrap_or_default();
        lf_rig::RigView { rev: self.rev, slots: [slot(0), slot(1)] }
    }

    pub(crate) fn remove_plate(&mut self, slot: usize, id: UnitId) -> bool {
        let Some(pos) = self.slots[slot].find(id) else { return false };
        let legacy = self.slots[slot].plates[pos].legacy;
        self.slots[slot].plates.remove(pos);
        if !legacy {
            self.bump_rev();
        }
        true
    }

    pub(crate) fn set_activated(&mut self, slot: usize, id: UnitId, is_reload: bool, a: Activated) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        let legacy = plate.legacy;
        if is_reload {
            plate.reload_activated = Some(a);
        } else {
            plate.name = a.name;
            plate.source = a.source;
            plate.latency = a.latency;
            if !legacy {
                self.bump_rev();
            }
        }
    }

    pub(crate) fn set_committed(&mut self, slot: usize, id: UnitId, params: Params) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        let legacy = plate.legacy;
        plate.params = Some(params);
        if !legacy {
            self.bump_rev();
        }
    }

    pub(crate) fn mark_reloading(&mut self, slot: usize, id: UnitId, plugin: PluginRef) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        plate.reloading = true;
        plate.state = PlateState::Loading;
        plate.reload_plugin = Some(plugin);
        plate.reload_activated = None;
        if !plate.legacy {
            self.bump_rev();
        }
    }

    pub(crate) fn cancel_reloading(&mut self, slot: usize, id: UnitId) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        if plate.reloading {
            plate.reloading = false;
            plate.held = false;
            plate.reload_plugin = None;
            plate.reload_activated = None;
            if plate.seated_in_engine && plate.state != PlateState::Leaving {
                plate.state = if plate.parked { PlateState::Failed } else { PlateState::Seated };
            }
            if !plate.legacy {
                self.bump_rev();
            }
        }
    }

    pub(crate) fn mark_parked(&mut self, slot: usize, id: UnitId, why: String) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        plate.state = PlateState::Failed;
        plate.parked = true;
        plate.error = Some(why);
        plate.held = false;
        plate.reseating_since = None;
        if !plate.legacy {
            self.bump_rev();
        }
    }

    pub(crate) fn mark_failed(&mut self, slot: usize, id: UnitId, why: String) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        plate.state = PlateState::Failed;
        plate.parked = false;
        plate.seated_in_engine = false;
        plate.withdrawn = false;
        plate.held = false;
        plate.reloading = false;
        plate.reseating_since = None;
        plate.error = Some(why);
        if !plate.legacy {
            self.bump_rev();
        }
    }

    pub(crate) fn mark_leaving(&mut self, slot: usize, id: UnitId) {
        let Some(pos) = self.slots[slot].find(id) else { return };
        let plate = &mut self.slots[slot].plates[pos];
        plate.state = PlateState::Leaving;
        if !plate.legacy {
            self.bump_rev();
        }
    }

    /// Compute the engine index at which `id` should be inserted in `slot`, counting every plate
    /// ahead of `id` in plate order that will occupy a seat in the engine when this install runs.
    pub(crate) fn engine_index_for_insert(&self, slot: usize, id: UnitId) -> usize {
        let projected: BTreeSet<UnitId> = self.slots[slot].projected_engine_ids().into_iter().map(|(x, _)| x).collect();
        let mut idx = 0;
        for plate in &self.slots[slot].plates {
            if plate.id == id {
                break;
            }
            if projected.contains(&plate.id) {
                idx += 1;
            }
        }
        idx
    }

    /// Whether `id` currently has a pending install in `slot`'s queue.
    pub(crate) fn has_pending_install(&self, slot: usize, id: UnitId) -> bool {
        self.slots[slot]
            .pending
            .iter()
            .any(|p| p.unit == id && matches!(p.kind, PendingKind::Install { .. }))
    }

    /// The first non-refused pending `Install` in `slot`'s engine order whose `replaces == Some(id)`:
    /// returns `(install_token, is_reload)`. Used when `Left::Replaced` is reaped before `Event::Seat`
    /// drains to tell whether a same-id reload candidate or a different-id install replaced `id`.
    pub(crate) fn first_pending_replacer(
        &self,
        slot: usize,
        id: UnitId,
        table: &Table,
    ) -> Option<(InstallToken, bool)> {
        self.slots[slot].pending.iter().find_map(|p| match p.kind {
            PendingKind::Install { token, is_reload, replaces: Some(old_id), .. }
                if old_id == id && !table.is_install_refused(p.op.n()) =>
            {
                Some((token, is_reload))
            }
            _ => None,
        })
    }

    /// Whether `id` currently has a pending remove in `slot`'s queue; returns its op.
    pub(crate) fn pending_remove_op(&self, slot: usize, id: UnitId) -> Option<Op> {
        self.slots[slot]
            .pending
            .iter()
            .find(|p| p.unit == id && matches!(p.kind, PendingKind::Remove { .. }))
            .map(|p| p.op)
    }

    /// Whether `id` currently has a pending remove in any slot's queue.
    pub(crate) fn has_pending_remove(&self, id: UnitId) -> bool {
        self.slots
            .iter()
            .any(|s| s.pending.iter().any(|p| p.unit == id && matches!(p.kind, PendingKind::Remove { .. })))
    }

    /// Whether `id` currently has a pending withdraw in `slot`'s queue.
    pub(crate) fn has_pending_withdraw(&self, slot: usize, id: UnitId) -> bool {
        self.slots[slot]
            .pending
            .iter()
            .any(|p| p.unit == id && matches!(p.kind, PendingKind::Withdraw))
    }

    /// Translate plate index `to_plate` for moving `id` within `slot` into the engine index when
    /// the pushed `Move` executes (accounting for failed plates and pending removals/installs ahead).
    pub(crate) fn translate_move(&self, slot: usize, id: UnitId, to_plate: usize) -> Result<(usize, usize), String> {
        let projected: Vec<(UnitId, bool)> = self.slots[slot].projected_engine_ids();
        if !projected.iter().any(|&(x, _)| x == id) {
            return Err(format!("unit {id} is not seated in slot {slot}"));
        }
        let projected_set: BTreeSet<UnitId> = projected.iter().map(|&(x, _)| x).collect();
        let without: Vec<UnitId> = self.slots[slot].plates.iter().map(|p| p.id).filter(|&x| x != id).collect();
        let clamped_plate = to_plate.min(without.len());
        let engine_to = without[..clamped_plate]
            .iter()
            .filter(|x| projected_set.contains(x))
            .count();
        Ok((clamped_plate, engine_to))
    }

    /// Project an `Event::Seat` into the model and table.
    pub(crate) fn on_seat(
        &mut self,
        gen: u64,
        slot: u8,
        unit: UnitId,
        op: SeatOp,
        outcome: SeatOutcome,
        table: &mut Table,
        protocol_errors: &AtomicU64,
    ) {
        if gen != self.generation {
            return;
        }
        let s = usize::from(slot);
        if s >= SLOT_COUNT {
            protocol_errors.fetch_add(1, Relaxed);
            log::error!("[engine_io] Event::Seat for out-of-range slot {slot}");
            return;
        }
        let matches_head = self.slots[s]
            .pending
            .front()
            .is_some_and(|head| head.unit == unit && head.seat_op == op);
        if !matches_head {
            protocol_errors.fetch_add(1, Relaxed);
            log::error!(
                "[engine_io] slot {s}: protocol error: Event::Seat ({op:?} unit {unit} {outcome:?}) does not match head {:?}",
                self.slots[s].pending.front().map(|h| (h.seat_op, h.unit))
            );
            return;
        }
        match outcome {
            SeatOutcome::Waiting => {
                let mut rev_bump = false;
                if let Some(head) = self.slots[s].pending.front_mut() {
                    head.state = OpState::Held;
                    let is_install = matches!(head.kind, PendingKind::Install { .. });
                    if let Some(pos) = self.slots[s].find(unit) {
                        let plate = &mut self.slots[s].plates[pos];
                        plate.held = true;
                        if is_install {
                            plate.state = PlateState::Waiting;
                        }
                        rev_bump = !plate.legacy;
                    }
                }
                if rev_bump {
                    self.bump_rev();
                }
            }
            SeatOutcome::Applied => {
                let head = self.slots[s].pending.pop_front().expect("checked head");
                self.record_settled(table, head.op, OpState::Applied);
                let mut rev_bump = false;
                match head.kind {
                    PendingKind::Install { token, is_reload, replaces, source, .. } => {
                        table.mark_seated(token, gen);
                        if is_reload {
                            table.promote_reload(token);
                            if self.slots[s].retarget_pending_remove(unit, token) {
                                table.transfer_remove_to_promoted(token, gen);
                            }
                        }
                        table.clear_install_receipt(token, head.op.n());
                        if let Some(old_id) = replaces {
                            if old_id != unit {
                                if let Some(old_pos) = self.slots[s].find(old_id) {
                                    rev_bump |= !self.slots[s].plates[old_pos].legacy;
                                    self.slots[s].plates.remove(old_pos);
                                }
                                table.finish_replaced(old_id);
                            }
                        }
                        if let Some(pos) = self.slots[s].find(unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            if is_reload {
                                if let Some(p) = plate.reload_plugin.take() {
                                    plate.plugin = Some(p);
                                }
                                if let Some(a) = plate.reload_activated.take() {
                                    plate.name = a.name;
                                    plate.source = a.source;
                                    plate.latency = a.latency;
                                }
                                plate.reloading = false;
                            }
                            if plate.state != PlateState::Leaving {
                                plate.state = PlateState::Seated;
                            }
                            plate.seated_in_engine = true;
                            plate.applied = true;
                            plate.seated_source = Some(plate.source);
                            plate.withdrawn = false;
                            plate.parked = false;
                            plate.held = false;
                            plate.error = None;
                            plate.reseating_since = None;
                            rev_bump |= !plate.legacy;
                        }
                        if source && !is_reload {
                            for old_id in self.take_older_sources_out_of_engine(s, unit, table) {
                                table.finish_replaced(old_id);
                                rev_bump = true;
                            }
                        }
                    }
                    PendingKind::Remove { owner_token } => {
                        if let Some(pos) = self.slots[s].find(unit) {
                            rev_bump |= !self.slots[s].plates[pos].legacy;
                            self.slots[s].plates.remove(pos);
                        }
                        if head.op.install() != 0 {
                            table.note_remove_settled(head.op.install(), head.op.n(), OpState::Applied);
                        }
                        if let Some(tok) = owner_token {
                            table.finish_remove(tok);
                        }
                    }
                    PendingKind::Withdraw => {
                        let has_pending_install = self.has_pending_install(s, unit);
                        let now = self.now(Instant::now());
                        if let Some(pos) = self.slots[s].find(unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            plate.withdrawn = true;
                            plate.held = false;
                            // Arm the withdrawal deadline only while this withdrawal still awaits a
                            // resubmit (not if a timely resubmit is already pending/held, or parked/leaving).
                            if !has_pending_install && !plate.parked && plate.state != PlateState::Leaving {
                                plate.reseating_since = Some(now);
                            }
                            rev_bump |= !plate.legacy;
                        }
                    }
                    PendingKind::Move { to_plate, .. } => {
                        if let Some(pos) = self.slots[s].find(unit) {
                            let mut plate = self.slots[s].plates.remove(pos);
                            plate.held = false;
                            rev_bump |= !plate.legacy;
                            let idx = to_plate.min(self.slots[s].plates.len());
                            self.slots[s].plates.insert(idx, plate);
                        }
                    }
                }
                if rev_bump {
                    self.bump_rev();
                }
            }
            SeatOutcome::Refused(why) => {
                let head = self.slots[s].pending.pop_front().expect("checked head");
                let mut rev_bump = false;
                match head.kind {
                    PendingKind::Install { token, is_reload, .. } => {
                        self.record_settled(table, head.op, OpState::Refused(why));
                        table.clear_install_receipt(token, head.op.n());
                        if is_reload {
                            if table.fail_reload(token) {
                                if let Some(pos) = self.slots[s].find(unit) {
                                    let plate = &mut self.slots[s].plates[pos];
                                    plate.reloading = false;
                                    plate.held = false;
                                    plate.reload_plugin = None;
                                    plate.reload_activated = None;
                                    if plate.seated_in_engine && plate.state != PlateState::Leaving {
                                        plate.state = if plate.parked { PlateState::Failed } else { PlateState::Seated };
                                    }
                                    rev_bump |= !plate.legacy;
                                }
                            }
                        } else if let Some(pos) = self.slots[s].find(unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            plate.held = false;
                            rev_bump |= !plate.legacy;
                        }
                    }
                    PendingKind::Remove { owner_token } if why == lf_engine::SeatRefusal::Unknown => {
                        // No seat with this id exists in the engine: nothing left to remove. Settle
                        // as done (`Applied`) so the plate goes and the owner gets its terminal.
                        self.record_settled(table, head.op, OpState::Applied);
                        if let Some(pos) = self.slots[s].find(unit) {
                            rev_bump |= !self.slots[s].plates[pos].legacy;
                            self.slots[s].plates.remove(pos);
                        }
                        if head.op.install() != 0 {
                            table.note_remove_settled(head.op.install(), head.op.n(), OpState::Applied);
                        }
                        if let Some(tok) = owner_token {
                            table.finish_remove(tok);
                        }
                    }
                    PendingKind::Remove { owner_token } => {
                        self.record_settled(table, head.op, OpState::Refused(why));
                        if head.op.install() != 0 {
                            table.note_remove_settled(head.op.install(), head.op.n(), OpState::Refused(why));
                        }
                        if let Some(tok) = owner_token {
                            table.cancel_remove(tok);
                        }
                        if let Some(pos) = self.slots[s].find(unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            plate.held = false;
                            if plate.state == PlateState::Leaving {
                                plate.state = if plate.parked {
                                    PlateState::Failed
                                } else if plate.seated_in_engine {
                                    PlateState::Seated
                                } else {
                                    PlateState::Failed
                                };
                            }
                            rev_bump |= !plate.legacy;
                        }
                    }
                    PendingKind::Withdraw | PendingKind::Move { .. } => {
                        self.record_settled(table, head.op, OpState::Refused(why));
                        if let Some(pos) = self.slots[s].find(unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            plate.held = false;
                            rev_bump |= !plate.legacy;
                        }
                    }
                }
                if rev_bump {
                    self.bump_rev();
                }
            }
        }
    }

    /// Source `unit`'s install applied at the head of `slot` (answered by its `Seat`, or found applied
    /// by a rebuild's handoff): take the plates of every older source of that slot whose seat is out of
    /// this engine (evicted and not back yet, or parked after a failed re-activation) and return their
    /// ids for the caller to end as a replacement would (`Table::finish_replaced`: `Back::Dismissed`,
    /// the tone retires). The engine holds no seat of theirs for the install to name in `replaces`, so
    /// without this each kept its owner and tone until its retry found the newer head. Nothing ends one
    /// earlier: a newer source the engine refuses costs the older one nothing (its retry meanwhile
    /// answers `Behind`, `UnitHost::submit`, so it never has an install queued behind the newer one's).
    /// One with an op still queued is leaving or seated and is left to that op; a failed add (its id
    /// freed) is the player's to remove. Each is judged by the role it last seated in (its reported one
    /// if it never seated): an effect parked for coming back a source was never the slot's source, and
    /// a source parked for coming back an effect (refused, say, for its latency) still was.
    fn take_older_sources_out_of_engine(&mut self, slot: usize, unit: UnitId, table: &Table) -> Vec<UnitId> {
        let older = self.older_sources_out_of_engine(slot, unit, table);
        self.slots[slot].plates.retain(|p| !older.contains(&p.id));
        older
    }

    /// The older sources of `slot` that source `unit`'s install would take out when it applies at the
    /// head ([`Self::take_older_sources_out_of_engine`]): each one's row live, its seat out of this
    /// engine (a rebuild's eviction it has not reseated from, or parked) and no op of its queued.
    fn older_sources_out_of_engine(&self, slot: usize, unit: UnitId, table: &Table) -> Vec<UnitId> {
        let sm = &self.slots[slot];
        let Some(rev) = sm.find(unit).map(|i| sm.plates[i].added_rev) else { return Vec::new() };
        sm.plates
            .iter()
            .filter(|p| {
                p.seated_source.unwrap_or(p.source)
                    && p.id != unit
                    && !p.legacy
                    && !p.seated_in_engine
                    && p.state != PlateState::Leaving
                    && p.added_rev < rev
                    && table.has_active_row(p.id)
                    && !sm.pending.iter().any(|q| q.unit == p.id)
            })
            .map(|p| p.id)
            .collect()
    }

    /// Whether a plate of `slot` other than `unit` still holds the slot's source role, as a host add's
    /// first install must leave it (D9, `UnitHost::submit`): its row live, not leaving, and a source by
    /// the role its last applied install seated it in (`seated_source`; its reported role if it never
    /// applied). It keeps that role until an install in another role applies or it leaves, whatever is
    /// queued for it meanwhile and wherever its seat is: a reseat as an effect the engine may still
    /// refuse (for its latency, say), a rebuild's eviction it has not reseated from, a park. No
    /// projected seat need show it (`head_source_id` projects the reported role), and a source install
    /// applying while it stands takes it out ([`Self::take_older_sources_out_of_engine`]).
    pub(crate) fn source_role_held(&self, slot: usize, unit: UnitId, table: &Table) -> bool {
        self.slots[slot].plates.iter().any(|p| {
            p.id != unit
                && !p.legacy
                && p.state != PlateState::Leaving
                && p.seated_source.unwrap_or(p.source)
                && table.has_active_row(p.id)
        })
    }

    /// Project an `Event::Chain` (or a snapshot read off `Engine::chain`) into the model.
    pub(crate) fn on_chain(&mut self, gen: u64, slot: u8, seats: &[ChainSeat]) {
        if gen != self.generation {
            return;
        }
        let s = usize::from(slot);
        if s >= SLOT_COUNT {
            return;
        }
        self.apply_chain_seats(s, seats);
    }

    fn apply_chain_seats(&mut self, s: usize, seats: &[ChainSeat]) {
        let mut changed = false;
        let seat_ids: Vec<UnitId> = seats.iter().map(|cs| cs.unit).collect();
        // Update attributes of each seated plate present in `seats`.
        for cs in seats {
            if let Some(pos) = self.slots[s].find(cs.unit) {
                let plate = &mut self.slots[s].plates[pos];
                let new_latency = Frame::from(cs.latency);
                if plate.bypass != cs.bypassed
                    || plate.latency != new_latency
                    || plate.kind != cs.kind
                    || !plate.seated_in_engine
                {
                    plate.bypass = cs.bypassed;
                    plate.latency = new_latency;
                    plate.kind = cs.kind;
                    plate.seated_in_engine = true;
                    if !plate.legacy {
                        changed = true;
                    }
                }
            }
        }
        // Reorder the plates whose ids appear in `seats` to match `seats`'s order, keeping
        // non-engine plates (e.g. failed-add plates or loading plates) in their relative slots.
        if reorder_plates(&mut self.slots[s].plates, &seat_ids) {
            changed = true;
        }
        if changed {
            self.bump_rev();
        }
    }

    /// Project an `Event::UnitParam` (or an unsent param read off `Engine::unsent_unit_params`).
    pub(crate) fn on_unit_param(&mut self, gen: u64, unit: UnitId, param: UnitParam, value: f64) {
        if gen != self.generation {
            return;
        }
        self.apply_unit_param(unit, param, value);
    }

    fn apply_unit_param(&mut self, unit: UnitId, param: UnitParam, value: f64) {
        let Some((_, plate)) = self.find_plate_mut(unit) else { return };
        let legacy = plate.legacy;
        let params = plate.params.get_or_insert_with(Vec::new);
        let changed = if let Some((_, v)) = params.iter_mut().find(|(p, _)| *p == param) {
            let diff = *v != value;
            *v = value;
            diff
        } else {
            params.push((param, value));
            true
        };
        if changed && !legacy {
            self.bump_rev();
        }
    }

    /// Begin a rebuild into generation `next`: fold the old engine's chains and owed param answers
    /// into the model, then move `self.generation` to `next` so every later event from the old
    /// generation (eviction `Chain`s included) is ignored.
    pub(crate) fn begin_rebuild(
        &mut self,
        next: u64,
        old_chains: &[Vec<ChainSeat>; SLOT_COUNT],
        unsent_params: &[(UnitId, UnitParam, f64)],
    ) {
        for (s, seats) in old_chains.iter().enumerate() {
            self.apply_chain_seats(s, seats);
        }
        for &(unit, param, value) in unsent_params {
            self.apply_unit_param(unit, param, value);
        }
        self.advance(next);
    }

    /// Settle every pending op and plate after `old.evict_slots()` returned `returned`.
    pub(crate) fn settle_rebuild(
        &mut self,
        returned: Vec<(usize, Returned)>,
        table: &mut Table,
        now: Instant,
    ) {
        let now = self.now(now);
        // The ports are replaced with the engine: no install waits for room on one any more.
        table.clear_port_claims();
        // Index returned boxes by each row's latest install op for pending-op settling.
        let ret_info: BTreeMap<u64, (Left, bool)> = returned
            .iter()
            .filter_map(|(_, r)| table.install_op(r.token).map(|op_n| (op_n, (r.left, r.bypassed))))
            .collect();

        let mut rev_bump = false;
        let mut removed_owners: BTreeSet<InstallToken> = BTreeSet::new();
        let mut removed_finish: Vec<InstallToken> = Vec::new();
        let mut replaced_other_ids: Vec<UnitId> = Vec::new();
        // 1. Settle every pending op in push order per slot, replaying the engine's seat order
        //    using each op's settled outcome so moves behind refused installs land accurately.
        for s in 0..SLOT_COUNT {
            let mut engine_order: Vec<(UnitId, bool)> = self.slots[s]
                .plates
                .iter()
                .filter(|p| p.seated_in_engine)
                .map(|p| (p.id, p.source))
                .collect();
            while let Some(head) = self.slots[s].pending.pop_front() {
                match head.kind {
                    PendingKind::Install { token, is_reload, replaces, at, source } => {
                        // The boxes this handoff collected, else what was filed for this op before
                        // its `Seat` drained (kept even if the owner already released).
                        let received = table.take_received(head.op.n());
                        table.clear_install_op(token, head.op.n());
                        let left = ret_info.get(&head.op.n()).copied().or(received);
                        match left {
                            Some((Left::Refused(why), _)) => {
                                self.record_settled(table, head.op, OpState::Refused(why));
                                if is_reload && table.fail_reload(token) {
                                    if let Some(pos) = self.slots[s].find(head.unit) {
                                        let plate = &mut self.slots[s].plates[pos];
                                        plate.reloading = false;
                                        plate.held = false;
                                        plate.reload_plugin = None;
                                        plate.reload_activated = None;
                                        if plate.seated_in_engine && plate.state != PlateState::Leaving {
                                            plate.state = if plate.parked { PlateState::Failed } else { PlateState::Seated };
                                        }
                                        rev_bump |= !plate.legacy;
                                    }
                                }
                            }
                            Some((Left::Unsent, _)) | None => {
                                self.record_settled(table, head.op, OpState::Rebuilt);
                            }
                            Some((Left::Evicted | Left::Removed | Left::Withdrawn | Left::Replaced, bypassed)) => {
                                self.record_settled(table, head.op, OpState::Applied);
                                if let Some(old_id) = replaces {
                                    if let Some(pos) = engine_order.iter().position(|&(x, _)| x == old_id) {
                                        engine_order[pos] = (head.unit, source);
                                    } else if let Some(pos) = engine_order.iter().position(|&(x, _)| x == head.unit) {
                                        engine_order[pos].1 = source;
                                    } else if source {
                                        engine_order.insert(0, (head.unit, true));
                                    } else {
                                        let idx = at.min(engine_order.len());
                                        engine_order.insert(idx, (head.unit, false));
                                    }
                                } else if let Some(pos) = engine_order.iter().position(|&(x, _)| x == head.unit) {
                                    engine_order[pos].1 = source;
                                } else if source {
                                    engine_order.insert(0, (head.unit, true));
                                } else {
                                    let idx = at.min(engine_order.len());
                                    engine_order.insert(idx, (head.unit, false));
                                }
                                if is_reload {
                                    table.promote_reload(token);
                                    self.slots[s].retarget_pending_remove(head.unit, token);
                                }
                                if let Some(old_id) = replaces {
                                    if old_id != head.unit {
                                        if let Some(old_pos) = self.slots[s].find(old_id) {
                                            rev_bump |= !self.slots[s].plates[old_pos].legacy;
                                            self.slots[s].plates.remove(old_pos);
                                        }
                                        replaced_other_ids.push(old_id);
                                    }
                                }
                                if let Some(pos) = self.slots[s].find(head.unit) {
                                    let plate = &mut self.slots[s].plates[pos];
                                    if is_reload {
                                        if let Some(p) = plate.reload_plugin.take() {
                                            plate.plugin = Some(p);
                                        }
                                        if let Some(a) = plate.reload_activated.take() {
                                            plate.name = a.name;
                                            plate.source = a.source;
                                            plate.latency = a.latency;
                                        }
                                        plate.reloading = false;
                                    }
                                    plate.bypass = bypassed;
                                    plate.seated_in_engine = true;
                                    plate.applied = true;
                                    plate.seated_source = Some(plate.source);
                                    plate.withdrawn = false;
                                    plate.parked = false;
                                    plate.held = false;
                                    plate.error = None;
                                    rev_bump |= !plate.legacy;
                                }
                                // The old engine applied it (its idle service, before the eviction):
                                // it seated, as an answered install does, and ends what that ends, once
                                // the returned boxes are filed (below).
                                if source && !is_reload {
                                    let older = self.take_older_sources_out_of_engine(s, head.unit, table);
                                    rev_bump |= !older.is_empty();
                                    replaced_other_ids.extend(older);
                                }
                            }
                        }
                    }
                    PendingKind::Remove { owner_token } => {
                        self.record_settled(table, head.op, OpState::Applied);
                        engine_order.retain(|&(x, _)| x != head.unit);
                        if let Some(pos) = self.slots[s].find(head.unit) {
                            rev_bump |= !self.slots[s].plates[pos].legacy;
                            self.slots[s].plates.remove(pos);
                        }
                        if head.op.install() != 0 {
                            table.note_remove_settled(head.op.install(), head.op.n(), OpState::Applied);
                        }
                        if let Some(tok) = owner_token {
                            removed_owners.insert(tok);
                            removed_finish.push(tok);
                        }
                    }
                    PendingKind::Withdraw => {
                        self.record_settled(table, head.op, OpState::Applied);
                    }
                    PendingKind::Move { to_engine, .. } => {
                        if let Some(pos) = engine_order.iter().position(|&(x, _)| x == head.unit) {
                            let to = to_engine.min(engine_order.len() - 1);
                            let role = engine_order.first().is_some_and(|&(_, src)| src);
                            if pos != to && (engine_order[pos].1 || (to == 0 && role)) {
                                self.record_settled(table, head.op, OpState::Refused(lf_engine::SeatRefusal::Placement));
                            } else {
                                let item = engine_order.remove(pos);
                                engine_order.insert(to, item);
                                self.record_settled(table, head.op, OpState::Applied);
                            }
                        } else {
                            self.record_settled(table, head.op, OpState::Refused(lf_engine::SeatRefusal::Unknown));
                        }
                        if let Some(pos) = self.slots[s].find(head.unit) {
                            let plate = &mut self.slots[s].plates[pos];
                            plate.held = false;
                            rev_bump |= !plate.legacy;
                        }
                    }
                }
            }
            let order_ids: Vec<UnitId> = engine_order.iter().map(|&(id, _)| id).collect();
            if reorder_plates(&mut self.slots[s].plates, &order_ids) {
                rev_bump = true;
            }
        }

        // Rule 4: end every reload still in flight (not promoted by step 1).
        for (slot, id) in table.cancel_in_flight_reloads() {
            self.cancel_reloading(slot, id);
        }

        // 2. File every returned box into its row's mailbox (by install token) and record applied
        //    bypass on its plate where the box came back evicted/withdrawn for the active row.
        for (slot, ret) in returned {
            let is_pending_remove = removed_owners.contains(&ret.token);
            let update_bypass = matches!(ret.left, Left::Evicted | Left::Withdrawn)
                && !is_pending_remove
                && !table.is_superseded(ret.token);
            if update_bypass {
                if let Some(pos) = self.slots[slot].find(ret.id) {
                    let plate = &mut self.slots[slot].plates[pos];
                    if plate.bypass != ret.bypassed {
                        plate.bypass = ret.bypassed;
                        rev_bump |= !plate.legacy;
                    }
                }
            }
            table.file_returned_rebuild(slot, ret, is_pending_remove);
        }

        // Settle any pending removal or different-id replacement whose box was already out of the
        // engine before this handoff (and therefore did not come back in `returned` above).
        for tok in removed_finish {
            table.finish_remove(tok);
        }
        for old_id in replaced_other_ids {
            table.finish_replaced(old_id);
        }

        // 3. Transition every remaining plate to the new (empty) engine.
        for s in 0..SLOT_COUNT {
            for plate in &mut self.slots[s].plates {
                let was_seated = plate.seated_in_engine;
                plate.seated_in_engine = false;
                plate.held = false;
                if plate.parked {
                    // A withdrawn seat with no row re-activating (parked) becomes `failed`, no seat.
                    plate.parked = false;
                    plate.withdrawn = false;
                    plate.state = PlateState::Failed;
                    plate.reseating_since = None;
                    rev_bump |= !plate.legacy;
                } else if plate.is_failed_add() {
                    // Stays failed, no seat.
                } else if was_seated || plate.reseating_since.is_some() || table.has_active_row(plate.id) {
                    plate.withdrawn = false;
                    if plate.state == PlateState::Waiting {
                        plate.state = PlateState::Loading;
                    }
                    // Keep the earliest deadline if it was already reseating across a second rebuild?
                    // Fresh swap gives each unit `GATE_DEADLINE` after this swap.
                    plate.reseating_since = Some(now);
                    rev_bump |= !plate.legacy;
                }
            }
        }

        if rev_bump {
            self.bump_rev();
        }
    }

    /// Expire any reseating or withdrawn plate that was not re-submitted within `deadline`:
    /// its plate reads `failed`, and its id is returned so the gate drops and counts its commands.
    pub(crate) fn expire_reseats(&mut self, now: Instant, deadline: Duration) -> Vec<UnitId> {
        let now = self.now(now);
        let mut expired = Vec::new();
        let mut rev_bump = false;
        for s in 0..SLOT_COUNT {
            let pending_installs: BTreeSet<UnitId> = self.slots[s]
                .pending
                .iter()
                .filter(|p| matches!(p.kind, PendingKind::Install { .. }))
                .map(|p| p.unit)
                .collect();
            for plate in &mut self.slots[s].plates {
                if let Some(since) = plate.reseating_since {
                    if (!plate.seated_in_engine || plate.withdrawn)
                        && !pending_installs.contains(&plate.id)
                        && now.saturating_duration_since(since) >= deadline
                    {
                        plate.reseating_since = None;
                        plate.state = PlateState::Failed;
                        plate.error = Some("unit did not reseat in time".to_string());
                        expired.push(plate.id);
                        if !plate.legacy {
                            rev_bump = true;
                        }
                    }
                }
            }
        }
        if rev_bump {
            self.bump_rev();
        }
        expired
    }

    /// Whether a plate of `kind` is reseating ([`GateStatus::Reseating`]): a `Toggle::Units(kind)` waits
    /// in the gate until none is, so it switches every unit of its kind once each is back.
    pub(crate) fn kind_reseating(&self, kind: UnitKind) -> bool {
        self.slots.iter().flat_map(|s| &s.plates).any(|p| p.kind == kind && !p.legacy && self.unit_gate_status(p.id) == GateStatus::Reseating)
    }

    pub(crate) fn unit_gate_status(&self, id: UnitId) -> GateStatus {
        let Some((slot, plate)) = self.find_plate(id) else {
            return GateStatus::Gone;
        };
        let has_pending_install = self.has_pending_install(slot, id);
        if plate.parked && !has_pending_install {
            return GateStatus::Gone;
        }
        if self.has_pending_withdraw(slot, id) || has_pending_install {
            return GateStatus::Reseating;
        }
        if plate.withdrawn {
            return if plate.reseating_since.is_some() {
                GateStatus::Reseating
            } else {
                GateStatus::Gone
            };
        }
        if plate.seated_in_engine {
            GateStatus::Seated
        } else if plate.reseating_since.is_some() {
            GateStatus::Reseating
        } else {
            GateStatus::Gone
        }
    }
}

/// Reorder the plates whose ids appear in `order` to match `order`, keeping all other plates in
/// their existing positions. Returns whether any non-legacy plate moved.
fn reorder_plates(plates: &mut Vec<Plate>, order: &[UnitId]) -> bool {
    let order_set: BTreeSet<UnitId> = order.iter().copied().collect();
    let positions: Vec<usize> = plates
        .iter()
        .enumerate()
        .filter_map(|(i, p)| order_set.contains(&p.id).then_some(i))
        .collect();
    if positions.len() <= 1 {
        return false;
    }
    let before_ids: Vec<UnitId> = positions.iter().map(|&i| plates[i].id).collect();
    let mut by_id: BTreeMap<UnitId, Plate> = BTreeMap::new();
    for &pos in positions.iter().rev() {
        let p = plates.remove(pos);
        by_id.insert(p.id, p);
    }
    let ordered: Vec<Plate> = order.iter().filter_map(|id| by_id.remove(id)).collect();
    let mut changed = false;
    if ordered.len() == positions.len() {
        let after_ids: Vec<UnitId> = ordered.iter().map(|p| p.id).collect();
        let moved = before_ids != after_ids;
        let mut iter = ordered.into_iter();
        for &pos in &positions {
            if let Some(p) = iter.next() {
                if moved && !p.legacy {
                    changed = true;
                }
                plates.insert(pos, p);
            }
        }
    } else {
        for (pos, p) in positions.into_iter().zip(ordered.into_iter().chain(by_id.into_values())) {
            let idx = pos.min(plates.len());
            plates.insert(idx, p);
        }
    }
    changed
}

/// The model's snapshot for the feed: `FeedDrain` carries it, read in the same `settings` + `ends`
/// acquisition as the events.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ChainRev {
    /// The model generation it was read at (`Model::snapshot_for`).
    pub generation: u64,
    pub rev: u64,
    pub chains: Arc<[ChainState; SLOT_COUNT]>,
}

/// The engine events a wait drained for itself while the feed was stale (`EngineHost::wait_op`), for
/// the feed to send. Tagged by generation and cleared at a swap; filled only when no drain ran for
/// [`STASH_AFTER`] (so never beside a ticking feed). Every event is projected into settings and the
/// model as it leaves the ring, once. Past [`STASH_LEN`] it drops the oldest and latches `lost` (the
/// feed sends a reset).
pub(crate) struct Stash {
    pub(crate) generation: u64,
    pub(crate) events: VecDeque<Event>,
    pub(crate) lost: bool,
    last_feed_drain: Option<Instant>,
}

impl Stash {
    pub(crate) fn new() -> Stash {
        Stash {
            generation: 0,
            events: VecDeque::new(),
            lost: false,
            last_feed_drain: None,
        }
    }

    pub(crate) fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.events.clear();
        self.lost = false;
    }

    pub(crate) fn mark_feed_drain(&mut self, now: Instant) {
        self.last_feed_drain = Some(now);
    }

    pub(crate) fn is_stale(&self, now: Instant) -> bool {
        self.last_feed_drain.is_none_or(|last| now.saturating_duration_since(last) >= STASH_AFTER)
    }

    pub(crate) fn push(&mut self, generation: u64, event: Event) {
        if self.generation != generation {
            self.reset(generation);
        }
        if self.events.len() >= STASH_LEN {
            self.events.pop_front();
            self.lost = true;
        }
        self.events.push_back(event);
    }
}

/// [`Stash`]'s bound (`Core::stash`).
pub(crate) const STASH_LEN: usize = 4096;
/// How long without a feed drain before a wait drains for itself.
pub(crate) const STASH_AFTER: Duration = Duration::from_millis(250);

/// One slot's chain as applied (the feed's `chains`, on a reset frame and when the revision moved):
/// its units in order, their latencies' sum (bypassed ones included), and whether an edit waits for a
/// take to end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainState {
    pub units: Vec<UnitState>,
    pub latency: Frame,
    pub held: bool,
}

/// A plate's state on the wire (lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlateState {
    Loading,
    Waiting,
    Seated,
    Leaving,
    Failed,
}

/// A unit as the engine applied it. `plugin` for a plugin, `params` for a built-in; the flags are
/// present only when true.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitState {
    pub id: UnitId,
    #[serde(with = "kind_key")]
    pub kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<WireParams>,
    pub bypass: bool,
    pub source: bool,
    pub name: String,
    pub latency: Frame,
    pub state: PlateState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `failed` by a park: withdrawn, its seat kept with its place and latency.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    /// An edit of this unit waits for a take to end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    /// `loading` or `waiting` by a reload: the old instance stays seated until the new one lands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reloading: bool,
}

/// What restores a unit (`chain_add`'s `spec`, the TS `UnitSpec`): `{kind:"plugin", plugin}` or a
/// built-in's `{kind, params?}`, each with an optional `bypass`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitSpec {
    #[serde(with = "kind_key")]
    pub kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<WireParams>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass: bool,
}

impl UnitSpec {
    /// The host's [`Spec`] and the bypass it asks for; Err for a plugin without its `plugin` (or with
    /// built-in `params`), or a built-in with a `plugin`.
    pub fn into_spec(self) -> Result<(Spec, bool), String> {
        let Self { kind, plugin, params, bypass } = self;
        match kind {
            UnitKind::Plugin => {
                if params.is_some() {
                    return Err("plugin unit spec must not carry built-in params".to_string());
                }
                let plugin = plugin.ok_or_else(|| "plugin unit spec requires a plugin".to_string())?;
                Ok((Spec::Plugin(plugin), bypass))
            }
            kind => {
                if plugin.is_some() {
                    return Err(format!("built-in unit spec ({kind:?}) must not carry a plugin"));
                }
                Ok((Spec::Builtin { kind, params: params.map_or_else(Vec::new, |p| p.0) }, bypass))
            }
        }
    }
}

/// A built-in's params on the wire: an object of their keys (`UnitParam::key`), in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WireParams(pub Params);

impl Serialize for WireParams {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (param, value) in &self.0 {
            map.serialize_entry(param.key(), value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for WireParams {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> Visitor<'de> for Keys {
            type Value = WireParams;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of unit parameter keys and numbers")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireParams, A::Error> {
                let mut params = Params::new();
                while let Some((key, value)) = map.next_entry::<String, f64>()? {
                    let param = UnitParam::from_key(&key).ok_or_else(|| A::Error::custom(format!("unknown unit parameter {key:?}")))?;
                    params.push((param, value));
                }
                Ok(WireParams(params))
            }
        }
        d.deserialize_map(Keys)
    }
}

/// A [`UnitKind`] as its wire key: `plugin`, `ott`, `echo`, `reverb`, `ring`, `synth`.
mod kind_key {
    use lf_engine::UnitKind;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    const KEYS: [(UnitKind, &str); 6] = [
        (UnitKind::Plugin, "plugin"),
        (UnitKind::Ott, "ott"),
        (UnitKind::Echo, "echo"),
        (UnitKind::Reverb, "reverb"),
        (UnitKind::Ring, "ring"),
        (UnitKind::Synth, "synth"),
    ];

    pub(super) fn serialize<S: Serializer>(kind: &UnitKind, s: S) -> Result<S::Ok, S::Error> {
        let key = KEYS.iter().find(|(k, _)| k == kind).map(|(_, key)| *key).expect("every unit kind has a key");
        s.serialize_str(key)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<UnitKind, D::Error> {
        let key = String::deserialize(d)?;
        KEYS.iter().find(|(_, k)| *k == key).map(|(kind, _)| *kind).ok_or_else(|| D::Error::custom(format!("unknown unit kind {key:?}")))
    }
}
