//! OWNS: a VST3 plugin in the engine: `Vst3Unit` (the processor with its event and parameter lists,
//! as an `lf_engine::SlotProcessor`) and the engine-mode owner thread that loads, activates,
//! restarts, re-activates after an eviction and tears it down (`engine_slot` holds the API), keeping
//! the plugin's tone as `engine_slot` describes: the component's and the edit controller's states in
//! one container (`save_state`, `restore_state`). It followed the WebView bridge's VST3 owner (deleted in
//! Stage 6) minus its RT thread, its hop-1 ring and the device; the Stage 1 spike's `PluginUnit` was
//! the unit's prototype. Unlike that owner's, this load creates the
//! edit controller and sets its component handler before the component activates, the SDK host's
//! order, so a stored tone reaches both halves before anything processes; a component that refuses the
//! tone is torn down and created again from the same module (`load`).

use super::super::engine_slot::{
    keep_tone, report_faults, restore_tone, EngineSlotEvent, OwnerCtx, Ready, Restore, FAULT_PROCESS,
    FAULT_START, OWNER_POLL, REMOVE_TIMEOUT,
};
use super::super::super::state::ToneRestore;
use super::super::super::tone::{self, ToneKeeper};
use super::*;
use vst3::Steinberg::kNotImplemented;

use lf_engine::grid::Frame;
use lf_engine::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

use crate::engine_io::SlotHost;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// One activated VST3 processor with its event and parameter lists and pre-sized rows, built on
/// the owner thread.
pub(super) struct Vst3Unit {
    processor: ComPtr<IAudioProcessor>,
    kind: SlotKind,
    latency: Frame,
    max_frames: usize,
    params: Consumer<PluginEvent>,
    events: Rc<EventListInner>,
    _event_list: ComWrapper<RtEventList>,
    event_list_ptr: ComPtr<IEventList>,
    changes: Rc<ParamChangesInner>,
    _param_changes: ComWrapper<RtParamChanges>,
    param_changes_ptr: ComPtr<IParameterChanges>,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    /// `setProcessing(1)` succeeded and `setProcessing(0)` has not run since.
    processing: bool,
    /// `setProcessing(1)` failed: silent until the owner re-activates and reinstalls it.
    refused: bool,
    faults: Arc<AtomicU32>,
}

// SAFETY: one thread at a time holds the unit. The owner builds it and hands it over; the engine
// touches it only under its lock (the device callback, or whoever holds the engine while no device
// runs); it comes back through the slot's port before the owner touches it again. The `Rc`s and
// the row pointers point only into the unit itself, and the plugin sees the lists only during a
// `process` call.
unsafe impl Send for Vst3Unit {}

impl Vst3Unit {
    pub(super) fn new(
        processor: ComPtr<IAudioProcessor>,
        activation: Activation,
        max_frames: u32,
        params: Consumer<PluginEvent>,
        faults: Arc<AtomicU32>,
    ) -> Result<Box<Vst3Unit>, String> {
        let events = Rc::new(EventListInner::new());
        let event_list = ComWrapper::new(RtEventList { inner: events.clone() });
        let event_list_ptr = event_list.to_com_ptr::<IEventList>().ok_or("event list COM failed")?;
        let changes = Rc::new(ParamChangesInner::new(MAX_PARAM_QUEUES).ok_or("param queues failed")?);
        let param_changes = ComWrapper::new(RtParamChanges { inner: changes.clone() });
        let param_changes_ptr = param_changes
            .to_com_ptr::<IParameterChanges>()
            .ok_or("param changes COM failed")?;
        let mut unit = Box::new(Vst3Unit {
            processor,
            kind: SlotKind::Instrument,
            latency: 0,
            max_frames: 0,
            params,
            events,
            _event_list: event_list,
            event_list_ptr,
            changes,
            _param_changes: param_changes,
            param_changes_ptr,
            in_bufs: Vec::new(),
            out_bufs: Vec::new(),
            in_ptrs: Vec::new(),
            out_ptrs: Vec::new(),
            processing: false,
            refused: false,
            faults,
        });
        unit.rearm(activation, max_frames);
        Ok(unit)
    }

    /// Owner thread: size the rows to what an activation negotiated (a `kIoChanged` restart can
    /// change the channel counts, a device change the block).
    fn rearm(&mut self, activation: Activation, max_frames: u32) {
        self.kind = if activation.in_channels > 0 { SlotKind::Effect } else { SlotKind::Instrument };
        self.latency = activation.latency_frames as Frame;
        self.max_frames = (max_frames as usize).max(1);
        self.in_bufs = vec![vec![0.0; self.max_frames]; activation.in_channels as usize];
        self.out_bufs = vec![vec![0.0; self.max_frames]; activation.out_channels.max(1) as usize];
        self.in_ptrs = self.in_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        self.out_ptrs = self.out_bufs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        self.refused = false;
    }
}

impl SlotProcessor for Vst3Unit {
    fn kind(&self) -> SlotKind {
        self.kind
    }

    fn latency(&self) -> Frame {
        self.latency
    }

    fn process(&mut self, _frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]) {
        out.fill(0.0);
        if !self.processing {
            if self.refused {
                return;
            }
            // SAFETY: the processor is active (the owner activated it before the install);
            // setProcessing runs on the thread that processes, as the Stage 1 spike does.
            if unsafe { self.processor.setProcessing(1) } != kResultOk {
                self.refused = true;
                self.faults.fetch_or(FAULT_START, Relaxed);
                return;
            }
            self.processing = true;
        }
        let chans = self.out_bufs.len();
        let n = out.len();
        let (mut at, mut next) = (0, 0);
        // A call longer than the plugin's max frames goes in slices; each note lands in its own.
        while at < n {
            let len = (n - at).min(self.max_frames);
            self.events.clear();
            self.changes.clear();
            if at == 0 {
                // Params from the ring apply at the block start; more than the cap, or a new id
                // past the queue pool, wait in the ring.
                drain_params(&mut self.params, &self.changes, MAX_EVENTS_PER_BLOCK);
            }
            while let Some(e) = events.get(next).filter(|e| (e.offset as usize) < at + len) {
                let note = match e.kind {
                    SlotEventKind::NoteOn { key, velocity } => {
                        PluginEvent::NoteOn { key: key as u16, velocity: velocity as f64 }
                    }
                    SlotEventKind::NoteOff { key } => PluginEvent::NoteOff { key: key as u16 },
                };
                if let Some(mut event) = plugin_event_to_vst3(note) {
                    event.sampleOffset = (e.offset as usize - at) as i32;
                    self.events.push(event);
                }
                next += 1;
            }
            for ch in self.in_bufs.iter_mut() {
                ch[..len].copy_from_slice(&input[at..at + len]);
            }
            // A plugin that reports silence may leave its outputs untouched.
            for ch in self.out_bufs.iter_mut() {
                ch[..len].fill(0.0);
            }
            // SAFETY: ProcessData points at the unit's rows (the channel counts the activation
            // negotiated, `len` ≤ max_frames frames each) and its host lists, all valid for the
            // synchronous call. No input bus (an instrument) is the canonical numInputs=0 layout.
            let status = unsafe {
                let mut out_bus = AudioBusBuffers {
                    numChannels: chans as i32,
                    silenceFlags: 0,
                    __field0: AudioBusBuffers__type0 { channelBuffers32: self.out_ptrs.as_mut_ptr() },
                };
                let mut in_bus = AudioBusBuffers {
                    numChannels: self.in_bufs.len() as i32,
                    silenceFlags: 0,
                    __field0: AudioBusBuffers__type0 { channelBuffers32: self.in_ptrs.as_mut_ptr() },
                };
                let has_input = !self.in_bufs.is_empty();
                let mut pd: ProcessData = std::mem::zeroed();
                pd.processMode = ProcessModes_::kRealtime as i32;
                pd.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as i32;
                pd.numSamples = len as i32;
                pd.numInputs = has_input as i32;
                pd.numOutputs = 1;
                pd.inputs = if has_input { &mut in_bus } else { std::ptr::null_mut() };
                pd.outputs = &mut out_bus;
                pd.inputEvents = self.event_list_ptr.as_ptr();
                pd.inputParameterChanges = self.param_changes_ptr.as_ptr();
                self.processor.process(&mut pd)
            };
            if status != kResultOk {
                self.faults.fetch_or(FAULT_PROCESS, Relaxed);
            }
            sum_to_mono(&self.out_bufs, &mut out[at..at + len], len, chans);
            at += len;
        }
    }

    fn stop(&mut self) {
        if self.processing {
            // SAFETY: pairs the successful setProcessing(1), on the thread that last processed.
            unsafe {
                let _ = self.processor.setProcessing(0);
            }
            self.processing = false;
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A unit the engine handed back: a `SlotHost` hands back only what its own handle installed. The
/// engine stops a unit before it leaves; stopping again is a no-op, and covers one that did not.
fn own(unit: Box<dyn SlotProcessor>) -> Box<Vst3Unit> {
    let mut unit = unit
        .into_any()
        .downcast::<Vst3Unit>()
        .unwrap_or_else(|_| panic!("an engine slot hands back the unit its owner installed"));
    unit.stop();
    unit
}

/// Everything the owner holds of a loaded plugin besides the unit. Dropping it tears down in the
/// live order: the separated controller disconnects and terminates, the component deactivates and
/// terminates, then the fields drop in declaration order (the COM objects, the module last). The
/// unit, which holds the processor, must be gone first.
struct Vst3Plugin {
    component: ComPtr<IComponent>,
    controller: Option<ComPtr<IEditController>>,
    separated: bool,
    host_ctx: ComPtr<FUnknown>,
    _hostapp: ComWrapper<LfHostApp>,
    factory: ComPtr<IPluginFactory>,
    /// `None` for an in-process plugin (the fixtures).
    _module: Option<Vst3Module>,
    /// The component is active (between a successful `activate_component` and `setActive(0)`).
    active: bool,
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // SAFETY: owner thread; the unit is gone and no editor view is open, so nothing processes.
        // Disconnect before terminating so neither side notifies a half-torn-down peer.
        unsafe {
            if let Some(ctl) = self.controller.take() {
                if self.separated {
                    if let (Some(comp_cp), Some(ctrl_cp)) =
                        (self.component.cast::<IConnectionPoint>(), ctl.cast::<IConnectionPoint>())
                    {
                        let _ = comp_cp.disconnect(ctrl_cp.as_ptr());
                        let _ = ctrl_cp.disconnect(comp_cp.as_ptr());
                    }
                    let _ = ctl.terminate();
                }
            }
            // VST3 call sequence: never deactivate an inactive component (after a failed restart).
            if self.active {
                let _ = self.component.setActive(0);
                self.active = false;
            }
            let _ = self.component.terminate();
        }
    }
}

impl Vst3Plugin {
    /// Owner thread, nothing processing: deactivate, then run the one activation sequence at the
    /// engine's current rate with its largest block as the max. Returns what it negotiated, its max
    /// block and the rate (`SlotHost::install` checks it).
    fn activate(
        &mut self,
        processor: &ComPtr<IAudioProcessor>,
        slot: &SlotHost,
    ) -> Result<(Activation, u32, u32), String> {
        if self.active {
            // SAFETY: owner thread; the unit is out of the engine and stopped.
            unsafe {
                let _ = self.component.setActive(0);
            }
            self.active = false;
        }
        let rate = slot.rate().ok_or_else(|| "no audio device is open".to_string())?;
        let max_frames = slot.max_block().max(1) as u32;
        // SAFETY: owner thread; the component is initialised and inactive, and nothing processes.
        let activation = unsafe { activate_component(&self.component, processor, rate as f64, max_frames) }?;
        self.active = true;
        Ok((activation, max_frames, rate))
    }
}

/// A fresh `MemStream` and the `IBStream` pointer a plugin call takes.
fn stream(over: &[u8]) -> Result<(ComWrapper<MemStream>, ComPtr<IBStream>), String> {
    let stream = ComWrapper::new(MemStream::reading(over));
    let ptr = stream.to_com_ptr::<IBStream>().ok_or("stream COM failed")?;
    Ok((stream, ptr))
}

/// The plugin's state, for its tone: the component's (`IComponent::getState`, which must succeed) and
/// the edit controller's (`IEditController::getState`; one that refuses or keeps nothing adds an empty
/// state), in one container (`tone::encode_vst3`). Owner thread.
fn save_state(plugin: &Vst3Plugin) -> Result<Option<Vec<u8>>, String> {
    let (component, ptr) = stream(&[])?;
    // SAFETY: owner thread; the component is initialised and the stream outlives the call.
    let r = unsafe { plugin.component.getState(ptr.as_ptr()) };
    if r != kResultOk {
        return Err(format!("IComponent::getState → {r:#x}"));
    }
    let controller = match plugin.controller.as_ref() {
        Some(ctl) => {
            let (state, ptr) = stream(&[])?;
            // SAFETY: owner thread; live controller; the stream outlives the call.
            match unsafe { ctl.getState(ptr.as_ptr()) } {
                r if r == kResultOk => state.bytes(),
                _ => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    Ok(Some(tone::encode_vst3(&component.bytes(), &controller)))
}

/// Restore a tone's state, in the SDK host's order: `IComponent::setState`, then the edit controller's
/// `setComponentState` with the component's state (a stream of its own, from the start) and its own
/// `setState`. The component refusing is the restore failing; a controller that refuses only leaves
/// the editor's knobs behind the sound, which is logged. Owner thread, before the component activates.
fn restore_state(plugin: &Vst3Plugin, state: &[u8], slot: usize) -> Result<(), String> {
    let (component, controller) = tone::decode_vst3(state)?;
    let (_keep, ptr) = stream(component)?;
    // SAFETY: owner thread; the component is initialised and inactive; the stream outlives the call.
    let r = unsafe { plugin.component.setState(ptr.as_ptr()) };
    if r != kResultOk {
        return Err(format!("the component refused its saved state ({r:#x})"));
    }
    let Some(ctl) = plugin.controller.as_ref() else { return Ok(()) };
    let (_keep, ptr) = stream(component)?;
    // SAFETY: owner thread; live controller; the stream outlives the call.
    let r = unsafe { ctl.setComponentState(ptr.as_ptr()) };
    if r != kResultOk && r != kNotImplemented {
        log::warn!("[plugin_host] engine slot {slot}: the edit controller refused the component state ({r:#x}); its editor may show stale values");
    }
    if !controller.is_empty() {
        let (_keep, ptr) = stream(controller)?;
        // SAFETY: as above.
        let r = unsafe { ctl.setState(ptr.as_ptr()) };
        if r != kResultOk && r != kNotImplemented {
            log::warn!("[plugin_host] engine slot {slot}: the edit controller refused its saved state ({r:#x})");
        }
    }
    Ok(())
}

/// Re-activate the unit at the engine's current terms and install it. A restart report the plugin
/// raises from inside the cycle (a `kLatencyChanged` from `setActive(1)`) describes the state just
/// activated and is consumed, as the live cycle does. On failure the unit comes back to the caller.
fn reinstall(
    plugin: &mut Vst3Plugin,
    mut unit: Box<Vst3Unit>,
    slot: &SlotHost,
    restart: &RestartFlags,
) -> Result<(), (Box<Vst3Unit>, String)> {
    let activated = plugin.activate(&unit.processor, slot);
    let raised = restart.take();
    if raised != 0 {
        log::info!(
            "[plugin_host] engine slot {} restartComponent({}) raised during the cycle describes the state just activated; consumed",
            slot.slot(),
            restart_flag_names(raised)
        );
    }
    let rate = match activated {
        Ok((activation, max_frames, rate)) => {
            unit.rearm(activation, max_frames);
            rate
        }
        Err(e) => return Err((unit, e)),
    };
    slot.install(unit, rate).map_err(|(unit, e)| (own(unit), e))
}

/// A restart or an eviction: get the unit back (it came from the engine, the owner still holds it
/// from a failed cycle, or the engine hands it back now) and reinstall it.
fn cycle(
    plugin: &mut Vst3Plugin,
    slot: &SlotHost,
    restart: &RestartFlags,
    parked: &mut Option<Box<Vst3Unit>>,
    back: Option<Box<Vst3Unit>>,
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
                log::error!("[plugin_host] engine slot {index} {why}: {e}; it keeps playing un-restarted");
                return;
            }
        },
    };
    match reinstall(plugin, unit, slot, restart) {
        Ok(()) => log::info!("[plugin_host] engine slot {index} {why}: setActive(0) → activate → reinstalled"),
        Err((unit, e)) => {
            log::error!("[plugin_host] engine slot {index} {why} failed, the slot stays bypassed: {e}");
            *parked = Some(unit);
        }
    }
}

/// The module and factory an owner loads from.
pub(super) type Opened = (Option<Vst3Module>, ComPtr<IPluginFactory>);

/// The production owner: load the bundle at `path`.
pub(in super::super) fn run(ctx: OwnerCtx, path: String, id: &str) -> Result<(), String> {
    run_with(ctx, id, move || {
        let binary = super::super::super::scan::resolve_vst3_binary(std::path::Path::new(&path))
            .ok_or_else(|| format!("no loadable VST3 binary inside {path}"))?;
        let wide: Vec<u16> = binary.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: FFI module load and its standard factory entry; the out-of-process scan vetted the
        // bundle. The module guard owns the library at once, so every later error unloads it.
        unsafe {
            let module = Vst3Module::load(PCWSTR(wide.as_ptr()))?;
            let gpf = GetProcAddress(module.handle(), s!("GetPluginFactory"))
                .ok_or_else(|| "GetPluginFactory not exported".to_string())?;
            let get_factory: unsafe extern "system" fn() -> *mut IPluginFactory = std::mem::transmute(gpf);
            let factory =
                ComPtr::from_raw(get_factory()).ok_or_else(|| "GetPluginFactory returned null".to_string())?;
            Ok((Some(module), factory))
        }
    })
}

/// What a load hands the owner: the plugin, its unit, its name, the rate it activated at and what it
/// did with the stored tone.
type Loaded = (Vst3Plugin, Box<Vst3Unit>, String, u32, Option<ToneRestore>);

/// Create class `target` (`id`) from the factory, initialise it, create its edit controller and give it
/// the load's component `handler`, with the controller before the activation. The plugin takes the
/// module and the factory; every error after the component exists tears it down (`Vst3Plugin`), and
/// one before it drops the factory, then the module.
fn create(
    opened: Opened,
    target: &TUID,
    id: &str,
    handler: &ComWrapper<LfComponentHandler>,
) -> Result<(Vst3Plugin, ComPtr<IAudioProcessor>, String), String> {
    let (module, factory) = opened;
    // SAFETY: raw FUnknown COM on the owner thread, the VST3 load sequence; every pointer is
    // valid for its call.
    unsafe {
        let mut component: Option<ComPtr<IComponent>> = None;
        let mut name = id.to_string();
        for i in 0..factory.countClasses() {
            let mut info: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(i, &mut info) != kResultOk || info.cid != *target {
                continue;
            }
            name = super::super::super::scan::c_chars_to_string(&info.name);
            let mut obj: *mut c_void = std::ptr::null_mut();
            let iid = vst3::Steinberg::Vst::IComponent_iid;
            if factory.createInstance(info.cid.as_ptr(), iid.as_ptr(), &mut obj) == kResultOk && !obj.is_null() {
                component = ComPtr::from_raw(obj as *mut IComponent);
            }
            break;
        }
        let component = component.ok_or_else(|| format!("class {id} not found / createInstance failed"))?;
        let hostapp = ComWrapper::new(LfHostApp);
        let host_ctx = hostapp.to_com_ptr::<FUnknown>().ok_or_else(|| "host app FUnknown failed".to_string())?;
        // From here every early return drops `plugin`: setActive(0) → terminate → release → module.
        let mut plugin = Vst3Plugin {
            component,
            controller: None,
            separated: false,
            host_ctx,
            _hostapp: hostapp,
            factory,
            _module: module,
            active: false,
        };
        if plugin.component.initialize(plugin.host_ctx.as_ptr()) != kResultOk {
            return Err("component.initialize failed".to_string());
        }
        let processor = plugin
            .component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| "plugin has no IAudioProcessor".to_string())?;
        let (controller, separated) = obtain_controller(&plugin.factory, &plugin.component, &plugin.host_ctx);
        plugin.controller = controller;
        plugin.separated = separated;
        // The one component handler of this load, set before anything can report through it and kept
        // until the plugin is torn down (editors use it too).
        if let (Some(ctl), Some(hp)) = (plugin.controller.as_ref(), handler.to_com_ptr::<IComponentHandler>()) {
            ctl.setComponentHandler(hp.as_ptr());
        }
        Ok((plugin, processor, name))
    }
}

/// Create the plugin (`create`), restore the stored tone, activate, and build its unit. A component
/// that refuses its tone may have taken part of it: it is torn down and created again, without a tone,
/// from the same module, so the plugin runs at the defaults the player is told it loaded with. The
/// handler outlives the plugin (the caller's).
fn load(
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
    slot: &SlotHost,
    params: Consumer<PluginEvent>,
    faults: Arc<AtomicU32>,
    handler: &ComWrapper<LfComponentHandler>,
    restart: &RestartFlags,
    tone: &mut ToneKeeper,
) -> Result<Loaded, String> {
    let target = super::super::super::scan::hex_to_tuid(id).ok_or_else(|| format!("bad VST3 class id: {id}"))?;
    let (mut plugin, mut processor, name) = create(open()?, &target, id, handler)?;
    // The stored tone goes in before the component activates: nothing processes it yet.
    let restored = restore_tone(tone, slot.slot(), &name, |state| restore_state(&plugin, state, slot.slot()));
    if restored == Restore::Refused {
        // The module stays loaded across the swap: the plugin gives it up before it is torn down.
        let opened = (plugin._module.take(), plugin.factory.clone());
        drop(processor);
        drop(plugin);
        (plugin, processor, _) = create(opened, &target, id, handler)?;
    }
    let (activation, max_frames, rate) = plugin.activate(&processor, slot)?;
    // A restart or a re-list the restore or the activation raised describes the state just
    // activated, as a cycle's does (`reinstall`); the param ids are listed after the load anyway.
    let _ = (restart.take(), restart.take_notify());
    let unit = Vst3Unit::new(processor, activation, max_frames, params, faults)?;
    Ok((plugin, unit, name, rate, restored.report()))
}

/// The engine-mode VST3 owner. `open` yields the module and factory (a bundle in production, an
/// in-process factory in the tests); returns the teardown's result.
pub(super) fn run_with(
    ctx: OwnerCtx,
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
) -> Result<(), String> {
    let OwnerCtx { slot, running, requests, params, params_tx, param_ids, sink, editor_parent, ready, mut tone } = ctx;
    let index = slot.slot();
    let faults = Arc::new(AtomicU32::new(0));
    // The load's component handler: an editor's `performEdit` reaches the processor, the caller and
    // the tone (a change to save).
    let restart = Arc::new(RestartFlags::default());
    let edits = sink.clone();
    let edited = tone.edited();
    let handler = ComWrapper::new(LfComponentHandler {
        event_tx: params_tx,
        param_changed: Box::new(move |id, value| {
            edited.store(true, Release);
            edits(EngineSlotEvent::ParamChanged { id, value })
        }),
        restart: restart.clone(),
    });
    let loaded = load(id, open, &slot, params, faults.clone(), &handler, &restart, &mut tone);
    let (mut plugin, unit, name, rate, restored) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    // Before the load is reported, so the caller's first `set_param` finds its ids.
    publish_param_ids(&param_ids, &list_vst3_params(&plugin.controller).unwrap_or_default());
    let kind = unit.kind;
    // Not installed: the unit, then the plugin, go before the handler the controller holds.
    if !running.load(Acquire) {
        drop((unit, plugin));
        let _ = ready.send(Err("plugin load cancelled".to_string()));
        return Ok(());
    }
    if let Err((unit, e)) = slot.install(unit, rate) {
        drop((unit, plugin));
        let _ = ready.send(Err(e));
        return Ok(());
    }
    if ready.send(Ok(Ready { name: name.clone(), kind, tone: restored })).is_err() {
        log::warn!("[plugin_host] engine slot {index}: the load finished after its caller gave up; undoing it");
        return teardown(&slot, plugin, None, handler);
    }

    // The unit while it is out of the engine (a failed restart); `None` while the engine holds it.
    let mut parked: Option<Box<Vst3Unit>> = None;
    let mut editor = Vst3Editor::Closed;
    let mut reported = 0u32;
    // A panic in the loop must not unwind past a unit the engine still runs (dropping `plugin` would
    // deactivate and unload code the audio thread is inside): it is caught here, and the ordered
    // teardown below takes the unit out of the engine first.
    let served = catch_unwind(AssertUnwindSafe(|| {
    while running.load(Acquire) {
        // restartComponent only raised flags (maybe on a foreign thread); the cycle runs here.
        let flags = restart.take();
        if flags != 0 {
            let why = format!("restartComponent({})", restart_flag_names(flags));
            cycle(&mut plugin, &slot, &restart, &mut parked, None, &why);
        }
        // After the cycle, so a re-list the plugin raised during its re-activation lands this turn.
        if restart.take_notify() & RestartFlags::RELIST != 0 {
            publish_param_ids(&param_ids, &list_vst3_params(&plugin.controller).unwrap_or_default());
            sink(EngineSlotEvent::ParamsChanged);
            tone.note_change(Instant::now());
        }
        // A new engine at another rate evicted the unit; it comes back stopped.
        if let Some(unit) = slot.take_evicted() {
            cycle(&mut plugin, &slot, &restart, &mut parked, Some(own(unit)), "re-activation after a device change");
        }
        // Every turn, editor or not: a JUCE plugin (Neural DSP) runs its message thread here, and its
        // timers and async updates must not wait for the next editor, or its state lags what it plays
        // and a tone saved meanwhile is stale. The hosted editor's window needs it too, and reports its
        // close box here.
        pump_thread_messages();
        if matches!(editor, Vst3Editor::Open { .. }) {
            if matches!(&editor, Vst3Editor::Open { win, .. } if win.close_requested()) {
                vst3_editor_close(&mut editor);
                sink(EngineSlotEvent::EditorClosed);
                let _ = keep_tone(&mut tone, index, "editor closed", &name, || save_state(&plugin));
            }
        }
        let request = if matches!(editor, Vst3Editor::Open { .. }) {
            wait_for_input(20);
            requests.try_recv().ok()
        } else {
            match requests.recv_timeout(OWNER_POLL) {
                Ok(request) => Some(request),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                // The handle is gone without an unload; `running` is false by then.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        if let Some(request) = request.and_then(|r| super::super::take_uncancelled(r, index as u8)) {
            match request {
                OwnerRequest::OpenEditor(cancelled, reply) => {
                    let was_closed = matches!(editor, Vst3Editor::Closed);
                    let res = if was_closed {
                        vst3_editor_open(&plugin.controller, editor_parent, index as u8).map(|opened| editor = opened)
                    } else {
                        Ok(())
                    };
                    // Opened after the caller's 5 s: it reported failure, so close what this opened.
                    if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                        vst3_editor_close(&mut editor);
                    }
                    let _ = reply.send(res);
                }
                OwnerRequest::CloseEditor(_, reply) => {
                    if !matches!(editor, Vst3Editor::Closed) {
                        vst3_editor_close(&mut editor);
                        let _ = keep_tone(&mut tone, index, "editor closed", &name, || save_state(&plugin));
                    }
                    let _ = reply.send(Ok(()));
                }
                OwnerRequest::SaveTone(reply) => {
                    let _ = reply.send(keep_tone(&mut tone, index, "asked", &name, || save_state(&plugin)));
                }
                OwnerRequest::ListParams(reply) => {
                    let res = list_vst3_params(&plugin.controller);
                    if let Ok(listed) = &res {
                        publish_param_ids(&param_ids, listed);
                    }
                    let _ = reply.send(res);
                }
                // The processor got the value through the ring; the controller (what the editor
                // shows, and what decides a controller-side restart) is told here.
                OwnerRequest::SetParamNormalized(id, value) => {
                    if let Some(ctl) = plugin.controller.as_ref() {
                        // SAFETY: owner thread, live controller.
                        let r = unsafe { ctl.setParamNormalized(id as ParamID, value) };
                        if r != kResultOk {
                            log::debug!("[plugin_host] engine slot {index} controller.setParamNormalized({id}) → {r:#x}");
                        }
                    }
                }
                OwnerRequest::Wake => {}
            }
        }
        if tone.poll(Instant::now()) {
            let _ = keep_tone(&mut tone, index, "changes went quiet", &name, || save_state(&plugin));
        }
        report_faults(&faults, index, &mut reported);
    }
    }));
    if served.is_err() {
        log::error!("[plugin_host] engine slot {index}: the VST3 owner panicked; tearing the plugin down");
    }
    let editor_open = !matches!(editor, Vst3Editor::Closed);
    if editor_open {
        vst3_editor_close(&mut editor);
    }
    // Before the teardown: a change not saved yet, and what an open editor may have changed unseen. Not
    // after a panic, which may have left the plugin half way through something.
    if served.is_ok() && (editor_open || tone.dirty()) {
        let _ = keep_tone(&mut tone, index, "unload", &name, || save_state(&plugin));
    }
    let result = teardown(&slot, plugin, parked, handler);
    report_faults(&faults, index, &mut reported);
    result
}

/// The ordered teardown: the unit leaves the engine (`setProcessing(0)` on the audio thread) and
/// releases the processor → the controller terminates → `setActive(0)` → `terminate` → the COM
/// objects release → the module unloads last. A unit the engine does not hand back keeps the plugin
/// loaded (leaked) rather than unloading code a processor still runs.
fn teardown(
    slot: &SlotHost,
    plugin: Vst3Plugin,
    parked: Option<Box<Vst3Unit>>,
    handler: ComWrapper<LfComponentHandler>,
) -> Result<(), String> {
    let index = slot.slot();
    let t = Instant::now();
    let unit = match parked {
        Some(unit) => Some(unit),
        None => match slot.remove(REMOVE_TIMEOUT) {
            Ok(unit) => unit.map(own),
            Err(e) => {
                log::error!("[plugin_host] engine slot {index} VST3 teardown: {e}; leaving the plugin loaded");
                slot.abandon();
                std::mem::forget(plugin);
                std::mem::forget(handler);
                return Err(e);
            }
        },
    };
    let remove_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(unit);
    drop(plugin);
    drop(handler);
    log::info!(
        "[plugin_host] engine slot {index} VST3 teardown: remove={remove_ms} release+deactivate+terminate+module={} ms",
        t.elapsed().as_millis()
    );
    Ok(())
}
