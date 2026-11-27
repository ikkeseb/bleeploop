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

    struct Rig {
        host: EngineHost,
        frame: i64,
    }

    impl Rig {
        fn new(rate: u32) -> Rig {
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
            let slot = SlotHost::new(self.host.core.clone(), 0);
            engine_slot::load(format, path.to_string(), id.to_string(), slot, 0, Arc::new(|_| {}), tone).expect("the plugin loads")
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

    /// The VST3 twin: a separated component and controller in a `.vst3` bundle (`ModuleEntry`,
    /// `Contents/x86_64-linux/`), its class id the descriptor id the scan gives it.
    #[test]
    #[ignore = "needs LF_TEST_VST3: docs/VERIFY.md § Linux plugin hosting"]
    fn a_vst3_plugin_plays_takes_its_param_and_restores_its_tone() {
        gain_plugin_round_trip(PluginFormat::Vst3, "LF_TEST_VST3", "4c46546573744761696e50726f633031", 0.75);
    }
}
