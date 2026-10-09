//! OWNS: engine mode's plugin slots: the
//! `plugin_*` load, unload, list, parameter and editor commands, routed to the engine slot owners
//! (`host/engine_slot.rs`) with their window events (`plugin:param-changed`,
//! `plugin:params-changed`, `plugin:editor-closed`), and tone recall's commands (`host/tone.rs`): every
//! load restores the plugin's stored tone, a session export takes a slot's tone fresh, an import stores
//! one (checked against the plugin session.json names; every load of that plugin from before it stops
//! saving, and the reload of the slot that held it is handed the imported bytes under a reload token),
//! and the app's exit saves them all before the unloads. A plugin loads into an engine that exists:
//! open the device first. GO LIVE is the engine's `SetSlotLive` and the plugin's gain `SetSlotGain` (sent
//! in `input_send`'s batch); notes go through native MIDI's router (`midi_mode`).
//!
//! A slot is reserved while it loads, for the WebView document that asked (its `frontendEpoch`): a
//! reload's unload cancels the reservation, and a load that finishes for a replaced document unloads
//! again. Commands clone a loaded slot's handle out of the lock, so a slow owner
//! round trip on one slot holds up nobody else.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::{Command, TimedCommand};
use tauri::Emitter;

use super::mode::EngineApp;
use super::EngineHost;
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, EventSink, PluginFormat};
use crate::host::tone::{ToneBinding, ToneHandoff, ToneIdentity, ToneStore};
use crate::host::{ParamDesc, PluginDescriptor, PluginHostState, PluginInfo, ToneImport};

/// How long an unload waits for a command still using the slot's handle (an owner round trip is ≤ 5 s).
const UNLOAD_WAIT: Duration = Duration::from_secs(6);

/// How long the exit waits, in all, for the slots' tone saves before it unloads them anyway.
const EXIT_SAVE_WAIT: Duration = Duration::from_secs(2);

pub(crate) enum EngineSlot {
    Empty,
    /// A load for the document with this epoch is running.
    Loading { epoch: u32 },
    Loaded { handle: Arc<EngineSlotHandle>, info: PluginInfo },
}

/// The `plugin:param-changed` payload.
#[derive(Clone, serde::Serialize)]
struct ParamChanged {
    slot: u8,
    id: u32,
    value: f64,
}

/// Where a load lands and what it reports through: the app's slots, tone store and engine, with the
/// window's events (`EngineApp::plugin_load`).
struct LoadTarget<'a> {
    slots: &'a Mutex<[EngineSlot; lf_engine::SLOT_COUNT]>,
    reload_tones: &'a Mutex<ToneHandoff<{ lf_engine::SLOT_COUNT }>>,
    tones: Option<ToneStore>,
    host: &'a EngineHost,
    /// The main window's HWND (0 = none).
    parent: usize,
    sink: EventSink,
}

impl LoadTarget<'_> {
    fn slots(&self) -> Result<MutexGuard<'_, [EngineSlot; lf_engine::SLOT_COUNT]>, String> {
        self.slots.lock().map_err(|_| "engine slots poisoned".to_string())
    }

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
        {
            let mut slots = self.slots()?;
            let current = state.frontend_epoch.load(Relaxed);
            if frontend_epoch != current {
                return Err(format!("stale frontend epoch {frontend_epoch} (current {current})"));
            }
            match slots[usize::from(slot)] {
                EngineSlot::Empty => slots[usize::from(slot)] = EngineSlot::Loading { epoch: frontend_epoch },
                EngineSlot::Loading { .. } => return Err(format!("slot {slot} is already loading a plugin")),
                EngineSlot::Loaded { .. } => return Err(format!("slot {slot} already has a plugin loaded")),
            }
        }
        let identity = ToneIdentity { format: format.as_str().to_string(), path: path.clone(), id: id.clone() };
        // Past the reservation, so nothing below may return early: a poisoned lock just hands nothing over.
        let imported = self.reload_tones.lock().ok().and_then(|mut tones| tones.take(usize::from(slot), tone_token, &identity));
        let tone = self.tones.clone().map(|store| ToneBinding { store, slot: usize::from(slot), identity, imported });
        let began = Instant::now();
        let loaded = engine_slot::load(format, path.clone(), id.clone(), self.host.slot(usize::from(slot)), self.parent, self.sink.clone(), tone);
        let mut slots = self.slots()?;
        let owns = matches!(slots[usize::from(slot)], EngineSlot::Loading { epoch } if epoch == frontend_epoch)
            && state.frontend_epoch.load(Relaxed) == frontend_epoch;
        let handle = match loaded {
            Ok(handle) => handle,
            Err(e) => {
                if owns {
                    slots[usize::from(slot)] = EngineSlot::Empty;
                }
                return Err(e);
            }
        };
        if !owns {
            drop(slots);
            if let Err(e) = handle.unload() {
                log::error!("[plugin_host] engine slot {slot}: unloading a superseded load: {e}");
            }
            return Err(format!("plugin load for slot {slot} was superseded by a frontend reload"));
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
}

impl EngineApp {
    fn slots(&self) -> Result<MutexGuard<'_, [EngineSlot; lf_engine::SLOT_COUNT]>, String> {
        self.slots.lock().map_err(|_| "engine slots poisoned".to_string())
    }

    /// Per slot: the tone a session import stored for the plugin the slot held, parked for the reload
    /// that follows (`ToneHandoff`). Taken after `slots` when both are held.
    fn reload_tones(&self) -> Result<MutexGuard<'_, ToneHandoff<{ lf_engine::SLOT_COUNT }>>, String> {
        self.reload_tones.lock().map_err(|_| "engine reload tones poisoned".to_string())
    }

    fn handle(&self, slot: u8) -> Result<Arc<EngineSlotHandle>, String> {
        match &self.slots()?[usize::from(slot)] {
            EngineSlot::Loaded { handle, .. } => Ok(handle.clone()),
            _ => Err(format!("no plugin loaded in slot {slot}")),
        }
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
        let parent = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
        let target = LoadTarget { slots: &self.slots, reload_tones: &self.reload_tones, tones: self.tones.clone(), host: &host, parent, sink };
        target.load(state, slot, path, id, frontend_epoch, tone_token)
    }

    /// `plugin_unload`: take the plugin out of the engine and unload it; a load still running for the
    /// slot is cancelled (it unloads what it loaded).
    pub(crate) fn plugin_unload(&self, slot: u8) -> Result<(), String> {
        let previous = std::mem::replace(&mut self.slots()?[usize::from(slot)], EngineSlot::Empty);
        let EngineSlot::Loaded { mut handle, .. } = previous else { return Ok(()) };
        // A command may still hold the handle for an owner round trip: wait it out, then unload here.
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

    /// Every slot's unload, on exit (while the device still plays).
    pub(super) fn unload_all(&self) {
        for slot in 0..lf_engine::SLOT_COUNT as u8 {
            if let Err(e) = self.plugin_unload(slot) {
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

    /// `plugin_set_param`: an empty slot is a no-op (a set racing an unload).
    pub(crate) fn plugin_set_param(&self, slot: u8, id: u32, value: f64) -> Result<(), String> {
        match self.handle(slot) {
            Ok(handle) => handle.set_param(id, value),
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn plugin_list_params(&self, slot: u8) -> Result<Vec<ParamDesc>, String> {
        self.handle(slot)?.list_params()
    }

    /// `plugin_tone_take`: the slot's tone, saved fresh through its owner (it lands in the store too);
    /// the tone file's bytes, empty when the plugin keeps no state.
    pub(crate) fn plugin_tone_take(&self, slot: u8) -> Result<Vec<u8>, String> {
        self.handle(slot)?.take_tone()
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

    pub(crate) fn plugin_open_editor(&self, slot: u8) -> Result<(), String> {
        self.handle(slot)?.open_editor()
    }

    /// Idempotent.
    pub(crate) fn plugin_close_editor(&self, slot: u8) -> Result<(), String> {
        match self.handle(slot) {
            Ok(handle) => handle.close_editor(),
            Err(_) => Ok(()),
        }
    }

    /// `plugin_set_monitor_gain`: the slot's output level, the engine's `SetSlotGain`.
    pub(crate) fn plugin_gain(&self, slot: u8, gain: f32) -> Result<(), String> {
        self.host()?.send(TimedCommand { frame: None, command: Command::SetSlotGain(slot, gain) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_io::test_rig::TestDevice;

    /// A load is refused before it reserves its slot when its path names no format, and a load whose
    /// own loader fails frees the slot it reserved: the next load reaches its loader either way.
    #[test]
    fn a_load_this_host_cannot_start_leaves_the_slot_free_for_the_next() {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let state = PluginHostState::default();
        let epoch = state.begin_frontend_session();
        let slots = Mutex::new(std::array::from_fn(|_| EngineSlot::Empty));
        let reload_tones = Mutex::default();
        let load = |path: &str| {
            let target = LoadTarget { slots: &slots, reload_tones: &reload_tones, tones: None, host: device.host(), parent: 0, sink: Arc::new(|_| {}) };
            target.load(&state, 0, path.to_string(), "some.id".to_string(), epoch, None).map(|info| info.descriptor.name)
        };
        let free = || matches!(slots.lock().unwrap()[0], EngineSlot::Empty);

        let unknown = load(r"C:\plugins\Thing.txt").unwrap_err();
        assert!(unknown.contains("not a .clap, .vst3 or .dll plugin"), "{unknown}");
        assert!(free(), "an unknown extension reserves nothing");
        assert!(load(r"C:\plugins\no-extension").is_err());
        assert!(free());

        // Each format gets as far as its own loader, which finds no file, and the slot takes a load again.
        let missing = load(r"C:\no-such-folder\Missing.DLL").unwrap_err();
        assert!(missing.starts_with("LoadLibraryW"), "{missing}");
        assert!(free(), "the failed VST2 load gave its reservation back");
        let missing = load(r"C:\no-such-folder\Missing.clap").unwrap_err();
        assert!(missing.starts_with("load failed"), "{missing}");
        assert!(free());
        let missing = load(r"C:\no-such-folder\Missing.vst3").unwrap_err();
        assert!(!missing.contains("already") && !missing.contains("not a .clap"), "{missing}");
        assert!(free());
    }
}
