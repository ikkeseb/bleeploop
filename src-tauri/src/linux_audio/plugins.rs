//! OWNS: the Linux app's plugin slots: each slot's handshake with the engine (`SlotHost`, the API the
//! shared slot owner in `host/engine_slot.rs` drives) and the `plugin_*` commands, routed to the same
//! CLAP and VST3 owners the Windows app runs (`host/clap_engine.rs`, `host/vst3_engine.rs`).
//!
//! The rules are the Windows app's (`engine_io/plugins.rs`, `engine_io/slot_host.rs`), trimmed to what
//! the Linux app has: no chain units, no VST2. A slot seats one plugin unit at a time, under an id
//! minted per install. The engine never drops a unit: one it hands back (a removal, a refusal, or a new
//! engine at another sample rate evicting it) waits in its owner's mailbox, and an owner that gave up on
//! its unit (`abandon`) leaks what comes back rather than unload code it may still run. While JACK runs
//! the engine services the ports at each block start; while none does, the host services them at once
//! on the idle engine. A load waits for a device: the unit is activated at the engine's rate.
//!
//! Lock order (`mod.rs`): `run` → `settings` → `ports` → `ends`; `SlotHost` never holds `ports` while it
//! takes `run`.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lf_engine::{Command, Engine, Left, SeatRefusal, SlotPort, TimedCommand, UnitId, UnitProcessor, SLOT_COUNT};
use tauri::Emitter;

use super::{lock, Core, EngineHost};
use crate::host::engine_slot::{self, EngineSlotEvent, EngineSlotHandle, EventSink, PluginFormat};
use crate::host::tone::{ToneBinding, ToneHandoff, ToneIdentity, ToneKey, ToneStore};
use crate::host::{ParamDesc, PluginDescriptor, PluginHostState, PluginInfo, ToneImport};

/// The engine terms a unit is activated at (`engine_io::unit_host::Terms` on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terms {
    pub generation: u64,
    pub rate: u32,
    pub max_block: usize,
}

type Unit = Box<dyn UnitProcessor>;

/// The largest block the engine hands a unit (`EngineConfig::new`'s `max_block`).
const MAX_BLOCK: usize = 4096;
/// How long `install` waits for the engine to answer.
const SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(1);
/// How long a load waits for a slot that is still clearing (a plugin's release can take seconds).
const VACATE_WAIT: Duration = Duration::from_secs(30);
const UNLOAD_WAIT: Duration = Duration::from_secs(6);
const LIVE_OFF_WAIT: Duration = Duration::from_secs(1);
const EXIT_SAVE_WAIT: Duration = Duration::from_secs(2);

/// One unit the engine handed back, for its owner: why it left when that was a refusal.
struct Mail {
    token: u64,
    unit: Unit,
    refused: Option<SeatRefusal>,
}

/// The engine's slot ports and who holds what in them.
#[derive(Default)]
pub(super) struct SlotPorts {
    ports: Option<[SlotPort; SLOT_COUNT]>,
    /// Per slot, the owner (its token) and the unit id it has in the engine. An abandoned owner's
    /// stays until its unit comes back, so no other load takes the slot while the engine holds it.
    seated: [Option<(u64, UnitId)>; SLOT_COUNT],
    /// Units the engine handed back, for their owners to take.
    mailbox: VecDeque<Mail>,
    /// Owners that gave up on their unit: whatever comes back for them leaks.
    abandoned: Vec<u64>,
    next_id: UnitId,
    next_token: u64,
}

impl SlotPorts {
    /// Move everything the engine handed back into the owners' mailbox.
    fn reap(&mut self) {
        let Some(ports) = self.ports.as_mut() else { return };
        for (slot, port) in ports.iter_mut().enumerate() {
            while let Some(back) = port.returned_unit() {
                let owner = self.seated[slot].filter(|&(_, id)| id == back.id).map(|(token, _)| token);
                if owner.is_some() {
                    self.seated[slot] = None;
                }
                match owner {
                    Some(token) if !self.abandoned.contains(&token) => {
                        let refused = match back.left {
                            Left::Refused(why) => Some(why),
                            _ => None,
                        };
                        self.mailbox.push_back(Mail { token, unit: back.unit, refused });
                    }
                    // Abandoned, or no owner known: the unit may still be inside its plugin's code.
                    _ => std::mem::forget(back.unit),
                }
            }
        }
    }

    fn take_mail(&mut self, token: u64) -> Option<Mail> {
        let at = self.mailbox.iter().position(|m| m.token == token)?;
        self.mailbox.remove(at)
    }

    /// A new engine's ports (`None`: no engine), after evicting every unit from the one it replaces
    /// into its owner's mailbox. An engine that panics while evicting is leaked with its units.
    pub(super) fn replace(&mut self, old: Option<Box<Engine>>, new: Option<[SlotPort; SLOT_COUNT]>) {
        if let Some(mut engine) = old {
            let evicted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for _ in 0..8 {
                    engine.evict_slots();
                    self.reap();
                }
            }));
            if evicted.is_err() || self.seated.iter().any(Option::is_some) {
                log::error!("[plugin_host] an engine kept a plugin unit on replacement: leaked with it");
                std::mem::forget(engine);
            }
        } else {
            self.reap();
        }
        self.seated = Default::default();
        self.ports = new;
    }
}

/// The ports' lock: taken before `Core::ends` wherever both are held (a rebuild publishes the new
/// rate and the new ports under it), after `Core::run`.
pub(super) fn ports(core: &Core) -> MutexGuard<'_, SlotPorts> {
    lock(&core.ports)
}

/// A new engine replaces `old` (another sample rate, or a fault): its units go back to their owners,
/// who re-activate them at the new engine's rate once it exists. Called with `Core::run` held.
pub(super) fn replace_ports(core: &Core, old: Option<Box<Engine>>, new: Option<[SlotPort; SLOT_COUNT]>) {
    ports(core).replace(old, new);
}

/// One slot's handshake with the engine. Cheap to clone; one plugin owner uses a slot at a time.
#[derive(Clone)]
pub struct SlotHost {
    core: Arc<Core>,
    slot: usize,
    token: u64,
}

impl SlotHost {
    pub(super) fn new(core: Arc<Core>, slot: usize) -> SlotHost {
        let token = {
            let mut ports = ports(&core);
            ports.next_token += 1;
            ports.next_token
        };
        SlotHost { core, slot, token }
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The engine's sample rate: what a unit is activated at. `None` before a device first opened.
    pub fn rate(&self) -> Option<u32> {
        self.core.rate()
    }

    pub fn max_block(&self) -> usize {
        MAX_BLOCK
    }

    /// While no device runs, apply the ports' messages on the idle engine at once. An engine that
    /// panics there is faulted as a callback's would be: its units go back to their owners.
    fn service_if_idle(&self) {
        let mut run = lock(&self.core.run);
        if run.device.is_some() {
            return;
        }
        let Some(engine) = run.idle.as_mut() else { return };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.service_slots_idle())).is_ok() {
            return;
        }
        log::error!("[plugin_host] the idle engine panicked while it serviced its slots");
        let engine = run.idle.take();
        run.events.push(super::DeviceEvent::EngineFaulted);
        run.frame = 0;
        let mut ports = ports(&self.core);
        *lock(&self.core.ends) = None;
        self.core.engine_gen.fetch_add(1, std::sync::atomic::Ordering::Release);
        ports.replace(engine, None);
    }

    /// Hand a unit activated at `rate` to the engine. A unit for an engine a rebuild has since
    /// replaced goes to `take_evicted`, for its owner to re-activate. Gives the unit back with the
    /// reason when no engine exists, the slot already holds a unit, the port is full or the engine
    /// refused it.
    pub fn install(&self, unit: Unit, rate: u32) -> Result<(), (Unit, String)> {
        let (id, ticket) = {
            let mut ports = ports(&self.core);
            ports.reap();
            if ports.ports.is_none() {
                return Err((unit, "no audio device is open".to_string()));
            }
            if ports.seated[self.slot].is_some() {
                return Err((unit, format!("slot {} already holds a unit", self.slot)));
            }
            // Under the ports' lock: a rebuild publishes its rate and its ports under it.
            if self.core.rate() != Some(rate) {
                ports.mailbox.push_back(Mail { token: self.token, unit, refused: None });
                return Ok(());
            }
            ports.next_id += 1;
            let id = ports.next_id;
            let port = &mut ports.ports.as_mut().expect("checked above")[self.slot];
            let ticket = match port.install(id, unit) {
                Ok(ticket) => ticket,
                Err(unit) => return Err((unit, format!("slot {}'s port is full", self.slot))),
            };
            ports.seated[self.slot] = Some((self.token, id));
            (id, ticket)
        };
        let deadline = Instant::now() + SETTLE;
        loop {
            self.service_if_idle();
            {
                let mut ports = ports(&self.core);
                // The answer first: the engine hands a refused unit back before it answers.
                let answered = ticket.answered();
                ports.reap();
                let refused = ports.mailbox.iter().position(|m| m.token == self.token && m.refused.is_some());
                if let Some(at) = refused {
                    let mail = ports.mailbox.remove(at).expect("found above");
                    let why = mail.refused.expect("found above");
                    return Err((mail.unit, format!("the engine refused slot {}'s unit: {why:?}", self.slot)));
                }
                if answered || ports.seated[self.slot] != Some((self.token, id)) {
                    // Applied, held for a take (it applies after), or evicted by a rebuild already.
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                log::warn!("[plugin_host] slot {}: the engine did not take unit {id} within {} ms; it stands as sent", self.slot, SETTLE.as_millis());
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    /// Take the slot's unit back: the engine releases its notes, crossfades it out and stops it.
    /// `Ok(None)` when the slot holds none of this owner's; `Err` when it does not come back within
    /// `timeout` (it stays the engine's; a later `remove` or `take_evicted` can still recover it). A
    /// removal the engine holds for a take waits for the take to end.
    pub fn remove(&self, timeout: Duration) -> Result<Option<Unit>, String> {
        {
            let mut ports = ports(&self.core);
            ports.reap();
            if let Some(mail) = ports.take_mail(self.token) {
                return Ok(Some(mail.unit));
            }
            let Some((token, id)) = ports.seated[self.slot] else { return Ok(None) };
            if token != self.token {
                return Ok(None);
            }
            let port = ports.ports.as_mut().ok_or("no engine holds this slot")?;
            if !port[self.slot].remove(id) {
                return Err(format!("slot {}'s port is full", self.slot));
            }
        }
        let mut deadline = Instant::now() + timeout.max(Duration::from_millis(500));
        loop {
            self.service_if_idle();
            {
                let mut ports = ports(&self.core);
                ports.reap();
                if let Some(mail) = ports.take_mail(self.token) {
                    return Ok(Some(mail.unit));
                }
                if ports.ports.as_ref().is_some_and(|p| p[self.slot].holding()) {
                    deadline = Instant::now() + timeout;
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {}'s unit did not come back within {} ms", self.slot, timeout.as_millis()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// This owner's unit, if the engine handed it back unasked (a rebuild at another rate, a late
    /// refusal): re-activate it at `rate` and install it again. The owner polls this every turn. While
    /// no engine exists (after a fault) the unit waits here for the next one, so it is not activated
    /// for no engine and parked.
    pub fn take_evicted(&self) -> Option<Unit> {
        let mut ports = ports(&self.core);
        ports.reap();
        if ports.ports.is_none() || self.core.rate().is_none() {
            return None;
        }
        ports.take_mail(self.token).map(|mail| mail.unit)
    }

    /// Give up on this owner's unit (a `remove` timed out and the owner leaks its plugin): whenever it
    /// comes back it leaks too. The slot stays taken until it does.
    pub fn abandon(&self) {
        let mut ports = ports(&self.core);
        ports.abandoned.push(self.token);
        while let Some(mail) = ports.take_mail(self.token) {
            std::mem::forget(mail.unit);
        }
    }
}

// ---- the `plugin_*` commands -------------------------------------------------------------------

pub(crate) enum EngineSlot {
    Empty,
    /// A load for the document with this epoch is running; `cancelled`: an unload asked meanwhile.
    Loading { epoch: u32, cancelled: bool },
    Loaded { handle: Arc<EngineSlotHandle>, info: PluginInfo },
    /// On its way out: its unit is in the engine until the teardown ends.
    Leaving,
}

/// The app's plugin slots, tone store and the tones parked for a session import's reload.
pub(crate) struct Slots {
    host: EngineHost,
    slots: Mutex<[EngineSlot; SLOT_COUNT]>,
    tones: Option<ToneStore>,
    reload_tones: Mutex<ToneHandoff<SLOT_COUNT>>,
}

#[derive(Clone, serde::Serialize)]
struct ParamChanged {
    slot: u8,
    id: u32,
    value: f64,
}

impl Slots {
    pub(super) fn new(host: EngineHost, tones_dir: Option<std::path::PathBuf>) -> Slots {
        Slots {
            host,
            slots: Mutex::new([EngineSlot::Empty, EngineSlot::Empty]),
            tones: tones_dir.map(ToneStore::new),
            reload_tones: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, [EngineSlot; SLOT_COUNT]> {
        lock(&self.slots)
    }

    fn reserve(&self, state: &PluginHostState, slot: u8, epoch: u32) -> Result<(), String> {
        let deadline = Instant::now() + VACATE_WAIT;
        loop {
            {
                let mut slots = self.lock();
                current_document(state, epoch)?;
                match slots[usize::from(slot)] {
                    EngineSlot::Empty => {
                        slots[usize::from(slot)] = EngineSlot::Loading { epoch, cancelled: false };
                        return Ok(());
                    }
                    EngineSlot::Loading { epoch: e, cancelled: false } if e == epoch => {
                        return Err(format!("slot {slot} is already loading a plugin"));
                    }
                    EngineSlot::Loaded { .. } => return Err(format!("slot {slot} already has a plugin loaded")),
                    EngineSlot::Loading { .. } | EngineSlot::Leaving => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {slot} is still clearing the plugin before it"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `plugin_load`: load plugin `id` from `path` into `slot` for the document with `epoch`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn load(
        &self,
        state: &PluginHostState,
        window: &tauri::WebviewWindow,
        slot: u8,
        path: String,
        id: String,
        epoch: u32,
        tone_token: Option<u32>,
    ) -> Result<PluginInfo, String> {
        let format = PluginFormat::of_path(std::path::Path::new(&path)).ok_or_else(|| format!("{path}: not a .clap or .vst3 plugin"))?;
        self.reserve(state, slot, epoch)?;
        let identity = ToneIdentity { format: format.as_str().to_string(), path: path.clone(), id: id.clone() };
        let imported = self.reload_tones.lock().ok().and_then(|mut t| t.take(usize::from(slot), tone_token, &identity));
        let tone = self.tones.clone().map(|store| ToneBinding { store, key: ToneKey::Slot { slot: usize::from(slot), identity }, imported });
        let events = window.clone();
        let sink: EventSink = Arc::new(move |event| {
            let _ = match event {
                EngineSlotEvent::ParamChanged { id, value } => events.emit("plugin:param-changed", ParamChanged { slot, id, value }),
                EngineSlotEvent::ParamsChanged => events.emit("plugin:params-changed", slot),
                EngineSlotEvent::EditorClosed => events.emit("plugin:editor-closed", slot),
            };
        });
        let began = Instant::now();
        let slot_host = SlotHost::new(self.host.core.clone(), usize::from(slot));
        let loaded = engine_slot::load(format, path.clone(), id.clone(), slot_host, 0, sink, tone);
        let mut slots = self.lock();
        let wanted = matches!(slots[usize::from(slot)], EngineSlot::Loading { cancelled: false, .. })
            && state.frontend_epoch.load(Relaxed) == epoch;
        let handle = match loaded {
            Ok(handle) => handle,
            Err(e) => {
                slots[usize::from(slot)] = EngineSlot::Empty;
                return Err(e);
            }
        };
        if !wanted {
            slots[usize::from(slot)] = EngineSlot::Leaving;
            drop(slots);
            if let Err(e) = handle.unload() {
                log::error!("[plugin_host] slot {slot}: unloading a superseded load: {e}");
            }
            self.vacate(slot);
            return Err(format!("plugin load for slot {slot} was superseded by a frontend reload or an unload"));
        }
        let descriptor = PluginDescriptor { id, name: handle.name().to_string(), format: format.as_str().to_string(), path, is_effect: None };
        let info = PluginInfo { slot, descriptor, tone: handle.tone() };
        log::info!("[plugin_host] slot {slot}: {} ({:?}) loaded in {} ms", info.descriptor.name, handle.kind(), began.elapsed().as_millis());
        slots[usize::from(slot)] = EngineSlot::Loaded { handle: Arc::new(handle), info: info.clone() };
        Ok(info)
    }

    fn vacate(&self, slot: u8) {
        let mut slots = self.lock();
        if matches!(slots[usize::from(slot)], EngineSlot::Leaving) {
            slots[usize::from(slot)] = EngineSlot::Empty;
        }
    }

    fn loaded(&self, slot: u8, document: Option<(&PluginHostState, u32)>) -> Result<Option<Arc<EngineSlotHandle>>, String> {
        let slots = self.lock();
        if let Some((state, epoch)) = document {
            current_document(state, epoch)?;
        }
        Ok(match &slots[usize::from(slot)] {
            EngineSlot::Loaded { handle, .. } => Some(handle.clone()),
            _ => None,
        })
    }

    fn handle(&self, slot: u8, document: Option<(&PluginHostState, u32)>) -> Result<Arc<EngineSlotHandle>, String> {
        self.loaded(slot, document)?.ok_or_else(|| format!("no plugin loaded in slot {slot}"))
    }

    /// Take `slot`'s plugin out and unload it; with `host`, the slot goes off live first and the
    /// plugin stays loaded when it cannot.
    /// `silence`: take the slot off live first (`EngineHost::silence_slot`).
    pub(crate) fn unload(&self, slot: u8, document: Option<(&PluginHostState, u32)>, silence: bool) -> Result<(), String> {
        if silence {
            let host = &self.host;
            // A replaced document's unload must not take the new one's slot off live.
            let loaded = {
                let slots = self.lock();
                if let Some((state, epoch)) = document {
                    current_document(state, epoch)?;
                }
                matches!(slots[usize::from(slot)], EngineSlot::Loaded { .. })
            };
            if loaded {
                host.silence_slot(slot, LIVE_OFF_WAIT)
                    .map_err(|e| format!("the plugin in slot {slot} stays loaded, as its live input could not be stopped first: {e}"))?;
            }
        }
        let handle = {
            let mut slots = self.lock();
            if let Some((state, epoch)) = document {
                current_document(state, epoch)?;
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
        self.vacate(slot);
        unloaded
    }

    pub(crate) fn list_loaded(&self) -> Vec<PluginInfo> {
        self.lock()
            .iter()
            .filter_map(|s| match s {
                EngineSlot::Loaded { info, .. } => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn set_param(&self, state: &PluginHostState, epoch: u32, slot: u8, id: u32, value: f64) -> Result<(), String> {
        match self.loaded(slot, Some((state, epoch)))? {
            Some(handle) => handle.set_param(id, value),
            None => Ok(()),
        }
    }

    pub(crate) fn list_params(&self, slot: u8) -> Result<Vec<ParamDesc>, String> {
        self.handle(slot, None)?.list_params()
    }

    pub(crate) fn tone_take(&self, slot: u8) -> Result<Vec<u8>, String> {
        self.handle(slot, None)?.take_tone()
    }

    pub(crate) fn tone_import(&self, slot: u8, bytes: &[u8], expected: &ToneIdentity) -> Result<ToneImport, String> {
        let store = self.tones.as_ref().ok_or("no app-local data folder to keep tones in")?;
        let imported = store.import(usize::from(slot), bytes, expected)?;
        let (name, ToneIdentity { format, path, id }) = (imported.tone.name.clone(), imported.tone.identity.clone());
        let reload_token = {
            let slots = self.lock();
            let held = matches!(&slots[usize::from(slot)], EngineSlot::Loaded { info, .. } if {
                let d = &info.descriptor;
                d.format == expected.format && d.path == expected.path && d.id == expected.id
            });
            if held { Some(lock(&self.reload_tones).park(usize::from(slot), imported)) } else { None }
        };
        Ok(ToneImport { reload_token, name, format, path, id })
    }

    pub(crate) fn tone_forget(&self, slot: u8, token: u32) {
        lock(&self.reload_tones).forget(usize::from(slot), token);
    }

    pub(crate) fn open_editor(&self, state: &PluginHostState, epoch: u32, slot: u8) -> Result<(), String> {
        self.handle(slot, Some((state, epoch)))?.open_editor()
    }

    pub(crate) fn close_editor(&self, state: &PluginHostState, epoch: u32, slot: u8) -> Result<(), String> {
        match self.loaded(slot, Some((state, epoch)))? {
            Some(handle) => handle.close_editor(),
            None => Ok(()),
        }
    }

    /// On exit, before the unloads: every loaded slot saves its tone (bounded in all).
    pub(super) fn save_tones_on_exit(&self) {
        let loaded: Vec<(u8, Arc<EngineSlotHandle>)> = (0u8..)
            .zip(self.lock().iter())
            .filter_map(|(slot, s)| match s {
                EngineSlot::Loaded { handle, .. } => Some((slot, handle.clone())),
                _ => None,
            })
            .collect();
        let asked: Vec<_> = loaded.iter().map(|(slot, handle)| (*slot, handle.start_tone_save())).collect();
        let deadline = Instant::now() + EXIT_SAVE_WAIT;
        for (slot, asked) in asked {
            let saved = asked.and_then(|reply| {
                reply
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| format!("no answer within {} ms", EXIT_SAVE_WAIT.as_millis()))?
            });
            if let Err(e) = saved {
                log::error!("[plugin_host] slot {slot}: the tone save on exit failed: {e}");
            }
        }
    }

    pub(super) fn unload_all(&self) {
        for slot in 0..SLOT_COUNT as u8 {
            if let Err(e) = self.unload(slot, None, true).or_else(|_| self.unload(slot, None, false)) {
                log::error!("[plugin_host] slot {slot}: unload on exit: {e}");
            }
        }
    }
}

fn current_document(state: &PluginHostState, epoch: u32) -> Result<(), String> {
    let current = state.frontend_epoch.load(Relaxed);
    if epoch == current { Ok(()) } else { Err(format!("stale frontend epoch {epoch} (current {current})")) }
}

/// A command may still hold the handle for an owner round trip: wait it out, then unload.
fn unload_handle(slot: u8, mut handle: Arc<EngineSlotHandle>) -> Result<(), String> {
    let deadline = Instant::now() + UNLOAD_WAIT;
    loop {
        match Arc::try_unwrap(handle) {
            Ok(handle) => return handle.unload(),
            Err(shared) if Instant::now() >= deadline => {
                log::warn!("[plugin_host] slot {slot}: still in use; the last user unloads it");
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

impl EngineHost {
    /// Take `slot` off live and wait (≤ `timeout`) for the engine's gate on it to close, so a plugin
    /// that leaves does not hand the dry input to the monitor and the record path (the Windows
    /// host's `EngineHost::silence_slot`). While a device runs, a block the engine rendered after
    /// taking the live-off answers it; with none, the idle engine shuts the gate at once until the
    /// live-off applies; while a route is in transit (neither), it waits for one of the two; with no
    /// engine at all, the next one replays the live-off before its first block.
    pub(super) fn silence_slot(&self, slot: u8, timeout: Duration) -> Result<(), String> {
        self.send(TimedCommand { frame: None, command: Command::SetSlotLive(slot, false) })
            .map_err(|()| "the engine's command ring is full".to_string())?;
        // The engine and the ring's count the answer must reach, read together: a rebuild since the
        // send replayed the live-off within its own count.
        let mut target = {
            let ends = lock(&self.core.ends);
            match ends.as_ref() {
                Some(e) => (self.core.engine_gen.load(Acquire), e.pushed),
                None => return Ok(()),
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut run = lock(&self.core.run);
                if run.device.is_none() {
                    if let Some(engine) = run.idle.as_mut() {
                        let ends = lock(&self.core.ends);
                        let gen = self.core.engine_gen.load(Acquire);
                        let Some(e) = ends.as_ref() else { return Ok(()) };
                        let taken = if gen == target.0 { target.1 } else { e.pushed };
                        engine.close_slot_idle(usize::from(slot), taken);
                        return Ok(());
                    }
                }
            }
            let running = lock(&self.core.run).device.is_some();
            {
                let ends = lock(&self.core.ends);
                let Some(e) = ends.as_ref() else { return Ok(()) };
                let gen = self.core.engine_gen.load(Acquire);
                if gen != target.0 {
                    target = (gen, e.pushed);
                }
                if running && e.overview.slot_gate_closed(usize::from(slot), target.1) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("slot {slot}'s live input did not close within {} ms", timeout.as_millis()));
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Real plugins through the real host, rendered offline on the idle engine (no device, nothing
/// sounds). Each test needs its plugin built first (`docs/VERIFY.md § Linux plugin hosting`), named by an
/// environment variable; run them with `--ignored`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tone::{ToneBinding, ToneIdentity, ToneKey, ToneStore};
    use lf_engine::{EngineConfig, ProcessContext};

    const RATE: u32 = 48_000;
    const BLOCK: usize = 256;
    const DRY: f32 = 0.125;

    /// The host's own log lines on stderr (`--nocapture` shows them), from the first rig on.
    struct StderrLog;

    impl log::Log for StderrLog {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
            }
        }
        fn flush(&self) {}
    }

    fn init_log() {
        static LOG: StderrLog = StderrLog;
        if log::set_logger(&LOG).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }

    struct Rig {
        host: EngineHost,
        frame: i64,
    }

    impl Rig {
        fn new(rate: u32) -> Rig {
            init_log();
            let host = EngineHost::default();
            let rig = Rig { host, frame: 0 };
            rig.engine_at(rate);
            rig.host
                .send(TimedCommand { frame: None, command: Command::SetSlotLive(0, true) })
                .expect("the ring takes GO LIVE");
            rig
        }

        /// A new engine at `rate`, as a device open at another rate builds one: the old engine's
        /// units go back to their owners.
        fn engine_at(&self, rate: u32) {
            let (engine, handle) = Engine::new(EngineConfig::new(rate));
            let (mut ends, ports) = super::super::engine_ends(handle, rate);
            for command in lock(&self.host.core.settings).replay() {
                ends.commands.push(TimedCommand { frame: None, command }).unwrap();
                ends.pushed += 1;
            }
            let mut run = lock(&self.host.core.run);
            *lock(&self.host.core.ends) = Some(ends);
            let old = run.idle.replace(Box::new(engine));
            replace_ports(&self.host.core, old, Some(ports));
        }

        /// Render `blocks` blocks of a steady `DRY` on slot 0's input; the left output's mean over the
        /// last one.
        fn render(&mut self, blocks: usize) -> f32 {
            let input = [DRY; BLOCK];
            let silent = [0.0; BLOCK];
            let mut mean = 0.0;
            for _ in 0..blocks {
                let (mut l, mut r) = ([0.0; BLOCK], [0.0; BLOCK]);
                {
                    let mut run = lock(&self.host.core.run);
                    let engine = run.idle.as_mut().expect("an idle engine");
                    let cx = ProcessContext { frame: self.frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                    engine.process_inputs(&cx, [[&input, &input], [&silent, &silent]], &mut l, &mut r);
                }
                self.frame += BLOCK as i64;
                mean = l.iter().sum::<f32>() / BLOCK as f32;
                // The owner thread services its slot between blocks, as it would beside a device.
                std::thread::yield_now();
            }
            mean
        }

        fn load(&self, format: PluginFormat, path: &str, id: &str, tone: Option<ToneBinding>) -> EngineSlotHandle {
            self.load_with(format, path, id, tone, Arc::new(|_| {}))
        }

        fn load_with(&self, format: PluginFormat, path: &str, id: &str, tone: Option<ToneBinding>, sink: EventSink) -> EngineSlotHandle {
            let slot = SlotHost::new(self.host.core.clone(), 0);
            engine_slot::load(format, path.to_string(), id.to_string(), slot, 0, sink, tone).expect("the plugin loads")
        }

        fn send(&self, command: Command) {
            self.host.send(TimedCommand { frame: None, command }).expect("the ring takes the command");
        }

        /// Render blocks for `time`, letting the slot's owner run its turns meanwhile.
        fn run_for(&mut self, time: Duration) {
            let until = Instant::now() + time;
            while Instant::now() < until {
                self.render(4);
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Render until `done` holds, at most `time`; whether it did.
        fn run_until(&mut self, time: Duration, mut done: impl FnMut(&mut Rig) -> bool) -> bool {
            let until = Instant::now() + time;
            while Instant::now() < until {
                if done(self) {
                    return true;
                }
                self.render(4);
                std::thread::sleep(Duration::from_millis(5));
            }
            done(self)
        }

        /// The left output's peak over `blocks` blocks.
        fn peak(&mut self, blocks: usize) -> f32 {
            let mut peak = 0.0f32;
            for _ in 0..blocks {
                let input = [0.0; BLOCK];
                let (mut l, mut r) = ([0.0; BLOCK], [0.0; BLOCK]);
                {
                    let mut run = lock(&self.host.core.run);
                    let engine = run.idle.as_mut().expect("an idle engine");
                    let cx = ProcessContext { frame: self.frame, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
                    engine.process_inputs(&cx, [[&input, &input], [&input, &input]], &mut l, &mut r);
                }
                self.frame += BLOCK as i64;
                peak = l.iter().fold(peak, |p, s| p.max(s.abs()));
                std::thread::yield_now();
            }
            peak
        }
    }

    fn ratio(wet: f32, dry: f32) -> f32 {
        wet / dry
    }

    fn assert_near(got: f32, want: f32, what: &str) {
        eprintln!("{what}: x{got:.4} (want x{want})");
        assert!((got - want).abs() < 0.02, "{what}: wanted ×{want}, got ×{got}");
    }

    /// The test plugin's whole life on Linux: the live input passes through it (×0.5 at its default),
    /// a host-set parameter reaches the audio (×1.5), its tone is saved at the unload and restored at
    /// the next load before it plays, and a new engine at another rate gets it back re-activated.
    /// `x1_5`: the value the plugin's GAIN takes for ×1.5 (CLAP: plain; VST3: normalized).
    fn gain_plugin_round_trip(format: PluginFormat, env: &str, id: &str, x1_5: f64) {
        let path = std::env::var(env).unwrap_or_else(|_| panic!("{env} names the built test plugin (docs/VERIFY.md § Linux plugin hosting)"));
        let tones = std::env::temp_dir().join(format!("lf-linux-plugin-test-{}-{}", format.as_str(), std::process::id()));
        let _ = std::fs::remove_dir_all(&tones);
        let binding = || ToneBinding {
            store: ToneStore::new(tones.clone()),
            key: ToneKey::Slot { slot: 0, identity: ToneIdentity { format: format.as_str().into(), path: path.clone(), id: id.into() } },
            imported: None,
        };
        let mut rig = Rig::new(RATE);
        let dry = rig.render(200);
        assert!((dry - DRY).abs() < 0.01, "the empty live slot passes its input dry: {dry}");

        let handle = rig.load(format, &path, id, Some(binding()));
        let params = handle.list_params().expect("the plugin lists its parameters");
        let gain = params.iter().find(|p| p.name == "Gain").expect("a Gain parameter");
        assert_eq!(gain.id, 7, "the plugin's own parameter id");
        assert_near(ratio(rig.render(200), dry), 0.5, "through the plugin at its default");

        handle.set_param(gain.id, x1_5).expect("a set parameter");
        assert_near(ratio(rig.render(200), dry), 1.5, "after a host-set parameter");

        // Another sample rate: the old engine evicts the unit, its owner re-activates it at the new
        // rate and installs it into the new engine (polled every 20 ms).
        rig.engine_at(44_100);
        let mut re_rated = 0.0;
        for _ in 0..100 {
            re_rated = ratio(rig.render(20), dry);
            if (re_rated - 1.5).abs() < 0.02 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_near(re_rated, 1.5, "after the engine was rebuilt at 44.1 kHz");

        let unloading = std::thread::spawn(move || handle.unload());
        while !unloading.is_finished() {
            rig.render(1);
        }
        unloading.join().unwrap().expect("the unload");
        assert_near(ratio(rig.render(200), dry), 1.0, "after the unload, dry again");

        let handle = rig.load(format, &path, id, Some(binding()));
        assert!(handle.tone().is_some(), "the load reports a restored tone");
        assert_near(ratio(rig.render(200), dry), 1.5, "the reload plays its restored tone");
        let unloading = std::thread::spawn(move || handle.unload());
        while !unloading.is_finished() {
            rig.render(1);
        }
        unloading.join().unwrap().expect("the second unload");
        let _ = std::fs::remove_dir_all(&tones);
    }

    /// A stand-in unit: passes its input on, with a latency of its own choosing.
    struct Stub(lf_engine::grid::Frame);
    impl UnitProcessor for Stub {
        fn info(&self) -> lf_engine::UnitInfo {
            lf_engine::UnitInfo { kind: lf_engine::UnitKind::Plugin, source: false, latency: self.0 }
        }
        fn process(&mut self, _cx: &lf_engine::UnitCx, input: [&[f32]; 2], _e: &[lf_engine::SlotEvent], out: [&mut [f32]; 2]) {
            let [l, r] = out;
            l.copy_from_slice(input[0]);
            r.copy_from_slice(input[1]);
        }
        fn stop(&mut self) {}
        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
            self
        }
    }

    /// A unit the engine refuses (its latency past the chain's bound) comes back to the load as an
    /// error with the unit, and the slot stays free for the next.
    #[test]
    fn a_refused_install_answers_an_error_and_leaves_the_slot_free() {
        let rig = Rig::new(RATE);
        let slot = SlotHost::new(rig.host.core.clone(), 0);
        let too_late = lf_engine::max_chain_latency(RATE) + 1;
        let Err((unit, why)) = slot.install(Box::new(Stub(too_late)), RATE) else { panic!("the engine took it") };
        assert!(why.contains("refused") && why.contains("Latency"), "{why}");
        assert_eq!(unit.info().latency, too_late, "the unit comes back to its owner");
        assert!(slot.install(Box::new(Stub(0)), RATE).is_ok(), "the slot is free for the next load");
        assert!(matches!(slot.remove(Duration::from_secs(2)), Ok(Some(_))), "comes back");
    }

    /// After a fault leaves no engine, an owner's unit waits instead of being activated for none;
    /// the next engine gets it back for its owner to re-activate there.
    #[test]
    fn a_unit_waits_out_a_fault_for_the_next_engine() {
        let rig = Rig::new(RATE);
        let slot = SlotHost::new(rig.host.core.clone(), 0);
        assert!(slot.install(Box::new(Stub(0)), RATE).is_ok(), "installed");
        {
            let mut run = lock(&rig.host.core.run);
            *lock(&rig.host.core.ends) = None;
            let engine = run.idle.take();
            replace_ports(&rig.host.core, engine, None);
        }
        assert!(slot.take_evicted().is_none(), "no engine: the unit waits");
        assert!(slot.install(Box::new(Stub(0)), RATE).is_err(), "no engine takes a unit");
        rig.engine_at(44_100);
        let back = slot.take_evicted().expect("the next engine hands it back");
        assert!(slot.install(back, 44_100).is_ok(), "re-installed in the new engine");
        assert!(matches!(slot.remove(Duration::from_secs(2)), Ok(Some(_))), "comes back");
    }

    #[test]
    #[ignore = "needs LF_TEST_CLAP: docs/VERIFY.md § Linux plugin hosting"]
    fn a_clap_plugin_plays_takes_its_param_and_restores_its_tone() {
        gain_plugin_round_trip(PluginFormat::Clap, "LF_TEST_CLAP", "dev.bleeploop.test-gain", 1.5);
    }

    /// The CLAP test plugin's 10 ms timer and its fd (a socket pair it writes on each tick) run on its
    /// owner's loop with no editor open and no display: both counts rise while it is loaded, and the
    /// unload ends them with no callback after `destroy` (the plugin aborts the process on one).
    #[test]
    #[ignore = "needs LF_TEST_CLAP: docs/VERIFY.md § Linux plugin hosting"]
    fn a_clap_plugins_timer_and_fd_run_on_its_owner_loop_without_an_editor() {
        let path = std::env::var("LF_TEST_CLAP").expect("LF_TEST_CLAP names the built test plugin (docs/VERIFY.md § Linux plugin hosting)");
        timer_and_fd_round_trip(&path, None);
    }

    /// An instance that leaves its timer and fd registered at `destroy`, on an owner that lives on: the
    /// test plugin refuses the stored tone `LEAKREGS` and leaks its registrations, and the owner
    /// destroys it and creates the next instance. The host must forget them: the plugin aborts the
    /// process on a callback for a registration the new instance does not hold.
    #[test]
    #[ignore = "needs LF_TEST_CLAP: docs/VERIFY.md § Linux plugin hosting"]
    fn a_clap_plugins_leftover_timer_and_fd_are_forgotten_at_its_destroy() {
        use crate::host::tone::{encode, Tone};
        let path = std::env::var("LF_TEST_CLAP").expect("LF_TEST_CLAP names the built test plugin (docs/VERIFY.md § Linux plugin hosting)");
        let tones = std::env::temp_dir().join(format!("lf-linux-plugin-leak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tones);
        let identity = ToneIdentity { format: PluginFormat::Clap.as_str().into(), path: path.clone(), id: "dev.bleeploop.test-gain".into() };
        let store = ToneStore::new(tones.clone());
        let leak = Tone { identity: identity.clone(), name: "LF Test Gain".into(), state: b"LEAKREGS".to_vec() };
        store.save_encoded(0, &encode(&leak)).expect("the tone file");
        timer_and_fd_round_trip(&path, Some(ToneBinding { store, key: ToneKey::Slot { slot: 0, identity }, imported: None }));
        let _ = std::fs::remove_dir_all(&tones);
    }

    /// The test plugin's timer and fd counts rise on its owner loop; the unload ends them, and a
    /// callback after it would abort the process.
    fn timer_and_fd_round_trip(path: &str, tone: Option<ToneBinding>) {
        let mut rig = Rig::new(RATE);
        let handle = rig.load(PluginFormat::Clap, path, "dev.bleeploop.test-gain", tone);
        let counts = || {
            let params = handle.list_params().expect("params");
            let count = |name: &str| params.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("a {name} parameter")).value;
            (count("Ticks"), count("Fd wakes"))
        };
        let (ticks, wakes) = counts();
        rig.run_for(Duration::from_millis(500));
        let (ticks_after, wakes_after) = counts();
        eprintln!("in 500 ms: {} timer ticks, {} fd wakes", ticks_after - ticks, wakes_after - wakes);
        // A 10 ms timer at the owner's 20 ms turns: about 25 in 500 ms.
        assert!(ticks_after - ticks >= 10.0, "the timer runs on the owner loop: {ticks} → {ticks_after}");
        assert!(wakes_after - wakes >= 5.0, "the fd wakes the owner loop: {wakes} → {wakes_after}");

        let unloading = std::thread::spawn(move || handle.unload());
        while !unloading.is_finished() {
            rig.render(1);
        }
        unloading.join().unwrap().expect("the unload");
        // A late callback would abort the process here.
        rig.run_for(Duration::from_millis(200));
    }

    /// The test's own X client on the private display the editor opens on: finds the editor window by
    /// its `WM_CLASS`, reads its pixels, clicks it through XTEST and closes it as a window manager would.
    struct Screen {
        conn: x11rb::rust_connection::RustConnection,
        root: u32,
    }

    impl Screen {
        fn connect() -> Screen {
            assert!(std::env::var_os("DISPLAY").is_some(), "DISPLAY names a private Xvfb (docs/VERIFY.md § Linux plugin hosting)");
            use x11rb::connection::Connection as _;
            use x11rb::protocol::xproto::ConnectionExt as _;
            let (conn, screen) = x11rb::connect(None).expect("the X display");
            let root = conn.setup().roots[screen].root;
            // A window manager's stand-in: a desktop's window manager has interned these long before a
            // plugin loads, and a bare Xvfb has none. JUCE looks them up only if they exist, when its
            // editor is created (before the host window), and later sets `WM_PROTOCOLS` on its window
            // with the atom it got: 0 here, a `BadAtom` that Xlib's default handler ends the process on.
            for name in [
                "WM_PROTOCOLS", "WM_DELETE_WINDOW", "WM_TAKE_FOCUS", "WM_STATE", "WM_CHANGE_STATE",
                "_NET_WM_PING", "_NET_WM_STATE", "_NET_WM_STATE_HIDDEN", "_NET_WM_WINDOW_TYPE",
                "_NET_WM_WINDOW_TYPE_NORMAL", "_NET_WORKAREA", "_MOTIF_WM_HINTS",
            ] {
                conn.intern_atom(false, name.as_bytes()).unwrap().reply().expect("an atom");
            }
            Screen { conn, root }
        }

        /// The editor window this host made (its `WM_CLASS`), if one exists.
        fn editor_window(&self) -> Option<u32> {
            use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
            let tree = self.conn.query_tree(self.root).ok()?.reply().ok()?;
            tree.children.into_iter().find(|&w| {
                self.conn
                    .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|p| p.value.starts_with(b"bleeploop-plugin\0"))
            })
        }

        /// The window's origin on the screen and its size.
        fn geometry(&self, window: u32) -> (i16, i16, u16, u16) {
            use x11rb::protocol::xproto::ConnectionExt as _;
            let g = self.conn.get_geometry(window).unwrap().reply().unwrap();
            let at = self.conn.translate_coordinates(window, self.root, 0, 0).unwrap().reply().unwrap();
            (at.dst_x, at.dst_y, g.width, g.height)
        }

        /// The pixels of `area` (x, y, width, height in window coordinates).
        fn area_pixels(&self, window: u32, (x, y, w, h): (i16, i16, u16, u16)) -> Vec<u8> {
            use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
            self.conn.get_image(ImageFormat::Z_PIXMAP, window, x, y, w, h, !0).unwrap().reply().unwrap().data
        }

        /// How many distinct colours the window shows (its children's drawing included): a window the
        /// plugin never drew into is one flat colour.
        fn colours(&self, window: u32) -> usize {
            use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
            let (_, _, w, h) = self.geometry(window);
            let image = self.conn.get_image(ImageFormat::Z_PIXMAP, window, 0, 0, w, h, !0).unwrap().reply().unwrap();
            let mut seen: Vec<u32> = image.data.chunks_exact(4).step_by(7).map(|p| u32::from_le_bytes([p[0], p[1], p[2], 0])).collect();
            seen.sort_unstable();
            seen.dedup();
            seen.len()
        }

        /// A left-button drag from `from` to `to` (window coordinates), through XTEST.
        fn drag(&self, window: u32, from: (i16, i16), to: (i16, i16)) {
            use x11rb::connection::Connection as _;
            use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
            use x11rb::protocol::xtest::ConnectionExt as _;
            let (x0, y0, _, _) = self.geometry(window);
            let at = |(x, y): (i16, i16)| (x0 + x, y0 + y);
            let motion = |(x, y): (i16, i16)| {
                self.conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, self.root, x, y, 0).unwrap();
                self.conn.flush().unwrap();
                std::thread::sleep(Duration::from_millis(15));
            };
            motion(at(from));
            self.conn.xtest_fake_input(BUTTON_PRESS_EVENT, 1, 0, self.root, 0, 0, 0).unwrap();
            for step in 1..=10 {
                let x = from.0 + (to.0 - from.0) * step / 10;
                let y = from.1 + (to.1 - from.1) * step / 10;
                motion(at((x, y)));
            }
            self.conn.xtest_fake_input(BUTTON_RELEASE_EVENT, 1, 0, self.root, 0, 0, 0).unwrap();
            self.conn.flush().unwrap();
        }

        /// `WM_DELETE_WINDOW`, sent to the window the way a window manager's close button sends it: only
        /// after the window advertised it in its `WM_PROTOCOLS`, as a window manager checks.
        fn wm_close(&self, window: u32) {
            use x11rb::connection::Connection as _;
            use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask};
            let atom = |name: &[u8]| self.conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
            let protocols = self.conn.get_property(false, window, atom(b"WM_PROTOCOLS"), AtomEnum::ATOM, 0, 32).unwrap().reply().unwrap();
            let advertised: Vec<u32> = protocols.value32().map(Iterator::collect).unwrap_or_default();
            assert!(advertised.contains(&atom(b"WM_DELETE_WINDOW")), "the window advertises WM_DELETE_WINDOW: {advertised:?}");
            let event = ClientMessageEvent::new(32, window, atom(b"WM_PROTOCOLS"), [atom(b"WM_DELETE_WINDOW"), 0, 0, 0, 0]);
            self.conn.send_event(false, window, EventMask::NO_EVENT, event).unwrap();
            self.conn.flush().unwrap();
        }

        /// A screenshot of the whole display into `LF_TEST_SHOTS` (when set), as `<name>.png`.
        fn shot(&self, name: &str) {
            if let Some(dir) = std::env::var_os("LF_TEST_SHOTS") {
                let file = std::path::Path::new(&dir).join(format!("{name}.png"));
                let ok = std::process::Command::new("import").args(["-window", "root"]).arg(&file).status().is_ok_and(|s| s.success());
                eprintln!("screenshot {}: {}", file.display(), if ok { "saved" } else { "FAILED" });
            }
        }
    }

    type Events = Arc<Mutex<Vec<EngineSlotEvent>>>;

    /// One drag from `from` to `to`: the parameter it moved, when it moved one. It must move exactly
    /// one (the control under it), reach the host as that parameter's `performEdit` (VST3), and the
    /// plugin must redraw the drag's own area of the window.
    fn drag_one(rig: &mut Rig, screen: &Screen, window: u32, handle: &EngineSlotHandle, events: &Events, format: PluginFormat, from: (i16, i16), to: (i16, i16)) -> Option<u32> {
        let (_, _, w, h) = screen.geometry(window);
        // The drag's column, 24 px either side, from above its top to below its bottom.
        let (x0, y0) = ((from.0 - 24).max(0), (from.1.min(to.1) - 24).max(0));
        let area = (x0, y0, 48.min(w - x0 as u16), ((from.1 - to.1).unsigned_abs() + 48).min(h - y0 as u16));
        let before = handle.list_params().expect("params");
        let drawn = screen.area_pixels(window, area);
        events.lock().unwrap().clear();
        screen.drag(window, from, to);
        rig.run_for(Duration::from_millis(150));
        let after = handle.list_params().expect("params");
        let moved: Vec<_> = before.iter().zip(&after).filter(|(b, a)| b.id == a.id && (b.value - a.value).abs() > 1e-6).collect();
        let [(b, a)] = moved.as_slice() else {
            let names: Vec<_> = moved.iter().map(|(b, _)| b.name.as_str()).collect();
            assert!(names.is_empty(), "one drag moved {} parameters: {names:?}", names.len());
            return None;
        };
        eprintln!("drag {from:?} → {to:?} moved {:?} (id {}): {} → {}", b.name, b.id, b.value, a.value);
        if format == PluginFormat::Vst3 {
            let edited = events.lock().unwrap().iter().any(|e| matches!(e, EngineSlotEvent::ParamChanged { id, .. } if *id == b.id));
            assert!(edited, "performEdit of the dragged parameter {:?} reached the host", b.name);
        }
        assert!(rig.run_until(Duration::from_secs(2), |_| screen.area_pixels(window, area) != drawn), "the plugin redrew the dragged control {:?}", b.name);
        Some(b.id)
    }

    /// Drag upward across the editor's lower right (Surge's envelope and LFO sliders) until a drag
    /// moves a parameter: its start point and the parameter's id and name.
    fn find_a_control(rig: &mut Rig, screen: &Screen, window: u32, handle: &EngineSlotHandle, events: &Events, format: PluginFormat) -> Option<((i16, i16), u32, String)> {
        let (_, _, w, h) = screen.geometry(window);
        let (w, h) = (i32::from(w), i32::from(h));
        // In i32: a 900 px editor times a percentage overflows i16.
        let point = |x: i32, y: i32| (i16::try_from(x).expect("on the screen"), i16::try_from(y).expect("on the screen"));
        for row in [80, 88, 72] {
            for col in (52..=96).step_by(4) {
                let (x, y) = (w * col / 100, h * row / 100);
                if let Some(id) = drag_one(rig, screen, window, handle, events, format, point(x, y), point(x, y - h / 12)) {
                    let name = handle.list_params().expect("params").into_iter().find(|p| p.id == id).map(|p| p.name).unwrap_or_default();
                    return Some((point(x, y), id, name));
                }
            }
        }
        None
    }

    /// Surge XT's editor from slot 0 on a private X display: it opens, draws and takes input (an XTEST
    /// drag moves a parameter the host reads back); stays open, drawing and playing across an engine
    /// rebuild that re-activates the unit; closes through the app and through `WM_DELETE_WINDOW` (the
    /// latter as the app's `EditorClosed`), leaving no window behind.
    fn surge_editor_round_trip(format: PluginFormat, env: &str, id: &str) {
        // Skipped, not failed, without its display or plugin: `--ignored` on this module also runs the
        // offline gain tests, which need neither.
        let (Some(path), Some(_)) = (std::env::var_os(env), std::env::var_os("DISPLAY")) else {
            eprintln!("SKIPPED: the Surge editor test needs {env} and DISPLAY (a private Xvfb): docs/VERIFY.md § Linux plugin hosting");
            return;
        };
        let path = path.into_string().expect("a UTF-8 plugin path");
        let screen = Screen::connect();
        assert_eq!(screen.editor_window(), None, "no editor window before the test");
        let tag = format.as_str();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink: EventSink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let mut rig = Rig::new(RATE);
        rig.send(Command::SelectInstrument(lf_engine::NoteTarget::Slot(0)));
        let handle = rig.load_with(format, &path, id, None, sink);
        rig.send(Command::NoteOn(60, 0.8));
        assert!(rig.run_until(Duration::from_secs(3), |rig| rig.peak(8) > 1e-3), "Surge plays a note");
        rig.send(Command::NoteOff(60));

        handle.open_editor().expect("the editor opens");
        let mut window = None;
        assert!(rig.run_until(Duration::from_secs(5), |_| { window = screen.editor_window(); window.is_some() }), "an editor window appears");
        let window = window.unwrap();
        assert!(rig.run_until(Duration::from_secs(10), |_| screen.colours(window) > 50), "the editor draws into it");
        screen.shot(&format!("{tag}-1-open"));
        let (at, dragged, name) = find_a_control(&mut rig, &screen, window, &handle, &events, format).expect("an XTEST drag moves a parameter");
        screen.shot(&format!("{tag}-2-dragged-{}", name.replace(|c: char| !c.is_ascii_alphanumeric(), "_")));

        // A new engine at another rate: the owner re-activates the unit and installs it there, the
        // editor still open.
        rig.engine_at(44_100);
        rig.send(Command::SelectInstrument(lf_engine::NoteTarget::Slot(0)));
        let replays = rig.run_until(Duration::from_secs(5), |rig| {
            rig.send(Command::NoteOn(64, 0.8));
            rig.peak(16) > 1e-3
        });
        assert!(replays, "the re-activated unit plays in the new engine");
        rig.send(Command::NoteOff(64));
        assert_eq!(screen.editor_window(), Some(window), "the editor stayed open across the rebuild");
        // Shown by a new change, not by pixels from before the rebuild: the same control dragged back
        // down must move the same parameter (VST3: through its performEdit), and the plugin must
        // redraw it.
        let back = (at.0, at.1 + i16::try_from(screen.geometry(window).3 / 12).unwrap());
        let moved = drag_one(&mut rig, &screen, window, &handle, &events, format, at, back);
        assert_eq!(moved, Some(dragged), "after the rebuild the drag moves {name:?} again");
        screen.shot(&format!("{tag}-3-after-rebuild"));

        // Closed by the app (CloseEditor).
        handle.close_editor().expect("the editor closes");
        assert!(rig.run_until(Duration::from_secs(2), |_| screen.editor_window().is_none()), "no window left after CloseEditor");
        screen.shot(&format!("{tag}-4-closed-by-app"));

        // Closed by the window manager's close button.
        handle.open_editor().expect("the editor opens again");
        let mut window = None;
        assert!(rig.run_until(Duration::from_secs(5), |_| { window = screen.editor_window(); window.is_some() }), "it reopens");
        let window = window.unwrap();
        assert!(rig.run_until(Duration::from_secs(10), |_| screen.colours(window) > 50), "and draws");
        screen.shot(&format!("{tag}-5-reopened"));
        events.lock().unwrap().clear();
        screen.wm_close(window);
        let closed = rig.run_until(Duration::from_secs(2), |_| events.lock().unwrap().contains(&EngineSlotEvent::EditorClosed));
        assert!(closed, "WM_DELETE_WINDOW arrives as the app's EditorClosed");
        assert!(rig.run_until(Duration::from_secs(2), |_| screen.editor_window().is_none()), "no window left after the WM close");
        screen.shot(&format!("{tag}-6-closed-by-wm"));

        // Open again and unload with it open: the unload closes it first.
        handle.open_editor().expect("the editor opens a third time");
        assert!(rig.run_until(Duration::from_secs(5), |_| screen.editor_window().is_some()), "it reopens");
        let unloading = std::thread::spawn(move || handle.unload());
        while !unloading.is_finished() {
            rig.render(1);
        }
        unloading.join().unwrap().expect("the unload");
        assert_eq!(screen.editor_window(), None, "the unload leaves no window");
    }

    #[test]
    #[ignore = "needs DISPLAY (a private Xvfb) and LF_TEST_SURGE_CLAP: docs/VERIFY.md § Linux plugin hosting"]
    fn surge_clap_editor_opens_draws_takes_input_survives_a_rebuild_and_closes() {
        surge_editor_round_trip(PluginFormat::Clap, "LF_TEST_SURGE_CLAP", "org.surge-synth-team.surge-xt");
    }

    #[test]
    #[ignore = "needs DISPLAY (a private Xvfb) and LF_TEST_SURGE_VST3: docs/VERIFY.md § Linux plugin hosting"]
    fn surge_vst3_editor_opens_draws_takes_input_survives_a_rebuild_and_closes() {
        surge_editor_round_trip(PluginFormat::Vst3, "LF_TEST_SURGE_VST3", "abcdef019182faeb566d624153675854");
    }

    /// The VST3 twin: a separated component and controller in a `.vst3` bundle (`ModuleEntry`,
    /// `Contents/x86_64-linux/`), its class id the descriptor id the scan gives it.
    #[test]
    #[ignore = "needs LF_TEST_VST3: docs/VERIFY.md § Linux plugin hosting"]
    fn a_vst3_plugin_plays_takes_its_param_and_restores_its_tone() {
        gain_plugin_round_trip(PluginFormat::Vst3, "LF_TEST_VST3", "4c46546573744761696e50726f633031", 0.75);
    }
}
