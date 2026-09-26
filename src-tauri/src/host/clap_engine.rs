//! OWNS: a CLAP plugin in the engine: `ClapUnit` (its audio processor as an
//! `lf_engine::SlotProcessor`) and the engine-mode owner thread that loads, activates, restarts,
//! re-activates after an eviction and tears it down (`engine_slot` holds the API). It follows
//! `owner_main` minus the RT thread, the hop-1 ring and the device; the load sequence is copied from
//! there rather than shared, because nothing but a window-bound owner exercises that one.

use super::engine_slot::{
    report_faults, EngineSlotEvent, OwnerCtx, Ready, FAULT_PARAM, FAULT_PROCESS, FAULT_START,
    OWNER_POLL, REMOVE_TIMEOUT,
};
use super::*;

use lf_engine::grid::Frame;
use lf_engine::slots::MAX_SLOT_EVENTS;
use lf_engine::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

use crate::engine_io::SlotHost;
use std::panic::{catch_unwind, AssertUnwindSafe};

enum Processor {
    Stopped(StoppedPluginAudioProcessor<LfHost>),
    Started(StartedPluginAudioProcessor<LfHost>),
}

/// One activated CLAP processor with everything its `process` touches, pre-sized on the owner
/// thread. `Send` without help: clack's processor handles are `Send` (an `Arc` of the `Send + Sync`
/// instance plus a `!Sync` marker, clack-host 0.1.0 `process.rs`), and so is every buffer here.
pub(super) struct ClapUnit {
    /// `None` only while the owner holds the unit between a deactivate and the next activate.
    processor: Option<Processor>,
    /// `start_processing` failed: silent until the owner re-activates and reinstalls it.
    refused: bool,
    kind: SlotKind,
    latency: Frame,
    max_frames: usize,
    params: Consumer<PluginEvent>,
    events: EventBuffer,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    steady: u64,
    faults: Arc<AtomicU32>,
}

/// What one activation gave the unit to build on.
pub(super) struct Terms {
    /// The engine rate it activated at (`SlotHost::install` checks it).
    pub(super) rate: u32,
    pub(super) max_frames: u32,
    pub(super) in_channels: u32,
    pub(super) out_channels: u32,
    pub(super) latency: u32,
}

impl ClapUnit {
    pub(super) fn new(
        stopped: StoppedPluginAudioProcessor<LfHost>,
        terms: &Terms,
        params: Consumer<PluginEvent>,
        faults: Arc<AtomicU32>,
    ) -> Box<ClapUnit> {
        let mut unit = Box::new(ClapUnit {
            processor: None,
            refused: false,
            kind: SlotKind::Instrument,
            latency: 0,
            max_frames: 0,
            params,
            // Ring params and engine notes share one buffer; neither side ever exceeds its cap, so
            // `push` stays inside the reserve and never allocates.
            events: EventBuffer::with_capacity(MAX_EVENTS_PER_BLOCK + MAX_SLOT_EVENTS),
            in_bufs: Vec::new(),
            out_bufs: Vec::new(),
            in_ports: AudioPorts::with_capacity(1, 1),
            out_ports: AudioPorts::with_capacity(1, 1),
            steady: 0,
            faults,
        });
        unit.rearm(stopped, terms);
        unit
    }

    /// Owner thread: take a freshly activated processor and size everything to its terms.
    fn rearm(&mut self, stopped: StoppedPluginAudioProcessor<LfHost>, terms: &Terms) {
        let (inputs, outputs) = (terms.in_channels as usize, terms.out_channels.max(1) as usize);
        self.processor = Some(Processor::Stopped(stopped));
        self.refused = false;
        self.kind = if inputs > 0 { SlotKind::Effect } else { SlotKind::Instrument };
        self.latency = terms.latency as Frame;
        self.max_frames = (terms.max_frames as usize).max(1);
        self.in_bufs = vec![vec![0.0; self.max_frames]; inputs];
        self.out_bufs = vec![vec![0.0; self.max_frames]; outputs];
        self.in_ports = AudioPorts::with_capacity(inputs.max(1), 1);
        self.out_ports = AudioPorts::with_capacity(outputs, 1);
    }

    /// Owner thread: the stopped processor, for `deactivate`. The engine stops a unit before it
    /// leaves; one that left started is stopped here rather than leaked.
    pub(super) fn take_stopped(&mut self) -> Option<StoppedPluginAudioProcessor<LfHost>> {
        match self.processor.take()? {
            Processor::Stopped(stopped) => Some(stopped),
            Processor::Started(started) => Some(started.stop_processing()),
        }
    }

    /// CLAP `start_processing` on the first call, on the thread that processes.
    fn start(&mut self) {
        if self.refused {
            return;
        }
        self.processor = match self.processor.take() {
            Some(Processor::Stopped(stopped)) => Some(match stopped.start_processing() {
                Ok(started) => Processor::Started(started),
                Err(e) => {
                    self.refused = true;
                    self.faults.fetch_or(FAULT_START, Relaxed);
                    Processor::Stopped(e.into_stopped_processor())
                }
            }),
            other => other,
        };
    }
}

impl SlotProcessor for ClapUnit {
    fn kind(&self) -> SlotKind {
        self.kind
    }

    fn latency(&self) -> Frame {
        self.latency
    }

    fn process(&mut self, _frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]) {
        out.fill(0.0);
        self.start();
        let ClapUnit {
            processor,
            max_frames,
            params,
            events: buf,
            in_bufs,
            out_bufs,
            in_ports,
            out_ports,
            steady,
            faults,
            ..
        } = self;
        let Some(Processor::Started(started)) = processor.as_mut() else {
            return;
        };
        let chans = out_bufs.len();
        let n = out.len();
        let (mut at, mut next) = (0, 0);
        // A call longer than the plugin's max frames goes in slices; each note lands in its own.
        while at < n {
            let len = (n - at).min(*max_frames);
            buf.clear();
            if at == 0 {
                // Params from the ring apply at the block start, ahead of every note (time order);
                // more than the cap wait in the ring for the next call.
                for _ in 0..MAX_EVENTS_PER_BLOCK {
                    let Ok(event) = params.pop() else { break };
                    // Only params ride this ring in engine mode: the engine routes the notes.
                    let PluginEvent::Param { id, value } = event else { continue };
                    match ClapId::from_raw(id) {
                        Some(id) => {
                            buf.push(&ParamValueEvent::new(0, id, Pckn::match_all(), value, Cookie::empty()))
                        }
                        None => {
                            faults.fetch_or(FAULT_PARAM, Relaxed);
                        }
                    }
                }
            }
            while let Some(e) = events.get(next).filter(|e| (e.offset as usize) < at + len) {
                let time = e.offset - at as u32;
                match e.kind {
                    SlotEventKind::NoteOn { key, velocity } => {
                        let note = Pckn::new(0u16, 0u16, key as u16, Match::All);
                        buf.push(&NoteOnEvent::new(time, note, velocity as f64))
                    }
                    SlotEventKind::NoteOff { key } => {
                        buf.push(&NoteOffEvent::new(time, Pckn::new(0u16, 0u16, key as u16, Match::All), 0.0))
                    }
                }
                next += 1;
            }
            for ch in in_bufs.iter_mut() {
                ch[..len].copy_from_slice(&input[at..at + len]);
            }
            // A plugin may report silence by leaving its outputs untouched.
            for ch in out_bufs.iter_mut() {
                ch[..len].fill(0.0);
            }
            let status = {
                // `InputChannel::variable`, never `constant`: a live input varies sample to sample.
                let input_audio = if in_bufs.is_empty() {
                    InputAudioBuffers::empty()
                } else {
                    in_ports.with_input_buffers([AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            in_bufs.iter_mut().map(|c| InputChannel::variable(&mut c[..len])),
                        ),
                    }])
                };
                let input_events = InputEvents::from_buffer(buf);
                let mut output_events = OutputEvents::void();
                let mut output_audio = out_ports.with_output_buffers([AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(out_bufs.iter_mut().map(|c| &mut c[..len])),
                }]);
                let steady_time = Some(*steady);
                started.process(&input_audio, &mut output_audio, &input_events, &mut output_events, steady_time, None)
            };
            if status.is_err() {
                faults.fetch_or(FAULT_PROCESS, Relaxed);
            }
            sum_to_mono(out_bufs, &mut out[at..at + len], len, chans);
            *steady = steady.wrapping_add(len as u64);
            at += len;
        }
    }

    fn stop(&mut self) {
        self.processor = match self.processor.take() {
            Some(Processor::Started(started)) => Some(Processor::Stopped(started.stop_processing())),
            other => other,
        };
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

/// A unit the engine handed back: a `SlotHost` hands back only what its own handle installed.
fn own(unit: Box<dyn SlotProcessor>) -> Box<ClapUnit> {
    unit.into_any()
        .downcast::<ClapUnit>()
        .unwrap_or_else(|_| panic!("an engine slot hands back the unit its owner installed"))
}

/// The latency the active plugin reports (CLAP `latency` extension, a main-thread call); 0 without
/// one. Read through the raw ABI because clack-extensions' `latency` feature is off in Cargo.toml.
fn reported_latency(instance: &PluginInstance<LfHost>) -> u32 {
    /// `clap_plugin_latency`: one function taking the plugin pointer.
    #[repr(C)]
    struct RawLatency {
        get: Option<unsafe extern "C" fn(*const std::ffi::c_void) -> u32>,
    }
    let plugin = instance.raw_instance();
    let Some(get_extension) = plugin.get_extension else {
        return 0;
    };
    // SAFETY: `plugin` is the live, activated instance on its main thread. A non-null
    // "clap.latency" extension is a `clap_plugin_latency`, whose one field `RawLatency` mirrors (its
    // plugin argument is a pointer either way); `get` takes the same plugin pointer.
    unsafe {
        let ext = get_extension(plugin, c"clap.latency".as_ptr()).cast::<RawLatency>();
        match ext.as_ref().and_then(|ext| ext.get) {
            Some(get) => get(std::ptr::from_ref(plugin).cast()),
            None => 0,
        }
    }
}

/// Owner thread, instance inactive: query the ports, activate at the engine's current rate with its
/// largest block as the max frame count, and read the latency the plugin now reports.
fn activate(
    instance: &mut PluginInstance<LfHost>,
    slot: &SlotHost,
) -> Result<(StoppedPluginAudioProcessor<LfHost>, Terms), String> {
    let rate = slot.rate().ok_or_else(|| "no audio device is open".to_string())?;
    let max_frames = slot.max_block().max(1) as u32;
    let out_channels = query_out_channels(instance)?;
    let in_channels = query_in_channels(instance)?;
    let config =
        PluginAudioConfiguration { sample_rate: rate as f64, min_frames_count: 1, max_frames_count: max_frames };
    let stopped = instance.activate(|_, _| (), config).map_err(|e| format!("activate failed: {e}"))?;
    let latency = reported_latency(instance);
    Ok((stopped, Terms { rate, max_frames, in_channels, out_channels, latency }))
}

/// Deactivate what the unit holds, activate again at the engine's current terms and install it.
/// On failure the unit comes back to the caller, deactivated: the slot stays bypassed.
fn reinstall(
    instance: &mut PluginInstance<LfHost>,
    mut unit: Box<ClapUnit>,
    slot: &SlotHost,
) -> Result<(), (Box<ClapUnit>, String)> {
    if let Some(stopped) = unit.take_stopped() {
        instance.deactivate(stopped);
    }
    let (stopped, terms) = match activate(instance, slot) {
        Ok(activated) => activated,
        Err(e) => return Err((unit, e)),
    };
    unit.rearm(stopped, &terms);
    match slot.install(unit, terms.rate) {
        Ok(()) => Ok(()),
        Err((unit, e)) => {
            let mut unit = own(unit);
            if let Some(stopped) = unit.take_stopped() {
                instance.deactivate(stopped);
            }
            Err((unit, e))
        }
    }
}

/// A restart or an eviction: get the unit back (it came from the engine, the owner still holds it
/// from a failed cycle, or the engine hands it back now) and reinstall it.
fn cycle(
    instance: &mut PluginInstance<LfHost>,
    slot: &SlotHost,
    parked: &mut Option<Box<ClapUnit>>,
    back: Option<Box<ClapUnit>>,
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
    match reinstall(instance, unit, slot) {
        Ok(()) => log::info!("[plugin_host] engine slot {index} {why}: deactivate → activate → reinstalled"),
        Err((unit, e)) => {
            log::error!("[plugin_host] engine slot {index} {why} failed, the slot stays bypassed: {e}");
            *parked = Some(unit);
        }
    }
}

/// Instantiate `id` from `entry` with the production host handlers. Copied from `owner_main`'s setup.
fn instantiate(
    entry: &PluginEntry,
    id: &str,
    editor_closed: &Arc<EditorClosed>,
    hosted_hwnd: &Arc<AtomicIsize>,
) -> Result<(PluginInstance<LfHost>, String), String> {
    let id_c = CString::new(id).map_err(|e| format!("bad id: {e}"))?;
    let name = entry
        .get_plugin_factory()
        .ok_or_else(|| "no plugin factory".to_string())?
        .plugin_descriptors()
        .find(|d| d.id() == Some(id_c.as_c_str()))
        .and_then(|d| d.name())
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.to_string());
    let host_info = HostInfo::new("BleepLoop", "BleepLoop", "https://bleeploop.local", "0.1.0")
        .map_err(|e| format!("host info: {e}"))?;
    let (ec, hh) = (editor_closed.clone(), hosted_hwnd.clone());
    let instance = PluginInstance::<LfHost>::new(
        move |_| LfShared {
            editor_closed: ec,
            hosted_hwnd: hh,
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
        },
        |_| LfMain::default(),
        entry,
        id_c.as_c_str(),
        &host_info,
    )
    .map_err(|e| format!("instantiate failed: {e}"))?;
    Ok((instance, name))
}

/// The engine-mode CLAP owner: clack's main thread for this slot. `open` yields the entry (a
/// bundle path in production, an in-process fixture in the tests); returns the teardown's result.
pub(super) fn run(
    ctx: OwnerCtx,
    id: &str,
    open: impl FnOnce() -> Result<PluginEntry, String>,
) -> Result<(), String> {
    let OwnerCtx { slot, running, requests, params, param_ids, sink, editor_parent, ready, .. } = ctx;
    let index = slot.slot();
    let editor_closed = Arc::new(EditorClosed::default());
    let hosted_hwnd = Arc::new(AtomicIsize::new(0));
    let faults = Arc::new(AtomicU32::new(0));
    let setup = open().and_then(|entry| {
        let (mut instance, name) = instantiate(&entry, id, &editor_closed, &hosted_hwnd)?;
        // init() may request a callback, but only the fully initialized instance can receive it.
        deliver_plugin_callback(&mut instance);
        let (stopped, terms) = activate(&mut instance, &slot)?;
        Ok((entry, instance, name, terms.rate, ClapUnit::new(stopped, &terms, params, faults.clone())))
    });
    let (entry, mut instance, name, rate, mut unit) = match setup {
        Ok(loaded) => loaded,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    // Before the load is reported, so the caller's first `set_param` finds its ids.
    refresh_clap_param_ids(&mut instance, &param_ids);
    let kind = unit.kind;
    if !running.load(Acquire) {
        if let Some(stopped) = unit.take_stopped() {
            instance.deactivate(stopped);
        }
        let _ = ready.send(Err("plugin load cancelled".to_string()));
        return Ok(());
    }
    if let Err((back, e)) = slot.install(unit, rate) {
        if let Some(stopped) = own(back).take_stopped() {
            instance.deactivate(stopped);
        }
        let _ = ready.send(Err(e));
        return Ok(());
    }
    if ready.send(Ok(Ready { name, kind })).is_err() {
        log::warn!("[plugin_host] engine slot {index}: the load finished after its caller gave up; undoing it");
        return teardown(&slot, instance, entry, None);
    }

    // The unit while it is out of the engine (a failed restart); `None` while the engine holds it.
    let mut parked: Option<Box<ClapUnit>> = None;
    let mut editor = EditorSlot::Closed;
    let mut reported = 0u32;
    // A panic in the loop must not unwind past a unit the engine still runs: it is caught here, and the
    // ordered teardown below takes the unit out of the engine first.
    let served = catch_unwind(AssertUnwindSafe(|| {
    while running.load(Acquire) {
        deliver_plugin_callback(&mut instance);
        if take_params_rescan(&mut instance) {
            refresh_clap_param_ids(&mut instance, &param_ids);
            sink(EngineSlotEvent::ParamsChanged);
        }
        if instance.access_shared_handler(|s| s.restart_requested.swap(false, Acquire)) {
            cycle(&mut instance, &slot, &mut parked, None, "restart at the plugin's request");
        }
        // A new engine at another rate evicted the unit; it comes back stopped.
        if let Some(unit) = slot.take_evicted() {
            cycle(&mut instance, &slot, &mut parked, Some(own(unit)), "re-activation after a device change");
        }
        // A floating editor's `closed` came from the plugin's own window thread: ack it here.
        if editor_closed.fired.swap(false, Acquire) && matches!(editor, EditorSlot::Floating) {
            editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
            sink(EngineSlotEvent::EditorClosed);
        }
        // A hosted editor needs this thread to pump its window, and reports its close box here.
        if matches!(editor, EditorSlot::Hosted(_)) {
            pump_thread_messages();
            if matches!(&editor, EditorSlot::Hosted(window) if window.close_requested()) {
                editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                sink(EngineSlotEvent::EditorClosed);
            }
        }
        let request = if matches!(editor, EditorSlot::Hosted(_)) {
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
        if let Some(request) = request.and_then(|r| take_uncancelled(r, index as u8)) {
            match request {
                OwnerRequest::OpenEditor(cancelled, reply) => {
                    let was_closed = matches!(editor, EditorSlot::Closed);
                    let res = if was_closed {
                        editor_open(&mut instance, editor_parent, &hosted_hwnd).map(|opened| editor = opened)
                    } else {
                        Ok(())
                    };
                    // Opened after the caller's 5 s: it reported failure, so close what this opened.
                    if was_closed && res.is_ok() && cancelled.load(Relaxed) {
                        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                    }
                    let _ = reply.send(res);
                }
                OwnerRequest::CloseEditor(_, reply) => {
                    if !matches!(editor, EditorSlot::Closed) {
                        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
                    }
                    let _ = reply.send(Ok(()));
                }
                // State and params as the live owner serves them; a device request never comes
                // from an engine handle and is answered as misrouted.
                other => handle_owner_request(other, &mut instance, &param_ids),
            }
        }
        report_faults(&faults, index, &mut reported);
    }
    }));
    if served.is_err() {
        log::error!("[plugin_host] engine slot {index}: the CLAP owner panicked; tearing the plugin down");
    }
    if !matches!(editor, EditorSlot::Closed) {
        editor_teardown(&mut instance, &mut editor, &hosted_hwnd);
    }
    let result = teardown(&slot, instance, entry, parked);
    report_faults(&faults, index, &mut reported);
    result
}

/// The ordered teardown: the unit leaves the engine (stopped on the audio thread) → deactivate →
/// the instance is destroyed → the entry unloads last. A unit the engine does not hand back keeps
/// the plugin loaded (leaked) rather than unloading code a processor still runs.
fn teardown(
    slot: &SlotHost,
    mut instance: PluginInstance<LfHost>,
    entry: PluginEntry,
    parked: Option<Box<ClapUnit>>,
) -> Result<(), String> {
    let index = slot.slot();
    let t = Instant::now();
    let unit = match parked {
        Some(unit) => Some(unit),
        None => match slot.remove(REMOVE_TIMEOUT) {
            Ok(unit) => unit.map(own),
            Err(e) => {
                log::error!("[plugin_host] engine slot {index} CLAP teardown: {e}; leaving the plugin loaded");
                slot.abandon();
                std::mem::forget(instance);
                std::mem::forget(entry);
                return Err(e);
            }
        },
    };
    let remove_ms = t.elapsed().as_millis();
    let t = Instant::now();
    match unit.and_then(|mut unit| unit.take_stopped()) {
        Some(stopped) => instance.deactivate(stopped),
        None if instance.is_active() => {
            let _ = instance.try_deactivate();
        }
        None => {}
    }
    let deactivate_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(instance);
    let destroy_ms = t.elapsed().as_millis();
    let t = Instant::now();
    drop(entry);
    log::info!(
        "[plugin_host] engine slot {index} CLAP teardown: remove={remove_ms} deactivate={deactivate_ms} destroy={destroy_ms} entry={} ms",
        t.elapsed().as_millis()
    );
    Ok(())
}
