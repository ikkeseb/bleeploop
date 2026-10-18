//! OWNS: a CLAP plugin in the engine: `ClapUnit` (its audio processor as an
//! `lf_engine::SlotProcessor`) and what its owner thread does with the plugin (`Clap`, which the
//! shared owner loop `engine_slot::serve` drives): the load, activation, a restart and a
//! re-activation after an eviction, and the teardown's order, keeping the
//! plugin's tone as `engine_slot` describes (the `state` extension's blob, captured under the tone
//! limit: `CappedState`). It followed the WebView bridge's CLAP owner (deleted in Stage 6) minus its RT
//! thread, its hop-1 ring and the device.

use super::engine_slot::{
    own, restore_tone, serve, Editor, EventSink, Loaded, OwnedPlugin, OwnerCtx, Reported, Restore, Setup,
    FAULT_PARAM, FAULT_PROCESS, FAULT_START,
};
use crate::host::tone::MAX_STATE_BYTES;
use super::*;

use clack_host::events::io::{OutputEventBuffer, TryPushError};
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::UnknownEvent;
use lf_engine::grid::Frame;
use lf_engine::slots::MAX_SLOT_EVENTS;
use lf_engine::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

use crate::engine_io::SlotHost;

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
    params: Consumer<ParamEvent>,
    events: EventBuffer,
    /// One buffer set per declared port, in port order (port → channel → frames): input port 0
    /// carries the live input and the others stay silent; output port 0 is the slot's wet signal and
    /// the others are scratch the unit never reads.
    in_bufs: Vec<Vec<Vec<f32>>>,
    out_bufs: Vec<Vec<Vec<f32>>>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    steady: u64,
    faults: Arc<AtomicU32>,
    /// The plugin's own parameter changes, reported on its output events.
    edits: ParamEdits,
    /// The owner's `ToneKeeper` flag, raised when `edits` saw one (an atomic store on the audio thread).
    edited: Arc<AtomicBool>,
}

/// The unit's output events: the plugin reports its editor's parameter changes here (CLAP has the
/// plugin send them from `process`). None is kept; the unit only notes that one came, so the owner
/// saves the tone. No allocation, no lock.
#[derive(Default)]
struct ParamEdits {
    seen: bool,
}

impl OutputEventBuffer for ParamEdits {
    fn try_push(&mut self, event: &UnknownEvent) -> Result<(), TryPushError> {
        if matches!(event.as_core_event(), Some(CoreEventSpace::ParamValue(_) | CoreEventSpace::ParamGestureEnd(_))) {
            self.seen = true;
        }
        Ok(())
    }
}

/// What one activation gave the unit to build on.
pub(super) struct Terms {
    /// The engine rate it activated at (`SlotHost::install` checks it).
    pub(super) rate: u32,
    pub(super) max_frames: u32,
    /// The plugin's audio ports per direction, as channel counts in port order (`query_ports`).
    pub(super) inputs: Vec<u32>,
    pub(super) outputs: Vec<u32>,
    pub(super) latency: u32,
}

impl ClapUnit {
    pub(super) fn new(
        stopped: StoppedPluginAudioProcessor<LfHost>,
        terms: &Terms,
        params: Consumer<ParamEvent>,
        faults: Arc<AtomicU32>,
        edited: Arc<AtomicBool>,
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
            edits: ParamEdits::default(),
            edited,
        });
        unit.rearm(stopped, terms);
        unit
    }

    /// Owner thread: take a freshly activated processor and size everything to its terms.
    fn rearm(&mut self, stopped: StoppedPluginAudioProcessor<LfHost>, terms: &Terms) {
        self.processor = Some(Processor::Stopped(stopped));
        self.refused = false;
        let live_input = terms.inputs.first().is_some_and(|&channels| channels > 0);
        self.kind = if live_input { SlotKind::Effect } else { SlotKind::Instrument };
        self.latency = terms.latency as Frame;
        self.max_frames = (terms.max_frames as usize).max(1);
        let frames = self.max_frames;
        let sets = |ports: &[u32]| -> Vec<Vec<Vec<f32>>> {
            ports.iter().map(|&channels| vec![vec![0.0; frames]; channels as usize]).collect()
        };
        // Zeroed once: an input port past 0 is never written again, and CLAP makes input buffers
        // read-only to the plugin (`process.h`), so it stays silent across calls.
        self.in_bufs = sets(&terms.inputs);
        self.out_bufs = sets(&terms.outputs);
        // Sized to the whole layout, so clack's `with_*_buffers` never grows them on the audio thread
        // (its pointer rewrite after a growth is also only right for a single port).
        let channels = |ports: &[u32]| ports.iter().map(|&c| c as usize).sum::<usize>().max(1);
        self.in_ports = AudioPorts::with_capacity(channels(&terms.inputs), terms.inputs.len().max(1));
        self.out_ports = AudioPorts::with_capacity(channels(&terms.outputs), terms.outputs.len().max(1));
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
            edits,
            edited,
            ..
        } = self;
        let Some(Processor::Started(started)) = processor.as_mut() else {
            return;
        };
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
                    let ParamEvent { id, value } = event;
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
            // Port 0 carries the live input; port 0's outputs are what the slot plays.
            for ch in in_bufs.iter_mut().take(1).flatten() {
                ch[..len].copy_from_slice(&input[at..at + len]);
            }
            // A plugin may report silence by leaving its outputs untouched.
            for ch in out_bufs.iter_mut().take(1).flatten() {
                ch[..len].fill(0.0);
            }
            let status = {
                // Every declared port, in port order. `InputChannel::variable`, never `constant`: a
                // live input varies sample to sample.
                let input_audio = in_ports.with_input_buffers(in_bufs.iter_mut().map(move |port| AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        port.iter_mut().map(move |c| InputChannel::variable(&mut c[..len])),
                    ),
                }));
                let input_events = InputEvents::from_buffer(buf);
                let mut output_events = OutputEvents::from_buffer(edits);
                let mut output_audio = out_ports.with_output_buffers(out_bufs.iter_mut().map(move |port| AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_output_only(port.iter_mut().map(move |c| &mut c[..len])),
                }));
                let steady_time = Some(*steady);
                started.process(&input_audio, &mut output_audio, &input_events, &mut output_events, steady_time, None)
            };
            if status.is_err() {
                // CLAP discards a failed call's output: the slice stays silent.
                faults.fetch_or(FAULT_PROCESS, Relaxed);
            } else if let Some(main) = out_bufs.first() {
                sum_to_mono(main, &mut out[at..at + len], len, main.len());
            }
            *steady = steady.wrapping_add(len as u64);
            at += len;
        }
        if std::mem::take(&mut edits.seen) {
            edited.store(true, Release);
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
    let outputs = query_ports(instance, false)?;
    let inputs = query_ports(instance, true)?;
    let config =
        PluginAudioConfiguration { sample_rate: rate as f64, min_frames_count: 1, max_frames_count: max_frames };
    let stopped = instance.activate(|_, _| (), config).map_err(|e| format!("activate failed: {e}"))?;
    let latency = reported_latency(instance);
    Ok((stopped, Terms { rate, max_frames, inputs, outputs, latency }))
}

/// Instantiate `id` from `entry` with the production host handlers.
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
            pending_resize: AtomicU64::new(NO_RESIZE),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            keeps_tone: true,
        },
        |_| LfMain::default(),
        entry,
        id_c.as_c_str(),
        &host_info,
    )
    .map_err(|e| format!("instantiate failed: {e}"))?;
    Ok((instance, name))
}

/// Where `state.save` writes: it refuses a write that would grow the state past `MAX_STATE_BYTES`,
/// and stays refused, so a plugin that ignores a failed write and reports success still ends in an
/// error rather than a truncated tone.
#[derive(Default)]
struct CappedState {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl std::io::Write for CappedState {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.overflowed || self.bytes.len().saturating_add(buf.len()) > MAX_STATE_BYTES {
            self.overflowed = true;
            return Err(std::io::Error::other("past the tone limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The plugin's state (`state` extension), for its tone: `Ok(None)` when it has no such extension.
fn save_state(instance: &mut PluginInstance<LfHost>) -> Result<Option<Vec<u8>>, String> {
    let mut handle = instance.plugin_handle();
    let Some(ext) = handle.get_extension::<PluginState>() else { return Ok(None) };
    let mut state = CappedState::default();
    let saved = ext.save(&mut handle, &mut state);
    if state.overflowed {
        return Err(format!("the plugin's state is more than {MAX_STATE_BYTES} bytes"));
    }
    saved.map_err(|e| format!("state.save: {e}"))?;
    Ok(Some(state.bytes))
}

/// Restore a tone's state (`state.load`, a main-thread call; the load runs it before activating).
fn load_state(instance: &mut PluginInstance<LfHost>, state: &[u8]) -> Result<(), String> {
    let mut handle = instance.plugin_handle();
    let ext = handle.get_extension::<PluginState>().ok_or("the plugin has no state extension")?;
    ext.load(&mut handle, &mut std::io::Cursor::new(state)).map_err(|_| "the plugin refused its saved state".to_string())
}

/// The engine-mode CLAP owner (`engine_slot::serve`): clack's main thread for this slot. `open`
/// yields the entry (a bundle path in production, an in-process fixture in the tests); returns the
/// teardown's result.
pub(super) fn run(
    ctx: OwnerCtx,
    id: &str,
    open: impl FnOnce() -> Result<PluginEntry, String>,
) -> Result<(), String> {
    serve(ctx, |setup| load(id, open, setup))
}

/// Instantiate the plugin, restore the stored tone, activate it and build its unit.
fn load(
    id: &str,
    open: impl FnOnce() -> Result<PluginEntry, String>,
    Setup { slot, params, editor_parent, faults, tone, .. }: Setup<'_>,
) -> Result<Loaded<Clap>, String> {
    let editor_closed = Arc::new(EditorClosed::default());
    let hosted_hwnd = Arc::new(AtomicIsize::new(0));
    let entry = open()?;
    let (mut instance, name) = instantiate(&entry, id, &editor_closed, &hosted_hwnd)?;
    // init() may request a callback, but only the fully initialized instance can receive it.
    deliver_plugin_callback(&mut instance);
    // The stored tone goes in before the plugin activates: nothing processes it yet.
    let restored = restore_tone(tone, slot.slot(), &name, |state| load_state(&mut instance, state));
    if restored == Restore::Refused {
        // The plugin may have taken part of the tone before it refused: a fresh instance is at the
        // defaults the player is told it loaded with.
        drop(instance);
        instance = instantiate(&entry, id, &editor_closed, &hosted_hwnd)?.0;
        deliver_plugin_callback(&mut instance);
    }
    // What the restore raised (a restart, a rescan, `mark_dirty`) describes the state just loaded,
    // which the activation and the param listing after the load read anyway.
    instance.access_shared_handler(|s| s.restart_requested.store(false, Release));
    let _ = take_params_rescan(&mut instance);
    instance.access_handler_mut(|m| m.state_dirty = false);
    let (stopped, terms) = activate(&mut instance, slot)?;
    let unit = ClapUnit::new(stopped, &terms, params, faults, tone.edited());
    let plugin = Clap { instance, entry, editor: EditorSlot::Closed, editor_closed, hosted_hwnd, editor_parent };
    Ok(Loaded { plugin, unit, name, rate: terms.rate, tone: restored.report() })
}

/// A loaded CLAP plugin as its owner holds it.
struct Clap {
    /// Before `entry`: the instance is destroyed before its entry unloads.
    instance: PluginInstance<LfHost>,
    entry: PluginEntry,
    editor: EditorSlot,
    /// Raised by a floating editor's own window thread when the player closes it.
    editor_closed: Arc<EditorClosed>,
    hosted_hwnd: Arc<AtomicIsize>,
    /// The main window's HWND (0 = none): a floating editor's transient parent.
    editor_parent: usize,
}

impl OwnedPlugin for Clap {
    type Unit = ClapUnit;
    const FORMAT: &'static str = "CLAP";

    fn take_restart(&mut self) -> Option<String> {
        let asked = self.instance.access_shared_handler(|s| s.restart_requested.swap(false, Acquire));
        asked.then(|| "restart at the plugin's request".to_string())
    }

    fn reinstall(&mut self, mut unit: Box<ClapUnit>, slot: &SlotHost) -> Result<(), (Box<ClapUnit>, String)> {
        let instance = &mut self.instance;
        if let Some(stopped) = unit.take_stopped() {
            instance.deactivate(stopped);
        }
        let (stopped, terms) = match activate(instance, slot) {
            Ok(activated) => activated,
            Err(e) => return Err((unit, e)),
        };
        unit.rearm(stopped, &terms);
        slot.install(unit, terms.rate).map_err(|(unit, e)| {
            let mut unit = own::<ClapUnit>(unit);
            if let Some(stopped) = unit.take_stopped() {
                instance.deactivate(stopped);
            }
            (unit, e)
        })
    }

    fn turn(&mut self, _sink: &EventSink) -> Reported {
        deliver_plugin_callback(&mut self.instance);
        let relist = take_params_rescan(&mut self.instance);
        let changed = self.instance.access_handler_mut(|m| std::mem::take(&mut m.state_dirty));
        Reported { relist, changed }
    }

    fn idle(&mut self) -> bool {
        // The plugin's `closed` (maybe from its own window thread), for either editor form: acked
        // here, a GUI it destroyed with `destroy` alone (no hide, no resize first).
        let closed_by_plugin = self.editor_closed.take().filter(|_| !matches!(self.editor, EditorSlot::Closed));
        if let Some(destroyed) = closed_by_plugin {
            editor_teardown(&mut self.instance, &mut self.editor, &self.hosted_hwnd, destroyed);
            return true;
        }
        if matches!(self.editor, EditorSlot::Hosted(_)) {
            deliver_pending_resize(&mut self.instance);
        }
        let closed = matches!(&self.editor, EditorSlot::Hosted(window) if window.close_requested());
        if closed {
            self.editor_close();
        }
        closed
    }

    fn params(&mut self) -> Result<Vec<ParamDesc>, String> {
        clap_param_descs(&mut self.instance)
    }

    fn state(&mut self) -> Result<Option<Vec<u8>>, String> {
        save_state(&mut self.instance)
    }

    fn editor(&self) -> Editor {
        match self.editor {
            EditorSlot::Closed => Editor::Closed,
            EditorSlot::Floating => Editor::Floating,
            EditorSlot::Hosted(_) => Editor::Hosted,
        }
    }

    fn editor_open(&mut self) -> Result<(), String> {
        self.editor = editor_open(&mut self.instance, self.editor_parent, &self.hosted_hwnd)?;
        Ok(())
    }

    fn editor_close(&mut self) {
        editor_teardown(&mut self.instance, &mut self.editor, &self.hosted_hwnd, false);
    }

    /// Deactivate (with the stopped processor the unit holds), destroy the instance, unload the
    /// entry last.
    fn release(self, unit: Option<Box<ClapUnit>>) {
        let Clap { mut instance, entry, .. } = self;
        match unit.and_then(|mut unit| unit.take_stopped()) {
            Some(stopped) => instance.deactivate(stopped),
            None if instance.is_active() => {
                let _ = instance.try_deactivate();
            }
            None => {}
        }
        drop(instance);
        drop(entry);
    }
}
