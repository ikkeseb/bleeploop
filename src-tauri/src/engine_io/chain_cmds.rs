//! OWNS: the chain commands (E2, `docs/unit-contract.md` §2): the `chain_*` Tauri commands the UI's
//! `ChainHost` calls (`src/platform/host.ts`), the per-slot call order, the built-in unit owner loop,
//! the run exclusion with legacy plugin slots, shutdown teardown, and (in round 2) plugin adds, reload,
//! editor affinity, params, tone tokens, and `chain:editor-closed`. JSON arguments are camelCase; every
//! command that can wait runs on `spawn_blocking`.
//!
//! The epoch (`host_init`'s `frontendEpoch`) is checked at entry: a call from a replaced document is
//! refused; a call already running finishes and its unit stays (the units are the host's, and the new
//! document reads them from the reset frame). `plugin:*`, `plugin_*` and `host_init` are untouched:
//! chain units emit `chain:*` only.

use std::collections::BTreeMap;
use std::sync::atomic::{
    AtomicBool,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::{InstallToken, SeatRefusal, UnitId, UnitKind, SLOT_COUNT};
use serde::Serialize;

use super::chain::{PlateState, PluginRef, UnitSpec};
use super::mode::{engine, EngineApp};
use super::plugins::EngineSlot;
use super::unit_host::{
    self, Activated, Back, BuiltinFactory, Op, OpState, Removal, Spec, SubmitError, Unit, UnitHost,
};
use super::EngineHost;
use crate::host::{ParamDesc, PluginHostState};

#[cfg(test)]
#[path = "chain_cmds_tests.rs"]
pub(crate) mod tests;

/// What unbuilt round-2 commands answer until plugin adds, reload, editors, params and tones land.
const NOT_BUILT: &str = "chain host not built yet";

/// How often a built-in unit's owner thread polls its mailbox and pending install op.
const BUILTIN_POLL: Duration = Duration::from_millis(20);

/// How long a built-in add waits for its owner thread's first answer (`Seated`, `Waiting`, or `Err`).
const BUILTIN_ADD_TIMEOUT: Duration = Duration::from_secs(15);

/// Unheld wait bound for `chain_remove` and `chain_move` (mirrors `host::engine_slot::REMOVE_TIMEOUT`).
const REMOVE_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 10 } else { 2 });

/// Combined wait budget for all in-flight chain unit removals during app shutdown.
const SHUTDOWN_UNLOAD_TIMEOUT: Duration = Duration::from_secs(2);

/// The window event a unit's editor sends when the player closed it by its own close, or it went with
/// its unit. (`chain:param-changed` and `chain:params-changed` wait for the activation thread's drawer:
/// no subscriber in E2.)
#[allow(dead_code)] // round 2: editors
pub(crate) const EDITOR_CLOSED: &str = "chain:editor-closed";

/// [`EDITOR_CLOSED`]'s payload.
#[allow(dead_code)] // round 2: editors
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EditorClosed {
    pub unit: UnitId,
}

/// `chain_add`'s answer: the engine took the install, or holds it until a take ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Added {
    Seated,
    Waiting,
}

/// `chain_reload`'s answer: a fresh instance seated, the unit moved on since `since` (nothing done),
/// or the load failed (the old instance stays).
#[allow(dead_code)] // round 2: reload
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reloaded {
    Reloaded,
    Moved,
    Failed,
}

/// `chain_tone_import`'s answer: the token a matching unit's reload passes (`None`: no unit matched,
/// and the bytes became the slot's file), the plugin's name, and the units the tone may go to: its own
/// unit if seated with that identity, in either slot; then the import slot's other seated units of that
/// identity, in chain order.
#[allow(dead_code)] // round 2: tones
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainToneImport {
    pub token: Option<u32>,
    pub name: String,
    pub matches: Vec<UnitId>,
}

/// One registered chain unit: its slot, row install token, and owner thread handle.
struct Entry {
    #[allow(dead_code)] // round 2: reload, editors, tone import
    slot: usize,
    token: InstallToken,
    owner: Owner,
}

/// A chain unit's owner; round 2 adds the plugin owner variant here.
enum Owner {
    Builtin(BuiltinOwner),
}

impl Owner {
    fn is_finished(&self) -> bool {
        match self {
            Owner::Builtin(b) => b.is_finished(),
        }
    }

    fn request_stop(&self) {
        match self {
            Owner::Builtin(b) => b.request_stop(),
        }
    }

    fn join(&self) {
        match self {
            Owner::Builtin(b) => b.join(),
        }
    }
}

struct BuiltinOwner {
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl BuiltinOwner {
    fn is_finished(&self) -> bool {
        let join = self.join.lock().unwrap_or_else(|e| e.into_inner());
        join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn request_stop(&self) {
        self.stop.store(true, Release);
        let _g = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        self.wake.1.notify_all();
    }

    fn join(&self) {
        let handle = self.join.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for BuiltinOwner {
    fn drop(&mut self) {
        self.request_stop();
        self.join();
    }
}

/// Compile-time guarantee that `Entry`, `Owner` and `Chains` stay `Send + Sync`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Entry>();
    assert_send_sync::<Owner>();
    assert_send_sync::<Chains>();
};

/// The chain host's per-app state: built-in factory, live unit registry, departing owners, per-slot
/// call serialization, and shutdown latch.
pub(crate) struct Chains {
    factory: Option<Arc<dyn BuiltinFactory>>,
    registry: Mutex<BTreeMap<UnitId, Arc<Entry>>>,
    gone: Mutex<Vec<Arc<Entry>>>,
    order: [Mutex<()>; SLOT_COUNT],
    closing: AtomicBool,
}

impl Chains {
    /// Production state: no built-in factory in E2 (built-in adds are refused at once).
    pub(crate) fn new() -> Self {
        Self::with_optional_factory(None)
    }

    #[cfg(test)]
    pub(crate) fn with_factory(factory: Arc<dyn BuiltinFactory>) -> Self {
        Self::with_optional_factory(Some(factory))
    }

    fn with_optional_factory(factory: Option<Arc<dyn BuiltinFactory>>) -> Self {
        Chains {
            factory,
            registry: Mutex::new(BTreeMap::new()),
            gone: Mutex::new(Vec::new()),
            order: std::array::from_fn(|_| Mutex::new(())),
            closing: AtomicBool::new(false),
        }
    }

    /// Move finished registry entries (e.g. a replaced source or `Dismissed` unit) into `gone`, and
    /// drop finished `gone` entries whose last `Arc` lives here (instant join).
    fn sweep(&self) {
        let mut moved = Vec::new();
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let finished: Vec<UnitId> = reg
                .iter()
                .filter_map(|(&id, entry)| entry.owner.is_finished().then_some(id))
                .collect();
            for id in finished {
                if let Some(entry) = reg.remove(&id) {
                    moved.push(entry);
                }
            }
        }
        let mut drop_now = Vec::new();
        {
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            gone.extend(moved);
            let mut i = 0;
            while i < gone.len() {
                if gone[i].owner.is_finished() && Arc::strong_count(&gone[i]) == 1 {
                    drop_now.push(gone.swap_remove(i));
                } else {
                    i += 1;
                }
            }
        }
        drop(drop_now);
    }

    /// Move `unit`'s registry entry to `gone` if it still matches `token` (or any entry if `None`).
    fn retire_to_gone(&self, unit: UnitId, token: Option<InstallToken>) {
        let removed = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let matches = reg.get(&unit).is_some_and(|e| token.is_none_or(|t| e.token == t));
            if matches {
                reg.remove(&unit)
            } else {
                None
            }
        };
        if let Some(entry) = removed {
            self.gone.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
        }
    }

    /// Remove and drop `unit`'s entry if it still matches `token` (used when an add fails before seat).
    fn drop_entry_if_token(&self, unit: UnitId, token: InstallToken) {
        let removed = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            if reg.get(&unit).is_some_and(|e| e.token == token) {
                reg.remove(&unit)
            } else {
                None
            }
        };
        drop(removed);
    }

    /// Add a unit to `slot` at plate index `at`. Round 1 implements built-in units through the injected
    /// factory; plugin adds answer `NOT_BUILT` until round 2.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
        slot: usize,
        unit: UnitId,
        spec: UnitSpec,
        at: usize,
    ) -> Result<Added, String> {
        current_document(doc.0, doc.1)?;
        self.sweep();
        let (spec, bypassed) = spec.into_spec()?;
        let (kind, params) = match spec {
            Spec::Plugin(_) => return Err(NOT_BUILT.into()),
            Spec::Legacy => return Err("legacy unit spec is not valid for chain_add".to_string()),
            Spec::Builtin { kind, params } => (kind, params),
        };
        let Some(factory) = self.factory.as_ref().cloned() else {
            return Err(builtin_unavailable_reason(kind));
        };
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }

        let _order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
        if self.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }

        // Lock order: order[slot] -> slots -> host.core.state.
        let (unit_host, token) = {
            let slots_guard = slots.lock().map_err(|_| "engine slots poisoned".to_string())?;
            for (s, entry) in slots_guard.iter().enumerate() {
                if !matches!(entry, EngineSlot::Empty) {
                    return Err(format!(
                        "a plugin is loaded in slot {s} through the plugin slot: restart the app to use chains"
                    ));
                }
            }
            let unit_host = match host.unit(slot, unit, Spec::Builtin { kind, params: params.clone() }, at, bypassed) {
                Ok(u) => u,
                Err(why) => {
                    if !host.core.closing.load(Acquire) && is_unit_leaving(host, unit) {
                        return Err(format!(
                            "unit {unit} is still leaving its chain (a take holds its removal); add it again when the take ends"
                        ));
                    }
                    return Err(why);
                }
            };
            let token = {
                let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .table
                    .active_row_for_id(unit)
                    .map(|r| r.token)
                    .ok_or_else(|| format!("unit {unit} row missing after reservation"))?
            };
            (unit_host, token)
        };

        let Some(terms) = unit_host.terms() else {
            let why = "no audio device is open".to_string();
            unit_host.failed(why.clone());
            unit_host.release();
            return Err(why);
        };

        let (boxed, clamped) = match factory.build(kind, &params, terms) {
            Ok(built) => built,
            Err(why) => {
                unit_host.failed(why.clone());
                unit_host.release();
                return Err(why);
            }
        };

        let info = boxed.info();
        unit_host.committed(clamped);
        unit_host.activated(Activated {
            name: builtin_display_name(kind).to_string(),
            source: info.source,
            latency: info.latency,
        });

        let initial_seat = match unit_host.submit(boxed, terms) {
            Ok(op) => BuiltinSeat::Installing { op },
            Err((returned, SubmitError::Stale | SubmitError::PortFull | SubmitError::Reloading)) => {
                BuiltinSeat::Owed { unit: returned }
            }
            Err((returned, err)) => {
                drop(returned);
                let why = submit_error_reason(err);
                unit_host.failed(why.clone());
                unit_host.release();
                return Err(why);
            }
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<Added, String>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let owner_stop = stop.clone();
        let owner_wake = wake.clone();
        let in_engine = matches!(initial_seat, BuiltinSeat::Installing { .. });
        let spawned = std::thread::Builder::new()
            .name(format!("lf-builtin-unit-{unit}"))
            .spawn(move || serve_builtin(unit_host, initial_seat, ready_tx, owner_stop, owner_wake));

        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                // `unit_host` was moved into the closure and dropped only on Err(spawn), which isn't
                // reachable here because Rust drops the closure; `in_engine` is just for diagnostics.
                let _ = in_engine;
                return Err(format!("could not spawn owner for built-in unit {unit}: {e}"));
            }
        };

        let entry = Arc::new(Entry {
            slot,
            token,
            owner: Owner::Builtin(BuiltinOwner {
                stop,
                wake,
                join: Mutex::new(Some(handle)),
            }),
        });

        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            if self.closing.load(Acquire) {
                drop(reg);
                entry.owner.request_stop();
                entry.owner.join();
                return Err("the host is closing".to_string());
            }
            reg.insert(unit, entry.clone());
        }

        match ready_rx.recv_timeout(BUILTIN_ADD_TIMEOUT) {
            Ok(Ok(added)) => Ok(added),
            Ok(Err(why)) => {
                self.drop_entry_if_token(unit, token);
                Err(why)
            }
            Err(_) => Err(format!(
                "built-in unit {unit} did not answer within {} s",
                BUILTIN_ADD_TIMEOUT.as_secs()
            )),
        }
    }

    /// Remove `unit` from its chain. Holds the slot's order mutex until the op settles or is held;
    /// releases it before waiting across a take.
    pub(crate) fn remove(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        self.sweep();
        let slot = find_unit_slot(host, unit)?;
        let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
        let token = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).map(|e| e.token)
        };
        let op = match host.remove_unit(unit)? {
            Removal::Plate => {
                drop(order);
                if token.is_some() {
                    self.retire_to_gone(unit, token);
                }
                return Ok(());
            }
            Removal::Sent(op) => op,
        };
        let state = wait_ordered(host, order, op, REMOVE_TIMEOUT)?;
        match state {
            OpState::Applied => {
                self.retire_to_gone(unit, token);
                Ok(())
            }
            OpState::Refused(why) => Err(format!("removing unit {unit} was refused: {}", refusal_reason(why))),
            OpState::Held => Err(format!("removing unit {unit} was interrupted by shutdown")),
            other => Err(format!("removing unit {unit} did not apply ({other:?})")),
        }
    }

    /// Move `unit` to plate index `to` within its own slot. Holds the slot's order mutex until the op
    /// settles or is held; releases it before waiting across a take.
    pub(crate) fn move_unit(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
        to: usize,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        self.sweep();
        let slot = find_unit_slot(host, unit)?;
        let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
        let op = host.move_unit(unit, to)?;
        let state = wait_ordered(host, order, op, REMOVE_TIMEOUT)?;
        match state {
            OpState::Applied => Ok(()),
            OpState::Refused(why) => Err(format!("moving unit {unit} was refused: {}", refusal_reason(why))),
            OpState::Held => Err(format!("moving unit {unit} was interrupted by shutdown")),
            other => Err(format!("moving unit {unit} did not apply ({other:?})")),
        }
    }

    /// On shutdown, while the device still plays: mark closing (so any add about to register stops on
    /// the spot), sweep finished entries, send `remove_unit` for every registered unit, and wait for
    /// all of them under one 2 s deadline (held ops return at once with the host closing).
    pub(super) fn unload_all(&self, host: &EngineHost) {
        self.closing.store(true, Release);
        host.set_closing();
        self.sweep();
        let ids: Vec<UnitId> = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.keys().copied().collect()
        };
        let mut ops = Vec::new();
        for id in ids {
            match host.remove_unit(id) {
                Ok(Removal::Plate) => {}
                Ok(Removal::Sent(op)) => ops.push((id, op)),
                Err(why) => log::warn!("[chain_cmds] shutdown remove unit {id}: {why}"),
            }
        }
        let deadline = Instant::now() + SHUTDOWN_UNLOAD_TIMEOUT;
        for (id, op) in ops {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match host.wait_op(op, remaining) {
                Ok(OpState::Applied | OpState::Held) => {}
                Ok(other) => log::warn!("[chain_cmds] shutdown remove unit {id} ended {other:?}"),
                Err(why) => log::warn!("[chain_cmds] shutdown wait unit {id}: {why}"),
            }
        }
    }

    /// After `host.shutdown()`: signal stop to every remaining owner in `registry` and `gone` and join
    /// their threads.
    pub(super) fn join_all(&self) {
        let mut entries = Vec::new();
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            entries.extend(reg.values().cloned());
            reg.clear();
        }
        {
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            entries.append(&mut *gone);
        }
        for entry in &entries {
            entry.owner.request_stop();
        }
        for entry in &entries {
            entry.owner.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn active_entries(&self) -> usize {
        let r = self.registry.lock().unwrap_or_else(|e| e.into_inner()).len();
        let g = self.gone.lock().unwrap_or_else(|e| e.into_inner()).len();
        r + g
    }
}

/// Whether any chain unit (or non-legacy plate) exists in this run; called under the legacy slots lock
/// (`plugins::LoadTarget::reserve`) to enforce run-wide exclusion between legacy plugin slots and chains.
pub(super) fn chain_units_exist(host: &EngineHost) -> bool {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    state.table.has_any_chain() || state.model.has_non_legacy_plate()
}

/// A request from a replaced WebView document is refused: its `frontendEpoch` is no longer the one
/// `host_init` last answered. Checked at entry only; a call already running finishes.
fn current_document(state: &PluginHostState, frontend_epoch: u32) -> Result<(), String> {
    let current = state.frontend_epoch.load(Relaxed);
    if frontend_epoch == current {
        Ok(())
    } else {
        Err(format!("stale frontend epoch {frontend_epoch} (current {current})"))
    }
}

fn find_unit_slot(host: &EngineHost, unit: UnitId) -> Result<usize, String> {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    state
        .model
        .find_plate(unit)
        .map(|(s, _)| s)
        .ok_or_else(|| format!("unit {unit} not found"))
}

fn is_unit_leaving(host: &EngineHost, unit: UnitId) -> bool {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    state
        .model
        .find_plate(unit)
        .is_some_and(|(s, p)| p.state == PlateState::Leaving || state.model.pending_remove_op(s, unit).is_some())
        || state.model.has_pending_remove(unit)
}

/// Wait on `op` while holding `order` until it settles or becomes `Held`; on `Held`, release `order`
/// first so other calls on the slot are never blocked across a take, then finish with `host.wait_op`.
fn wait_ordered(
    host: &EngineHost,
    order: std::sync::MutexGuard<'_, ()>,
    op: Op,
    bound: Duration,
) -> Result<OpState, String> {
    let first = unit_host::wait_op_inner(&host.core, op, bound, true)?;
    drop(order);
    if first == OpState::Held {
        host.wait_op(op, bound)
    } else {
        Ok(first)
    }
}

fn builtin_display_name(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Ott => "OTT",
        UnitKind::Echo => "ECHO",
        UnitKind::Reverb => "REVERB",
        UnitKind::Ring => "RING MOD",
        UnitKind::Synth => "SYNTH",
        UnitKind::Plugin => "PLUGIN",
    }
}

fn builtin_unavailable_reason(kind: UnitKind) -> String {
    let name = builtin_display_name(kind);
    let article = match kind {
        UnitKind::Ott | UnitKind::Echo => "an",
        _ => "a",
    };
    format!("built-in units arrive in a later build: this one cannot add {article} {name}")
}

fn refusal_reason(why: SeatRefusal) -> String {
    match why {
        SeatRefusal::Full => "the chain already seats the maximum number of units (Full)".to_string(),
        SeatRefusal::Latency => "the chain latency exceeds the maximum allowed (Latency)".to_string(),
        SeatRefusal::Id => "the unit id is invalid or already seated (Id)".to_string(),
        SeatRefusal::Placement => "the unit cannot be placed at that position in the chain (Placement)".to_string(),
        SeatRefusal::Unknown => "the engine refused the unit (Unknown)".to_string(),
    }
}

fn submit_error_reason(err: SubmitError) -> String {
    match err {
        SubmitError::Stale => "engine terms changed before submit".to_string(),
        SubmitError::PortFull => "the slot's engine port is full".to_string(),
        SubmitError::Superseded => "the unit was superseded before seating".to_string(),
        SubmitError::Removing => "the unit is being removed".to_string(),
        SubmitError::Reloading => "the unit is reloading".to_string(),
        SubmitError::Closing => "the audio host is closing".to_string(),
        SubmitError::NoEngine => "no audio device is open".to_string(),
    }
}

/// Where a built-in unit's box stands relative to the engine.
enum BuiltinSeat {
    Owed { unit: Unit },
    Installing { op: Op },
    Seated,
    Parked { unit: Unit },
    Removing { unit: Unit },
    Closing { unit: Unit },
    InTransit,
}

impl BuiltinSeat {
    fn take_unit(&mut self) -> Option<Unit> {
        match std::mem::replace(self, BuiltinSeat::InTransit) {
            BuiltinSeat::Owed { unit }
            | BuiltinSeat::Parked { unit }
            | BuiltinSeat::Removing { unit }
            | BuiltinSeat::Closing { unit } => Some(unit),
            other => {
                *self = other;
                None
            }
        }
    }
}

fn send_first_answer(ready: &mut Option<SyncSender<Result<Added, String>>>, answer: Result<Added, String>) {
    if let Some(tx) = ready.take() {
        let _ = tx.try_send(answer);
    }
}

/// Built-in unit owner loop (`lf-builtin-unit-{id}`): polls every 20 ms without blocking, answers the
/// caller once (`Seated` / `Waiting` / `Err`), resubmits the same box after evictions/withdrawals,
/// parks on post-seat refusal, drops the box off the RT thread on terminal return, and drains/abandons
/// cleanly on stop.
fn serve_builtin(
    host: UnitHost,
    mut seat: BuiltinSeat,
    ready_tx: SyncSender<Result<Added, String>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
) {
    let mut ready = Some(ready_tx);
    let mut ever_seated = false;

    while !stop.load(Acquire) {
        while let Some(back) = host.take_back() {
            match back {
                Back::Removed(unit) | Back::Superseded(unit) => {
                    drop(seat.take_unit());
                    drop(unit);
                    host.release();
                    send_first_answer(&mut ready, Err("unit left before seating".to_string()));
                    return;
                }
                Back::Dismissed => {
                    drop(seat.take_unit());
                    host.release();
                    send_first_answer(&mut ready, Err("unit dismissed before seating".to_string()));
                    return;
                }
                Back::Refused { unit, why } => {
                    let msg = refusal_reason(why);
                    if !ever_seated {
                        drop(seat.take_unit());
                        drop(unit);
                        host.failed(msg.clone());
                        host.release();
                        send_first_answer(&mut ready, Err(msg));
                        return;
                    }
                    host.parked(msg);
                    seat = BuiltinSeat::Parked { unit };
                }
                Back::Withdrawn { unit, .. } => {
                    ever_seated = true;
                    seat = BuiltinSeat::Owed { unit };
                }
                Back::Evicted { unit, .. } => {
                    if matches!(seat, BuiltinSeat::Installing { op } if host.poll(op) == OpState::Applied) {
                        ever_seated = true;
                        send_first_answer(&mut ready, Ok(Added::Seated));
                    }
                    seat = BuiltinSeat::Owed { unit };
                }
            }
        }

        if matches!(seat, BuiltinSeat::Owed { .. }) {
            let unit = seat.take_unit().expect("checked Owed");
            match host.terms() {
                None => {
                    seat = BuiltinSeat::Owed { unit };
                }
                Some(terms) => match host.submit(unit, terms) {
                    Ok(op) => {
                        seat = BuiltinSeat::Installing { op };
                    }
                    Err((returned, SubmitError::Stale | SubmitError::NoEngine | SubmitError::PortFull | SubmitError::Reloading)) => {
                        seat = BuiltinSeat::Owed { unit: returned };
                    }
                    Err((returned, SubmitError::Removing)) => {
                        seat = BuiltinSeat::Removing { unit: returned };
                    }
                    Err((returned, SubmitError::Closing)) => {
                        seat = BuiltinSeat::Closing { unit: returned };
                    }
                    Err((returned, SubmitError::Superseded)) => {
                        drop(returned);
                        host.release();
                        send_first_answer(&mut ready, Err("the unit was superseded before seating".to_string()));
                        return;
                    }
                },
            }
        }

        if let BuiltinSeat::Installing { op } = seat {
            match host.poll(op) {
                OpState::Applied => {
                    seat = BuiltinSeat::Seated;
                    ever_seated = true;
                    send_first_answer(&mut ready, Ok(Added::Seated));
                }
                OpState::Held => {
                    send_first_answer(&mut ready, Ok(Added::Waiting));
                }
                OpState::Pending if host.held() => {
                    send_first_answer(&mut ready, Ok(Added::Waiting));
                }
                OpState::Pending | OpState::Refused(_) | OpState::Rebuilt => {}
            }
        }

        let guard = wake.0.lock().unwrap_or_else(|e| e.into_inner());
        if !stop.load(Acquire) {
            let _ = wake.1.wait_timeout(guard, BUILTIN_POLL);
        }
    }

    // Stop requested: drain anything returned (or held in Leave::Pending), abandon only if the box
    // never came back, then release the reservation.
    let mut have_box = seat.take_unit().is_some();
    let mut dismissed = false;
    while let Some(back) = host.take_back_for_shutdown() {
        match back {
            Back::Removed(u)
            | Back::Superseded(u)
            | Back::Withdrawn { unit: u, .. }
            | Back::Evicted { unit: u, .. }
            | Back::Refused { unit: u, .. } => {
                drop(u);
                have_box = true;
            }
            Back::Dismissed => {
                dismissed = true;
            }
        }
    }
    if !have_box && !dismissed {
        host.abandon();
    }
    host.release();
    send_first_answer(&mut ready, Err("the host is closing".to_string()));
}

/// A command's work on the runtime's blocking pool, reading the one shared `PluginHostState` there (as
/// `host/commands.rs`' `blocking`). A task that failed (it panicked) is the host's fault.
async fn blocking<T: Send + 'static>(
    app: tauri::AppHandle,
    op: &'static str,
    work: impl FnOnce(&'static EngineApp, &PluginHostState) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        work(engine()?, &app.state::<PluginHostState>())
    })
    .await
    .map_err(|e| format!("{op} task: {e}"))?
}

/// Reserve `unit` across both slots, build it, hand it over at plate index `at` (a source goes to the
/// head, replacing a seated one); settles on the engine's answer. Rejects at once for a reserved id or a
/// full chain, and on a failed load, or if the first answer does not arrive in time.
#[tauri::command]
pub async fn chain_add(
    slot: u8,
    unit: UnitId,
    spec: UnitSpec,
    at: usize,
    tone_token: Option<u32>,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<Added, String> {
    blocking(app, "chain_add", move |app, state| {
        let _ = tone_token;
        let host = app.host()?;
        app.chains.add(&host, (state, frontend_epoch), &app.slots, usize::from(slot), unit, spec, at)
    })
    .await
}

/// Remove `unit`; settles on the removal's `Applied` (a failed add's plate goes at once).
#[tauri::command]
pub async fn chain_remove(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_remove", move |app, state| {
        let host = app.host()?;
        app.chains.remove(&host, (state, frontend_epoch), unit)
    })
    .await
}

/// A fresh instance of `plugin` under the same id, place and bypass, if `unit` is still that plugin and
/// was not re-added since `since` (a `chains` frame's `seq`, `EngineHost::told`); the load applies
/// `tone_token`.
#[tauri::command]
pub async fn chain_reload(
    unit: UnitId,
    plugin: PluginRef,
    since: u64,
    tone_token: u32,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<Reloaded, String> {
    blocking(app, "chain_reload", move |_, _| {
        let _ = (unit, plugin, since, tone_token, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Move `unit` to plate index `to` in its own chain (no move across slots).
#[tauri::command]
pub async fn chain_move(unit: UnitId, to: usize, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_move", move |app, state| {
        let host = app.host()?;
        app.chains.move_unit(&host, (state, frontend_epoch), unit, to)
    })
    .await
}

/// Open `unit`'s editor on its owner thread (one editor per plugin file at a time: the affinity table).
#[tauri::command]
pub async fn chain_open_editor(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_open_editor", move |_, _| {
        let _ = (unit, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Close `unit`'s editor. Idempotent.
#[tauri::command]
pub async fn chain_close_editor(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_close_editor", move |_, _| {
        let _ = (unit, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// `unit`'s plugin parameters (stable ids, ranges, live values), from its owner thread.
#[tauri::command]
pub async fn chain_list_params(unit: UnitId, app: tauri::AppHandle) -> Result<Vec<ParamDesc>, String> {
    blocking(app, "chain_list_params", move |_, _| {
        let _ = unit;
        Err(NOT_BUILT.into())
    })
    .await
}

/// Set one of `unit`'s plugin parameters; `param_id` must be one it listed.
#[tauri::command]
pub async fn chain_set_param(
    unit: UnitId,
    param_id: u32,
    value: f64,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<(), String> {
    blocking(app, "chain_set_param", move |_, _| {
        let _ = (unit, param_id, value, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Save `unit`'s tone now, through its owner, and answer the tone file's bytes as the raw response
/// (empty: the plugin keeps no state).
#[tauri::command]
pub async fn chain_tone_take(unit: UnitId, app: tauri::AppHandle) -> Result<tauri::ipc::Response, String> {
    let bytes: Vec<u8> = blocking(app, "chain_tone_take", move |_, _| {
        let _ = unit;
        Err(NOT_BUILT.into())
    })
    .await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Park a session's tone for the units it may go to. The raw request body is the tone file's bytes;
/// headers: `slot`, `plugin` (`{ format, path, id }` as JSON, ASCII with `\u` escapes, as
/// `plugin_tone_import`), `unit` (optional: the unit the session saved it for), `epoch`. The tone is
/// checked against `plugin` and nothing is written unless no unit matches (then it becomes the slot's
/// file, token `null`).
#[tauri::command]
pub async fn chain_tone_import(request: tauri::ipc::Request<'_>, app: tauri::AppHandle) -> Result<ChainToneImport, String> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let slot: u8 = header("slot").and_then(|v| v.parse().ok()).ok_or("chain_tone_import needs a slot header")?;
    let plugin: PluginRef = header("plugin")
        .and_then(|v| serde_json::from_str(v).ok())
        .ok_or("chain_tone_import needs the plugin the session names (a plugin header)")?;
    let unit: Option<UnitId> = match header("unit") {
        Some(v) => Some(v.parse().map_err(|_| "chain_tone_import's unit header is no unit id")?),
        None => None,
    };
    let epoch: u32 = header("epoch").and_then(|v| v.parse().ok()).ok_or("chain_tone_import needs an epoch header")?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("chain_tone_import takes the tone file's bytes as the raw request body".to_string());
    };
    let bytes = bytes.clone();
    blocking(app, "chain_tone_import", move |_, _| {
        let _ = (slot, plugin, unit, epoch, bytes);
        Err(NOT_BUILT.into())
    })
    .await
}

/// The reload a tone import answered `token` for did not happen: drop the tone parked for it.
#[tauri::command]
pub async fn chain_tone_forget(token: u32, frontend_epoch: u32) -> Result<(), String> {
    let _ = (token, frontend_epoch);
    Err(NOT_BUILT.into())
}
