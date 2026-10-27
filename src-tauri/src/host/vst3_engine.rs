//! OWNS: a VST3 plugin in the engine: `Vst3Unit` (the processor with its event and parameter lists,
//! as an `lf_engine::SlotProcessor`) and what its owner thread does with the plugin (`Vst3`, which
//! the shared owner loop `engine_slot::serve` drives): the load, activation, a restart and a
//! re-activation after an eviction, and the teardown's order, keeping the plugin's tone as `engine_slot` describes: the component's and the edit controller's states in
//! one container (`save_state`, `restore_state`). It followed the WebView bridge's VST3 owner (deleted in
//! Stage 6) minus its RT thread, its hop-1 ring and the device; the Stage 1 spike's `PluginUnit` was
//! the unit's prototype. Unlike that owner's, this load creates the
//! edit controller and sets its component handler before the component activates, the SDK host's
//! order, so a stored tone reaches both halves before anything processes; a component that refuses the
//! tone is torn down and created again from the same module (`load`).

use super::super::engine_slot::{
    own, restore_tone, serve, Editor, EngineSlotEvent, EventSink, Loaded, OwnedPlugin, OwnerCtx, Reported,
    Restore, Setup, FAULT_PROCESS, FAULT_START,
};
use super::super::super::tone;
use super::*;
use vst3::Steinberg::kNotImplemented;

use lf_engine::grid::Frame;
use lf_engine::{SlotEvent, SlotKind, SlotProcessor};

use crate::engine_io::SlotHost;

/// One activated VST3 processor with its event and parameter lists and pre-sized rows, built on
/// the owner thread.
pub(super) struct Vst3Unit {
    processor: ComPtr<IAudioProcessor>,
    kind: SlotKind,
    latency: Frame,
    max_frames: usize,
    params: Consumer<ParamEvent>,
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
    /// `setProcessing(1)` was accepted (`kResultOk` or `kNotImplemented`) and `setProcessing(0)` has
    /// not run since.
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
        params: Consumer<ParamEvent>,
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
            // setProcessing runs on the thread that processes, as the Stage 1 spike does. It is an
            // optional notification: the SDK's AudioEffect answers kNotImplemented and processes.
            let started = unsafe { self.processor.setProcessing(1) };
            if started != kResultOk && started != kNotImplemented {
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
                self.events.push(note_to_vst3(e.kind, (e.offset as usize - at) as i32));
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
                // A failed call's output is discarded, as CLAP's: the slice stays silent.
                self.faults.fetch_or(FAULT_PROCESS, Relaxed);
            } else {
                sum_to_mono(&self.out_bufs, &mut out[at..at + len], len, chans);
            }
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

    fn start_refused(&self) -> bool {
        self.refused
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
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

/// Tell a separated edit controller the component's state, as the SDK host does once it connects the
/// halves: without it the editor and the listed values start at the controller's own defaults, not at
/// what the processor plays. A restored tone already did it (`restore_state`); a single component is
/// its own controller. A plugin that refuses only leaves its editor behind the sound, which is logged.
/// Owner thread, before the component activates.
fn sync_controller(plugin: &Vst3Plugin, slot: usize) -> Result<(), String> {
    let Some(ctl) = plugin.controller.as_ref().filter(|_| plugin.separated) else { return Ok(()) };
    let (state, ptr) = stream(&[])?;
    // SAFETY: owner thread; the component is initialised and inactive; the stream outlives the call.
    let r = unsafe { plugin.component.getState(ptr.as_ptr()) };
    if r != kResultOk {
        log::warn!("[plugin_host] engine slot {slot}: IComponent::getState → {r:#x}; the edit controller keeps its own defaults");
        return Ok(());
    }
    let (_keep, ptr) = stream(&state.bytes())?;
    // SAFETY: owner thread; live controller; the stream outlives the call.
    let r = unsafe { ctl.setComponentState(ptr.as_ptr()) };
    if r != kResultOk && r != kNotImplemented {
        log::warn!("[plugin_host] engine slot {slot}: the edit controller refused the component state ({r:#x}); its editor may show stale values");
    }
    Ok(())
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

/// Create class `target` (`id`) from the factory, initialise it, create its edit controller and give it
/// the load's component `handler`, with the controller before the activation. The plugin takes the
/// module and the factory; every error after the component initialised tears it down (`Vst3Plugin`),
/// and one before it releases what exists, then the factory, then the module.
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
        // IPluginBase: a component whose initialize failed is released, never terminated. The locals
        // drop in reverse order, the component before the factory and the module.
        if component.initialize(host_ctx.as_ptr()) != kResultOk {
            return Err("component.initialize failed".to_string());
        }
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

/// Create the plugin (`create`) with the load's component handler, restore the stored tone, activate,
/// and build its unit. A component that refuses its tone may have taken part of it: it is torn down
/// and created again, without a tone, from the same module, so the plugin runs at the defaults the
/// player is told it loaded with. A controller no tone reached learns the component's state
/// (`sync_controller`).
fn load(
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
    Setup { slot, params, params_tx, sink, faults, tone, .. }: Setup<'_>,
) -> Result<Loaded<Vst3>, String> {
    // The load's component handler: an editor's `performEdit` reaches the processor, the caller and
    // the tone (a change to save). It outlives the plugin, whose controller holds it.
    let restart = Arc::new(RestartFlags::default());
    let edits = sink.clone();
    let handler = ComWrapper::new(LfComponentHandler {
        event_tx: params_tx,
        edited: tone.edited(),
        param_changed: Box::new(move |id, value| edits(EngineSlotEvent::ParamChanged { id, value })),
        restart: restart.clone(),
    });
    let target = super::super::super::scan::hex_to_tuid(id).ok_or_else(|| format!("bad VST3 class id: {id}"))?;
    let (mut plugin, mut processor, name) = create(open()?, &target, id, &handler)?;
    // The stored tone goes in before the component activates: nothing processes it yet.
    let restored = restore_tone(tone, slot.slot(), &name, |state| restore_state(&plugin, state, slot.slot()));
    if restored == Restore::Refused {
        // The module stays loaded across the swap: the plugin gives it up before it is torn down.
        let opened = (plugin._module.take(), plugin.factory.clone());
        drop(processor);
        drop(plugin);
        (plugin, processor, _) = create(opened, &target, id, &handler)?;
    }
    if restored != Restore::Restored {
        sync_controller(&plugin, slot.slot())?;
    }
    let (activation, max_frames, rate) = plugin.activate(&processor, slot)?;
    // A restart or a re-list the restore or the activation raised describes the state just
    // activated, as a cycle's does (`reinstall`); the param ids are listed after the load anyway.
    let _ = (restart.take(), restart.take_notify());
    let unit = Vst3Unit::new(processor, activation, max_frames, params, faults)?;
    let plugin = Vst3 { editor: Vst3Editor::Closed, plugin, handler, restart, slot: slot.slot() as u8 };
    Ok(Loaded { plugin, unit, name, rate, tone: restored.report() })
}

/// The engine-mode VST3 owner (`engine_slot::serve`). `open` yields the module and factory (a bundle
/// in production, an in-process factory in the tests); returns the teardown's result.
pub(super) fn run_with(
    ctx: OwnerCtx,
    id: &str,
    open: impl FnOnce() -> Result<Opened, String>,
) -> Result<(), String> {
    serve(ctx, |setup| load(id, open, setup))
}

/// A loaded VST3 plugin as its owner holds it. Torn down in the live order (`release`): the editor,
/// the unit (it releases the processor), the plugin (its `Drop`, the module last), then the handler
/// its controller held.
struct Vst3 {
    editor: Vst3Editor,
    plugin: Vst3Plugin,
    handler: ComWrapper<LfComponentHandler>,
    /// What the plugin's `restartComponent` raised (maybe on a foreign thread), for the owner's turn.
    restart: Arc<RestartFlags>,
    slot: u8,
}

impl OwnedPlugin for Vst3 {
    type Unit = Vst3Unit;
    const FORMAT: &'static str = "VST3";

    fn take_restart(&mut self) -> Option<String> {
        let flags = self.restart.take();
        (flags != 0).then(|| format!("restartComponent({})", restart_flag_names(flags)))
    }

    /// A restart report the plugin raises from inside the cycle (a `kLatencyChanged` from
    /// `setActive(1)`) describes the state just activated and is consumed, as the live cycle does.
    fn reinstall(&mut self, mut unit: Box<Vst3Unit>, slot: &SlotHost) -> Result<(), (Box<Vst3Unit>, String)> {
        let activated = self.plugin.activate(&unit.processor, slot);
        let raised = self.restart.take();
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

    fn turn(&mut self, _sink: &EventSink) -> Reported {
        Reported { relist: self.restart.take_notify() & RestartFlags::RELIST != 0, changed: false }
    }

    fn idle(&mut self) -> bool {
        let closed = matches!(&self.editor, Vst3Editor::Open { win, .. } if win.close_requested());
        if closed {
            vst3_editor_close(&mut self.editor);
        }
        closed
    }

    fn params(&mut self) -> Result<Vec<ParamDesc>, String> {
        list_vst3_params(&self.plugin.controller)
    }

    fn state(&mut self) -> Result<Option<Vec<u8>>, String> {
        save_state(&self.plugin)
    }

    fn editor(&self) -> Editor {
        match self.editor {
            Vst3Editor::Closed => Editor::Closed,
            Vst3Editor::Open { .. } => Editor::Hosted,
        }
    }

    fn editor_open(&mut self) -> Result<(), String> {
        self.editor = vst3_editor_open(&self.plugin.controller, self.slot)?;
        Ok(())
    }

    fn editor_close(&mut self) {
        vst3_editor_close(&mut self.editor);
    }

    /// The controller (what the editor shows, and what decides a controller-side restart) hears
    /// what the processor got through the ring.
    fn set_param_normalized(&mut self, id: u32, value: f64) {
        if let Some(ctl) = self.plugin.controller.as_ref() {
            // SAFETY: owner thread, live controller.
            let r = unsafe { ctl.setParamNormalized(id as ParamID, value) };
            if r != kResultOk {
                log::debug!("[plugin_host] engine slot {} controller.setParamNormalized({id}) → {r:#x}", self.slot);
            }
        }
    }

    /// The unit releases the processor → the controller terminates → `setActive(0)` → `terminate` →
    /// the COM objects release → the module unloads last → the handler goes.
    fn release(self, unit: Option<Box<Vst3Unit>>) {
        let Vst3 { editor, plugin, handler, .. } = self;
        drop(editor);
        drop(unit);
        drop(plugin);
        drop(handler);
    }
}
