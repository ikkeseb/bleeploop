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

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::{Command, TimedCommand};
use tauri::Emitter;

use super::mode::EngineApp;
use super::{EngineHost, SlotHost};
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, EventSink, PluginFormat};
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

type Slots = Mutex<[EngineSlot; lf_engine::SLOT_COUNT]>;

pub(crate) enum EngineSlot {
    Empty,
    /// A load for the document with this epoch is running. `cancelled`: an unload asked for the slot
    /// meanwhile, so the load unloads what it loads.
    Loading { epoch: u32, cancelled: bool },
    Loaded { handle: Arc<EngineSlotHandle>, info: PluginInfo },
    /// A plugin is on its way out (an unload, or a load nobody wants any more): its unit is in the
    /// engine until the teardown ends, so no load may take the slot before that.
    Leaving,
}

/// The `plugin:param-changed` payload.
#[derive(Clone, serde::Serialize)]
struct ParamChanged {
    slot: u8,
    id: u32,
    value: f64,
}

/// What a load runs once its slot is reserved: `engine_slot::load` with the window's parent and
/// events (`EngineApp::plugin_load`), a test's stand-in owner otherwise.
type Open<'a> = &'a dyn Fn(PluginFormat, String, String, SlotHost, Option<ToneBinding>) -> Result<EngineSlotHandle, String>;

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
        let format = PluginFormat::of_path(std::path::Path::new(&path))
            .ok_or_else(|| format!("{path}: not a .clap, .vst3 or .dll plugin"))?;
        self.reserve(state, slot, frontend_epoch)?;
        let identity = ToneIdentity { format: format.as_str().to_string(), path: path.clone(), id: id.clone() };
        // Past the reservation, so nothing below may return early: a poisoned lock just hands nothing over.
        let imported = self.reload_tones.lock().ok().and_then(|mut tones| tones.take(usize::from(slot), tone_token, &identity));
        let tone = self.tones.clone().map(|store| ToneBinding { store, slot: usize::from(slot), identity, imported });
        let began = Instant::now();
        let loaded = (self.open)(format, path.clone(), id.clone(), self.host.slot(usize::from(slot)), tone);
        let mut slots = lock(self.slots)?;
        // The reservation is this load's (nothing else moves a slot out of `Loading`); whether its
        // document still wants the plugin is another question.
        let wanted = matches!(slots[usize::from(slot)], EngineSlot::Loading { cancelled: false, .. })
            && state.frontend_epoch.load(Relaxed) == frontend_epoch;
        let handle = match loaded {
            Ok(handle) => handle,
            Err(e) => {
                // A setup that timed out had not claimed its load, so its owner never installs
                // (`engine_slot::LoadClaim`); one that had is a load that stands. Only a claimed load
                // whose report then took 2 s more leaves a unit to undo (`src-tauri/OPEN-THREADS.md`).
                slots[usize::from(slot)] = EngineSlot::Empty;
                return Err(e);
            }
        };
        if !wanted {
            // Its unit is in the engine until the unload ends: a load for the new document waits.
            slots[usize::from(slot)] = EngineSlot::Leaving;
            drop(slots);
            if let Err(e) = handle.unload() {
                log::error!("[plugin_host] engine slot {slot}: unloading a superseded load: {e}");
            }
            vacate(self.slots, slot);
            return Err(format!("plugin load for slot {slot} was superseded by a frontend reload or an unload"));
        }
        let descriptor = PluginDescriptor {
            id,
            name: handle.name().to_string(),
            format: format.as_str().to_string(),
            path,
            // The frontend keeps the scan's descriptor for gain staging.
            is_effect: None,
        };
        let info = PluginInfo { slot, descriptor, tone: handle.tone() };
        log::info!("[plugin_host] engine slot {slot}: {} ({:?}) loaded in {} ms", info.descriptor.name, handle.kind(), began.elapsed().as_millis());
        slots[usize::from(slot)] = EngineSlot::Loaded { handle: Arc::new(handle), info: info.clone() };
        Ok(info)
    }

    /// Reserve `slot` for this load: at once when it is empty; after a wait (≤ `vacate_wait`) while a
    /// replaced document's load, a cancelled load or an unload still clears it. Refused for a replaced
    /// document, and while the slot holds a plugin or the current document's own load.
    fn reserve(&self, state: &PluginHostState, slot: u8, frontend_epoch: u32) -> Result<(), String> {
        let deadline = Instant::now() + self.vacate_wait;
        loop {
            {
                let mut slots = lock(self.slots)?;
                current_document(state, frontend_epoch)?;
                match slots[usize::from(slot)] {
                    EngineSlot::Empty => {
                        slots[usize::from(slot)] = EngineSlot::Loading { epoch: frontend_epoch, cancelled: false };
                        return Ok(());
                    }
                    EngineSlot::Loading { epoch, cancelled: false } if epoch == frontend_epoch => {
                        return Err(format!("slot {slot} is already loading a plugin"));
                    }
                    EngineSlot::Loaded { .. } => return Err(format!("slot {slot} already has a plugin loaded")),
                    EngineSlot::Loading { .. } | EngineSlot::Leaving => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {slot} is still clearing the plugin before it"));
            }
            std::thread::sleep(VACATE_POLL);
        }
    }
}

fn lock(slots: &Slots) -> Result<MutexGuard<'_, [EngineSlot; lf_engine::SLOT_COUNT]>, String> {
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
    Ok(match &slots[usize::from(slot)] {
        EngineSlot::Loaded { handle, .. } => Some(handle.clone()),
        _ => None,
    })
}

/// Take `slot`'s plugin out of the engine and unload it, for the document `document` names (`None`:
/// the app's exit). A load still running is cancelled and unloads what it loads; the slot is
/// `Leaving` until this teardown ends. With `host`, a loaded plugin's slot goes off live first, its gate
/// closed (`silence`), and the plugin stays loaded when that fails.
fn unload(slots: &Slots, slot: u8, document: Option<(&PluginHostState, u32)>, host: Option<&EngineHost>) -> Result<(), String> {
    if let Some(host) = host {
        silence(slots, slot, document, host).map_err(|e| format!("the plugin in slot {slot} stays loaded, as its live input could not be stopped first: {e}"))?;
    }
    let handle = {
        let mut slots = lock(slots)?;
        if let Some((state, frontend_epoch)) = document {
            current_document(state, frontend_epoch)?;
        }
        let taken = &mut slots[usize::from(slot)];
        match std::mem::replace(taken, EngineSlot::Leaving) {
            EngineSlot::Loaded { handle, .. } => handle,
            EngineSlot::Loading { epoch, .. } => {
                *taken = EngineSlot::Loading { epoch, cancelled: true };
                return Ok(());
            }
            other => {
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
        matches!(slots[usize::from(slot)], EngineSlot::Loaded { .. })
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
    if matches!(slots[usize::from(slot)], EngineSlot::Leaving) {
        slots[usize::from(slot)] = EngineSlot::Empty;
    }
}

impl EngineApp {
    fn slots(&self) -> Result<MutexGuard<'_, [EngineSlot; lf_engine::SLOT_COUNT]>, String> {
        lock(&self.slots)
    }

    /// Per slot: the tone a session import stored for the plugin the slot held, parked for the reload
    /// that follows (`ToneHandoff`). Taken after `slots` when both are held.
    fn reload_tones(&self) -> Result<MutexGuard<'_, ToneHandoff<{ lf_engine::SLOT_COUNT }>>, String> {
        self.reload_tones.lock().map_err(|_| "engine reload tones poisoned".to_string())
    }

    /// The slot's handle; with `document`, for a request from the current document only (`loaded`).
    fn handle(&self, slot: u8, document: Option<(&PluginHostState, u32)>) -> Result<Arc<EngineSlotHandle>, String> {
        loaded(&self.slots, slot, document)?.ok_or_else(|| format!("no plugin loaded in slot {slot}"))
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
        let events = window.clone();
        let sink: EventSink = Arc::new(move |event| {
            let _ = match event {
                EngineSlotEvent::ParamChanged { id, value } => events.emit("plugin:param-changed", ParamChanged { slot, id, value }),
                EngineSlotEvent::ParamsChanged => events.emit("plugin:params-changed", slot),
                EngineSlotEvent::EditorClosed => events.emit("plugin:editor-closed", slot),
            };
        });
        // The editor host window is owner-less; a floating CLAP editor takes the main window as its
        // transient parent.
        let parent = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
        let open = |format: PluginFormat, path: String, id: String, slot_host: SlotHost, tone: Option<ToneBinding>| {
            engine_slot::load(format, path, id, slot_host, parent, sink.clone(), tone, engine_slot::Entry::Install)
        };
        let target = LoadTarget {
            slots: &self.slots,
            reload_tones: &self.reload_tones,
            tones: self.tones.clone(),
            host: &host,
            open: &open,
            vacate_wait: VACATE_WAIT,
        };
        target.load(state, slot, path, id, frontend_epoch, tone_token)
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
                .filter_map(|(slot, s)| match s {
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
            .filter_map(|s| match s {
                EngineSlot::Loaded { info, .. } => Some(info.clone()),
                _ => None,
            })
            .collect())
    }

    /// `plugin_set_param`, for the document with `frontend_epoch`: an empty slot is a no-op (a set
    /// racing an unload).
    pub(crate) fn plugin_set_param(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8, id: u32, value: f64) -> Result<(), String> {
        match loaded(&self.slots, slot, Some((state, frontend_epoch)))? {
            Some(handle) => handle.set_param(id, value),
            None => Ok(()),
        }
    }

    pub(crate) fn plugin_list_params(&self, slot: u8) -> Result<Vec<ParamDesc>, String> {
        self.handle(slot, None)?.list_params()
    }

    /// `plugin_tone_take`: the slot's tone, saved fresh through its owner (it lands in the store too);
    /// the tone file's bytes, empty when the plugin keeps no state.
    pub(crate) fn plugin_tone_take(&self, slot: u8) -> Result<Vec<u8>, String> {
        self.handle(slot, None)?.take_tone()
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
        let holds = |s: &EngineSlot| {
            matches!(s, EngineSlot::Loaded { info, .. } if {
                let d = &info.descriptor;
                d.format == expected.format && d.path == expected.path && d.id == expected.id
            })
        };
        let (name, size, ToneIdentity { format, path, id }) =
            (imported.tone.name.clone(), imported.tone.state.len(), imported.tone.identity.clone());
        // What the slot holds and the parking in one step: any load after it takes or drops the park.
        let reload_token = {
            let slots = self.slots()?;
            let held = holds(&slots[usize::from(slot)]);
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

    /// `plugin_open_editor`, for the document with `frontend_epoch`.
    pub(crate) fn plugin_open_editor(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8) -> Result<(), String> {
        self.handle(slot, Some((state, frontend_epoch)))?.open_editor()
    }

    /// `plugin_close_editor`, for the document with `frontend_epoch`. Idempotent.
    pub(crate) fn plugin_close_editor(&self, state: &PluginHostState, frontend_epoch: u32, slot: u8) -> Result<(), String> {
        match loaded(&self.slots, slot, Some((state, frontend_epoch)))? {
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
    use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
    use std::sync::Barrier;

    use lf_engine::grid::Frame;
    use lf_engine::{SlotEvent, SlotKind, SlotProcessor};

    use super::*;
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

    /// The app's slots, as `EngineApp` holds them, over a test device's engine. Dropped in field
    /// order: a plugin left in a slot unloads while the device still runs.
    struct Rig {
        slots: Slots,
        reload_tones: Mutex<ToneHandoff<{ lf_engine::SLOT_COUNT }>>,
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
                slots: Mutex::new(std::array::from_fn(|_| EngineSlot::Empty)),
                reload_tones: Mutex::default(),
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

        fn slot(&self) -> &'static str {
            match lock(&self.slots).unwrap()[0] {
                EngineSlot::Empty => "empty",
                EngineSlot::Loading { .. } => "loading",
                EngineSlot::Loaded { .. } => "loaded",
                EngineSlot::Leaving => "leaving",
            }
        }
    }

    /// Opens a stand-in plugin with no hooks.
    fn stand_in(_: PluginFormat, _: String, _: String, slot: SlotHost, _: Option<ToneBinding>) -> Result<EngineSlotHandle, String> {
        engine_slot::stand_in(slot, Box::new(Silent), || {}, || {})
    }

    /// A load is refused before it reserves its slot when its path names no format, and a load whose
    /// own loader fails frees the slot it reserved: the next load reaches its loader either way.
    #[test]
    fn a_load_this_host_cannot_start_leaves_the_slot_free_for_the_next() {
        let rig = Rig::new();
        let epoch = rig.state.begin_frontend_session();
        let open = |format: PluginFormat, path: String, id: String, slot: SlotHost, tone: Option<ToneBinding>| {
            engine_slot::load(format, path, id, slot, 0, Arc::new(|_| {}), tone, engine_slot::Entry::Install)
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
        let fails_after_a_reload = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>| -> Result<EngineSlotHandle, String> {
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
        let held = |_: PluginFormat, _: String, _: String, slot: SlotHost, _: Option<ToneBinding>| {
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
        let probe = |_: PluginFormat, _: String, _: String, _: SlotHost, _: Option<ToneBinding>| -> Result<EngineSlotHandle, String> {
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
}
