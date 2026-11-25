//! OWNS: the chain commands (E2, `docs/unit-contract.md` §2): the `chain_*` Tauri commands the UI's
//! `ChainHost` calls (`src/platform/host.ts`), the per-slot call order, the built-in and plugin unit
//! owners, the run exclusion with legacy plugin slots, shutdown teardown, plugin adds, reload, editor
//! affinity, params, tone tokens, and `chain:*` window events. JSON arguments are camelCase; every
//! command that can wait runs on `spawn_blocking`.
//!
//! The epoch (`host_init`'s `frontendEpoch`) is checked at entry: a call from a replaced document is
//! refused; a call already running finishes and its unit stays (the units are the host's, and the new
//! document reads them from the reset frame). `plugin:*`, `plugin_*` and `host_init` are untouched:
//! chain units emit `chain:*` only.

use std::collections::{BTreeMap, BTreeSet};
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
use tauri::Emitter;

use super::builtins::Builtins;
use super::chain::{PlateState, PluginRef, UnitSpec};
use super::mode::{engine, EngineApp};
use super::plugins::EngineSlot;
use super::unit_host::{
    self, Activated, Back, BuiltinFactory, Op, OpState, Removal, Spec, SubmitError, Unit, UnitHost,
};
use super::EngineHost;
use crate::host::engine_slot::{EngineSlotEvent, EventSink, PluginFormat};
use crate::host::tone::{Tone, ToneBinding, ToneKey, ToneStore, ToneTokens};
use crate::host::unit_owner::{self, Answer, UnitOwner};
use crate::host::{ParamDesc, PluginHostState};

#[cfg(test)]
#[path = "chain_cmds_tests.rs"]
pub(crate) mod tests;

/// Literal returned by `unit_owner::spawn_unit_with` when plugin setup hangs past its bound and the
/// owner thread is left running (the module may stay loaded, so affinity leaks the holder).
const SETUP_TIMEOUT_ERR: &str = "plugin setup timed out";

/// How often a built-in unit's owner thread polls its mailbox and pending install op.
const BUILTIN_POLL: Duration = Duration::from_millis(20);

/// How long a built-in add waits for its owner thread's first answer (`Seated`, `Waiting`, or `Err`).
const BUILTIN_ADD_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound for an initial built-in submit retrying `Stale`/`Reloading` before failing cleanly; strictly
/// below `BUILTIN_ADD_TIMEOUT` so the owner always exits and answers before the caller releases order.
const BUILTIN_INITIAL_SUBMIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound for stopping/joining a late entry that finished opening after `join_all` already completed.
const LATE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Unheld wait bound for `chain_remove` and `chain_move` (mirrors `host::engine_slot::REMOVE_TIMEOUT`).
const REMOVE_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 10 } else { 2 });

/// Unheld wait bound for a waiting `chain_reload` to settle after the slot is no longer held.
const RELOAD_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Combined wait budget for all in-flight chain unit removals during app shutdown.
const SHUTDOWN_UNLOAD_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound for a stopping built-in owner to collect a unit box already on its way back from the engine
/// before declaring it abandoned (matches production `REMOVE_TIMEOUT`).
const BUILTIN_STOP_RETURN_TIMEOUT: Duration = Duration::from_secs(2);

/// The window event a unit's editor sends when the player closed it by its own close, or it went with
/// its unit.
pub(crate) const EDITOR_CLOSED: &str = "chain:editor-closed";

/// The window event a unit's plugin editor sends when a parameter moves.
pub(crate) const PARAM_CHANGED: &str = "chain:param-changed";

/// The window event a unit's plugin sends when its parameter list/values changed wholesale.
pub(crate) const PARAMS_CHANGED: &str = "chain:params-changed";

/// Pure mapping from an owner's [`EngineSlotEvent`] to its `chain:*` window event name and payload.
pub(crate) fn chain_event(unit: UnitId, event: EngineSlotEvent) -> (&'static str, serde_json::Value) {
    match event {
        EngineSlotEvent::EditorClosed => (EDITOR_CLOSED, serde_json::json!({ "unit": unit })),
        EngineSlotEvent::ParamChanged { id, value } => {
            (PARAM_CHANGED, serde_json::json!({ "unit": unit, "id": id, "value": value }))
        }
        EngineSlotEvent::ParamsChanged => (PARAMS_CHANGED, serde_json::json!({ "unit": unit })),
    }
}

/// Opener seam for loading a plugin unit: `unit_owner::load_unit` in production, a fake in tests.
pub(crate) type Open<'a> = &'a dyn Fn(
    PluginFormat,
    &PluginRef,
    UnitHost,
    EventSink,
    Option<ToneBinding>,
) -> Result<(UnitOwner, Answer), String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AffinityState {
    Owned,
    Orphaned,
    Unordered,
}

struct AffinityEntry {
    inst: u64,
    unit: UnitId,
    state: AffinityState,
}

/// Per-plugin-file editor affinity table (D29, unit contract §1), slot-blind.
///
/// The first load of a path no instance holds owns its editor; a second load while the owner's load
/// is still in flight makes the entry `unordered`; the owner going while other instances still hold
/// the module makes it `orphaned`; the last holder releasing deletes the entry so the next load owns
/// it afresh; a `leak`ed holder (e.g. a setup timeout whose owner thread stayed running) never
/// releases.
#[derive(Default)]
pub(crate) struct Affinity {
    next_inst: u64,
    entries: BTreeMap<String, AffinityEntry>,
    holders: BTreeMap<String, BTreeSet<u64>>,
    loading: BTreeSet<u64>,
    leaked: BTreeSet<u64>,
}

impl Affinity {
    pub(crate) fn claim(&mut self, path: &str) -> u64 {
        self.next_inst += 1;
        let inst = self.next_inst;
        if let Some(entry) = self.entries.get_mut(path) {
            if entry.state == AffinityState::Owned && self.loading.contains(&entry.inst) {
                entry.state = AffinityState::Unordered;
            }
        } else {
            self.entries.insert(
                path.to_string(),
                AffinityEntry { inst, unit: 0, state: AffinityState::Owned },
            );
        }
        self.holders.entry(path.to_string()).or_default().insert(inst);
        self.loading.insert(inst);
        inst
    }

    pub(crate) fn landed(&mut self, inst: u64, path: &str, unit: UnitId) {
        self.loading.remove(&inst);
        if let Some(entry) = self.entries.get_mut(path) {
            if entry.inst == inst {
                entry.unit = unit;
            }
        }
    }

    pub(crate) fn release(&mut self, inst: u64, path: &str) {
        if self.leaked.contains(&inst) {
            return;
        }
        self.loading.remove(&inst);
        let empty = match self.holders.get_mut(path) {
            Some(holding) => {
                holding.remove(&inst);
                holding.is_empty()
            }
            None => true,
        };
        if empty {
            self.holders.remove(path);
            self.entries.remove(path);
        } else if let Some(entry) = self.entries.get_mut(path) {
            if entry.inst == inst && entry.state == AffinityState::Owned {
                entry.state = AffinityState::Orphaned;
            }
        }
    }

    pub(crate) fn leak(&mut self, inst: u64, path: &str) {
        self.leaked.insert(inst);
        self.holders.entry(path.to_string()).or_default().insert(inst);
        if self.entries.get(path).is_some_and(|e| e.inst != inst) {
            self.loading.remove(&inst);
        }
    }

    pub(crate) fn check(&self, path: &str, inst: u64) -> Result<(), String> {
        let Some(entry) = self.entries.get(path) else {
            return Ok(());
        };
        match entry.state {
            AffinityState::Orphaned => Err(format!(
                "the editor is bound to unit {}'s previous load",
                entry.unit
            )),
            AffinityState::Unordered => Err(
                "the editor belongs to an unknown unit (two loaded it at once)".to_string(),
            ),
            AffinityState::Owned if entry.inst != inst => {
                Err(format!("the editor belongs to unit {}", entry.unit))
            }
            AffinityState::Owned => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_held(&self, path: &str) -> bool {
        self.holders.get(path).is_some_and(|h| !h.is_empty())
    }
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainToneImport {
    pub token: Option<u32>,
    pub name: String,
    pub matches: Vec<UnitId>,
}

/// One registered chain unit: its slot, row install token, and owner thread handle.
struct Entry {
    #[allow(dead_code)] // kept beside the owner for symmetry and diagnostics
    slot: usize,
    token: InstallToken,
    owner: Owner,
}

/// A plugin unit's registered owner and its editor-affinity identity.
struct PluginOwner {
    plugin: PluginRef,
    inst: u64,
    owner: UnitOwner,
}

/// A chain unit's owner (built-in or native plugin).
enum Owner {
    Builtin(BuiltinOwner),
    Plugin(PluginOwner),
}

impl Owner {
    fn is_finished(&self) -> bool {
        match self {
            Owner::Builtin(b) => b.is_finished(),
            Owner::Plugin(p) => p.owner.is_finished(),
        }
    }

    fn request_stop(&self) {
        match self {
            Owner::Builtin(b) => b.request_stop(),
            Owner::Plugin(p) => p.owner.request_stop(),
        }
    }

    fn join(&self) {
        match self {
            Owner::Builtin(b) => b.join(),
            Owner::Plugin(p) => {
                if let Err(e) = p.owner.join() {
                    log::error!("[chain_cmds] plugin unit {} stop: {e}", p.plugin.id);
                }
            }
        }
    }

    fn affinity_handle(&self) -> Option<(u64, String)> {
        match self {
            Owner::Builtin(_) => None,
            Owner::Plugin(p) => Some((p.inst, p.plugin.path.clone())),
        }
    }
}

/// A reload candidate held aside while waiting for a take to end (or after an `Unanswered`/timed-out
/// wait), to be swapped into `registry` on promotion or retired to `gone` on failure.
struct PendingReload {
    unit: UnitId,
    slot: usize,
    old_token: InstallToken,
    candidate_token: InstallToken,
    start_gen: u64,
    plugin: PluginRef,
    inst: u64,
    owner: UnitOwner,
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

/// Departing owners awaiting instant drop in `sweep` or final stop/join in `join_all`.
#[derive(Default)]
struct GoneState {
    entries: Vec<Arc<Entry>>,
    /// True once `join_all` has drained all tables (after `host.shutdown()`).
    joined: bool,
}

/// The chain host's per-app state: built-in factory, live unit registry, departing owners, waiting
/// reload candidates, per-slot call serialization, editor affinity, parked tone tokens, and shutdown
/// latch.
pub(crate) struct Chains {
    factory: Arc<dyn BuiltinFactory>,
    registry: Mutex<BTreeMap<UnitId, Arc<Entry>>>,
    gone: Mutex<GoneState>,
    pending_reloads: Mutex<Vec<PendingReload>>,
    order: [Mutex<()>; SLOT_COUNT],
    /// Serializes cross-slot `host.unit` reservations against `remove`/`move` slot verification so a
    /// freed id cannot move to the other slot between slot lookup and mutation.
    /// Lock order: `order[slot]` -> `id_guard` -> `slots` -> `host.core.ports` -> `host.core.state`;
    /// separately `registry` -> `pending_reloads` -> `gone` -> `host.core.state` (leaf) / `affinity` (leaf).
    id_guard: Mutex<()>,
    affinity: Mutex<Affinity>,
    tokens: Mutex<ToneTokens>,
    closing: AtomicBool,
    #[cfg(test)]
    fail_next_spawn: AtomicBool,
    #[cfg(test)]
    fail_next_late_drop_spawn: AtomicBool,
    #[cfg(test)]
    before_register_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    before_reload_poll_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_retire_remove_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_take_pending_reload_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_slot_lookup_hook: Mutex<Option<Arc<dyn Fn(usize) + Send + Sync>>>,
}

impl Chains {
    /// Production state: built-ins from `lf_engine::units` ([`Builtins`]).
    pub(crate) fn new() -> Self {
        Self::with_factory(Arc::new(Builtins))
    }

    /// Built-ins from `factory` (the tests' fakes).
    pub(crate) fn with_factory(factory: Arc<dyn BuiltinFactory>) -> Self {
        Chains {
            factory,
            registry: Mutex::new(BTreeMap::new()),
            gone: Mutex::new(GoneState::default()),
            pending_reloads: Mutex::new(Vec::new()),
            order: std::array::from_fn(|_| Mutex::new(())),
            id_guard: Mutex::new(()),
            affinity: Mutex::new(Affinity::default()),
            tokens: Mutex::new(ToneTokens::default()),
            closing: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_spawn: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_late_drop_spawn: AtomicBool::new(false),
            #[cfg(test)]
            before_register_hook: Mutex::new(None),
            #[cfg(test)]
            before_reload_poll_hook: Mutex::new(None),
            #[cfg(test)]
            after_retire_remove_hook: Mutex::new(None),
            #[cfg(test)]
            after_take_pending_reload_hook: Mutex::new(None),
            #[cfg(test)]
            after_slot_lookup_hook: Mutex::new(None),
        }
    }

    /// Resolve any reload candidates held aside, move finished registry entries into `gone`, drop
    /// finished `gone` entries whose last `Arc` lives here (instant join), and release their editor
    /// affinity instances once their owner threads have ended.
    fn sweep(&self, host: Option<&EngineHost>) {
        if let Some(host) = host {
            let checks: Vec<(usize, UnitId, InstallToken, InstallToken, u64, u64)> = {
                let pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
                pending
                    .iter()
                    .map(|c| (c.slot, c.unit, c.old_token, c.candidate_token, c.start_gen, c.inst))
                    .collect()
            };
            for (slot, unit, old_token, candidate_token, start_gen, inst) in checks {
                match reload_outcome(host, unit, old_token, candidate_token, start_gen) {
                    ReloadOutcome::Standing => {}
                    ReloadOutcome::Reloaded => {
                        let _ = self.promote_pending_reload(unit, inst, candidate_token);
                    }
                    ReloadOutcome::Failed(why) => {
                        if self.retire_pending_reload(inst) {
                            log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {why}");
                        }
                    }
                }
            }
        }
        let mut drop_now = Vec::new();
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            let finished: Vec<UnitId> = reg
                .iter()
                .filter_map(|(&id, entry)| entry.owner.is_finished().then_some(id))
                .collect();
            for id in finished {
                if let Some(entry) = reg.remove(&id) {
                    gone.entries.push(entry);
                }
            }
            let mut i = 0;
            while i < gone.entries.len() {
                if gone.entries[i].owner.is_finished() && Arc::strong_count(&gone.entries[i]) == 1 {
                    drop_now.push(gone.entries.swap_remove(i));
                } else {
                    i += 1;
                }
            }
        }
        let release_after: Vec<(u64, String)> = drop_now
            .iter()
            .filter_map(|entry| entry.owner.affinity_handle())
            .collect();
        drop(drop_now);
        if !release_after.is_empty() {
            let mut aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
            for (inst, path) in release_after {
                aff.release(inst, &path);
            }
        }
    }

    /// Hand `entry` to `gone` while holding `gone` (and any source table lock in
    /// `registry -> pending_reloads -> gone` order). If `join_all` already completed, returns
    /// `Some(entry)` so the caller drops all table locks and any slot `order` mutex before calling
    /// [`Self::dispose_late_entry`].
    fn push_gone_locked(gone: &mut GoneState, entry: Arc<Entry>) -> Option<Arc<Entry>> {
        if gone.joined {
            Some(entry)
        } else {
            gone.entries.push(entry);
            None
        }
    }

    /// Stop and drop an entry that arrived after `join_all` already ran. Never called while holding
    /// table locks or a slot's `order` mutex; bounds the wait so a command thread never joins
    /// without a deadline, and leaks the entry rather than dropping it on the caller if spawning
    /// `lf-chain-late-drop` fails.
    fn dispose_late_entry(&self, entry: Arc<Entry>) {
        let aff_handle = entry.owner.affinity_handle();
        entry.owner.request_stop();
        let (entry_tx, entry_rx) = std::sync::mpsc::sync_channel::<Arc<Entry>>(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let task = move || {
            if let Ok(entry) = entry_rx.recv() {
                entry.owner.join();
                drop(entry);
            }
            let _ = done_tx.try_send(());
        };
        #[cfg(test)]
        let spawned = if self.fail_next_late_drop_spawn.swap(false, Acquire) {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "injected late-drop spawn failure"))
        } else {
            std::thread::Builder::new()
                .name("lf-chain-late-drop".to_string())
                .spawn(task)
        };
        #[cfg(not(test))]
        let spawned = std::thread::Builder::new()
            .name("lf-chain-late-drop".to_string())
            .spawn(task);
        let finished = match spawned {
            Ok(_) => match entry_tx.send(entry) {
                Ok(()) => done_rx.recv_timeout(LATE_JOIN_TIMEOUT).is_ok(),
                Err(std::sync::mpsc::SendError(entry)) => {
                    log::error!(
                        "[chain_cmds] lf-chain-late-drop exited before taking slot {} entry; leaking it",
                        entry.slot
                    );
                    std::mem::forget(entry);
                    false
                }
            },
            Err(e) => {
                log::error!(
                    "[chain_cmds] could not spawn lf-chain-late-drop for slot {} entry: {e}; leaking it",
                    entry.slot
                );
                std::mem::forget(entry);
                false
            }
        };
        if let Some((inst, path)) = aff_handle {
            let mut aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
            if finished {
                aff.release(inst, &path);
            } else {
                aff.leak(inst, &path);
            }
        }
    }

    /// Move reload candidate `inst` from `pending_reloads` into `registry` (or into `gone` if
    /// closing or superseded), holding `registry -> pending_reloads -> gone` throughout so neither
    /// `join_all` nor a waiting `reload()` can observe an entry outside the tables. Returns `true`
    /// only when promotion did not install `new_token` (closing, superseded or retired before promotion).
    fn promote_pending_reload(&self, unit: UnitId, inst: u64, new_token: InstallToken) -> bool {
        let (late_entry, aborted) = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let mut pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            let Some(pos) = pending.iter().position(|c| c.inst == inst) else {
                // Another caller already moved the candidate: it landed only if the registry holds it
                // (a sweep that promoted it); a candidate retired meanwhile (a removal, a rebuild,
                // shutdown) never reached the registry, so the reload did not land.
                return !reg.get(&unit).is_some_and(|e| e.token == new_token);
            };
            let cand = pending.swap_remove(pos);
            #[cfg(test)]
            if let Some(hook) = self.after_take_pending_reload_hook.lock().unwrap_or_else(|e| e.into_inner()).take() {
                hook();
            }
            let entry = Arc::new(Entry {
                slot: cand.slot,
                token: new_token,
                owner: Owner::Plugin(PluginOwner {
                    plugin: cand.plugin,
                    inst: cand.inst,
                    owner: cand.owner,
                }),
            });
            if self.closing.load(Acquire) {
                (Self::push_gone_locked(&mut gone, entry), true)
            } else if reg.get(&cand.unit).is_some_and(|e| e.token != cand.old_token) {
                // Another load already replaced `old_token`: retire this candidate rather than clobbering it.
                (Self::push_gone_locked(&mut gone, entry), true)
            } else if let Some(old_entry) = reg.insert(cand.unit, entry) {
                (Self::push_gone_locked(&mut gone, old_entry), false)
            } else {
                (None, false)
            }
        };
        if let Some(late) = late_entry {
            self.dispose_late_entry(late);
        }
        aborted
    }

    fn retire_pending_reload(&self, inst: u64) -> bool {
        let (retired, late_entry) = {
            let mut pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            let Some(pos) = pending.iter().position(|c| c.inst == inst) else {
                return false;
            };
            let cand = pending.swap_remove(pos);
            let entry = Arc::new(Entry {
                slot: cand.slot,
                token: 0,
                owner: Owner::Plugin(PluginOwner {
                    plugin: cand.plugin,
                    inst: cand.inst,
                    owner: cand.owner,
                }),
            });
            (true, Self::push_gone_locked(&mut gone, entry))
        };
        if let Some(late) = late_entry {
            self.dispose_late_entry(late);
        }
        retired
    }

    /// Move `unit`'s registry entry to `gone` if it still matches `token` (or any entry if `None`),
    /// holding `registry -> gone` across the move so `join_all` can never miss it.
    fn retire_to_gone(&self, unit: UnitId, token: Option<InstallToken>) {
        let late_entry = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            let matches = reg.get(&unit).is_some_and(|e| token.is_none_or(|t| e.token == t));
            if matches {
                if let Some(entry) = reg.remove(&unit) {
                    #[cfg(test)]
                    if let Some(hook) = self.after_retire_remove_hook.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        hook();
                    }
                    Self::push_gone_locked(&mut gone, entry)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(late) = late_entry {
            self.dispose_late_entry(late);
        }
    }

    /// Add a unit (built-in or plugin) to `slot` at plate index `at`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
        tones: Option<&ToneStore>,
        slot: usize,
        unit: UnitId,
        spec: UnitSpec,
        at: usize,
        tone_token: Option<u32>,
        sink: EventSink,
        open: Open<'_>,
    ) -> Result<Added, String> {
        current_document(doc.0, doc.1)?;
        self.sweep(Some(host));
        let (spec, bypassed) = spec.into_spec()?;
        match spec {
            Spec::Legacy => Err("legacy unit spec is not valid for chain_add".to_string()),
            Spec::Builtin { kind, params } => {
                self.add_builtin(host, slots, slot, unit, kind, params, at, bypassed)
            }
            Spec::Plugin(plugin) => {
                self.add_plugin(host, slots, tones, slot, unit, plugin, at, bypassed, tone_token, sink, open)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_plugin(
        &self,
        host: &EngineHost,
        slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
        tones: Option<&ToneStore>,
        slot: usize,
        unit: UnitId,
        plugin: PluginRef,
        at: usize,
        bypassed: bool,
        tone_token: Option<u32>,
        sink: EventSink,
        open: Open<'_>,
    ) -> Result<Added, String> {
        let format = check_plugin_format(&plugin)?;
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }

        let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
        if self.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }

        // Lock order: order[slot] -> id_guard -> slots -> host.core.state.
        let unit_host = {
            let _id_guard = self.id_guard.lock().unwrap_or_else(|e| e.into_inner());
            let slots_guard = slots.lock().map_err(|_| "engine slots poisoned".to_string())?;
            for (s, entry) in slots_guard.iter().enumerate() {
                if !matches!(entry, EngineSlot::Empty) {
                    return Err(format!(
                        "a plugin is loaded in slot {s} through the plugin slot: restart the app to use chains"
                    ));
                }
            }
            match host.unit(slot, unit, Spec::Plugin(plugin.clone()), at, bypassed) {
                Ok(u) => u,
                Err(why) => {
                    if !host.core.closing.load(Acquire) && is_unit_leaving(host, unit) {
                        return Err(format!(
                            "unit {unit} is still leaving its chain (a take holds its removal); add it again when the take ends"
                        ));
                    }
                    return Err(why);
                }
            }
        };
        let token = unit_host.install_token();

        let inst = self.affinity.lock().unwrap_or_else(|e| e.into_inner()).claim(&plugin.path);
        // The token's tone this add redeemed into `unit-<id>.tone`: an owner whose add answers `Err` deletes
        // that file (`ToneKeeper::undo_add`), so the tone goes back under its token for a later load.
        let mut redeemed: Option<(u32, Tone)> = None;
        let tone = match tones {
            Some(store) => {
                let key = ToneKey::Unit { unit, slot, identity: plugin.clone() };
                let imported = match tone_token {
                    Some(tok) => {
                        let parked = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).take(tok, &plugin);
                        match parked {
                            Some(t) => match store.redeem(&key, t) {
                                Ok(imp) => {
                                    redeemed = Some((tok, imp.tone.clone()));
                                    Some(imp)
                                }
                                Err(why) => {
                                    log::warn!(
                                        "[chain_cmds] slot {slot} unit {unit}: tone token {tok} redeem failed: {why}; loading without it"
                                    );
                                    None
                                }
                            },
                            None => {
                                log::warn!(
                                    "[chain_cmds] slot {slot} unit {unit}: tone token {tok} unknown or bound to another plugin; loading without it"
                                );
                                None
                            }
                        }
                    }
                    None => None,
                };
                Some(ToneBinding { store: store.clone(), key, imported })
            }
            None => {
                if let Some(tok) = tone_token {
                    log::warn!(
                        "[chain_cmds] slot {slot} unit {unit}: no tone store; ignoring tone token {tok}"
                    );
                }
                None
            }
        };

        match open(format, &plugin, unit_host, sink, tone) {
            Ok((owner, answer)) => {
                self.affinity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .landed(inst, &plugin.path, unit);
                let entry = Arc::new(Entry {
                    slot,
                    token,
                    owner: Owner::Plugin(PluginOwner { plugin, inst, owner }),
                });
                #[cfg(test)]
                if let Some(hook) = self.before_register_hook.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    hook();
                }
                let (outcome_err, late_entry) = {
                    let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                    let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
                    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                    if self.closing.load(Acquire) {
                        (
                            Some("the host is closing".to_string()),
                            Self::push_gone_locked(&mut gone, entry),
                        )
                    } else if !state.table.active_row_for_id(unit).is_some_and(|r| r.token == token) {
                        let why = late_add_refusal_reason(&state, unit, token);
                        (Some(why), Self::push_gone_locked(&mut gone, entry))
                    } else {
                        let late = reg
                            .insert(unit, entry)
                            .and_then(|prev| Self::push_gone_locked(&mut gone, prev));
                        let err = late.as_ref().map(|_| "the host is closing".to_string());
                        (err, late)
                    }
                };
                if let Some(late) = late_entry {
                    drop(order);
                    self.dispose_late_entry(late);
                }
                if let Some(err) = outcome_err {
                    return Err(err);
                }
                match answer {
                    Answer::Seated { .. } => Ok(Added::Seated),
                    Answer::Waiting { .. } => Ok(Added::Waiting),
                    Answer::Unanswered => Err(format!(
                        "the audio engine did not answer within 30 s: unit {unit} stays loading and seats if the engine answers"
                    )),
                }
            }
            Err(e) => {
                {
                    let mut aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
                    if e == SETUP_TIMEOUT_ERR {
                        aff.leak(inst, &plugin.path);
                    } else {
                        aff.release(inst, &plugin.path);
                    }
                }
                if let Some((tok, tone)) = redeemed {
                    self.tokens.lock().unwrap_or_else(|e| e.into_inner()).repark(tok, tone);
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_builtin(
        &self,
        host: &EngineHost,
        slots: &Mutex<[EngineSlot; SLOT_COUNT]>,
        slot: usize,
        unit: UnitId,
        kind: UnitKind,
        params: unit_host::Params,
        at: usize,
        bypassed: bool,
    ) -> Result<Added, String> {
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }

        let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
        if self.closing.load(Acquire) {
            return Err("the host is closing".to_string());
        }

        // Lock order: order[slot] -> id_guard -> slots -> host.core.state.
        let unit_host = {
            let _id_guard = self.id_guard.lock().unwrap_or_else(|e| e.into_inner());
            let slots_guard = slots.lock().map_err(|_| "engine slots poisoned".to_string())?;
            for (s, entry) in slots_guard.iter().enumerate() {
                if !matches!(entry, EngineSlot::Empty) {
                    return Err(format!(
                        "a plugin is loaded in slot {s} through the plugin slot: restart the app to use chains"
                    ));
                }
            }
            match host.unit(slot, unit, Spec::Builtin { kind, params: params.clone() }, at, bypassed) {
                Ok(u) => u,
                Err(why) => {
                    if !host.core.closing.load(Acquire) && is_unit_leaving(host, unit) {
                        return Err(format!(
                            "unit {unit} is still leaving its chain (a take holds its removal); add it again when the take ends"
                        ));
                    }
                    return Err(why);
                }
            }
        };
        let token = unit_host.install_token();

        let Some(terms) = unit_host.terms() else {
            let why = "no audio device is open".to_string();
            unit_host.failed(why.clone());
            unit_host.release();
            return Err(why);
        };

        let (boxed, clamped) = match self.factory.build(kind, &params, terms) {
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

        // Spawn the owner BEFORE submitting to the engine; hand `(unit_host, boxed, terms)` across
        // only AFTER registering in `self.registry` so spawn failure cleanly drops/releases and
        // shutdown can never snapshot `registry` after `submit()` has handed the box to the engine.
        let (start_tx, start_rx) = std::sync::mpsc::sync_channel::<(UnitHost, Unit, unit_host::Terms)>(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<Added, String>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let owner_stop = stop.clone();
        let owner_wake = wake.clone();
        let rebuild = Rebuild { factory: self.factory.clone(), kind, host: host.clone() };
        #[cfg(test)]
        let spawned = if self.fail_next_spawn.swap(false, Acquire) {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "injected spawn failure"))
        } else {
            std::thread::Builder::new()
                .name(format!("lf-builtin-unit-{unit}"))
                .spawn(move || serve_builtin(start_rx, ready_tx, owner_stop, owner_wake, rebuild))
        };
        #[cfg(not(test))]
        let spawned = std::thread::Builder::new()
            .name(format!("lf-builtin-unit-{unit}"))
            .spawn(move || serve_builtin(start_rx, ready_tx, owner_stop, owner_wake, rebuild));

        let handle = match spawned {
            Ok(h) => h,
            Err(e) => {
                drop(boxed);
                let why = format!("could not spawn owner for built-in unit {unit}: {e}");
                unit_host.failed(why.clone());
                unit_host.release();
                return Err(why);
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

        #[cfg(test)]
        if let Some(hook) = self.before_register_hook.lock().unwrap_or_else(|e| e.into_inner()).take() {
            hook();
        }

        enum BuiltinRegErr {
            Closing,
            Inactive(String),
        }
        let (reg_err, late_entry) = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.closing.load(Acquire) {
                (Some(BuiltinRegErr::Closing), Self::push_gone_locked(&mut gone, entry))
            } else if !state.table.active_row_for_id(unit).is_some_and(|r| r.token == token) {
                let why = late_add_refusal_reason(&state, unit, token);
                (Some(BuiltinRegErr::Inactive(why)), Self::push_gone_locked(&mut gone, entry))
            } else {
                let late = reg
                    .insert(unit, entry.clone())
                    .and_then(|prev| Self::push_gone_locked(&mut gone, prev));
                let err = late.as_ref().map(|_| BuiltinRegErr::Closing);
                (err, late)
            }
        };
        if let Some(err) = reg_err {
            // Drop `start_tx` so the idle owner thread exits without ever touching the engine.
            drop(start_tx);
            drop(boxed);
            let msg = match err {
                BuiltinRegErr::Closing => {
                    unit_host.failed("the host is closing".to_string());
                    "the host is closing".to_string()
                }
                BuiltinRegErr::Inactive(why) => why,
            };
            unit_host.release();
            drop(order);
            if let Some(late) = late_entry {
                self.dispose_late_entry(late);
            }
            return Err(msg);
        }

        if let Err(std::sync::mpsc::SendError((unit_host, boxed, _))) = start_tx.send((unit_host, boxed, terms)) {
            drop(boxed);
            let why = format!("built-in unit {unit} owner exited before starting");
            unit_host.failed(why.clone());
            unit_host.release();
            self.retire_to_gone(unit, Some(token));
            return Err(why);
        }

        match ready_rx.recv_timeout(BUILTIN_ADD_TIMEOUT) {
            Ok(Ok(added)) => Ok(added),
            Ok(Err(why)) => {
                self.retire_to_gone(unit, Some(token));
                Err(why)
            }
            Err(_) => Err(format!(
                "built-in unit {unit} did not answer within {} s",
                BUILTIN_ADD_TIMEOUT.as_secs()
            )),
        }
    }

    /// Remove `unit` from its chain. Resolves the slot and unit under the slot's order mutex (and
    /// `id_guard`, so a freed id reused in the other slot cannot be mutated under the wrong lock),
    /// then releases `order` before waiting across a take.
    pub(crate) fn remove(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        self.sweep(Some(host));
        let (order, token, removal) = loop {
            let slot = find_unit_slot(host, unit)?;
            #[cfg(test)]
            if let Some(hook) = self.after_slot_lookup_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                hook(slot);
            }
            let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
            let _id_guard = self.id_guard.lock().unwrap_or_else(|e| e.into_inner());
            let still_in_slot = {
                let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.model.find_plate(unit) {
                    Some((_, p)) if p.legacy => {
                        return Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain"));
                    }
                    Some((s, _)) if s == slot => true,
                    Some(_) => false,
                    None => return Err(format!("unit {unit} not found")),
                }
            };
            if !still_in_slot {
                continue;
            }
            let token = {
                let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                reg.get(&unit).map(|e| e.token)
            };
            let removal = host.remove_unit(unit)?;
            break (order, token, removal);
        };
        let op = match removal {
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

    /// Move `unit` to plate index `to` within its own slot. Resolves the slot and unit under the
    /// slot's order mutex (and `id_guard`), then releases `order` before waiting across a take.
    pub(crate) fn move_unit(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
        to: usize,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        self.sweep(Some(host));
        let (order, op) = loop {
            let slot = find_unit_slot(host, unit)?;
            #[cfg(test)]
            if let Some(hook) = self.after_slot_lookup_hook.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                hook(slot);
            }
            let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
            let _id_guard = self.id_guard.lock().unwrap_or_else(|e| e.into_inner());
            let still_in_slot = {
                let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.model.find_plate(unit) {
                    Some((_, p)) if p.legacy => {
                        return Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain"));
                    }
                    Some((s, _)) if s == slot => true,
                    Some(_) => false,
                    None => return Err(format!("unit {unit} not found")),
                }
            };
            if !still_in_slot {
                continue;
            }
            let op = host.move_unit(unit, to)?;
            break (order, op);
        };
        let state = wait_ordered(host, order, op, REMOVE_TIMEOUT)?;
        match state {
            OpState::Applied => Ok(()),
            OpState::Refused(why) => Err(format!("moving unit {unit} was refused: {}", refusal_reason(why))),
            OpState::Held => Err(format!("moving unit {unit} was interrupted by shutdown")),
            other => Err(format!("moving unit {unit} did not apply ({other:?})")),
        }
    }

    /// On shutdown, while the device still plays: atomically mark closing under the registry locks
    /// (so no add can slip past the snapshot), sweep finished entries, send a removal for every
    /// registered or in-flight non-legacy chain unit, and wait for all of them under one 2 s deadline
    /// (held ops return at once with the host closing). Each removal is the shutdown's
    /// (`EngineHost::remove_unit_at_shutdown`): a plugin unit it ends keeps `unit-<id>.tone` for the next
    /// launch's restore under its id, and the slot's file stays as it was; one already leaving (the
    /// player's removal, a source replacement) or whose removal is refused keeps its own end.
    pub(super) fn unload_all(&self, host: &EngineHost) {
        host.set_closing();
        let mut ids = BTreeSet::new();
        {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
            self.closing.store(true, Release);
            ids.extend(reg.keys().copied());
            ids.extend(pending.iter().map(|c| c.unit));
        }
        self.sweep(Some(host));
        {
            let mut state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
            let rev = state.model.snapshot();
            for chain in rev.chains.iter() {
                for u in &chain.units {
                    ids.insert(u.id);
                }
            }
        }
        let mut ops = Vec::new();
        for id in ids {
            match host.remove_unit_at_shutdown(id) {
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

    /// After `host.shutdown()`: drain `registry`, `pending_reloads` and `gone` atomically (marking
    /// `gone.joined` so any late command knows device teardown already ran), signal stop to every
    /// remaining owner and join their threads.
    pub(super) fn join_all(&self) {
        let mut entries = Vec::new();
        {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let mut pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
            let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
            entries.extend(reg.values().cloned());
            reg.clear();
            for cand in pending.drain(..) {
                entries.push(Arc::new(Entry {
                    slot: cand.slot,
                    token: 0,
                    owner: Owner::Plugin(PluginOwner {
                        plugin: cand.plugin,
                        inst: cand.inst,
                        owner: cand.owner,
                    }),
                }));
            }
            entries.append(&mut gone.entries);
            gone.joined = true;
        }
        for entry in &entries {
            entry.owner.request_stop();
        }
        for entry in &entries {
            entry.owner.join();
        }
        let release_after: Vec<(u64, String)> = entries
            .iter()
            .filter_map(|e| e.owner.affinity_handle())
            .collect();
        drop(entries);
        if !release_after.is_empty() {
            let mut aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
            for (inst, path) in release_after {
                aff.release(inst, &path);
            }
        }
    }

    /// Reload `unit` with a fresh instance of `plugin` applying `tone_token`, if `unit` is still that
    /// seated plugin and was not re-added since `since`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reload(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        tones: Option<&ToneStore>,
        unit: UnitId,
        plugin: PluginRef,
        since: u64,
        tone_token: u32,
        sink: EventSink,
        open: Open<'_>,
    ) -> Result<Reloaded, String> {
        current_document(doc.0, doc.1)?;
        self.sweep(Some(host));
        let format = check_plugin_format(&plugin)?;

        let (slot, order, old, start_gen, unit_host) = loop {
            let slot = {
                let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.model.find_plate(unit) {
                    Some((_, p)) if p.legacy => {
                        return Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain"));
                    }
                    Some((s, _)) => s,
                    None => return Ok(Reloaded::Moved),
                }
            };
            let order = self.order[slot].lock().unwrap_or_else(|e| e.into_inner());
            if self.closing.load(Acquire) {
                return Err("the host is closing".to_string());
            }
            let _id_guard = self.id_guard.lock().unwrap_or_else(|e| e.into_inner());
            let old = {
                let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some((plate_slot, plate)) = state.model.find_plate(unit) else {
                    return Ok(Reloaded::Moved);
                };
                if plate.legacy {
                    return Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain"));
                }
                if plate_slot != slot {
                    continue;
                }
                if plate.kind != UnitKind::Plugin
                    || plate.plugin.as_ref() != Some(&plugin)
                    || plate.state != PlateState::Seated
                {
                    return Ok(Reloaded::Moved);
                }
                match plate.told_seq {
                    Some(told) if told <= since => {}
                    _ => return Ok(Reloaded::Moved),
                }
                match state.table.active_row_for_id(unit) {
                    Some(row) => row.token,
                    None => {
                        log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: active row missing");
                        return Ok(Reloaded::Failed);
                    }
                }
            };
            let start_gen = host.core.engine_gen.load(Acquire);
            let unit_host = match host.reload_unit(unit, plugin.clone()) {
                Ok(u) => u,
                Err(why) => {
                    log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {why}");
                    return Ok(Reloaded::Failed);
                }
            };
            break (slot, order, old, start_gen, unit_host);
        };
        let new_token = unit_host.install_token();

        let Some(store) = tones else {
            let why = "no app-local data folder to keep tones in".to_string();
            log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {why}");
            unit_host.failed(why);
            unit_host.release();
            return Ok(Reloaded::Failed);
        };
        let Some(tone) = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).take(tone_token, &plugin) else {
            let why = format!("tone token {tone_token} is not parked for {}", plugin.path);
            log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {why}");
            unit_host.failed(why);
            unit_host.release();
            return Ok(Reloaded::Failed);
        };
        let key = ToneKey::Unit { unit, slot, identity: plugin.clone() };
        let imported = match store.redeem(&key, tone) {
            Ok(imp) => imp,
            Err(why) => {
                log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: redeem tone: {why}");
                unit_host.failed(why);
                unit_host.release();
                return Ok(Reloaded::Failed);
            }
        };
        let binding = ToneBinding { store: store.clone(), key, imported: Some(imported) };

        let inst = self.affinity.lock().unwrap_or_else(|e| e.into_inner()).claim(&plugin.path);
        match open(format, &plugin, unit_host, sink, Some(binding)) {
            Err(e) => {
                {
                    let mut aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
                    if e == SETUP_TIMEOUT_ERR {
                        aff.leak(inst, &plugin.path);
                    } else {
                        aff.release(inst, &plugin.path);
                    }
                }
                log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {e}");
                Ok(Reloaded::Failed)
            }
            Ok((owner, Answer::Seated { .. })) => {
                self.affinity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .landed(inst, &plugin.path, unit);
                let entry = Arc::new(Entry {
                    slot,
                    token: new_token,
                    owner: Owner::Plugin(PluginOwner { plugin, inst, owner }),
                });
                let (closing, late_entry) = {
                    let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
                    let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
                    if self.closing.load(Acquire) {
                        (true, Self::push_gone_locked(&mut gone, entry))
                    } else {
                        let late = reg
                            .insert(unit, entry)
                            .and_then(|old_entry| Self::push_gone_locked(&mut gone, old_entry));
                        (false, late)
                    }
                };
                if let Some(late) = late_entry {
                    drop(order);
                    self.dispose_late_entry(late);
                }
                if closing {
                    return Err("the host is closing".to_string());
                }
                Ok(Reloaded::Reloaded)
            }
            Ok((owner, Answer::Waiting { .. })) => {
                self.affinity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .landed(inst, &plugin.path, unit);
                let (closing, late_entry) = {
                    let mut pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
                    let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
                    if self.closing.load(Acquire) {
                        let entry = Arc::new(Entry {
                            slot,
                            token: new_token,
                            owner: Owner::Plugin(PluginOwner { plugin, inst, owner }),
                        });
                        (true, Self::push_gone_locked(&mut gone, entry))
                    } else {
                        pending.push(PendingReload {
                            unit,
                            slot,
                            old_token: old,
                            candidate_token: new_token,
                            start_gen,
                            plugin,
                            inst,
                            owner,
                        });
                        (false, None)
                    }
                };
                if closing {
                    drop(order);
                    if let Some(late) = late_entry {
                        self.dispose_late_entry(late);
                    }
                    return Err("the host is closing".to_string());
                }
                drop(order);

                let mut unheld = Duration::ZERO;
                let mut last = Instant::now();
                loop {
                    #[cfg(test)]
                    if let Some(hook) = self.before_reload_poll_hook.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        hook();
                    }
                    match reload_outcome(host, unit, old, new_token, start_gen) {
                        ReloadOutcome::Reloaded => {
                            if self.promote_pending_reload(unit, inst, new_token) {
                                log::warn!(
                                    "[chain_cmds] slot {slot} unit {unit} reload interrupted before promotion"
                                );
                                return Ok(Reloaded::Failed);
                            }
                            return Ok(Reloaded::Reloaded);
                        }
                        ReloadOutcome::Failed(why) => {
                            self.retire_pending_reload(inst);
                            log::warn!("[chain_cmds] slot {slot} unit {unit} reload failed: {why}");
                            return Ok(Reloaded::Failed);
                        }
                        ReloadOutcome::Standing => {}
                    }
                    if self.closing.load(Acquire) || host.core.closing.load(Acquire) {
                        self.retire_pending_reload(inst);
                        log::warn!(
                            "[chain_cmds] slot {slot} unit {unit} reload interrupted by shutdown"
                        );
                        return Ok(Reloaded::Failed);
                    }
                    let now = Instant::now();
                    let dt = now.saturating_duration_since(last);
                    last = now;
                    let slot_held = {
                        let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.model.slot_held(slot)
                    };
                    if !slot_held {
                        unheld += dt;
                        if unheld >= RELOAD_WAIT_TIMEOUT {
                            return Err(format!(
                                "unit {unit} reload did not settle within {} s",
                                RELOAD_WAIT_TIMEOUT.as_secs()
                            ));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok((owner, Answer::Unanswered)) => {
                self.affinity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .landed(inst, &plugin.path, unit);
                let (closing, late_entry) = {
                    let mut pending = self.pending_reloads.lock().unwrap_or_else(|e| e.into_inner());
                    let mut gone = self.gone.lock().unwrap_or_else(|e| e.into_inner());
                    if self.closing.load(Acquire) {
                        let entry = Arc::new(Entry {
                            slot,
                            token: new_token,
                            owner: Owner::Plugin(PluginOwner { plugin, inst, owner }),
                        });
                        (true, Self::push_gone_locked(&mut gone, entry))
                    } else {
                        pending.push(PendingReload {
                            unit,
                            slot,
                            old_token: old,
                            candidate_token: new_token,
                            start_gen,
                            plugin,
                            inst,
                            owner,
                        });
                        (false, None)
                    }
                };
                if closing {
                    drop(order);
                    if let Some(late) = late_entry {
                        self.dispose_late_entry(late);
                    }
                    return Err("the host is closing".to_string());
                }
                Err(format!(
                    "the audio engine did not answer within 30 s: unit {unit} reload stays loading and seats if the engine answers"
                ))
            }
        }
    }

    pub(crate) fn open_editor(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        reject_if_legacy(host, unit)?;
        self.sweep(Some(host));
        let entry = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).cloned()
        }
        .ok_or_else(|| format!("unit {unit} not found"))?;
        match &entry.owner {
            Owner::Builtin(_) => Err(format!("unit {unit} has no plugin editor")),
            Owner::Plugin(p) => {
                {
                    let aff = self.affinity.lock().unwrap_or_else(|e| e.into_inner());
                    aff.check(&p.plugin.path, p.inst)?;
                }
                p.owner.open_editor()
            }
        }
    }

    pub(crate) fn close_editor(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        reject_if_legacy(host, unit)?;
        self.sweep(Some(host));
        let entry = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).cloned()
        };
        let Some(entry) = entry else {
            return Ok(());
        };
        match &entry.owner {
            Owner::Builtin(_) => Ok(()),
            Owner::Plugin(p) => {
                if p.owner.is_finished() {
                    Ok(())
                } else {
                    p.owner.close_editor()
                }
            }
        }
    }

    pub(crate) fn list_params(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<Vec<ParamDesc>, String> {
        current_document(doc.0, doc.1)?;
        reject_if_legacy(host, unit)?;
        self.sweep(Some(host));
        let entry = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).cloned()
        }
        .ok_or_else(|| format!("unit {unit} not found"))?;
        match &entry.owner {
            Owner::Builtin(_) => Err(format!("unit {unit} is not a plugin unit")),
            Owner::Plugin(p) => p.owner.list_params(),
        }
    }

    pub(crate) fn set_param(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
        param_id: u32,
        value: f64,
    ) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        reject_if_legacy(host, unit)?;
        self.sweep(Some(host));
        let entry = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).cloned()
        };
        let Some(entry) = entry else {
            return Ok(());
        };
        if is_unit_leaving(host, unit) {
            return Ok(());
        }
        match &entry.owner {
            Owner::Builtin(_) => Err(format!("unit {unit} is not a plugin unit")),
            Owner::Plugin(p) => {
                if p.owner.is_finished() {
                    Ok(())
                } else {
                    p.owner.set_param(param_id, value)
                }
            }
        }
    }

    pub(crate) fn tone_take(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        unit: UnitId,
    ) -> Result<Vec<u8>, String> {
        current_document(doc.0, doc.1)?;
        reject_if_legacy(host, unit)?;
        self.sweep(Some(host));
        let entry = {
            let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(&unit).cloned()
        }
        .ok_or_else(|| format!("unit {unit} not found"))?;
        match &entry.owner {
            Owner::Builtin(_) => Err(format!("unit {unit} is not a plugin unit")),
            Owner::Plugin(p) => p.owner.take_tone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tone_import(
        &self,
        host: &EngineHost,
        doc: (&PluginHostState, u32),
        tones: Option<&ToneStore>,
        slot: usize,
        bytes: &[u8],
        plugin: &PluginRef,
        hinted_unit: Option<UnitId>,
    ) -> Result<ChainToneImport, String> {
        current_document(doc.0, doc.1)?;
        if slot >= SLOT_COUNT {
            return Err(format!("no slot {slot}"));
        }
        let store = tones.ok_or_else(|| "no app-local data folder to keep tones in".to_string())?;
        let tone = ToneStore::check(bytes, plugin)?;

        let rev = {
            let mut state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
            state.model.snapshot()
        };
        let is_match = |u: &super::chain::UnitState| {
            u.kind == UnitKind::Plugin
                && u.state == PlateState::Seated
                && u.plugin.as_ref() == Some(plugin)
        };
        let mut matches = Vec::new();
        if let Some(hint) = hinted_unit {
            let found = rev
                .chains
                .iter()
                .flat_map(|c| c.units.iter())
                .find(|u| u.id == hint && is_match(u));
            if found.is_some() {
                matches.push(hint);
            }
        }
        for u in &rev.chains[slot].units {
            if is_match(u) && !matches.contains(&u.id) {
                matches.push(u.id);
            }
        }

        let name = tone.name.clone();
        if matches.is_empty() {
            store.import(slot, bytes, plugin)?;
            Ok(ChainToneImport { token: None, name, matches })
        } else {
            let token = self.tokens.lock().unwrap_or_else(|e| e.into_inner()).park(tone);
            Ok(ChainToneImport { token: Some(token), name, matches })
        }
    }

    pub(crate) fn tone_forget(&self, doc: (&PluginHostState, u32), token: u32) -> Result<(), String> {
        current_document(doc.0, doc.1)?;
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).forget(token);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn affinity_held(&self, path: &str) -> bool {
        self.affinity.lock().unwrap_or_else(|e| e.into_inner()).is_held(path)
    }

    #[cfg(test)]
    pub(crate) fn take_parked_tone(&self, token: u32, identity: &PluginRef) -> Option<crate::host::tone::Tone> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).take(token, identity)
    }

    #[cfg(test)]
    pub(crate) fn active_entries(&self) -> usize {
        let r = self.registry.lock().unwrap_or_else(|e| e.into_inner()).len();
        let g = self.gone.lock().unwrap_or_else(|e| e.into_inner()).entries.len();
        r + g
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReloadOutcome {
    Standing,
    Reloaded,
    Failed(String),
}

fn reload_outcome(
    host: &EngineHost,
    unit: UnitId,
    old: InstallToken,
    candidate: InstallToken,
    start_gen: u64,
) -> ReloadOutcome {
    unit_host::service_if_idle(&host.core);
    unit_host::drain_if_stale(&host.core);
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    if state
        .table
        .row(candidate)
        .is_some_and(|r| r.is_reload && !r.released && !r.superseded)
    {
        return ReloadOutcome::Standing;
    }
    if state.table.active_row_for_id(unit).is_some_and(|r| r.token == candidate) {
        return ReloadOutcome::Reloaded;
    }
    let why = if let Some(row) = state.table.row(candidate) {
        if let Some(why) = row.settled.values().find_map(|s| match s {
            OpState::Refused(why) => Some(*why),
            _ => None,
        }) {
            format!("engine refused reload: {}", refusal_reason(why))
        } else if row.settled.values().any(|s| matches!(s, OpState::Rebuilt)) {
            "cancelled by engine rebuild".to_string()
        } else if host.core.closing.load(Acquire) {
            "interrupted by shutdown".to_string()
        } else if host.core.engine_gen.load(Acquire) != start_gen {
            "cancelled by engine rebuild".to_string()
        } else {
            describe_missing_reload_cause(&state, unit, old, candidate)
        }
    } else if host.core.closing.load(Acquire) {
        "interrupted by shutdown".to_string()
    } else if host.core.engine_gen.load(Acquire) != start_gen {
        "cancelled by engine rebuild".to_string()
    } else {
        describe_missing_reload_cause(&state, unit, old, candidate)
    };
    ReloadOutcome::Failed(why)
}

fn describe_missing_reload_cause(
    state: &unit_host::HostState,
    unit: UnitId,
    old: InstallToken,
    candidate: InstallToken,
) -> String {
    match state.model.find_plate(unit) {
        None => "unit was removed or replaced before reload seated".to_string(),
        Some((_, plate)) if plate.state == PlateState::Leaving => {
            "unit removal started before reload seated".to_string()
        }
        Some(_) => {
            if state
                .table
                .active_row_for_id(unit)
                .is_some_and(|r| r.token != candidate && r.token != old)
            {
                "another load superseded the unit before reload seated".to_string()
            } else {
                "reload candidate was refused or cancelled before seating".to_string()
            }
        }
    }
}

fn late_add_refusal_reason(state: &unit_host::HostState, unit: UnitId, token: InstallToken) -> String {
    if let Some(row) = state.table.row(token) {
        if let Some(why) = row.settled.values().find_map(|s| match s {
            OpState::Refused(why) => Some(*why),
            _ => None,
        }) {
            return refusal_reason(why);
        }
        if row.superseded {
            return submit_error_reason(SubmitError::Superseded);
        }
        if row.leave.is_leaving() {
            return submit_error_reason(SubmitError::Removing);
        }
    }
    if let Some((_, plate)) = state.model.find_plate(unit) {
        if plate.state == PlateState::Failed {
            if let Some(err) = &plate.error {
                return err.clone();
            }
        }
    }
    format!("unit {unit} was refused or replaced before registration")
}

fn check_plugin_format(plugin: &PluginRef) -> Result<PluginFormat, String> {
    let format = PluginFormat::of_path(std::path::Path::new(&plugin.path))
        .ok_or_else(|| format!("{}: not a .clap, .vst3 or .dll plugin", plugin.path))?;
    if format.as_str() != plugin.format {
        return Err(format!(
            "{}: plugin format {:?} does not match path extension ({})",
            plugin.path,
            plugin.format,
            format.as_str()
        ));
    }
    Ok(format)
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

fn reject_if_legacy(host: &EngineHost, unit: UnitId) -> Result<(), String> {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.model.find_plate(unit).is_some_and(|(_, p)| p.legacy) {
        return Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain"));
    }
    Ok(())
}

fn find_unit_slot(host: &EngineHost, unit: UnitId) -> Result<usize, String> {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    match state.model.find_plate(unit) {
        Some((_, p)) if p.legacy => Err(format!("unit {unit} belongs to a legacy plugin slot, not a chain")),
        Some((s, _)) => Ok(s),
        None => Err(format!("unit {unit} not found")),
    }
}

fn is_unit_leaving(host: &EngineHost, unit: UnitId) -> bool {
    let state = host.core.state.lock().unwrap_or_else(|e| e.into_inner());
    match state.model.find_plate(unit) {
        Some((_, p)) if p.legacy => false,
        Some((s, p)) => p.state == PlateState::Leaving || state.model.pending_remove_op(s, unit).is_some(),
        None => state.model.has_pending_remove(unit),
    }
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
        SubmitError::Replaced => "a newer source replaced this unit".to_string(),
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

/// What a built-in owner builds its box again from when the engine runs at another rate: the factory
/// that built it, its kind, and the host whose model holds its params as applied.
struct Rebuild {
    factory: Arc<dyn BuiltinFactory>,
    kind: UnitKind,
    host: EngineHost,
}

impl Rebuild {
    /// A box at `terms`' rate with the params the model holds as applied (the engine's last answers;
    /// a rebuild reads the old engine's unanswered ones into it first, `unit_host::rebuild_handoff`),
    /// so a SYNTH keeps its pick and levels. The model's answer is unchanged: the same values again.
    fn build(&self, id: UnitId, terms: unit_host::Terms) -> Result<Unit, String> {
        let params = {
            let state = self.host.core.state.lock().unwrap_or_else(|e| e.into_inner());
            state.model.find_plate(id).and_then(|(_, p)| p.params.clone()).unwrap_or_default()
        };
        self.factory.build(self.kind, &params, terms).map(|(unit, _)| unit)
    }
}

/// Built-in unit owner loop (`lf-builtin-unit-{id}`): receives its host and box after registration,
/// performs the first submit, answers the caller once (`Seated` / `Waiting` / `Err`), resubmits the
/// same box after evictions/withdrawals at the rate it was built at, builds it again from its applied
/// params for an engine at another rate (its DSP is built for one rate), parks on post-seat refusal
/// or a failed rebuild, drops the box off the RT thread on terminal return, and drains/abandons
/// cleanly on stop.
fn serve_builtin(
    start_rx: std::sync::mpsc::Receiver<(UnitHost, Unit, unit_host::Terms)>,
    ready_tx: SyncSender<Result<Added, String>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    rebuild: Rebuild,
) {
    let Ok((host, boxed, initial_terms)) = start_rx.recv() else {
        return;
    };
    let mut built_rate = initial_terms.rate;
    let mut ready = Some(ready_tx);
    let mut ever_seated = false;
    let initial_deadline = Instant::now() + BUILTIN_INITIAL_SUBMIT_TIMEOUT;

    let mut seat = match host.submit(boxed, initial_terms) {
        Ok(op) => BuiltinSeat::Installing { op },
        Err((returned, SubmitError::Stale | SubmitError::Reloading)) => BuiltinSeat::Owed { unit: returned },
        Err((returned, SubmitError::Closing)) => {
            drop(returned);
            host.release();
            send_first_answer(&mut ready, Err("the host is closing".to_string()));
            return;
        }
        Err((returned, err)) => {
            // PortFull, NoEngine, Removing, Superseded on initial submit: fail immediately so the
            // caller releases the slot's order mutex and no background retry can enqueue out of order.
            drop(returned);
            let why = submit_error_reason(err);
            if !matches!(err, SubmitError::Superseded | SubmitError::Replaced | SubmitError::Removing) {
                host.failed(why.clone());
            }
            host.release();
            send_first_answer(&mut ready, Err(why));
            return;
        }
    };

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
            if !ever_seated && Instant::now() >= initial_deadline {
                drop(unit);
                let why = "built-in unit could not submit before timeout".to_string();
                host.failed(why.clone());
                host.release();
                send_first_answer(&mut ready, Err(why));
                return;
            }
            let terms = host.terms();
            let unit = match terms {
                Some(terms) if terms.rate != built_rate => match rebuild.build(host.id(), terms) {
                    Ok(rebuilt) => {
                        drop(unit);
                        built_rate = terms.rate;
                        rebuilt
                    }
                    Err(why) => {
                        let why = format!("could not build the unit again at {} Hz: {why}", terms.rate);
                        log::warn!("[chain_cmds] built-in unit {}: {why}", host.id());
                        if !ever_seated {
                            drop(unit);
                            host.failed(why.clone());
                            host.release();
                            send_first_answer(&mut ready, Err(why));
                            return;
                        }
                        host.parked(why);
                        seat = BuiltinSeat::Parked { unit };
                        continue;
                    }
                },
                _ => unit,
            };
            match terms {
                None => {
                    if !ever_seated {
                        drop(unit);
                        let why = "no audio device is open".to_string();
                        host.failed(why.clone());
                        host.release();
                        send_first_answer(&mut ready, Err(why));
                        return;
                    }
                    seat = BuiltinSeat::Owed { unit };
                }
                Some(terms) => match host.submit(unit, terms) {
                    Ok(op) => {
                        seat = BuiltinSeat::Installing { op };
                    }
                    Err((returned, SubmitError::PortFull | SubmitError::NoEngine)) if !ever_seated => {
                        drop(returned);
                        let why = "the slot's engine port is full".to_string();
                        host.failed(why.clone());
                        host.release();
                        send_first_answer(&mut ready, Err(why));
                        return;
                    }
                    Err((returned, SubmitError::Stale | SubmitError::NoEngine | SubmitError::PortFull | SubmitError::Reloading)) => {
                        seat = BuiltinSeat::Owed { unit: returned };
                    }
                    Err((returned, SubmitError::Removing)) => {
                        if !ever_seated {
                            drop(returned);
                            host.release();
                            send_first_answer(&mut ready, Err(submit_error_reason(SubmitError::Removing)));
                            return;
                        }
                        seat = BuiltinSeat::Removing { unit: returned };
                    }
                    Err((returned, SubmitError::Closing)) => {
                        if !ever_seated {
                            drop(returned);
                            host.release();
                            send_first_answer(&mut ready, Err("the host is closing".to_string()));
                            return;
                        }
                        seat = BuiltinSeat::Closing { unit: returned };
                    }
                    Err((returned, err @ (SubmitError::Superseded | SubmitError::Replaced))) => {
                        drop(returned);
                        host.release();
                        send_first_answer(&mut ready, Err(submit_error_reason(err)));
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

        let poll = if ready.is_some() { Duration::from_millis(1) } else { BUILTIN_POLL };
        let guard = wake.0.lock().unwrap_or_else(|e| e.into_inner());
        if !stop.load(Acquire) {
            let _ = wake.1.wait_timeout(guard, poll);
        }
    }

    // Stop requested: drain anything returned (or held in Leave::Pending), waiting up to
    // `BUILTIN_STOP_RETURN_TIMEOUT` while an engine still runs so a removed unit finishing its
    // fade-out is collected rather than abandoned, then release the reservation.
    let pending_op = match seat {
        BuiltinSeat::Installing { op } => Some(op),
        _ => None,
    };
    let mut have_box = seat.take_unit().is_some();
    let mut dismissed = false;
    let drain_shutdown = |have_box: &mut bool, dismissed: &mut bool| {
        while let Some(back) = host.take_back_for_shutdown() {
            match back {
                Back::Removed(u)
                | Back::Superseded(u)
                | Back::Withdrawn { unit: u, .. }
                | Back::Evicted { unit: u, .. }
                | Back::Refused { unit: u, .. } => {
                    drop(u);
                    *have_box = true;
                }
                Back::Dismissed => {
                    *dismissed = true;
                }
            }
        }
    };
    if let Some(op) = pending_op {
        let _ = host.poll(op);
    }
    drain_shutdown(&mut have_box, &mut dismissed);
    if !have_box && !dismissed && host.terms().is_some() && host.removal_in_flight() {
        let deadline = Instant::now() + BUILTIN_STOP_RETURN_TIMEOUT;
        while !have_box && !dismissed && Instant::now() < deadline {
            if let Some(op) = pending_op {
                let _ = host.poll(op);
            }
            drain_shutdown(&mut have_box, &mut dismissed);
            if have_box || dismissed || host.terms().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drain_shutdown(&mut have_box, &mut dismissed);
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

fn window_sink(window: tauri::WebviewWindow, unit: UnitId) -> EventSink {
    Arc::new(move |event| {
        let (name, payload) = chain_event(unit, event);
        let _ = window.emit(name, payload);
    })
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
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<Added, String> {
    blocking(app, "chain_add", move |app, state| {
        let host = app.host()?;
        let parent = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
        let sink = window_sink(window, unit);
        let open = |format: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit_host, parent, sink, tone)
        };
        let added = app.chains.add(
            &host,
            (state, frontend_epoch),
            &app.slots,
            app.tones.as_ref(),
            usize::from(slot),
            unit,
            spec,
            at,
            tone_token,
            sink,
            &open,
        );
        // The slot's seated SYNTH may have changed (a source replaces the head; the id may be one a
        // removed SYNTH had): native MIDI's router forgets the picks it kept, whatever the outcome, so the
        // next pick is sent (an extra one only re-sends what the SYNTH already plays).
        forget_picks(app, Some(slot), unit);
        added
    })
    .await
}

/// A slot's seated SYNTH may have changed: native MIDI's router forgets the picks it kept for `slot` and
/// for `unit` (`MidiHost::forget_picks`). Nothing when native MIDI does not run, nor in a release build
/// (no pick there; the `chain_*` commands are DEV-only too).
fn forget_picks(app: &EngineApp, slot: Option<u8>, unit: UnitId) {
    #[cfg(debug_assertions)]
    let _ = app.with_midi(|midi| midi.forget_picks(slot, unit));
    #[cfg(not(debug_assertions))]
    let _ = (app, slot, unit);
}

/// Remove `unit`; settles on the removal's `Applied` (a failed add's plate goes at once).
#[tauri::command]
pub async fn chain_remove(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_remove", move |app, state| {
        let host = app.host()?;
        let removed = app.chains.remove(&host, (state, frontend_epoch), unit);
        forget_picks(app, None, unit);
        removed
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
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<Reloaded, String> {
    blocking(app, "chain_reload", move |app, state| {
        let host = app.host()?;
        let parent = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
        let sink = window_sink(window, unit);
        let open = |format: PluginFormat, plugin: &PluginRef, unit_host: UnitHost, sink: EventSink, tone: Option<ToneBinding>| {
            unit_owner::load_unit(format, plugin.path.clone(), plugin.id.clone(), unit_host, parent, sink, tone)
        };
        app.chains.reload(
            &host,
            (state, frontend_epoch),
            app.tones.as_ref(),
            unit,
            plugin,
            since,
            tone_token,
            sink,
            &open,
        )
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
    blocking(app, "chain_open_editor", move |app, state| {
        let host = app.host()?;
        app.chains.open_editor(&host, (state, frontend_epoch), unit)
    })
    .await
}

/// Close `unit`'s editor. Idempotent.
#[tauri::command]
pub async fn chain_close_editor(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_close_editor", move |app, state| {
        let host = app.host()?;
        app.chains.close_editor(&host, (state, frontend_epoch), unit)
    })
    .await
}

/// `unit`'s plugin parameters (stable ids, ranges, live values), from its owner thread.
#[tauri::command]
pub async fn chain_list_params(
    unit: UnitId,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<Vec<ParamDesc>, String> {
    blocking(app, "chain_list_params", move |app, state| {
        let host = app.host()?;
        app.chains.list_params(&host, (state, frontend_epoch), unit)
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
    blocking(app, "chain_set_param", move |app, state| {
        let host = app.host()?;
        app.chains.set_param(&host, (state, frontend_epoch), unit, param_id, value)
    })
    .await
}

/// Save `unit`'s tone now, through its owner, and answer the tone file's bytes as the raw response
/// (empty: the plugin keeps no state).
#[tauri::command]
pub async fn chain_tone_take(
    unit: UnitId,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<tauri::ipc::Response, String> {
    let bytes: Vec<u8> = blocking(app, "chain_tone_take", move |app, state| {
        let host = app.host()?;
        app.chains.tone_take(&host, (state, frontend_epoch), unit)
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
    blocking(app, "chain_tone_import", move |app, state| {
        let host = app.host()?;
        app.chains
            .tone_import(&host, (state, epoch), app.tones.as_ref(), usize::from(slot), &bytes, &plugin, unit)
    })
    .await
}

/// The reload a tone import answered `token` for did not happen: drop the tone parked for it.
#[tauri::command]
pub async fn chain_tone_forget(token: u32, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_tone_forget", move |app, state| {
        app.chains.tone_forget((state, frontend_epoch), token)
    })
    .await
}
