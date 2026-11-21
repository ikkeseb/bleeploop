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

// The seam (E2 S0): the bodies are stubs that later pieces fill, so nothing here is called yet.
#![allow(dead_code)]

use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use lf_engine::grid::Frame;
use lf_engine::{Engine, InstallToken, SeatRefusal, SlotPort, UnitId, UnitKind, UnitParam, UnitProcessor, SLOT_COUNT};

use super::chain::PluginRef;
use super::settings::Settings;
use super::{Core, EngineHost, Ends};

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

/// The table: one row per install (two under one id while a reload's candidate stands beside the
/// seated instance), each with its install token and its mailbox. P2 builds it into `Core`.
///
/// - The port's reader, whoever holds its lock, files every `Returned` by token into its row's mailbox.
///   A row with no owner left, or abandoned, leaks its box. Nothing here drops a unit.
/// - The reservation lasts from [`EngineHost::unit`] to [`UnitHost::release`]; the plate leaves at the
///   removal's `Applied`.
/// - Legacy rows (`Spec::Legacy`, a device-minted id per install) are rows like any other.
pub(crate) struct Table {}

/// A unit command held back from the ring (`SetUnitBypass`, `SetUnitParam`, `ToggleUnit`:
/// `Command::unit`). A unit command goes to the ring unless its id is reseating or the gate already
/// holds one for it. The gate is per unit id, survives a second rebuild, and is flushed only by
/// [`flush_gate`]. A row not submitted [`GATE_DEADLINE`] after a swap reads `failed`, and its commands
/// drop; a later submit still seats it (its terms fence it).
pub(crate) struct Gate {}

/// How long after a swap a row may go unsubmitted before its plate reads `failed` and its gated
/// commands drop: the legacy load's bound (`host::engine_slot`'s `LOAD_TIMEOUT`).
pub(crate) const GATE_DEADLINE: Duration = Duration::from_secs(15);

/// Send the gate's commands whose id is seated, drop and count those whose id is gone, keep the rest;
/// a full ring leaves the rest for the next call. Called where `settings` and `ends` are already held.
/// Lock order: ports → settings → ends → model; the model is taken inside, and nothing is taken under
/// it.
pub(crate) fn flush_gate(core: &Core, settings: &mut Settings, ends: &mut Ends) {
    let _ = (core, settings, ends);
    todo!("E2 P2: the gate")
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
    let _ = (core, ports, old, next);
    todo!("E2 P2: the rebuild handoff")
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
    pub fn id(&self) -> UnitId {
        self.id
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The engine's terms, read under the port lock; `None` while no engine exists.
    pub fn terms(&self) -> Option<Terms> {
        todo!("E2 P2")
    }

    /// The name, role and latency onto the plate.
    pub fn activated(&self, a: Activated) {
        let _ = a;
        todo!("E2 P2")
    }

    /// A built-in's params as its build clamped them.
    pub fn committed(&self, params: Params) {
        let _ = params;
        todo!("E2 P2")
    }

    /// Hand `unit`, activated at `terms`, to the engine. The index, `replaces` (a head source over a
    /// seated one; a reload candidate: its own id) and the bypass are the model's (a same-id reseat
    /// sends the model's latest bypass, never a stale one). Index and push happen in one port-locked
    /// section. Stale terms, a full port, a superseded row, closing, or no engine: the box comes back.
    pub fn submit(&self, unit: Unit, terms: Terms) -> Result<Op, (Unit, SubmitError)> {
        let _ = (unit, terms);
        todo!("E2 P2")
    }

    /// Take the unit out and keep its seat (a restart): its box comes back as [`Back::Withdrawn`].
    pub fn withdraw(&self) -> Result<Op, SubmitError> {
        todo!("E2 P2")
    }

    /// Where `op` stands. Idempotent.
    pub fn poll(&self, op: Op) -> OpState {
        let _ = op;
        todo!("E2 P2")
    }

    /// The next box in this row's mailbox; every owner turn.
    pub fn take_back(&self) -> Option<Back> {
        todo!("E2 P2")
    }

    /// The slot holds a held message (the model's state).
    pub fn held(&self) -> bool {
        todo!("E2 P2")
    }

    /// The owner keeps the unit out (a refused reseat): the plate reads `failed` and `parked`, the seat
    /// stays.
    pub fn parked(&self, why: String) {
        let _ = why;
        todo!("E2 P2")
    }

    /// No seat: the plate reads `failed`, and the id is free.
    pub fn failed(&self, why: String) {
        let _ = why;
        todo!("E2 P2")
    }

    /// The owner gives up on its unit: its box, if it ever comes, leaks.
    pub fn abandon(&self) {
        todo!("E2 P2")
    }

    /// The owner is done: the reservation ends here.
    pub fn release(self) {
        todo!("E2 P2")
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
        let _ = (slot, id, spec, at, bypassed);
        todo!("E2 P2")
    }

    /// A reload candidate under a reserved id: a second row; the plate reads `reloading`. From the
    /// candidate's `submit` on, the outgoing row may send nothing and only receives.
    pub fn reload_unit(&self, id: UnitId, plugin: PluginRef) -> Result<UnitHost, String> {
        let _ = (id, plugin);
        todo!("E2 P2")
    }

    /// Remove `id`: a failed add's plate goes at once ([`Removal::Plate`]); otherwise the removal is
    /// sent, and the plate leaves at its `Applied`.
    pub fn remove_unit(&self, id: UnitId) -> Result<Removal, String> {
        let _ = id;
        todo!("E2 P2")
    }

    /// `to`: a plate index, translated to the engine's index as the op is pushed.
    pub fn move_unit(&self, id: UnitId, to: usize) -> Result<Op, String> {
        let _ = (id, to);
        todo!("E2 P2")
    }

    /// Block until `op` leaves `Pending`/`Held`; `bound` counts only while the model shows the slot
    /// holding nothing; with closing set, `Held` returns at once. Drains for itself when the feed is
    /// stale (the stash).
    pub fn wait_op(&self, op: Op, bound: Duration) -> Result<OpState, String> {
        let _ = (op, bound);
        todo!("E2 P2")
    }

    /// The feed: frame `seq` carried the model's revision `rev`.
    pub fn chain_told(&self, rev: u64, seq: u64) {
        let _ = (rev, seq);
        todo!("E2 P2")
    }

    /// The seq of the last frame that told `id`'s plate: a reload's `since` is checked against it.
    pub fn told(&self, id: UnitId) -> Option<u64> {
        let _ = id;
        todo!("E2 P2")
    }

    /// The app is closing: a wait that meets `Held` stops at once.
    pub fn set_closing(&self) {
        todo!("E2 P2")
    }
}
