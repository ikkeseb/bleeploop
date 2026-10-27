//! OWNS: engine mode's plugin slots: the
//! `plugin_*` load, unload, list, parameter and editor commands, routed to the engine slot owners
//! (`host/engine_slot.rs`) with their window events (`plugin:param-changed`,
//! `plugin:params-changed`, `plugin:editor-closed`), and tone recall's commands (`host/tone.rs`): every
//! load restores the plugin's stored tone, a session export takes a slot's tone fresh, an import stores
//! one (checked against the plugin session.json names; every load of that plugin from before it stops
//! saving, and the reload of the slot that held it is handed the imported bytes under a reload token),
//! and the app's exit saves them all before the unloads. A plugin loads into an engine that exists:
//! open the device first. GO LIVE is the engine's `Toggle::Live` and `SetSlotLive` and the plugin's gain
//! `SetSlotGain` (sent in `input_send`'s batch); notes go through native MIDI's router (`midi_mode`).
//!
//! A slot is reserved while it loads, by the load that asked, for the WebView document that asked (its
//! `frontendEpoch`); only that load moves the slot on. A load that finishes for a replaced document, or
//! one an unload cancelled meanwhile, unloads what it loaded, and the slot stays out of reach until
//! the plugin's unit has left the engine; a load for the new document waits for it (≤ `VACATE_WAIT`).
//! A setup past its bound frees the slot with no unit left in it (`engine_slot::LoadClaim`). A loaded
//! plugin's unload takes its slot off live first (`silence`), and refuses when it cannot. An unload
//! that gives up on a shared handle (`unload_handle`) frees it early; `src-tauri/OPEN-THREADS.md`.
//! Every request that changes a slot (load, unload, a parameter, the editor) names its document and is
//! refused from a replaced one, checked under the slots lock as it takes the slot. Commands clone a
//! loaded slot's handle out of the lock, so a slow owner round trip on one slot holds up nobody else.
//!
//! A hot swap (`swap`) loads the next plugin while the slot's plugin plays (`Swapping`, where requests
//! reach the playing one), and the plugin it replaced retires on a thread of its own (inline, when none
//! starts). Until that retire has ended, no load or swap takes the slot (≤ `VACATE_WAIT`): the next pick starts once the
//! earlier instance is gone.

use std::sync::atomic::{AtomicBool, Ordering::{Relaxed, Release}};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lf_engine::{Command, SlotKind, TimedCommand};
use tauri::Emitter;

use super::mode::EngineApp;
use super::{EngineHost, SlotHost};
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, Entry, EventSink, PluginFormat};
use crate::host::tone::{ToneBinding, ToneHandoff, ToneIdentity, ToneStore};
use crate::host::{ParamDesc, PluginDescriptor, PluginHostState, PluginInfo, ToneImport};

/// How long an unload waits for a command still using the slot's handle (an owner round trip is ≤ 5 s).
const UNLOAD_WAIT: Duration = Duration::from_secs(6);

/// How long an unload waits for its slot's live gate to close before the unit leaves the engine.
const LIVE_OFF_WAIT: Duration = Duration::from_secs(1);

/// How long the exit waits, in all, for the slots' tone saves before it unloads them anyway.
const EXIT_SAVE_WAIT: Duration = Duration::from_secs(2);

/// How long a load waits for a slot that is still clearing: a replaced document's load runs out its
/// setup (≤ 15 s) and its teardown, an unload its teardown (a plugin's release can take seconds).
const VACATE_WAIT: Duration = Duration::from_secs(30);

/// How often a load waiting for its slot looks again.
const VACATE_POLL: Duration = Duration::from_millis(10);

mod swap;

type Slots = Mutex<[SlotEntry; lf_engine::SLOT_COUNT]>;
type SlotsGuard<'a> = MutexGuard<'a, [SlotEntry; lf_engine::SLOT_COUNT]>;

/// One slot: what it holds, and the owner a hot swap replaced while that owner still tears down.
#[derive(Default)]
pub(crate) struct SlotEntry {
    state: EngineSlot,
    /// The replaced owner's retire (`swap::spawn_retire`), until the slot's next reservation reaps it,
    /// or until the swap that retires it inline clears it.
    retiring: Option<Retiring>,
}

/// A replaced owner's stop and join: on its own thread (`join`: `true` once it released its plugin in
/// order), or, with none started (`None`), inline on the swap's own thread.
struct Retiring {
    instance: u64,
    join: Option<JoinHandle<bool>>,
}

impl SlotEntry {
    /// The instance a swap replaced here while its retire still runs (an inline one runs until its swap
    /// clears it); a finished retire thread is reaped.
    fn retiring(&mut self) -> Option<u64> {
        match &self.retiring {
            Some(Retiring { instance, join }) if !join.as_ref().is_some_and(JoinHandle::is_finished) => Some(*instance),
            _ => {
                if let Some(Retiring { join: Some(join), .. }) = self.retiring.take() {
                    let _ = join.join();
                }
                None
            }
        }
    }

    /// The plugin the slot plays (loaded, or playing on while a swap loads the next) is `identity`'s.
    fn plays(&self, identity: &ToneIdentity) -> bool {
        matches!(&self.state, EngineSlot::Loaded { info, .. } | EngineSlot::Swapping { info, .. } if {
            let d = &info.descriptor;
            d.format == identity.format && d.path == identity.path && d.id == identity.id
        })
    }
}

#[derive(Default)]
pub(crate) enum EngineSlot {
    #[default]
    Empty,
    /// A load for the document with this epoch is running. `cancelled`: an unload asked for the slot
    /// meanwhile, so the load unloads what it loads.
    Loading { epoch: u32, cancelled: bool },
    Loaded { handle: Arc<EngineSlotHandle>, info: PluginInfo },
    /// A hot swap for the document with this epoch loads the next plugin while `playing` plays; `cancel`
    /// set calls it off (an unload). Only the swap's `finish` moves the slot on, and whoever moves it out
    /// of `Swapping` owns `playing` from then on.
    Swapping { epoch: u32, playing: Arc<EngineSlotHandle>, info: PluginInfo, cancel: Arc<AtomicBool> },
    /// A plugin is on its way out (an unload, or a load nobody wants any more): its unit is in the
    /// engine until the teardown ends, so no load may take the slot before that.
    Leaving,
}

/// The `plugin:param-changed` payload: the instance (`EngineSlotHandle::instance`) whose plugin moved it.
#[derive(Clone, serde::Serialize)]
struct ParamChanged {
    slot: u8,
    instance: u64,
    id: u32,
    value: f64,
}

/// The `plugin:params-changed` and `plugin:editor-closed` payload: the instance it happened to.
#[derive(Clone, serde::Serialize)]
struct SlotInstance {
    slot: u8,
    instance: u64,
}

/// What a load runs once its slot is reserved, its unit entering as the `Entry` says: `engine_slot::load`
/// with the window's parent and events (`window_open`), a test's stand-in owner otherwise.
type Open<'a> = &'a dyn Fn(PluginFormat, String, String, SlotHost, Option<ToneBinding>, Entry) -> Result<EngineSlotHandle, String>;

/// Where a load lands and how it opens the plugin: the app's slots, tone store and engine.
struct LoadTarget<'a> {
    slots: &'a Slots,
    reload_tones: &'a Mutex<ToneHandoff<{ lf_engine::SLOT_COUNT }>>,
    tones: Option<ToneStore>,
    host: &'a EngineHost,
    open: Open<'a>,
    /// How long the reservation waits for a slot that is still clearing (`VACATE_WAIT`).
    vacate_wait: Duration,
}

impl LoadTarget<'_> {
    fn load(
        self,
        state: &PluginHostState,
        slot: u8,
        path: String,
        id: String,
        frontend_epoch: u32,
        tone_token: Option<u32>,
    ) -> Result<PluginInfo, String> {
        // Before the reservation: a path that names no format must leave the slot as it was.
        let format = format_of(&path)?;
        self.reserve(state, slot, frontend_epoch)?;
        // Past the reservation, so nothing below may return early.
        let tone = self.tone(slot, format, &path, &id, tone_token);
        let began = Instant::now();
        let loaded = (self.open)(format, path.clone(), id.clone(), self.host.slot(usize::from(slot)), tone, Entry::Install);
        let mut slots = lock(self.slots)?;
        let taken = &mut slots[usize::from(slot)].state;
        // The reservation is this load's (nothing else moves a slot out of `Loading`); whether its
        // document still wants the plugin is another question.
        let wanted = matches!(taken, EngineSlot::Loading { cancelled: false, .. }) && state.frontend_epoch.load(Relaxed) == frontend_epoch;
        let handle = match loaded {
            Ok(handle) => handle,
            Err(e) => {
                // A setup that timed out had not claimed its load, so its owner never installs
                // (`engine_slot::LoadClaim`); one that had is a load that stands. Only a claimed load
                // whose report then took 2 s more leaves a unit to undo (`src-tauri/OPEN-THREADS.md`).
                *taken = EngineSlot::Empty;
                return Err(e);
            }
        };
        if !wanted {
            // Its unit is in the engine until the unload ends: a load for the new document waits.
            *taken = EngineSlot::Leaving;
            drop(slots);
            if let Err(e) = handle.unload() {
                log::error!("[plugin_host] engine slot {slot}: unloading a superseded load: {e}");
            }
            vacate(self.slots, slot);
            return Err(format!("plugin load for slot {slot} was superseded by a frontend reload or an unload"));
        }
        let info = describe(slot, format, path, id, &handle);
        log::info!("[plugin_host] engine slot {slot}: {} ({:?}) loaded in {} ms", info.descriptor.name, handle.kind(), began.elapsed().as_millis());
        *taken = EngineSlot::Loaded { handle: Arc::new(handle), info: info.clone() };
        Ok(info)
    }

    /// Reserve `slot` for this load: at once when it is empty; after a wait (≤ `vacate_wait`) while a
    /// replaced document's load, a cancelled load, an unload or a swapped-out plugin's retire still
    /// clears it. Refused for a replaced document, and while the slot holds a plugin or the current
    /// document's own load.
    fn reserve(&self, state: &PluginHostState, slot: u8, frontend_epoch: u32) -> Result<(), String> {
        take_slot(self.slots, slot, (state, frontend_epoch), self.vacate_wait, |taken, retiring| match *taken {
            EngineSlot::Empty if !retiring => {
                *taken = EngineSlot::Loading { epoch: frontend_epoch, cancelled: false };
                Some(Ok(()))
            }
            EngineSlot::Loading { epoch, cancelled: false } if epoch == frontend_epoch => Some(Err(format!("slot {slot} is already loading a plugin"))),
            EngineSlot::Loaded { .. } | EngineSlot::Swapping { .. } => Some(Err(format!("slot {slot} already has a plugin loaded"))),
            EngineSlot::Empty | EngineSlot::Loading { .. } | EngineSlot::Leaving => None,
        })
    }

    /// The tone binding a load of plugin `id` from `path` into `slot` restores and saves through, with
    /// the session tone parked under `tone_token` for this reload. A poisoned lock hands nothing over.
    fn tone(&self, slot: u8, format: PluginFormat, path: &str, id: &str, tone_token: Option<u32>) -> Option<ToneBinding> {
        let identity = ToneIdentity { format: format.as_str().to_string(), path: path.to_string(), id: id.to_string() };
        let imported = self.reload_tones.lock().ok().and_then(|mut tones| tones.take(usize::from(slot), tone_token, &identity));
        self.tones.clone().map(|store| ToneBinding { store, slot: usize::from(slot), identity, imported })
    }
}

/// The plugin format `path` names: refused before any reservation, so the slot stays as it was.
fn format_of(path: &str) -> Result<PluginFormat, String> {
    PluginFormat::of_path(std::path::Path::new(path)).ok_or_else(|| format!("{path}: not a .clap, .vst3 or .dll plugin"))
}

/// What a load answers for `handle`, opened from `path` as `format`.
fn describe(slot: u8, format: PluginFormat, path: String, id: String, handle: &EngineSlotHandle) -> PluginInfo {
    let descriptor = PluginDescriptor {
        id,
        name: handle.name().to_string(),
        format: format.as_str().to_string(),
        path,
        // The frontend keeps the scan's descriptor for gain staging.
        is_effect: None,
    };
    PluginInfo { slot, descriptor, tone: handle.tone(), instance: handle.instance(), effect: matches!(handle.kind(), SlotKind::Effect) }
}

/// Look at `slot` under the lock, for the document `document` names, until `take` settles it (`Some`)
/// from what the slot holds and whether a swapped-out plugin still retires there: every `VACATE_POLL`,
/// up to `wait`, then refused as still clearing.
fn take_slot<T>(
    slots: &Slots,
    slot: u8,
    (state, frontend_epoch): (&PluginHostState, u32),
    wait: Duration,
    mut take: impl FnMut(&mut EngineSlot, bool) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let deadline = Instant::now() + wait;
    loop {
        let retiring = {
            let mut slots = lock(slots)?;
            current_document(state, frontend_epoch)?;
            let entry = &mut slots[usize::from(slot)];
            let retiring = entry.retiring();
            if let Some(taken) = take(&mut entry.state, retiring.is_some()) {
                return taken;
            }
            retiring
        };
        if Instant::now() >= deadline {
            return Err(match retiring {
                Some(instance) => format!("slot {slot} is still releasing the plugin it played before (instance {instance})"),
                None => format!("slot {slot} is still clearing the plugin before it"),
            });
        }
        std::thread::sleep(VACATE_POLL);
    }
}

fn lock(slots: &Slots) -> Result<SlotsGuard<'_>, String> {
    slots.lock().map_err(|_| "engine slots poisoned".to_string())
}

/// A request from a replaced WebView document is refused: its `frontendEpoch` is no longer the one
/// `host_init` last answered. Checked under the slots lock, as the request takes its slot.
fn current_document(state: &PluginHostState, frontend_epoch: u32) -> Result<(), String> {
    let current = state.frontend_epoch.load(Relaxed);
    if frontend_epoch == current {
        Ok(())
    } else {
        Err(format!("stale frontend epoch {frontend_epoch} (current {current})"))
    }
}

/// The slot's handle while it holds a loaded plugin (`None` otherwise). With `document` (the host state
/// and the request's `frontendEpoch`), only for a request from the current document.
fn loaded(slots: &Slots, slot: u8, document: Option<(&PluginHostState, u32)>) -> Result<Option<Arc<EngineSlotHandle>>, String> {
    let slots = lock(slots)?;
    if let Some((state, frontend_epoch)) = document {
        current_document(state, frontend_epoch)?;
    }
    Ok(match &slots[usize::from(slot)].state {
        EngineSlot::Loaded { handle, .. } | EngineSlot::Swapping { playing: handle, .. } => Some(handle.clone()),
        _ => None,
    })
}

/// The slot's handle (`loaded`) for a request naming `instance`: refused `stale instance` while the
/// slot plays another (a swap replaced the one named, or the slot reloaded), so the request never
/// reaches a plugin it was not meant for.
fn playing(slots: &Slots, slot: u8, instance: u64, document: Option<(&PluginHostState, u32)>) -> Result<Option<Arc<EngineSlotHandle>>, String> {
    match loaded(slots, slot, document)? {
        Some(handle) if handle.instance() != instance => Err("stale instance".to_string()),
        handle => Ok(handle),
    }
}

/// Take `slot`'s plugin out of the engine and unload it, for the document `document` names (`None`:
/// the app's exit). A load still running is cancelled and unloads what it loads, a swap is cancelled
/// and its thread unloads both plugins; the slot is `Leaving` until this teardown ends. With `host`, a
/// loaded plugin's slot goes off live first, its gate closed (`silence`), and the plugin stays loaded
/// when that fails.
fn unload(slots: &Slots, slot: u8, document: Option<(&PluginHostState, u32)>, host: Option<&EngineHost>) -> Result<(), String> {
    if let Some(host) = host {
        silence(slots, slot, document, host).map_err(|e| format!("the plugin in slot {slot} stays loaded, as its live input could not be stopped first: {e}"))?;
    }
    let handle = {
        let mut slots = lock(slots)?;
        if let Some((state, frontend_epoch)) = document {
            current_document(state, frontend_epoch)?;
        }
        let taken = &mut slots[usize::from(slot)].state;
        match std::mem::replace(taken, EngineSlot::Leaving) {
            EngineSlot::Loaded { handle, .. } => handle,
            EngineSlot::Loading { epoch, .. } => {
                *taken = EngineSlot::Loading { epoch, cancelled: true };
                return Ok(());
            }
            other => {
                if let EngineSlot::Swapping { cancel, .. } = &other {
                    cancel.store(true, Release);
                }
                *taken = other;
                return Ok(());
            }
        }
    };
    let unloaded = unload_handle(slot, handle);
    vacate(slots, slot);
    unloaded
}

/// Before a loaded plugin's final unload: take its slot off live and wait for the engine's gate to close
/// (`EngineHost::silence_slot`). Removed live, the unit would crossfade toward the gated input, and an
/// empty live slot passes the dry input on to the monitor and the record path. A plugin-requested
/// restart removes the unit with the slot still live (`engine_slot::cycle`): it comes back.
fn silence(slots: &Slots, slot: u8, document: Option<(&PluginHostState, u32)>, host: &EngineHost) -> Result<(), String> {
    let loaded = {
        let slots = lock(slots)?;
        if let Some((state, frontend_epoch)) = document {
            current_document(state, frontend_epoch)?;
        }
        matches!(slots[usize::from(slot)].state, EngineSlot::Loaded { .. } | EngineSlot::Swapping { .. })
    };
    if loaded { host.silence_slot(slot, LIVE_OFF_WAIT) } else { Ok(()) }
}

/// A command may still hold the handle for an owner round trip: wait it out (≤ `UNLOAD_WAIT`), then
/// unload here; past that, the last user unloads it, after the caller has already vacated the slot
/// (`src-tauri/OPEN-THREADS.md`).
fn unload_handle(slot: u8, mut handle: Arc<EngineSlotHandle>) -> Result<(), String> {
    let deadline = Instant::now() + UNLOAD_WAIT;
    loop {
        match Arc::try_unwrap(handle) {
            Ok(handle) => return handle.unload(),
            Err(shared) if Instant::now() >= deadline => {
                log::warn!("[plugin_host] engine slot {slot}: still in use; the last user unloads it");
                drop(shared);
                return Ok(());
            }
            Err(shared) => {
                handle = shared;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// The teardown that made `slot` `Leaving` has ended: a load may take it again.
fn vacate(slots: &Slots, slot: u8) {
    let mut slots = slots.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let taken = &mut slots[usize::from(slot)].state;
    if matches!(taken, EngineSlot::Leaving) {
        *taken = EngineSlot::Empty;
    }
}

/// How the app opens a plugin for `window` into `slot`: `engine_slot::load` with the window's events,
/// each naming the instance this load makes (its owner's tag), and its HWND as a floating CLAP
/// editor's transient parent (the editor host window is owner-less).
fn window_open(window: &tauri::WebviewWindow, slot: u8) -> impl Fn(PluginFormat, String, String, SlotHost, Option<ToneBinding>, Entry) -> Result<EngineSlotHandle, String> {
    let window = window.clone();
    let parent = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
    move |format, path, id, slot_host, tone, entry| {
        let (events, instance) = (window.clone(), slot_host.tag());
        let sink: EventSink = Arc::new(move |event| {
            let _ = match event {
                EngineSlotEvent::ParamChanged { id, value } => events.emit("plugin:param-changed", ParamChanged { slot, instance, id, value }),
                EngineSlotEvent::ParamsChanged => events.emit("plugin:params-changed", SlotInstance { slot, instance }),
                EngineSlotEvent::EditorClosed => events.emit("plugin:editor-closed", SlotInstance { slot, instance }),
            };
        });
        engine_slot::load(format, path, id, slot_host, parent, sink, tone, entry)
    }
}

impl EngineApp {
    fn slots(&self) -> Result<SlotsGuard<'_>, String> {
        lock(&self.slots)
    }

    /// Where a load into the app's slots lands, opened by `open`.
    fn load_target<'a>(&'a self, host: &'a EngineHost, open: Open<'a>) -> LoadTarget<'a> {
        LoadTarget { slots: &self.slots, reload_tones: &self.reload_tones, tones: self.tones.clone(), host, open, vacate_wait: VACATE_WAIT }
    }

    /// Per slot: the tone a session import stored for the plugin the slot held, parked for the reload
    /// that follows (`ToneHandoff`). Taken after `slots` when both are held.
    fn reload_tones(&self) -> Result<MutexGuard<'_, ToneHandoff<{ lf_engine::SLOT_COUNT }>>, String> {
        self.reload_tones.lock().map_err(|_| "engine reload tones poisoned".to_string())
    }

    /// The slot's handle for a request naming `instance` (`playing`); with `document`, for a request
    /// from the current document only (`loaded`).
    fn handle(&self, slot: u8, instance: u64, document: Option<(&PluginHostState, u32)>) -> Result<Arc<EngineSlotHandle>, String> {
        playing(&self.slots, slot, instance, document)?.ok_or_else(|| format!("no plugin loaded in slot {slot}"))
    }

    /// `plugin_load`: load plugin `id` from `path` into the engine's `slot` (≤ 15 s) for the document
    /// with `frontend_epoch`. `tone_token`: the reload token a session import answered, when this load is
    /// that reload; it restores the imported tone parked under it. Any load drops what was parked.
    pub(crate) fn plugin_load(
        &self,
        state: &PluginHostState,
        window: &tauri::WebviewWindow,
        slot: u8,
        path: String,
        id: String,
        frontend_epoch: u32,
        tone_token: Option<u32>,
    ) -> Result<PluginInfo, String> {
        let host = self.host()?;
        let open = window_open(window, slot);
        self.load_target(&host, &open).load(state, slot, path, id, frontend_epoch, tone_token)
    }

    /// `plugin_unload`: take the plugin out of the engine and unload it, for the document with
    /// `frontend_epoch`; a load still running for the slot is cancelled (it unloads what it loads).
    pub(crate) fn plugin_unload(&self, state: &PluginHostState, slot: u8, frontend_epoch: u32) -> Result<(), String> {
        unload(&self.slots, slot, Some((state, frontend_epoch)), self.host().ok().as_ref())
    }

    /// On exit, before the unloads: every loaded slot saves its tone at once (each owner on its own
    /// thread), waited for at most `EXIT_SAVE_WAIT` in all. A plugin stuck in its save is logged and left
    /// to the unload's own bound (`mode::SHUTDOWN_WAIT`), and the other slot's save is not held up by it.
    pub(super) fn save_tones_on_exit(&self) {
        let loaded: Vec<(u8, Arc<EngineSlotHandle>)> = match self.slots() {
            Ok(slots) => (0u8..)
                .zip(slots.iter())
                .filter_map(|(slot, s)| match &s.state {
                    EngineSlot::Loaded { handle, .. } => Some((slot, handle.clone())),
                    _ => None,
                })
                .collect(),
            Err(e) => return log::error!("[plugin_host] tone saves on exit: {e}"),
        };
        let asked: Vec<_> = loaded.iter().map(|(slot, handle)| (*slot, handle.start_tone_save())).collect();
        let deadline = Instant::now() + EXIT_SAVE_WAIT;
        for (slot, asked) in asked {
            let saved = asked.and_then(|reply| {
                reply
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| format!("no answer within {} ms", EXIT_SAVE_WAIT.as_millis()))?
            });
            if let Err(e) = saved {
                log::error!("[plugin_host] engine slot {slot}: the tone save on exit failed: {e}");
            }
        }
    }

    /// Every slot's unload, on exit (while the device still plays), each slot off live first; one that
    /// cannot be is logged and unloads all the same.
    pub(super) fn unload_all(&self) {
        let host = self.host().ok();
        for slot in 0..lf_engine::SLOT_COUNT as u8 {
            if let Some(Err(e)) = host.as_ref().map(|host| silence(&self.slots, slot, None, host)) {
                log::error!("[plugin_host] engine slot {slot}: live-off on exit: {e}");
            }
            if let Err(e) = unload(&self.slots, slot, None, None) {
                log::error!("[plugin_host] engine slot {slot}: unload on exit: {e}");
            }
        }
    }

    /// `plugin_list_loaded`.
    pub(crate) fn plugin_list_loaded(&self) -> Result<Vec<PluginInfo>, String> {
        Ok(self
            .slots()?
            .iter()
            .filter_map(|s| match &s.state {
                EngineSlot::Loaded { info, .. } | EngineSlot::Swapping { info, .. } => Some(info.clone()),
                _ => None,
            })
            .collect())
    }

    /// `plugin_set_param`, for the document with `frontend_epoch` and the slot's `instance`: an empty
    /// slot is a no-op (a set racing an unload).
    pub(crate) fn plugin_set_param(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8, instance: u64, id: u32, value: f64) -> Result<(), String> {
        match playing(&self.slots, slot, instance, Some((state, frontend_epoch)))? {
            Some(handle) => handle.set_param(id, value),
            None => Ok(()),
        }
    }

    /// `plugin_list_params`, of the slot's `instance`.
    pub(crate) fn plugin_list_params(&self, slot: u8, instance: u64) -> Result<Vec<ParamDesc>, String> {
        self.handle(slot, instance, None)?.list_params()
    }

    /// `plugin_tone_take`: the tone of the slot's `instance`, saved fresh through its owner (it lands in
    /// the store too); the tone file's bytes, empty when the plugin keeps no state.
    pub(crate) fn plugin_tone_take(&self, slot: u8, instance: u64) -> Result<Vec<u8>, String> {
        self.handle(slot, instance, None)?.take_tone()
    }

    /// `plugin_tone_import`: a session's tone for `slot`, which session.json says belongs to `expected`
    /// (a tone file of another plugin is refused before anything is stored). It goes into the store
    /// under that plugin in `slot`, so its next load there restores it, and no load of it there from
    /// before the import saves over it (`ToneStore::import`). When `slot` holds that plugin
    /// now, the answer carries a reload token and the imported bytes are parked under it: the caller
    /// reloads the slot, passing the token to the load, to hear them, or `plugin_tone_forget`s it.
    /// Nothing is loaded, swapped or unloaded here.
    pub(crate) fn plugin_tone_import(&self, slot: u8, bytes: &[u8], expected: &ToneIdentity) -> Result<ToneImport, String> {
        let store = self.tones.as_ref().ok_or("no app-local data folder to keep tones in")?;
        let imported = store.import(usize::from(slot), bytes, expected)?;
        let (name, size, ToneIdentity { format, path, id }) =
            (imported.tone.name.clone(), imported.tone.state.len(), imported.tone.identity.clone());
        // What the slot holds and the parking in one step: any load after it takes or drops the park.
        let reload_token = {
            let slots = self.slots()?;
            let held = slots[usize::from(slot)].plays(expected);
            if held { Some(self.reload_tones()?.park(usize::from(slot), imported)) } else { None }
        };
        log::info!(
            "[plugin_host] engine slot {slot}: a session's tone for {name} stored ({size} bytes){}",
            if reload_token.is_some() { "; the slot holds it and reloads" } else { "" },
        );
        Ok(ToneImport { reload_token, name, format, path, id })
    }

    /// `plugin_tone_forget`: the reload `token` was answered for did not happen (the slot moved while
    /// the import ran): drop the tone parked under it, if it is still there.
    pub(crate) fn plugin_tone_forget(&self, slot: u8, token: u32) -> Result<(), String> {
        self.reload_tones()?.forget(usize::from(slot), token);
        Ok(())
    }

    /// `plugin_open_editor`, for the document with `frontend_epoch` and the slot's `instance`.
    pub(crate) fn plugin_open_editor(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8, instance: u64) -> Result<(), String> {
        self.handle(slot, instance, Some((state, frontend_epoch)))?.open_editor()
    }

    /// `plugin_close_editor`, for the document with `frontend_epoch` and the slot's `instance`.
    /// Idempotent: an empty slot is a no-op.
    pub(crate) fn plugin_close_editor(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8, instance: u64) -> Result<(), String> {
        match playing(&self.slots, slot, instance, Some((state, frontend_epoch)))? {
            Some(handle) => handle.close_editor(),
            None => Ok(()),
        }
    }

    /// `plugin_set_monitor_gain`: the slot's output level, the engine's `SetSlotGain`.
    pub(crate) fn plugin_gain(&self, slot: u8, gain: f32) -> Result<(), String> {
        self.host()?.send(TimedCommand { frame: None, command: Command::SetSlotGain(slot, gain) })
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU32, Ordering::Acquire};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Barrier;

    use lf_engine::grid::Frame;
    use lf_engine::{SlotEvent, SlotProcessor};

    use super::swap::{Retired, RetiredSink, SwapTarget};
    use super::*;
    use crate::host::engine_slot::SwapBounds;
    use crate::engine_io::test_rig::TestDevice;

    /// A stand-in owner's unit: a silent effect.
    struct Silent;

    impl SlotProcessor for Silent {
        fn kind(&self) -> SlotKind {
            SlotKind::Effect
        }
        fn latency(&self) -> Frame {
            0
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            out.fill(0.0);
        }
        fn stop(&mut self) {}
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    /// An effect for the other slot whose `process` waits while `hold` is up, once it has raised
    /// `entered`: it holds a block between the engine's command take and its gate publication.
    struct Held {
        hold: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }

    impl SlotProcessor for Held {
        fn kind(&self) -> SlotKind {
            SlotKind::Effect
        }
        fn latency(&self) -> Frame {
            0
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            if self.hold.load(Acquire) {
                self.entered.store(true, Release);
                while self.hold.load(Acquire) {
                    std::thread::yield_now();
                }
            }
            out.fill(0.0);
        }
        fn stop(&mut self) {}
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    /// A stand-in owner's unit of `kind`; one that `refuses` refuses to start processing as it stages.
    struct Unit {
        kind: SlotKind,
        refuses: bool,
    }

    impl SlotProcessor for Unit {
        fn kind(&self) -> SlotKind {
            self.kind
        }
        fn latency(&self) -> Frame {
            0
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            out.fill(0.0);
        }
        fn stop(&mut self) {}
        fn start_refused(&self) -> bool {
            self.refuses
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    fn effect() -> Box<dyn SlotProcessor> {
        Box::new(Unit { kind: SlotKind::Effect, refuses: false })
    }

    fn instrument() -> Box<dyn SlotProcessor> {
        Box::new(Unit { kind: SlotKind::Instrument, refuses: false })
    }

    fn refuser() -> Box<dyn SlotProcessor> {
        Box::new(Unit { kind: SlotKind::Effect, refuses: true })
    }

    /// A pause a stand-in owner takes once, where the test armed it: it tells the test it is there,
    /// then waits (≤ 10 s) for the go, so a failing test never leaves an owner waiting for good.
    #[derive(Default)]
    struct Hold(Mutex<Option<(Sender<()>, Receiver<()>)>>);

    impl Hold {
        /// The test's ends: told when the owner is there, and the go.
        fn arm(&self) -> (Receiver<()>, Sender<()>) {
            let ((there, here), (go, wait)) = (channel(), channel());
            *self.0.lock().unwrap() = Some((there, wait));
            (here, go)
        }

        fn take(&self) {
            let armed = self.0.lock().unwrap().take();
            if let Some((there, go)) = armed {
                let _ = there.send(());
                let _ = go.recv_timeout(Duration::from_secs(10));
            }
        }
    }

    /// What a test holds of a stand-in plugin: a pause once its unit is the slot's (`in_engine`), one
    /// as its teardown begins (`leaving`), and how many teardowns began (`ended`).
    #[derive(Default)]
    struct Hooks {
        in_engine: Arc<Hold>,
        leaving: Arc<Hold>,
        ended: Arc<AtomicU32>,
    }

    impl Hooks {
        /// Opens a stand-in plugin whose unit is `unit()`, entering as the load says, with these hooks.
        fn open(&self, unit: fn() -> Box<dyn SlotProcessor>) -> impl Fn(PluginFormat, String, String, SlotHost, Option<ToneBinding>, Entry) -> Result<EngineSlotHandle, String> {
            let (in_engine, leaving, ended) = (self.in_engine.clone(), self.leaving.clone(), self.ended.clone());
            move |_, _, _, slot, _, entry| {
                let (in_engine, leaving, ended) = (in_engine.clone(), leaving.clone(), ended.clone());
                let leaving = move || {
                    ended.fetch_add(1, Relaxed);
                    leaving.take();
                };
                engine_slot::stand_in_entering(slot, unit(), entry, move || in_engine.take(), leaving)
            }
        }
    }

    /// The app's slots, as `EngineApp` holds them, over a test device's engine. Dropped in field
    /// order: a plugin left in a slot unloads while the device still runs.
    struct Rig {
        slots: Slots,
        reload_tones: Mutex<ToneHandoff<{ lf_engine::SLOT_COUNT }>>,
        closing: AtomicBool,
        /// Each `plugin:retired`: slot, instance, released.
        retired: Arc<Mutex<Vec<(u8, u64, bool)>>>,
        state: PluginHostState,
        device: TestDevice,
        _one: std::sync::MutexGuard<'static, ()>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig::with_input(|_| 0.0)
        }

        /// With `input` as the device input.
        fn with_input(input: impl Fn(Frame) -> f32 + Send + Sync + 'static) -> Rig {
            let one = engine_slot::one_engine_test_at_a_time();
            Rig {
                slots: Mutex::new(std::array::from_fn(|_| SlotEntry::default())),
                reload_tones: Mutex::default(),
                closing: AtomicBool::new(false),
                retired: Arc::default(),
                state: PluginHostState::default(),
                device: TestDevice::start(48_000, 256, Duration::from_millis(5), input),
                _one: one,
            }
        }

        /// Wait until the device has rendered `blocks` more blocks.
        fn play(&self, blocks: u64) {
            let target = self.device.blocks.load(Relaxed) + blocks;
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.device.blocks.load(Relaxed) < target {
                assert!(Instant::now() < deadline, "the test device stopped rendering");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// The loudest of the last `frames` frames the device played.
        fn tail_peak(&self, frames: usize) -> f32 {
            let out = self.device.output.lock().unwrap();
            out[out.len().saturating_sub(frames)..].iter().fold(0.0, |m, x| m.max(x.abs()))
        }

        /// The frames the device has played so far (its kept output's length).
        fn played(&self) -> usize {
            self.device.output.lock().unwrap().len()
        }

        /// The loudest frame the device played from frame `from` of its kept output on.
        fn peak_from(&self, from: usize) -> f32 {
            self.device.output.lock().unwrap()[from..].iter().fold(0.0, |m, x| m.max(x.abs()))
        }

        /// A load into slot 0 for the document with `epoch`, opened by `open`, its reservation waiting
        /// up to `wait` for the slot to clear; the loaded plugin's name.
        fn load(&self, path: &str, epoch: u32, wait: Duration, open: Open<'_>) -> Result<String, String> {
            let target = LoadTarget { slots: &self.slots, reload_tones: &self.reload_tones, tones: None, host: self.device.host(), open, vacate_wait: wait };
            target.load(&self.state, 0, path.to_string(), "some.id".to_string(), epoch, None).map(|info| info.descriptor.name)
        }

        /// A swap of slot 0's plugin for the document with `epoch`, opened by `open`, its reservation
        /// waiting up to `wait` for the slot; the new plugin's info.
        fn swap(&self, path: &str, epoch: u32, wait: Duration, open: Open<'_>) -> Result<PluginInfo, String> {
            let record = self.retired.clone();
            let retired: RetiredSink = Arc::new(move |r: Retired| record.lock().unwrap().push((r.slot, r.instance, r.released)));
            let load = LoadTarget { slots: &self.slots, reload_tones: &self.reload_tones, tones: None, host: self.device.host(), open, vacate_wait: wait };
            SwapTarget { load, closing: &self.closing, retired }.swap(&self.state, 0, path.to_string(), "other.id".to_string(), 1.0, epoch, None)
        }

        fn slot(&self) -> &'static str {
            match lock(&self.slots).unwrap()[0].state {
                EngineSlot::Empty => "empty",
                EngineSlot::Loading { .. } => "loading",
                EngineSlot::Loaded { .. } => "loaded",
                EngineSlot::Swapping { .. } => "swapping",
                EngineSlot::Leaving => "leaving",
            }
        }

        /// Slot 0's playing handle.
        fn playing(&self) -> Arc<EngineSlotHandle> {
            loaded(&self.slots, 0, None).unwrap().expect("slot 0 plays a plugin")
        }

        fn retired(&self) -> Vec<(u8, u64, bool)> {
            self.retired.lock().unwrap().clone()
        }
    }

    /// Opens a stand-in plugin with no hooks, entering as the load says.
    fn stand_in(_: PluginFormat, _: String, _: String, slot: SlotHost, _: Option<ToneBinding>, entry: Entry) -> Result<EngineSlotHandle, String> {
        engine_slot::stand_in_entering(slot, Box::new(Silent), entry, || {}, || {})
    }

    /// A load is refused before it reserves its slot when its path names no format, and a load whose
    /// own loader fails frees the slot it reserved: the next load reaches its loader either way.
    #[test]
    fn a_load_this_host_cannot_start_leaves_the_slot_free_for_the_next() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let open = |format: PluginFormat, path: String, id: String, slot: SlotHost, tone: Option<ToneBinding>, entry: Entry| {
            engine_slot::load(format, path, id, slot, 0, Arc::new(|_| {}), tone, entry)
        };
        let load = |path: &str| rig.load(path, epoch, Duration::ZERO, &open);

        let unknown = load(r"C:\plugins\Thing.txt").unwrap_err();
        assert!(unknown.contains("not a .clap, .vst3 or .dll plugin"), "{unknown}");
        assert_eq!(rig.slot(), "empty", "an unknown extension reserves nothing");
        assert!(load(r"C:\plugins\no-extension").is_err());
        assert_eq!(rig.slot(), "empty");

        // Each format gets as far as its own loader, which finds no file, and the slot takes a load again.
        let missing = load(r"C:\no-such-folder\Missing.DLL").unwrap_err();
        assert!(missing.starts_with("LoadLibraryW"), "{missing}");
        assert_eq!(rig.slot(), "empty", "the failed VST2 load gave its reservation back");
        let missing = load(r"C:\no-such-folder\Missing.clap").unwrap_err();
        assert!(missing.starts_with("load failed"), "{missing}");
        assert_eq!(rig.slot(), "empty");
        let missing = load(r"C:\no-such-folder\Missing.vst3").unwrap_err();
        assert!(!missing.contains("already") && !missing.contains("not a .clap"), "{missing}");
        assert_eq!(rig.slot(), "empty");
    }

    /// A WebView reload while a load runs: the load that fails for the replaced document gives its
    /// reservation back, so the new document's load reaches its loader.
    #[test]
    fn a_failed_load_for_a_replaced_document_frees_its_slot() {
        let rig = Rig::new();
        let old = rig.state.begin_frontend_session();
        let reloaded = Mutex::new(None);
        let fails_after_a_reload = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            *reloaded.lock().unwrap() = Some(rig.state.begin_frontend_session());
            Err("load failed: the plugin is gone".to_string())
        };
        let failed = rig.load(r"C:\plugins\Amp.clap", old, Duration::ZERO, &fails_after_a_reload).unwrap_err();
        assert!(failed.starts_with("load failed"), "{failed}");
        assert_eq!(rig.slot(), "empty", "the stale load's reservation is gone");
        let new = reloaded.lock().unwrap().expect("the loader ran");
        assert_eq!(rig.load(r"C:\plugins\Amp.clap", new, Duration::ZERO, &stand_in).as_deref(), Ok("Stand-in"));
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, new)), None), Ok(()));
        assert_eq!(rig.slot(), "empty");
    }

    /// A WebView reload while a load runs, its plugin already in the engine: the load finishes for a
    /// replaced document and unloads what it loaded. Until its unit has left the engine the slot takes
    /// no load (one that tried would find the slot's unit there and fail); the new document's load waits
    /// for it, then loads.
    #[test]
    fn a_load_that_finishes_for_a_replaced_document_frees_its_slot_once_its_unit_has_left() {
        let rig = Rig::new();
        let old = rig.state.begin_frontend_session();
        // Each barrier: the stand-in's owner and this test meet there.
        let (in_engine, go_on) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let (leaving, leave) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let held = |_: PluginFormat, _: String, _: String, slot: SlotHost, _: Option<ToneBinding>, _: Entry| {
            let (in_engine, go_on, leaving, leave) = (in_engine.clone(), go_on.clone(), leaving.clone(), leave.clone());
            engine_slot::stand_in(
                slot,
                Box::new(Silent),
                move || {
                    in_engine.wait();
                    go_on.wait();
                },
                move || {
                    leaving.wait();
                    leave.wait();
                },
            )
        };
        // A load that reaches its loader while the slot still clears would install over the unit there.
        let reached = std::sync::atomic::AtomicBool::new(false);
        let probe = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            reached.store(true, Relaxed);
            Err("reached its loader".to_string())
        };
        let rig = &rig;
        // Observed inside, asserted after the joins: an assertion failing while the stand-in's owner
        // waits at a barrier would leave the scope waiting for it.
        let (before, during, leaving_slot, replacement_waited, stale, replacement) = std::thread::scope(|s| {
            let stale = s.spawn(move || rig.load(r"C:\plugins\Amp.clap", old, Duration::ZERO, &held));
            in_engine.wait();
            let new = rig.state.begin_frontend_session();
            let before = rig.load(r"C:\plugins\Amp.clap", new, Duration::ZERO, &probe);
            let replacement = s.spawn(move || rig.load(r"C:\plugins\Amp.clap", new, Duration::from_secs(10), &stand_in));
            go_on.wait();
            // The stale load is tearing its plugin down; its unit is still in the engine.
            leaving.wait();
            let leaving_slot = rig.slot();
            let during = rig.load(r"C:\plugins\Amp.clap", new, Duration::ZERO, &probe);
            let replacement_waited = !replacement.is_finished();
            leave.wait();
            (before, during, leaving_slot, replacement_waited, stale.join().unwrap(), replacement.join().unwrap())
        });
        for early in [before, during] {
            assert!(early.as_ref().is_err_and(|e| e.contains("still clearing")), "{early:?}");
        }
        assert!(!reached.load(Relaxed), "no load reached its loader while the slot was clearing");
        assert_eq!(leaving_slot, "leaving");
        assert!(replacement_waited, "the new document's load waits");
        assert!(stale.as_ref().is_err_and(|e| e.contains("superseded")), "{stale:?}");
        assert_eq!(replacement.as_deref(), Ok("Stand-in"), "the new document's load lands once the slot is clear");
        assert_eq!(rig.slot(), "loaded");
    }

    /// A final unload takes its slot off live and waits for the engine's gate before the unit leaves:
    /// the emptied slot passes no dry input on, and the settings a rebuild replays hold the live-off.
    /// One whose live-off the engine never applies (no block renders) keeps the plugin loaded.
    #[test]
    fn an_unload_takes_its_slot_off_live_before_the_unit_leaves_or_keeps_the_plugin() {
        let rig = Rig::with_input(|_| 0.5);
        let host = rig.device.host();
        host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        rig.play(4);
        assert!(rig.tail_peak(256) > 0.4, "an empty live slot passes the input dry");
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        rig.play(8);
        assert!(rig.tail_peak(1024) < 1e-6, "the silent effect is in, live");

        // The engine renders nothing while its lock is held: the live-off is never applied in time.
        let held = host.core.rt.lock().unwrap();
        let refused = unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(host)).unwrap_err();
        drop(held);
        assert!(refused.contains("stays loaded"), "{refused}");
        assert_eq!(rig.slot(), "loaded", "the plugin stays");

        host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        rig.play(8);
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(host)), Ok(()));
        assert_eq!(rig.slot(), "empty");
        rig.play(8);
        assert!(rig.tail_peak(8 * 256) < 1e-6, "no dry input after the unload: {}", rig.tail_peak(8 * 256));
        assert!(host.settings().contains(&Command::SetSlotLive(0, false)), "a rebuild replays the live-off");
    }

    /// The unload's live-off is answered by a block the engine rendered after taking it, never by a gate
    /// the overview showed closed before it: with a block held between its command take (a live-on)
    /// and its gate publication, the overview still shows slot A's gate closed, and the live-off gets
    /// no answer. Released, the gate closes on the live-off and the unload passes no dry input.
    #[test]
    fn an_unloads_live_off_is_answered_only_by_a_block_that_took_it() {
        let rig = Rig::with_input(|_| 0.5);
        let host = rig.device.host();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        let (hold, entered) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        // A failed assertion must not leave the render thread held: the device's drop joins it.
        struct Unhold(Arc<AtomicBool>);
        impl Drop for Unhold {
            fn drop(&mut self) {
                self.0.store(false, Release);
            }
        }
        let _unhold = Unhold(hold.clone());
        let slot_b = host.slot(1);
        assert!(slot_b.install(Box::new(Held { hold: hold.clone(), entered: entered.clone() }), 48_000).is_ok());
        rig.play(8);
        assert!(!host.core.ends.lock().unwrap().as_ref().unwrap().overview.slot_live(0), "slot A is off");

        // The next block takes a live-on, then holds in slot B's unit before it publishes its gates.
        {
            let _no_block = host.core.rt.lock().unwrap();
            host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
            hold.store(true, Release);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !entered.load(Acquire) {
            assert!(Instant::now() < deadline, "the held block never came");
            std::thread::sleep(Duration::from_millis(1));
        }
        let early = host.silence_slot(0, Duration::from_millis(100));
        assert!(early.is_err(), "no answer before a block that took the live-off has ended: {early:?}");

        hold.store(false, Release);
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(host)), Ok(()));
        let removed = rig.played();
        rig.play(8);
        assert!(rig.peak_from(removed) < 1e-6, "no dry input from the removal on: {}", rig.peak_from(removed));
        assert!(slot_b.remove(Duration::from_secs(10)).unwrap().is_some(), "slot B's unit comes back");
    }

    /// With the device stopped, an unload closes its slot's live gate in the engine before the unit
    /// leaves, a queued live-off being no acknowledgement: a reopen at the same rate (the engine kept,
    /// no settings replay) plays no dry input from its first frame.
    #[test]
    fn an_unload_while_the_device_is_stopped_passes_no_dry_input_at_the_reopen() {
        let mut rig = Rig::with_input(|_| 0.5);
        let host = rig.device.host().clone();
        host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        rig.play(8);
        assert!(rig.tail_peak(1024) < 1e-6, "the silent effect is in, live");

        rig.device.stop();
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(&host)), Ok(()));
        assert_eq!(rig.slot(), "empty");
        let reopened = rig.played();
        rig.device.resume();
        rig.play(8);
        assert!(rig.peak_from(reopened) < 1e-6, "no dry input from the reopen's first frame: {}", rig.peak_from(reopened));
        assert!(host.settings().contains(&Command::SetSlotLive(0, false)), "a rebuild replays the live-off");
    }

    /// The idle unload's gate holds until its own live-off has applied: a live-on queued ahead of it,
    /// more than a block's take (64 commands) before it, opens nothing at the reopen, and the slot ends
    /// off, in the engine and in the settings a rebuild replays.
    #[test]
    fn an_idle_unloads_gate_holds_against_a_live_on_more_than_a_take_ahead() {
        let mut rig = Rig::with_input(|_| 0.5);
        let host = rig.device.host().clone();
        host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        rig.play(8);
        assert!(rig.tail_peak(1024) < 1e-6, "the silent effect is in, live");

        rig.device.stop();
        host.send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) }).unwrap();
        for _ in 0..64 {
            host.send(TimedCommand { frame: None, command: Command::SetSlotGain(1, 1.0) }).unwrap();
        }
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(&host)), Ok(()));
        let reopened = rig.played();
        rig.device.resume();
        rig.play(8);
        assert!(rig.peak_from(reopened) < 1e-6, "no dry input from the reopen's first frame: {}", rig.peak_from(reopened));
        assert!(!host.core.ends.lock().unwrap().as_ref().unwrap().overview.slot_live(0), "the slot ends off");
        assert!(host.settings().contains(&Command::SetSlotLive(0, false)), "a rebuild replays the live-off");
    }

    /// An unload from a replaced document, delayed past the reload, finds the new document's plugin in
    /// the slot: it is refused, and so is any other change that document asks for; the plugin stays.
    #[test]
    fn a_replaced_documents_late_unload_leaves_the_new_documents_plugin_alone() {
        let rig = Rig::new();
        let old = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", old, Duration::ZERO, &stand_in).is_ok());
        // The reload: the new document clears the slot it found stranded and loads its own plugin.
        let new = rig.state.begin_frontend_session();
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, new)), None), Ok(()));
        assert_eq!(rig.slot(), "empty");
        assert!(rig.load(r"C:\plugins\Drive.clap", new, Duration::ZERO, &stand_in).is_ok());

        let late = unload(&rig.slots, 0, Some((&rig.state, old)), None).unwrap_err();
        assert!(late.contains("stale frontend epoch"), "{late}");
        let late = loaded(&rig.slots, 0, Some((&rig.state, old))).err().expect("a stale document takes no handle");
        assert!(late.contains("stale frontend epoch"), "{late}");
        assert_eq!(rig.slot(), "loaded");
        assert!(loaded(&rig.slots, 0, Some((&rig.state, new))).unwrap().is_some());
        assert_eq!(unload(&rig.slots, 0, None, None), Ok(()), "the app's exit names no document");
        assert_eq!(rig.slot(), "empty");
    }

    /// A hot swap commits: it answers the new instance and the kind it activated as, the replaced
    /// owner is retired, and its retire ends `released`. Until that end, the slot takes no cold load:
    /// one past its (shortened) wait is refused, one that waits lands after it.
    #[test]
    fn a_swap_commits_and_a_cold_load_waits_for_the_replaced_plugins_release() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, new) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let playing = rig.playing();
        let (tearing_down, go) = old.leaving.arm();
        let info = rig.swap(r"C:\plugins\Synth.clap", epoch, Duration::ZERO, &new.open(instrument)).unwrap();
        let swapped = rig.playing();
        assert_eq!((info.instance, info.effect, info.descriptor.path.as_str()), (swapped.instance(), false, r"C:\plugins\Synth.clap"));
        assert_ne!(info.instance, playing.instance(), "a new instance");
        assert!(playing.retired() && !swapped.retired(), "the replaced owner is retired, the new one plays");
        drop(swapped);
        assert!(tearing_down.recv_timeout(Duration::from_secs(10)).is_ok(), "the replaced owner tears down");

        // Its teardown is held: the emptied slot takes no cold load until the release has ended.
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), None), Ok(()));
        assert_eq!(rig.slot(), "empty");
        let early = rig.load(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &stand_in);
        let (waited, cold) = std::thread::scope(|s| {
            let cold = s.spawn(|| rig.load(r"C:\plugins\Drive.clap", epoch, Duration::from_secs(10), &stand_in));
            std::thread::sleep(Duration::from_millis(100));
            let waited = !cold.is_finished() && rig.retired().is_empty();
            let _ = go.send(());
            (waited, cold.join().unwrap())
        });
        assert!(early.as_ref().is_err_and(|e| e.contains("still releasing the plugin it played before")), "{early:?}");
        assert!(waited, "the cold load waits for the release");
        assert_eq!(cold.as_deref(), Ok("Stand-in"));
        assert_eq!(rig.retired(), vec![(0, playing.instance(), true)], "released, before the cold load landed");
    }

    /// A swap onto the file that plays, spelled in another case or with `/`, is refused before any
    /// load: the plugin plays on.
    #[test]
    fn a_swap_onto_the_file_that_plays_is_refused_and_the_old_plays_on() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        let playing = rig.playing().instance();
        let reached = AtomicBool::new(false);
        let probe = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            reached.store(true, Relaxed);
            Err("reached its loader".to_string())
        };
        for path in [r"c:\PLUGINS\amp.CLAP", "C:/plugins/Amp.clap"] {
            let refused = rig.swap(path, epoch, Duration::ZERO, &probe).unwrap_err();
            assert!(refused.contains("same plugin file: swap cold"), "{refused}");
        }
        assert!(!reached.load(Relaxed), "no load ran");
        assert_eq!(rig.slot(), "loaded");
        let handle = rig.playing();
        assert!(handle.instance() == playing && !handle.retired(), "the old plugin plays on");
    }

    /// A candidate that fails to load, or refuses to start processing as it stages: the swap answers
    /// that error unchanged and the slot goes back to the plugin that plays, which never stopped.
    #[test]
    fn a_refused_candidate_answers_its_error_and_the_old_plays_on() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, refusing) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let playing = rig.playing().instance();
        let fails = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            Err("load failed: the plugin is gone".to_string())
        };
        let failed = rig.swap(r"C:\plugins\Gone.clap", epoch, Duration::ZERO, &fails);
        assert_eq!(failed.unwrap_err(), "load failed: the plugin is gone");
        let refused = rig.swap(r"C:\plugins\Stubborn.clap", epoch, Duration::ZERO, &refusing.open(refuser));
        assert!(refused.as_ref().is_err_and(|e| e.contains("refused to start processing")), "{refused:?}");
        assert_eq!(rig.slot(), "loaded");
        let handle = rig.playing();
        assert!(handle.instance() == playing && !handle.retired(), "the old plugin plays on");
        assert_eq!(old.ended.load(Relaxed), 0, "its teardown never began");
        assert!(rig.retired().is_empty());
    }

    /// A swap `call_off` calls off once its plugin is the slot's, before the swap has heard so: both
    /// plugins end, neither as a retire, the slot ends empty, and the swap answers that it was superseded.
    fn a_called_off_swap_ends_both_plugins(call_off: impl FnOnce(&Rig, u32)) {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, new) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let (committed, go) = new.in_engine.arm();
        let (during, swapped) = std::thread::scope(|s| {
            let swap = s.spawn(|| rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &new.open(effect)));
            let _ = committed.recv_timeout(Duration::from_secs(10));
            let during = rig.slot();
            call_off(&rig, epoch);
            let _ = go.send(());
            (during, swap.join().unwrap())
        });
        assert_eq!(during, "swapping");
        assert!(swapped.as_ref().is_err_and(|e| e.contains("superseded")), "{swapped:?}");
        assert_eq!(rig.slot(), "empty");
        assert_eq!((old.ended.load(Relaxed), new.ended.load(Relaxed)), (1, 1), "both plugins ended");
        assert!(rig.retired().is_empty());
    }

    #[test]
    fn an_unload_during_a_swap_ends_both_plugins_and_empties_the_slot() {
        a_called_off_swap_ends_both_plugins(|rig, epoch| {
            assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(rig.device.host())), Ok(()));
        });
    }

    #[test]
    fn a_document_change_during_a_swap_ends_both_plugins_and_empties_the_slot() {
        a_called_off_swap_ends_both_plugins(|rig, _| {
            rig.state.begin_frontend_session();
        });
    }

    /// The next swap starts once the plugin swapped out before it has ended its retire: one past its
    /// (shortened) wait is refused, one that waits lands after that release.
    #[test]
    fn a_swap_waits_for_the_plugin_retiring_before_it_and_one_past_its_wait_is_refused() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (first, second) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &first.open(effect)).is_ok());
        let replaced = rig.playing().instance();
        let (tearing_down, go) = first.leaving.arm();
        let swapped = rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &second.open(effect)).unwrap();
        assert!(tearing_down.recv_timeout(Duration::from_secs(10)).is_ok(), "the replaced owner tears down");

        let early = rig.swap(r"C:\plugins\Fuzz.clap", epoch, Duration::ZERO, &stand_in);
        let (waited, late) = std::thread::scope(|s| {
            let late = s.spawn(|| rig.swap(r"C:\plugins\Fuzz.clap", epoch, Duration::from_secs(10), &stand_in));
            std::thread::sleep(Duration::from_millis(100));
            let waited = !late.is_finished() && rig.slot() == "loaded";
            let _ = go.send(());
            (waited, late.join().unwrap())
        });
        assert!(early.as_ref().is_err_and(|e| e.contains("still releasing the plugin it played before")), "{early:?}");
        assert!(waited, "the second swap waits, the slot untaken");
        let late = late.unwrap();
        assert_ne!(late.instance, swapped.instance);
        assert_eq!(rig.retired().first(), Some(&(0, replaced, true)), "released before the second swap took the slot");
        let deadline = Instant::now() + Duration::from_secs(10);
        while rig.retired().len() < 2 {
            assert!(Instant::now() < deadline, "the second swap's replaced plugin never ended its retire");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rig.retired()[1], (0, swapped.instance, true));
    }
    /// Wait (≤ 10 s) until the rig has heard `count` retires end.
    fn retires(rig: &Rig, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while rig.retired().len() < count {
            assert!(Instant::now() < deadline, "a replaced plugin never ended its retire");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A retire the swap runs inline (no thread started) holds the slot as a retire thread does: a cold
    /// load and a swap past their wait are refused as still releasing, and ones that wait land once the
    /// swap has cleared its mark.
    #[test]
    fn an_inline_retire_holds_the_slot_until_its_swap_clears_it() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let mark = |set: bool| lock(&rig.slots).unwrap()[0].retiring = set.then_some(Retiring { instance: 7, join: None });
        let held_until_cleared = |attempt: &(dyn Fn(Duration) -> Result<(), String> + Sync)| {
            mark(true);
            let early = attempt(Duration::ZERO);
            let (waited, late) = std::thread::scope(|s| {
                let late = s.spawn(|| attempt(Duration::from_secs(10)));
                std::thread::sleep(Duration::from_millis(100));
                let waited = !late.is_finished();
                mark(false);
                (waited, late.join().unwrap())
            });
            assert!(early.as_ref().is_err_and(|e| e.contains("still releasing the plugin it played before (instance 7)")), "{early:?}");
            assert!(waited, "it waits for the inline retire");
            assert_eq!(late, Ok(()));
        };
        held_until_cleared(&|wait| rig.load(r"C:\plugins\Amp.clap", epoch, wait, &stand_in).map(drop));
        held_until_cleared(&|wait| rig.swap(r"C:\plugins\Drive.clap", epoch, wait, &stand_in).map(drop));
    }

    /// A swap called off by a reload as its load fails, whose live-off cannot apply (no block renders):
    /// the plugin that plays stays loaded, and the error keeps the load's and says so.
    #[test]
    fn a_called_off_swap_whose_live_off_fails_keeps_the_playing_plugin() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let old = Hooks::default();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let playing = rig.playing().instance();
        let reloads = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            rig.state.begin_frontend_session();
            Err("load failed: the plugin is gone".to_string())
        };
        let held = rig.device.host().core.rt.lock().unwrap();
        let failed = rig.swap(r"C:\plugins\Gone.clap", epoch, Duration::ZERO, &reloads).unwrap_err();
        drop(held);
        assert!(failed.starts_with("load failed: the plugin is gone; the plugin playing in slot 0 stays loaded"), "{failed}");
        assert_eq!(rig.slot(), "loaded");
        let handle = rig.playing();
        assert!(handle.instance() == playing && !handle.retired(), "the old plugin plays on");
        assert_eq!(old.ended.load(Relaxed), 0, "its teardown never began");
    }

    /// A swap called off by a reload once its plugin is the slot's, whose live-off cannot apply: the new
    /// plugin stays loaded and the one it replaced retires, as on success.
    #[test]
    fn a_called_off_swap_whose_live_off_fails_keeps_the_new_plugin_once_it_plays() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, new) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let playing = rig.playing().instance();
        let (open_new, held) = (new.open(effect), RefCell::new(None));
        let commits_then_reloads = |format: PluginFormat, path: String, id: String, slot: SlotHost, tone: Option<ToneBinding>, entry: Entry| {
            let loaded = open_new(format, path, id, slot, tone, entry);
            rig.state.begin_frontend_session();
            *held.borrow_mut() = Some(rig.device.host().core.rt.lock().unwrap());
            loaded
        };
        let swapped = rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &commits_then_reloads).unwrap_err();
        drop(held.borrow_mut().take());
        assert!(swapped.contains("superseded") && swapped.contains("the plugin playing in slot 0 stays loaded"), "{swapped}");
        assert_eq!(rig.slot(), "loaded");
        let handle = rig.playing();
        assert!(handle.instance() != playing && !handle.retired(), "the new plugin plays");
        retires(&rig, 1);
        assert_eq!(rig.retired(), vec![(0, playing, true)], "the old one retired");
        assert_eq!((old.ended.load(Relaxed), new.ended.load(Relaxed)), (1, 0));
    }

    /// While a swap loads the next plugin, the slot still holds the one that plays: a tone import for
    /// that one gets its reload token (`plugin_tone_import`), one for the plugin loading does not.
    #[test]
    fn a_swapping_slot_holds_its_playing_plugin_for_a_tone_import() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        let identity = |path: &str, id: &str| ToneIdentity { format: "clap".to_string(), path: path.to_string(), id: id.to_string() };
        let seen = Cell::new(None);
        let looks = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            let slots = lock(&rig.slots).unwrap();
            seen.set(Some((slots[0].plays(&identity(r"C:\plugins\Amp.clap", "some.id")), slots[0].plays(&identity(r"C:\plugins\Drive.clap", "other.id")))));
            Err("load failed".to_string())
        };
        assert!(rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &looks).is_err());
        assert_eq!(seen.get(), Some((true, false)));
    }

    /// Once a swap commits, a request naming the replaced instance is refused `stale instance` and one
    /// naming the new instance reaches it (`playing`); with the slot empty, any instance finds no plugin.
    #[test]
    fn a_request_for_the_instance_a_swap_replaced_is_refused_stale() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        let old = rig.playing().instance();
        let new = rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &stand_in).unwrap().instance;
        let ask = |instance| playing(&rig.slots, 0, instance, Some((&rig.state, epoch))).map(|h| h.map(|h| h.instance()));
        assert_eq!(ask(old), Err("stale instance".to_string()));
        assert_eq!(ask(new), Ok(Some(new)));
        assert_eq!(unload(&rig.slots, 0, Some((&rig.state, epoch)), None), Ok(()));
        assert_eq!((ask(old), ask(new)), (Ok(None), Ok(None)), "an empty slot keeps each request's own answer");
    }

    /// While a swap loads the next plugin, a request naming the playing instance reaches it, and one
    /// naming the instance still loading is refused `stale instance`: it is not the slot's yet.
    #[test]
    fn a_request_during_a_swap_reaches_the_playing_instance_only() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &stand_in).is_ok());
        let old = rig.playing().instance();
        let ask = |instance| playing(&rig.slots, 0, instance, Some((&rig.state, epoch))).map(|h| h.map(|h| h.instance()));
        let seen = RefCell::new(None);
        let looks = |_: PluginFormat, _: String, _: String, slot: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            *seen.borrow_mut() = Some((rig.slot(), ask(old), ask(slot.tag())));
            Err("load failed".to_string())
        };
        assert!(rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &looks).is_err());
        assert_eq!(seen.take(), Some(("swapping", Ok(Some(old)), Err("stale instance".to_string()))));
    }

    /// A swap whose load fails after an unload called it off: the plugin it was to replace ends with it
    /// (its teardown, not a retire), the slot ends empty, and the swap answers the load's error.
    #[test]
    fn a_failed_load_for_a_called_off_swap_ends_the_playing_plugin() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let old = Hooks::default();
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let unloaded = Cell::new(None);
        let unloads = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>, _: Entry| -> Result<EngineSlotHandle, String> {
            unloaded.set(Some(unload(&rig.slots, 0, Some((&rig.state, epoch)), Some(rig.device.host()))));
            Err("load failed: the plugin is gone".to_string())
        };
        let failed = rig.swap(r"C:\plugins\Gone.clap", epoch, Duration::ZERO, &unloads);
        assert_eq!(unloaded.take(), Some(Ok(())), "the unload called the swap off");
        assert_eq!(failed.unwrap_err(), "load failed: the plugin is gone");
        assert_eq!(rig.slot(), "empty");
        assert_eq!(old.ended.load(Relaxed), 1, "the old plugin ended");
        assert!(rig.retired().is_empty());
    }

    /// A swap whose plugin committed but whose load failed (it ended again): the plugin it was to
    /// replace was retired by that commit, so it ends too and the slot ends empty, the error saying so.
    #[test]
    fn a_failed_swap_whose_old_plugin_was_retired_meanwhile_empties_the_slot() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, new) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let open_new = new.open(effect);
        let commits_then_fails = |format: PluginFormat, path: String, id: String, slot: SlotHost, tone: Option<ToneBinding>, entry: Entry| {
            open_new(format, path, id, slot, tone, entry)?.unload()?;
            Err("the engine did not answer the swap".to_string())
        };
        let failed = rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &commits_then_fails).unwrap_err();
        assert!(failed.ends_with("the plugin it was to replace was retired meanwhile, so slot 0 is empty"), "{failed}");
        assert_eq!(rig.slot(), "empty");
        assert_eq!((old.ended.load(Relaxed), new.ended.load(Relaxed)), (1, 1), "both plugins ended");
        assert!(rig.retired().is_empty(), "the old one ended by its teardown, not a retire");
    }

    /// A swap the engine never answers within its bounds (no block renders): the candidate is given up,
    /// the swap answers so, and the slot goes back to the plugin that plays, which never stopped.
    #[test]
    fn a_swap_the_engine_never_answers_gives_the_slot_back_to_the_playing_plugin() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let (old, new) = (Hooks::default(), Hooks::default());
        assert!(rig.load(r"C:\plugins\Amp.clap", epoch, Duration::ZERO, &old.open(effect)).is_ok());
        let playing = rig.playing().instance();
        let open_new = new.open(effect);
        let bounded = |format: PluginFormat, path: String, id: String, slot: SlotHost, tone: Option<ToneBinding>, entry: Entry| {
            let Entry::Replace { gain, cancel, .. } = entry else { unreachable!("a swap replaces") };
            let bounds = SwapBounds { commit: Duration::from_millis(20), settle: Duration::from_millis(50) };
            open_new(format, path, id, slot, tone, Entry::Replace { gain, cancel, bounds })
        };
        let held = rig.device.host().core.rt.lock().unwrap();
        let failed = rig.swap(r"C:\plugins\Drive.clap", epoch, Duration::ZERO, &bounded).unwrap_err();
        let (slot, handle) = (rig.slot(), rig.playing());
        let retired = handle.retired();
        drop(held);
        assert!(failed.contains("did not answer"), "{failed}");
        assert_eq!(slot, "loaded");
        assert!(handle.instance() == playing && !retired, "the old plugin plays on");
        assert_eq!(old.ended.load(Relaxed), 0, "its teardown never began");
    }
}
