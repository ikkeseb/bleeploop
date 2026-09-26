//! OWNS: the engine-mode plugin slot (`docs/plans/native-engine.md` § Stage 4): the load API that
//! spawns one owner thread per slot, and the handle a caller drives it through. The owner loads a
//! CLAP or VST3 plugin as the live owners do, activates it at the engine's rate and installs its
//! processor into the engine as an `lf_engine::SlotProcessor` unit (`clap_engine`, `vst3_engine`),
//! so the plugin renders inside the device callback instead of on its own RT thread. Engine mode's
//! `plugin_*` commands drive it (`engine_io/plugins.rs`); the live owners (`owner_main`,
//! `vst3_owner_main`) and everything they drive are the web audio path's, untouched.
//!
//! An owner services what the live owner does, less the device (the engine owns it): params (the
//! unit drains the slot's event ring; a VST3 set reaches the edit controller too), state, editors
//! with their Win32 pump, plugin callbacks and params rescans, a plugin-requested restart and an
//! eviction (the unit comes back → deactivate → activate at the engine's current rate and block →
//! reinstall; the slot is bypassed meanwhile and the engine never waits), and the ordered teardown.
//! Events go to a sink closure instead of a window, so an owner runs headless. Notes never pass
//! through here: the engine routes them (`Command::SelectInstrument(NoteTarget::Slot(i))`, then
//! `Command::NoteOn`).

use std::sync::atomic::{
    AtomicBool, AtomicU32,
    Ordering::{Relaxed, Release},
};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clack_host::entry::PluginEntry;
use lf_engine::SlotKind;
use rtrb::{Consumer, Producer, RingBuffer};

use super::super::state::ParamDesc;
use super::{
    load_ready_channel, owner_request_5s, OwnerRequest, ParamIds, PluginEvent, EVENT_RING_CAP,
};
use crate::engine_io::SlotHost;

/// The formats an engine slot hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PluginFormat {
    Clap,
    Vst3,
}

/// What an owner tells its caller (the window at Stage 5; a probe or a test before that).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum EngineSlotEvent {
    /// The plugin's own editor moved a parameter (VST3 `performEdit`); the processor has it too.
    ParamChanged { id: u32, value: f64 },
    /// The plugin changed its parameters behind the host's back (CLAP `params.rescan`, VST3
    /// `restartComponent` with a param flag): list them again.
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
/// serviced within this (the live CLAP owner's `CALLBACK_POLL_INTERVAL`).
pub(super) const OWNER_POLL: Duration = Duration::from_millis(20);

/// How long `load` waits for the owner's report, as the live load does.
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// The fault bits a unit latches on the audio thread; the owner reports each once per load.
pub(super) const FAULT_START: u32 = 1 << 0;
pub(super) const FAULT_PROCESS: u32 = 1 << 1;
pub(super) const FAULT_PARAM: u32 = 1 << 2;

const FAULT_MESSAGES: [(u32, &str); 3] = [
    (FAULT_START, "the plugin refused to start processing; the slot is silent until it is reinstalled"),
    (FAULT_PROCESS, "a process call failed; that block's output may be incomplete"),
    (FAULT_PARAM, "a parameter id the plugin cannot take was dropped"),
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
    pub(super) params: Consumer<PluginEvent>,
    /// The ring's producer, for a VST3 editor's `performEdit` (the handle holds the same one).
    pub(super) params_tx: Arc<Mutex<Producer<PluginEvent>>>,
    pub(super) param_ids: ParamIds,
    pub(super) sink: EventSink,
    /// The main window's HWND (0 = none): a floating CLAP editor's transient parent.
    pub(super) editor_parent: usize,
    /// Rendezvous (`load_ready_channel`): a send that fails means the caller gave up, and the owner
    /// undoes the load itself.
    pub(super) ready: SyncSender<Result<Ready, String>>,
}

/// An installed load, as the owner reports it.
pub(super) struct Ready {
    pub(super) name: String,
    pub(super) kind: SlotKind,
}

/// Load plugin `id` from `path` into the engine slot `slot`: spawn its owner thread
/// (`lf-clap-engine-{slot}` / `lf-vst3-engine-{slot}`) and wait (≤ 15 s) until the unit is in the
/// engine. Err when no device has opened yet (`SlotHost::rate` is `None`), the slot already holds a
/// unit, or the plugin fails to load or activate.
pub(crate) fn load(
    format: PluginFormat,
    path: String,
    id: String,
    slot: SlotHost,
    editor_parent: usize,
    sink: EventSink,
) -> Result<EngineSlotHandle, String> {
    match format {
        PluginFormat::Clap => spawn(format, slot, editor_parent, sink, move |ctx| {
            super::clap_engine::run(ctx, &id, move || {
                // SAFETY: PluginEntry::load runs the bundle's foreign entry-init code; the
                // out-of-process scan vetted the bundle, as for the live load.
                unsafe { PluginEntry::load(&path) }.map_err(|e| format!("load failed: {e}"))
            })
        }),
        PluginFormat::Vst3 => spawn(format, slot, editor_parent, sink, move |ctx| {
            super::vst3_host::engine::run(ctx, path, &id)
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
    run: impl FnOnce(OwnerCtx) -> Result<(), String> + Send + 'static,
) -> Result<EngineSlotHandle, String> {
    let index = slot.slot();
    let running = Arc::new(AtomicBool::new(true));
    let (params_tx, params) = RingBuffer::<PluginEvent>::new(EVENT_RING_CAP);
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
    };
    let tag = match format {
        PluginFormat::Clap => "clap",
        PluginFormat::Vst3 => "vst3",
    };
    let owner = std::thread::Builder::new()
        .name(format!("lf-{tag}-engine-{index}"))
        .spawn(move || run(ctx))
        .map_err(|e| format!("spawn owner thread: {e}"))?;
    match ready_rx.recv_timeout(LOAD_TIMEOUT) {
        Ok(Ok(Ready { name, kind })) => Ok(EngineSlotHandle {
            slot: index,
            format,
            name,
            kind,
            running,
            owner: Some(owner),
            requests: request_tx,
            params_tx,
            param_ids,
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
    running: Arc<AtomicBool>,
    owner: Option<JoinHandle<Result<(), String>>>,
    requests: Sender<OwnerRequest>,
    params_tx: Arc<Mutex<Producer<PluginEvent>>>,
    param_ids: ParamIds,
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

    /// Set a parameter the plugin listed: the unit applies it at the start of the next block it
    /// renders, and a VST3 edit controller is told as well (`src-tauri/AGENTS.md` § P11,
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
            .push(PluginEvent::Param { id, value })
            .is_ok();
        if !pushed {
            return Err(format!("slot {slot} event queue is full; parameter {id} change was dropped"));
        }
        if self.format == PluginFormat::Vst3 {
            let _ = self.requests.send(OwnerRequest::SetParamNormalized(id, value));
        }
        Ok(())
    }

    /// The plugin's parameters, with live values; refreshes the ids `set_param` accepts.
    pub(crate) fn list_params(&self) -> Result<Vec<ParamDesc>, String> {
        self.ask("list_params", OwnerRequest::ListParams)
    }

    /// DEV, as the live `plugin_save_state`: production save/recall is not built.
    #[cfg(debug_assertions)]
    pub(crate) fn save_state(&self) -> Result<Vec<u8>, String> {
        self.ask("save_state", OwnerRequest::SaveState)
    }

    /// DEV, as the live `plugin_load_state` (and with its residual: a restore that lands after the
    /// 5 s wait still applies).
    #[cfg(debug_assertions)]
    pub(crate) fn load_state(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.ask("load_state", |reply| OwnerRequest::LoadState(bytes, reply))
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

/// Test-only: the engine-mode tests run one at a time. Each builds whole engines (every page
/// touched) and a device thread; many at once starved the live restart tests' hop-1 reader on a
/// loaded machine.
#[cfg(test)]
pub(super) fn one_engine_test_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test-only: the allocations `f` makes on this thread under the RT alloc guard. The counter is
/// process-wide and a live producer test resets it after its warmup, so a window it went backwards
/// in is measured again.
#[cfg(all(test, debug_assertions))]
pub(super) fn rt_allocations(mut f: impl FnMut()) -> u64 {
    use super::super::rt_alloc::{guard, RT_ALLOCS};
    loop {
        let before = RT_ALLOCS.load(Relaxed);
        {
            let _guard = guard();
            f();
        }
        let after = RT_ALLOCS.load(Relaxed);
        if after >= before {
            return after - before;
        }
    }
}
