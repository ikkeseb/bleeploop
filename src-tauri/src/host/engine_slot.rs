//! OWNS: the engine-mode plugin slot (threads and gotchas: `src-tauri/AGENTS.md`): the load API that
//! spawns one owner thread per plugin instance, and the handle a caller drives it through. The owner
//! loads a CLAP, VST3 or VST2 plugin, activates it at the engine's rate and puts its processor into the
//! engine as an `lf_engine::SlotProcessor` unit (`clap_engine`, `vst3_engine`, `vst2_engine`), so the
//! plugin renders inside the device callback: installed into the empty slot, or, in a hot swap, staged
//! beside the plugin another owner plays and crossfaded to (`swap`). An owner whose plugin a swap
//! replaced never puts its unit in again; it waits for its teardown. The `plugin_*` commands drive it
//! (`engine_io/plugins.rs`).
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
    AtomicBool, AtomicU32, AtomicU8,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
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
use crate::engine_io::slot_host::{Back, Standing};
use crate::engine_io::SlotHost;

#[path = "engine_slot/swap.rs"]
mod swap;

use swap::Entered;
pub(crate) use swap::{Entry, SwapBounds};

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
/// crossfade and a block, with room for a slow device. (Tests wait 10 s: the render thread of a
/// `TestDevice` can starve for 2 s on a loaded machine, and the one test that wants the wait to run
/// out only takes longer.)
pub(super) const REMOVE_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 10 } else { 2 });

/// How long an owner round trip with a typed reply waits, like the live commands. (Tests wait 30 s, for
/// the same reason as `REMOVE_TIMEOUT`.)
const ASK_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 30 } else { 5 });

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

/// Settles a load whose caller's bound runs out as the owner's setup ends: the owner claims the load
/// before its unit goes in, the caller abandons it on its timeout, and only the first of the two
/// holds. A claimed load stands (its caller waits for the report, a step away); an abandoned one
/// never installs. So a load that failed on its bound leaves no unit behind in the slot it frees.
#[derive(Default)]
pub(super) struct LoadClaim(AtomicU8);

impl LoadClaim {
    const OPEN: u8 = 0;
    const CLAIMED: u8 = 1;
    const ABANDONED: u8 = 2;

    /// The owner, before the install: false when the caller has given up (the load is cancelled).
    pub(super) fn claim(&self) -> bool {
        self.0.compare_exchange(Self::OPEN, Self::CLAIMED, AcqRel, Acquire).is_ok()
    }

    /// The caller, on its timeout: false when the owner has claimed the load first.
    fn abandon(&self) -> bool {
        self.0.compare_exchange(Self::OPEN, Self::ABANDONED, AcqRel, Acquire).is_ok()
    }
}

/// What an engine-mode owner thread starts with.
pub(super) struct OwnerCtx {
    pub(super) slot: SlotHost,
    pub(super) running: Arc<AtomicBool>,
    /// Claimed before the unit goes in (`LoadClaim`).
    pub(super) claim: Arc<LoadClaim>,
    /// How the unit goes in: an install, or a hot swap's replace.
    pub(super) entry: Entry,
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
    /// The instance's generation: its owner's tag on the slot (`SlotHost::tag`).
    pub(super) instance: u64,
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
    let OwnerCtx { slot, running, claim, entry, requests, params, params_tx, param_ids, sink, editor_parent, ready, mut tone } = ctx;
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
    // A swap called off before the claim never reaches the engine.
    let entered = if entry.cancelled() || !claim.claim() {
        Entered::Refused(unit, "plugin load cancelled".to_string())
    } else {
        swap::enter(&slot, unit, rate, &entry, &name, Instant::now())
    };
    match entered {
        Entered::Committed => {}
        // Answered first: a plugin's release can take seconds, and the caller's bound runs meanwhile.
        Entered::Refused(unit, e) => {
            let _ = ready.send(Err(e));
            plugin.release(Some(own(unit)));
            return Ok(());
        }
        // Never handed back: the plugin stays loaded rather than unloaded under a unit that may still
        // run, and whatever comes back of the unit leaks on the desk.
        Entered::Unresolved => {
            let e = "the engine did not answer the swap; the plugin is left loaded".to_string();
            log::error!("[plugin_host] engine slot {index}: {name}: {e}");
            slot.abandon();
            std::mem::forget(plugin);
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    }
    if ready.send(Ok(Ready { name: name.clone(), kind, tone: restored, instance: slot.tag() })).is_err() {
        log::warn!("[plugin_host] engine slot {index}: the load finished after its caller gave up; undoing it");
        return teardown(&slot, plugin, None, None);
    }

    let mut parked = Parked { unit: None, retired: false };
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
            // A new engine at another rate evicted the unit; it comes back stopped. A unit another owner's
            // replace retired stays parked for the teardown.
            if let Some(unit) = slot.take_back().and_then(|back| parked.back(back)) {
                cycle(&mut plugin, &slot, &mut parked, Some(unit), "re-activation after a device change");
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
                    OwnerRequest::OpenEditor(_, reply) => {
                        let was_closed = plugin.editor() == Editor::Closed;
                        let res = if was_closed { plugin.editor_open() } else { Ok(()) };
                        let opened = was_closed && res.is_ok();
                        // The reply is a rendezvous: one that finds no caller came after its 5 s, and
                        // the caller reported failure, so close what this opened (never an editor that
                        // was open before it).
                        if reply.send(res).is_err() && opened {
                            plugin.editor_close();
                        }
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
    let result = teardown(&slot, plugin, parked.unit, keep.then_some((&mut tone, name.as_str())));
    report_faults(&faults, index, &mut reported);
    result
}

/// Owner turn: cycle the plugin now if it asked for a restart, so what comes next reads it as it is.
fn settle<P: OwnedPlugin>(plugin: &mut P, slot: &SlotHost, parked: &mut Parked<P::Unit>) {
    if let Some(why) = plugin.take_restart() {
        cycle(plugin, slot, parked, None, &why);
    }
}

/// An owner's unit while it is out of the engine: from a failed cycle, or retired.
struct Parked<U> {
    /// `None` while the engine holds it.
    unit: Option<Box<U>>,
    /// Another owner's replace retired this one (its unit came back `Back::Out`): it never goes in again.
    retired: bool,
}

impl<U: SlotProcessor + 'static> Parked<U> {
    /// The owner's unit came back from the engine, by whichever call took it: a unit that came back
    /// retired marks its owner retired and waits here for the teardown; any other is the caller's to
    /// put in again.
    fn back(&mut self, back: Back) -> Option<Box<U>> {
        match back {
            Back::Evicted(unit) => Some(own(unit)),
            Back::Out(unit) => {
                self.unit = Some(own(unit));
                self.retired = true;
                None
            }
        }
    }

    /// The parked unit to put in again: never a retired one.
    fn take(&mut self) -> Option<Box<U>> {
        if self.retired { None } else { self.unit.take() }
    }
}

/// A restart or an eviction: get the unit back (it came from the engine, the owner still holds it
/// from a failed cycle, or the engine hands it back now) and reinstall it. A failed cycle parks the
/// unit, the slot bypassed, until the next one. An owner another's replace retired drops the cycle:
/// its unit never goes in again.
fn cycle<P: OwnedPlugin>(
    plugin: &mut P,
    slot: &SlotHost,
    parked: &mut Parked<P::Unit>,
    back: Option<Box<P::Unit>>,
    why: &str,
) {
    let index = slot.slot();
    let unit = match back.or_else(|| parked.take()) {
        Some(unit) => Some(unit),
        None if parked.retired => None,
        None => match slot.remove_back(REMOVE_TIMEOUT) {
            Ok(Some(back)) => parked.back(back),
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
    let Some(unit) = unit else {
        log::info!("[plugin_host] engine slot {index} {why}: the plugin was replaced; it stays out");
        return;
    };
    match plugin.reinstall(unit, slot) {
        Ok(()) => log::info!("[plugin_host] engine slot {index} {why}: deactivated → activated → reinstalled"),
        Err((unit, e)) => {
            log::error!("[plugin_host] engine slot {index} {why} failed, the slot stays bypassed: {e}");
            parked.unit = Some(unit);
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
/// engine as `entry` says (installed, or a swap's replace committed), with the plugin's tone restored
/// from `tone` (`None`: no store, nothing kept). Err when no device has opened yet (`SlotHost::rate` is
/// `None`), an install finds the slot holding a unit, a replace is refused (`swap`), or the plugin fails
/// to load or activate; never because of its tone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn load(
    format: PluginFormat,
    path: String,
    id: String,
    slot: SlotHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
    entry: Entry,
) -> Result<EngineSlotHandle, String> {
    match format {
        PluginFormat::Clap => spawn(format, slot, editor_parent, sink, tone, entry, move |ctx| {
            super::clap_engine::run(ctx, &id, move || {
                // SAFETY: PluginEntry::load runs the bundle's foreign entry-init code; the
                // out-of-process scan vetted the bundle.
                unsafe { PluginEntry::load(&path) }.map_err(|e| format!("load failed: {e}"))
            })
        }),
        PluginFormat::Vst3 => spawn(format, slot, editor_parent, sink, tone, entry, move |ctx| {
            super::vst3_host::engine::run(ctx, path, &id)
        }),
        PluginFormat::Vst2 => spawn(format, slot, editor_parent, sink, tone, entry, move |ctx| {
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
    entry: Entry,
    run: impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    spawn_within(Receiver::recv_timeout, format, slot, editor_parent, sink, tone, entry, run)
}

/// `spawn`, each wait for the owner's report `wait` given its bound (`LOAD_TIMEOUT`, then
/// `REMOVE_TIMEOUT` for a claimed load's report): the bound itself, or a test's wait on its own cue.
#[allow(clippy::too_many_arguments)]
fn spawn_within(
    mut wait: impl FnMut(&Receiver<Result<Ready, String>>, Duration) -> Result<Result<Ready, String>, RecvTimeoutError>,
    format: PluginFormat,
    slot: SlotHost,
    editor_parent: usize,
    sink: EventSink,
    tone: Option<ToneBinding>,
    entry: Entry,
    run: impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    let index = slot.slot();
    let running = Arc::new(AtomicBool::new(true));
    let claim = Arc::new(LoadClaim::default());
    let edited = Arc::new(AtomicBool::new(false));
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let params_tx = Arc::new(Mutex::new(params_tx));
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = load_ready_channel();
    let param_ids = ParamIds::default();
    let slot_host = slot.clone();
    let ctx = OwnerCtx {
        slot,
        running: running.clone(),
        claim: claim.clone(),
        entry,
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
    let report = match wait(&ready_rx, LOAD_TIMEOUT) {
        // The bound ran out as the owner's unit went in: the load stands, its report a step away.
        Err(late) if !claim.abandon() => {
            log::warn!(
                "[plugin_host] engine slot {index}: the plugin's setup outlasted its {} ms bound as it went into the engine; the load stands",
                LOAD_TIMEOUT.as_millis()
            );
            wait(&ready_rx, REMOVE_TIMEOUT).map_err(|_| late)
        }
        report => report,
    };
    match report {
        Ok(Ok(Ready { name, kind, tone, instance })) => Ok(EngineSlotHandle {
            slot: index,
            format,
            name,
            kind,
            tone,
            instance,
            slot_host,
            running,
            owner: Mutex::new(Some(owner)),
            requests: request_tx,
            params_tx,
            param_ids,
            edited,
            #[cfg(test)]
            pushed: None,
        }),
        Ok(Err(e)) => {
            running.store(false, Release);
            let _ = owner.join();
            Err(e)
        }
        Err(e) => {
            // Setup hung: the owner never installs once it finds the load abandoned, and releases the
            // plugin when its setup ends. A claimed load whose report outlasted the second wait too is
            // the owner's to undo (its ready send fails once `ready_rx` drops here), its unit still in
            // the slot this frees (`src-tauri/OPEN-THREADS.md`).
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
    instance: u64,
    /// The owner's handshake with the slot, for where it stands (`retired`).
    slot_host: SlotHost,
    running: Arc<AtomicBool>,
    /// Taken by the one caller that stops and joins the owner (`retire`), through a shared handle.
    owner: Mutex<Option<JoinHandle<Result<(), String>>>>,
    requests: Sender<OwnerRequest>,
    params_tx: Arc<Mutex<Producer<ParamEvent>>>,
    param_ids: ParamIds,
    /// The owner's `ToneKeeper` flag: a host parameter set is a change to save.
    edited: Arc<AtomicBool>,
    /// Test-only: runs inside `set_param` between its push and the controller's request.
    #[cfg(test)]
    pushed: Option<Box<dyn Fn() + Send + Sync>>,
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

    /// The instance's generation: its owner's tag on the slot (`SlotHost::tag`), new with every load.
    pub(crate) fn instance(&self) -> u64 {
        self.instance
    }

    /// Whether another owner's replace retired this instance (it never plays again), from the commit on,
    /// or the owner gave its unit up: the slot's desk says, while the owner runs (once it has ended, the
    /// desk has forgotten it). A unit out of the engine for a restart or a re-activation after a device
    /// change is not retired.
    pub(crate) fn retired(&self) -> bool {
        matches!(self.slot_host.standing(), Some(Standing::Retiring | Standing::Abandoned))
    }

    /// Set a parameter the plugin listed: the unit applies it at the start of the next block it
    /// renders, and a VST3 edit controller is told as well (`src-tauri/AGENTS.md` § Plugin hosting,
    /// "Host-set VST3 params go to BOTH halves"). Err for an id the plugin never listed (an unknown
    /// id can crash a plugin) or a full ring; the controller only hears what the processor got.
    /// The ring's lock is held through the controller's request too, so two sets from two threads
    /// reach the processor and the controller in the same order (a VST3's halves agree on the
    /// last value).
    pub(crate) fn set_param(&self, id: u32, value: f64) -> Result<(), String> {
        let slot = self.slot;
        let known = self.param_ids.read().map(|set| set.contains(&id)).unwrap_or(false);
        if !known {
            return Err(format!("parameter {id} is not one the plugin in slot {slot} listed"));
        }
        let mut ring = self.params_tx.lock().map_err(|_| format!("slot {slot} event queue is unavailable (lock poisoned)"))?;
        if ring.push(ParamEvent { id, value }).is_err() {
            return Err(format!("slot {slot} event queue is full; parameter {id} change was dropped"));
        }
        self.edited.store(true, Release);
        #[cfg(test)]
        if let Some(pushed) = &self.pushed {
            pushed();
        }
        if self.format == PluginFormat::Vst3 && self.requests.send(OwnerRequest::SetParamNormalized(id, value)).is_err() {
            return Err(format!("slot {slot} owner thread gone; parameter {id} reached the processor, not the controller"));
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

    /// `retire`, for the handle's one holder.
    pub(crate) fn unload(self) -> Result<(), String> {
        self.retire()
    }

    /// Tear the slot down in order (the unit leaves the engine → deactivate → terminate and release
    /// → the module last) and join the owner, through a handle other threads may still share. Ok once
    /// the plugin is released in order; Err when the engine did not hand the unit back (the plugin is
    /// then left loaded rather than unloaded under a running processor), when the owner panicked, or
    /// when another caller has stopped the owner already. Unbounded: a caller that must not wait for a
    /// plugin's own release bounds it from outside.
    pub(crate) fn retire(&self) -> Result<(), String> {
        self.stop_owner().unwrap_or_else(|| Err(format!("slot {}: another caller stopped this plugin's owner", self.slot)))
    }

    /// Test-only: queue a request for the owner as the methods above do, without waiting for it.
    #[cfg(test)]
    pub(super) fn queue(&self, request: OwnerRequest) {
        self.requests.send(request).expect("the owner is serving");
    }

    /// Test-only: an unload's first step alone: the owner's loop ends at its next check, and
    /// `unload` still joins it.
    #[cfg(test)]
    pub(super) fn stop_serving(&self) {
        self.running.store(false, Release);
    }

    /// Stop the owner and join it: the teardown's result, `None` when another caller took the join.
    fn stop_owner(&self) -> Option<Result<(), String>> {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        let t = Instant::now();
        self.running.store(false, Release);
        let _ = self.requests.send(OwnerRequest::Wake);
        let result = owner.join().unwrap_or_else(|_| Err("the owner thread panicked".to_string()));
        log::info!("[plugin_host] engine slot {} owner joined in {} ms", self.slot, t.elapsed().as_millis());
        Some(result)
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
            .recv_timeout(ASK_TIMEOUT)
            .map_err(|e| format!("{op} timed out: {e}"))?
    }
}

impl Drop for EngineSlotHandle {
    fn drop(&mut self) {
        if let Some(Err(e)) = self.stop_owner() {
            log::error!("[plugin_host] engine slot {} unload: {e}", self.slot);
        }
    }
}

/// Test-only: an owner with no plugin behind it, whose unit is `unit` (`engine_io/plugins.rs`'s slot
/// tests), installed. `in_engine` runs once the unit is installed, before the owner reports the load;
/// `leaving` once an unload has asked, before the unit leaves the engine.
#[cfg(test)]
pub(crate) fn stand_in(
    slot: SlotHost,
    unit: Box<dyn lf_engine::SlotProcessor>,
    in_engine: impl FnOnce() + Send + 'static,
    leaving: impl FnOnce() + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    stand_in_entering(slot, unit, Entry::Install, in_engine, leaving)
}

/// Test-only: `stand_in`, its unit entering as `entry` says (a swap's replace: `in_engine` runs once it
/// committed).
#[cfg(test)]
pub(crate) fn stand_in_entering(
    slot: SlotHost,
    unit: Box<dyn lf_engine::SlotProcessor>,
    entry: Entry,
    in_engine: impl FnOnce() + Send + 'static,
    leaving: impl FnOnce() + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    spawn(PluginFormat::Clap, slot, 0, Arc::new(|_| {}), None, entry, stand_in_owner(unit, in_engine, leaving))
}

/// Test-only: `stand_in`'s owner.
#[cfg(test)]
fn stand_in_owner(
    unit: Box<dyn lf_engine::SlotProcessor>,
    in_engine: impl FnOnce() + Send + 'static,
    leaving: impl FnOnce() + Send + 'static,
) -> impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static {
    move |ctx| {
        let kind = unit.kind();
        let entered = match ctx.slot.rate() {
            _ if ctx.entry.cancelled() || !ctx.claim.claim() => Entered::Refused(unit, "plugin load cancelled".to_string()),
            Some(rate) => swap::enter(&ctx.slot, unit, rate, &ctx.entry, "Stand-in", Instant::now()),
            None => Entered::Refused(unit, "no audio device is open".to_string()),
        };
        let refused = match entered {
            Entered::Committed => None,
            Entered::Refused(_, e) => Some(e),
            Entered::Unresolved => {
                ctx.slot.abandon();
                Some("the engine did not answer the swap".to_string())
            }
        };
        if let Some(e) = refused {
            let _ = ctx.ready.send(Err(e));
            return Ok(());
        }
        in_engine();
        if ctx.ready.send(Ok(Ready { name: "Stand-in".to_string(), kind, tone: None, instance: ctx.slot.tag() })).is_ok() {
            while ctx.running.load(std::sync::atomic::Ordering::Acquire) {
                let _ = ctx.requests.recv_timeout(OWNER_POLL);
            }
            leaving();
        }
        ctx.slot.remove(REMOVE_TIMEOUT).map(drop)
    }
}

/// Test-only: when a load's caller gave up, against its owner's claim (`LoadClaim`). `spawn`'s timeout
/// abandons a load its owner has not claimed; one it has, it gives up on only when the report outlasts
/// a second wait too, and then drops the rendezvous. Which the owner meets decides how it undoes the
/// load.
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
    let claim = Arc::new(LoadClaim::default());
    if gave_up == GaveUp::Before {
        claim.abandon();
    }
    let (params_tx, params) = RingBuffer::<ParamEvent>::new(EVENT_RING_CAP);
    let (request_tx, requests) = std::sync::mpsc::channel();
    let (ready, ready_rx) = load_ready_channel();
    let ctx = OwnerCtx {
        slot,
        running: running.clone(),
        claim,
        entry: Entry::Install,
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
    use crate::engine_io::slot_host::Standing;
    use crate::engine_io::test_rig::{TestDevice, PATIENCE};

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
                let _ = go.recv_timeout(PATIENCE);
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
        /// The unit's end of the param ring, which no unit drains here: the test reads it.
        ring: Mutex<Option<Consumer<ParamEvent>>>,
        /// What a VST3 edit controller would hear (`set_param_normalized`), in order.
        controller: Mutex<Vec<(u32, f64)>>,
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
        fn set_param_normalized(&mut self, id: u32, value: f64) {
            self.0.controller.lock().unwrap().push((id, value));
        }
        fn release(self, unit: Option<Box<Silent>>) {
            drop(unit);
        }
    }

    fn load_fake(device: &TestDevice, shared: &Arc<Shared>) -> EngineSlotHandle {
        load_fake_as(PluginFormat::Clap, device, shared)
    }

    fn load_fake_as(format: PluginFormat, device: &TestDevice, shared: &Arc<Shared>) -> EngineSlotHandle {
        let shared = shared.clone();
        spawn(format, device.host().slot(0), 0, Arc::new(|_| {}), None, Entry::Install, move |ctx| {
            serve(ctx, move |setup| {
                let rate = setup.slot.rate().ok_or("no audio device is open")?;
                *shared.ring.lock().unwrap() = Some(setup.params);
                Ok(Loaded { plugin: Fake(shared), unit: Box::new(Silent), name: "Fake".to_string(), rate, tone: None })
            })
        })
        .expect("the fake loads")
    }

    /// An editor open whose caller stops waiting after the owner's cancel check (its 5 s run out
    /// as the editor comes up) was reported as failed: the editor this request opened must not stay
    /// open behind the player's back.
    #[test]
    fn an_editor_opened_for_a_caller_that_stopped_waiting_is_closed_again() {
        let _one = one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let shared = Arc::new(Shared::default());
        let handle = load_fake(&device, &shared);
        let (reply, answer) = super::super::owner_reply_channel();
        handle.requests.send(OwnerRequest::OpenEditor(Arc::new(AtomicBool::new(false)), reply)).unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !shared.editor_open.load(Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(shared.editor_open.load(Relaxed), "the owner opens the editor");
        // The caller has stopped receiving but its end still exists (between its timeout and the drop).
        std::thread::sleep(Duration::from_millis(100));
        drop(answer);
        let deadline = Instant::now() + PATIENCE;
        while shared.closes.load(Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(shared.closes.load(Relaxed), 1, "the owner closes what it opened");
        assert!(!shared.editor_open.load(Relaxed));
        // An editor that was open before an undelivered open stays open.
        assert_eq!(handle.open_editor(), Ok(()));
        let (reply, answer) = super::super::owner_reply_channel();
        drop(answer);
        handle.requests.send(OwnerRequest::OpenEditor(Arc::new(AtomicBool::new(false)), reply)).unwrap();
        assert_eq!(handle.close_editor(), Ok(()), "served after that open");
        assert_eq!(shared.closes.load(Relaxed), 2, "closed once more, by the close alone");
        handle.unload().expect("it unloads");
    }

    /// Two host sets of one VST3 parameter from two threads, the first paused between its push and
    /// its controller request: the processor and the controller must hear them in the same order,
    /// or the halves end on different values.
    #[test]
    fn two_concurrent_param_sets_reach_both_halves_in_one_order() {
        let _one = one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let shared = Arc::new(Shared::default());
        let mut handle = load_fake_as(PluginFormat::Vst3, &device, &shared);
        handle.param_ids.write().unwrap().insert(7);
        let pause = Arc::new(Hold::default());
        let (there, go) = pause.arm();
        let paused = pause.clone();
        handle.pushed = Some(Box::new(move || paused.take()));
        std::thread::scope(|scope| {
            let handle = &handle;
            let first = scope.spawn(move || handle.set_param(7, 0.25));
            there.recv_timeout(PATIENCE).expect("the first set has pushed");
            let second = scope.spawn(move || handle.set_param(7, 0.75));
            // Room for the second to run to its end, if nothing holds it back.
            std::thread::sleep(Duration::from_millis(100));
            go.send(()).unwrap();
            assert_eq!(first.join().unwrap(), Ok(()));
            assert_eq!(second.join().unwrap(), Ok(()));
        });
        let mut ring = shared.ring.lock().unwrap().take().expect("the load kept the ring");
        let processor: Vec<(u32, f64)> = std::iter::from_fn(|| ring.pop().ok()).map(|e| (e.id, e.value)).collect();
        assert_eq!(processor, [(7, 0.25), (7, 0.75)]);
        let deadline = Instant::now() + PATIENCE;
        while shared.controller.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(*shared.controller.lock().unwrap(), processor, "the controller hears them in the processor's order");
        handle.unload().expect("it unloads");
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
        there.recv_timeout(PATIENCE).expect("the owner is before its wait");
        let (cycling, activated) = shared.reinstalling.arm();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        shared.restart.store(true, Relaxed);
        handle.requests.send(OwnerRequest::CloseEditor(cancelled.clone(), reply)).unwrap();
        go.send(()).unwrap();
        cycling.recv_timeout(PATIENCE).expect("the request waits on the restart's activation");
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

    /// The setup's bound runs out just as the owner puts its unit in the engine: the load stands.
    /// Failed, it would free the slot while its unit is still there, and the next load into the slot
    /// would fail on it ("already holds a unit").
    #[test]
    fn a_load_whose_unit_goes_in_as_its_bound_runs_out_stands() {
        let _one = one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        // The bound runs out once the owner has claimed the load and its unit is in the engine, its
        // report held until then: the order is the test's, not the runner's.
        let (in_engine_tx, in_engine) = channel();
        let (ran_out_tx, ran_out) = channel();
        let report_late = move || {
            in_engine_tx.send(()).unwrap();
            ran_out.recv_timeout(PATIENCE).expect("the bound ran out");
        };
        let mut cue = Some((in_engine, ran_out_tx));
        let wait = move |ready: &Receiver<Result<Ready, String>>, _bound| match cue.take() {
            Some((in_engine, ran_out_tx)) => {
                in_engine.recv_timeout(PATIENCE).expect("the unit goes in");
                ran_out_tx.send(()).unwrap();
                Err(RecvTimeoutError::Timeout)
            }
            // The claimed load's report comes once the owner is scheduled again, however late: only
            // a deadlock outlasts this.
            None => ready.recv_timeout(Duration::from_secs(30)),
        };
        let owner = stand_in_owner(Box::new(Silent), report_late, || {});
        let loaded = spawn_within(wait, PluginFormat::Clap, device.host().slot(0), 0, Arc::new(|_| {}), None, Entry::Install, owner);
        let handle = loaded.expect("the owner had its unit going in: the load stands");
        assert_eq!(handle.name(), "Stand-in");
        let next = stand_in(device.host().slot(0), Box::new(Silent), || {}, || {}).err().expect("the slot is taken");
        assert!(next.contains("already holds a unit"), "{next}");
        handle.unload().expect("it unloads");
        stand_in(device.host().slot(0), Box::new(Silent), || {}, || {}).expect("the slot takes the next load").unload().unwrap();
    }

    /// A setup that outlasts its bound: the load fails and the owner never puts its unit in the
    /// engine, so the slot takes the next load once the owner has ended.
    #[test]
    fn a_setup_past_its_bound_fails_and_leaves_the_slot_free() {
        let _one = one_engine_test_at_a_time();
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let shared = Arc::new(Shared::default());
        let (ended_tx, ended) = channel();
        // The setup goes on only once its bound has run out and the load has failed.
        let (failed_tx, failed) = channel();
        let fake = shared.clone();
        let bound = |_: &Receiver<Result<Ready, String>>, _| Err(RecvTimeoutError::Timeout);
        let late = spawn_within(bound, PluginFormat::Clap, device.host().slot(0), 0, Arc::new(|_| {}), None, Entry::Install, move |ctx| {
            let served = serve(ctx, move |setup| {
                failed.recv_timeout(PATIENCE).map_err(|e| format!("the load never failed: {e}"))?;
                let rate = setup.slot.rate().ok_or("no audio device is open")?;
                Ok(Loaded { plugin: Fake(fake), unit: Box::new(Silent), name: "Fake".to_string(), rate, tone: None })
            });
            let _ = ended_tx.send(served.clone());
            served
        });
        assert!(late.as_ref().is_err_and(|e| e.contains("timed out")), "{:?}", late.err());
        failed_tx.send(()).unwrap();
        assert_eq!(ended.recv_timeout(PATIENCE), Ok(Ok(())), "the owner ends on its own");
        load_fake(&device, &shared).unload().expect("the slot takes the next load");
    }

    /// A swap test's plugin instance: its unit's latency and start, the unit's calls, and what its owner
    /// did with the plugin.
    #[derive(Default)]
    struct Bench {
        latency: Frame,
        refuse: bool,
        calls: Arc<AtomicU32>,
        restart: AtomicBool,
        /// Taken inside the `take_restart` that takes a restart, before it returns.
        restarting: Hold,
        reinstalls: AtomicU32,
        /// Each `release`: whether the unit came back with it.
        releases: Mutex<Vec<bool>>,
        /// Taken inside `release`, before it counts.
        releasing: Hold,
        /// The plugin's drops: a `release` ends in one.
        drops: AtomicU32,
        /// The unit's stops, the plugin's quiesce and its release, in order.
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Bench {
        fn calls(&self) -> u32 {
            self.calls.load(Relaxed)
        }

        fn releases(&self) -> Vec<bool> {
            self.releases.lock().unwrap().clone()
        }
    }

    /// `Bench`'s unit: an effect that counts its calls.
    struct Probe {
        latency: Frame,
        refuse: bool,
        calls: Arc<AtomicU32>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl SlotProcessor for Probe {
        fn kind(&self) -> SlotKind {
            SlotKind::Effect
        }
        fn latency(&self) -> Frame {
            self.latency
        }
        fn process(&mut self, _frame: Frame, _input: &[f32], _events: &[SlotEvent], out: &mut [f32]) {
            self.calls.fetch_add(1, Relaxed);
            out.fill(0.0);
        }
        fn stop(&mut self) {
            self.order.lock().unwrap().push("stop");
        }
        fn start_refused(&self) -> bool {
            self.refuse
        }
        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    struct Benched(Arc<Bench>);

    impl Drop for Benched {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Relaxed);
        }
    }

    impl OwnedPlugin for Benched {
        type Unit = Probe;
        const FORMAT: &'static str = "bench";

        fn take_restart(&mut self) -> Option<String> {
            let restart = self.0.restart.swap(false, Relaxed);
            if restart {
                self.0.restarting.take();
            }
            restart.then(|| "restart".to_string())
        }
        fn reinstall(&mut self, unit: Box<Probe>, slot: &SlotHost) -> Result<(), (Box<Probe>, String)> {
            self.0.reinstalls.fetch_add(1, Relaxed);
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
            Editor::Closed
        }
        fn editor_open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn editor_close(&mut self) {}
        fn quiesce(&mut self) {
            self.0.order.lock().unwrap().push("quiesce");
        }
        fn release(self, unit: Option<Box<Probe>>) {
            self.0.releasing.take();
            self.0.order.lock().unwrap().push("release");
            self.0.releases.lock().unwrap().push(unit.is_some());
        }
    }

    type Wait = Result<Result<Ready, String>, RecvTimeoutError>;

    /// Load a `Bench` plugin through `slot` (the test keeps a clone: the same owner) as `entry` says,
    /// each wait for its report `wait`.
    fn bench_within(
        wait: impl FnMut(&Receiver<Result<Ready, String>>, Duration) -> Wait,
        slot: &SlotHost,
        bench: &Arc<Bench>,
        entry: Entry,
    ) -> Result<EngineSlotHandle, String> {
        let bench = bench.clone();
        spawn_within(wait, PluginFormat::Clap, slot.clone(), 0, Arc::new(|_| {}), None, entry, move |ctx| {
            serve(ctx, move |setup| {
                let rate = setup.slot.rate().ok_or("no audio device is open")?;
                let unit = Box::new(Probe { latency: bench.latency, refuse: bench.refuse, calls: bench.calls.clone(), order: bench.order.clone() });
                Ok(Loaded { plugin: Benched(bench), unit, name: "Bench".to_string(), rate, tone: None })
            })
        })
    }

    fn bench_in(slot: &SlotHost, bench: &Arc<Bench>, entry: Entry) -> Result<EngineSlotHandle, String> {
        bench_within(Receiver::recv_timeout, slot, bench, entry)
    }

    /// A swap at gain 1 that `cancel` calls off.
    fn swap_entry(cancel: &Arc<AtomicBool>, bounds: SwapBounds) -> Entry {
        Entry::Replace { gain: 1.0, cancel: cancel.clone(), bounds }
    }

    /// A device rendering a 256-frame block every 5 ms, with `old` playing in slot 0: its owner's slot
    /// handle and the plugin's.
    fn swap_rig(old: &Arc<Bench>) -> (TestDevice, SlotHost, EngineSlotHandle) {
        let device = TestDevice::start(48_000, 256, Duration::from_millis(5), |_| 0.0);
        let slot = device.host().slot(0);
        let playing = bench_in(&slot, old, Entry::Install).expect("the playing plugin loads");
        (device, slot, playing)
    }

    /// `bench`'s unit is called again within a few blocks.
    fn plays(device: &TestDevice, bench: &Bench) -> bool {
        let calls = bench.calls();
        device.wait_blocks(5) && bench.calls() > calls
    }

    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A swap commits: the new plugin plays as its own instance, the old owner is retired at the commit
    /// and reads retired throughout, its unit back with its owner included, and stopping it releases the
    /// plugin with its unit, which never went in again, in the teardown's order: the unit stopped, the
    /// plugin quiesced, released.
    #[test]
    fn a_swap_commits_and_the_replaced_owner_retires_without_reinstalling() {
        let _one = one_engine_test_at_a_time();
        let (old, new) = (Arc::new(Bench::default()), Arc::new(Bench::default()));
        let (device, old_slot, playing) = swap_rig(&old);
        let new_slot = device.host().slot(0);
        let cancel = Arc::new(AtomicBool::new(false));
        let swapped = bench_in(&new_slot, &new, swap_entry(&cancel, SwapBounds::default())).expect("the swap commits");
        assert_eq!((swapped.instance(), playing.instance()), (new_slot.tag(), old_slot.tag()));
        assert_ne!(old_slot.standing(), Some(Standing::Active), "retired at the commit");
        assert!(plays(&device, &new), "the new plugin plays");
        eventually("the old unit fades out and its owner takes it back", || {
            assert!(playing.retired(), "retired throughout the handover");
            old_slot.holds_nothing()
        });
        assert!(playing.retired() && old_slot.standing() == Some(Standing::Retiring), "still retired, its unit back");
        let calls = old.calls();
        assert!(device.wait_blocks(5));
        assert_eq!(old.calls(), calls, "the old unit is out of the engine");
        playing.retire().expect("the old owner releases its plugin");
        assert_eq!((old.releases(), old.reinstalls.load(Relaxed)), (vec![true], 0), "released with its unit, never reinstalled");
        let mut order = old.order.lock().unwrap().clone();
        order.dedup();
        assert_eq!(order, ["stop", "quiesce", "release"], "the teardown's order");
        assert!(plays(&device, &new), "the new plugin plays on");
        swapped.unload().expect("it unloads");
        assert_eq!(new.releases(), [true]);
    }

    /// The replaced plugin's restart is taken before the swap commits and its cycle's remove runs after
    /// (the fake holds the owner between the two, as the commit lands): the remove takes the unit back
    /// retired, so the owner never reinstalls it, nor at a restart once the new plugin has left the slot.
    #[test]
    fn a_restart_taken_as_the_swap_commits_never_reinstalls_the_replaced_plugin() {
        let _one = one_engine_test_at_a_time();
        let (old, new) = (Arc::new(Bench::default()), Arc::new(Bench::default()));
        let (device, old_slot, playing) = swap_rig(&old);
        let (there, go) = old.restarting.arm();
        old.restart.store(true, Relaxed);
        there.recv_timeout(PATIENCE).expect("the old owner takes the restart");
        assert_eq!(old_slot.standing(), Some(Standing::Active), "taken before the commit");
        let cancel = Arc::new(AtomicBool::new(false));
        let swapped = bench_in(&device.host().slot(0), &new, swap_entry(&cancel, SwapBounds::default())).expect("the swap commits");
        go.send(()).unwrap();
        eventually("the old owner's remove takes its unit back", || old_slot.holds_nothing());
        // A few owner turns (20 ms each), for a reinstall to show.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(old.reinstalls.load(Relaxed), 0, "never reinstalled");
        swapped.unload().expect("the new plugin unloads");
        old.restart.store(true, Relaxed);
        eventually("the old owner takes the second restart", || !old.restart.load(Relaxed));
        std::thread::sleep(Duration::from_millis(100));
        let calls = old.calls();
        assert!(device.wait_blocks(5));
        assert_eq!((old.reinstalls.load(Relaxed), old.calls(), old_slot.standing()), (0, calls, Some(Standing::Retiring)), "nor once the slot is empty");
        playing.retire().expect("the old owner releases its plugin");
        assert_eq!(old.releases(), [true]);
    }

    /// The replaced plugin asks for a restart after the commit, then again once its unit is back with
    /// its owner: neither reinstalls it.
    #[test]
    fn a_restart_the_replaced_plugin_asks_for_never_reinstalls_it() {
        let _one = one_engine_test_at_a_time();
        let (old, new) = (Arc::new(Bench::default()), Arc::new(Bench::default()));
        let (device, old_slot, playing) = swap_rig(&old);
        let cancel = Arc::new(AtomicBool::new(false));
        let swapped = bench_in(&device.host().slot(0), &new, swap_entry(&cancel, SwapBounds::default())).expect("the swap commits");
        old.restart.store(true, Relaxed);
        eventually("the old owner takes the restart", || !old.restart.load(Relaxed));
        eventually("the old unit is back", || old_slot.holds_nothing());
        old.restart.store(true, Relaxed);
        eventually("the old owner takes the second restart", || !old.restart.load(Relaxed));
        // A few owner turns (20 ms each), for a reinstall to show.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!((old.reinstalls.load(Relaxed), old_slot.standing()), (0, Some(Standing::Retiring)), "never reinstalled");
        assert!(plays(&device, &new), "the new plugin plays on");
        playing.retire().expect("the old owner releases its plugin");
        assert_eq!(old.releases(), [true]);
        swapped.unload().expect("it unloads");
    }

    /// A candidate that refuses to start: the load fails with the refusal, answered before the plugin's
    /// release (which gets its unit back), and the playing plugin plays on, its owner Active.
    #[test]
    fn a_candidate_that_refuses_to_start_is_answered_before_its_release_and_the_old_plays_on() {
        let _one = one_engine_test_at_a_time();
        let old = Arc::new(Bench::default());
        let new = Arc::new(Bench { refuse: true, ..Bench::default() });
        let (device, old_slot, playing) = swap_rig(&old);
        // The release waits for the go the wait gives once it has the answer.
        let (_there, go) = new.releasing.arm();
        let released_at_answer = Arc::new(Mutex::new(None));
        let (watched, at_answer) = (new.clone(), released_at_answer.clone());
        let wait = move |ready: &Receiver<Result<Ready, String>>, bound| {
            let report = ready.recv_timeout(bound);
            *at_answer.lock().unwrap() = Some(watched.releases().len());
            let _ = go.send(());
            report
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let refused = bench_within(wait, &device.host().slot(0), &new, swap_entry(&cancel, SwapBounds::default())).err();
        assert_eq!(refused.as_deref(), Some("Bench refused to start processing; the playing plugin stays"));
        assert_eq!(*released_at_answer.lock().unwrap(), Some(0), "answered before the release");
        assert_eq!(new.releases(), [true], "then released, with its unit back");
        assert_eq!(old_slot.standing(), Some(Standing::Active));
        assert!(plays(&device, &old), "the old plugin plays on");
        playing.unload().expect("it unloads");
    }

    /// A swap called off before its claim: nothing enters the engine, and its plugin is released.
    #[test]
    fn a_swap_cancelled_before_its_claim_never_enters_the_engine() {
        let _one = one_engine_test_at_a_time();
        let (old, new) = (Arc::new(Bench::default()), Arc::new(Bench::default()));
        let (device, old_slot, playing) = swap_rig(&old);
        let new_slot = device.host().slot(0);
        let refused = bench_in(&new_slot, &new, swap_entry(&Arc::new(AtomicBool::new(true)), SwapBounds::default())).err();
        assert_eq!(refused.as_deref(), Some("plugin load cancelled"));
        assert_eq!((new.calls(), new_slot.standing(), new.releases()), (0, None, vec![true]));
        assert_eq!(old_slot.standing(), Some(Standing::Active));
        assert!(plays(&device, &old), "the old plugin plays on");
        playing.unload().expect("it unloads");
    }

    /// A candidate staging through a long latency (10 s of frames) that its caller calls off: withdrawn,
    /// the load cancelled, its plugin released, and the old plugin plays on.
    #[test]
    fn a_swap_cancelled_while_its_candidate_stages_is_withdrawn() {
        let _one = one_engine_test_at_a_time();
        let old = Arc::new(Bench::default());
        let new = Arc::new(Bench { latency: 480_000, ..Bench::default() });
        let (device, old_slot, playing) = swap_rig(&old);
        let new_slot = device.host().slot(0);
        let cancel = Arc::new(AtomicBool::new(false));
        let refused = std::thread::scope(|s| {
            let swap = s.spawn(|| bench_in(&new_slot, &new, swap_entry(&cancel, SwapBounds::default())).err());
            eventually("the candidate stages", || new.calls() > 0);
            cancel.store(true, Release);
            swap.join().unwrap()
        });
        assert_eq!(refused.as_deref(), Some("plugin load cancelled"));
        assert_eq!((new_slot.standing(), new.releases()), (None, vec![true]));
        assert_eq!(old_slot.standing(), Some(Standing::Active));
        assert!(plays(&device, &old), "the old plugin plays on");
        playing.unload().expect("it unloads");
    }

    /// A candidate whose staging outlasts the commit deadline (as a take recording at another latency
    /// holds one): withdrawn, the load refused busy, its plugin released, and the old plugin plays on.
    #[test]
    fn a_swap_past_its_commit_deadline_is_withdrawn_and_reported_busy() {
        let _one = one_engine_test_at_a_time();
        let old = Arc::new(Bench::default());
        let new = Arc::new(Bench { latency: 480_000, ..Bench::default() });
        let (device, old_slot, playing) = swap_rig(&old);
        let new_slot = device.host().slot(0);
        let bounds = SwapBounds { commit: Duration::from_millis(100), ..SwapBounds::default() };
        let refused = bench_in(&new_slot, &new, swap_entry(&Arc::new(AtomicBool::new(false)), bounds)).err();
        assert_eq!(refused.as_deref(), Some("busy: a take is recording at another latency; swap after it"));
        assert_eq!((new_slot.standing(), new.releases()), (None, vec![true]));
        assert_eq!(old_slot.standing(), Some(Standing::Active));
        assert!(plays(&device, &old), "the old plugin plays on");
        playing.unload().expect("it unloads");
    }

    /// The engine answers neither the replace nor its withdraw by the settle deadline (no block runs while
    /// the test holds the engine lock): the owner abandons its unit and leaves its plugin loaded (never
    /// released nor dropped, also once the owner has ended); once the engine runs again the unit comes
    /// back and leaks on the desk, and the old plugin plays on.
    #[test]
    fn a_swap_the_engine_never_answers_is_abandoned_and_its_plugin_left_loaded() {
        let _one = one_engine_test_at_a_time();
        let (old, new) = (Arc::new(Bench::default()), Arc::new(Bench::default()));
        let (device, old_slot, playing) = swap_rig(&old);
        let new_slot = device.host().slot(0);
        let bounds = SwapBounds { commit: Duration::from_millis(20), settle: Duration::from_millis(200) };
        let held = device.host().core.rt.lock().unwrap();
        let refused = bench_in(&new_slot, &new, swap_entry(&Arc::new(AtomicBool::new(false)), bounds)).err();
        assert_eq!(refused.as_deref(), Some("the engine did not answer the swap; the plugin is left loaded"));
        // The owner has ended: a load that reports an error is joined before it returns.
        assert_eq!(new.drops.load(Relaxed), 0, "the plugin is never dropped");
        assert_eq!(new_slot.standing(), Some(Standing::Abandoned));
        drop(held);
        eventually("the withdrawn unit comes back and leaks", || new_slot.standing().is_none());
        assert_eq!((new.releases(), new.drops.load(Relaxed)), (vec![], 0), "never released nor dropped");
        assert_eq!(old_slot.standing(), Some(Standing::Active));
        assert!(plays(&device, &old), "the old plugin plays on");
        playing.unload().expect("it unloads");
    }

    /// A retire thread stops and joins the owner through its clone of a shared handle while another
    /// clone is alive: the plugin is released with its unit; a second retire, while the first still
    /// joins (the plugin's release held), is refused at once.
    #[test]
    fn a_retire_through_a_shared_handle_releases_the_plugin_and_a_second_one_does_not_wait() {
        let _one = one_engine_test_at_a_time();
        let old = Arc::new(Bench::default());
        let (_device, slot, playing) = swap_rig(&old);
        let shared = Arc::new(playing);
        let other = shared.clone();
        let (there, go) = old.releasing.arm();
        let first = std::thread::spawn(move || shared.retire());
        there.recv_timeout(PATIENCE).expect("the first retire reaches the plugin's release");
        // On its own thread, so a second retire that waits for the first fails here rather than hangs.
        let (again_tx, again) = channel();
        std::thread::spawn(move || again_tx.send(other.retire()));
        let again = again.recv_timeout(Duration::from_secs(1)).expect("refused while the first still joins");
        assert!(again.as_ref().is_err_and(|e| e.contains("another caller")), "{again:?}");
        assert!(old.releases().is_empty(), "the first is still releasing");
        go.send(()).unwrap();
        first.join().unwrap().expect("released in order");
        assert_eq!((old.releases(), slot.standing()), (vec![true], None));
    }
}
