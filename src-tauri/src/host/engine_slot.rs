//! OWNS: the engine-mode plugin slot (threads and gotchas: `src-tauri/AGENTS.md`): the load API that
//! spawns one owner thread per slot, and the handle a caller drives it through. The owner loads a
//! CLAP, VST3 or VST2 plugin, activates it at the engine's rate and installs its processor into the
//! engine as an `lf_engine::SlotProcessor` unit (`clap_engine`, `vst3_engine`, `vst2_engine`), so the
//! plugin renders inside the device callback. The `plugin_*` commands drive it (`engine_io/plugins.rs`).
//!
//! One owner loop serves every format (`serve`; what differs is a format's `OwnedPlugin`). It services,
//! less the device (the engine owns it): params (the
//! unit drains the slot's event ring; a VST3 set reaches the edit controller too), editors, plugin
//! callbacks and params rescans, the thread's Win32 messages on every turn, a bounded batch each (a
//! JUCE plugin's message thread is this one), a
//! plugin-requested restart and an eviction (the
//! unit comes back → deactivate → activate at the engine's current rate and block → reinstall; the
//! slot is bypassed meanwhile and the engine never waits), and the ordered teardown. It also keeps the
//! plugin's TONE (`host/tone.rs`): the stored one is restored inside the load, before the plugin
//! activates (`restore_tone`; a plugin that refuses it is discarded and created again, so it runs at its
//! true defaults), and the owner saves it on its own thread (`keep_tone`) once a change has gone quiet,
//! when the editor closes, before the teardown, and when asked (a session export, the app's exit).
//! Events go to a sink closure instead of a window, so an owner runs headless. Notes never
//! pass through here: the engine routes them (`Command::SelectInstrument(NoteTarget::Slot(i))`, then
//! `Command::NoteOn`).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{
    AtomicBool, AtomicU32,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clack_host::entry::PluginEntry;
use lf_engine::{SlotKind, SlotProcessor};
use rtrb::{Consumer, Producer, RingBuffer};

use super::super::editor_window::{pump_thread_messages, wait_for_input};
use super::super::state::{ParamDesc, ToneRestore};
use super::super::tone::{Saved, ToneBinding, ToneKeeper};
use super::{
    load_ready_channel, owner_request_5s, publish_param_ids, take_uncancelled, OwnerRequest, ParamEvent, ParamIds,
    EVENT_RING_CAP,
};
use crate::engine_io::SlotHost;

/// The plugin formats this host knows, and the ONE place a format is told from a path or a stored
/// name: the scan, the load, the tone store and the probes all ask here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PluginFormat {
    Clap,
    Vst3,
    Vst2,
}

impl PluginFormat {
    /// In scan order: the picker lists CLAP, then VST3, then VST2.
    pub(crate) const ALL: [PluginFormat; 3] = [PluginFormat::Clap, PluginFormat::Vst3, PluginFormat::Vst2];

    /// The extension a plugin of this format has on Windows. A VST2 plugin has none of its own:
    /// it is a `.dll` among other DLLs (`host/pe.rs` tells them apart).
    pub(crate) fn extension(self) -> &'static str {
        match self {
            PluginFormat::Clap => "clap",
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Vst2 => "dll",
        }
    }

    /// The format a path's extension names, whatever its case; `None` for any other extension.
    pub(crate) fn of_path(path: &std::path::Path) -> Option<Self> {
        let extension = path.extension()?;
        Self::ALL.into_iter().find(|f| extension.eq_ignore_ascii_case(f.extension()))
    }

    /// The format a stored name is (`as_str`): a descriptor's `format`, a tone's.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == name)
    }

    /// The format's name in descriptors, tones and thread names.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PluginFormat::Clap => "clap",
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Vst2 => "vst2",
        }
    }
}

/// What an owner tells its caller (the window at Stage 5; a probe or a test before that).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EngineSlotEvent {
    /// The plugin's own editor moved a parameter (VST3 `performEdit`, VST2 `audioMasterAutomate`);
    /// the processor has it too.
    ParamChanged { id: u32, value: f64 },
    /// The plugin changed its parameters behind the host's back (CLAP `params.rescan`, VST3
    /// `restartComponent` with a param flag, VST2 `audioMasterUpdateDisplay`): list them again.
    ParamsChanged,
    /// The user closed the plugin's editor.
    EditorClosed,
}

/// Where an owner's events go; called on the owner thread.
pub(crate) type EventSink = Arc<dyn Fn(EngineSlotEvent) + Send + Sync>;

/// How long a teardown, a restart or an undo waits for the engine to hand the unit back: a
/// crossfade and a block, with room for a slow device.
pub(super) const REMOVE_TIMEOUT: Duration = Duration::from_secs(2);

/// The owner's poll while no hosted editor is open: plugin callbacks, restarts and evictions are
/// serviced within this.
pub(super) const OWNER_POLL: Duration = Duration::from_millis(20);

/// How long `load` waits for the owner's report.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// The fault bits a unit latches on the audio thread; the owner reports each once per load.
pub(super) const FAULT_START: u32 = 1 << 0;
pub(super) const FAULT_PROCESS: u32 = 1 << 1;
pub(super) const FAULT_PARAM: u32 = 1 << 2;
pub(super) const FAULT_EVENTS: u32 = 1 << 3;

const FAULT_MESSAGES: [(u32, &str); 4] = [
    (FAULT_START, "the plugin refused to start processing; the slot is silent until it is reinstalled"),
    (FAULT_PROCESS, "a process call failed; that slice's output was discarded"),
    (FAULT_PARAM, "a parameter id the plugin cannot take was dropped"),
    (FAULT_EVENTS, "more notes than one block's event list holds; the rest were dropped"),
];

/// Owner-thread drain for a unit's latched faults: each category reaches the log once per load.
pub(super) fn report_faults(faults: &AtomicU32, slot: usize, reported: &mut u32) {
    let new = faults.load(Relaxed) & !*reported;
    if new == 0 {
        return;
    }
    *reported |= new;
    for (bit, message) in FAULT_MESSAGES {
        if new & bit != 0 {
            log::error!("[plugin_host] engine slot {slot} fault: {message}");
        }
    }
}

/// What an engine-mode owner thread starts with.
pub(super) struct OwnerCtx {
    pub(super) slot: SlotHost,
    pub(super) running: Arc<AtomicBool>,
    pub(super) requests: Receiver<OwnerRequest>,
    /// The unit's end of the slot's event ring (`EngineSlotHandle::set_param` pushes params).
    pub(super) params: Consumer<ParamEvent>,
    /// The ring's producer, for a VST3 editor's `performEdit` (the handle holds the same one).
    pub(super) params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    pub(super) param_ids: ParamIds,
    pub(super) sink: EventSink,
    /// The main window's HWND (0 = none): a floating CLAP editor's transient parent.
    pub(super) editor_parent: usize,
    /// Rendezvous (`load_ready_channel`): a send that fails means the caller gave up, and the owner
    /// undoes the load itself.
    pub(super) ready: SyncSender<Result<Ready, String>>,
    /// The plugin's tone: what the load restores and where the owner saves it.
    pub(super) tone: ToneKeeper,
}

/// An installed load, as the owner reports it.
pub(super) struct Ready {
    pub(super) name: String,
    pub(super) kind: SlotKind,
    /// What the load did with the stored tone (`None`: nothing was stored).
    pub(super) tone: Option<ToneRestore>,
}

/// What a load did with the plugin's stored tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Restore {
    /// Nothing was stored.
    Nothing,
    Restored,
    /// The tone could not be read: the plugin never saw it.
    Unreadable,
    /// The plugin's state setter refused it, maybe after applying part of it: this instance is not at
    /// its defaults, so the owner discards it and creates the plugin again, without a tone.
    Refused,
}

impl Restore {
    /// What the load answers (`PluginInfo.tone`).
    pub(super) fn report(self) -> Option<ToneRestore> {
        match self {
            Restore::Nothing => None,
            Restore::Restored => Some(ToneRestore::Restored),
            Restore::Unreadable | Restore::Refused => Some(ToneRestore::Failed),
        }
    }
}

/// Owner thread, inside the load, before the plugin activates: hand the stored tone to `apply` (the
/// format's state setter). A tone that cannot be read, or that the plugin refuses, is logged, the store
/// keeps it until the player changes something (`ToneKeeper::keep_stored`), and the load goes on at the
/// plugin's defaults: a load never fails because of its tone.
pub(super) fn restore_tone(
    tone: &mut ToneKeeper,
    slot: usize,
    name: &str,
    apply: impl FnOnce(&[u8]) -> Result<(), String>,
) -> Restore {
    let restore = match tone.stored() {
        Ok(None) => Restore::Nothing,
        Ok(Some(state)) => match apply(&state) {
            Ok(()) => {
                log::info!("[plugin_host] engine slot {slot}: {name} restored its tone ({} bytes)", state.len());
                Restore::Restored
            }
            Err(e) => {
                log::warn!("[plugin_host] engine slot {slot}: {name} refused its saved tone ({e}); it loads afresh, at its defaults");
                Restore::Refused
            }
        },
        Err(e) => {
            log::warn!("[plugin_host] engine slot {slot}: {name}'s saved tone could not be read ({e}); it loads with its defaults");
            Restore::Unreadable
        }
    };
    tone.settle();
    if matches!(restore, Restore::Unreadable | Restore::Refused) {
        tone.keep_stored();
    }
    restore
}

/// Owner thread: save the tone now (`ToneKeeper::save`, `take` being the format's state getter) for
/// `why`, and log the outcome. Returns the tone file's bytes for a caller that asked for them.
pub(super) fn keep_tone(
    tone: &mut ToneKeeper,
    slot: usize,
    why: &str,
    name: &str,
    take: impl FnOnce() -> Result<Option<Vec<u8>>, String>,
) -> Result<Vec<u8>, String> {
    let t = Instant::now();
    match tone.save(name, take) {
        Ok(Saved { bytes, not_stored: None }) => {
            log::info!("[plugin_host] engine slot {slot}: tone saved ({why}): {} bytes in {} ms", bytes.len(), t.elapsed().as_millis());
            Ok(bytes)
        }
        Ok(Saved { bytes, not_stored: Some(reason) }) => {
            log::info!("[plugin_host] engine slot {slot}: tone taken ({why}), not stored: {reason}");
            Ok(bytes)
        }
        Err(e) => {
            log::error!("[plugin_host] engine slot {slot}: saving the tone ({why}) failed: {e}");
            Err(e)
        }
    }
}

/// What a format's owner loaded, for the shared owner (`serve`) to install and report.
pub(super) struct Loaded<P: OwnedPlugin> {
    pub(super) plugin: P,
    pub(super) unit: Box<P::Unit>,
    pub(super) name: String,
    /// The engine rate it activated at (`SlotHost::install` checks it).
    pub(super) rate: u32,
    /// What the load did with the stored tone.
    pub(super) tone: Option<ToneRestore>,
}

/// What a format's load may use: the owner's slot, the unit's ends and the tone it restores.
pub(super) struct Setup<'a> {
    pub(super) slot: &'a SlotHost,
    pub(super) params: Consumer<ParamEvent>,
    pub(super) params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    pub(super) sink: &'a EventSink,
    pub(super) editor_parent: usize,
    pub(super) faults: Arc<AtomicU32>,
    pub(super) tone: &'a mut ToneKeeper,
}

/// Whether a plugin's editor is open, and whether it is a host window this thread pumps.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Editor {
    Closed,
    /// A CLAP plugin's own floating window, pumped by the plugin.
    Floating,
    Hosted,
}

/// What one turn found the plugin reported (`OwnedPlugin::turn`).
#[derive(Default)]
pub(super) struct Reported {
    /// Its parameters changed behind the host's back: list them again.
    pub(super) relist: bool,
    /// Its state changed: a tone to save.
    pub(super) changed: bool,
}

/// A loaded plugin as the shared owner (`serve`) drives it, every call on its owner thread: what
/// differs between CLAP, VST3 and VST2. Everything else (the install and the report, the loop, the
/// requests, the tone, the ordered teardown) is `serve`'s, once.
pub(super) trait OwnedPlugin {
    /// The unit it installs into the engine.
    type Unit: SlotProcessor + 'static;
    /// The format's name in the log.
    const FORMAT: &'static str;
    /// A restart the plugin asked for since the last call, as the log names it.
    fn take_restart(&mut self) -> Option<String>;
    /// Deactivate what the unit holds, activate again at the engine's current terms and install it.
    /// On failure the unit comes back to the caller, and the plugin is left inactive.
    fn reinstall(&mut self, unit: Box<Self::Unit>, slot: &SlotHost) -> Result<(), (Box<Self::Unit>, String)>;
    /// The turn's plugin side, before the requests: deliver what the plugin asked for, and tell
    /// `sink` the parameters its own editor moved.
    fn turn(&mut self, sink: &EventSink) -> Reported;
    /// After the turn's message pump: serve an open editor. True when the player closed it (it is
    /// closed by then).
    fn idle(&mut self) -> bool;
    fn params(&mut self) -> Result<Vec<ParamDesc>, String>;
    /// Its state, for its tone: `Ok(None)` when it keeps none.
    fn state(&mut self) -> Result<Option<Vec<u8>>, String>;
    fn editor(&self) -> Editor;
    fn editor_open(&mut self) -> Result<(), String>;
    fn editor_close(&mut self);
    /// A host-set parameter the unit got through the ring, for a VST3 edit controller.
    fn set_param_normalized(&mut self, _id: u32, _value: f64) {}
    /// The unit is out of the engine: what the plugin does before the unload's tone save (VST2 stops
    /// and suspends, so its state is read from a plugin that no longer runs).
    fn quiesce(&mut self) {}
    /// The unit is out of the engine, or never was in it: tear down in the format's order.
    fn release(self, unit: Option<Box<Self::Unit>>);
}

/// A unit the engine handed back: a `SlotHost` hands back only what its own handle installed. The
/// engine stops a unit before it leaves; stopping again is a no-op, and covers one that did not.
pub(super) fn own<U: SlotProcessor + 'static>(unit: Box<dyn SlotProcessor>) -> Box<U> {
    let mut unit = unit
        .into_any()
        .downcast::<U>()
        .unwrap_or_else(|_| panic!("an engine slot hands back the unit its owner installed"));
    unit.stop();
    unit
}

/// The engine-mode owner every format shares: `load` builds the plugin and its unit (on this
/// thread, from `Setup`); this installs the unit, reports the load, serves the plugin until the
/// unload and tears it down. A load its caller gave up on is undone here. Returns the teardown's
/// result.
pub(super) fn serve<P: OwnedPlugin>(
    ctx: OwnerCtx,
    load: impl FnOnce(Setup<'_>) -> Result<Loaded<P>, String>,
) -> Result<(), String> {
    let OwnerCtx { slot, running, requests, params, params_tx, param_ids, sink, editor_parent, ready, mut tone } = ctx;
    let index = slot.slot();
    let faults = Arc::new(AtomicU32::new(0));
    let setup = Setup { slot: &slot, params, params_tx, sink: &sink, editor_parent, faults: faults.clone(), tone: &mut tone };
    let Loaded { mut plugin, unit, name, rate, tone: restored } = match load(setup) {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    // Before the load is reported, so the caller's first `set_param` finds its ids.
    publish_param_ids(&param_ids, &plugin.params().unwrap_or_default());
    let kind = unit.kind();
    let installed = match running.load(Acquire) {
        true => slot.install(unit, rate).map_err(|(unit, e)| (own(unit), e)),
        false => Err((unit, "plugin load cancelled".to_string())),
    };
    if let Err((unit, e)) = installed {
        plugin.release(Some(unit));
        let _ = ready.send(Err(e));
        return Ok(());
    }
    if ready.send(Ok(Ready { name: name.clone(), kind, tone: restored })).is_err() {
        log::warn!("[plugin_host] engine slot {index}: the load finished after its caller gave up; undoing it");
        return teardown(&slot, plugin, None, None);
    }

    // The unit while it is out of the engine (a failed restart); `None` while the engine holds it.
    let mut parked: Option<Box<P::Unit>> = None;
    let mut reported = 0u32;
    let save = |tone: &mut ToneKeeper, plugin: &mut P, why: &str| keep_tone(tone, index, why, &name, || plugin.state());
    // A panic in the loop must not unwind past a unit the engine still runs (dropping the plugin would
    // deactivate and unload code the audio thread is inside): it is caught here, and the ordered
    // teardown below takes the unit out of the engine first.
    let served = catch_unwind(AssertUnwindSafe(|| {
        while running.load(Acquire) {
            // A restart the plugin asked for only raised a flag (maybe on a foreign thread); the cycle
            // runs here, before anything reads the plugin, so a re-list it raised lands this turn.
            settle(&mut plugin, &slot, &mut parked);
            let Reported { relist, changed } = plugin.turn(&sink);
            if relist {
                publish_param_ids(&param_ids, &plugin.params().unwrap_or_default());
                sink(EngineSlotEvent::ParamsChanged);
            }
            if relist || changed {
                tone.note_change(Instant::now());
            }
            // A new engine at another rate evicted the unit; it comes back stopped.
            if let Some(unit) = slot.take_evicted() {
                cycle(&mut plugin, &slot, &mut parked, Some(own(unit)), "re-activation after a device change");
            }
            // Every turn, editor or not: a JUCE plugin (Neural DSP) runs its message thread here, and its
            // timers and async updates must not wait for the next editor, or its state lags what it plays
            // and a tone saved meanwhile is stale. A hosted editor's window reports its close box here.
            pump_thread_messages();
            let closed = plugin.idle();
            // The pump ran the plugin's own code.
            settle(&mut plugin, &slot, &mut parked);
            if closed {
                sink(EngineSlotEvent::EditorClosed);
                let _ = save(&mut tone, &mut plugin, "editor closed");
            }
            let request = if plugin.editor() == Editor::Hosted {
                wait_for_input(20);
                requests.try_recv().ok()
            } else {
                match requests.recv_timeout(OWNER_POLL) {
                    Ok(request) => Some(request),
                    Err(RecvTimeoutError::Timeout) => None,
                    // The handle is gone without an unload; `running` is false by then.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            // A request may have waited through a restart report: the cycle runs first, and only then
            // is the request's caller asked whether it still waits (a slow cycle can outlast its 5 s).
            if request.is_some() {
                settle(&mut plugin, &slot, &mut parked);
            }
            if let Some(request) = request.and_then(|r| take_uncancelled(r, index as u8)) {
                match request {
                    OwnerRequest::OpenEditor(cancelled, reply) => {
                        let was_closed = plugin.editor() == Editor::Closed;
                        let res = if was_closed { plugin.editor_open() } else { Ok(()) };
                        // Opened after the caller's 5 s: it reported failure, so close what this opened.
                        if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                            plugin.editor_close();
                        }
                        let _ = reply.send(res);
                    }
                    OwnerRequest::CloseEditor(_, reply) => {
                        if plugin.editor() != Editor::Closed {
                            plugin.editor_close();
                            let _ = save(&mut tone, &mut plugin, "editor closed");
                        }
                        let _ = reply.send(Ok(()));
                    }
                    OwnerRequest::SaveTone(reply) => {
                        let _ = reply.send(save(&mut tone, &mut plugin, "asked"));
                    }
                    OwnerRequest::ListParams(reply) => {
                        let res = plugin.params();
                        if let Ok(listed) = &res {
                            publish_param_ids(&param_ids, listed);
                        }
                        let _ = reply.send(res);
                    }
                    // The processor got the value through the ring; a VST3 controller is told here.
                    OwnerRequest::SetParamNormalized(id, value) => plugin.set_param_normalized(id, value),
                    OwnerRequest::Wake => {}
                }
            }
            if tone.poll(Instant::now()) {
                let _ = save(&mut tone, &mut plugin, "changes went quiet");
            }
            report_faults(&faults, index, &mut reported);
        }
    }));
    if served.is_err() {
        log::error!("[plugin_host] engine slot {index}: the {} owner panicked; tearing the plugin down", P::FORMAT);
    }
    let editor_open = plugin.editor() != Editor::Closed;
    if editor_open {
        plugin.editor_close();
    }
    // The teardown saves a change not saved yet, what the plugin reported since the loop's last turn
    // (taken for the tone alone: no caller is told any more), and what an open editor may have changed
    // unseen. Not after a panic, which may have left the plugin half way through something.
    if served.is_ok() {
        let Reported { relist, changed } = plugin.turn(&(Arc::new(|_| {}) as EventSink));
        if relist || changed {
            tone.note_change(Instant::now());
        }
    }
    let keep = served.is_ok() && (editor_open || tone.dirty());
    let result = teardown(&slot, plugin, parked, keep.then_some((&mut tone, name.as_str())));
    report_faults(&faults, index, &mut reported);
    result
}

/// Owner turn: cycle the plugin now if it asked for a restart, so what comes next reads it as it is.
fn settle<P: OwnedPlugin>(plugin: &mut P, slot: &SlotHost, parked: &mut Option<Box<P::Unit>>) {
    if let Some(why) = plugin.take_restart() {
        cycle(plugin, slot, parked, None, &why);
    }
}

/// A restart or an eviction: get the unit back (it came from the engine, the owner still holds it
/// from a failed cycle, or the engine hands it back now) and reinstall it. A failed cycle parks the
/// unit, the slot bypassed, until the next one.
fn cycle<P: OwnedPlugin>(
    plugin: &mut P,
    slot: &SlotHost,
    parked: &mut Option<Box<P::Unit>>,
    back: Option<Box<P::Unit>>,
    why: &str,
) {
    let index = slot.slot();
    let unit = match back.or_else(|| parked.take()) {
        Some(unit) => unit,
        None => match slot.remove(REMOVE_TIMEOUT) {
            Ok(Some(unit)) => own(unit),
            Ok(None) => {
                log::error!("[plugin_host] engine slot {index} {why}: no unit to cycle");
                return;
            }
            Err(e) => {
                log::error!("[plugin_host] engine slot {index} {why}: {e}; it stays as it is");
                return;
            }
        },
    };
    match plugin.reinstall(unit, slot) {
        Ok(()) => log::info!("[plugin_host] engine slot {index} {why}: deactivated → activated → reinstalled"),
        Err((unit, e)) => {
            log::error!("[plugin_host] engine slot {index} {why} failed, the slot stays bypassed: {e}");
            *parked = Some(unit);
        }
    }
}

/// The ordered teardown: the unit leaves the engine (stopped on the audio thread) → the plugin
/// quiesces (`OwnedPlugin::quiesce`) → the tone is saved (`save`: there is a change to keep) → the
/// plugin goes in its format's order (`OwnedPlugin::release`), its module last. A unit the engine does
/// not hand back keeps the plugin loaded (leaked) rather than unloading code a processor still runs;
/// its tone is saved as every other save is, with the unit still processing.
fn teardown<P: OwnedPlugin>(
    slot: &SlotHost,
    mut plugin: P,
    parked: Option<Box<P::Unit>>,
    save: Option<(&mut ToneKeeper, &str)>,
) -> Result<(), String> {
    let index = slot.slot();
    let t = Instant::now();
    let unit = match parked {
        Some(unit) => Some(unit),
        None => match slot.remove(REMOVE_TIMEOUT) {
            Ok(unit) => unit.map(own),
            Err(e) => {
                log::error!("[plugin_host] engine slot {index} {} teardown: {e}; leaving the plugin loaded", P::FORMAT);
                if let Some((tone, name)) = save {
                    let _ = keep_tone(tone, index, "unload", name, || plugin.state());
                }
                slot.abandon();
                std::mem::forget(plugin);
                return Err(e);
            }
        },
    };
    let remove_ms = t.elapsed().as_millis();
    let t = Instant::now();
    plugin.quiesce();
    if let Some((tone, name)) = save {
        let _ = keep_tone(tone, index, "unload", name, || plugin.state());
    }
    plugin.release(unit);
    log::info!(
        "[plugin_host] engine slot {index} {} teardown: remove={remove_ms} release={} ms",
        P::FORMAT,
        t.elapsed().as_millis()
    );
    Ok(())
}

/// Load plugin `id` from `path` into the engine slot `slot`: spawn its owner thread
/// (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}` / `lf-vst2-engine-{slot}`) and wait (≤ 15 s) until the unit is in the
/// engine, with the plugin's tone restored from `tone` (`None`: no store, nothing kept). Err when no
/// device has opened yet (`SlotHost::rate` is `None`), the slot already holds a unit, or the plugin
/// fails to load or activate; never because of its tone.
pub(crate) fn load(
    format: PluginFormat,
    path: String,
    id: String,
    slot: SlotHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
) -> Result<EngineSlotHandle, String> {
    match format {
        PluginFormat::Clap => spawn(format, slot, editor_parent, sink, tone, move |ctx| {
            super::clap_engine::run(ctx, &id, move || {
                // SAFETY: PluginEntry::load runs the bundle's foreign entry-init code; the
                // out-of-process scan vetted the bundle.
                unsafe { PluginEntry::load(&path) }.map_err(|e| format!("load failed: {e}"))
            })
        }),
        PluginFormat::Vst3 => spawn(format, slot, editor_parent, sink, tone, move |ctx| {
            super::vst3_host::engine::run(ctx, path, &id)
        }),
        PluginFormat::Vst2 => spawn(format, slot, editor_parent, sink, tone, move |ctx| {
            super::vst2_engine::run(ctx, path, &id)
        }),
    }
}

/// Spawn an owner running `run` and wait for its report. The fixtures come in here with an
/// in-process plugin instead of a path.
pub(super) fn spawn(
    format: PluginFormat,
    slot: SlotHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
    run: impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    let index = slot.slot();
    let running = Arc::new(AtomicBool::new(true));
    let edited = Arc::new(AtomicBool::new(false));
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let params_tx = Arc::new(Mutex::new(params_tx));
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = load_ready_channel();
    let param_ids = ParamIds::default();
    let ctx = OwnerCtx {
        slot,
        running: running.clone(),
        requests,
        params,
        params_tx: params_tx.clone(),
        param_ids: param_ids.clone(),
        sink,
        editor_parent,
        ready,
        tone: ToneKeeper::new(tone, edited.clone()),
    };
    let owner = std::thread::Builder::new()
        .name(format!("lf-{}-engine-{index}", format.as_str()))
        .spawn(move || run(ctx))
        .map_err(|e| format!("spawn owner thread: {e}"))?;
    match ready_rx.recv_timeout(LOAD_TIMEOUT) {
        Ok(Ok(Ready { name, kind, tone })) => Ok(EngineSlotHandle {
            slot: index,
            format,
            name,
            kind,
            tone,
            running,
            owner: Some(owner),
            requests: request_tx,
            params_tx,
            param_ids,
            edited,
        }),
        Ok(Err(e)) => {
            running.store(false, Release);
            let _ = owner.join();
            Err(e)
        }
        Err(e) => {
            // Setup hung: the owner is left to finish and undo its own late result (its ready
            // send fails once `ready_rx` drops here).
            running.store(false, Release);
            Err(format!("plugin setup timed out: {e}"))
        }
    }
}

/// A loaded engine slot. Every method is callable from any non-RT thread; dropping the handle
/// unloads like `unload`.
pub(crate) struct EngineSlotHandle {
    slot: usize,
    format: PluginFormat,
    name: String,
    kind: SlotKind,
    tone: Option<ToneRestore>,
    running: Arc<AtomicBool>,
    owner: Option<JoinHandle<Result<(), String>>>,
    requests: Sender<OwnerRequest>,
    params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    param_ids: ParamIds,
    /// The owner's `ToneKeeper` flag: a host parameter set is a change to save.
    edited: Arc<AtomicBool>,
}

impl EngineSlotHandle {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// What the plugin was at load (an effect has an audio input). The engine reads the kind again
    /// at every reinstall, so a restart that adds or drops the input bus changes it there.
    pub(crate) fn kind(&self) -> SlotKind {
        self.kind
    }

    /// What the load did with the plugin's stored tone (`None`: nothing was stored).
    pub(crate) fn tone(&self) -> Option<ToneRestore> {
        self.tone
    }

    /// Set a parameter the plugin listed: the unit applies it at the start of the next block it
    /// renders, and a VST3 edit controller is told as well (`src-tauri/AGENTS.md` § Plugin hosting,
    /// "Host-set VST3 params go to BOTH halves"). Err for an id the plugin never listed (an unknown
    /// id can crash a plugin) or a full ring; the controller only hears what the processor got.
    pub(crate) fn set_param(&self, id: u32, value: f64) -> Result<(), String> {
        let slot = self.slot;
        let known = self.param_ids.read().map(|set| set.contains(&id)).unwrap_or(false);
        if !known {
            return Err(format!("parameter {id} is not one the plugin in slot {slot} listed"));
        }
        let pushed = self
            .params_tx
            .lock()
            .map_err(|_| format!("slot {slot} event queue is unavailable (lock poisoned)"))?
            .push(ParamEvent { id, value })
            .is_ok();
        if !pushed {
            return Err(format!("slot {slot} event queue is full; parameter {id} change was dropped"));
        }
        self.edited.store(true, Release);
        if self.format == PluginFormat::Vst3 {
            let _ = self.requests.send(OwnerRequest::SetParamNormalized(id, value));
        }
        Ok(())
    }

    /// The plugin's parameters, with live values; refreshes the ids `set_param` accepts.
    pub(crate) fn list_params(&self) -> Result<Vec<ParamDesc>, String> {
        self.ask("list_params", OwnerRequest::ListParams)
    }

    /// Save the plugin's tone now, through its owner, into the store; the tone file's bytes (empty:
    /// the plugin keeps no state). A session export takes each slot's tone fresh this way (≤ 5 s).
    pub(crate) fn take_tone(&self) -> Result<Vec<u8>, String> {
        self.ask("take_tone", OwnerRequest::SaveTone)
    }

    /// Ask for a tone save without waiting: the app's exit sends one to every slot, then waits on all
    /// the replies under one deadline.
    pub(crate) fn start_tone_save(&self) -> Result<Receiver<Result<Vec<u8>, String>>, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.requests.send(OwnerRequest::SaveTone(reply_tx)).map_err(|_| "owner thread gone".to_string())?;
        Ok(reply_rx)
    }

    /// Open the plugin's editor (≤ 5 s; an editor that comes up later is closed again).
    pub(crate) fn open_editor(&self) -> Result<(), String> {
        owner_request_5s(&self.requests, "open_editor", OwnerRequest::OpenEditor)
    }

    pub(crate) fn close_editor(&self) -> Result<(), String> {
        owner_request_5s(&self.requests, "close_editor", OwnerRequest::CloseEditor)
    }

    /// Tear the slot down in order (the unit leaves the engine → deactivate → terminate and release
    /// → the module last) and join the owner. Err when the engine did not hand the unit back: the
    /// plugin is then left loaded rather than unloaded under a running processor.
    pub(crate) fn unload(mut self) -> Result<(), String> {
        self.shutdown()
    }

    /// Test-only: an unload's first step alone: the owner's loop ends at its next check, and
    /// `unload` still joins it.
    #[cfg(test)]
    pub(super) fn stop_serving(&self) {
        self.running.store(false, Release);
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        let t = Instant::now();
        self.running.store(false, Release);
        let _ = self.requests.send(OwnerRequest::Wake);
        let result = owner.join().unwrap_or_else(|_| Err("the owner thread panicked".to_string()));
        log::info!("[plugin_host] engine slot {} owner joined in {} ms", self.slot, t.elapsed().as_millis());
        result
    }

    /// One owner round trip for a request with a typed reply, bounded at 5 s like the live commands.
    fn ask<T>(
        &self,
        op: &str,
        build: impl FnOnce(SyncSender<Result<T, String>>) -> OwnerRequest,
    ) -> Result<T, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.requests
            .send(build(reply_tx))
            .map_err(|_| "owner thread gone".to_string())?;
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("{op} timed out: {e}"))?
    }
}

impl Drop for EngineSlotHandle {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            log::error!("[plugin_host] engine slot {} unload: {e}", self.slot);
        }
    }
}

/// Test-only: an owner with no plugin behind it, whose unit is `unit` (`engine_io/plugins.rs`'s slot
/// tests). `in_engine` runs once the unit is installed, before the owner reports the load; `leaving`
/// once an unload has asked, before the unit leaves the engine.
#[cfg(test)]
pub(crate) fn stand_in(
    slot: SlotHost,
    unit: Box<dyn lf_engine::SlotProcessor>,
    in_engine: impl FnOnce() + Send + 'static,
    leaving: impl FnOnce() + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    spawn(PluginFormat::Clap, slot, 0, Arc::new(|_| {}), None, move |ctx| {
        let kind = unit.kind();
        let installed = match ctx.slot.rate() {
            Some(rate) => ctx.slot.install(unit, rate).map_err(|(_, e)| e),
            None => Err("no audio device is open".to_string()),
        };
        if let Err(e) = installed {
            let _ = ctx.ready.send(Err(e));
            return Ok(());
        }
        in_engine();
        if ctx.ready.send(Ok(Ready { name: "Stand-in".to_string(), kind, tone: None })).is_ok() {
            while ctx.running.load(std::sync::atomic::Ordering::Acquire) {
                let _ = ctx.requests.recv_timeout(OWNER_POLL);
            }
            leaving();
        }
        ctx.slot.remove(REMOVE_TIMEOUT).map(drop)
    })
}

/// Test-only: when a load's caller gave up, against its owner's check of `running`. `spawn`'s timeout
/// lowers `running`, then drops the rendezvous; which of the two the owner meets first decides how it
/// undoes the load.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GaveUp {
    /// Before the check: the owner never installs its unit, and reports the cancel.
    Before,
    /// After it: the unit is in the engine when the report finds nobody to take it. `running` stays
    /// up, so the failed report alone must undo the load.
    After,
}

/// Test-only: run an owner as `spawn` does for a caller that `gave_up` on its load, and wait (≤ 10 s)
/// for the owner to end. Returns what the owner returned and, for `Before`, what it reported (the
/// plugin's name when it reported a load).
#[cfg(test)]
pub(super) fn spawn_given_up(
    slot: SlotHost,
    gave_up: GaveUp,
    run: impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static,
) -> (Result<(), String>, Option<Result<String, String>>) {
    let running = Arc::new(AtomicBool::new(gave_up == GaveUp::After));
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = load_ready_channel();
    let ctx = OwnerCtx {
        slot,
        running: running.clone(),
        requests,
        params,
        params_tx: Arc::new(Mutex::new(params_tx)),
        param_ids: ParamIds::default(),
        sink: Arc::new(|_| {}),
        editor_parent: 0,
        ready,
        tone: ToneKeeper::new(None, Arc::new(AtomicBool::new(false))),
    };
    let ready_rx = (gave_up == GaveUp::Before).then_some(ready_rx);
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || done_tx.send(run(ctx)));
    let wait = Duration::from_secs(10);
    let report = ready_rx.map(|rx| match rx.recv_timeout(wait) {
        Ok(report) => report.map(|ready| ready.name),
        Err(e) => Err(format!("no report: {e}")),
    });
    let ended = done.recv_timeout(wait).unwrap_or_else(|_| {
        // Still serving (a bug): end it, so the test fails rather than hangs.
        running.store(false, Release);
        let _ = request_tx.send(OwnerRequest::Wake);
        Err("the owner was still running 10 s after its caller gave up".to_string())
    });
    (ended, report)
}

/// Test-only: the engine-mode tests run one at a time. Each builds whole engines (every page
/// touched) and a device thread; many at once starved the (since deleted) restart tests' reader
/// threads on a loaded machine.
#[cfg(test)]
pub(crate) fn one_engine_test_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test-only: the allocations `f` makes on this thread under the RT alloc guard.
#[cfg(all(test, debug_assertions))]
pub(super) fn rt_allocations(mut f: impl FnMut()) -> u64 {
    use super::super::rt_alloc::{allocations, guard};
    let before = allocations();
    {
        let _guard = guard();
        f();
    }
    allocations() - before
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::mpsc::channel;

    use lf_engine::grid::Frame;
    use lf_engine::SlotEvent;

    use super::*;
    use crate::engine_io::test_rig::TestDevice;

    /// The fake's unit: a silent instrument.
    struct Silent;

    impl SlotProcessor for Silent {
        fn kind(&self) -> SlotKind {
            SlotKind::Instrument
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

    /// A pause the fake takes once, where the test armed it: it tells the test it is there, then
    /// waits (≤ 5 s) for the test's go.
    #[derive(Default)]
    struct Hold(Mutex<Option<(Sender<()>, Receiver<()>)>>);

    impl Hold {
        /// The test's ends: told when the fake is there, and the go.
        fn arm(&self) -> (Receiver<()>, Sender<()>) {
            let ((there, here), (go, wait)) = (channel(), channel());
            *self.0.lock().unwrap() = Some((there, wait));
            (here, go)
        }

        fn take(&self) {
            let armed = self.0.lock().unwrap().take();
            if let Some((there, go)) = armed {
                let _ = there.send(());
                let _ = go.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    /// What the test raises in, and reads of, the fake plugin.
    #[derive(Default)]
    struct Shared {
        restart: AtomicBool,
        editor_open: AtomicBool,
        closes: AtomicU32,
        /// Taken by the next `editor` query: armed between two turns, the one before the turn's wait.
        before_wait: Hold,
        /// Taken inside the next `reinstall`, the restart's activation.
        reinstalling: Hold,
    }

    /// A plugin with nothing behind it, as `serve` drives every format: a floating editor that is a
    /// flag, a restart the test raises.
    struct Fake(Arc<Shared>);

    impl OwnedPlugin for Fake {
        type Unit = Silent;
        const FORMAT: &'static str = "fake";

        fn take_restart(&mut self) -> Option<String> {
            self.0.restart.swap(false, Relaxed).then(|| "restart".to_string())
        }
        fn reinstall(&mut self, unit: Box<Silent>, slot: &SlotHost) -> Result<(), (Box<Silent>, String)> {
            self.0.reinstalling.take();
            let rate = slot.rate().expect("the test device is open");
            slot.install(unit, rate).map_err(|(unit, e)| (own(unit), e))
        }
        fn turn(&mut self, _sink: &EventSink) -> Reported {
            Reported::default()
        }
        fn idle(&mut self) -> bool {
            false
        }
        fn params(&mut self) -> Result<Vec<ParamDesc>, String> {
            Ok(Vec::new())
        }
        fn state(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn editor(&self) -> Editor {
            self.0.before_wait.take();
            if self.0.editor_open.load(Relaxed) { Editor::Floating } else { Editor::Closed }
        }
        fn editor_open(&mut self) -> Result<(), String> {
            self.0.editor_open.store(true, Relaxed);
            Ok(())
        }
        fn editor_close(&mut self) {
            self.0.editor_open.store(false, Relaxed);
            self.0.closes.fetch_add(1, Relaxed);
        }
        fn release(self, unit: Option<Box<Silent>>) {
            drop(unit);
        }
    }

    fn load_fake(device: &TestDevice, shared: &Arc<Shared>) -> EngineSlotHandle {
        let shared = shared.clone();
        spawn(PluginFormat::Clap, device.host().slot(0), 0, Arc::new(|_| {}), None, move |ctx| {
            serve(ctx, move |setup| {
                let rate = setup.slot.rate().ok_or("no audio device is open")?;
                Ok(Loaded { plugin: Fake(shared), unit: Box::new(Silent), name: "Fake".to_string(), rate, tone: None })
            })
        })
        .expect("the fake loads")
    }

    /// A close request that waited through a restart whose cycle outlasted its caller's 5 s: the
    /// caller reported the close as failed and the UI still shows the editor open, so the owner
    /// must not close it after the cycle. The request's cancel is read after the cycle, for every
    /// format (the request order is `serve`'s alone).
    #[test]
    fn a_request_its_caller_gave_up_on_during_a_restart_is_not_served_after_it() {
        let _one = one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let shared = Arc::new(Shared::default());
        let handle = load_fake(&device, &shared);
        assert_eq!(handle.open_editor(), Ok(()));
        assert!(shared.editor_open.load(Relaxed));

        // The owner between this turn's restart checks and its wait: the restart and the close
        // arrive there, so the request's own check is the one that meets the restart.
        let (there, go) = shared.before_wait.arm();
        there.recv_timeout(Duration::from_secs(5)).expect("the owner is before its wait");
        let (cycling, activated) = shared.reinstalling.arm();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        shared.restart.store(true, Relaxed);
        handle.requests.send(OwnerRequest::CloseEditor(cancelled.clone(), reply)).unwrap();
        go.send(()).unwrap();
        cycling.recv_timeout(Duration::from_secs(5)).expect("the request waits on the restart's activation");
        // The activation outlasts the caller's bound: it gives up.
        cancelled.store(true, Relaxed);
        activated.send(()).unwrap();

        assert!(answer.recv_timeout(Duration::from_secs(2)).is_err(), "a request its caller gave up on gets no answer");
        assert!(shared.editor_open.load(Relaxed), "and the editor stays open, as the caller was told");
        assert_eq!(shared.closes.load(Relaxed), 0);
        // The player's next close is served.
        assert_eq!(handle.close_editor(), Ok(()));
        assert_eq!(shared.closes.load(Relaxed), 1);
        handle.unload().expect("it unloads");
    }
}
